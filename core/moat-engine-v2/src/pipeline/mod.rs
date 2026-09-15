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
//! the persistence barrier. Recovered allocations are read-only.

mod driver;
mod error;
mod index;
mod read;
mod verify;
mod write;

use std::collections::VecDeque;

use crate::io::Buffer;
pub use error::{Error, Rejected, Result};
use index::{Index, Location};
use moat_common::{ChunkId, PAGE_SIZE, is_aligned};
use read::{Read, ReadExtent};
pub use read::{ReadBuffers, ReadRange, ReadRequirements};
use verify::VerifiedRead;

use crate::{
    frame::{FrameLimits, FramePosition, Metadata, RecordKind},
    io::{Operation, Queue, Request},
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
    /// All preceding accepted writes and the subsequent persistence barrier finished.
    Flush {
        /// Admission identity.
        ticket: Ticket,
        /// Durability outcome.
        result: Result<()>,
    },
}

impl Completion {
    /// The admission identity regardless of operation kind.
    pub fn ticket(&self) -> Ticket {
        match self {
            Self::Write { ticket, .. } | Self::Read { ticket, .. } | Self::Flush { ticket, .. } => *ticket,
        }
    }
}

struct Write {
    ticket: Ticket,
    entries: Vec<(ChunkId, Location)>,
    completed: Option<(Result<()>, Buffer)>,
}

enum Pending {
    Write(Write),
    Read(Read),
    VerifiedRead(VerifiedRead),
    EmptyRead { ticket: Ticket, buffers: ReadBuffers },
    Flush { ticket: Ticket, submitted: bool },
}

/// A bounded pipeline whose queue, index, and publication order share one owner.
///
/// Different workers can own independent pipelines without locks. A returned
/// read is a snapshot of the index at admission; newer writes do not invalidate
/// its immutable storage. Segment reuse and concurrent external modification are
/// prohibited while this pipeline or any read into its storage remains live.
pub struct Pipeline<Q> {
    queue: Q,
    header: SegmentHeader,
    base: u64,
    limits: FrameLimits,
    segment: Option<SegmentBuilder>,
    index: Index,
    slots: Vec<Option<Pending>>,
    free: Vec<usize>,
    writes: VecDeque<usize>,
    ready_reads: VecDeque<usize>,
    flush: Option<usize>,
    next_ticket: u64,
    failed_at: Option<u64>,
    queue_failed: bool,
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
        let depth = queue.depth();
        Ok(Self {
            queue,
            header,
            base,
            limits,
            segment: None,
            index: Index::default(),
            slots: (0..depth).map(|_| None).collect(),
            free: (0..depth).rev().collect(),
            writes: VecDeque::with_capacity(depth),
            ready_reads: VecDeque::with_capacity(depth),
            flush: None,
            next_ticket: 1,
            failed_at: None,
            queue_failed: false,
        })
    }

    /// Rebuilds the index from scanner-validated frames or a validated sealed footer.
    /// Call only during read-only startup, before submitting operations. Tombstones
    /// and older physical records are resolved by LSN, just as during publication.
    pub fn restore(&mut self, metadata: Metadata<'_>) -> Result<()> {
        if self.segment.is_some() || self.next_ticket != 1 {
            return Err(Error::InvalidArgument("restore is only valid before serving reads"));
        }
        let h = metadata.header();
        let position = self.position(h.position().offset())?;
        if position.segment_seq() != h.position().segment_seq()
            || h.position().offset() as u64 + h.frame_len() as u64
                > self
                    .header
                    .footer_range()
                    .map_or(self.header.segment_len(), |range| range.start) as u64
        {
            return Err(Error::InvalidArgument(
                "recovered frame is outside the assigned segment",
            ));
        }
        let metadata = Metadata::decode(metadata.as_bytes(), self.limits, position)?;
        index::apply(&mut self.index, index::entries(metadata));
        Ok(())
    }

    fn position(&self, offset: u32) -> Result<FramePosition> {
        let end = self
            .header
            .footer_range()
            .map_or(self.header.segment_len(), |range| range.start);
        Ok(FramePosition::new(self.header.id().sequence, offset, end)?)
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
        if self.free.is_empty() || self.queue.vacant() == 0 {
            return Err(Error::Backpressure);
        }
        if self.next_ticket == u64::MAX {
            return Err(Error::InvalidArgument("ticket counter exhausted"));
        }
        Ok(Ticket(self.next_ticket))
    }

    fn take_slot(&mut self, pending: Pending) -> usize {
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
        let ticket = self.admission(true)?;
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
