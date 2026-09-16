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

use super::{Error, MIN_FRAME_METADATA_LEN, Result, SegmentHeader, footer_len};
use crate::frame::{self, Frame, FrameLimits, FramePosition};

/// Incremental recovery of complete frames from a trusted segment header.
///
/// Supply complete candidate frame bytes (or the remaining segment slice) at
/// `position`. A damaged active tail ends recovery without searching for another
/// magic value. Sealed segments require every frame before their committed data
/// boundary to validate; footer failure never weakens that requirement.
/// This scanner neither merges LSNs nor authorizes appending to recovered tails.
#[derive(Debug)]
pub struct Scanner {
    header: SegmentHeader,
    limits: FrameLimits,
    offset: u32,
    frame_count: u32,
    metadata_len: u64,
    done: bool,
    tail_error: Option<frame::Error>,
}

impl Scanner {
    /// Starts after the segment header. A damaged header must not reach here.
    pub fn new(header: SegmentHeader, limits: FrameLimits) -> Self {
        Self {
            header,
            limits,
            offset: PAGE_SIZE as u32,
            frame_count: 0,
            metadata_len: 0,
            done: false,
            tail_error: None,
        }
    }

    /// Expected position of the next frame, or none once recovery has ended.
    /// An I/O caller may validate its fixed frame header first to bound the read.
    pub fn position(&self) -> Option<FramePosition> {
        if self.done || self.at_end() {
            return None;
        }
        self.header.position(self.offset).ok()
    }

    fn at_end(&self) -> bool {
        if let Some(seal) = self.header.seal {
            return self.offset == seal.data_end;
        }
        // Even an empty-value frame needs one descriptor and a footer reservation.
        self.offset as u64 + PAGE_SIZE + footer_len(self.metadata_len + MIN_FRAME_METADATA_LEN)
            > self.header.segment_len as u64
    }

    /// Validates the entire next frame before exposing any of its records.
    ///
    /// `Ok(None)` means a complete sealed extent or the end of an active prefix.
    /// For active tails, `tail_error` explains a rejected candidate. An unsupported
    /// frame version is always returned as an error, never silently discarded.
    /// I/O failures must be handled by the caller, not converted to empty input.
    pub fn next_frame<'a>(&mut self, bytes: &'a [u8]) -> Result<Option<Frame<'a>>> {
        if self.done {
            return Ok(None);
        }
        if self.at_end() {
            self.done = true;
            return Ok(None);
        }
        let decode = || {
            let position = self.header.position(self.offset)?;
            let frame = Frame::decode(bytes, self.limits, position)?;
            let end = self.offset as u64 + frame.as_bytes().len() as u64;
            let metadata_len = self.metadata_len + frame.metadata().as_bytes().len() as u64;
            if end + footer_len(metadata_len) > self.header.segment_len as u64 {
                return Err(frame::Error::Corrupt("frame leaves no room for the segment footer"));
            }
            if let Some(seal) = self.header.seal {
                let count = self.frame_count + 1;
                if count > seal.frame_count
                    || metadata_len > seal.metadata_len as u64
                    || (end == seal.data_end as u64
                        && (count != seal.frame_count || metadata_len != seal.metadata_len as u64))
                    || (end < seal.data_end as u64 && count == seal.frame_count)
                {
                    return Err(frame::Error::Corrupt("frames disagree with sealed summary"));
                }
            }
            Ok(frame)
        };
        match decode() {
            Ok(frame) => {
                self.offset += frame.as_bytes().len() as u32;
                self.frame_count += 1;
                self.metadata_len += frame.metadata().as_bytes().len() as u64;
                Ok(Some(frame))
            }
            Err(source) => {
                self.done = true;
                if self.header.is_sealed() || matches!(source, frame::Error::UnsupportedVersion(_)) {
                    return Err(Error::Frame {
                        offset: self.offset,
                        source,
                    });
                }
                self.tail_error = Some(source);
                Ok(None)
            }
        }
    }

    /// End of the fully validated frame prefix, in segment-relative bytes.
    pub fn data_end(&self) -> u32 {
        self.offset
    }

    /// Why the first rejected active-tail candidate could not be recovered.
    /// Absence does not prove that an active segment had been cleanly sealed.
    pub fn tail_error(&self) -> Option<&frame::Error> {
        self.tail_error.as_ref()
    }
}
