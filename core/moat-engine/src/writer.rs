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
//! Exactly one [`Writer`] exists per engine. It owns the log tail of the hot
//! and cold active segments, assigns LSNs, performs every index mutation,
//! seals segments, and runs reclaim. Because all of that happens on one
//! thread, the engine needs no locking at all: the index is a single-writer
//! structure, and everything else the writer touches is owned by it.
//!
//! # Non-blocking with io_uring
//!
//! Every method either does memory work and returns, enqueues I/O on the
//! caller's [`IoQueue`] and returns a [`Ticket`], or reports
//! [`Error::Busy`] (the pool is out of buffers; poll and retry). Completion is
//! observed only through [`Writer::poll`], which also advances the state
//! machines behind sealing, barriers and reclaim. A slow disk therefore never
//! stalls the other disks that share its worker when using io_uring. The
//! synchronous development backend performs blocking I/O on this thread.
//!
//! # I/O pipeline
//!
//! Records are encoded straight into pool buffers: small values into a packed
//! staging batch (inline or framed, see [`crate::layout`]), large values into
//! a buffer whose value area the caller may fill directly
//! ([`Writer::prepare_large`]), so the only copy on the write path is the one
//! the caller chooses to make. Closed batches wait in a *ready* queue until
//! the I/O queue has room, then stay in flight; completions are applied
//! strictly in submission order, so acknowledged records always form a
//! contiguous prefix of the log and a crash never leaves a hole in front of
//! acknowledged data. A segment header is written before any batch of the
//! segment, a footer before the header that marks the segment sealed, and a
//! segment returns to the free list only after its header says so on disk.
//!
//! A failed write truncates its segment at the failure offset: that batch and
//! every later batch of the same segment are reported as failed, the segment
//! is sealed with the records that did land, and writing continues on a fresh
//! segment.
//!
//! # Reclaim and LSNs
//!
//! Relocated records keep their LSN. Reclaim is thereby a purely physical
//! move: the set of `(key, lsn, kind, value)` records recovery sees is
//! unchanged by it, so no ordering argument between reclaim and concurrent
//! foreground writes is needed, and the LSN a client observed for a chunk
//! stays valid across compaction. Whether a relocation takes effect in memory
//! is decided when it is applied (the index must still point at the old
//! location); a copy superseded meanwhile is dead data in the cold segment.
//!
//! # Source map
//!
//! This module owns writer state, admission, batch submission, and completion
//! ordering. `batch` encodes packed records, `sealing` manages segment lifecycle
//! and barriers, `reclaim` relocates live records, and `types` defines the public
//! operation types.

use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::{Arc, atomic::Ordering},
};

use moat_common::{
    CHECKSUM_BLOCK_SIZE, ChunkId, PAGE_SIZE, PooledBuf, align_up, block_checksums, block_count,
    chunk_id::ChunkIdHashBuilder, crc32c,
};

use crate::{
    error::{Error, Result},
    index::{IndexValue, IndexWriter, InsertOutcome, Location, flags_from_record},
    io::{Descriptor, IoCompletion, IoQueue},
    layout::{
        BATCH_HEADER_LEN, BatchHeader, BatchKind, FooterEntry, RECORD_FLAG_FRAMED, RECORD_FLAG_LARGE, RecordGeometry,
        RecordHeader, RecordKind, SEGMENT_HEADER_LEN, SegmentHeader, SegmentKind, SegmentState, footer_len,
        large_batch_len, large_value_offset, prefer_framed, record_meta_len,
    },
    scan::max_batch_len,
    shared::Shared,
};

mod batch;
mod reclaim;
mod sealing;
mod types;

use batch::{Pending, PendingRecord, footer_entry};
use reclaim::{ReclaimJob, ReclaimStage};
use sealing::{Barrier, Fsync, SealStage, Sealing};
pub use types::{Completion, DeleteOutcome, LargeValue, Lsn, Outcome, PutOptions, PutOutcome, ReclaimReport, Ticket};

/// Tokens with this bit set are auxiliary operations (headers, footers,
/// fsync, reclaim reads), not batch writes.
const AUX_TOKEN: u64 = 1 << 63;

// ---------------------------------------------------------------------------
// Segments under construction
// ---------------------------------------------------------------------------

struct Active {
    seg_no: u32,
    seq: u64,
    kind: SegmentKind,
    /// Offset of the next batch within the segment.
    tail: u64,
    /// Footer entries of every record applied so far.
    footer: Vec<FooterEntry>,
    /// A write into this segment failed; it is truncated at `tail` and
    /// abandoned.
    broken: bool,
    /// The header write that opened the segment has completed; batches may
    /// go to the device.
    opened: bool,
    /// Batches closed for this segment and not yet applied, and the records
    /// they hold: the footer must have room for them too.
    unapplied: u32,
    unapplied_records: usize,
}

impl Active {
    fn fits(&self, batch_len: u64, new_records: usize, segment_size: u64) -> bool {
        !self.broken
            && self.tail + batch_len + footer_len(self.footer.len() + self.unapplied_records + new_records)
                <= segment_size
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
    /// A tombstone: remove only versions at or below its LSN on completion.
    Tombstone,
}

/// An encoded batch with its position fixed, waiting for the queue or in
/// flight.
struct Batch {
    /// Close sequence number; barriers are expressed in terms of it.
    id: u64,
    seg_no: u32,
    offset: u64,
    batch_len: usize,
    records: Vec<PendingRecord>,
    /// The encoded bytes while the batch waits for the queue; `None` once the
    /// queue holds them.
    buf: Option<PooledBuf>,
}

struct InFlight {
    batch: Batch,
    token: u64,
    /// Set when the completion arrives; applied once every earlier batch has
    /// been applied.
    result: Option<io::Result<usize>>,
}

// ---------------------------------------------------------------------------
// Auxiliary operations and state machines
// ---------------------------------------------------------------------------

/// What an auxiliary I/O was for.
#[derive(Debug, Clone, Copy)]
enum Aux {
    /// The header that opens a fresh active segment.
    OpenHeader { seg_no: u32 },
    /// One piece of a footer.
    Footer { seg_no: u32 },
    /// The header that marks a segment sealed.
    SealHeader { seg_no: u32 },
    /// A barrier's `fdatasync`.
    Fsync { ticket: Ticket },
    /// A reclaim window read.
    ReclaimRead,
    /// The header that returns a reclaimed segment to the free list.
    FreeHeader { seg_no: u32 },
}

/// An auxiliary operation the queue had no room for yet.
struct AuxOp {
    aux: Aux,
    buf: PooledBuf,
    len: usize,
    offset: u64,
    read: bool,
}

/// A data record appended but not yet applied.
struct Unapplied {
    count: u32,
    max_lsn: Lsn,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// The single writer of an engine: owns the log tails, assigns LSNs, performs
/// every index mutation, seals segments and runs reclaim, all without blocking.
///
/// It is `Send` but not `Sync`: drive it from one thread, the one that owns
/// the queue it was attached to.
pub struct Writer {
    shared: Arc<Shared>,
    index: IndexWriter,
    desc: Descriptor,
    active: [Option<Active>; 2],
    /// Pending packed batches per segment kind: `[inline, framed]`.
    pending: [[Pending; 2]; 2],
    /// Closed batches waiting for queue room, per segment kind.
    ready: [VecDeque<Batch>; 2],
    inflight: VecDeque<InFlight>,
    aux: HashMap<u64, (Aux, usize)>,
    aux_ready: VecDeque<AuxOp>,
    sealing: Vec<Sealing>,
    barriers: VecDeque<Barrier>,
    reclaim: Option<ReclaimJob>,
    completions: Vec<Completion>,
    scratch: Vec<IoCompletion>,
    /// Data records appended but not yet applied, per key.
    unapplied: HashMap<ChunkId, Unapplied, ChunkIdHashBuilder>,
    /// Newest pending tombstone for the writer's admission view.
    deleted: HashMap<ChunkId, Lsn, ChunkIdHashBuilder>,
    /// Completed tombstones that must suppress older, still pending puts.
    applied_deletes: HashMap<ChunkId, Lsn, ChunkIdHashBuilder>,
    free: VecDeque<u32>,
    /// Batch write tokens are consecutive in submission order, so a
    /// completion is located in `inflight` by subtracting the front token.
    next_batch_token: u64,
    next_batch_id: u64,
    next_aux_token: u64,
    next_ticket: Ticket,
    next_seq: u64,
    next_lsn: Lsn,
    max_batch: u64,
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
pub(crate) fn small_batch_kind(value_len: u32) -> BatchKind {
    if prefer_framed(value_len) {
        BatchKind::Framed
    } else {
        BatchKind::Inline
    }
}

fn record_flags(batch: BatchKind) -> u8 {
    match batch {
        BatchKind::Large => RECORD_FLAG_LARGE,
        BatchKind::Framed => RECORD_FLAG_FRAMED,
        BatchKind::Inline => 0,
    }
}

fn header(kind: RecordKind, flags: u8, value_len: u32, lsn: Lsn, key: ChunkId) -> RecordHeader {
    RecordHeader {
        kind,
        flags,
        value_len,
        lsn,
        key,
    }
}

fn io_error(msg: impl Into<String>) -> Error {
    Error::Io(io::Error::other(msg.into()))
}

impl Writer {
    pub(crate) fn new(shared: Arc<Shared>, desc: Descriptor) -> Self {
        let max_batch = max_batch_len(&shared);
        let segments = &shared.segments;
        let free: VecDeque<u32> = segments
            .iter()
            .filter(|&s| segments.state(s) == SegmentState::Free)
            .collect();
        let next_seq = shared.next_seq.load(Ordering::Acquire);
        let next_lsn = shared.next_lsn.load(Ordering::Acquire);
        Self {
            index: IndexWriter::new(shared.index.clone()),
            shared,
            desc,
            active: [None, None],
            pending: [
                [Pending::new(BatchKind::Inline), Pending::new(BatchKind::Framed)],
                [Pending::new(BatchKind::Inline), Pending::new(BatchKind::Framed)],
            ],
            ready: [VecDeque::new(), VecDeque::new()],
            inflight: VecDeque::new(),
            aux: HashMap::new(),
            aux_ready: VecDeque::new(),
            sealing: Vec::new(),
            barriers: VecDeque::new(),
            reclaim: None,
            completions: Vec::new(),
            scratch: Vec::new(),
            unapplied: HashMap::default(),
            deleted: HashMap::default(),
            applied_deletes: HashMap::default(),
            free,
            next_batch_token: 0,
            next_batch_id: 1,
            next_aux_token: 0,
            next_ticket: 1,
            next_seq,
            next_lsn,
            max_batch,
        }
    }

    // -- data ---------------------------------------------------------------

    /// Hints the index location of an upcoming put or delete.
    ///
    /// Call this while processing an earlier request to overlap a cold index
    /// lookup with useful work. It does not reserve a slot, perform I/O, or
    /// change the result of any operation. Unsupported targets ignore it.
    #[inline]
    pub fn prefetch(&self, id: &ChunkId) {
        self.index.prefetch(id);
    }

    /// Appends a chunk, copying `value` into the log buffers.
    ///
    /// For values at or above the pack threshold, [`Writer::prepare_large`]
    /// followed by [`Writer::put_large`] avoids this copy.
    pub fn put(&mut self, q: &mut dyn IoQueue, id: ChunkId, value: &[u8], opts: PutOptions) -> Result<PutOutcome> {
        self.check_len(value.len() as u64)?;
        if self.check_exists(&id, opts)? {
            return Ok(PutOutcome::Exists);
        }
        self.check_index_room(&id)?;
        // Small puts need at most one checksum; avoid a heap allocation for
        // every packed record. Encoding consumes the slice before returning.
        let single;
        let multiple;
        let checksums: &[u32] = if value.len() <= CHECKSUM_BLOCK_SIZE {
            single = [crc32c(value)];
            &single[..usize::from(!value.is_empty())]
        } else {
            multiple = block_checksums(value);
            &multiple
        };
        let len = value.len() as u32;
        if value.len() >= self.shared.options.pack_threshold as usize {
            let mut large = self.prepare_large(q, len)?;
            large.value_mut().copy_from_slice(value);
            self.ensure_room(q, SegmentKind::Hot, large_batch_len(len), 1)?;
            let (ticket, lsn) = self.next_ids();
            let flags = record_flags(BatchKind::Large);
            let hdr = header(RecordKind::Data, flags, len, lsn, id);
            self.write_large(
                q,
                SegmentKind::Hot,
                &hdr,
                checksums,
                large.buf,
                Apply::Insert,
                Some(ticket),
            );
            Ok(PutOutcome::Written { ticket, lsn })
        } else {
            let batch = small_batch_kind(len);
            self.reserve_small(q, SegmentKind::Hot, batch, value.len())?;
            let (ticket, lsn) = self.next_ids();
            let flags = record_flags(batch);
            let hdr = header(RecordKind::Data, flags, len, lsn, id);
            self.append_small(
                q,
                SegmentKind::Hot,
                batch,
                &hdr,
                checksums,
                value,
                Apply::Insert,
                Some(ticket),
            );
            Ok(PutOutcome::Written { ticket, lsn })
        }
    }

    /// Allocates a buffer for a value of `value_len` bytes laid out as a large
    /// batch, so the value can be produced in place and written without a
    /// copy. `value_len` must be at least the pack threshold.
    pub fn prepare_large(&mut self, q: &mut dyn IoQueue, value_len: u32) -> Result<LargeValue> {
        self.check_len(value_len as u64)?;
        if value_len < self.shared.options.pack_threshold {
            return Err(Error::InvalidOption(format!(
                "large values must be at least {} bytes",
                self.shared.options.pack_threshold
            )));
        }
        let buf = q.pool().alloc(large_batch_len(value_len) as usize).ok_or(Error::Busy)?;
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
        q: &mut dyn IoQueue,
        id: ChunkId,
        value: LargeValue,
        checksums: Option<&[u32]>,
        opts: PutOptions,
    ) -> Result<PutOutcome> {
        self.check_len(value.len() as u64)?;
        if self.check_exists(&id, opts)? {
            return Ok(PutOutcome::Exists);
        }
        self.check_index_room(&id)?;
        let expected = block_count(value.len() as u64) as usize;
        let computed;
        let checksums = match checksums {
            Some(c) if c.len() == expected => c,
            Some(c) => {
                return Err(Error::InvalidOption(format!(
                    "expected {expected} block checksums, got {}",
                    c.len()
                )));
            }
            None => {
                computed = block_checksums(value.value());
                &computed
            }
        };
        self.ensure_room(q, SegmentKind::Hot, large_batch_len(value.len()), 1)?;
        let (ticket, lsn) = self.next_ids();
        let flags = record_flags(BatchKind::Large);
        let hdr = header(RecordKind::Data, flags, value.len(), lsn, id);
        self.write_large(
            q,
            SegmentKind::Hot,
            &hdr,
            checksums,
            value.buf,
            Apply::Insert,
            Some(ticket),
        );
        Ok(PutOutcome::Written { ticket, lsn })
    }

    /// Deletes a chunk by appending a tombstone.
    ///
    /// The chunk disappears from this writer's view at once (a subsequent put
    /// of the id is not `Exists`) and from readers' when the completion
    /// reports success. Use a flush barrier with the appropriate sync options
    /// when device durability is required.
    pub fn delete(&mut self, q: &mut dyn IoQueue, id: &ChunkId) -> Result<DeleteOutcome> {
        if self.deleted.contains_key(id) && !self.exists(id, PutOptions::default()) {
            return Err(Error::Busy);
        }
        if !self.exists(id, PutOptions::default()) {
            return Ok(DeleteOutcome::Missing);
        }
        self.reserve_small(q, SegmentKind::Hot, BatchKind::Inline, 0)?;
        let (ticket, lsn) = self.next_ids();
        self.deleted.insert(*id, lsn);
        let hdr = header(RecordKind::Tombstone, 0, 0, lsn, *id);
        self.append_small(
            q,
            SegmentKind::Hot,
            BatchKind::Inline,
            &hdr,
            &[],
            &[],
            Apply::Tombstone,
            Some(ticket),
        );
        Ok(DeleteOutcome::Deleted { ticket, lsn })
    }

    // -- progress -----------------------------------------------------------

    /// Applies completions that arrived for this writer, advances the sealing,
    /// barrier and reclaim state machines, submits whatever became ready
    /// (including any partially filled pending batch), and appends the
    /// resulting [`Completion`]s to `out`. With io_uring, never waits. Returns the number
    /// appended.
    pub fn poll(&mut self, q: &mut dyn IoQueue, out: &mut Vec<Completion>) -> Result<usize> {
        self.scratch.clear();
        q.take(self.desc, &mut self.scratch);
        let mut finished = std::mem::take(&mut self.scratch);
        for done in finished.drain(..) {
            if done.token & AUX_TOKEN != 0 {
                if let Some((aux, len)) = self.aux.remove(&done.token) {
                    self.finish_aux(aux, len, done);
                }
            } else if let Some(front) = self.inflight.front() {
                let index = (done.token - front.token) as usize;
                // The buffer returns to the pool when `done` is dropped here.
                self.inflight[index].result = Some(done.result);
            }
        }
        self.scratch = finished;
        while self.inflight.front().is_some_and(|f| f.result.is_some()) {
            let mut front = self.inflight.pop_front().expect("front exists");
            let result = front.result.take().expect("checked");
            self.apply_batch(front.batch, result);
        }

        self.advance_sealing(q);
        self.advance_barriers(q);
        self.advance_reclaim(q);
        self.index.gc();

        // Close whatever accumulated during this iteration; a packing window
        // is one worker iteration. A batch that cannot be placed yet stays
        // pending until the next poll.
        let _ = self.close_all_pending(q);
        self.push_ready(q);

        let n = self.completions.len();
        out.append(&mut self.completions);
        Ok(n)
    }

    /// Batches and auxiliary operations accepted but not yet completed.
    pub fn in_flight(&self) -> usize {
        self.inflight.len()
            + self.ready.iter().map(VecDeque::len).sum::<usize>()
            + self.aux.len()
            + self.aux_ready.len()
    }

    /// Whether nothing is pending: no unwritten records, no I/O in flight, no
    /// sealing, barrier or reclaim in progress. Safe to `detach` when true.
    pub fn is_idle(&self) -> bool {
        self.in_flight() == 0
            && self.pending.iter().all(|p| p.iter().all(Pending::is_empty))
            && self.sealing.is_empty()
            && self.barriers.is_empty()
            && self.reclaim.is_none()
    }

    /// Number of free segments.
    pub fn free_segments(&self) -> u32 {
        self.free.len() as u32
    }

    /// The LSN the next record will receive.
    pub fn next_lsn(&self) -> Lsn {
        self.next_lsn
    }

    /// Closes the descriptor. Call after `seal` has completed and
    /// [`Writer::is_idle`] is true; otherwise in-flight batches are abandoned
    /// (recovery handles that, but the segment is scanned on next open).
    pub fn detach(self, q: &mut dyn IoQueue) {
        q.detach(self.desc);
        drop(self);
    }

    // -- helpers -------------------------------------------------------------

    fn check_len(&self, len: u64) -> Result<()> {
        let max = self.shared.superblock.chunk_max as u64;
        if len > max {
            return Err(Error::ValueTooLarge { len, max });
        }
        Ok(())
    }

    /// A put of a key the index does not hold needs an index slot.
    fn check_index_room(&self, id: &ChunkId) -> Result<()> {
        if self.index.is_full() && self.index.get(id).is_none() {
            return Err(Error::IndexFull);
        }
        Ok(())
    }

    fn exists(&self, id: &ChunkId, opts: PutOptions) -> bool {
        if opts.overwrite {
            return false;
        }
        let deleted_at = self
            .deleted
            .get(id)
            .copied()
            .unwrap_or(0)
            .max(self.applied_deletes.get(id).copied().unwrap_or(0));
        self.index.get(id).is_some_and(|v| v.lsn > deleted_at)
            || self.unapplied.get(id).is_some_and(|u| u.max_lsn > deleted_at)
    }

    fn check_exists(&self, id: &ChunkId, opts: PutOptions) -> Result<bool> {
        if !self.exists(id, opts) {
            return Ok(false);
        }
        let deleted_at = self.deleted.get(id).copied().unwrap_or(0);
        if self.index.get(id).is_some_and(|v| v.lsn > deleted_at) {
            Ok(true)
        } else {
            // A duplicate cannot acknowledge a write that may still fail.
            Err(Error::Busy)
        }
    }

    /// Visits the completed live inventory while holding the disk's unique
    /// writer. No disk I/O is performed; pending mutations are excluded.
    pub fn visit_chunks(&self, mut visitor: impl FnMut(ChunkId, crate::ChunkStat)) {
        self.index
            .for_each(|id, value| visitor(*id, crate::ChunkStat::from_value(value)));
    }

    fn next_ids(&mut self) -> (Ticket, Lsn) {
        let ticket = self.next_ticket();
        (ticket, self.take_lsn())
    }

    fn next_ticket(&mut self) -> Ticket {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        ticket
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

    fn complete(&mut self, ticket: Ticket, result: Result<Outcome>) {
        self.completions.push(Completion { ticket, result });
    }

    /// The segment record for `seg_no`, whether active or being sealed.
    fn segment_mut(&mut self, seg_no: u32) -> Option<&mut Active> {
        if let Some(a) = self.active.iter_mut().flatten().find(|a| a.seg_no == seg_no) {
            return Some(a);
        }
        self.sealing.iter_mut().map(|s| &mut s.seg).find(|s| s.seg_no == seg_no)
    }

    fn segment(&self, seg_no: u32) -> Option<&Active> {
        if let Some(a) = self.active.iter().flatten().find(|a| a.seg_no == seg_no) {
            return Some(a);
        }
        self.sealing.iter().map(|s| &s.seg).find(|s| s.seg_no == seg_no)
    }

    /// The lowest id among batches not yet applied, if any.
    fn oldest_unapplied(&self) -> Option<u64> {
        self.ready
            .iter()
            .filter_map(|r| r.front().map(|b| b.id))
            .chain(self.inflight.iter().map(|f| f.batch.id))
            .min()
    }

    // -- append path ---------------------------------------------------------

    /// Makes sure the pending batch of `(kind, batch)` can take a record with a
    /// value of `value_len` bytes, closing it when full and attaching a fresh
    /// staging buffer. A pending batch is placed in a segment only when it is
    /// closed, so building one needs no segment room.
    fn reserve_small(
        &mut self,
        q: &mut dyn IoQueue,
        kind: SegmentKind,
        batch: BatchKind,
        value_len: usize,
    ) -> Result<()> {
        let (k, b) = (slot(kind), batch_slot(batch));
        let meta = record_meta_len(value_len as u32);
        let limit = self.shared.options.batch_limit;
        if self.pending[k][b].buf.is_some() && !self.pending[k][b].fits(meta, value_len) {
            self.close_pending(q, k, b)?;
        }
        if self.pending[k][b].buf.is_none() {
            let buf = q.pool().alloc(limit).ok_or(Error::Busy)?;
            self.pending[k][b].attach(buf);
        }
        debug_assert!(self.pending[k][b].fits(meta, value_len));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn append_small(
        &mut self,
        q: &mut dyn IoQueue,
        kind: SegmentKind,
        batch: BatchKind,
        hdr: &RecordHeader,
        checksums: &[u32],
        value: &[u8],
        apply: Apply,
        ticket: Option<Ticket>,
    ) {
        let (k, b) = (slot(kind), batch_slot(batch));
        self.track(hdr, apply);
        self.pending[k][b].append(hdr, checksums, value, apply, ticket);
        if self.pending[k][b].len >= self.shared.options.batch_limit {
            // A batch that cannot be placed now (no free segment, no buffer
            // for a segment header) stays pending and is retried by `poll`.
            if self.close_pending(q, k, b).is_ok() {
                self.push_ready(q);
            }
        }
    }

    fn track(&mut self, hdr: &RecordHeader, apply: Apply) {
        if matches!(apply, Apply::Insert) {
            let u = self
                .unapplied
                .entry(hdr.key)
                .or_insert(Unapplied { count: 0, max_lsn: 0 });
            u.count += 1;
            u.max_lsn = u.max_lsn.max(hdr.lsn);
        }
    }

    /// Encodes a large batch and queues it. The caller has reserved room with
    /// `ensure_room`.
    #[allow(clippy::too_many_arguments)]
    fn write_large(
        &mut self,
        q: &mut dyn IoQueue,
        kind: SegmentKind,
        hdr: &RecordHeader,
        checksums: &[u32],
        mut buf: PooledBuf,
        apply: Apply,
        ticket: Option<Ticket>,
    ) {
        let batch_len = large_batch_len(hdr.value_len) as usize;
        let value_off = large_value_offset(hdr.value_len) as usize;
        let meta = hdr.meta_len();
        hdr.encode(&mut buf[BATCH_HEADER_LEN..BATCH_HEADER_LEN + meta], checksums);
        // Padding between the headers and the value, and after the value, is
        // never parsed but must not leak stale buffer contents to disk.
        buf[BATCH_HEADER_LEN + meta..value_off].fill(0);
        buf[value_off + hdr.value_len as usize..batch_len].fill(0);

        let active = self.active[slot(kind)].as_ref().expect("room ensured");
        let (seg_no, seq) = (active.seg_no, active.seq);
        BatchHeader {
            seg_seq: seq,
            batch_len: batch_len as u32,
            record_count: 1,
            first_lsn: hdr.lsn,
            kind: BatchKind::Large,
            header_len: 0,
        }
        .encode(&mut buf[..BATCH_HEADER_LEN]);
        self.track(hdr, apply);
        let record = PendingRecord {
            entry: footer_entry(hdr, checksums, BATCH_HEADER_LEN, value_off),
            apply,
            ticket,
        };
        self.enqueue_batch(slot(kind), seg_no, buf, batch_len, vec![record]);
        self.push_ready(q);
    }

    /// Closes every non-empty pending batch. A batch that cannot be placed
    /// (no free segment, no buffer for a segment header) stays pending; the
    /// first such error is returned after the others were tried.
    fn close_all_pending(&mut self, q: &mut dyn IoQueue) -> Result<()> {
        let mut first = Ok(());
        for k in 0..2 {
            for b in 0..2 {
                if let Err(e) = self.close_pending(q, k, b)
                    && first.is_ok()
                {
                    first = Err(e);
                }
            }
        }
        first
    }

    /// Closes the pending batch `(k, b)` into the ready queue at the tail of
    /// the active segment of its kind.
    fn close_pending(&mut self, q: &mut dyn IoQueue, k: usize, b: usize) -> Result<()> {
        if self.pending[k][b].is_empty() {
            return Ok(());
        }
        let kind = if k == 0 { SegmentKind::Hot } else { SegmentKind::Cold };
        let n = self.pending[k][b].records.len();
        let batch_len = align_up(self.pending[k][b].len as u64, PAGE_SIZE);
        self.ensure_room(q, kind, batch_len, n)?;
        let active = self.active[k].as_ref().expect("room ensured");
        let (seg_no, seq) = (active.seg_no, active.seq);
        let batch_kind = self.pending[k][b].kind;
        let mut pending = std::mem::replace(&mut self.pending[k][b], Pending::new(batch_kind));
        let batch_len = pending.finish(seq);
        let buf = pending.buf.take().expect("non-empty pending has a buffer");
        self.enqueue_batch(k, seg_no, buf, batch_len, pending.records);
        Ok(())
    }

    /// Fixes a fully encoded batch at the tail of `seg_no` and appends it to
    /// the ready queue of kind `k`.
    fn enqueue_batch(
        &mut self,
        k: usize,
        seg_no: u32,
        buf: PooledBuf,
        batch_len: usize,
        mut records: Vec<PendingRecord>,
    ) {
        let seg = self.segment_mut(seg_no).expect("segment exists");
        let offset = seg.tail;
        for rec in &mut records {
            rec.entry.offset += offset as u32;
            rec.entry.value_off += offset as u32;
        }
        seg.tail += batch_len as u64;
        seg.unapplied += 1;
        seg.unapplied_records += records.len();
        let id = self.next_batch_id;
        self.next_batch_id += 1;
        self.ready[k].push_back(Batch {
            id,
            seg_no,
            offset,
            batch_len,
            records,
            buf: Some(buf),
        });
    }

    /// Pushes deferred auxiliary operations and ready batches (foreground
    /// first) while the queue has room.
    fn push_ready(&mut self, q: &mut dyn IoQueue) {
        // Auxiliary operations first: segment headers gate the batches behind
        // them, footers and free headers gate reclaim.
        while q.vacant() > 0 {
            let Some(op) = self.aux_ready.pop_front() else {
                break;
            };
            if let Some(op) = self.try_enqueue_aux(q, op) {
                self.aux_ready.push_front(op);
                break;
            }
        }
        for k in 0..2 {
            while let Some(front) = self.ready[k].front() {
                let seg_no = front.seg_no;
                let seg = self.segment(seg_no).expect("ready batch for a known segment");
                if seg.broken {
                    // Everything from the failure onwards in this segment is
                    // lost; nothing beyond the truncated tail is written.
                    let batch = self.ready[k].pop_front().expect("front exists");
                    let seg = self.segment_mut(seg_no).expect("known segment");
                    seg.unapplied -= 1;
                    seg.unapplied_records -= batch.records.len();
                    self.fail_batch(batch, "write after a failed write in the same segment");
                    continue;
                }
                if !seg.opened || q.vacant() == 0 {
                    break;
                }
                let mut batch = self.ready[k].pop_front().expect("front exists");
                let buf = batch.buf.take().expect("ready batch holds its buffer");
                let token = self.next_batch_token;
                let device_offset = self.shared.geometry.segment_offset(batch.seg_no) + batch.offset;
                match q.write(self.desc, buf, batch.batch_len, device_offset, token) {
                    Ok(()) => {
                        self.next_batch_token += 1;
                        self.inflight.push_back(InFlight {
                            batch,
                            token,
                            result: None,
                        });
                    }
                    Err(buf) => {
                        batch.buf = Some(buf);
                        self.ready[k].push_front(batch);
                        break;
                    }
                }
            }
        }
    }

    // -- completion path -----------------------------------------------------

    fn apply_batch(&mut self, batch: Batch, result: io::Result<usize>) {
        let failure = match result {
            Ok(n) if n == batch.batch_len => None,
            Ok(n) => Some(format!("short write: {n} of {} bytes", batch.batch_len)),
            Err(e) => Some(e.to_string()),
        };
        let seg_no = batch.seg_no;
        let broken = {
            let seg = self.segment_mut(seg_no).expect("segment outlives its batches");
            seg.unapplied -= 1;
            seg.unapplied_records -= batch.records.len();
            seg.broken
        };
        if failure.is_none() && !broken {
            self.segment_mut(seg_no)
                .expect("segment outlives its batches")
                .footer
                .extend(batch.records.iter().map(|rec| rec.entry));
            for rec in batch.records {
                let entry = rec.entry;
                let result = self.apply_record(seg_no, &entry, rec.apply);
                if let Some(ticket) = rec.ticket {
                    let outcome = match rec.apply {
                        Apply::Tombstone => Outcome::Delete { lsn: entry.lsn },
                        _ => Outcome::Put { lsn: entry.lsn },
                    };
                    self.complete(ticket, result.map(|()| outcome));
                }
            }
            return;
        }
        // Everything from the first failure onwards in this segment is lost;
        // truncate the segment there and abandon it.
        let seg = self.segment_mut(seg_no).expect("segment outlives its batches");
        if !seg.broken {
            seg.broken = true;
            seg.tail = batch.offset;
        }
        let message = failure.unwrap_or_else(|| "write after a failed write in the same segment".to_string());
        self.fail_batch(batch, &message);
    }

    fn fail_batch(&mut self, batch: Batch, message: &str) {
        for rec in batch.records {
            self.untrack(&rec.entry, rec.apply);
            if rec.ticket.is_none()
                && let Some(job) = &mut self.reclaim
            {
                job.failed = true;
            }
            if let Some(ticket) = rec.ticket {
                self.complete(ticket, Err(io_error(message)));
            }
        }
        for b in &mut self.barriers {
            b.error.get_or_insert_with(|| message.to_string());
        }
    }

    fn untrack(&mut self, entry: &FooterEntry, apply: Apply) {
        match apply {
            Apply::Insert => {
                if let Some(u) = self.unapplied.get_mut(&entry.key) {
                    u.count -= 1;
                    if u.count == 0 {
                        self.unapplied.remove(&entry.key);
                        self.applied_deletes.remove(&entry.key);
                    }
                }
            }
            Apply::Tombstone => {
                if self.deleted.get(&entry.key) == Some(&entry.lsn) {
                    self.deleted.remove(&entry.key);
                }
            }
            Apply::Relocate(_) => {}
        }
    }

    /// Updates the index and live-byte accounting for a record now on disk.
    fn apply_record(&mut self, seg_no: u32, entry: &FooterEntry, apply: Apply) -> Result<()> {
        let footprint = RecordGeometry::footprint(entry.value_len, entry.flags);
        let value = IndexValue {
            loc: Location {
                seg_no,
                offset: entry.offset,
            },
            value_off: entry.value_off,
            value_len: entry.value_len,
            flags: flags_from_record(entry.flags),
            lsn: entry.lsn,
        };
        let segments = &self.shared.segments;
        let mut result = Ok(());
        match apply {
            Apply::Insert => {
                // Only completed tombstones suppress a put. A pending delete
                // may fail and must not hide an acknowledged version.
                let superseded = self.applied_deletes.get(&entry.key).is_some_and(|lsn| *lsn > entry.lsn);
                if !superseded {
                    match self.index.insert_if_newer(entry.key, value) {
                        InsertOutcome::Inserted => segments.add_live(seg_no, footprint),
                        InsertOutcome::Replaced(old) => {
                            segments.sub_live(
                                old.loc.seg_no,
                                RecordGeometry::footprint(old.value_len, old.record_flags()),
                            );
                            segments.add_live(seg_no, footprint);
                        }
                        // An even newer version was applied first.
                        InsertOutcome::Rejected => {}
                        // The budget was checked at put time; a rebuild that
                        // failed meanwhile leaves the record unindexed.
                        InsertOutcome::Full => result = Err(Error::IndexFull),
                    }
                }
            }
            Apply::Relocate(from) => {
                // A relocation that no longer applies was superseded by a
                // foreground write meanwhile; that is not a failure.
                if self.index.replace_if_at(&entry.key, from, value) {
                    segments.add_live(seg_no, footprint);
                }
            }
            Apply::Tombstone => {
                if self.index.get(&entry.key).is_some_and(|v| v.lsn <= entry.lsn) {
                    let old = self.index.remove(&entry.key).expect("entry exists");
                    segments.sub_live(
                        old.loc.seg_no,
                        RecordGeometry::footprint(old.value_len, old.record_flags()),
                    );
                }
                if self.unapplied.contains_key(&entry.key) {
                    let watermark = self.applied_deletes.entry(entry.key).or_default();
                    *watermark = (*watermark).max(entry.lsn);
                }
            }
        }
        self.untrack(entry, apply);
        result
    }

    // -- auxiliary I/O --------------------------------------------------------

    /// Enqueues an auxiliary operation now, or defers it until the queue has
    /// room.
    fn enqueue_aux(&mut self, q: &mut dyn IoQueue, op: AuxOp) {
        if let Some(op) = self.try_enqueue_aux(q, op) {
            self.aux_ready.push_back(op);
        }
    }

    fn try_enqueue_aux(&mut self, q: &mut dyn IoQueue, op: AuxOp) -> Option<AuxOp> {
        let token = self.next_aux_token();
        let AuxOp {
            aux,
            buf,
            len,
            offset,
            read,
        } = op;
        let outcome = if read {
            q.read(self.desc, buf, len, offset, token)
        } else {
            q.write(self.desc, buf, len, offset, token)
        };
        match outcome {
            Ok(()) => {
                self.aux.insert(token, (aux, len));
                None
            }
            Err(buf) => Some(AuxOp {
                aux,
                buf,
                len,
                offset,
                read,
            }),
        }
    }

    /// Encodes `header` into a pool buffer and queues it as `aux`.
    fn write_header(&mut self, q: &mut dyn IoQueue, header: &SegmentHeader, aux: Aux) -> Result<()> {
        let mut buf = q.pool().alloc(SEGMENT_HEADER_LEN as usize).ok_or(Error::Busy)?;
        header.encode(&mut buf[..SEGMENT_HEADER_LEN as usize]);
        let offset = self.shared.geometry.segment_offset(header.seg_no);
        self.enqueue_aux(
            q,
            AuxOp {
                aux,
                buf,
                len: SEGMENT_HEADER_LEN as usize,
                offset,
                read: false,
            },
        );
        Ok(())
    }

    fn finish_aux(&mut self, aux: Aux, len: usize, mut done: IoCompletion) {
        if let Ok(n) = done.result
            && n != len
        {
            done.result = Err(io::Error::other(format!("short I/O: {n} of {len} bytes")));
        }
        let failure = match &done.result {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        match aux {
            Aux::OpenHeader { seg_no } => {
                if let Some(seg) = self.segment_mut(seg_no) {
                    seg.opened = true;
                    // A segment whose header may not be on disk is not
                    // recoverable: abandon it; its batches fail at push time.
                    if failure.is_some() {
                        seg.broken = true;
                    }
                }
            }
            Aux::Footer { seg_no } => {
                if let Some(s) = self.sealing.iter_mut().find(|s| s.seg.seg_no == seg_no)
                    && let SealStage::Footer { outstanding, .. } = &mut s.stage
                {
                    *outstanding -= 1;
                }
                // A failed footer piece is not fatal: recovery falls back to a
                // scan when the footer does not validate.
            }
            Aux::SealHeader { seg_no } => {
                if let Some(pos) = self.sealing.iter().position(|s| s.seg.seg_no == seg_no) {
                    let sealing = self.sealing.swap_remove(pos);
                    // Even if the header write failed the data is intact; the
                    // in-memory state moves on and recovery scans the segment.
                    self.shared.segments.set_sealed(seg_no, sealing.seg.tail);
                }
            }
            Aux::Fsync { ticket } => {
                if let Some(b) = self.barriers.iter_mut().find(|b| b.ticket == ticket) {
                    b.fsync = Fsync::Done;
                    if let Some(e) = failure {
                        b.error.get_or_insert(e);
                    }
                }
            }
            Aux::ReclaimRead => {
                let Some(job) = &mut self.reclaim else {
                    return;
                };
                match (done.result, done.buf) {
                    (Ok(n), Some(buf)) => {
                        job.stage = ReclaimStage::Process {
                            buf,
                            len: n,
                            rel: 0,
                            rec: 0,
                        };
                    }
                    (Err(e), _) => {
                        let ticket = job.ticket;
                        self.reclaim = None;
                        self.complete(ticket, Err(Error::Io(e)));
                    }
                    (Ok(_), None) => unreachable!("reads return their buffer"),
                }
            }
            Aux::FreeHeader { seg_no } => {
                let Some(job) = self.reclaim.take() else {
                    return;
                };
                debug_assert_eq!(job.seg_no, seg_no);
                match failure {
                    None => {
                        let segments = &self.shared.segments;
                        segments.set(seg_no, SegmentState::Free, job.kind, job.seq);
                        segments.reset_live(seg_no);
                        self.free.push_back(seg_no);
                        self.complete(job.ticket, Ok(Outcome::Reclaim(job.report)));
                    }
                    Some(e) => self.complete(job.ticket, Err(io_error(e))),
                }
            }
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.shared.next_lsn.store(self.next_lsn, Ordering::Release);
        self.shared.next_seq.store(self.next_seq, Ordering::Release);
        self.shared.writer_taken.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::prefer_framed;

    #[test]
    fn short_auxiliary_io_fails_without_reclaiming_live_data() {
        use moat_common::{HugePages, PoolOptions};

        use crate::{
            FormatOptions, MemDevice, Options, QueueOptions, blocking,
            io::{CompletionOrder, SyncQueue},
        };

        for reclaim in [false, true] {
            let device = Arc::new(MemDevice::new(5 << 20));
            crate::format(
                &*device,
                &FormatOptions {
                    segment_size: 1 << 20,
                    chunk_max: 64 << 10,
                    ..Default::default()
                },
            )
            .unwrap();
            let (engine, _) = crate::open(
                device,
                Options {
                    index_capacity: 64,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut queue = SyncQueue::new(
                &QueueOptions {
                    pool: PoolOptions {
                        bytes: 2 << 20,
                        max_class: 1 << 20,
                        huge_pages: HugePages::Disabled,
                    },
                    ..Default::default()
                },
                CompletionOrder::Fifo,
            )
            .unwrap();
            let mut writer = engine.writer(&mut queue).unwrap();
            let key = ChunkId::from_u128(1);
            writer.put(&mut queue, key, b"value", PutOptions::default()).unwrap();
            let ticket = if reclaim {
                blocking::seal(&mut queue, &mut writer).unwrap();
                writer.reclaim(&mut queue).unwrap().unwrap()
            } else {
                writer.flush(&mut queue).unwrap()
            };
            queue.poll(false).unwrap();
            let mut done = Vec::new();
            assert_eq!(queue.take(writer.desc, &mut done), 1);
            let mut completion = done.pop().unwrap();
            completion.result = Ok(0);
            let (aux, len) = writer.aux.remove(&completion.token).unwrap();
            writer.finish_aux(aux, len, completion);
            assert!(blocking::wait(&mut queue, &mut writer, ticket).is_err());
            if reclaim {
                assert_eq!(engine.usage().sealed_segments, 1);
                let mut reader = engine.reader(&mut queue).unwrap();
                assert_eq!(
                    &*blocking::get(&mut queue, &mut reader, &key, None).unwrap().unwrap(),
                    b"value"
                );
            } else {
                assert!(!engine.contains(&key));
            }
        }
    }

    #[test]
    fn framing_decision_for_near_page_multiples() {
        assert!(prefer_framed(40942));
        assert_eq!(small_batch_kind(40942), BatchKind::Framed);
        assert_eq!(small_batch_kind(5000), BatchKind::Inline);
    }
}
