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

use crate::io::Buffer;
use moat_common::{ChunkId, PAGE_SIZE, align_up};

use super::{
    Error, Pending, Pipeline, ReadBuffers, ReadExtent, ReadRange, ReadRequirements, Rejected, Result, Ticket,
    index::Location,
};
use crate::frame::{FrameHeader, FrameLimits, FramePosition, Metadata, RecordKind, verification_range};

pub(super) struct VerifiedRead {
    pub ticket: Ticket,
    pub key: ChunkId,
    pub location: Location,
    pub requested: Range<u32>,
    pub verified: Range<u32>,
    pub io_offset: u64,
    pub io_len: usize,
    pub prefix: usize,
    pub metadata: Option<Buffer>,
    pub value: Option<Buffer>,
    validated_header: Option<FrameHeader>,
}

impl VerifiedRead {
    pub fn plan(ticket: Ticket, key: ChunkId, location: Location, requested: Range<u32>, base: u64) -> Result<Self> {
        let verified = verification_range(location.value_len, requested.clone())?;
        let extent = ReadExtent::new(
            base + location.frame_offset as u64 + location.value_offset as u64,
            &verified,
        );
        Ok(Self {
            ticket,
            key,
            location,
            requested,
            verified,
            io_offset: extent.offset,
            io_len: extent.len,
            prefix: extent.prefix,
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
            metadata: Some(self.metadata.expect("metadata buffer returned")),
            value: self.value.expect("value buffer returned"),
        }
    }
}

impl<Q: crate::io::Queue> Pipeline<Q> {
    /// Computes buffer sizes without reserving capacity or submitting I/O.
    /// If another write publishes before `read`, admission checks sizes again.
    pub(super) fn verified_read_requirements(&self, key: ChunkId, range: Range<u32>) -> Result<ReadRequirements> {
        let location = self.location(key)?;
        let read = VerifiedRead::plan(Ticket(self.next_ticket), key, location, range, self.base)?;
        Ok(ReadRequirements {
            metadata_len: read.metadata_len(),
            value_len: read.value_buffer_len(),
        })
    }

    /// Starts a verified range read of the newest currently published version.
    /// Metadata and payload use separate aligned extents; intervening values are
    /// never read merely to reach this value. Empty ranges validate metadata only.
    pub(super) fn read_verified(
        &mut self,
        key: ChunkId,
        range: Range<u32>,
        buffers: ReadBuffers,
    ) -> std::result::Result<Ticket, Rejected<ReadBuffers>> {
        let prepare = || -> Result<VerifiedRead> {
            let ticket = self.admission(false)?;
            let location = self.location(key)?;
            let read = VerifiedRead::plan(ticket, key, location, range, self.base)?;
            for (required, available) in [
                (
                    read.metadata_len(),
                    buffers.metadata.as_ref().map_or(0, |buffer| buffer.len()),
                ),
                (read.value_buffer_len(), buffers.value.len()),
            ] {
                if required > available {
                    return Err(Error::Frame(crate::frame::Error::BufferTooSmall {
                        required,
                        available,
                    }));
                }
            }
            Ok(read)
        };
        let mut read = match prepare() {
            Ok(read) => read,
            Err(error) => return Err(Rejected { error, input: buffers }),
        };
        let ticket = read.ticket;
        let offset = self.base + read.location.frame_offset as u64;
        let len = read.metadata_len();
        read.value = Some(buffers.value);
        let slot = self.take_slot(Pending::VerifiedRead(read));
        self.submit(slot, crate::io::Operation::Read, offset, len, buffers.metadata);
        Ok(ticket)
    }
}
