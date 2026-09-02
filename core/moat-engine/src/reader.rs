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

//! Concurrent read access to an engine.
//!
//! [`Reader`] is a cheap, cloneable handle to the shared state (index, segment
//! table, device). Actual reads go through a [`ReadRing`], which a thread
//! creates for itself: it owns an I/O queue and buffer pool, submits reads,
//! and hands back verified values as [`ChunkData`], a view into the pool buffer
//! the device wrote into. No byte of the value is copied by the engine; the
//! buffer is released when the `ChunkData` is dropped.

use std::{
    io,
    ops::{Deref, Range},
    sync::Arc,
};

use moat_common::{CHECKSUM_BLOCK_SIZE, ChunkId, PooledBuf, crc32c, verify_blocks_with};

use crate::{
    error::{Error, Result},
    index::IndexValue,
    io::{IoCompletion, IoQueue, QueueOptions},
    layout::{RecordGeometry, RecordHeader, RecordKind, SegmentState, large_batch_len},
    shared::Shared,
    writer::Lsn,
};

/// Metadata about a stored chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkStat {
    /// Value length in bytes.
    pub len: u32,
    /// LSN of the newest record.
    pub lsn: Lsn,
    /// Physical segment holding the record.
    pub segment: u32,
    /// Offset of the value within the segment.
    pub value_offset: u32,
    /// Whether the record is stored framed (header apart from the page
    /// aligned value).
    pub framed: bool,
}

/// Space usage of an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Total number of segments.
    pub segments: u32,
    /// Segments not in use.
    pub free_segments: u32,
    /// Segments closed and eligible for reclaim.
    pub sealed_segments: u32,
    /// Bytes of records the index points at, across all segments.
    pub live_bytes: u64,
    /// Number of chunks in the index.
    pub chunks: usize,
}

/// A handle to an engine's shared state. Cheap to clone; create a
/// [`ReadRing`] per thread to read data.
#[derive(Clone)]
pub struct Reader {
    shared: Arc<Shared>,
}

impl Reader {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// Opens a read ring for the calling thread.
    ///
    /// The pool's maximum class is raised to fit the largest record if needed.
    pub fn ring(&self, opts: &QueueOptions) -> Result<ReadRing> {
        let mut opts = *opts;
        let largest = large_batch_len(self.shared.superblock.chunk_max) as usize;
        opts.pool.max_class = opts.pool.max_class.max(largest.next_power_of_two());
        let queue = self.shared.device.open_queue(&opts)?;
        let depth = queue.depth();
        Ok(ReadRing {
            shared: self.shared.clone(),
            queue,
            pending: (0..depth).map(|_| None).collect(),
            free_slots: (0..depth as u32).rev().collect(),
            scratch: Vec::new(),
        })
    }

    /// Returns metadata about a chunk without touching the disk.
    ///
    /// Expiry is not evaluated here; an expired chunk still reports its stat
    /// until reclaim drops it.
    pub fn stat(&self, id: &ChunkId) -> Option<ChunkStat> {
        self.shared.index.get(id).map(|v| ChunkStat {
            len: v.value_len,
            lsn: v.lsn,
            segment: v.loc.seg_no,
            value_offset: v.value_off,
            framed: v.is_framed(),
        })
    }

    /// Whether a chunk exists in the index.
    pub fn contains(&self, id: &ChunkId) -> bool {
        self.shared.index.get(id).is_some()
    }

    /// Current space usage.
    pub fn usage(&self) -> Usage {
        let segments = &self.shared.segments;
        let mut usage = Usage {
            segments: segments.len(),
            free_segments: 0,
            sealed_segments: 0,
            live_bytes: 0,
            chunks: self.shared.index.len(),
        };
        for s in segments.iter() {
            match segments.state(s) {
                SegmentState::Free => usage.free_segments += 1,
                SegmentState::Sealed => usage.sealed_segments += 1,
                SegmentState::Active => {}
            }
            usage.live_bytes += segments.live_bytes(s);
        }
        usage
    }

    /// The segment size this device was formatted with.
    pub fn segment_size(&self) -> u64 {
        self.shared.superblock.segment_size
    }

    /// The largest value this device accepts.
    pub fn chunk_max(&self) -> u32 {
        self.shared.superblock.chunk_max
    }
}

/// A verified value (or part of one) in the buffer the device read it into.
///
/// Dereferences to the requested bytes. Dropping it returns the buffer to the
/// ring's pool; [`ChunkData::into_raw`] hands the buffer over for callers that
/// DMA out of it directly.
pub struct ChunkData {
    buf: PooledBuf,
    range: Range<usize>,
}

impl ChunkData {
    /// The underlying pool buffer and the byte range of the value in it.
    pub fn into_raw(self) -> (PooledBuf, Range<usize>) {
        (self.buf, self.range)
    }
}

impl Deref for ChunkData {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        &self.buf[self.range.clone()]
    }
}

impl AsRef<[u8]> for ChunkData {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for ChunkData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkData").field("len", &self.range.len()).finish()
    }
}

/// Immediate result of [`ReadRing::get`].
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The chunk is not in the index; nothing was submitted.
    Miss,
    /// A read was submitted; its [`ReadCompletion`] carries the caller's token.
    Submitted,
}

/// A finished read.
#[derive(Debug)]
pub struct ReadCompletion {
    /// The token passed to [`ReadRing::get`].
    pub token: u64,
    /// The bytes, `None` if the chunk had expired, or an error.
    pub result: Result<Option<ChunkData>>,
}

struct PendingRead {
    id: ChunkId,
    value: IndexValue,
    geometry: RecordGeometry,
    range: Range<u64>,
    user_token: u64,
}

/// A per-thread read pipeline.
///
/// Owns an I/O queue and its buffer pool. [`ReadRing::get`] looks a chunk up
/// and submits the read; [`ReadRing::poll`] reaps, verifies and hands back
/// [`ChunkData`] views into the pool buffers the device wrote into, so the
/// engine copies no value bytes.
pub struct ReadRing {
    shared: Arc<Shared>,
    queue: Box<dyn IoQueue>,
    /// In-flight reads indexed by the slot number used as the I/O token.
    pending: Vec<Option<PendingRead>>,
    free_slots: Vec<u32>,
    scratch: Vec<IoCompletion>,
}

impl ReadRing {
    /// Looks a chunk up and, if present, submits the read covering `range`
    /// (the whole value when `None`). A range past the end is clamped.
    ///
    /// Fails with [`Error::Busy`] when no buffer or queue slot is free; poll
    /// and retry.
    pub fn get(&mut self, id: &ChunkId, range: Option<Range<u64>>, token: u64) -> Result<ReadOutcome> {
        let Some(slot) = self.free_slots.pop() else {
            return Err(Error::Busy);
        };
        let shared = &*self.shared;
        let Some(value) = shared.index.get_and_pin(id, &shared.segments) else {
            self.free_slots.push(slot);
            return Ok(ReadOutcome::Miss);
        };
        let total = value.value_len as u64;
        let range = match range {
            None => 0..total,
            Some(r) => {
                let start = r.start.min(total);
                start..r.end.min(total).max(start)
            }
        };
        // Framed single-block records are verified from the index CRC without
        // their header; everything else (and any record with an expiry, or a
        // strict configuration) reads the header too.
        let with_header = !value.is_framed()
            || value.expires()
            || shared.options.verify_header_on_read
            || value.value_len as usize > CHECKSUM_BLOCK_SIZE;
        let geometry = RecordGeometry::new(
            value.loc.offset as u64,
            value.value_off as u64,
            value.value_len,
            with_header,
        );
        let Some(buf) = self.queue.pool().alloc(geometry.extent.len as usize) else {
            shared.segments.unpin(value.loc.seg_no);
            self.free_slots.push(slot);
            return Err(Error::Busy);
        };
        let offset = shared.geometry.segment_offset(value.loc.seg_no) + geometry.extent.start;
        if let Err(e) = self.queue.read(buf, geometry.extent.len as usize, offset, slot as u64) {
            shared.segments.unpin(value.loc.seg_no);
            self.free_slots.push(slot);
            return Err(Error::Io(e));
        }
        self.pending[slot as usize] = Some(PendingRead {
            id: *id,
            value,
            geometry,
            range,
            user_token: token,
        });
        Ok(ReadOutcome::Submitted)
    }

    /// Pushes submitted reads to the device without waiting.
    pub fn submit(&mut self) -> Result<()> {
        self.queue.submit()?;
        Ok(())
    }

    /// Reaps finished reads, verifies them, and appends the results to `out`.
    /// With `wait` set and reads in flight, blocks until at least one
    /// completes. Returns the number appended.
    pub fn poll(&mut self, out: &mut Vec<ReadCompletion>, wait: bool) -> Result<usize> {
        self.scratch.clear();
        self.queue.poll(&mut self.scratch, wait)?;
        let mut n = 0;
        let mut finished = std::mem::take(&mut self.scratch);
        for done in finished.drain(..) {
            let slot = done.token as u32;
            let Some(pending) = self.pending[slot as usize].take() else {
                continue;
            };
            self.free_slots.push(slot);
            let result = self.finish(&pending, done);
            self.shared.segments.unpin(pending.value.loc.seg_no);
            if let Err(Error::Corrupt(_)) = &result
                && let Some(old) = self.shared.index.remove_if_at(&pending.id, pending.value.loc)
            {
                self.shared.segments.sub_live(
                    old.loc.seg_no,
                    RecordGeometry::footprint(old.value_len, old.record_flags()),
                );
            }
            out.push(ReadCompletion {
                token: pending.user_token,
                result,
            });
            n += 1;
        }
        self.scratch = finished;
        Ok(n)
    }

    /// Reads a chunk synchronously: submits, waits, returns the value (or the
    /// requested range of it). Convenience for tools and tests.
    pub fn get_sync(&mut self, id: &ChunkId, range: Option<Range<u64>>) -> Result<Option<ChunkData>> {
        const TOKEN: u64 = u64::MAX;
        match self.get(id, range, TOKEN)? {
            ReadOutcome::Miss => return Ok(None),
            ReadOutcome::Submitted => {}
        }
        let mut out = Vec::with_capacity(1);
        loop {
            self.poll(&mut out, true)?;
            if let Some(pos) = out.iter().position(|c| c.token == TOKEN) {
                return out.swap_remove(pos).result;
            }
        }
    }

    /// Reads in flight.
    pub fn in_flight(&self) -> usize {
        self.pending.len() - self.free_slots.len()
    }

    /// Verifies a completed read and cuts the requested range out of it.
    fn finish(&self, pending: &PendingRead, done: IoCompletion) -> Result<Option<ChunkData>> {
        let id = &pending.id;
        let value = &pending.value;
        let geometry = &pending.geometry;
        let buf = match done.result {
            Ok(n) if n as u64 == geometry.extent.len => done.buf.expect("read returns its buffer"),
            Ok(n) => {
                return Err(Error::Io(io::Error::other(format!(
                    "chunk {id}: short read {n} of {} bytes",
                    geometry.extent.len
                ))));
            }
            Err(e) => return Err(Error::Io(e)),
        };

        let range = &pending.range;
        let value_at = geometry.value_in_extent as usize;
        match geometry.header_in_extent {
            Some(header_at) => {
                let (header, checksums) = RecordHeader::decode(&buf[header_at as usize..])
                    .ok_or_else(|| Error::corrupt(format!("chunk {id}: record header invalid")))?;
                if header.key != *id || header.lsn != value.lsn || header.value_len != value.value_len {
                    return Err(Error::corrupt(format!(
                        "chunk {id}: record header does not match index"
                    )));
                }
                if header.kind != RecordKind::Data {
                    return Err(Error::corrupt(format!("chunk {id}: index points at a tombstone")));
                }
                if self.shared.is_expired(header.expire_at) {
                    return Ok(None);
                }
                if !range.is_empty() {
                    // Verify only the checksum blocks the range touches.
                    let block = CHECKSUM_BLOCK_SIZE as u64;
                    let first_block = range.start / block;
                    let verify_end = (((range.end - 1) / block + 1) * block).min(value.value_len as u64);
                    let covered = &buf[value_at + (first_block * block) as usize..value_at + verify_end as usize];
                    if let Err(block) = verify_blocks_with(covered, first_block as u32, |i| checksums.get(i)) {
                        return Err(Error::corrupt(format!("chunk {id}: checksum block {block} mismatch")));
                    }
                }
            }
            None => {
                // Header-less read: the value is a single checksum block whose
                // CRC the index carries. The pin protocol guarantees the
                // location is current; the CRC guards against media errors.
                let whole = &buf[value_at..value_at + value.value_len as usize];
                if crc32c(whole) != value.crc {
                    return Err(Error::corrupt(format!("chunk {id}: value checksum mismatch")));
                }
            }
        }
        Ok(Some(ChunkData {
            buf,
            range: value_at + range.start as usize..value_at + range.end as usize,
        }))
    }
}
