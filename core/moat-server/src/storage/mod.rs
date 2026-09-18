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

//! Transitional application boundary over the engine.
//!
//! Disk handles contain configuration and an ownership lease, never a shared
//! engine. A Session owns one engine, index, queue and pool on its caller thread.
//! All persistence and recovery use the engine; there is no legacy-format reader or GC.

mod device;
mod queue;

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

pub use device::{Device, FileDevice, MemDevice};
use moat_common::{BufferPool, ChunkId, PoolOptions, PooledBuf};
use moat_engine::{
    engine,
    frame::{FrameBuilder, PreparedFrame},
    pipeline::{self, ReadBuffers, Ticket},
};
pub use moat_engine::{
    engine::{FormatOptions, Layout},
    frame::FrameLimits,
    pipeline::{Completion, ReadRange},
};
pub use queue::QueueBackend;

/// Application storage failures, preserving the typed engine cause.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No buffer or queue slot is available, or another session owns the disk.
    #[error("storage backpressure")]
    Busy,
    /// Invalid application configuration.
    #[error("invalid storage configuration: {0}")]
    Invalid(&'static str),
    /// A capability is not implemented by the engine.
    #[error("unsupported storage operation: {0}")]
    Unsupported(&'static str),
    /// Cold I/O or queue initialization failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The engine failed.
    #[error(transparent)]
    Engine(#[from] engine::Error),
}
impl From<pipeline::Error> for Error {
    fn from(error: pipeline::Error) -> Self {
        Self::Engine(error.into())
    }
}
impl From<moat_engine::frame::Error> for Error {
    fn from(error: moat_engine::frame::Error) -> Self {
        Self::Engine(engine::Error::Pipeline(error.into()))
    }
}
/// Storage result.
pub type Result<T> = std::result::Result<T, Error>;

/// Runtime settings independent of persisted geometry.
#[derive(Debug, Clone)]
pub struct Options {
    /// Explicit device-sync policy; disabled flushes still drain preceding writes.
    pub sync_mode: engine::SyncMode,
    /// Initial index reservation and default upper-layer live-entry limit.
    /// Also raises the native key budget above its default when larger.
    /// The native budget includes tombstones and pending writes.
    pub index_capacity: usize,
    /// Validate metadata and payload checksums on reads.
    pub verify_reads: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            sync_mode: engine::SyncMode::Enabled,
            index_capacity: 1024,
            verify_reads: false,
        }
    }
}
/// Per-device queue and registered pool configuration.
#[derive(Debug, Clone)]
pub struct QueueOptions {
    /// Maximum in-flight physical requests.
    pub depth: usize,
    /// Pool owned by the same thread as the queue.
    pub pool: PoolOptions,
}
impl Default for QueueOptions {
    fn default() -> Self {
        Self {
            depth: 64,
            pool: PoolOptions::default(),
        }
    }
}
/// Physical append-only allocation snapshot.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    /// Total segment slots.
    pub segments: u32,
    /// Never-allocated slots; deletion cannot increase this count.
    pub free_segments: u32,
}

struct Shared {
    device: Arc<dyn Device>,
    layout: Layout,
    options: Options,
    owned: AtomicBool,
    allocated: AtomicU32,
}
/// Cloneable configuration handle. Only one Session may own a handle's device.
/// Callers must also prevent opening the same physical device through separate handles.
#[derive(Clone)]
pub struct Disk(Arc<Shared>);
impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disk").field("layout", &self.0.layout).finish()
    }
}
impl Disk {
    /// Reads engine geometry; index recovery runs on the eventual owner thread.
    pub fn open(device: Arc<dyn Device>, options: Options) -> Result<Self> {
        if options.index_capacity == 0 {
            return Err(Error::Invalid("index capacity must be nonzero"));
        }
        let layout = Layout::read(&queue::DeviceRef(&*device))?;
        Ok(Self(Arc::new(Shared {
            device,
            layout,
            options,
            owned: AtomicBool::new(false),
            allocated: AtomicU32::new(0),
        })))
    }
    /// Persisted device layout.
    pub fn layout(&self) -> Layout {
        self.0.layout
    }
    /// Default upper-layer live-entry limit.
    pub fn index_capacity(&self) -> usize {
        self.0.options.index_capacity
    }
    /// Last owner-published allocation snapshot, initialized by Session recovery.
    pub fn usage(&self) -> Usage {
        Usage {
            segments: self.layout().segment_count(),
            free_segments: self
                .layout()
                .segment_count()
                .saturating_sub(self.0.allocated.load(Ordering::Acquire)),
        }
    }
    /// Conservative bytes for one frame, its footer metadata and allocation headers.
    pub fn write_cost(&self, len: u32) -> Result<u64> {
        let frame = PreparedFrame::required_len(self.layout().limits(), len)? as u64;
        Ok(frame.saturating_mul(2).saturating_add(3 * moat_common::PAGE_SIZE))
    }
}
/// Formats only in the engine encoding. The identity must be fresh and nonzero.
pub fn format(device: &dyn Device, options: &FormatOptions) -> Result<()> {
    engine::format(&queue::DeviceRef(device), *options)?;
    Ok(())
}

struct Lease(Disk);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.0.owned.store(false, Ordering::Release);
    }
}

/// One exclusive owner. Construct, poll and drop it on the owning thread.
pub struct Session {
    engine: engine::Engine<queue::OwnedDevice, queue::DeviceQueue>,
    pool: Arc<BufferPool>,
    next_lsn: u64,
    completions: Vec<engine::Completion>,
    // Drop the engine (including its queue) before releasing ownership.
    lease: Lease,
    // Pools are thread-owned even with the synchronous queue.
    _owner: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Session {
    /// Acquires ownership, initializes buffers and recovers the engine index.
    pub fn open(disk: Disk, options: &QueueOptions, backend: QueueBackend) -> Result<Self> {
        disk.0
            .owned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let lease = Lease(disk);
        let disk = &lease.0;
        if disk.layout().limits().max_frame_len() as usize > options.pool.max_class {
            return Err(Error::Invalid(
                "pool maximum class must cover the persisted frame limit",
            ));
        }
        let pool = BufferPool::new(options.pool)?;
        let queue = queue::DeviceQueue::new(disk.0.device.clone(), options.depth, backend, pool.clone())?;
        let mut runtime = engine::Options {
            sync_mode: disk.0.options.sync_mode,
            ..Default::default()
        };
        runtime.resources.index_entries = runtime.resources.index_entries.max(disk.index_capacity());
        let mut engine =
            engine::Engine::open_blocking_with_options(queue::OwnedDevice(disk.0.device.clone()), queue, runtime)?;
        if engine.layout()? != disk.layout() {
            return Err(Error::Invalid("device geometry changed before ownership"));
        }
        engine.reserve_index(disk.index_capacity().saturating_sub(engine.indexed_versions()?))?;
        let mut max_lsn = 0;
        engine.visit_versions(|_, lsn, _| max_lsn = max_lsn.max(lsn))?;
        let next_lsn = max_lsn.checked_add(1).ok_or(Error::Invalid("LSN space exhausted"))?;
        let session = Self {
            engine,
            pool,
            next_lsn,
            completions: Vec::with_capacity(options.depth),
            lease,
            _owner: std::marker::PhantomData,
        };
        session.publish_usage();
        Ok(session)
    }
    fn publish_usage(&self) {
        self.lease
            .0
            .0
            .allocated
            .store(self.engine.allocated_segments() as u32, Ordering::Release);
    }
    /// Pool backing owned by this session.
    pub fn pool(&self) -> &BufferPool {
        &self.pool
    }
    /// Published record version and length.
    pub fn stat(&self, id: &ChunkId) -> Option<(u64, u32)> {
        self.engine.stat(id).ok().flatten()
    }
    /// Visits live published records without copying the index.
    pub fn visit(&self, mut visit: impl FnMut(ChunkId, u64, u32)) {
        self.engine
            .visit_versions(|id, lsn, len| {
                if let Some(len) = len {
                    visit(id, lsn, len);
                }
            })
            .expect("opened session");
    }
    /// Outstanding operations, including undelivered completions.
    pub fn in_flight(&self) -> usize {
        self.engine.in_flight()
    }
    /// Drives the owner's queue, preserving native completions and buffers.
    pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize> {
        let result = self.engine.poll(wait, &mut self.completions).map_err(Error::from);
        let before = out.len();
        out.extend(self.completions.drain(..).filter_map(engine::Completion::into_pipeline));
        self.publish_usage();
        result.map(|_| out.len() - before)
    }
    /// Appends a data record or tombstone and returns its ticket and assigned LSN.
    /// The caller serializes conditional mutations of the same key.
    pub fn write(&mut self, id: ChunkId, value: Option<&[u8]>) -> Result<(Ticket, u64)> {
        let lsn = self.next_lsn;
        let next = lsn.checked_add(1).ok_or(Error::Invalid("LSN space exhausted"))?;
        let limits: FrameLimits = self.engine.layout()?.limits();
        let result = if let Some(value) = value.filter(|value| value.len() >= 65536) {
            let len = u32::try_from(value.len()).map_err(|_| Error::Invalid("value length exceeds u32"))?;
            let mut buffer = self
                .pool
                .alloc(PreparedFrame::required_len(limits, len)?)
                .ok_or(Error::Busy)?;
            PreparedFrame::new(limits, len, &mut buffer)?
                .value_mut()
                .copy_from_slice(value);
            self.engine.write_prepared(id, lsn, len, buffer)
        } else {
            let mut frame = FrameBuilder::new(limits);
            if let Some(value) = value {
                frame.push(id, lsn, value)?;
            } else {
                frame.push_tombstone(id, lsn)?;
            }
            let buffer = self.pool.alloc(frame.encoded_len()).ok_or(Error::Busy)?;
            self.engine.write(&frame, buffer)
        };
        self.publish_usage();
        match result {
            Ok(ticket) => {
                self.next_lsn = next;
                Ok((ticket, lsn))
            }
            Err(rejected) => Err(map_error(rejected.error)),
        }
    }
    /// Reads a published record; the adapter clamps ranges to its logical length.
    pub fn read(&mut self, id: ChunkId, range: Option<Range<u64>>) -> Result<Ticket> {
        let (_, len) = self.stat(&id).ok_or(pipeline::Error::NotFound)?;
        let range = range.unwrap_or(0..len as u64);
        if range.start > range.end {
            return Err(Error::Invalid("reversed read range"));
        }
        let range = range.start.min(len as u64) as u32..range.end.min(len as u64) as u32;
        let verify = self.lease.0.0.options.verify_reads;
        let needs = self.engine.read_requirements(id, range.clone(), verify)?;
        let value = self.pool.alloc(needs.value_len.max(4096)).ok_or(Error::Busy)?;
        let metadata = if verify {
            Some(self.pool.alloc(needs.metadata_len.max(4096)).ok_or(Error::Busy)?.into())
        } else {
            None
        };
        self.engine
            .read(
                id,
                range,
                verify,
                ReadBuffers {
                    value: value.into(),
                    metadata,
                },
            )
            .map_err(|rejected| map_error(rejected.error))
    }
    /// Orders a durable barrier after earlier writes.
    pub fn flush(&mut self) -> Result<Ticket> {
        self.engine.flush().map_err(map_error)
    }
    /// Seals after all completions have been delivered. This is a cold, blocking operation.
    pub fn seal(&mut self) -> Result<()> {
        self.engine.seal_blocking().map_err(map_error)
    }
}
fn map_error(error: engine::Error) -> Error {
    match error {
        engine::Error::Pipeline(pipeline::Error::Backpressure) => Error::Busy,
        error => error.into(),
    }
}
/// Keeps only the pool allocation containing the returned range; no payload copy.
pub fn read_buffer(buffers: ReadBuffers, range: ReadRange) -> (PooledBuf, Range<usize>) {
    let (buffer, range) = match range {
        ReadRange::Value(range) => (buffers.value, range),
        ReadRange::Metadata(range) => (buffers.metadata.expect("verified metadata"), range),
    };
    let moat_engine::io::Buffer::Pooled(buffer) = buffer else {
        unreachable!("Session only submits pooled buffers")
    };
    (buffer, range)
}
