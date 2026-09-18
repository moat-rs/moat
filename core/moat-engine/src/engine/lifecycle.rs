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

//! Incremental lifecycle transitions sharing the payload I/O queue.

use super::{Device, Engine, Lifecycle, Result, recovery::Recovery};
use crate::{
    io::{Buffer, Operation, Queue},
    pipeline::Ticket,
    segment::FooterEncoder,
};
use moat_common::{AlignedBuf, PAGE_SIZE};

pub(super) enum Progress {
    Continue,
    Wait,
    Done,
    Io {
        operation: Operation,
        offset: u64,
        len: usize,
        buffer: Option<Buffer>,
    },
}
impl Progress {
    pub(super) fn read(offset: u64, buffer: Buffer, len: usize) -> Self {
        Self::Io {
            operation: Operation::Read,
            offset,
            len,
            buffer: Some(buffer),
        }
    }
    fn write(offset: u64, buffer: Buffer) -> Self {
        let len = buffer.len();
        Self::Io {
            operation: Operation::Write,
            offset,
            len,
            buffer: Some(buffer),
        }
    }
    fn sync(mode: super::SyncMode) -> Self {
        if mode == super::SyncMode::Disabled {
            return Self::Continue;
        }
        Self::Io {
            operation: Operation::Sync,
            offset: 0,
            len: 0,
            buffer: None,
        }
    }
}
pub(super) struct Job {
    pub ticket: Option<Ticket>,
    pub kind: Lifecycle,
    pub waiting: bool,
    pub blocked: bool,
    work: Work,
}
enum Work {
    Open(Box<Recovery>),
    Transition(Box<Transition>),
}
impl Job {
    pub fn open(ticket: Ticket) -> Self {
        Self {
            ticket: Some(ticket),
            kind: Lifecycle::Open,
            waiting: false,
            blocked: false,
            work: Work::Open(Box::new(Recovery::new())),
        }
    }
    pub fn transition(ticket: Option<Ticket>, kind: Lifecycle, next: Option<u32>) -> Self {
        Self {
            ticket,
            kind,
            waiting: false,
            blocked: false,
            work: Work::Transition(Box::new(Transition {
                stage: Stage::Drain,
                next,
                footer: None,
                base: 0,
                last: None,
            })),
        }
    }
    pub fn step<D: Device, Q: Queue>(&mut self, engine: &mut Engine<D, Q>, input: Option<Buffer>) -> Result<Progress> {
        match &mut self.work {
            Work::Open(recovery) => recovery.step(engine, input),
            Work::Transition(transition) => transition.step(engine, self.kind, input),
        }
    }
}
enum Stage {
    Drain,
    Footer,
    LastWrite,
    LastSync,
    Allocate,
    HeaderSync,
    Attach,
    Closed,
}
struct Transition {
    stage: Stage,
    next: Option<u32>,
    footer: Option<FooterEncoder>,
    base: u64,
    last: Option<(u32, Buffer)>,
}
impl Transition {
    fn step<D: Device, Q: Queue>(
        &mut self,
        engine: &mut Engine<D, Q>,
        kind: Lifecycle,
        _input: Option<Buffer>,
    ) -> Result<Progress> {
        if !engine.opened {
            return Err(super::Error::Failed);
        }
        let layout = engine.layout.expect("recovered layout");
        match self.stage {
            Stage::Drain => {
                if engine.pipeline.writes_pending() || (kind == Lifecycle::Close && engine.pipeline.in_flight() != 0) {
                    return Ok(Progress::Wait);
                }
                if let Some(number) = engine.active {
                    let builder = engine.pipeline.take_segment()?.expect("active segment builder");
                    engine.active = None;
                    self.base = layout.segment_base(number)?;
                    self.footer = Some(builder.into_footer());
                    self.stage = Stage::Footer;
                    Ok(Progress::Continue)
                } else if kind == Lifecycle::Close {
                    self.stage = Stage::Closed;
                    Ok(Progress::sync(engine.options.sync_mode))
                } else {
                    self.stage = Stage::Allocate;
                    Ok(Progress::Continue)
                }
            }
            Stage::Footer => {
                let Some((offset, bytes, last)) = self.footer.as_mut().expect("footer encoder").next() else {
                    unreachable!()
                };
                if last {
                    self.last = Some((offset, bytes.into()));
                    self.stage = Stage::LastWrite;
                    Ok(Progress::sync(engine.options.sync_mode))
                } else {
                    Ok(Progress::write(self.base + offset as u64, bytes.into()))
                }
            }
            Stage::LastWrite => {
                let (offset, buffer) = self.last.take().expect("final footer page");
                self.stage = Stage::LastSync;
                Ok(Progress::write(self.base + offset as u64, buffer))
            }
            Stage::LastSync => {
                self.stage = Stage::Allocate;
                Ok(Progress::sync(engine.options.sync_mode))
            }
            Stage::Allocate => {
                self.footer = None;
                let Some(number) = self.next else {
                    return Ok(Progress::Done);
                };
                let header = layout.header(number)?;
                let mut bytes = AlignedBuf::zeroed(PAGE_SIZE as usize);
                header.encode_into(&mut bytes)?;
                // An uncertain allocation is never reused within this owner.
                engine.used[number as usize] = true;
                engine.allocated += 1;
                self.stage = Stage::HeaderSync;
                Ok(Progress::write(layout.segment_base(number)?, bytes.into()))
            }
            Stage::HeaderSync => {
                self.stage = Stage::Attach;
                Ok(Progress::sync(engine.options.sync_mode))
            }
            Stage::Attach => {
                let number = self.next.expect("allocation target");
                engine
                    .pipeline
                    .attach(layout.header(number)?, layout.segment_base(number)?, true)?;
                engine.active = Some(number);
                Ok(Progress::Done)
            }
            Stage::Closed => Ok(Progress::Done),
        }
    }
}
