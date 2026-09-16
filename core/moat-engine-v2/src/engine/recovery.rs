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

//! Bounded device recovery with immutable allocation headers and optional seals.

use super::{Device, Engine, Error, Result};
use crate::{
    frame::FrameHeader,
    io::Queue,
    segment::{self, Footer, Scanner, SegmentHeader},
};
use moat_common::{AlignedBuf, PAGE_SIZE};

impl<D: Device, Q: Queue> Engine<D, Q> {
    pub(super) fn recover(&mut self) -> Result<()> {
        let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
        let mut seal = AlignedBuf::zeroed(PAGE_SIZE as usize);
        let mut frame = AlignedBuf::zeroed(self.layout.limits().max_frame_len() as usize);
        for number in 0..self.layout.segment_count() {
            let base = self.layout.segment_base(number)?;
            self.device.read_at(&mut page, base)?;
            self.device.read_at(&mut seal, self.layout.seal_offset(number)?)?;
            if page.iter().all(|&b| b == 0) {
                if seal.iter().any(|&b| b != 0) {
                    return Err(Error::Corrupt("seal without allocation header"));
                }
                continue;
            }
            let expected = self.layout.header(number)?;
            let mut header = SegmentHeader::decode(&page, expected.id().device_id, number, expected.segment_len())?;
            if header != expected {
                return Err(Error::Corrupt("allocation header or epoch"));
            }
            if seal.iter().any(|&b| b != 0) {
                match SegmentHeader::decode(&seal, expected.id().device_id, number, expected.segment_len()) {
                    Ok(sealed) if sealed.id() == expected.id() && sealed.is_sealed() => header = sealed,
                    Ok(_) => return Err(Error::Corrupt("seal allocation identity or state")),
                    Err(segment::Error::UnsupportedVersion(version)) => {
                        return Err(segment::Error::UnsupportedVersion(version).into());
                    }
                    // The immutable allocation header survives a torn seal page.
                    Err(_) => {}
                }
            }
            self.used[number as usize] = true;
            self.allocated += 1;
            self.pipeline.attach(header, base, false)?;
            if let Some(range) = header.footer_range() {
                let mut bytes = AlignedBuf::zeroed(range.len());
                self.device.read_at(&mut bytes, base + range.start as u64)?;
                match Footer::decode(&bytes, header, self.layout.limits()) {
                    Ok(footer) => {
                        for metadata in footer.frames() {
                            self.pipeline.restore(metadata)?;
                        }
                        continue;
                    }
                    Err(error) if unsupported(&error) => return Err(error.into()),
                    // Scan against the committed boundary, not an active prefix.
                    Err(_) => {}
                }
            }
            let mut scanner = Scanner::new(header, self.layout.limits());
            while let Some(position) = scanner.position() {
                self.device
                    .read_at(&mut frame[..PAGE_SIZE as usize], base + position.offset() as u64)?;
                let len = match FrameHeader::decode(&frame[..PAGE_SIZE as usize], self.layout.limits(), position) {
                    Ok(header) => header.frame_len(),
                    Err(_) => {
                        // Scanner distinguishes incomplete active tails, sealed
                        // damage, and unsupported versions using the same bytes.
                        scanner.next_frame(&frame[..PAGE_SIZE as usize])?;
                        break;
                    }
                };
                if len > PAGE_SIZE as usize {
                    self.device.read_at(
                        &mut frame[PAGE_SIZE as usize..len],
                        base + position.offset() as u64 + PAGE_SIZE,
                    )?;
                }
                match scanner.next_frame(&frame[..len])? {
                    Some(frame) => self.pipeline.restore(frame.metadata())?,
                    None => break,
                }
            }
        }
        Ok(())
    }
}

fn unsupported(error: &segment::Error) -> bool {
    matches!(
        error,
        segment::Error::UnsupportedVersion(_)
            | segment::Error::Frame {
                source: crate::frame::Error::UnsupportedVersion(_),
                ..
            }
    )
}
