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

use moat_common::{CHECKSUM_BLOCK_SIZE, align_down, align_up, block_count, crc32c, is_aligned, verify_blocks_with};

use super::{
    DESCRIPTOR_LEN, Error, FrameHeader, FrameLimits, FramePosition, HEADER_LEN, RecordDescriptor, RecordKind, Result,
    VALUE_ALIGN, codec::u32_at,
};

/// A validated view of the header, directory, and checksum area.
///
/// Payload bytes need not be present. Metadata CRC validation always covers
/// the entire directory and checksum area, including other records' entries.
/// Sequential payload layouts require no allocation. Reordered payloads use a
/// temporary range vector to check overlap in O(N log N), then discard it.
#[derive(Debug, Clone, Copy)]
pub struct Metadata<'a> {
    bytes: &'a [u8],
    header: FrameHeader,
}

impl<'a> Metadata<'a> {
    /// Validates all metadata and value geometry without reading any payload.
    pub fn decode(bytes: &'a [u8], limits: FrameLimits, position: FramePosition) -> Result<Self> {
        let header = FrameHeader::decode(bytes, limits, position)?;
        let bytes = bytes.get(..header.metadata_len()).ok_or(Error::Truncated {
            required: header.metadata_len(),
            available: bytes.len(),
        })?;
        if crc32c(&bytes[HEADER_LEN..]) != header.metadata_crc {
            return Err(Error::Corrupt("metadata checksum"));
        }
        let metadata = Self { bytes, header };
        let mut checksum_at = HEADER_LEN + header.record_count() as usize * DESCRIPTOR_LEN;
        let mut previous_end = header.metadata_len();
        let mut ordered = true;
        let mut value_count = 0;
        for ordinal in 0..header.record_count() {
            let descriptor = metadata.decode_descriptor(ordinal)?;
            if descriptor.value_len > limits.max_value_len() {
                return Err(Error::Corrupt("value length exceeds format limit"));
            }
            if descriptor.kind == RecordKind::Tombstone && descriptor.value_len != 0 {
                return Err(Error::Corrupt("tombstone has a value"));
            }
            if descriptor.value_len == 0 {
                if descriptor.value_offset != 0 || descriptor.checksum_offset != 0 || descriptor.checksum_count != 0 {
                    return Err(Error::Corrupt("nonzero empty-value geometry"));
                }
                continue;
            }
            if descriptor.checksum_count != block_count(descriptor.value_len as u64)
                || descriptor.checksum_offset as usize != checksum_at
            {
                return Err(Error::Corrupt("checksum array geometry"));
            }
            let checksum_end = checksum_at as u64 + 4 * descriptor.checksum_count as u64;
            if checksum_end > header.metadata_len() as u64 {
                return Err(Error::Corrupt("checksum array exceeds metadata"));
            }
            checksum_at = checksum_end as usize;
            let start = descriptor.value_offset as usize;
            let end = start as u64 + descriptor.value_len as u64;
            if start < header.metadata_len()
                || !is_aligned(start as u64, VALUE_ALIGN)
                || end > header.frame_len() as u64
            {
                return Err(Error::Corrupt("value range or alignment"));
            }
            ordered &= start >= previous_end;
            previous_end = end as usize;
            value_count += 1;
        }
        if checksum_at != header.metadata_len() {
            return Err(Error::Corrupt("unreferenced checksum bytes"));
        }
        if !ordered {
            let mut ranges = Vec::with_capacity(value_count);
            ranges.extend(
                metadata
                    .records()
                    .filter(|record| record.descriptor.value_len != 0)
                    .map(|record| (record.descriptor.value_offset, record.descriptor.value_len)),
            );
            ranges.sort_unstable_by_key(|&(start, _)| start);
            if ranges
                .windows(2)
                .any(|pair| pair[0].0 as u64 + pair[0].1 as u64 > pair[1].0 as u64)
            {
                return Err(Error::Corrupt("overlapping values"));
            }
        }
        Ok(metadata)
    }

    // Used when iterating an immutable footer that already validated each frame.
    // The slice must be exactly the validated metadata for this header.
    pub(crate) fn from_validated(bytes: &'a [u8], header: FrameHeader) -> Self {
        Self { bytes, header }
    }

    /// The encoded header, directory, and checksum area, without payload or padding.
    pub fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// The validated fixed header.
    pub fn header(self) -> FrameHeader {
        self.header
    }

    fn decode_descriptor(self, ordinal: u32) -> Result<RecordDescriptor> {
        let at = HEADER_LEN + ordinal as usize * DESCRIPTOR_LEN;
        RecordDescriptor::decode(&self.bytes[at..at + DESCRIPTOR_LEN])
    }

    /// Looks up a record by directory index without allocation.
    pub fn record(self, ordinal: u32) -> Option<Record<'a>> {
        if ordinal >= self.header.record_count() {
            return None;
        }
        let descriptor = self.decode_descriptor(ordinal).expect("metadata already validated");
        let start = descriptor.checksum_offset as usize;
        let end = start + descriptor.checksum_count as usize * 4;
        Some(Record {
            descriptor,
            ordinal,
            checksums: &self.bytes[start..end],
        })
    }

    /// Iterates in directory order, which need not match value placement or LSN order.
    pub fn records(self) -> impl ExactSizeIterator<Item = Record<'a>> {
        (0..self.header.record_count()).map(move |ordinal| self.record(ordinal).expect("directory index in bounds"))
    }
}

/// A directory entry and its borrowed, validated checksum array.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    descriptor: RecordDescriptor,
    ordinal: u32,
    checksums: &'a [u8],
}

impl Record<'_> {
    /// The record's logical identity and physical geometry.
    pub fn descriptor(self) -> RecordDescriptor {
        self.descriptor
    }

    /// Returns a checksum by logical 64 KiB block index.
    pub fn checksum(self, block: u32) -> Option<u32> {
        if block >= self.descriptor.checksum_count {
            return None;
        }
        Some(u32_at(self.checksums, block as usize * 4))
    }

    /// Expands an in-bounds value-relative range to complete checksum blocks.
    /// Empty ranges remain empty and require no payload I/O or verification.
    pub fn verification_range(self, range: Range<u32>) -> Result<Range<u32>> {
        verification_range(self.descriptor.value_len, range)
    }

    /// Verifies complete logical checksum blocks supplied by a range read.
    ///
    /// `range` is value-relative and must equal its `verification_range`;
    /// `bytes` must contain exactly that range. This prevents short input from
    /// accidentally validating as a final partial block. Tombstones and empty
    /// data both accept an empty payload, but remain distinct record kinds.
    pub fn verify(self, range: Range<u32>, bytes: &[u8]) -> Result<()> {
        if self.verification_range(range.clone())? != range || bytes.len() as u64 != (range.end - range.start) as u64 {
            return Err(Error::InvalidArgument("verification requires complete checksum blocks"));
        }
        let first_block = range.start / CHECKSUM_BLOCK_SIZE as u32;
        verify_blocks_with(bytes, first_block, |block| self.checksum(block)).map_err(|block| Error::PayloadChecksum {
            record: self.ordinal,
            block,
        })
    }
}

/// A complete frame whose metadata and every payload checksum have passed.
///
/// Recovery should accept records only after `decode` succeeds for the whole
/// frame. CRCs detect corruption; they do not make an I/O atomic or durable.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    bytes: &'a [u8],
    metadata: Metadata<'a>,
}

impl<'a> Frame<'a> {
    /// Validates structure, bounds, and all payloads, without copying values.
    pub fn decode(bytes: &'a [u8], limits: FrameLimits, position: FramePosition) -> Result<Self> {
        let metadata = Metadata::decode(bytes, limits, position)?;
        let bytes = bytes.get(..metadata.header.frame_len()).ok_or(Error::Truncated {
            required: metadata.header.frame_len(),
            available: bytes.len(),
        })?;
        let frame = Self { bytes, metadata };
        for record in metadata.records() {
            let start = record.descriptor.value_offset as usize;
            let len = record.descriptor.value_len as usize;
            record.verify(0..len as u32, &bytes[start..start + len])?;
        }
        Ok(frame)
    }

    /// Metadata view, retaining the lifetime of the original encoded buffer.
    pub fn metadata(self) -> Metadata<'a> {
        self.metadata
    }

    /// Encoded bytes covering exactly one frame.
    pub fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// Returns a contiguous value; tombstones and out-of-bounds indices return `None`.
    /// A zero-length data record returns `Some(&[])`.
    pub fn value(self, ordinal: u32) -> Option<&'a [u8]> {
        let descriptor = self.metadata.record(ordinal)?.descriptor;
        if descriptor.kind == RecordKind::Tombstone {
            return None;
        }
        let start = descriptor.value_offset as usize;
        Some(&self.bytes[start..start + descriptor.value_len as usize])
    }
}

// Shared with read planning so checksum coverage has one definition.
pub(crate) fn verification_range(value_len: u32, range: Range<u32>) -> Result<Range<u32>> {
    validate_range(value_len, &range)?;
    if range.is_empty() {
        return Ok(range);
    }
    let block = CHECKSUM_BLOCK_SIZE as u64;
    let start = align_down(range.start as u64, block);
    let end = align_up(range.end as u64, block);
    Ok(start as u32..end.min(value_len as u64) as u32)
}

pub(crate) fn validate_range(value_len: u32, range: &Range<u32>) -> Result<()> {
    if range.start > range.end || range.end > value_len {
        return Err(Error::InvalidArgument("read range exceeds value"));
    }
    Ok(())
}
