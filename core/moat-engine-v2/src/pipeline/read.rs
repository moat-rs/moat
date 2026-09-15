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

use std::ops::Range;

use moat_common::{AlignedBuf, ChunkId, PAGE_SIZE, align_down, align_up};

use super::{Pending, Pipeline, Rejected, Result, Ticket};
use crate::{
    frame,
    io::{Operation, Queue},
};

/// Reusable, caller-owned buffers for metadata and the requested value extent.
#[derive(Debug)]
pub struct ReadBuffers {
    /// Space for front metadata when verification is enabled; otherwise optional.
    pub metadata: Option<AlignedBuf>,
    /// Space for requested pages, expanded to checksum blocks when verifying.
    pub value: AlignedBuf,
}

/// Minimum buffer capacities for a read of the currently indexed version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequirements {
    /// Page-rounded front metadata extent; zero when verification is disabled.
    pub metadata_len: usize,
    /// Page-rounded value extent, expanded to checksum blocks when verifying.
    /// Zero for empty ranges or verified bytes already covered by metadata.
    pub value_len: usize,
}

/// Location of requested bytes within the returned buffers.
/// Small values already fetched with metadata need no second read or payload copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadRange {
    /// Requested bytes are in the metadata I/O buffer.
    Metadata(Range<usize>),
    /// Requested bytes are in the separate value I/O buffer.
    Value(Range<usize>),
}

impl ReadRange {
    /// Whether the requested range was empty.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Metadata(range) | Self::Value(range) => range.is_empty(),
        }
    }
}

impl ReadBuffers {
    /// Creates buffers for ordinary reads without allocating metadata storage.
    pub fn new(value: AlignedBuf) -> Self {
        Self { metadata: None, value }
    }

    /// Borrows returned bytes without copying them.
    pub fn view(&self, range: ReadRange) -> &[u8] {
        match range {
            ReadRange::Metadata(range) => &self.metadata.as_ref().expect("metadata read returned its buffer")[range],
            ReadRange::Value(range) => &self.value[range],
        }
    }
}

pub(super) struct Read {
    pub ticket: Ticket,
    pub range: Range<usize>,
    pub metadata: Option<AlignedBuf>,
}

// The caller validates the range and supplies a value address inside the
// pipeline's bounded segment. Both read paths use the same page geometry.
pub(super) struct ReadExtent {
    pub offset: u64,
    pub len: usize,
    pub prefix: usize,
}

impl ReadExtent {
    pub fn new(value_start: u64, range: &Range<u32>) -> Self {
        let start = value_start + range.start as u64;
        let offset = align_down(start, PAGE_SIZE);
        let prefix = (start - offset) as usize;
        let len = if range.is_empty() {
            0
        } else {
            align_up(prefix as u64 + (range.end - range.start) as u64, PAGE_SIZE) as usize
        };
        Self { offset, len, prefix }
    }
}

impl<Q: Queue> Pipeline<Q> {
    /// Required capacities for the currently indexed version and read policy.
    /// Without verification metadata is unused and ranges only round to pages.
    /// Admission checks capacities again if a write publishes in the meantime.
    pub fn read_requirements(&self, key: ChunkId, range: Range<u32>, verify: bool) -> Result<ReadRequirements> {
        if verify {
            self.verified_read_requirements(key, range)
        } else {
            Ok(ReadRequirements {
                metadata_len: 0,
                value_len: self.direct_extent(key, &range)?.len,
            })
        }
    }

    /// Reads a snapshot of the indexed version into reusable buffers.
    ///
    /// With `verify = false`, read only requested value pages and trust the
    /// published index. No metadata is decoded and no CRC is checked. I/O errors,
    /// short transfers, range bounds and segment lifetime rules still apply.
    /// With `verify = true`, also fetch and validate metadata and verify complete
    /// checksum blocks. In both modes buffers return on completion or rejection.
    /// Empty unverified reads complete through `poll` without submitting I/O.
    pub fn read(
        &mut self,
        key: ChunkId,
        range: Range<u32>,
        verify: bool,
        buffers: ReadBuffers,
    ) -> std::result::Result<Ticket, Rejected<ReadBuffers>> {
        if verify {
            self.read_verified(key, range, buffers)
        } else {
            self.read_direct(key, range, buffers)
        }
    }

    fn direct_extent(&self, key: ChunkId, range: &Range<u32>) -> Result<ReadExtent> {
        let location = self.location(key)?;
        frame::validate_range(location.value_len, range)?;
        Ok(ReadExtent::new(
            self.base + location.frame_offset as u64 + location.value_offset as u64,
            range,
        ))
    }

    fn read_direct(
        &mut self,
        key: ChunkId,
        range: Range<u32>,
        buffers: ReadBuffers,
    ) -> std::result::Result<Ticket, Rejected<ReadBuffers>> {
        let prepare = || -> Result<(Ticket, ReadExtent)> {
            let ticket = self.admission(false)?;
            let extent = self.direct_extent(key, &range)?;
            if buffers.value.len() < extent.len {
                return Err(frame::Error::BufferTooSmall {
                    required: extent.len,
                    available: buffers.value.len(),
                }
                .into());
            }
            Ok((ticket, extent))
        };
        let (ticket, extent) = match prepare() {
            Ok(prepared) => prepared,
            Err(error) => return Err(Rejected { error, input: buffers }),
        };
        if extent.len == 0 {
            let slot = self.take_slot(Pending::EmptyRead { ticket, buffers });
            self.ready_reads.push_back(slot);
        } else {
            let read = Read {
                ticket,
                range: extent.prefix..extent.prefix + (range.end - range.start) as usize,
                metadata: buffers.metadata,
            };
            let slot = self.take_slot(Pending::Read(read));
            self.submit(slot, Operation::Read, extent.offset, extent.len, Some(buffers.value));
        }
        Ok(ticket)
    }
}
