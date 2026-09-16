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

use moat_common::PAGE_SIZE;

use super::{Error, FOOTER_HEADER_LEN, FOOTER_MAGIC, Result, SegmentHeader, header::validate_prefix};
use crate::{
    codec::*,
    frame::{FrameHeader, FrameLimits, Metadata},
};

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
        validate_prefix(bytes, FOOTER_MAGIC)?;
        if bytes[16..32] != header.id.device_id
            || u32_at(bytes, 32) != header.id.segment_no
            || u32_at(bytes, 36) != seal.data_end
            || u64_at(bytes, 40) != header.id.sequence
            || u32_at(bytes, 48) != seal.frame_count
            || u32_at(bytes, 52) != seal.metadata_len
            || u32_at(bytes, 56) as usize != len
            || u32_at(bytes, 60) != 0
        {
            return Err(Error::Corrupt("footer identity or geometry"));
        }
        let end = FOOTER_HEADER_LEN + seal.metadata_len as usize;
        if bytes[end..].iter().any(|&byte| byte != 0) {
            return Err(Error::Corrupt("footer padding"));
        }
        let footer = Self {
            bytes: &bytes[FOOTER_HEADER_LEN..end],
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
