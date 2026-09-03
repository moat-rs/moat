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

//! State shared by the writer and all readers of one engine instance.

use std::sync::Arc;

use moat_common::{AlignedBuf, PAGE_SIZE};

use crate::{
    device::Device,
    error::Result,
    index::Index,
    layout::{Extent, SEGMENT_HEADER_LEN, SegmentHeader, Superblock},
    options::Options,
    segments::{Geometry, SegmentTable},
};

pub(crate) struct Shared {
    pub(crate) device: Arc<dyn Device>,
    pub(crate) superblock: Superblock,
    pub(crate) geometry: Geometry,
    pub(crate) index: Index,
    pub(crate) segments: SegmentTable,
    pub(crate) options: Options,
}

impl Shared {
    /// Reads an aligned extent of a segment into a fresh buffer.
    pub(crate) fn read_extent(&self, seg_no: u32, extent: Extent) -> Result<AlignedBuf> {
        let mut buf = AlignedBuf::zeroed(extent.len as usize);
        self.device
            .read_at(&mut buf, self.geometry.segment_offset(seg_no) + extent.start)?;
        Ok(buf)
    }

    /// Writes `data` at `offset` within segment `seg_no`.
    pub(crate) fn write_segment_bytes(&self, seg_no: u32, offset: u64, data: &[u8]) -> Result<()> {
        debug_assert!(offset.is_multiple_of(PAGE_SIZE) && (data.len() as u64).is_multiple_of(PAGE_SIZE));
        self.device
            .write_at(data, self.geometry.segment_offset(seg_no) + offset)?;
        Ok(())
    }

    pub(crate) fn read_segment_header(&self, seg_no: u32) -> Result<SegmentHeader> {
        let mut buf = AlignedBuf::zeroed(SEGMENT_HEADER_LEN as usize);
        self.device.read_at(&mut buf, self.geometry.segment_offset(seg_no))?;
        SegmentHeader::decode(&buf)
    }

    pub(crate) fn write_segment_header(&self, header: &SegmentHeader) -> Result<()> {
        let mut buf = AlignedBuf::zeroed(SEGMENT_HEADER_LEN as usize);
        header.encode(&mut buf);
        self.device
            .write_at(&buf, self.geometry.segment_offset(header.seg_no))?;
        Ok(())
    }

    pub(crate) fn now(&self) -> u64 {
        self.options.clock.now_secs()
    }

    /// Whether a record with the given expiry is expired at the current time.
    pub(crate) fn is_expired(&self, expire_at: u64) -> bool {
        expire_at != 0 && self.now() >= expire_at
    }
}
