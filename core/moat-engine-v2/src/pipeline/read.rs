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

use super::{Error, Result, Ticket, index::Location};
use crate::frame::{FrameHeader, FrameLimits, FramePosition, Metadata, RecordKind, verification_range};

/// Reusable, caller-owned buffers for metadata and the requested value extent.
#[derive(Debug)]
pub struct ReadBuffers {
    /// Space for the page-rounded front metadata region.
    pub metadata: AlignedBuf,
    /// Space for the page-rounded complete checksum blocks covering the request.
    pub value: AlignedBuf,
}

/// Minimum buffer capacities for a read of the currently indexed version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequirements {
    /// Page-rounded front metadata extent.
    pub metadata_len: usize,
    /// Page-rounded value extent, or zero if metadata already covers it.
    pub value_len: usize,
}

/// Location of verified bytes within the returned buffers.
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
    /// Borrows a verified result without copying it or changing buffer ownership.
    pub fn view(&self, range: ReadRange) -> &[u8] {
        match range {
            ReadRange::Metadata(range) => &self.metadata[range],
            ReadRange::Value(range) => &self.value[range],
        }
    }
}

pub(super) struct Read {
    pub ticket: Ticket,
    pub key: ChunkId,
    pub location: Location,
    pub requested: Range<u32>,
    pub verified: Range<u32>,
    pub io_offset: u64,
    pub io_len: usize,
    pub prefix: usize,
    pub metadata: Option<AlignedBuf>,
    pub value: Option<AlignedBuf>,
    validated_header: Option<FrameHeader>,
}

impl Read {
    pub fn plan(ticket: Ticket, key: ChunkId, location: Location, requested: Range<u32>, base: u64) -> Result<Self> {
        let verified = verification_range(location.value_len, requested.clone())?;
        let start = base + location.frame_offset as u64 + location.value_offset as u64 + verified.start as u64;
        let offset = align_down(start, PAGE_SIZE);
        let prefix = (start - offset) as usize;
        let io_len = if requested.is_empty() {
            0
        } else {
            align_up(prefix as u64 + verified.len() as u64, PAGE_SIZE) as usize
        };
        Ok(Self {
            ticket,
            key,
            location,
            requested,
            verified,
            io_offset: offset,
            io_len,
            prefix,
            metadata: None,
            value: None,
            validated_header: None,
        })
    }

    pub fn value_buffer_len(&self) -> usize {
        if self.requested.is_empty()
            || self.location.value_offset as usize + self.verified.end as usize <= self.metadata_len()
        {
            0
        } else {
            self.io_len
        }
    }

    pub fn metadata_len(&self) -> usize {
        align_up(self.location.metadata_len as u64, PAGE_SIZE) as usize
    }

    pub fn validate_metadata(&mut self, limits: FrameLimits, position: FramePosition) -> Result<()> {
        let metadata = Metadata::decode(
            self.metadata.as_ref().expect("metadata read completed"),
            limits,
            position,
        )?;
        let d = metadata
            .record(self.location.ordinal)
            .ok_or(Error::Frame(crate::frame::Error::Corrupt("indexed record is missing")))?
            .descriptor();
        if metadata.header().frame_len() != self.location.frame_len as usize
            || metadata.header().metadata_len() != self.location.metadata_len as usize
            || d.key != self.key
            || d.lsn != self.location.lsn
            || d.value_len != self.location.value_len
            || d.value_offset != self.location.value_offset
            || d.kind != RecordKind::Data
        {
            return Err(Error::Frame(crate::frame::Error::Corrupt("indexed record identity")));
        }
        self.validated_header = Some(metadata.header());
        Ok(())
    }

    pub fn finish_metadata(&mut self, limits: FrameLimits, position: FramePosition) -> Result<Option<ReadRange>> {
        self.validate_metadata(limits, position)?;
        if self.requested.is_empty() {
            return Ok(Some(ReadRange::Value(0..0)));
        }
        let start = self.location.value_offset as usize + self.verified.start as usize;
        let end = self.location.value_offset as usize + self.verified.end as usize;
        if end > self.metadata_len() {
            return Ok(None);
        }
        let bytes = self.metadata.as_ref().expect("metadata read completed");
        let header = self.validated_header.expect("metadata validated");
        let metadata = Metadata::from_validated(&bytes[..header.metadata_len()], header);
        metadata
            .record(self.location.ordinal)
            .expect("validated indexed record")
            .verify(self.verified.clone(), &bytes[start..end])?;
        let start = self.location.value_offset as usize + self.requested.start as usize;
        Ok(Some(ReadRange::Metadata(start..start + self.requested.len())))
    }

    pub fn verify(&self) -> Result<ReadRange> {
        // This buffer is retained unchanged after the metadata phase. Reuse its
        // validated view rather than recalculating metadata CRC on payload completion.
        let header = self.validated_header.expect("metadata phase succeeded");
        let bytes = &self.metadata.as_ref().expect("owned metadata")[..header.metadata_len()];
        let metadata = Metadata::from_validated(bytes, header);
        let record = metadata
            .record(self.location.ordinal)
            .expect("validated indexed record");
        record.verify(
            self.verified.clone(),
            &self.value.as_ref().expect("value read completed")[self.prefix..self.prefix + self.verified.len()],
        )?;
        let start = self.prefix + (self.requested.start - self.verified.start) as usize;
        Ok(ReadRange::Value(start..start + self.requested.len()))
    }

    pub fn buffers(self) -> ReadBuffers {
        ReadBuffers {
            metadata: self.metadata.expect("metadata buffer returned"),
            value: self.value.expect("value buffer returned"),
        }
    }
}
