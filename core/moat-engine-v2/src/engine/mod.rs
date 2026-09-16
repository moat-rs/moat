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

//! Single-owner device routing and append-only segment rollover.
//!
//! One index and one queue serve all allocated segments. Ordinary reads/writes
//! remain asynchronous. Formatting, recovery, sealing, and allocation use cold
//! positional I/O; rollover requires a drained pipeline. No segment is reused.

mod device;
mod error;
mod layout;
mod recovery;

use crate::{
    frame::FrameBuilder,
    io::{Buffer, Queue},
    pipeline::{self, Completion, Pipeline, ReadBuffers, ReadRequirements, Ticket},
    segment,
};
use moat_common::{AlignedBuf, ChunkId, PAGE_SIZE};
use std::ops::Range;

pub use device::Device;
pub use error::{Error, Rejected, Result};
pub use layout::{FormatOptions, Layout, format};

/// An exclusively owned device, its global index, and a dedicated I/O queue.
///
/// Callers must provide exclusive device ownership and a queue for that same
/// device. Completed data remains immutable. Reopen never resumes an old active
/// tail: new writes allocate an unused segment. Rollover chooses the next unused
/// slot by default; `rollover_to` lets the caller select a different unused slot.
/// There is no automatic reclamation, segment reuse, or background worker.
pub struct Engine<D, Q> {
    device: D,
    layout: Layout,
    pipeline: Pipeline<Q>,
    used: Vec<bool>,
    next: u32,
    allocated: u32,
    active: Option<u32>,
    failed: bool,
}

impl<D: Device, Q: Queue> Engine<D, Q> {
    /// Opens persisted geometry and rebuilds the global index. Sealed footers
    /// accelerate recovery; active segments are scanned with payload validation.
    pub fn open(device: D, queue: Q) -> Result<Self> {
        let layout = Layout::read(&device)?;
        let pipeline = Pipeline::empty(queue, layout.limits())?;
        let mut engine = Self {
            device,
            layout,
            pipeline,
            used: vec![false; layout.segment_count() as usize],
            next: 0,
            allocated: 0,
            active: None,
            failed: false,
        };
        engine.recover()?;
        Ok(engine)
    }

    /// Persisted geometry and decoding bounds.
    pub fn layout(&self) -> Layout {
        self.layout
    }
    /// Segment receiving new frames, if one is allocated in this session.
    pub fn active_segment(&self) -> Option<u32> {
        self.active
    }
    /// Number of allocated segments, including recovered active tails.
    pub fn allocated_segments(&self) -> usize {
        self.allocated as usize
    }
    /// Reserves room for additional indexed keys before serving the workload.
    /// This is a memory reservation, not a hard admission budget.
    pub fn reserve_index(&mut self, additional: usize) -> Result<()> {
        Ok(self.pipeline.reserve_index(additional)?)
    }
    /// Number of indexed latest versions, including tombstones.
    pub fn indexed_versions(&self) -> usize {
        self.pipeline.index_len()
    }
    /// Number of user operations still awaiting completion delivery.
    pub fn in_flight(&self) -> usize {
        self.pipeline.in_flight()
    }
    /// Whether the latest published version is a data record.
    pub fn contains(&self, key: &ChunkId) -> bool {
        self.pipeline.contains(key)
    }
    /// Drives the shared queue and delivers read/write/flush completions.
    pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize> {
        Ok(self.pipeline.poll(wait, out)?)
    }
    /// Required buffer capacities for the currently published record version.
    pub fn read_requirements(&self, key: ChunkId, range: Range<u32>, verify: bool) -> Result<ReadRequirements> {
        Ok(self.pipeline.read_requirements(key, range, verify)?)
    }
    /// Reads a snapshot from any allocated segment, with optional CRC checks.
    pub fn read(
        &mut self,
        key: ChunkId,
        range: Range<u32>,
        verify: bool,
        buffers: ReadBuffers,
    ) -> std::result::Result<Ticket, Rejected<ReadBuffers>> {
        self.pipeline.read(key, range, verify, buffers).map_err(|r| Rejected {
            error: r.error.into(),
            input: r.input,
        })
    }
    /// Orders a persistence barrier after preceding writes. Does not seal the segment.
    pub fn flush(&mut self) -> Result<Ticket> {
        if self.failed {
            return Err(Error::Failed);
        }
        Ok(self.pipeline.flush()?)
    }
    /// Encodes and submits a frame, rolling over a full segment after draining.
    /// Backpressure preserves the buffer; call `poll` and retry. Segment transition
    /// I/O is synchronous and occurs only on allocation or rollover, never per record.
    pub fn write(
        &mut self,
        frame: &FrameBuilder<'_>,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        self.submit(buffer.into(), |pipeline, buffer| pipeline.write(frame, buffer))
    }
    /// Submits an already filled prepared value without another payload copy.
    pub fn write_prepared(
        &mut self,
        key: ChunkId,
        lsn: u64,
        len: u32,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        self.submit(buffer.into(), |pipeline, buffer| {
            pipeline.write_prepared(key, lsn, len, buffer)
        })
    }

    fn submit(
        &mut self,
        buffer: Buffer,
        mut submit: impl FnMut(&mut Pipeline<Q>, Buffer) -> std::result::Result<Ticket, pipeline::Rejected<Buffer>>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        if let Err(error) = self.ensure_active() {
            return Err(Rejected { error, input: buffer });
        }
        match submit(&mut self.pipeline, buffer) {
            Ok(ticket) => Ok(ticket),
            Err(rejected) => {
                if matches!(rejected.error, pipeline::Error::Segment(segment::Error::Full { .. }))
                    && self.pipeline.data_end().is_some_and(|end| end > PAGE_SIZE as u32)
                {
                    if let Err(error) = self.rollover() {
                        return Err(Rejected {
                            error,
                            input: rejected.input,
                        });
                    }
                    submit(&mut self.pipeline, rejected.input).map_err(|r| Rejected {
                        error: r.error.into(),
                        input: r.input,
                    })
                } else {
                    Err(Rejected {
                        error: rejected.error.into(),
                        input: rejected.input,
                    })
                }
            }
        }
    }

    fn ensure_active(&mut self) -> Result<()> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.active.is_none() {
            self.rollover()?;
        }
        Ok(())
    }

    fn check_idle(&self) -> Result<()> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.pipeline.in_flight() != 0 {
            return Err(pipeline::Error::Backpressure.into());
        }
        Ok(())
    }

    /// Seals the current allocation and activates the next unused segment.
    /// Requires all completions to have been delivered; otherwise returns backpressure.
    pub fn rollover(&mut self) -> Result<()> {
        self.check_idle()?;
        while self.next < self.layout.segment_count() && self.used[self.next as usize] {
            self.next += 1;
        }
        if self.next == self.layout.segment_count() {
            return Err(Error::OutOfSpace);
        }
        self.rollover_to(self.next)
    }

    /// Selects a caller-chosen unused segment. Occupied segments are never reclaimed.
    /// The previous allocation is durably sealed before the new header is persisted.
    pub fn rollover_to(&mut self, number: u32) -> Result<()> {
        self.check_idle()?;
        let base = self.layout.segment_base(number)?;
        if self.used[number as usize] {
            return Err(Error::InvalidArgument("segment is already allocated"));
        }
        self.seal()?;
        let header = self.layout.header(number)?;
        let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
        header.encode_into(&mut page)?;
        // Retire the slot even if the header write fails. Reopen decides whether
        // it is a valid allocation; this owner cannot retry into uncertain bytes.
        self.used[number as usize] = true;
        self.allocated += 1;
        if let Err(error) = self.device.write_at(&page, base).and_then(|()| self.device.sync()) {
            self.failed = true;
            return Err(error.into());
        }
        if let Err(error) = self.pipeline.attach(header, base, true) {
            self.failed = true;
            return Err(error.into());
        }
        self.active = Some(number);
        Ok(())
    }

    /// Writes the footer, persists data/footer, then persists an independent seal
    /// header at the end of the physical slot. The original active header stays
    /// unchanged. An interrupted seal can therefore recover by scanning frames.
    /// Requires a drained pipeline. A lifecycle I/O failure prevents further writes.
    pub fn seal(&mut self) -> Result<()> {
        self.check_idle()?;
        let Some(number) = self.active else {
            return Ok(());
        };
        let mut builder = self.pipeline.take_segment()?.expect("active allocation has a builder");
        self.active = None;
        let result = (|| -> Result<()> {
            let offset = builder.data_end();
            let mut footer = AlignedBuf::zeroed(builder.footer_len());
            let header = builder.seal_into(&mut footer)?;
            self.device
                .write_at(&footer, self.layout.segment_base(number)? + offset as u64)?;
            self.device.sync()?;
            let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
            header.encode_into(&mut page)?;
            self.device.write_at(&page, self.layout.seal_offset(number)?)?;
            self.device.sync()?;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
