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

//! The single writer of a disk.
//!
//! Exactly one [`Writer`] exists per open engine. It owns the log tail of the
//! hot and cold active segments, assigns LSNs, performs every index mutation,
//! seals segments, and runs reclaim. Because all of that happens on one thread,
//! the engine needs no locking beyond the index shard mutexes that readers
//! also take.
//!
//! # I/O pipeline
//!
//! Records are encoded straight into pool buffers: small values into a packed
//! staging batch (inline or framed, see [`crate::layout`]), large values into
//! a buffer whose value area the caller may fill directly
//! ([`Writer::prepare_large`]), so the only copy on the write path is the one
//! the caller chooses to make. Batches are submitted to the writer's
//! [`IoQueue`] and several stay in flight; completions are applied strictly in
//! submission order, so acknowledged records always form a contiguous prefix of
//! the log and a crash never leaves a hole in front of acknowledged data.
//!
//! A failed write truncates its segment at the failure offset: that batch and
//! every later batch of the same segment are reported as failed, the segment is
//! sealed with the records that did land, and writing continues on a fresh
//! segment.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::Arc,
    thread,
};

use moat_common::{
    ChunkId, PAGE_SIZE, PooledBuf, align_up, block_checksums, block_count, chunk_id::ChunkIdHashBuilder,
};

use crate::{
    error::{Error, Result},
    index::{IndexValue, InsertOutcome, Location, flags_from_record},
    io::{IoCompletion, IoQueue},
    layout::{
        BATCH_HEADER_LEN, BatchHeader, BatchKind, FooterEntry, RECORD_ALIGN, RECORD_FLAG_EXPIRES, RECORD_FLAG_FRAMED,
        RECORD_FLAG_LARGE, RecordGeometry, RecordHeader, RecordKind, SegmentHeader, SegmentKind, SegmentState,
        encode_footer, footer_len, large_batch_len, large_value_offset, prefer_framed, record_meta_len,
    },
    scan::{BatchStep, max_batch_len, next_batch, parse_batch},
    shared::Shared,
};

/// A per-disk write sequence number.
pub type Lsn = u64;

/// Identifies a submitted write until its [`Completion`] is delivered.
pub type Ticket = u64;

/// Tokens with this bit set are not batch writes (reclaim reads, fsync).
const AUX_TOKEN: u64 = 1 << 63;

/// Options for a single `put`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PutOptions {
    /// Replace an existing chunk with the same identifier. When `false` (the
    /// default) a put of an existing identifier returns
    /// [`PutOutcome::Exists`] without writing anything.
    pub overwrite: bool,
    /// Unix time (seconds) after which the chunk reads as missing and reclaim
    /// drops it. Zero means never.
    pub expire_at: u64,
}

/// Result of a `put`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The record was accepted. It is durable and visible once the
    /// [`Completion`] for `ticket` reports success (or after [`Writer::flush`]).
    Written {
        /// Identifies the eventual completion.
        ticket: Ticket,
        /// The record's LSN.
        lsn: Lsn,
    },
    /// The identifier already exists and `overwrite` was not set.
    Exists,
}

/// The outcome of a write accepted earlier.
#[derive(Debug)]
pub struct Completion {
    /// The ticket returned by `put`.
    pub ticket: Ticket,
    /// The record's LSN.
    pub lsn: Lsn,
    /// `Ok` once the record is on disk and indexed.
    pub result: Result<()>,
}

/// How reclaim decides what to keep when it processes a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimPolicy {
    /// Every live record is relocated; nothing is ever lost. Victims are the
    /// sealed segments with the fewest live bytes.
    Storage,
    /// Cache semantics: the oldest sealed segment is reclaimed and its live
    /// records are dropped, except that with `reinsert_accessed` records read
    /// since they were written are relocated (and their access bit cleared).
    Cache {
        /// Relocate records that have been read; drop only the never-read ones.
        reinsert_accessed: bool,
    },
}

/// What a reclaim pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    /// The segment that was reclaimed.
    pub seg_no: u32,
    /// Records examined.
    pub records: u64,
    /// Live data records copied to a cold segment.
    pub relocated: u64,
    /// Data records dropped (dead, expired, or evicted).
    pub dropped: u64,
    /// Tombstones copied forward because an older version might still exist.
    pub tombstones_relocated: u64,
    /// Tombstones no longer needed.
    pub tombstones_dropped: u64,
    /// Value bytes copied.
    pub bytes_relocated: u64,
}

/// A pool buffer laid out as a large batch, with the value area exposed for
/// the caller to fill in place (for example as an RDMA landing buffer).
pub struct LargeValue {
    buf: PooledBuf,
    value_len: u32,
    value_off: usize,
}

impl LargeValue {
    /// The value bytes to fill.
    pub fn value_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.value_off..self.value_off + self.value_len as usize]
    }

    /// The value bytes.
    pub fn value(&self) -> &[u8] {
        &self.buf[self.value_off..self.value_off + self.value_len as usize]
    }

    /// Length of the value.
    pub fn len(&self) -> u32 {
        self.value_len
    }

    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.value_len == 0
    }

    /// The underlying buffer and the offset of the value within it, for
    /// callers that DMA into the buffer directly.
    pub fn raw_parts(&mut self) -> (&mut PooledBuf, usize) {
        (&mut self.buf, self.value_off)
    }
}

struct Active {
    seg_no: u32,
    seq: u64,
    kind: SegmentKind,
    /// Offset of the next batch within the segment.
    tail: u64,
    /// Footer entries of every record applied so far.
    footer: Vec<FooterEntry>,
    /// A write into this segment failed; it must be sealed at `tail` and
    /// abandoned.
    broken: bool,
}

impl Active {
    fn fits(&self, batch_len: u64, new_records: usize, segment_size: u64) -> bool {
        !self.broken && self.tail + batch_len + footer_len(self.footer.len() + new_records) <= segment_size
    }
}

/// How the index should be updated once a record is on disk.
#[derive(Debug, Clone, Copy)]
enum Apply {
    /// A fresh write: newest LSN wins.
    Insert,
    /// A record relocated by reclaim: only applies if the index still points
    /// at the old location.
    Relocate(Location),
    /// No index change (tombstones; the index was updated at delete time).
    None,
}

struct PendingRecord {
    /// Offset of the record header from the start of the batch.
    offset_in_batch: u32,
    /// Offset of the value from the start of the batch.
    value_in_batch: u32,
    /// Footer entry with segment-relative offsets left at zero until the batch
    /// position is known.
    entry: FooterEntry,
    apply: Apply,
    ticket: Option<Ticket>,
}

/// A packed batch under construction, encoded directly into a pool buffer.
///
/// Inline batches place each header right before its value; framed batches
/// keep headers in a reserved area at the front and page-align every value.
struct Pending {
    kind: BatchKind,
    buf: Option<PooledBuf>,
    /// Bytes used so far: the end of the last record (inline) or of the last
    /// value (framed).
    len: usize,
    /// Framed batches: size of the reserved header area, and the position of
    /// the next header within it.
    header_len: usize,
    header_pos: usize,
    records: Vec<PendingRecord>,
    keys: HashSet<ChunkId, ChunkIdHashBuilder>,
    first_lsn: Lsn,
}

impl Pending {
    fn new(kind: BatchKind) -> Self {
        Self {
            kind,
            buf: None,
            len: BATCH_HEADER_LEN,
            header_len: 0,
            header_pos: BATCH_HEADER_LEN,
            records: Vec::new(),
            keys: HashSet::default(),
            first_lsn: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Attaches a fresh staging buffer. Framed batches reserve a header area
    /// large enough for the most records that could fit their values (every
    /// framed value is close to a page multiple, so at most one per page).
    fn attach(&mut self, buf: PooledBuf) {
        if self.kind == BatchKind::Framed {
            let max_records = buf.capacity() / PAGE_SIZE as usize;
            let meta = record_meta_len(PAGE_SIZE as u32);
            self.header_len = align_up((BATCH_HEADER_LEN + max_records * meta) as u64, PAGE_SIZE) as usize;
            self.header_pos = BATCH_HEADER_LEN;
            self.len = self.header_len;
        }
        self.buf = Some(buf);
    }

    /// Whether a record of `meta` + `value_len` bytes fits in the attached
    /// buffer.
    fn fits(&self, meta: usize, value_len: usize) -> bool {
        let Some(buf) = &self.buf else {
            return false;
        };
        match self.kind {
            BatchKind::Framed => {
                self.header_pos + meta <= self.header_len
                    && align_up(self.len as u64, PAGE_SIZE) as usize + value_len <= buf.capacity()
            }
            _ => self.next_position(meta, value_len) + meta + value_len <= buf.capacity(),
        }
    }

    /// Position the next inline record of `meta + value_len` bytes starts at:
    /// 8-byte aligned, but moved to the next page boundary whenever that lets
    /// the record span fewer pages. The gap is zero-filled and recognised by
    /// the scanner.
    fn next_position(&self, meta: usize, value_len: usize) -> usize {
        let pos = align_up(self.len as u64, RECORD_ALIGN) as usize;
        let len = (meta + value_len) as u64;
        let min_pages = len.div_ceil(PAGE_SIZE);
        let spanned = (pos as u64 % PAGE_SIZE + len).div_ceil(PAGE_SIZE);
        if spanned > min_pages {
            align_up(pos as u64, PAGE_SIZE) as usize
        } else {
            pos
        }
    }

    fn append(&mut self, hdr: &RecordHeader, checksums: &[u32], value: &[u8], apply: Apply, ticket: Option<Ticket>) {
        let meta = hdr.meta_len();
        let (hdr_pos, value_pos) = match self.kind {
            BatchKind::Framed => (self.header_pos, align_up(self.len as u64, PAGE_SIZE) as usize),
            _ => {
                let pos = self.next_position(meta, value.len());
                (pos, pos + meta)
            }
        };
        let buf = self.buf.as_mut().expect("pending buffer allocated");
        // Zero whatever lies between the previous record and this one.
        buf[self.len..value_pos.max(self.len)].fill(0);
        hdr.encode(&mut buf[hdr_pos..hdr_pos + meta], checksums);
        buf[value_pos..value_pos + value.len()].copy_from_slice(value);
        if self.records.is_empty() {
            self.first_lsn = hdr.lsn;
        }
        self.len = value_pos + value.len();
        if self.kind == BatchKind::Framed {
            self.header_pos = hdr_pos + meta;
        }
        self.keys.insert(hdr.key);
        self.records.push(PendingRecord {
            offset_in_batch: hdr_pos as u32,
            value_in_batch: value_pos as u32,
            entry: footer_entry(hdr, checksums),
            apply,
            ticket,
        });
    }

    /// Finishes the batch: zero-fills the unused header area and the tail
    /// padding, encodes the batch header, and returns the batch length.
    fn finish(&mut self, seg_seq: u64) -> usize {
        let batch_len = align_up(self.len as u64, PAGE_SIZE) as usize;
        let buf = self.buf.as_mut().expect("pending buffer allocated");
        if self.kind == BatchKind::Framed {
            buf[self.header_pos..self.header_len].fill(0);
        }
        buf[self.len..batch_len].fill(0);
        BatchHeader {
            seg_seq,
            batch_len: batch_len as u32,
            record_count: self.records.len() as u32,
            first_lsn: self.first_lsn,
            kind: self.kind,
            header_len: if self.kind == BatchKind::Framed {
                self.header_len as u32
            } else {
                0
            },
        }
        .encode(&mut buf[..BATCH_HEADER_LEN]);
        batch_len
    }
}

/// The footer entry for a record, with segment offsets still unresolved.
fn footer_entry(hdr: &RecordHeader, checksums: &[u32]) -> FooterEntry {
    FooterEntry {
        key: hdr.key,
        offset: 0,
        value_off: 0,
        value_len: hdr.value_len,
        lsn: hdr.lsn,
        crc: checksums.first().copied().unwrap_or(0),
        kind: hdr.kind,
        flags: hdr.flags,
    }
}

struct InFlight {
    token: u64,
    slot: usize,
    seg_no: u32,
    offset: u64,
    batch_len: u64,
    records: Vec<PendingRecord>,
    /// Set when the completion arrives; applied once every earlier batch has
    /// been applied.
    result: Option<io::Result<usize>>,
}

/// The single writer of an engine.
///
/// Exactly one writer exists per open engine. It owns the log tail of the hot
/// and cold active segments, assigns LSNs, performs every index mutation, seals
/// segments and runs reclaim. It is `Send` but not `Sync`: drive it from one
/// thread per disk.
///
/// # Durability and acknowledgement
///
/// `put` only enqueues. Values shorter than
/// [`Options::pack_threshold`](crate::Options::pack_threshold) accumulate in a
/// packed batch that is submitted once it reaches
/// [`Options::batch_limit`](crate::Options::batch_limit); larger values are
/// submitted immediately. A record is durable and visible to readers when its
/// [`Completion`] arrives through [`Writer::poll`] with `Ok`, or after
/// [`Writer::flush`] returns. Operations that need a consistent view
/// (`delete`, `reclaim`, `seal_active`, `close`) flush internally.
pub struct Writer {
    shared: Arc<Shared>,
    queue: Box<dyn IoQueue>,
    active: [Option<Active>; 2],
    /// Pending packed batches per segment kind: `[inline, framed]`.
    pending: [[Pending; 2]; 2],
    inflight: VecDeque<InFlight>,
    /// Completed auxiliary operations (reclaim reads, fsync).
    aux_done: HashMap<u64, IoCompletion>,
    completions: Vec<Completion>,
    scratch: Vec<IoCompletion>,
    /// Keys of records submitted but not yet applied, with multiplicity.
    inflight_keys: HashMap<ChunkId, u32, ChunkIdHashBuilder>,
    free: VecDeque<u32>,
    /// Batch write tokens are consecutive, so a completion is located in
    /// `inflight` by subtracting the front token.
    next_batch_token: u64,
    next_aux_token: u64,
    next_ticket: Ticket,
    next_seq: u64,
    next_lsn: Lsn,
    max_batch: u64,
    relocation_failures: u64,
}

fn slot(kind: SegmentKind) -> usize {
    match kind {
        SegmentKind::Hot => 0,
        SegmentKind::Cold => 1,
    }
}

fn batch_slot(kind: BatchKind) -> usize {
    match kind {
        BatchKind::Framed => 1,
        _ => 0,
    }
}

/// Chooses how a small (below the pack threshold) value is stored.
fn small_batch_kind(value_len: u32, expire_at: u64) -> BatchKind {
    // Framed records are verified from the index CRC without reading their
    // header, which is where the expiry lives; expiring values stay inline.
    if expire_at == 0 && prefer_framed(value_len) {
        BatchKind::Framed
    } else {
        BatchKind::Inline
    }
}

fn record_flags(batch: BatchKind, expire_at: u64) -> u8 {
    let mut flags = match batch {
        BatchKind::Large => RECORD_FLAG_LARGE,
        BatchKind::Framed => RECORD_FLAG_FRAMED,
        BatchKind::Inline => 0,
    };
    if expire_at != 0 {
        flags |= RECORD_FLAG_EXPIRES;
    }
    flags
}

fn header(kind: RecordKind, flags: u8, value_len: u32, lsn: Lsn, key: ChunkId, expire_at: u64) -> RecordHeader {
    RecordHeader {
        kind,
        flags,
        value_len,
        lsn,
        key,
        expire_at,
    }
}

impl Writer {
    pub(crate) fn new(
        shared: Arc<Shared>,
        queue: Box<dyn IoQueue>,
        free: VecDeque<u32>,
        next_seq: u64,
        next_lsn: Lsn,
    ) -> Self {
        let max_batch = max_batch_len(&shared);
        Self {
            shared,
            queue,
            active: [None, None],
            pending: [
                [Pending::new(BatchKind::Inline), Pending::new(BatchKind::Framed)],
                [Pending::new(BatchKind::Inline), Pending::new(BatchKind::Framed)],
            ],
            inflight: VecDeque::new(),
            aux_done: HashMap::new(),
            completions: Vec::new(),
            scratch: Vec::new(),
            inflight_keys: HashMap::default(),
            free,
            next_batch_token: 0,
            next_aux_token: 0,
            next_ticket: 1,
            next_seq,
            next_lsn,
            max_batch,
            relocation_failures: 0,
        }
    }

    // -- public API ----------------------------------------------------------

    /// Appends a chunk, copying `value` into the log buffers.
    ///
    /// For values at or above the pack threshold, [`Writer::prepare_large`]
    /// followed by [`Writer::put_large`] avoids this copy.
    pub fn put(&mut self, id: ChunkId, value: &[u8], opts: PutOptions) -> Result<PutOutcome> {
        self.check_len(value.len() as u64)?;
        if self.exists(&id, opts) {
            return Ok(PutOutcome::Exists);
        }
        let checksums = block_checksums(value);
        let len = value.len() as u32;
        if value.len() >= self.shared.options.pack_threshold as usize {
            let mut large = self.prepare_large(len)?;
            large.value_mut().copy_from_slice(value);
            let (ticket, lsn) = self.next_ids();
            let flags = record_flags(BatchKind::Large, opts.expire_at);
            let hdr = header(RecordKind::Data, flags, len, lsn, id, opts.expire_at);
            self.write_large(
                SegmentKind::Hot,
                &hdr,
                &checksums,
                large.buf,
                Apply::Insert,
                Some(ticket),
            )?;
            Ok(PutOutcome::Written { ticket, lsn })
        } else {
            let batch = small_batch_kind(len, opts.expire_at);
            self.reserve_small(SegmentKind::Hot, batch, value.len())?;
            let (ticket, lsn) = self.next_ids();
            let flags = record_flags(batch, opts.expire_at);
            let hdr = header(RecordKind::Data, flags, len, lsn, id, opts.expire_at);
            self.append_small(
                SegmentKind::Hot,
                batch,
                &hdr,
                &checksums,
                value,
                Apply::Insert,
                Some(ticket),
            )?;
            Ok(PutOutcome::Written { ticket, lsn })
        }
    }

    /// Allocates a buffer for a value of `value_len` bytes laid out as a large
    /// batch, so the value can be produced in place and written without a
    /// copy. `value_len` must be at least the pack threshold.
    pub fn prepare_large(&mut self, value_len: u32) -> Result<LargeValue> {
        let max = self.shared.superblock.chunk_max;
        if value_len > max {
            return Err(Error::ValueTooLarge {
                len: value_len as u64,
                max: max as u64,
            });
        }
        if value_len < self.shared.options.pack_threshold {
            return Err(Error::InvalidOption(format!(
                "large values must be at least {} bytes",
                self.shared.options.pack_threshold
            )));
        }
        let buf = self.alloc(large_batch_len(value_len) as usize)?;
        Ok(LargeValue {
            buf,
            value_len,
            value_off: large_value_offset(value_len) as usize,
        })
    }

    /// Appends a chunk from a buffer obtained with [`Writer::prepare_large`].
    ///
    /// `checksums` are the per-block CRC32Cs of the value if the producer
    /// already has them (end-to-end integrity); they are computed otherwise.
    pub fn put_large(
        &mut self,
        id: ChunkId,
        value: LargeValue,
        checksums: Option<&[u32]>,
        opts: PutOptions,
    ) -> Result<PutOutcome> {
        self.check_len(value.len() as u64)?;
        if self.exists(&id, opts) {
            return Ok(PutOutcome::Exists);
        }
        let expected = block_count(value.len() as u64) as usize;
        let checksums = match checksums {
            Some(c) if c.len() == expected => c.to_vec(),
            Some(c) => {
                return Err(Error::InvalidOption(format!(
                    "expected {expected} block checksums, got {}",
                    c.len()
                )));
            }
            None => block_checksums(value.value()),
        };
        let (ticket, lsn) = self.next_ids();
        let flags = record_flags(BatchKind::Large, opts.expire_at);
        let hdr = header(RecordKind::Data, flags, value.len(), lsn, id, opts.expire_at);
        self.write_large(
            SegmentKind::Hot,
            &hdr,
            &checksums,
            value.buf,
            Apply::Insert,
            Some(ticket),
        )?;
        Ok(PutOutcome::Written { ticket, lsn })
    }

    /// Deletes a chunk. Returns whether it existed.
    ///
    /// The deletion is durable when this returns: the index entry is removed
    /// and a tombstone has been written so recovery cannot resurrect the
    /// chunk.
    pub fn delete(&mut self, id: &ChunkId) -> Result<bool> {
        // Pending and in-flight puts of this key must be applied with their
        // (older) LSNs before the tombstone takes a newer one.
        self.flush_pending(SegmentKind::Hot)?;
        self.wait_inflight()?;
        let Some(old) = self.shared.index.remove(id) else {
            return Ok(false);
        };
        self.shared.segments.sub_live(
            old.loc.seg_no,
            RecordGeometry::footprint(old.value_len, old.record_flags()),
        );
        self.reserve_small(SegmentKind::Hot, BatchKind::Inline, 0)?;
        let lsn = self.take_lsn();
        let hdr = header(RecordKind::Tombstone, 0, 0, lsn, *id, 0);
        self.append_small(SegmentKind::Hot, BatchKind::Inline, &hdr, &[], &[], Apply::None, None)?;
        self.flush_pending(SegmentKind::Hot)?;
        self.wait_inflight()?;
        Ok(true)
    }

    /// Pushes queued I/O to the device without waiting for anything.
    pub fn submit(&mut self) -> Result<()> {
        self.queue.submit()?;
        Ok(())
    }

    /// Reaps I/O completions, applies them in order, and appends the resulting
    /// [`Completion`]s to `out`. With `wait` set and writes in flight, blocks
    /// until at least one completes. Returns the number appended.
    pub fn poll(&mut self, out: &mut Vec<Completion>, wait: bool) -> Result<usize> {
        self.poll_io(wait)?;
        let n = self.completions.len();
        out.append(&mut self.completions);
        Ok(n)
    }

    /// Writes every pending batch and waits for all in-flight writes.
    ///
    /// Afterwards every previous put is durable and visible. Completions
    /// produced meanwhile are discarded (use [`Writer::poll`] to observe them);
    /// the first failure, if any, is returned.
    pub fn flush(&mut self) -> Result<()> {
        self.flush_pending(SegmentKind::Hot)?;
        self.flush_pending(SegmentKind::Cold)?;
        self.wait_inflight()?;
        if self.shared.options.sync_on_flush {
            let token = self.next_aux_token();
            self.queue.fsync(token)?;
            let done = self.wait_aux(token)?;
            done.result?;
        }
        let first_err = self.completions.drain(..).find_map(|c| c.result.err());
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Flushes and seals both active segments.
    ///
    /// Sealed segments are described by a footer (fast recovery) and become
    /// eligible for reclaim. The writer allocates fresh segments on the next
    /// write, so sealing a nearly empty segment wastes its remaining space;
    /// callers normally leave sealing to the writer and use this only before
    /// shutdown or when reclaim must be able to reach recent data.
    pub fn seal_active(&mut self) -> Result<()> {
        self.flush()?;
        for kind in [SegmentKind::Hot, SegmentKind::Cold] {
            if let Some(active) = self.active[slot(kind)].take() {
                self.seal(active)?;
            }
        }
        Ok(())
    }

    /// Flushes and seals the active segments so the next open needs no scan.
    ///
    /// Dropping a writer without closing is safe; recovery scans whatever was
    /// left active.
    pub fn close(mut self) -> Result<()> {
        self.seal_active()
    }

    /// Number of free segments.
    pub fn free_segments(&self) -> u32 {
        self.free.len() as u32
    }

    /// The LSN the next record will receive.
    pub fn next_lsn(&self) -> Lsn {
        self.next_lsn
    }

    /// Batches submitted but not yet applied.
    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// Runs one reclaim pass under `policy`.
    ///
    /// Picks a victim among the sealed segments, relocates what the policy
    /// keeps, and frees the segment. Returns `None` if there is no sealed
    /// segment to reclaim.
    ///
    /// Relocation needs a free segment for the cold log; callers should reclaim
    /// before the free list is exhausted (keeping at least two free segments is
    /// enough).
    pub fn reclaim(&mut self, policy: ReclaimPolicy) -> Result<Option<ReclaimReport>> {
        let Some(victim) = self.pick_victim(policy) else {
            return Ok(None);
        };
        self.reclaim_segment(victim, policy).map(Some)
    }

    /// Chooses the segment [`Writer::reclaim`] would process next.
    pub fn pick_victim(&self, policy: ReclaimPolicy) -> Option<u32> {
        let segments = &self.shared.segments;
        segments
            .iter()
            .filter(|&s| segments.state(s) == SegmentState::Sealed)
            .min_by_key(|&s| match policy {
                ReclaimPolicy::Storage => (segments.live_bytes(s), segments.seq(s)),
                ReclaimPolicy::Cache { .. } => (segments.seq(s), 0),
            })
    }

    // -- helpers -------------------------------------------------------------

    fn check_len(&self, len: u64) -> Result<()> {
        let max = self.shared.superblock.chunk_max as u64;
        if len > max {
            return Err(Error::ValueTooLarge { len, max });
        }
        Ok(())
    }

    fn exists(&self, id: &ChunkId, opts: PutOptions) -> bool {
        !opts.overwrite
            && (self.shared.index.get(id).is_some()
                || self.pending[slot(SegmentKind::Hot)].iter().any(|p| p.keys.contains(id))
                || self.inflight_keys.contains_key(id))
    }

    fn next_ids(&mut self) -> (Ticket, Lsn) {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        (ticket, self.take_lsn())
    }

    fn take_lsn(&mut self) -> Lsn {
        let lsn = self.next_lsn;
        self.next_lsn += 1;
        lsn
    }

    fn next_aux_token(&mut self) -> u64 {
        let t = self.next_aux_token;
        self.next_aux_token += 1;
        t | AUX_TOKEN
    }

    /// Allocates from the queue's pool, waiting for in-flight I/O to return
    /// buffers if necessary. Fails with [`Error::Busy`] only when nothing is
    /// in flight to wait for.
    fn alloc(&mut self, len: usize) -> Result<PooledBuf> {
        loop {
            if let Some(buf) = self.queue.pool().alloc(len) {
                return Ok(buf);
            }
            if self.queue.in_flight() == 0 {
                return Err(Error::Busy);
            }
            self.poll_io(true)?;
        }
    }

    // -- append path ---------------------------------------------------------

    /// Makes sure the pending batch of `(kind, batch)` can take a record with a
    /// value of `value_len` bytes, flushing it and allocating a fresh staging
    /// buffer as needed.
    fn reserve_small(&mut self, kind: SegmentKind, batch: BatchKind, value_len: usize) -> Result<()> {
        let (k, b) = (slot(kind), batch_slot(batch));
        let meta = record_meta_len(value_len as u32);
        let limit = self.shared.options.batch_limit;
        if self.pending[k][b].buf.is_some() && !self.pending[k][b].fits(meta, value_len) {
            self.flush_pending_kind(kind, batch)?;
        }
        if self.pending[k][b].buf.is_none() {
            let buf = self.alloc(limit)?;
            self.pending[k][b].attach(buf);
        }
        debug_assert!(self.pending[k][b].fits(meta, value_len));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn append_small(
        &mut self,
        kind: SegmentKind,
        batch: BatchKind,
        hdr: &RecordHeader,
        checksums: &[u32],
        value: &[u8],
        apply: Apply,
        ticket: Option<Ticket>,
    ) -> Result<()> {
        let (k, b) = (slot(kind), batch_slot(batch));
        self.pending[k][b].append(hdr, checksums, value, apply, ticket);
        if self.pending[k][b].len >= self.shared.options.batch_limit {
            self.flush_pending_kind(kind, batch)?;
        }
        Ok(())
    }

    fn write_large(
        &mut self,
        kind: SegmentKind,
        hdr: &RecordHeader,
        checksums: &[u32],
        mut buf: PooledBuf,
        apply: Apply,
        ticket: Option<Ticket>,
    ) -> Result<()> {
        let batch_len = large_batch_len(hdr.value_len) as usize;
        let value_off = large_value_offset(hdr.value_len) as usize;
        let meta = hdr.meta_len();
        hdr.encode(&mut buf[BATCH_HEADER_LEN..BATCH_HEADER_LEN + meta], checksums);
        // Padding between the headers and the value, and after the value, is
        // never parsed but must not leak stale buffer contents to disk.
        buf[BATCH_HEADER_LEN + meta..value_off].fill(0);
        buf[value_off + hdr.value_len as usize..batch_len].fill(0);

        let active = self.ensure_room(kind, batch_len as u64, 1)?;
        BatchHeader {
            seg_seq: active.seq,
            batch_len: batch_len as u32,
            record_count: 1,
            first_lsn: hdr.lsn,
            kind: BatchKind::Large,
            header_len: 0,
        }
        .encode(&mut buf[..BATCH_HEADER_LEN]);
        let record = PendingRecord {
            offset_in_batch: BATCH_HEADER_LEN as u32,
            value_in_batch: value_off as u32,
            entry: footer_entry(hdr, checksums),
            apply,
            ticket,
        };
        self.submit_batch(kind, buf, batch_len, vec![record])
    }

    /// Flushes both pending packed batches of `kind`.
    fn flush_pending(&mut self, kind: SegmentKind) -> Result<()> {
        self.flush_pending_kind(kind, BatchKind::Inline)?;
        self.flush_pending_kind(kind, BatchKind::Framed)
    }

    fn flush_pending_kind(&mut self, kind: SegmentKind, batch: BatchKind) -> Result<()> {
        let (k, b) = (slot(kind), batch_slot(batch));
        if self.pending[k][b].is_empty() {
            return Ok(());
        }
        let n = self.pending[k][b].records.len();
        let batch_len = align_up(self.pending[k][b].len as u64, PAGE_SIZE);
        let seq = self.ensure_room(kind, batch_len, n)?.seq;

        let mut pending = std::mem::replace(&mut self.pending[k][b], Pending::new(batch));
        let batch_len = pending.finish(seq);
        let buf = pending.buf.take().expect("non-empty pending has a buffer");
        self.submit_batch(kind, buf, batch_len, pending.records)
    }

    /// Submits a fully encoded batch at the tail of the active segment of
    /// `kind` (which the caller has already sized with `ensure_room`).
    fn submit_batch(
        &mut self,
        kind: SegmentKind,
        buf: PooledBuf,
        batch_len: usize,
        records: Vec<PendingRecord>,
    ) -> Result<()> {
        let k = slot(kind);
        let active = self.active[k]
            .as_mut()
            .expect("ensure_room allocated an active segment");
        let (seg_no, offset) = (active.seg_no, active.tail);
        active.tail += batch_len as u64;

        while self.queue.in_flight() >= self.queue.depth() {
            self.poll_io(true)?;
        }
        let token = self.next_batch_token;
        self.next_batch_token += 1;
        let device_offset = self.shared.geometry.segment_offset(seg_no) + offset;
        self.queue.write(buf, batch_len, device_offset, token)?;
        for rec in &records {
            *self.inflight_keys.entry(rec.entry.key).or_insert(0) += 1;
        }
        self.inflight.push_back(InFlight {
            token,
            slot: k,
            seg_no,
            offset,
            batch_len: batch_len as u64,
            records,
            result: None,
        });
        Ok(())
    }

    // -- completion path -----------------------------------------------------

    /// Reaps completions and applies every write whose turn has come.
    fn poll_io(&mut self, wait: bool) -> Result<()> {
        self.scratch.clear();
        self.queue.poll(&mut self.scratch, wait)?;
        for done in self.scratch.drain(..) {
            if done.token & AUX_TOKEN != 0 {
                self.aux_done.insert(done.token, done);
            } else {
                let front = self.inflight.front().expect("completion for an in-flight batch").token;
                let index = (done.token - front) as usize;
                // The buffer returns to the pool when `done` is dropped here.
                self.inflight[index].result = Some(done.result);
            }
        }
        while self.inflight.front().is_some_and(|f| f.result.is_some()) {
            let mut batch = self.inflight.pop_front().expect("front exists");
            let result = batch.result.take().expect("checked");
            self.apply_batch(batch, result);
        }
        Ok(())
    }

    fn apply_batch(&mut self, batch: InFlight, result: io::Result<usize>) {
        for rec in &batch.records {
            if let Some(n) = self.inflight_keys.get_mut(&rec.entry.key) {
                *n -= 1;
                if *n == 0 {
                    self.inflight_keys.remove(&rec.entry.key);
                }
            }
        }
        let active = self.active[batch.slot]
            .as_mut()
            .expect("active segment outlives its in-flight batches");
        debug_assert_eq!(active.seg_no, batch.seg_no);
        let failure = match result {
            Ok(n) if n as u64 == batch.batch_len => None,
            Ok(n) => Some(format!("short write: {n} of {} bytes", batch.batch_len)),
            Err(e) => Some(e.to_string()),
        };
        if failure.is_none() && !active.broken {
            for rec in batch.records {
                let mut entry = rec.entry;
                entry.offset = (batch.offset + rec.offset_in_batch as u64) as u32;
                entry.value_off = (batch.offset + rec.value_in_batch as u64) as u32;
                active.footer.push(entry);
                // A relocation that no longer applies was superseded by a
                // foreground write meanwhile; that is not a failure.
                apply_record(&self.shared, batch.seg_no, &entry, rec.apply);
                if let Some(ticket) = rec.ticket {
                    self.completions.push(Completion {
                        ticket,
                        lsn: entry.lsn,
                        result: Ok(()),
                    });
                }
            }
            return;
        }

        // Everything from the first failure onwards in this segment is lost;
        // truncate the segment there and abandon it.
        if !active.broken {
            active.broken = true;
            active.tail = batch.offset;
        }
        let message = failure.unwrap_or_else(|| "write after a failed write in the same segment".to_string());
        for rec in batch.records {
            if matches!(rec.apply, Apply::Relocate(_)) {
                self.relocation_failures += 1;
            }
            if let Some(ticket) = rec.ticket {
                self.completions.push(Completion {
                    ticket,
                    lsn: rec.entry.lsn,
                    result: Err(Error::Io(io::Error::other(message.clone()))),
                });
            }
        }
    }

    fn wait_inflight(&mut self) -> Result<()> {
        while !self.inflight.is_empty() {
            self.poll_io(true)?;
        }
        Ok(())
    }

    /// Waits for the auxiliary operation `token`.
    fn wait_aux(&mut self, token: u64) -> Result<IoCompletion> {
        loop {
            if let Some(done) = self.aux_done.remove(&token) {
                return Ok(done);
            }
            self.poll_io(true)?;
        }
    }

    /// Reads `len` bytes at `offset` within `seg_no` through the queue,
    /// applying write completions while waiting.
    fn read_blocking(&mut self, seg_no: u32, offset: u64, len: usize) -> Result<PooledBuf> {
        let buf = self.alloc(len)?;
        let token = self.next_aux_token();
        while self.queue.in_flight() >= self.queue.depth() {
            self.poll_io(true)?;
        }
        self.queue
            .read(buf, len, self.shared.geometry.segment_offset(seg_no) + offset, token)?;
        let done = self.wait_aux(token)?;
        match done.result {
            Ok(n) if n == len => Ok(done.buf.expect("read returns its buffer")),
            Ok(n) => Err(Error::Io(io::Error::other(format!("short read: {n} of {len} bytes")))),
            Err(e) => Err(Error::Io(e)),
        }
    }

    // -- segment lifecycle ---------------------------------------------------

    /// Returns the active segment of `kind` with room for a batch of
    /// `batch_len` bytes and `new_records` footer entries, sealing and
    /// allocating as needed.
    fn ensure_room(&mut self, kind: SegmentKind, batch_len: u64, new_records: usize) -> Result<&mut Active> {
        let k = slot(kind);
        let segment_size = self.shared.superblock.segment_size;
        let fits = self.active[k]
            .as_ref()
            .is_some_and(|a| a.fits(batch_len, new_records, segment_size));
        if !fits {
            if self.active[k].is_some() {
                // The footer must describe every record of the segment, so all
                // of its batches have to be applied first.
                self.wait_inflight()?;
                let active = self.active[k].take().expect("checked");
                self.seal(active)?;
            }
            let active = self.allocate(kind)?;
            if !active.fits(batch_len, new_records, segment_size) {
                // Only reachable if format validation was bypassed.
                return Err(Error::ValueTooLarge {
                    len: batch_len,
                    max: segment_size,
                });
            }
            self.active[k] = Some(active);
        }
        Ok(self.active[k].as_mut().expect("just ensured"))
    }

    fn allocate(&mut self, kind: SegmentKind) -> Result<Active> {
        let seg_no = self.free.pop_front().ok_or(Error::NoSpace)?;
        let seq = self.next_seq;
        self.next_seq += 1;
        self.shared.write_segment_header(&SegmentHeader {
            disk_uuid: self.shared.superblock.disk_uuid,
            seg_no,
            state: SegmentState::Active,
            kind,
            seq,
            footer_offset: 0,
            footer_len: 0,
            record_count: 0,
        })?;
        self.shared.segments.set(seg_no, SegmentState::Active, kind, seq);
        Ok(Active {
            seg_no,
            seq,
            kind,
            tail: self.shared.geometry.data_start(),
            footer: Vec::new(),
            broken: false,
        })
    }

    fn seal(&self, active: Active) -> Result<()> {
        seal_segment(
            &self.shared,
            active.seg_no,
            active.seq,
            active.kind,
            active.tail,
            &active.footer,
        )
    }

    // -- reclaim -------------------------------------------------------------

    fn oldest_live_seq(&self) -> Option<u64> {
        let segments = &self.shared.segments;
        segments
            .iter()
            .filter(|&s| segments.state(s) != SegmentState::Free)
            .map(|s| segments.seq(s))
            .min()
    }

    fn reclaim_segment(&mut self, seg_no: u32, policy: ReclaimPolicy) -> Result<ReclaimReport> {
        // Liveness is judged against the index, which must reflect every
        // accepted foreground write.
        self.flush_pending(SegmentKind::Hot)?;
        self.wait_inflight()?;

        let shared = self.shared.clone();
        let seg_header = shared.read_segment_header(seg_no)?;
        if seg_header.state != SegmentState::Sealed {
            return Err(Error::InvalidOption(format!("segment {seg_no} is not sealed")));
        }
        let is_oldest = self.oldest_live_seq() == Some(seg_header.seq);
        let failures_before = self.relocation_failures;
        let mut report = ReclaimReport {
            seg_no,
            ..Default::default()
        };

        let window = align_up(shared.options.scan_window as u64, PAGE_SIZE).max(self.max_batch) as usize;
        let end = seg_header.footer_offset;
        let mut cursor = shared.geometry.data_start();
        'scan: while cursor < end {
            let len = (end - cursor).min(window as u64) as usize;
            let buf = self.read_blocking(seg_no, cursor, len)?;
            let mut rel = 0usize;
            loop {
                match next_batch(
                    &buf[..len],
                    rel,
                    seg_header.seq,
                    self.max_batch,
                    end - cursor - rel as u64,
                ) {
                    BatchStep::Batch(batch, batch_len) => {
                        let batch_off = cursor + rel as u64;
                        // A corrupt batch aborts the pass without freeing
                        // anything: live records behind it could be lost.
                        for rec in parse_batch(&buf[rel..rel + batch_len], &batch, true)? {
                            let loc = Location {
                                seg_no,
                                offset: (batch_off + rec.offset_in_batch as u64) as u32,
                            };
                            report.records += 1;
                            // Reclaim is off the hot path; copying the checksum
                            // array out keeps the re-encoding API simple.
                            let checksums = rec.checksums.to_vec();
                            self.reclaim_record(
                                &rec.header,
                                &checksums,
                                rec.value,
                                loc,
                                policy,
                                is_oldest,
                                &mut report,
                            )?;
                        }
                        rel += batch_len;
                    }
                    BatchStep::NeedMore => break,
                    BatchStep::End => break 'scan,
                }
            }
            if rel == 0 {
                return Err(Error::corrupt(format!(
                    "segment {seg_no}: batch at {cursor} larger than the scan window"
                )));
            }
            cursor += rel as u64;
        }

        // Relocated records must be applied to the index before the victim
        // disappears from it, and every relocation must have succeeded.
        self.flush_pending(SegmentKind::Cold)?;
        self.wait_inflight()?;
        if self.relocation_failures != failures_before {
            return Err(Error::Io(io::Error::other(format!(
                "segment {seg_no}: relocation writes failed; segment kept"
            ))));
        }

        // Readers that looked the victim up before its entries were removed
        // hold a pin; wait for them to finish.
        while shared.segments.pins(seg_no) > 0 {
            thread::yield_now();
        }
        shared.write_segment_header(&SegmentHeader {
            disk_uuid: shared.superblock.disk_uuid,
            seg_no,
            state: SegmentState::Free,
            kind: seg_header.kind,
            seq: seg_header.seq,
            footer_offset: 0,
            footer_len: 0,
            record_count: 0,
        })?;
        shared
            .segments
            .set(seg_no, SegmentState::Free, seg_header.kind, seg_header.seq);
        shared.segments.reset_live(seg_no);
        self.free.push_back(seg_no);
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn reclaim_record(
        &mut self,
        hdr: &RecordHeader,
        checksums: &[u32],
        value: &[u8],
        loc: Location,
        policy: ReclaimPolicy,
        is_oldest: bool,
        report: &mut ReclaimReport,
    ) -> Result<()> {
        let shared = self.shared.clone();
        match hdr.kind {
            RecordKind::Data => {
                let Some(current) = shared.index.get(&hdr.key).filter(|v| v.loc == loc) else {
                    report.dropped += 1;
                    return Ok(());
                };
                let keep = !shared.is_expired(hdr.expire_at)
                    && match policy {
                        ReclaimPolicy::Storage => true,
                        ReclaimPolicy::Cache { reinsert_accessed } => reinsert_accessed && current.is_accessed(),
                    };
                if !keep {
                    shared.index.remove_if_at(&hdr.key, loc);
                    report.dropped += 1;
                    return Ok(());
                }
                let apply = Apply::Relocate(loc);
                if hdr.is_large() {
                    let mut large = self.prepare_large(hdr.value_len)?;
                    large.value_mut().copy_from_slice(value);
                    let lsn = self.take_lsn();
                    let flags = record_flags(BatchKind::Large, hdr.expire_at);
                    let new = header(RecordKind::Data, flags, hdr.value_len, lsn, hdr.key, hdr.expire_at);
                    self.write_large(SegmentKind::Cold, &new, checksums, large.buf, apply, None)?;
                } else {
                    let batch = small_batch_kind(hdr.value_len, hdr.expire_at);
                    self.reserve_small(SegmentKind::Cold, batch, value.len())?;
                    let lsn = self.take_lsn();
                    let flags = record_flags(batch, hdr.expire_at);
                    let new = header(RecordKind::Data, flags, hdr.value_len, lsn, hdr.key, hdr.expire_at);
                    self.append_small(SegmentKind::Cold, batch, &new, checksums, value, apply, None)?;
                }
                report.relocated += 1;
                report.bytes_relocated += value.len() as u64;
            }
            RecordKind::Tombstone => {
                // A live newer version supersedes the tombstone; and if this is
                // the oldest segment, no older data can exist.
                if shared.index.get(&hdr.key).is_some() || is_oldest {
                    report.tombstones_dropped += 1;
                } else {
                    self.reserve_small(SegmentKind::Cold, BatchKind::Inline, 0)?;
                    let lsn = self.take_lsn();
                    let new = header(RecordKind::Tombstone, 0, 0, lsn, hdr.key, 0);
                    self.append_small(SegmentKind::Cold, BatchKind::Inline, &new, &[], &[], Apply::None, None)?;
                    report.tombstones_relocated += 1;
                }
            }
        }
        Ok(())
    }
}

/// Updates the index and live-byte accounting for a record now on disk.
/// Returns whether the index now points at it.
fn apply_record(shared: &Shared, seg_no: u32, entry: &FooterEntry, apply: Apply) -> bool {
    let footprint = RecordGeometry::footprint(entry.value_len, entry.flags);
    let value = IndexValue {
        loc: Location {
            seg_no,
            offset: entry.offset,
        },
        value_off: entry.value_off,
        value_len: entry.value_len,
        flags: flags_from_record(entry.flags),
        crc: entry.crc,
        lsn: entry.lsn,
    };
    let segments = &shared.segments;
    match apply {
        Apply::Insert => match shared.index.insert_if_newer(entry.key, value) {
            InsertOutcome::Inserted => {
                segments.add_live(seg_no, footprint);
                true
            }
            InsertOutcome::Replaced(old) => {
                segments.sub_live(
                    old.loc.seg_no,
                    RecordGeometry::footprint(old.value_len, old.record_flags()),
                );
                segments.add_live(seg_no, footprint);
                true
            }
            InsertOutcome::Rejected => false,
        },
        Apply::Relocate(from) => {
            let moved = shared.index.replace_if_at(&entry.key, from, value);
            if moved {
                segments.add_live(seg_no, footprint);
            }
            moved
        }
        Apply::None => false,
    }
}

/// Writes a footer at `tail` and marks the segment sealed.
pub(crate) fn seal_segment(
    shared: &Shared,
    seg_no: u32,
    seq: u64,
    kind: SegmentKind,
    tail: u64,
    footer: &[FooterEntry],
) -> Result<()> {
    let len = footer_len(footer.len());
    let mut buf = moat_common::AlignedBuf::zeroed(len as usize);
    encode_footer(seq, footer, &mut buf);
    shared.write_segment_bytes(seg_no, tail, &buf)?;
    shared.write_segment_header(&SegmentHeader {
        disk_uuid: shared.superblock.disk_uuid,
        seg_no,
        state: SegmentState::Sealed,
        kind,
        seq,
        footer_offset: tail,
        footer_len: len,
        record_count: footer.len() as u64,
    })?;
    shared.segments.set_state(seg_no, SegmentState::Sealed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::prefer_framed;

    #[test]
    fn framing_decision_for_near_page_multiples() {
        assert!(prefer_framed(40942));
        assert_eq!(small_batch_kind(40942, 0), BatchKind::Framed);
        assert_eq!(small_batch_kind(40942, 5), BatchKind::Inline);
        assert_eq!(small_batch_kind(5000, 0), BatchKind::Inline);
    }
}

#[cfg(test)]
mod framed_tests {
    use std::sync::Arc;

    use moat_common::{HugePages, PoolOptions};

    use crate::{FormatOptions, MemDevice, Options, PutOptions, QueueOptions, format, open};

    #[test]
    fn near_page_multiple_value_is_framed_and_page_aligned() {
        let device = Arc::new(MemDevice::new(8 << 20));
        format(
            &*device,
            &FormatOptions {
                segment_size: 1 << 20,
                chunk_max: 128 << 10,
                disk_uuid: [1; 16],
            },
        )
        .unwrap();
        let opts = Options {
            queue: QueueOptions {
                depth: 8,
                pool: PoolOptions {
                    bytes: 8 << 20,
                    max_class: 1 << 20,
                    huge_pages: HugePages::Disabled,
                },
                force_sync: true,
            },
            ..Default::default()
        };
        let opened = open(device, opts).unwrap();
        let mut writer = opened.writer;
        let id = moat_common::ChunkId::from_u128(7);
        writer.put(id, &vec![0xabu8; 40942], PutOptions::default()).unwrap();
        writer.flush().unwrap();
        let v = writer.shared.index.get(&id).unwrap();
        assert!(v.is_framed(), "flags {:#x}", v.flags);
        assert_eq!(v.value_off % 4096, 0, "value offset {}", v.value_off);
    }
}
