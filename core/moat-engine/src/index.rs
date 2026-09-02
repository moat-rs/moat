// Copyright 2026- Moat Project Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The in-memory chunk index.
//!
//! One sharded hash table per disk, mapping [`ChunkId`] to the location of the
//! newest record. Readers only look entries up; every mutation is performed by
//! the disk's single writer. Shard mutexes therefore see negligible contention
//! and are held only for memory operations.
//!
//! The index is the *only* structure shared between the writer and readers,
//! and the segment pin protocol ([`Index::get_and_pin`]) is anchored on its
//! shard lock: a reader pins the segment inside the same critical section that
//! looks the entry up, and reclaim removes entries under the same lock before
//! waiting for pins to drain. A reader can therefore never observe a segment
//! that reclaim has already freed.

use std::collections::HashMap;

use moat_common::{ChunkId, chunk_id::ChunkIdHashBuilder};
use parking_lot::Mutex;

use crate::segments::SegmentTable;

/// Index flag: the record lives in a large batch.
pub const FLAG_LARGE: u32 = 1;
/// Index flag: the record has been read since it was written or last
/// relocated. Used by cache-mode reclaim to decide what to keep.
pub const FLAG_ACCESSED: u32 = 2;
/// Index flag (recovery only): the newest record for this key is a tombstone.
pub(crate) const FLAG_DEAD: u32 = 4;
/// Index flag: the record lives in a framed batch (header separate from the
/// page-aligned value).
pub const FLAG_FRAMED: u32 = 8;
/// Index flag: the record carries an expiry time in its header.
pub const FLAG_EXPIRES: u32 = 16;

/// Translates on-disk record flags into index flags.
pub(crate) fn flags_from_record(record_flags: u8) -> u32 {
    use crate::layout::{RECORD_FLAG_EXPIRES, RECORD_FLAG_FRAMED, RECORD_FLAG_LARGE};
    let mut flags = 0;
    if record_flags & RECORD_FLAG_LARGE != 0 {
        flags |= FLAG_LARGE;
    }
    if record_flags & RECORD_FLAG_FRAMED != 0 {
        flags |= FLAG_FRAMED;
    }
    if record_flags & RECORD_FLAG_EXPIRES != 0 {
        flags |= FLAG_EXPIRES;
    }
    flags
}

/// The physical position of a record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Location {
    /// Physical segment number.
    pub seg_no: u32,
    /// Offset of the record header within the segment.
    pub offset: u32,
}

/// What the index knows about the newest record of a chunk: enough to read
/// and verify a single-block value without touching its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexValue {
    /// Where the record header is.
    pub loc: Location,
    /// Offset of the value within the segment.
    pub value_off: u32,
    /// Value length in bytes.
    pub value_len: u32,
    /// `FLAG_*` bits.
    pub flags: u32,
    /// CRC32C of the value's first checksum block.
    pub crc: u32,
    /// The record's LSN.
    pub lsn: u64,
}

impl IndexValue {
    /// Whether the record lives in a large batch.
    #[inline]
    pub fn is_large(&self) -> bool {
        self.flags & FLAG_LARGE != 0
    }

    /// Whether the record lives in a framed batch.
    #[inline]
    pub fn is_framed(&self) -> bool {
        self.flags & FLAG_FRAMED != 0
    }

    /// Whether the record has an expiry time.
    #[inline]
    pub fn expires(&self) -> bool {
        self.flags & FLAG_EXPIRES != 0
    }

    /// Whether the record has been read since written or relocated.
    #[inline]
    pub fn is_accessed(&self) -> bool {
        self.flags & FLAG_ACCESSED != 0
    }

    /// The on-disk record flags this entry was derived from, for footprint
    /// accounting.
    #[inline]
    pub(crate) fn record_flags(&self) -> u8 {
        use crate::layout::{RECORD_FLAG_EXPIRES, RECORD_FLAG_FRAMED, RECORD_FLAG_LARGE};
        let mut flags = 0;
        if self.is_large() {
            flags |= RECORD_FLAG_LARGE;
        }
        if self.is_framed() {
            flags |= RECORD_FLAG_FRAMED;
        }
        if self.expires() {
            flags |= RECORD_FLAG_EXPIRES;
        }
        flags
    }
}

/// Result of [`Index::insert_if_newer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// No entry existed; the value was inserted.
    Inserted,
    /// An older entry was replaced.
    Replaced(IndexValue),
    /// The existing entry is at least as new; nothing changed.
    Rejected,
}

type Shard = Mutex<HashMap<ChunkId, IndexValue, ChunkIdHashBuilder>>;

/// The sharded chunk index of one disk.
pub struct Index {
    shards: Box<[Shard]>,
    mask: u64,
}

impl Index {
    /// Creates an empty index with `shard_count` shards (rounded up to a power
    /// of two).
    pub fn new(shard_count: usize) -> Self {
        let n = shard_count.max(1).next_power_of_two();
        let shards = (0..n).map(|_| Mutex::new(HashMap::default())).collect();
        Self {
            shards,
            mask: n as u64 - 1,
        }
    }

    #[inline]
    fn shard(&self, id: &ChunkId) -> &Shard {
        // hashbrown consumes the low and the top bits of the hash; take shard
        // bits from the middle so shards stay well distributed internally.
        &self.shards[((id.mix() >> 32) & self.mask) as usize]
    }

    /// Looks up a chunk.
    pub fn get(&self, id: &ChunkId) -> Option<IndexValue> {
        self.shard(id).lock().get(id).copied()
    }

    /// Looks up a chunk and, while still holding the shard lock, pins the
    /// segment it lives in and marks the entry as accessed.
    ///
    /// The caller must unpin the segment once it has finished reading.
    pub fn get_and_pin(&self, id: &ChunkId, segments: &SegmentTable) -> Option<IndexValue> {
        let mut shard = self.shard(id).lock();
        let value = shard.get_mut(id)?;
        value.flags |= FLAG_ACCESSED;
        segments.pin(value.loc.seg_no);
        Some(*value)
    }

    /// Inserts `value` unless the existing entry has an equal or higher LSN.
    ///
    /// Records may be applied out of LSN order (a large record is written
    /// before an earlier small record still sitting in a pending batch, and
    /// recovery sees records in arbitrary order), so the highest LSN must win
    /// regardless of arrival order.
    pub fn insert_if_newer(&self, id: ChunkId, value: IndexValue) -> InsertOutcome {
        let mut shard = self.shard(&id).lock();
        match shard.get_mut(&id) {
            Some(existing) if existing.lsn >= value.lsn => InsertOutcome::Rejected,
            Some(existing) => InsertOutcome::Replaced(std::mem::replace(existing, value)),
            None => {
                shard.insert(id, value);
                InsertOutcome::Inserted
            }
        }
    }

    /// Removes an entry, returning it.
    pub fn remove(&self, id: &ChunkId) -> Option<IndexValue> {
        self.shard(id).lock().remove(id)
    }

    /// Removes the entry if it still points at `expected`.
    pub fn remove_if_at(&self, id: &ChunkId, expected: Location) -> Option<IndexValue> {
        let mut shard = self.shard(id).lock();
        match shard.get(id) {
            Some(v) if v.loc == expected => shard.remove(id),
            _ => None,
        }
    }

    /// Replaces the entry with `value` if it still points at `expected`.
    ///
    /// The accessed flag of the existing entry is *not* carried over: a
    /// relocated record starts a fresh access history.
    pub fn replace_if_at(&self, id: &ChunkId, expected: Location, value: IndexValue) -> bool {
        let mut shard = self.shard(id).lock();
        match shard.get_mut(id) {
            Some(v) if v.loc == expected => {
                *v = value;
                true
            }
            _ => false,
        }
    }

    /// Removes every entry for which `keep` returns `false`.
    pub fn retain(&self, mut keep: impl FnMut(&ChunkId, &IndexValue) -> bool) {
        for shard in &self.shards {
            shard.lock().retain(|k, v| keep(k, v));
        }
    }

    /// Visits every entry. Shards are locked one at a time; the visitor must
    /// not call back into the index.
    pub fn for_each(&self, mut f: impl FnMut(&ChunkId, &IndexValue)) {
        for shard in &self.shards {
            for (k, v) in shard.lock().iter() {
                f(k, v);
            }
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(seg: u32, off: u32, lsn: u64) -> IndexValue {
        IndexValue {
            loc: Location {
                seg_no: seg,
                offset: off,
            },
            value_off: off + 68,
            value_len: 10,
            flags: 0,
            crc: 0,
            lsn,
        }
    }

    #[test]
    fn newer_wins() {
        let index = Index::new(4);
        let id = ChunkId::from_u128(1);
        assert_eq!(index.insert_if_newer(id, value(1, 0, 5)), InsertOutcome::Inserted);
        assert_eq!(index.insert_if_newer(id, value(2, 0, 3)), InsertOutcome::Rejected);
        assert_eq!(index.insert_if_newer(id, value(1, 0, 5)), InsertOutcome::Rejected);
        assert_eq!(index.get(&id).unwrap().lsn, 5);
        assert_eq!(
            index.insert_if_newer(id, value(3, 0, 9)),
            InsertOutcome::Replaced(value(1, 0, 5))
        );
        assert_eq!(index.get(&id).unwrap().loc.seg_no, 3);
    }

    #[test]
    fn conditional_updates() {
        let index = Index::new(4);
        let id = ChunkId::from_u128(2);
        index.insert_if_newer(id, value(1, 64, 1));
        let wrong = Location { seg_no: 1, offset: 128 };
        let right = Location { seg_no: 1, offset: 64 };
        assert!(!index.replace_if_at(&id, wrong, value(5, 0, 2)));
        assert!(index.replace_if_at(&id, right, value(5, 0, 2)));
        assert_eq!(index.get(&id).unwrap().loc.seg_no, 5);
        assert!(index.remove_if_at(&id, right).is_none());
        assert!(index.remove_if_at(&id, Location { seg_no: 5, offset: 0 }).is_some());
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn pin_and_access_flag() {
        let index = Index::new(4);
        let segments = SegmentTable::new(4);
        let id = ChunkId::from_u128(3);
        index.insert_if_newer(id, value(2, 0, 1));
        let v = index.get_and_pin(&id, &segments).unwrap();
        assert!(v.is_accessed());
        assert_eq!(segments.pins(2), 1);
        segments.unpin(2);
        assert_eq!(segments.pins(2), 0);
        // Relocation starts a fresh access history.
        assert!(index.replace_if_at(&id, v.loc, value(3, 0, 2)));
        assert!(!index.get(&id).unwrap().is_accessed());
    }
}
