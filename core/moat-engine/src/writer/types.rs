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

//! Public write options, tickets, completions, and caller-owned value buffers.

use moat_common::PooledBuf;

use crate::Result;

/// A per-disk write sequence number.
pub type Lsn = u64;

/// Identifies an accepted operation until its [`Completion`] is delivered.
pub type Ticket = u64;

/// Options for a single `put`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PutOptions {
    /// Replace an existing chunk with the same identifier. When `false` (the
    /// default) a put of an existing identifier returns
    /// [`PutOutcome::Exists`] without writing anything.
    pub overwrite: bool,
}

/// Result of a `put`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The record was accepted. It is durable and visible once the
    /// [`Completion`] for `ticket` reports success.
    Written {
        /// Identifies the eventual completion.
        ticket: Ticket,
        /// The record's LSN.
        lsn: Lsn,
    },
    /// The identifier already exists and `overwrite` was not set.
    Exists,
}

/// Result of a `delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// A tombstone was appended. The chunk is invisible to this writer at
    /// once and to readers (and after a crash) once the completion for
    /// `ticket` reports success.
    Deleted {
        /// Identifies the eventual completion.
        ticket: Ticket,
        /// The tombstone's LSN.
        lsn: Lsn,
    },
    /// The chunk is neither indexed nor pending; nothing was written.
    Missing,
}

/// What a completed ticket did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A record is on disk and indexed.
    Put {
        /// The record's LSN.
        lsn: Lsn,
    },
    /// A tombstone is on disk.
    Delete {
        /// The tombstone's LSN.
        lsn: Lsn,
    },
    /// Every write accepted before the barrier is durable.
    Flush,
    /// As `Flush`, and both active segments are sealed.
    Seal,
    /// A reclaim pass finished.
    Reclaim(ReclaimReport),
}

/// The outcome of an operation accepted earlier.
#[derive(Debug)]
pub struct Completion {
    /// The ticket the operation returned.
    pub ticket: Ticket,
    /// What happened.
    pub result: Result<Outcome>,
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
    /// Obsolete data records dropped.
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
    pub(super) buf: PooledBuf,
    pub(super) value_len: u32,
    pub(super) value_off: usize,
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
