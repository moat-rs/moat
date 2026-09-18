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

//! Queue-driven recovery. Each step validates at most one frame or CRC window.

use super::{Device, Engine, Error, Layout, Result, lifecycle::Progress};
use crate::{
    frame::{FrameHeader, Metadata},
    io::{Buffer, Queue},
    segment::{self, FooterTrailer, FooterValidator, Scanner, SegmentHeader},
};
use moat_common::{AlignedBuf, PAGE_SIZE};

const PAGE: usize = PAGE_SIZE as usize;
enum Stage {
    Start,
    SuperA,
    SuperB,
    Header,
    HeaderDone,
    Tail,
    FooterRead,
    FooterValidate,
    Restore,
    Scan,
    ScanHeader,
    ScanBody,
}
pub(super) struct Recovery {
    stage: Stage,
    found: Option<Layout>,
    number: u32,
    header: Option<SegmentHeader>,
    page: Option<Buffer>,
    bytes: Option<Buffer>,
    validator: Option<FooterValidator>,
    scanner: Option<Scanner>,
    metadata_at: usize,
    frame_offset: u32,
    frame_len: usize,
}
impl Recovery {
    pub fn new() -> Self {
        Self {
            stage: Stage::Start,
            found: None,
            number: 0,
            header: None,
            page: None,
            bytes: None,
            validator: None,
            scanner: None,
            metadata_at: 0,
            frame_offset: PAGE as u32,
            frame_len: 0,
        }
    }
    fn page_read(offset: u64) -> Progress {
        Progress::read(offset, AlignedBuf::zeroed(PAGE).into(), PAGE)
    }
    fn superblock(&mut self, bytes: &[u8]) -> Result<()> {
        match Layout::decode(bytes) {
            Ok(layout) => {
                if self.found.is_some_and(|old| old != layout) {
                    return Err(Error::Corrupt("superblocks disagree"));
                }
                self.found = Some(layout);
                Ok(())
            }
            Err(Error::NotFormatted) => Ok(()),
            Err(error) => Err(error),
        }
    }
    fn scan(&mut self, limits: crate::frame::FrameLimits) {
        self.bytes = None;
        self.validator = None;
        self.scanner = Some(Scanner::new(self.header.expect("allocation header"), limits));
        self.stage = Stage::Scan;
    }
    fn next(&mut self) -> Progress {
        self.number += 1;
        self.header = None;
        self.page = None;
        self.bytes = None;
        self.validator = None;
        self.scanner = None;
        self.stage = Stage::Header;
        Progress::Continue
    }
    pub fn step<D: Device, Q: Queue>(&mut self, engine: &mut Engine<D, Q>, input: Option<Buffer>) -> Result<Progress> {
        match self.stage {
            Stage::Start => {
                self.stage = Stage::SuperA;
                return Ok(Self::page_read(0));
            }
            Stage::SuperA => {
                self.superblock(&input.expect("first superblock"))?;
                self.stage = Stage::SuperB;
                return Ok(Self::page_read(PAGE_SIZE));
            }
            Stage::SuperB => {
                self.superblock(&input.expect("second superblock"))?;
                let layout = self.found.ok_or(Error::NotFormatted)?;
                if engine.capacity < layout.capacity() {
                    return Err(Error::Corrupt("device was truncated"));
                }
                if layout.segment_count() as usize
                    > crate::pipeline::Pipeline::<Q>::route_capacity(engine.options.resources.metadata_bytes)
                {
                    return Err(crate::pipeline::Error::ResourceLimit("segment routing metadata").into());
                }
                if layout.limits().max_frame_len() as usize > engine.options.resources.frame_bytes {
                    return Err(crate::pipeline::Error::ResourceLimit("configured frame buffer bound").into());
                }
                engine.used.try_reserve_exact(layout.segment_count() as usize)?;
                engine.used.resize(layout.segment_count() as usize, false);
                engine.pipeline.set_limits(layout.limits());
                engine.layout = Some(layout);
                self.stage = Stage::Header;
                return Ok(Progress::Continue);
            }
            _ => {}
        }
        let layout = engine.layout.expect("validated superblock");
        let limits = layout.limits();
        if matches!(self.stage, Stage::Header) && self.number == layout.segment_count() {
            return Ok(Progress::Done);
        }
        let base = layout.segment_base(self.number)?;
        match self.stage {
            Stage::Header => {
                self.stage = Stage::HeaderDone;
                Ok(Self::page_read(base))
            }
            Stage::HeaderDone => {
                self.page = input;
                self.stage = Stage::Tail;
                Ok(Self::page_read(layout.footer_tail_offset(self.number)?))
            }
            Stage::Tail => {
                let tail = input.expect("tail page");
                let page = self.page.take().expect("allocation page");
                if page.iter().all(|b| *b == 0) {
                    if tail.iter().any(|b| *b != 0) {
                        return Err(Error::Corrupt("footer without allocation header"));
                    }
                    return Ok(self.next());
                }
                let mut header = SegmentHeader::decode(&page, layout.device_id(), self.number, layout.segment_size())?;
                if tail.iter().any(|b| *b != 0) {
                    match FooterTrailer::decode(&tail, header.segment_len()) {
                        Ok(trailer) => {
                            let sealed = trailer.header();
                            if sealed.id().device_id != header.id().device_id
                                || sealed.id().segment_no != header.id().segment_no
                                || sealed.id().sequence > header.id().sequence
                            {
                                return Err(Error::Corrupt("footer allocation identity or future generation"));
                            }
                            if sealed.id().sequence == header.id().sequence {
                                header = sealed;
                            }
                        }
                        Err(segment::Error::UnsupportedVersion(v)) => {
                            return Err(segment::Error::UnsupportedVersion(v).into());
                        }
                        Err(_) => {}
                    }
                }
                engine.used[self.number as usize] = true;
                engine.allocated += 1;
                engine.pipeline.attach(header, base, false)?;
                self.header = Some(header);
                if let Some(range) = header.footer_range()
                    && range.len() <= engine.options.resources.metadata_bytes
                {
                    self.stage = Stage::FooterRead;
                    if range.len() == PAGE {
                        self.bytes = Some(tail);
                        return Ok(Progress::Continue);
                    }
                    let mut bytes = AlignedBuf::zeroed(range.len());
                    let split = range.len() - PAGE;
                    bytes[split..].copy_from_slice(&tail);
                    return Ok(Progress::read(base + range.start as u64, bytes.into(), split));
                }
                self.scan(limits);
                Ok(Progress::Continue)
            }
            Stage::FooterRead => {
                if let Some(input) = input {
                    self.bytes = Some(input);
                }
                match FooterValidator::new(
                    self.bytes.as_ref().expect("footer"),
                    self.header.expect("sealed header"),
                    limits,
                ) {
                    Ok(validator) => {
                        self.validator = Some(validator);
                        self.stage = Stage::FooterValidate;
                    }
                    Err(error) if unsupported(&error) => return Err(error.into()),
                    Err(_) => self.scan(limits),
                }
                Ok(Progress::Continue)
            }
            Stage::FooterValidate => {
                match self
                    .validator
                    .as_mut()
                    .expect("validator")
                    .step(self.bytes.as_ref().expect("footer"))
                {
                    Ok(true) => {
                        self.metadata_at = 0;
                        self.frame_offset = PAGE as u32;
                        self.stage = Stage::Restore;
                    }
                    Ok(false) => {}
                    Err(error) if unsupported(&error) => return Err(error.into()),
                    Err(_) => self.scan(limits),
                }
                Ok(Progress::Continue)
            }
            Stage::Restore => {
                let header = self.header.expect("sealed header");
                if self.frame_offset == header.data_end().expect("sealed boundary") {
                    return Ok(self.next());
                }
                let position = crate::frame::FramePosition::new(
                    header.id().sequence,
                    self.frame_offset,
                    header.data_end().unwrap(),
                )?;
                let metadata = Metadata::decode(
                    &self.bytes.as_ref().expect("footer")[self.metadata_at..],
                    limits,
                    position,
                )?;
                engine.pipeline.restore(metadata)?;
                self.metadata_at += metadata.as_bytes().len();
                self.frame_offset += metadata.header().frame_len() as u32;
                Ok(Progress::Continue)
            }
            Stage::Scan => {
                let Some(position) = self.scanner.as_ref().expect("scanner").position() else {
                    return Ok(self.next());
                };
                self.stage = Stage::ScanHeader;
                Ok(Self::page_read(base + position.offset() as u64))
            }
            Stage::ScanHeader => {
                let page = input.expect("frame header page");
                let scanner = self.scanner.as_mut().expect("scanner");
                let position = scanner.position().expect("candidate position");
                self.frame_len = match FrameHeader::decode(&page, limits, position) {
                    Ok(header) => header.frame_len(),
                    Err(_) => {
                        scanner.next_frame(&page)?;
                        return Ok(self.next());
                    }
                };
                if self.frame_len == PAGE {
                    match scanner.next_frame(&page)? {
                        Some(frame) => engine.pipeline.restore(frame.metadata())?,
                        None => return Ok(self.next()),
                    }
                    self.stage = Stage::Scan;
                    return Ok(Progress::Continue);
                }
                self.page = Some(page);
                self.stage = Stage::ScanBody;
                let len = self.frame_len - PAGE;
                Ok(Progress::read(
                    base + position.offset() as u64 + PAGE_SIZE,
                    AlignedBuf::zeroed(len).into(),
                    len,
                ))
            }
            Stage::ScanBody => {
                let body = input.expect("frame body");
                let mut bytes = AlignedBuf::zeroed(self.frame_len);
                bytes[..PAGE].copy_from_slice(&self.page.take().expect("frame header"));
                bytes[PAGE..].copy_from_slice(&body);
                match self.scanner.as_mut().expect("scanner").next_frame(&bytes)? {
                    Some(frame) => engine.pipeline.restore(frame.metadata())?,
                    None => return Ok(self.next()),
                }
                self.stage = Stage::Scan;
                Ok(Progress::Continue)
            }
            Stage::Start | Stage::SuperA | Stage::SuperB => unreachable!(),
        }
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
