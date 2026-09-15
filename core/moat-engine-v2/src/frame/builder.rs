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

use moat_common::{CHECKSUM_BLOCK_SIZE, ChunkId, PAGE_SIZE, align_up, block_checksums_iter, block_count, crc32c};

use super::{
    DESCRIPTOR_LEN, Error, FrameHeader, FrameLimits, FramePosition, HEADER_LEN, RecordDescriptor, RecordKind, Result,
    VALUE_ALIGN, codec::put_u32,
};

#[derive(Clone, Copy)]
struct Input<'a> {
    key: ChunkId,
    lsn: u64,
    kind: RecordKind,
    value: &'a [u8],
}

/// Collects borrowed records and encodes them into one immutable frame.
///
/// Admission is bounded by the format limit and leaves the builder unchanged
/// on failure. Values are borrowed until encoding, then copied once to their
/// final locations. An asynchronous writer can supply slices of its own bounded
/// staging buffers; this codec neither allocates payload buffers nor owns I/O.
///
/// The common admission path is O(1). Near the limit, admission uses an
/// exact placement walk rather than rejecting on a conservative bound. Encoding is linear in records and
/// payload bytes. `clear` retains the directory allocation for reuse.
pub struct FrameBuilder<'a> {
    limits: FrameLimits,
    records: Vec<Input<'a>>,
    checksum_len: u64,
    // Layout of the values starting at offset zero, without metadata. Because
    // placement is monotone and page-periodic, shifting by the actual metadata
    // length changes the rounded total by at most one additional page.
    payload_end: u64,
}

impl<'a> FrameBuilder<'a> {
    /// Starts an empty frame, bounded independently of the batching policy.
    pub fn new(limits: FrameLimits) -> Self {
        Self {
            limits,
            records: Vec::new(),
            checksum_len: 0,
            payload_end: 0,
        }
    }

    /// Admits a data record, including a zero-length value.
    pub fn push(&mut self, key: ChunkId, lsn: u64, value: &'a [u8]) -> Result<()> {
        self.admit(Input {
            key,
            lsn,
            kind: RecordKind::Data,
            value,
        })
    }

    /// Admits an explicit deletion; it has no value or payload checksums.
    pub fn push_tombstone(&mut self, key: ChunkId, lsn: u64) -> Result<()> {
        self.admit(Input {
            key,
            lsn,
            kind: RecordKind::Tombstone,
            value: &[],
        })
    }

    fn admit(&mut self, input: Input<'a>) -> Result<()> {
        if input.value.len() as u64 > self.limits.max_value_len() as u64 {
            return Err(Error::ValueTooLarge {
                len: input.value.len() as u64,
                max: self.limits.max_value_len(),
            });
        }
        let checksum_len = self.checksum_len + 4 * block_count(input.value.len() as u64) as u64;
        let metadata_len = HEADER_LEN as u64 + (self.records.len() as u64 + 1) * DESCRIPTOR_LEN as u64 + checksum_len;
        let payload_end = place(self.payload_end, input.value.len()).1;
        let upper = align_up(metadata_len, PAGE_SIZE) + align_up(payload_end, PAGE_SIZE);
        if upper > self.limits.max_frame_len() as u64 {
            let end = self
                .records
                .iter()
                .chain(std::iter::once(&input))
                .fold(metadata_len, |end, record| place(end, record.value.len()).1);
            let required = align_up(end, PAGE_SIZE);
            if required > self.limits.max_frame_len() as u64 {
                return Err(Error::FrameFull {
                    required,
                    limit: self.limits.max_frame_len(),
                });
            }
        }
        self.records.push(input);
        self.checksum_len = checksum_len;
        self.payload_end = payload_end;
        Ok(())
    }

    /// Number of accepted records, including tombstones.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether no records have been accepted.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Actual front metadata size, including header, directory, and checksums.
    pub fn metadata_len(&self) -> usize {
        HEADER_LEN + self.records.len() * DESCRIPTOR_LEN + self.checksum_len as usize
    }

    /// Exact page-rounded encoded length; zero when empty. This walks records.
    pub fn encoded_len(&self) -> usize {
        if self.is_empty() {
            return 0;
        }
        let end = self.records.iter().fold(self.metadata_len() as u64, |end, record| {
            place(end, record.value.len()).1
        });
        align_up(end, PAGE_SIZE) as usize
    }

    /// Encodes accepted records in directory order using sequential placement.
    ///
    /// The destination may be an aligned heap or registered pool buffer. Only
    /// the first `encoded_len()` bytes are modified. Capacity and segment bounds
    /// are checked before modification; an error preserves inputs for retry.
    /// Buffer address alignment is the submitting I/O layer's responsibility.
    pub fn encode_into(&self, position: FramePosition, bytes: &mut [u8]) -> Result<FrameHeader> {
        if self.is_empty() {
            return Err(Error::InvalidArgument("cannot encode an empty frame"));
        }
        let frame_len = self.encoded_len();
        position.check_len(frame_len)?;
        if bytes.len() < frame_len {
            return Err(Error::BufferTooSmall {
                required: frame_len,
                available: bytes.len(),
            });
        }
        let bytes = &mut bytes[..frame_len];
        let mut checksum_at = HEADER_LEN + self.records.len() * DESCRIPTOR_LEN;
        let mut end = self.metadata_len();
        for (ordinal, input) in self.records.iter().enumerate() {
            let (value_at, next_end) = place(end as u64, input.value.len());
            let value_at = value_at as usize;
            let next_end = next_end as usize;
            if !input.value.is_empty() {
                bytes[end..value_at].fill(0);
                bytes[value_at..next_end].copy_from_slice(input.value);
            }
            let descriptor = RecordDescriptor {
                key: input.key,
                lsn: input.lsn,
                kind: input.kind,
                value_offset: value_at as u32,
                value_len: input.value.len() as u32,
                checksum_offset: if input.value.is_empty() { 0 } else { checksum_at as u32 },
                checksum_count: block_count(input.value.len() as u64),
            };
            let descriptor_at = HEADER_LEN + ordinal * DESCRIPTOR_LEN;
            descriptor.encode(&mut bytes[descriptor_at..descriptor_at + DESCRIPTOR_LEN]);
            checksum_at = write_checksums(bytes, checksum_at, value_at, input.value.len());
            end = next_end;
        }
        bytes[end..].fill(0);
        let header = FrameHeader {
            position,
            frame_len: frame_len as u32,
            record_count: self.records.len() as u32,
            checksum_len: self.checksum_len as u32,
            metadata_crc: crc32c(&bytes[HEADER_LEN..self.metadata_len()]),
        };
        header.encode(bytes);
        Ok(header)
    }

    /// Drops accepted records while retaining directory capacity for reuse.
    pub fn clear(&mut self) {
        self.records.clear();
        self.checksum_len = 0;
        self.payload_end = 0;
    }
}

// Pick the earliest 8-byte-aligned start that minimizes whole-value read pages.
// Values of at least one checksum block always start on a page for range I/O.
// No gap filling: the format permits it, but the initial builder stays linear.
// Format limits bound lengths to u32. Calculate in u64, including the candidate
// that may exceed the limit, so rounding cannot overflow before admission.
fn place(end: u64, len: usize) -> (u64, u64) {
    if len == 0 {
        return (0, end);
    }
    let packed = align_up(end, VALUE_ALIGN);
    let len = len as u64;
    let extra_page = (packed % PAGE_SIZE + len).div_ceil(PAGE_SIZE) > len.div_ceil(PAGE_SIZE);
    let start = if len >= CHECKSUM_BLOCK_SIZE as u64 || extra_page {
        align_up(end, PAGE_SIZE)
    } else {
        packed
    };
    (start, start + len)
}

fn write_checksums(bytes: &mut [u8], mut at: usize, value_at: usize, value_len: usize) -> usize {
    if value_len == 0 {
        return at;
    }
    let (metadata, payload) = bytes.split_at_mut(value_at);
    for sum in block_checksums_iter(&payload[..value_len]) {
        put_u32(metadata, at, sum);
        at += 4;
    }
    at
}

/// A single-record frame exposing the final page-aligned value region.
///
/// The caller supplies the buffer and fills `value_mut()` directly. Finishing
/// writes only metadata and padding; it computes checksums without moving the
/// payload. The returned immutable slice covers exactly the encoded frame.
/// No caller-supplied checksum API is exposed until its trust contract is set.
pub struct PreparedFrame<'a> {
    bytes: &'a mut [u8],
    value_at: usize,
    value_len: usize,
}

impl<'a> PreparedFrame<'a> {
    /// Required buffer length for a prepared, page-aligned value.
    pub fn required_len(limits: FrameLimits, value_len: u32) -> Result<usize> {
        if value_len > limits.max_value_len() {
            return Err(Error::ValueTooLarge {
                len: value_len as u64,
                max: limits.max_value_len(),
            });
        }
        let metadata_len = HEADER_LEN as u64 + DESCRIPTOR_LEN as u64 + 4 * block_count(value_len as u64) as u64;
        let len = if value_len == 0 {
            PAGE_SIZE
        } else {
            align_up(align_up(metadata_len, PAGE_SIZE) + value_len as u64, PAGE_SIZE)
        };
        if len > limits.max_frame_len() as u64 {
            return Err(Error::FrameFull {
                required: len,
                limit: limits.max_frame_len(),
            });
        }
        Ok(len as usize)
    }

    /// Borrows a buffer large enough for the final frame, without copying data.
    pub fn new(limits: FrameLimits, value_len: u32, bytes: &'a mut [u8]) -> Result<Self> {
        let len = Self::required_len(limits, value_len)?;
        if bytes.len() < len {
            return Err(Error::BufferTooSmall {
                required: len,
                available: bytes.len(),
            });
        }
        let metadata_len = HEADER_LEN + DESCRIPTOR_LEN + 4 * block_count(value_len as u64) as usize;
        let value_at = if value_len == 0 {
            0
        } else {
            align_up(metadata_len as u64, PAGE_SIZE) as usize
        };
        Ok(Self {
            bytes: &mut bytes[..len],
            value_at,
            value_len: value_len as usize,
        })
    }

    /// Final payload region; its offset is page-aligned for nonempty values.
    pub fn value_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[self.value_at..self.value_at + self.value_len]
    }

    /// Writes metadata and padding and relinquishes mutable access to the frame.
    /// An error leaves the caller's underlying buffer intact.
    pub fn finish(self, position: FramePosition, key: ChunkId, lsn: u64) -> Result<&'a [u8]> {
        position.check_len(self.bytes.len())?;
        let count = block_count(self.value_len as u64);
        let checksum_at = HEADER_LEN + DESCRIPTOR_LEN;
        let metadata_len = checksum_at + 4 * count as usize;
        let descriptor = RecordDescriptor {
            key,
            lsn,
            kind: RecordKind::Data,
            value_offset: self.value_at as u32,
            value_len: self.value_len as u32,
            checksum_offset: if count == 0 { 0 } else { checksum_at as u32 },
            checksum_count: count,
        };
        descriptor.encode(&mut self.bytes[HEADER_LEN..checksum_at]);
        write_checksums(self.bytes, checksum_at, self.value_at, self.value_len);
        if self.value_len == 0 {
            self.bytes[metadata_len..].fill(0);
        } else {
            self.bytes[metadata_len..self.value_at].fill(0);
            self.bytes[self.value_at + self.value_len..].fill(0);
        }
        let header = FrameHeader {
            position,
            frame_len: self.bytes.len() as u32,
            record_count: 1,
            checksum_len: count * 4,
            metadata_crc: crc32c(&self.bytes[HEADER_LEN..metadata_len]),
        };
        header.encode(self.bytes);
        Ok(self.bytes)
    }
}
