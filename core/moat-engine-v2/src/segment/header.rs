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

use moat_common::{PAGE_SIZE, is_aligned};

use super::{Error, FORMAT_VERSION, HEADER_MAGIC, MIN_FRAME_METADATA_LEN, Result, footer_len};
use crate::{codec::*, frame::FramePosition};

/// Physical segment identity, including the allocation incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentId {
    /// Device identity from the future device superblock.
    pub device_id: [u8; 16],
    /// Segment number within the device.
    pub segment_no: u32,
    /// Nonzero device-wide allocation sequence; never reuse it on that device.
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Seal {
    pub data_end: u32,
    pub frame_count: u32,
    pub metadata_len: u32,
}

/// Validated segment identity, geometry, and optional sealed boundary.
///
/// An active header carries no speculative tail. Its durable prefix is found
/// by scanning frames. A sealed header commits to an exact data/footer boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    pub(super) id: SegmentId,
    pub(super) segment_len: u32,
    pub(super) seal: Option<Seal>,
}

impl SegmentHeader {
    /// Creates an active header. Even an empty segment needs header/footer pages.
    pub fn new(id: SegmentId, segment_len: u32) -> Result<Self> {
        if id.sequence == 0 || segment_len < 2 * PAGE_SIZE as u32 || !is_aligned(segment_len as u64, PAGE_SIZE) {
            return Err(Error::InvalidArgument(
                "invalid segment length or allocation incarnation",
            ));
        }
        Ok(Self {
            id,
            segment_len,
            seal: None,
        })
    }

    /// Validates the complete header page against the containing device geometry.
    ///
    /// The incarnation is discovered from the validated header. The caller must
    /// supply device identity, segment number, and size from trusted geometry.
    pub fn decode(bytes: &[u8], device_id: [u8; 16], segment_no: u32, segment_len: u32) -> Result<Self> {
        let bytes = bytes.get(..PAGE_SIZE as usize).ok_or(Error::Truncated {
            required: PAGE_SIZE as usize,
            available: bytes.len(),
        })?;
        validate_prefix(bytes, HEADER_MAGIC)?;
        if bytes[16..32] != device_id || u32_at(bytes, 32) != segment_no || u32_at(bytes, 36) != segment_len {
            return Err(Error::Corrupt("segment identity or length"));
        }
        let id = SegmentId {
            device_id,
            segment_no,
            sequence: u64_at(bytes, 40),
        };
        let mut header = Self::new(id, segment_len).map_err(|_| Error::Corrupt("segment geometry"))?;
        if bytes[68..].iter().any(|&byte| byte != 0) {
            return Err(Error::Corrupt("reserved segment header bytes"));
        }
        match u32_at(bytes, 48) {
            0 if bytes[52..68].iter().all(|&byte| byte == 0) => {}
            1 => {
                let seal = Seal {
                    data_end: u32_at(bytes, 52),
                    frame_count: u32_at(bytes, 60),
                    metadata_len: u32_at(bytes, 64),
                };
                let len = footer_len(seal.metadata_len as u64);
                if seal.data_end < PAGE_SIZE as u32
                    || !is_aligned(seal.data_end as u64, PAGE_SIZE)
                    || !seal.metadata_len.is_multiple_of(4)
                    || len != u32_at(bytes, 56) as u64
                    || seal.data_end as u64 + len > segment_len as u64
                    || (seal.frame_count == 0) != (seal.data_end == PAGE_SIZE as u32)
                    || (seal.frame_count == 0) != (seal.metadata_len == 0)
                    || seal.frame_count as u64 * PAGE_SIZE > (seal.data_end as u64 - PAGE_SIZE)
                    || seal.frame_count as u64 * MIN_FRAME_METADATA_LEN > seal.metadata_len as u64
                    || seal.metadata_len as u64 > seal.data_end as u64 - PAGE_SIZE
                {
                    return Err(Error::Corrupt("sealed segment geometry"));
                }
                header.seal = Some(seal);
            }
            _ => return Err(Error::Corrupt("segment state or active seal fields")),
        }
        Ok(header)
    }

    /// Writes one header page, zeroing all reserved bytes.
    pub fn encode_into(self, bytes: &mut [u8]) -> Result<()> {
        let available = bytes.len();
        let bytes = bytes.get_mut(..PAGE_SIZE as usize).ok_or(Error::BufferTooSmall {
            required: PAGE_SIZE as usize,
            available,
        })?;
        bytes.fill(0);
        bytes[..8].copy_from_slice(&HEADER_MAGIC);
        put_u32(bytes, 8, FORMAT_VERSION);
        bytes[16..32].copy_from_slice(&self.id.device_id);
        put_u32(bytes, 32, self.id.segment_no);
        put_u32(bytes, 36, self.segment_len);
        put_u64(bytes, 40, self.id.sequence);
        if let Some(seal) = self.seal {
            put_u32(bytes, 48, 1);
            put_u32(bytes, 52, seal.data_end);
            put_u32(bytes, 56, footer_len(seal.metadata_len as u64) as u32);
            put_u32(bytes, 60, seal.frame_count);
            put_u32(bytes, 64, seal.metadata_len);
        }
        put_u32(bytes, 12, crc_with_zeroed_checksum(bytes));
        Ok(())
    }

    /// Physical identity, suitable for caller-owned segment selection.
    pub fn id(self) -> SegmentId {
        self.id
    }

    /// Complete physical segment size in bytes.
    pub fn segment_len(self) -> u32 {
        self.segment_len
    }

    /// Whether a durable sealed boundary is claimed by this header.
    pub fn is_sealed(self) -> bool {
        self.seal.is_some()
    }

    /// Segment-relative footer extent, present only for sealed headers.
    pub fn footer_range(self) -> Option<Range<u32>> {
        self.seal
            .map(|seal| seal.data_end..seal.data_end + footer_len(seal.metadata_len as u64) as u32)
    }

    pub(super) fn position(self, offset: u32) -> crate::frame::Result<FramePosition> {
        let end = self.seal.map_or(self.segment_len, |seal| seal.data_end);
        FramePosition::new(self.id.sequence, offset, end)
    }
}

pub(super) fn validate_prefix(bytes: &[u8], magic: [u8; 8]) -> Result<()> {
    if bytes[..8] != magic {
        return Err(Error::Corrupt("segment metadata magic"));
    }
    if u32_at(bytes, 12) != crc_with_zeroed_checksum(bytes) {
        return Err(Error::Corrupt("segment metadata checksum"));
    }
    if u32_at(bytes, 8) != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion(u32_at(bytes, 8)));
    }
    Ok(())
}
