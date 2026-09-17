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

//! Immutable, duplicated device geometry and allocation epochs.

use moat_common::{AlignedBuf, PAGE_SIZE, is_aligned};

use super::{Device, Error, Result};
use crate::{
    codec::*,
    frame::FrameLimits,
    segment::{SegmentHeader, SegmentId},
};

const MAGIC: [u8; 8] = *b"MOATDEV1";
const VERSION: u32 = 1;
const START: u64 = 2 * PAGE_SIZE;

/// Geometry fixed by format and loaded on every reopen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    id: [u8; 16],
    capacity: u64,
    segment_size: u32,
    segments: u32,
    limits: FrameLimits,
    sequence: u64,
}

/// Destructive format parameters. Each format must use a fresh device identity.
#[derive(Debug, Clone, Copy)]
pub struct FormatOptions {
    /// Fresh random 128-bit identity supplied by the application.
    /// Required even after an interrupted format to distinguish old frames.
    pub device_id: [u8; 16],
    /// Physical allocation stride, including the allocation header and tail footer.
    pub segment_size: u32,
    /// Persistent frame/value bounds, independent of batching targets.
    pub limits: FrameLimits,
}

impl Layout {
    fn new(capacity: u64, options: FormatOptions, sequence: u64) -> Result<Self> {
        let count = capacity.saturating_sub(START) / u64::from(options.segment_size.max(1));
        if options.device_id == [0; 16]
            || options.segment_size < 4 * PAGE_SIZE as u32
            || !is_aligned(options.segment_size as u64, PAGE_SIZE)
            || count == 0
            || count > u32::MAX as u64
            || sequence == 0
            || sequence.checked_add(count).is_none()
            || options.limits.max_frame_len() > i32::MAX as u32
            || options.limits.max_frame_len() as u64 + 3 * PAGE_SIZE > options.segment_size as u64
        {
            return Err(Error::InvalidArgument("invalid device geometry or frame bounds"));
        }
        Ok(Self {
            id: options.device_id,
            capacity,
            segment_size: options.segment_size,
            segments: count as u32,
            limits: options.limits,
            sequence,
        })
    }

    /// Device identity persisted at format.
    pub fn device_id(self) -> [u8; 16] {
        self.id
    }
    /// Physical capacity recorded at format; trailing partial slots are unused.
    pub fn capacity(self) -> u64 {
        self.capacity
    }
    /// Physical bytes per allocation, including its header and tail footer.
    pub fn segment_size(self) -> u32 {
        self.segment_size
    }
    /// Number of complete allocation slots.
    pub fn segment_count(self) -> u32 {
        self.segments
    }
    /// Format-wide limits loaded from the superblock.
    pub fn limits(self) -> FrameLimits {
        self.limits
    }
    /// Absolute base of one segment, or an error for an out-of-range selection.
    pub fn segment_base(self, number: u32) -> Result<u64> {
        if number >= self.segments {
            return Err(Error::InvalidArgument("segment number out of range"));
        }
        Ok(START + number as u64 * self.segment_size as u64)
    }
    pub(super) fn header(self, number: u32) -> Result<SegmentHeader> {
        self.segment_base(number)?;
        Ok(SegmentHeader::new(
            SegmentId {
                device_id: self.id,
                segment_no: number,
                sequence: self.sequence + number as u64,
            },
            self.segment_size,
        )?)
    }
    pub(super) fn footer_tail_offset(self, number: u32) -> Result<u64> {
        Ok(self.segment_base(number)? + self.segment_size as u64 - PAGE_SIZE)
    }
    fn encode(self, page: &mut [u8]) {
        page.fill(0);
        page[..8].copy_from_slice(&MAGIC);
        put_u32(page, 8, VERSION);
        page[16..32].copy_from_slice(&self.id);
        put_u64(page, 32, self.capacity);
        put_u32(page, 40, self.segment_size);
        put_u32(page, 44, self.segments);
        put_u32(page, 48, self.limits.max_frame_len());
        put_u32(page, 52, self.limits.max_value_len());
        put_u64(page, 56, self.sequence);
        put_u32(page, 12, crc_with_zeroed_checksum(page));
    }
    fn decode(page: &[u8]) -> Result<Self> {
        if page[..8] != MAGIC || u32_at(page, 12) != crc_with_zeroed_checksum(page) {
            return Err(Error::NotFormatted);
        }
        if u32_at(page, 8) != VERSION {
            return Err(Error::UnsupportedVersion(u32_at(page, 8)));
        }
        if page[64..].iter().any(|&b| b != 0) {
            return Err(Error::Corrupt("reserved superblock bytes"));
        }
        let options = FormatOptions {
            device_id: page[16..32].try_into().unwrap(),
            segment_size: u32_at(page, 40),
            limits: FrameLimits::new(u32_at(page, 48), u32_at(page, 52))?,
        };
        let layout = Self::new(u64_at(page, 32), options, u64_at(page, 56))?;
        if layout.segments != u32_at(page, 44) {
            return Err(Error::Corrupt("segment count"));
        }
        Ok(layout)
    }
    /// Loads matching geometry from either independently checksummed copy.
    pub fn read(device: &impl Device) -> Result<Self> {
        let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
        let mut found = None;
        for offset in [0, PAGE_SIZE] {
            device.read_at(&mut page, offset)?;
            match Self::decode(&page) {
                Ok(layout) => {
                    if found.is_some_and(|old| old != layout) {
                        return Err(Error::Corrupt("superblocks disagree"));
                    }
                    found = Some(layout);
                }
                Err(Error::NotFormatted) => {}
                Err(error) => return Err(error),
            }
        }
        let layout = found.ok_or(Error::NotFormatted)?;
        if device.capacity()? < layout.capacity {
            return Err(Error::Corrupt("device was truncated"));
        }
        Ok(layout)
    }
}

/// Reinitializes all allocation headers and commits immutable device geometry.
///
/// Destructive: exclusive access is required. This does not erase payloads or
/// discard the device. Epochs prevent old payloads from becoming new frames.
/// An interrupted format requires another format if no superblock was committed.
pub fn format(device: &impl Device, options: FormatOptions) -> Result<Layout> {
    let capacity = device.capacity()?;
    let sequence = match Layout::read(device) {
        Ok(old) => {
            if old.id == options.device_id {
                return Err(Error::InvalidArgument("format requires a fresh device identity"));
            }
            // Recovery trusts the allocation page's generation. Account for
            // newer incarnations too, so reformat cannot reuse their frame IDs.
            let mut last = old.sequence + old.segments as u64 - 1;
            let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
            for number in 0..old.segments {
                device.read_at(&mut page, old.segment_base(number)?)?;
                if page.iter().any(|&byte| byte != 0) {
                    let header = SegmentHeader::decode(&page, old.id, number, old.segment_size)?;
                    last = last.max(header.id().sequence);
                }
            }
            last.checked_add(1)
                .ok_or(Error::InvalidArgument("allocation epoch exhausted"))?
        }
        // With no surviving device identity, a fresh random UUID supplies the epoch.
        Err(Error::NotFormatted) => (u64::from_le_bytes(options.device_id[..8].try_into().unwrap()) >> 1).max(1),
        Err(error) => return Err(error),
    };
    let layout = Layout::new(capacity, options, sequence)?;
    let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
    for offset in [0, PAGE_SIZE] {
        device.write_at(&page, offset)?;
    }
    device.sync()?;
    for number in 0..layout.segments {
        device.write_at(&page, layout.segment_base(number)?)?;
        device.write_at(&page, layout.footer_tail_offset(number)?)?;
    }
    device.sync()?;
    layout.encode(&mut page);
    device.write_at(&page, 0)?;
    device.sync()?;
    device.write_at(&page, PAGE_SIZE)?;
    device.sync()?;
    Ok(layout)
}
