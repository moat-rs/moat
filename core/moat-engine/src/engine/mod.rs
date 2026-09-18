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

//! Single-owner asynchronous recovery, I/O, and segment lifecycle.

mod completion;
mod device;
mod error;
mod layout;
mod lifecycle;
mod recovery;

use crate::{
    frame::{FrameBuilder, FrameLimits},
    io::{Buffer, Queue},
    pipeline::{self, Pipeline, ReadBuffers, ReadRequirements, Ticket},
    segment,
};
use moat_common::{ChunkId, PAGE_SIZE};
use std::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

pub use crate::pipeline::{PollBudget, ResourceLimits};
pub use completion::{Completion, Lifecycle};
pub use device::Device;
pub use error::{Error, Rejected, Result};
pub use layout::{FormatOptions, Layout, format};
use lifecycle::{Job, Progress};

/// Whether the engine requests persistence barriers from its backing device.
/// This is an explicit runtime policy, not a persisted property or PLP detector.
/// It does not change the asynchronous ticket/poll API in either mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncMode {
    /// Request device syncs for format, lifecycle transitions, and explicit flush.
    #[default]
    Enabled,
    /// Never request device syncs. Flush still drains preceding writes and reports
    /// errors. Durability depends on the backing device's write-completion contract;
    /// otherwise use disposable data and reformat after an unclean shutdown.
    Disabled,
}

/// Runtime resource limits and cooperative work targets, not persistent geometry.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Persistence policy for this owner, including explicit flush and close.
    pub sync_mode: SyncMode,
    /// Metadata/index admission bounds.
    pub resources: ResourceLimits,
    /// Work retired by one data-path poll; lifecycle advances at most two steps.
    pub poll: PollBudget,
}
/// Public engine lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Geometry/index recovery is pending.
    Opening,
    /// Reads are available; a segment transition may still backpressure writes.
    Ready,
    /// Admission stopped; accepted operations are being drained.
    Closing,
    /// Explicit shutdown completed; the object can be dropped.
    Closed,
    /// Recovery, queue, or write lifecycle failed. Previously published data may
    /// remain readable if recovery completed and the queue is healthy.
    Failed,
}
/// A bounded, weakly consistent traversal of the keys present when created.
/// Later overwrites may be observed; later new keys are excluded.
#[derive(Debug)]
pub struct VersionCursor {
    owner: u64,
    next: usize,
    end: usize,
}

/// One exclusive device owner. All online disk I/O goes through its Queue.
/// `open` returns before recovery and must be driven with `poll`. Blocking
/// helpers are explicitly named; no background thread is created.
pub struct Engine<D, Q> {
    _device: D,
    capacity: u64,
    layout: Option<Layout>,
    pipeline: Pipeline<Q>,
    used: Vec<bool>,
    next: u32,
    allocated: u32,
    active: Option<u32>,
    state: State,
    opened: bool,
    job: Option<Job>,
    options: Options,
    completions: Vec<pipeline::Completion>,
    identity: u64,
}
impl<D: Device, Q: Queue> Engine<D, Q> {
    /// Accepts recovery without reading device contents. The capacity query and
    /// owner construction are synchronous setup; recovery I/O is poll-driven.
    pub fn open(device: D, queue: Q) -> Result<(Self, Ticket)> {
        Self::open_with_options(device, queue, Options::default())
    }
    /// Opens with explicit index/metadata bounds and progress budgets.
    pub fn open_with_options(device: D, queue: Q, options: Options) -> Result<(Self, Ticket)> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let capacity = device.capacity()?;
        let depth = queue.depth();
        let mut pipeline = Pipeline::empty(queue, FrameLimits::new(4096, 0)?)?;
        pipeline.configure(options.resources, options.poll)?;
        pipeline.set_sync_enabled(options.sync_mode == SyncMode::Enabled);
        let ticket = pipeline.next_ticket()?;
        pipeline.maintenance(true);
        Ok((
            Self {
                _device: device,
                capacity,
                layout: None,
                pipeline,
                used: Vec::new(),
                next: 0,
                allocated: 0,
                active: None,
                state: State::Opening,
                opened: false,
                job: Some(Job::open(ticket)),
                options,
                completions: Vec::with_capacity(depth),
                identity: NEXT.fetch_add(1, Ordering::Relaxed),
            },
            ticket,
        ))
    }
    /// Explicit blocking recovery helper for tools and synchronous adapters.
    pub fn open_blocking(device: D, queue: Q) -> Result<Self> {
        Self::open_blocking_with_options(device, queue, Options::default())
    }
    /// Explicit blocking recovery with caller-selected runtime bounds.
    pub fn open_blocking_with_options(device: D, queue: Q, options: Options) -> Result<Self> {
        let (mut engine, ticket) = Self::open_with_options(device, queue, options)?;
        engine.wait_lifecycle(ticket)?;
        Ok(engine)
    }
    /// Current initialization/shutdown state.
    pub fn state(&self) -> State {
        self.state
    }
    /// Persisted geometry, available once recovery has succeeded.
    pub fn layout(&self) -> Result<Layout> {
        self.check_readable()?;
        Ok(self.layout.expect("recovered layout"))
    }
    /// Current writable segment, if any.
    pub fn active_segment(&self) -> Option<u32> {
        self.active
    }
    /// Allocated slots discovered or allocated so far.
    pub fn allocated_segments(&self) -> usize {
        self.allocated as usize
    }
    /// Preallocates index buckets within the configured logical entry bound.
    pub fn reserve_index(&mut self, additional: usize) -> Result<()> {
        self.check_readable()?;
        if self.pipeline.index_len().saturating_add(additional) > self.options.resources.index_entries {
            return Err(pipeline::Error::ResourceLimit("index reservation").into());
        }
        Ok(self.pipeline.reserve_index(additional)?)
    }
    /// Indexed versions including tombstones; unavailable before recovery completes.
    pub fn indexed_versions(&self) -> Result<usize> {
        self.check_readable()?;
        Ok(self.pipeline.index_len())
    }
    /// Operations awaiting completion, including an internal automatic transition.
    pub fn in_flight(&self) -> usize {
        self.pipeline.in_flight() + usize::from(self.job.is_some())
    }
    /// Whether a published live record exists.
    pub fn contains(&self, key: &ChunkId) -> Result<bool> {
        self.check_readable()?;
        Ok(self.pipeline.contains(key))
    }
    /// Published LSN and length; absence is distinct from not-ready.
    pub fn stat(&self, key: &ChunkId) -> Result<Option<(u64, u32)>> {
        self.check_readable()?;
        Ok(self.pipeline.stat(key))
    }
    /// Synchronously visits all versions. Use the cursor API to bound owner work.
    pub fn visit_versions(&self, visit: impl FnMut(ChunkId, u64, Option<u32>)) -> Result<()> {
        self.check_readable()?;
        self.pipeline.visit_versions(visit);
        Ok(())
    }
    /// Captures the traversal boundary without copying the index.
    pub fn version_cursor(&self) -> Result<VersionCursor> {
        self.check_readable()?;
        Ok(VersionCursor {
            owner: self.identity,
            next: 0,
            end: self.pipeline.key_count(),
        })
    }
    /// Visits at most `limit` keys; returns true at the captured end. The cursor
    /// must belong to this engine and does not prevent concurrent overwrites.
    pub fn visit_versions_batch(
        &self,
        cursor: &mut VersionCursor,
        limit: usize,
        mut visit: impl FnMut(ChunkId, u64, Option<u32>),
    ) -> Result<bool> {
        self.check_readable()?;
        if cursor.owner != self.identity || limit == 0 {
            return Err(Error::InvalidArgument("invalid version cursor or batch limit"));
        }
        let end = cursor.next.saturating_add(limit).min(cursor.end);
        self.pipeline.visit_batch(cursor.next, end, &mut visit);
        cursor.next = end;
        Ok(end == cursor.end)
    }
    /// Runs one bounded progress turn. A nonblocking Queue is required for
    /// nonblocking disk I/O; synchronous FileQueue remains an explicit backend.
    pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize> {
        let before = out.len();
        let progressed = self.drive_job(out);
        self.pipeline.poll(
            wait && !progressed && out.len() == before && !self.has_ready(),
            &mut self.completions,
        )?;
        out.extend(self.completions.drain(..).map(Completion::from));
        if self.pipeline.queue_failed() && self.state != State::Closed {
            self.state = State::Failed;
        }
        self.drive_job(out);
        Ok(out.len() - before)
    }
    /// Local work remains runnable; poll again before waiting on a descriptor.
    pub fn has_ready(&self) -> bool {
        self.pipeline.has_ready() || self.job.as_ref().is_some_and(|job| !job.waiting && !job.blocked)
    }
    /// Optional queue readiness descriptor; available with notification-enabled backends.
    #[cfg(unix)]
    pub fn notification_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.pipeline.notification_fd()
    }
    /// Minimum caller-buffer sizes for a published record and verification mode.
    pub fn read_requirements(&self, key: ChunkId, range: Range<u32>, verify: bool) -> Result<ReadRequirements> {
        self.check_readable()?;
        Ok(self.pipeline.read_requirements(key, range, verify)?)
    }
    /// Reads a published snapshot. Reads can continue during sealing/rollover.
    pub fn read(
        &mut self,
        key: ChunkId,
        range: Range<u32>,
        verify: bool,
        buffers: ReadBuffers,
    ) -> std::result::Result<Ticket, Rejected<ReadBuffers>> {
        if let Err(error) = self.check_readable() {
            return Err(Rejected { error, input: buffers });
        }
        self.pipeline.read(key, range, verify, buffers).map_err(|r| Rejected {
            error: r.error.into(),
            input: r.input,
        })
    }
    /// Enqueues a persistence barrier. Retry after an active lifecycle transition.
    /// With sync disabled, completion is a write fence without a persistence barrier.
    pub fn flush(&mut self) -> Result<Ticket> {
        self.check_writable()?;
        Ok(self.pipeline.flush()?)
    }
    /// Encodes and accepts a frame. On segment pressure, starts asynchronous
    /// rollover and returns Backpressure with the original buffer for retry.
    pub fn write(
        &mut self,
        frame: &FrameBuilder<'_>,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        self.submit(buffer.into(), |p, b| p.write(frame, b))
    }
    /// Accepts a prepared value. Checksum work remains synchronous and is bounded
    /// by the frame limit; rejected buffers retain their prepared payload.
    pub fn write_prepared(
        &mut self,
        key: ChunkId,
        lsn: u64,
        len: u32,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        self.submit(buffer.into(), |p, b| p.write_prepared(key, lsn, len, b))
    }
    fn submit(
        &mut self,
        buffer: Buffer,
        submit: impl FnOnce(&mut Pipeline<Q>, Buffer) -> std::result::Result<Ticket, pipeline::Rejected<Buffer>>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        if let Err(error) = self.check_writable() {
            return Err(Rejected { error, input: buffer });
        }
        if self.active.is_none() {
            if self.pipeline.writes_pending() {
                return Err(Rejected {
                    error: pipeline::Error::Backpressure.into(),
                    input: buffer,
                });
            }
            let error = self
                .start_rollover(None, None)
                .err()
                .unwrap_or_else(|| pipeline::Error::Backpressure.into());
            return Err(Rejected { error, input: buffer });
        }
        submit(&mut self.pipeline, buffer).map_err(|r| {
            let full = matches!(
                r.error,
                pipeline::Error::Segment(segment::Error::Full { .. }) | pipeline::Error::MetadataFull
            );
            let error = if full && self.pipeline.data_end().is_some_and(|end| end > PAGE_SIZE as u32) {
                self.start_rollover(None, None)
                    .err()
                    .unwrap_or_else(|| pipeline::Error::Backpressure.into())
            } else {
                r.error.into()
            };
            Rejected { error, input: r.input }
        })
    }
    fn check_readable(&self) -> Result<()> {
        if !self.opened {
            return Err(if self.state == State::Opening {
                Error::NotReady
            } else {
                Error::Failed
            });
        }
        if matches!(self.state, State::Closing | State::Closed) {
            return Err(Error::Closed);
        }
        Ok(())
    }
    fn check_writable(&self) -> Result<()> {
        self.check_readable()?;
        if self.state == State::Failed {
            return Err(Error::Failed);
        }
        if self.job.is_some() {
            return Err(pipeline::Error::Backpressure.into());
        }
        Ok(())
    }
    /// Asynchronously seals and selects the next unused segment.
    pub fn rollover(&mut self) -> Result<Ticket> {
        self.check_writable()?;
        let ticket = self.pipeline.next_ticket()?;
        self.start_rollover(None, Some(ticket))?;
        Ok(ticket)
    }
    /// Asynchronously selects a caller-chosen unused slot; no reuse is authorized.
    pub fn rollover_to(&mut self, number: u32) -> Result<Ticket> {
        self.check_writable()?;
        let ticket = self.pipeline.next_ticket()?;
        self.start_rollover(Some(number), Some(ticket))?;
        Ok(ticket)
    }
    fn start_rollover(&mut self, number: Option<u32>, ticket: Option<Ticket>) -> Result<()> {
        let layout = self.layout.expect("ready layout");
        let number = match number {
            Some(n) => n,
            None => {
                while self.next < layout.segment_count() && self.used[self.next as usize] {
                    self.next += 1;
                }
                if self.next == layout.segment_count() {
                    return Err(Error::OutOfSpace);
                }
                self.next
            }
        };
        layout.segment_base(number)?;
        if self.used[number as usize] {
            return Err(Error::InvalidArgument("segment is already allocated"));
        }
        self.pipeline.maintenance(true);
        self.job = Some(Job::transition(ticket, Lifecycle::Rollover, Some(number)));
        Ok(())
    }
    /// Asynchronously seals after preceding writes; existing reads may continue.
    pub fn seal(&mut self) -> Result<Ticket> {
        self.check_writable()?;
        let ticket = self.pipeline.next_ticket()?;
        self.pipeline.maintenance(true);
        self.job = Some(Job::transition(Some(ticket), Lifecycle::Seal, None));
        Ok(ticket)
    }
    /// Stops admission and asynchronously drains and seals, syncing if enabled. Dropping
    /// before completion may block in the queue's buffer-safety fallback.
    pub fn close(&mut self) -> Result<Ticket> {
        if matches!(self.state, State::Opening | State::Closing | State::Closed) || self.job.is_some() {
            return Err(if self.state == State::Closed {
                Error::Closed
            } else {
                pipeline::Error::Backpressure.into()
            });
        }
        let ticket = self.pipeline.next_ticket()?;
        self.state = State::Closing;
        self.pipeline.maintenance(true);
        self.job = Some(Job::transition(Some(ticket), Lifecycle::Close, None));
        Ok(ticket)
    }
    /// Explicit blocking seal helper. Requires no outstanding operations so it
    /// cannot consume another caller's completion.
    pub fn seal_blocking(&mut self) -> Result<()> {
        self.blocking_transition(|e| e.seal())
    }
    /// Explicit blocking rollover helper for tools.
    pub fn rollover_blocking(&mut self) -> Result<()> {
        self.blocking_transition(|e| e.rollover())
    }
    /// Explicit blocking selection helper for tools.
    pub fn rollover_to_blocking(&mut self, number: u32) -> Result<()> {
        self.blocking_transition(|e| e.rollover_to(number))
    }
    /// Explicit blocking orderly shutdown helper for tools.
    pub fn close_blocking(&mut self) -> Result<()> {
        self.blocking_transition(|e| e.close())
    }
    fn blocking_transition(&mut self, start: impl FnOnce(&mut Self) -> Result<Ticket>) -> Result<()> {
        if self.in_flight() != 0 {
            return Err(pipeline::Error::Backpressure.into());
        }
        let ticket = start(self)?;
        self.wait_lifecycle(ticket)
    }
    fn wait_lifecycle(&mut self, ticket: Ticket) -> Result<()> {
        let mut out = Vec::new();
        loop {
            self.poll(true, &mut out)?;
            for completion in out.drain(..) {
                if let Completion::Lifecycle {
                    ticket: got, result, ..
                } = completion
                    && got == ticket
                {
                    return result;
                }
            }
        }
    }
    fn drive_job(&mut self, out: &mut Vec<Completion>) -> bool {
        let Some(mut job) = self.job.take() else { return false };
        if self.pipeline.queue_failed() && job.kind != Lifecycle::Close {
            return self.finish_job(job, Err(pipeline::Error::QueueFailed.into()), out);
        }
        let input = if job.waiting {
            if self.pipeline.queue_failed() {
                return self.finish_job(job, Err(pipeline::Error::QueueFailed.into()), out);
            }
            let Some(completion) = self.pipeline.take_control() else {
                self.job = Some(job);
                return false;
            };
            job.waiting = false;
            match completion.result {
                Ok(actual) if actual == completion.request.len => completion.request.buffer,
                Ok(actual) => {
                    return self.finish_job(
                        job,
                        Err(pipeline::Error::ShortIo {
                            operation: completion.request.operation,
                            offset: completion.request.offset,
                            expected: completion.request.len,
                            actual,
                        }
                        .into()),
                        out,
                    );
                }
                Err(source) => {
                    return self.finish_job(
                        job,
                        Err(pipeline::Error::Io {
                            operation: completion.request.operation,
                            offset: completion.request.offset,
                            source,
                        }
                        .into()),
                        out,
                    );
                }
            }
        } else {
            None
        };
        job.blocked = false;
        match job.step(self, input) {
            Ok(Progress::Done) => self.finish_job(job, Ok(()), out),
            Ok(Progress::Continue) => {
                self.job = Some(job);
                true
            }
            Ok(Progress::Wait) => {
                job.blocked = true;
                self.job = Some(job);
                false
            }
            Ok(Progress::Io {
                operation,
                offset,
                len,
                buffer,
            }) => match self.pipeline.control_io(operation, offset, len, buffer) {
                Ok(()) => {
                    job.waiting = true;
                    self.job = Some(job);
                    true
                }
                Err(error) => self.finish_job(job, Err(error.into()), out),
            },
            Err(error) => self.finish_job(job, Err(error), out),
        }
    }
    fn finish_job(&mut self, job: Job, result: Result<()>, out: &mut Vec<Completion>) -> bool {
        if job.kind == Lifecycle::Close {
            self.state = State::Closed;
        } else if result.is_err() {
            self.state = State::Failed;
        } else {
            self.state = State::Ready;
            if job.kind == Lifecycle::Open {
                self.opened = true;
            }
        }
        self.pipeline.maintenance(false);
        if let Some(ticket) = job.ticket {
            out.push(Completion::Lifecycle {
                ticket,
                operation: job.kind,
                result,
            });
        }
        true
    }
}
