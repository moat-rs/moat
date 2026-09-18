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

//! Single-owner reads, ordered write publication, and durability barriers.
//!
//! A pipeline serves one explicitly assigned segment extent on an exclusive I/O
//! queue. The upper layer owns device format, segment selection, and lifecycle.
//! There are no shared mutable indexes, locks, channels, or per-ticket atomics.
//! A successful write completion means readable, not durable; `flush` supplies
//! the persistence barrier unless the owning engine disables device syncs.
//! Recovered allocations are read-only.

mod driver;
mod error;
mod index;
mod maintenance;
mod options;
mod read;
mod verify;
mod write;

use std::collections::VecDeque;

pub use error::{Error, Rejected, Result};
use index::{Index, Location};
use moat_common::{ChunkId, PAGE_SIZE, is_aligned};
pub use options::{PollBudget, ResourceLimits};
use read::{Read, ReadExtent};
pub use read::{ReadBuffers, ReadRange, ReadRequirements};
use verify::VerifiedRead;

use crate::{
    frame::{FrameLimits, FramePosition, Metadata, RecordKind},
    io::{Buffer, Operation, Queue, Request},
    segment::{SegmentBuilder, SegmentHeader},
};

/// Completion identity, scoped to the pipeline that issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticket(u64);

impl Ticket {
    /// Monotonically increasing number within this pipeline.
    pub fn number(self) -> u64 {
        self.0
    }
}

/// An operation result and its reusable buffers, including on failure.
#[derive(Debug)]
pub enum Completion {
    /// Frame write is complete and its records have been applied by LSN on success.
    Write {
        /// Admission identity.
        ticket: Ticket,
        /// Publication outcome. Success does not imply persistence.
        result: Result<()>,
        /// Original frame buffer.
        buffer: Buffer,
    },
    /// Read of the version visible at admission, with the requested verification policy.
    Read {
        /// Admission identity.
        ticket: Ticket,
        /// Requested bytes in either buffer, or an I/O/verification failure.
        result: Result<ReadRange>,
        /// Original buffers, returned on both success and failure.
        buffers: ReadBuffers,
    },
    /// Terminal notification after queue failure. Submitted buffers remain owned
    /// by the queue until safe teardown; this event does not transfer them.
    Failed {
        /// Accepted operation identity.
        ticket: Ticket,
        /// Shared fatal cause.
        error: std::sync::Arc<Error>,
    },
    /// All preceding writes and the configured optional persistence barrier completed.
    Flush {
        /// Admission identity.
        ticket: Ticket,
        /// Write-fence and optional durability outcome.
        result: Result<()>,
    },
}

impl Completion {
    /// The admission identity regardless of operation kind.
    pub fn ticket(&self) -> Ticket {
        match self {
            Self::Write { ticket, .. }
            | Self::Read { ticket, .. }
            | Self::Flush { ticket, .. }
            | Self::Failed { ticket, .. } => *ticket,
        }
    }
}

struct Write {
    ticket: Ticket,
    entries: Vec<(ChunkId, Location)>,
    reserved_new: usize,
    completed: Option<(Result<()>, Buffer)>,
}

enum Pending {
    Write(Write),
    Read(Read),
    VerifiedRead(VerifiedRead),
    EmptyRead { ticket: Ticket, buffers: ReadBuffers },
    Flush { ticket: Ticket, submitted: bool },
}

struct Extent {
    header: SegmentHeader,
    base: u64,
}

/// A bounded pipeline whose queue, index, and publication order share one owner.
///
/// Different workers can own independent pipelines without locks. A returned
/// read is a snapshot of the index at admission; newer writes do not invalidate
/// its immutable storage. Segment reuse and concurrent external modification are
/// prohibited while this pipeline or any read into its storage remains live.
pub struct Pipeline<Q> {
    queue: Q,
    extents: Vec<Extent>,
    current: usize,
    limits: FrameLimits,
    segment: Option<SegmentBuilder>,
    index: Index,
    slots: Vec<Option<Pending>>,
    free: Vec<usize>,
    writes: VecDeque<usize>,
    ready_reads: VecDeque<usize>,
    flush: Option<usize>,
    sync_enabled: bool,
    next_ticket: u64,
    serving: bool,
    failed_at: Option<u64>,
    queue_failed: bool,
    fatal: Option<std::sync::Arc<Error>>,
    failed_cursor: usize,
    control: Option<Request>,
    control_active: bool,
    control_done: Option<crate::io::Completion>,
    maintenance: bool,
    budget: PollBudget,
    resources: ResourceLimits,
    pending_entries: usize,
    pending_new: usize,
    keys: Vec<ChunkId>,
}

impl<Q: Queue> Pipeline<Q> {
    /// Attaches a newly allocated segment whose active header is already durable.
    ///
    /// The queue must be empty and dedicated to this pipeline. `base` is the
    /// segment's absolute device offset. The caller must persist `limits` with
    /// device geometry and establish a fresh incarnation before calling this.
    /// Never use this constructor to resume an existing active allocation.
    pub fn new(queue: Q, header: SegmentHeader, limits: FrameLimits, base: u64) -> Result<Self> {
        let segment = SegmentBuilder::new(header)?;
        let mut pipeline = Self::read_only(queue, header, limits, base)?;
        pipeline.segment = Some(segment);
        Ok(pipeline)
    }

    /// Attaches recovered immutable storage; populate its index with `restore`.
    pub fn read_only(queue: Q, header: SegmentHeader, limits: FrameLimits, base: u64) -> Result<Self> {
        if queue.depth() == 0
            || queue.depth() > 32768
            || queue.vacant() != queue.depth()
            || !is_aligned(base, PAGE_SIZE)
            || base.checked_add(header.segment_len() as u64).is_none()
            || limits.max_frame_len() > i32::MAX as u32
        {
            return Err(Error::InvalidArgument("invalid queue, extent, or I/O frame limit"));
        }
        let mut pipeline = Self::empty(queue, limits)?;
        pipeline.extents.push(Extent { header, base });
        Ok(pipeline)
    }

    pub(crate) fn empty(queue: Q, limits: FrameLimits) -> Result<Self> {
        if queue.depth() == 0
            || queue.depth() > 32768
            || queue.vacant() != queue.depth()
            || limits.max_frame_len() > i32::MAX as u32
        {
            return Err(Error::InvalidArgument("invalid queue or frame limit"));
        }
        let depth = queue.depth();
        Ok(Self {
            queue,
            extents: Vec::new(),
            current: 0,
            limits,
            segment: None,
            index: Index::default(),
            slots: (0..depth).map(|_| None).collect(),
            free: (0..depth).rev().collect(),
            writes: VecDeque::with_capacity(depth),
            ready_reads: VecDeque::with_capacity(depth),
            flush: None,
            sync_enabled: true,
            next_ticket: 1,
            serving: false,
            failed_at: None,
            queue_failed: false,
            fatal: None,
            failed_cursor: 0,
            control: None,
            control_active: false,
            control_done: None,
            maintenance: false,
            budget: PollBudget::default(),
            resources: ResourceLimits::default(),
            pending_entries: 0,
            pending_new: 0,
            keys: Vec::new(),
        })
    }

    /// Rebuilds the index from scanner-validated frames or a validated sealed footer.
    /// Call only during read-only startup, before submitting operations. Tombstones
    /// and older physical records are resolved by LSN, just as during publication.
    pub fn restore(&mut self, metadata: Metadata<'_>) -> Result<()> {
        if self.segment.is_some() || self.serving {
            return Err(Error::InvalidArgument("restore is only valid before serving reads"));
        }
        let h = metadata.header();
        let position = self.position(h.position().offset())?;
        if position.segment_seq() != h.position().segment_seq()
            || h.position().offset() as u64 + h.frame_len() as u64
                > self.extents[self.current]
                    .header
                    .data_end()
                    .unwrap_or(self.extents[self.current].header.segment_len()) as u64
        {
            return Err(Error::InvalidArgument(
                "recovered frame is outside the assigned segment",
            ));
        }
        let metadata = Metadata::decode(metadata.as_bytes(), self.limits, position)?;
        let mut entries = Vec::new();
        entries.try_reserve_exact(metadata.header().record_count() as usize)?;
        entries.extend(index::entries(metadata, self.current as u32));
        self.reserve_entries(&entries)?;
        index::apply(&mut self.index, &mut self.keys, entries);
        Ok(())
    }

    fn position(&self, offset: u32) -> Result<FramePosition> {
        self.position_in(self.current, offset)
    }

    fn position_in(&self, segment: usize, offset: u32) -> Result<FramePosition> {
        let header = self.extents[segment].header;
        let end = header.data_end().unwrap_or(header.segment_len());
        Ok(FramePosition::new(header.id().sequence, offset, end)?)
    }

    // Device geometry validates disjoint extents and never reuses an allocation.
    pub(crate) fn attach(&mut self, header: SegmentHeader, base: u64, writable: bool) -> Result<()> {
        if !self.writes.is_empty()
            || self.flush.is_some()
            || self.segment.is_some()
            || self.queue_failed
            || self.failed_at.is_some()
            || self.extents.len() >= u32::MAX as usize
        {
            return Err(Error::InvalidArgument("pipeline cannot switch segment"));
        }
        let builder = if writable {
            Some(SegmentBuilder::new(header)?)
        } else {
            None
        };
        self.extents.try_reserve(1)?;
        self.current = self.extents.len();
        self.extents.push(Extent { header, base });
        self.segment = builder;
        Ok(())
    }

    pub(crate) fn take_segment(&mut self) -> Result<Option<SegmentBuilder>> {
        if !self.writes.is_empty() || self.flush.is_some() {
            return Err(Error::Backpressure);
        }
        if self.queue_failed {
            return Err(Error::QueueFailed);
        }
        if let Some(ticket) = self.failed_at {
            return Err(Error::WriteFailed(ticket));
        }
        Ok(self.segment.take())
    }

    pub(crate) fn reserve_index(
        &mut self,
        additional: usize,
    ) -> std::result::Result<(), std::collections::TryReserveError> {
        let additional = additional.min(self.resources.index_entries.saturating_sub(self.index.len()));
        self.index.try_reserve(additional)?;
        self.keys.try_reserve(additional)
    }

    pub(crate) fn visit_versions(&self, mut visit: impl FnMut(ChunkId, u64, Option<u32>)) {
        for (&key, location) in &self.index {
            visit(
                key,
                location.lsn,
                (location.kind == RecordKind::Data).then_some(location.value_len),
            );
        }
    }

    pub(crate) fn stat(&self, key: &ChunkId) -> Option<(u64, u32)> {
        self.index
            .get(key)
            .filter(|loc| loc.kind == RecordKind::Data)
            .map(|loc| (loc.lsn, loc.value_len))
    }

    pub(crate) fn index_len(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn data_end(&self) -> Option<u32> {
        self.segment.as_ref().map(SegmentBuilder::data_end)
    }

    fn location(&self, key: ChunkId) -> Result<Location> {
        self.index
            .get(&key)
            .copied()
            .filter(|loc| loc.kind == RecordKind::Data)
            .ok_or(Error::NotFound)
    }

    fn admission(&self, write: bool) -> Result<Ticket> {
        if self.queue_failed {
            return Err(Error::QueueFailed);
        }
        if write {
            if self.segment.is_none() {
                return Err(Error::ReadOnly);
            }
            if let Some(ticket) = self.failed_at {
                return Err(Error::WriteFailed(ticket));
            }
            if self.flush.is_some() {
                return Err(Error::Backpressure);
            }
        }
        if self.free.is_empty() || self.queue.vacant() == 0 || (self.maintenance && self.queue.vacant() <= 1) {
            return Err(Error::Backpressure);
        }
        if self.next_ticket == u64::MAX {
            return Err(Error::InvalidArgument("ticket counter exhausted"));
        }
        Ok(Ticket(self.next_ticket))
    }

    fn take_slot(&mut self, pending: Pending) -> usize {
        self.serving = true;
        let slot = self.free.pop().expect("admission checked capacity");
        self.slots[slot] = Some(pending);
        self.next_ticket += 1;
        slot
    }

    fn submit(&mut self, slot: usize, operation: Operation, offset: u64, len: usize, buffer: Option<Buffer>) {
        // An exclusively owned queue must accept while capacity is available.
        self.queue
            .try_submit(Request {
                token: slot as u64,
                operation,
                offset,
                len,
                buffer,
            })
            .expect("queue violated its capacity contract");
    }

    /// Queues a persistence barrier after preceding writes, blocking new write
    /// admission until it completes. Reads may continue. Poll to drive progress.
    pub fn flush(&mut self) -> Result<Ticket> {
        // A persistence barrier needs no active allocation, including immediately
        // after open. It still must preserve failed-write and barrier ordering.
        let ticket = self.admission(false)?;
        if let Some(ticket) = self.failed_at {
            return Err(Error::WriteFailed(ticket));
        }
        if self.flush.is_some() {
            return Err(Error::Backpressure);
        }
        let slot = self.take_slot(Pending::Flush {
            ticket,
            submitted: false,
        });
        self.flush = Some(slot);
        Ok(ticket)
    }

    /// Number of admitted operations whose completions have not been delivered.
    pub fn in_flight(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// Whether a published data version exists (tombstones return false).
    pub fn contains(&self, key: &ChunkId) -> bool {
        self.index.get(key).is_some_and(|loc| loc.kind == RecordKind::Data)
    }
}
