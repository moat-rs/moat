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

use moat_common::{PAGE_SIZE, is_aligned};

use super::{
    Error, FOOTER_MAGIC, FOOTER_TRAILER_LEN, FORMAT_VERSION, MIN_FRAME_METADATA_LEN, Result, SegmentHeader, footer_len,
    header::Seal,
};
use crate::{
    codec::*,
    frame::{FramePosition, Metadata},
};

/// Sequential frame allocation accounting and construction of the seal footer.
///
/// Call `position` before encoding, then `append` before submitting each frame.
/// Appended metadata includes every allocated frame, even if its write has not
/// completed. The caller owns submission order, completion tracking, and buffers.
/// On a failed frame write, abandon this builder rather than seal its footer.
#[derive(Debug)]
pub struct SegmentBuilder {
    header: SegmentHeader,
    data_end: u32,
    frame_count: u32,
    metadata: Vec<u8>,
}

impl SegmentBuilder {
    /// Starts accounting for a newly allocated, empty segment.
    ///
    /// An active header does not prove that a segment is empty. The caller must
    /// durably establish its new incarnation before submitting any frame writes.
    /// It may also collect validated recovery metadata to seal a recovered prefix,
    /// but must never be used to append new frame writes to that old allocation.
    pub fn new(header: SegmentHeader) -> Result<Self> {
        if header.is_sealed() {
            return Err(Error::Sealed);
        }
        Ok(Self {
            header,
            data_end: PAGE_SIZE as u32,
            frame_count: 0,
            metadata: Vec::new(),
        })
    }

    /// Finds the next position while reserving all accumulated footer metadata.
    /// This is a nonmutating check; `append` commits the allocation accounting.
    pub fn position(&self, frame_len: usize, metadata_len: usize) -> Result<FramePosition> {
        if self.header.is_sealed() {
            return Err(Error::Sealed);
        }
        if frame_len == 0
            || frame_len > u32::MAX as usize
            || !is_aligned(frame_len as u64, PAGE_SIZE)
            || metadata_len < MIN_FRAME_METADATA_LEN as usize
            || metadata_len > frame_len
            || !metadata_len.is_multiple_of(4)
        {
            return Err(Error::InvalidArgument("invalid frame or metadata length"));
        }
        let required =
            self.data_end as u64 + frame_len as u64 + footer_len(self.metadata.len() as u64 + metadata_len as u64);
        if required > self.header.segment_len as u64 {
            return Err(Error::Full {
                required,
                capacity: self.header.segment_len,
            });
        }
        self.header.position(self.data_end).map_err(|source| Error::Frame {
            offset: self.data_end,
            source,
        })
    }

    /// Records one encoded frame's validated metadata before its I/O submission.
    ///
    /// The metadata must match the next physical position. This copies metadata
    /// only; it neither reads payloads nor verifies that a write has completed.
    /// Failed admission leaves this builder unchanged.
    pub fn append(&mut self, metadata: Metadata<'_>) -> Result<()> {
        let frame = metadata.header();
        let position = self.position(frame.frame_len(), metadata.as_bytes().len())?;
        if frame.position().segment_seq() != position.segment_seq() || frame.position().offset() != position.offset() {
            return Err(Error::InvalidArgument("frame does not match the next segment position"));
        }
        self.metadata.extend_from_slice(metadata.as_bytes());
        self.data_end += frame.frame_len() as u32;
        self.frame_count += 1;
        Ok(())
    }

    /// Current allocated data boundary, including writes not yet completed.
    pub fn data_end(&self) -> u32 {
        self.data_end
    }

    /// Bytes reserved for the complete page-rounded footer.
    pub fn footer_len(&self) -> usize {
        footer_len(self.metadata.len() as u64) as usize
    }

    /// Encodes the footer and stops further admission, returning the sealed header.
    ///
    /// This only constructs bytes. After successful frame writes, the caller
    /// places the footer at the segment end. Persist data and preceding footer
    /// pages before writing and persisting the final page containing the trailer.
    /// The returned sealed header is an in-memory view, never an allocation write.
    /// A short output buffer leaves both the builder and destination unchanged.
    pub fn seal_into(&mut self, bytes: &mut [u8]) -> Result<SegmentHeader> {
        if self.header.is_sealed() {
            return Err(Error::Sealed);
        }
        let len = self.footer_len();
        let available = bytes.len();
        let bytes = bytes.get_mut(..len).ok_or(Error::BufferTooSmall {
            required: len,
            available,
        })?;
        bytes.fill(0);
        bytes[..self.metadata.len()].copy_from_slice(&self.metadata);
        let at = len - FOOTER_TRAILER_LEN;
        let trailer = &mut bytes[at..];
        trailer[..8].copy_from_slice(&FOOTER_MAGIC);
        put_u32(trailer, 8, FORMAT_VERSION);
        trailer[16..32].copy_from_slice(&self.header.id.device_id);
        put_u32(trailer, 32, self.header.id.segment_no);
        put_u32(trailer, 36, self.data_end);
        put_u64(trailer, 40, self.header.id.sequence);
        put_u32(trailer, 48, self.frame_count);
        put_u32(trailer, 52, self.metadata.len() as u32);
        put_u32(trailer, 56, len as u32);
        let checksum = super::footer::checksum(bytes);
        put_u32(&mut bytes[at..], 60, checksum);
        let checksum = crc_with_zeroed_checksum(&bytes[at..]);
        put_u32(&mut bytes[at..], 12, checksum);
        self.header.seal = Some(Seal {
            data_end: self.data_end,
            frame_count: self.frame_count,
            metadata_len: self.metadata.len() as u32,
        });
        Ok(self.header)
    }
}
