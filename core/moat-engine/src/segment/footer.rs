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

use moat_common::{Crc32c, PAGE_SIZE, is_aligned};

use super::{
    Error, FOOTER_MAGIC, FOOTER_TRAILER_LEN, MIN_FRAME_METADATA_LEN, Result, SegmentHeader, SegmentId, footer_len,
    header::{Seal, validate_prefix},
};
use crate::{
    codec::*,
    frame::{FrameHeader, FrameLimits, Metadata},
};

/// Independently validated seal record at the end of a segment's final page.
///
/// Identity and generation must be compared with the allocation header before
/// using its lengths or accepting any embedded metadata.
#[derive(Debug, Clone, Copy)]
pub struct FooterTrailer {
    header: SegmentHeader,
    checksum: u32,
}

impl FooterTrailer {
    /// Decodes the final 64 bytes of a tail page against trusted segment geometry.
    /// This does not validate the preceding footer bytes or allocation identity.
    pub fn decode(tail: &[u8], segment_len: u32) -> Result<Self> {
        let at = tail.len().checked_sub(FOOTER_TRAILER_LEN).ok_or(Error::Truncated {
            required: FOOTER_TRAILER_LEN,
            available: tail.len(),
        })?;
        let bytes = &tail[at..];
        validate_prefix(bytes, FOOTER_MAGIC)?;
        let id = SegmentId {
            device_id: bytes[16..32].try_into().expect("fixed trailer length"),
            segment_no: u32_at(bytes, 32),
            sequence: u64_at(bytes, 40),
        };
        let mut header = SegmentHeader::new(id, segment_len).map_err(|_| Error::Corrupt("footer segment geometry"))?;
        let seal = Seal {
            data_end: u32_at(bytes, 36),
            frame_count: u32_at(bytes, 48),
            metadata_len: u32_at(bytes, 52),
        };
        let len = footer_len(seal.metadata_len as u64);
        if seal.data_end < PAGE_SIZE as u32
            || !is_aligned(seal.data_end as u64, PAGE_SIZE)
            || !seal.metadata_len.is_multiple_of(4)
            || len != u32_at(bytes, 56) as u64
            || seal.data_end as u64 + len > segment_len as u64
            || (seal.frame_count == 0) != (seal.data_end == PAGE_SIZE as u32)
            || (seal.frame_count == 0) != (seal.metadata_len == 0)
            || seal.frame_count as u64 * PAGE_SIZE > seal.data_end as u64 - PAGE_SIZE
            || seal.frame_count as u64 * MIN_FRAME_METADATA_LEN > seal.metadata_len as u64
            || seal.metadata_len as u64 > seal.data_end as u64 - PAGE_SIZE
        {
            return Err(Error::Corrupt("sealed footer geometry"));
        }
        header.seal = Some(seal);
        Ok(Self {
            header,
            checksum: u32_at(bytes, 60),
        })
    }

    /// Sealed in-memory segment view; compare its identity with the allocation.
    pub fn header(self) -> SegmentHeader {
        self.header
    }
}

// Both CRC fields are zeroed in the complete footer checksum. The trailer CRC
// is computed afterwards and also protects the stored complete-footer checksum.
pub(super) fn checksum(bytes: &[u8]) -> u32 {
    let at = bytes.len() - FOOTER_TRAILER_LEN;
    Crc32c::new()
        .update(&bytes[..at + 12])
        .update(&[0; 4])
        .update(&bytes[at + 16..at + 60])
        .update(&[0; 4])
        .finalize()
}

/// Validated sealed metadata, borrowing the footer without copying its directory.
///
/// A footer summarizes frames, not payload integrity. Verified reads must still
/// fetch and check the requested payload blocks. On footer failure, the caller
/// may scan frames while preserving the sealed header's exact data boundary.
#[derive(Debug, Clone, Copy)]
pub struct Footer<'a> {
    bytes: &'a [u8],
    header: SegmentHeader,
    limits: FrameLimits,
}

impl<'a> Footer<'a> {
    /// Validates the footer and every embedded frame's metadata and position.
    /// The header must already be validated independently of these footer bytes.
    pub fn decode(bytes: &'a [u8], header: SegmentHeader, limits: FrameLimits) -> Result<Self> {
        let seal = header
            .seal
            .ok_or(Error::InvalidArgument("active segment has no committed footer"))?;
        let len = header.footer_range().expect("sealed header").len();
        let bytes = bytes.get(..len).ok_or(Error::Truncated {
            required: len,
            available: bytes.len(),
        })?;
        let trailer = FooterTrailer::decode(bytes, header.segment_len)?;
        if trailer.header != header || trailer.checksum != checksum(bytes) {
            return Err(Error::Corrupt("footer identity, geometry, or checksum"));
        }
        let end = seal.metadata_len as usize;
        if bytes[end..len - FOOTER_TRAILER_LEN].iter().any(|&byte| byte != 0) {
            return Err(Error::Corrupt("footer padding"));
        }
        let footer = Self {
            bytes: &bytes[..end],
            header,
            limits,
        };
        let mut remaining = footer.bytes;
        let mut offset = PAGE_SIZE as u32;
        for _ in 0..seal.frame_count {
            let (metadata, rest) = footer.decode_next(remaining, offset)?;
            offset += metadata.header().frame_len() as u32;
            remaining = rest;
        }
        if !remaining.is_empty() || offset != seal.data_end {
            return Err(Error::Corrupt("footer frame coverage"));
        }
        Ok(footer)
    }

    fn decode_next(self, bytes: &'a [u8], offset: u32) -> Result<(Metadata<'a>, &'a [u8])> {
        let decode = || {
            let position = self.header.position(offset)?;
            // Header validation bounds metadata before slicing or allocating.
            let metadata = Metadata::decode(bytes, self.limits, position)?;
            Ok((metadata, &bytes[metadata.as_bytes().len()..]))
        };
        decode().map_err(|source| Error::Frame { offset, source })
    }

    /// Iterates validated frame metadata in physical frame order.
    /// Record LSN order is independent; index reconstruction must compare LSNs.
    pub fn frames(self) -> impl ExactSizeIterator<Item = Metadata<'a>> {
        let mut bytes = self.bytes;
        let mut offset = PAGE_SIZE as u32;
        (0..self.header.seal.expect("validated sealed footer").frame_count).map(move |_| {
            // Re-read the small header only; the immutable footer already passed
            // metadata CRC, directory, and overlap validation in `decode`.
            let position = self.header.position(offset).expect("validated frame position");
            let header = FrameHeader::decode(bytes, self.limits, position).expect("validated frame header");
            let (front, rest) = bytes.split_at(header.metadata_len());
            let metadata = Metadata::from_validated(front, header);
            bytes = rest;
            offset += metadata.header().frame_len() as u32;
            metadata
        })
    }
}
