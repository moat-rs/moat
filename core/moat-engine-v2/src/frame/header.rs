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

use super::{DESCRIPTOR_LEN, Error, FORMAT_VERSION, HEADER_LEN, MAGIC, Result, codec::*};

/// Format-wide decoding bounds, independent of a writer's batching target.
///
/// A future device superblock must persist these bounds. Reopening with a
/// smaller batching target must not reject previously written frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    max_frame_len: u32,
    max_value_len: u32,
}

impl FrameLimits {
    /// Sets the maximum encoded frame and logical value lengths in bytes.
    pub fn new(max_frame_len: u32, max_value_len: u32) -> Result<Self> {
        if max_frame_len == 0 || !is_aligned(max_frame_len as u64, PAGE_SIZE) {
            return Err(Error::InvalidArgument("frame limit must be a nonzero page multiple"));
        }
        if max_value_len > max_frame_len {
            return Err(Error::InvalidArgument("value limit exceeds frame limit"));
        }
        Ok(Self {
            max_frame_len,
            max_value_len,
        })
    }

    /// Maximum encoded frame length, including padding.
    pub fn max_frame_len(self) -> u32 {
        self.max_frame_len
    }

    /// Maximum logical value length.
    pub fn max_value_len(self) -> u32 {
        self.max_value_len
    }
}

/// Expected physical identity of a frame in a particular segment allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePosition {
    segment_seq: u64,
    offset: u32,
    available: u32,
}

impl FramePosition {
    /// Binds a frame to a segment incarnation and page-aligned byte offset.
    ///
    /// The first page belongs to the segment header. Segment length and all
    /// frame offsets must fit in the format's 32-bit geometry fields. The
    /// segment allocator must separately reserve space for its eventual footer.
    pub fn new(segment_seq: u64, offset: u32, segment_len: u32) -> Result<Self> {
        if !is_aligned(segment_len as u64, PAGE_SIZE)
            || !is_aligned(offset as u64, PAGE_SIZE)
            || offset < PAGE_SIZE as u32
            || offset >= segment_len
        {
            return Err(Error::InvalidArgument("invalid segment length or frame offset"));
        }
        Ok(Self {
            segment_seq,
            offset,
            available: segment_len - offset,
        })
    }

    /// Allocation incarnation of the containing segment.
    pub fn segment_seq(self) -> u64 {
        self.segment_seq
    }

    /// Byte offset relative to the start of the segment.
    pub fn offset(self) -> u32 {
        self.offset
    }

    pub(super) fn check_len(self, len: usize) -> Result<()> {
        if len > self.available as usize {
            return Err(Error::SegmentFull {
                required: len as u64,
                available: self.available,
            });
        }
        Ok(())
    }
}

/// A checksummed and bounds-checked frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub(super) position: FramePosition,
    pub(super) frame_len: u32,
    pub(super) record_count: u32,
    pub(super) checksum_len: u32,
    pub(super) metadata_crc: u32,
}

impl FrameHeader {
    /// Validates the fixed header before any variable-sized allocation or read.
    /// The supplied slice may contain just the header, a page, or a whole frame.
    pub fn decode(bytes: &[u8], limits: FrameLimits, position: FramePosition) -> Result<Self> {
        let bytes = bytes.get(..HEADER_LEN).ok_or(Error::Truncated {
            required: HEADER_LEN,
            available: bytes.len(),
        })?;
        if bytes[..8] != MAGIC {
            return Err(Error::Corrupt("frame magic"));
        }
        if u32_at(bytes, 12) != header_crc(bytes) {
            return Err(Error::Corrupt("header checksum"));
        }
        let version = u32_at(bytes, 8);
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        if bytes[48..].iter().any(|&byte| byte != 0) {
            return Err(Error::Corrupt("reserved header bytes"));
        }
        if u64_at(bytes, 16) != position.segment_seq || u32_at(bytes, 24) != position.offset {
            return Err(Error::Corrupt("frame position or segment incarnation"));
        }
        let frame_len = u32_at(bytes, 28);
        let record_count = u32_at(bytes, 32);
        let directory_len = u32_at(bytes, 36);
        let checksum_len = u32_at(bytes, 40);
        if frame_len == 0
            || !is_aligned(frame_len as u64, PAGE_SIZE)
            || frame_len > limits.max_frame_len
            || frame_len > position.available
        {
            return Err(Error::Corrupt("frame length"));
        }
        if record_count == 0 || record_count as u64 * DESCRIPTOR_LEN as u64 != directory_len as u64 {
            return Err(Error::Corrupt("directory length or record count"));
        }
        let metadata_len = HEADER_LEN as u64 + directory_len as u64 + checksum_len as u64;
        if !checksum_len.is_multiple_of(4) || metadata_len > frame_len as u64 {
            return Err(Error::Corrupt("metadata length"));
        }
        Ok(Self {
            position,
            frame_len,
            record_count,
            checksum_len,
            metadata_crc: u32_at(bytes, 44),
        })
    }

    /// Physical identity checked by the decoder.
    pub fn position(self) -> FramePosition {
        self.position
    }

    /// Encoded byte length, including final page padding.
    pub fn frame_len(self) -> usize {
        self.frame_len as usize
    }

    /// Number of descriptors in the directory, including tombstones.
    pub fn record_count(self) -> u32 {
        self.record_count
    }

    /// Actual metadata byte length, without rounding to a page.
    pub fn metadata_len(self) -> usize {
        HEADER_LEN + self.record_count as usize * DESCRIPTOR_LEN + self.checksum_len as usize
    }

    pub(super) fn encode(self, bytes: &mut [u8]) {
        bytes[..HEADER_LEN].fill(0);
        bytes[..8].copy_from_slice(&MAGIC);
        put_u32(bytes, 8, FORMAT_VERSION);
        put_u64(bytes, 16, self.position.segment_seq);
        put_u32(bytes, 24, self.position.offset);
        put_u32(bytes, 28, self.frame_len);
        put_u32(bytes, 32, self.record_count);
        put_u32(bytes, 36, self.record_count * DESCRIPTOR_LEN as u32);
        put_u32(bytes, 40, self.checksum_len);
        put_u32(bytes, 44, self.metadata_crc);
        put_u32(bytes, 12, header_crc(bytes));
    }
}
