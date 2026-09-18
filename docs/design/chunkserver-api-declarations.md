# Current chunkserver public declarations

Reviewed against the working source on 2026-09-17. See the
[API review guide](chunkserver-api-inventory.md) for layer boundaries, defaults,
ownership, completion semantics, and missing capabilities.

This appendix was extracted from Rust syntax trees for `moat-server`,
`moat-engine`, `moat-cache-store`, and `moat-common`. It covers declared public
functions, inherent methods, traits, types, public fields, variants, constants,
module exports, reexports, and explicit trait implementations on public types.
Derived traits are shown by their attributes; generated trait methods are not
expanded. Internal types and restricted-visibility methods are omitted.

Code blocks are declaration sketches for review, not compilable Rust modules:
implementation bodies are replaced by semicolons and private fields are omitted.
Names and imports resolve in the linked source file. Source-file headings are
not necessarily public import paths: private implementation modules expose their
types through the reexports shown here. Platform alternatives are included;
`engine` and `FileQueue` require Unix, while `UringQueue` requires Linux. Parent
module gating still applies even when not repeated on a declaration.

The extraction includes 335 function/method declarations, counting explicit
trait implementations and platform alternatives separately. This is not a count
of distinct user operations. These declarations are a point-in-time snapshot;
source code remains authoritative after subsequent changes.

## `moat-server` declaration inventory

### `core/moat-server/src/disk.rs`

[Source](../../core/moat-server/src/disk.rs)

```rust
/// One NVMe namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeDisk {
    /// Kernel block device name, e.g. `nvme3n1`.
    pub name: String,
    /// Device node, e.g. `/dev/nvme3n1`.
    pub path: PathBuf,
    /// Controller serial number (trimmed).
    pub serial: String,
    /// Controller model (trimmed).
    pub model: String,
    /// Capacity in bytes.
    pub capacity: u64,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// NUMA node of the controller, if known.
    pub numa_node: Option<usize>,
    /// Why the disk is unavailable as a data disk, if it is.
    pub in_use: Option<InUse>,
}

/// What claims a disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InUse {
    /// The namespace is partitioned.
    Partitioned,
    /// A stacked device (md, dm) sits on top of it.
    Holder(String),
    /// A filesystem is mounted from it.
    Mounted(String),
    /// It is an active swap device.
    Swap,
}

impl NvmeDisk {
    /// Whether the disk may be used (and formatted) as a data disk.
    pub fn is_available(&self) -> bool;
}

/// Lists every NVMe namespace on the machine, in name order.
#[cfg(target_os = "linux")]
pub fn discover() -> io::Result<Vec<NvmeDisk>>;

/// NVMe discovery is Linux-only; open regular files explicitly on other platforms.
#[cfg(not(target_os = "linux"))]
pub fn discover() -> io::Result<Vec<NvmeDisk>>;

/// The CPUs of NUMA node `node`, from sysfs. Empty if unknown.
pub fn cpus_of_node(node: usize) -> Vec<usize>;

/// The CPUs this process may run on.
#[cfg(target_os = "linux")]
pub fn online_cpus() -> Vec<usize>;

/// Logical worker indices based on available parallelism; not affinity IDs.
#[cfg(not(target_os = "linux"))]
pub fn online_cpus() -> Vec<usize>;

/// Parses a kernel CPU list such as `0-3,8,10-11`.
pub fn parse_cpu_list(s: &str) -> Vec<usize>;
```

### `core/moat-server/src/lib.rs`

[Source](../../core/moat-server/src/lib.rs)

```rust
pub mod disk;

pub mod node;

pub mod placement;

pub mod storage;

pub mod worker;

pub use node::{Node, NodeError};

pub use placement::{Placement, Target};

pub use worker::{
    Context, DiskId, DiskSlot, Handler, PollMode, QueueBackend, Step, Worker, WorkerError, WorkerOptions,
};
```

### `core/moat-server/src/node.rs`

[Source](../../core/moat-server/src/node.rs)

```rust
/// Errors from assembling a node.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// A disk failed to open.
    #[error("disk {disk}: {source}")]
    Open {
        /// Index of the disk in the list passed to `open`.
        disk: DiskId,
        /// The cause.
        #[source]
        source: crate::storage::Error,
    },
    /// A worker failed to start.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// Two disks have the same persistent placement identity.
    #[error("disks {first} and {second} have the same UUID")]
    DuplicateIdentity {
        /// First disk using the identity.
        first: DiskId,
        /// Second disk using the identity.
        second: DiskId,
    },
    /// The node has no disks.
    #[error("no disks")]
    NoDisks,
    /// An assigned owner does not exist in the worker list.
    #[error("disk owner is outside the worker list")]
    InvalidOwners,
}

/// The opened disks of a machine.
pub struct Node {
    // Private fields omitted.
}

impl Node {
    /// Validates device geometry. Use [`Self::assign_owners`] before starting
    /// workers, which recover indexes on their owner threads.
    pub fn open(devices: Vec<Arc<dyn Device>>, options: Options) -> Result<Self, NodeError>;
    /// Device handles, indexed by [`DiskId`]; no shared engine state.
    pub fn engines(&self) -> &[Disk];
    /// The worker assigned exclusive ownership of `disk`.
    pub fn owner_of(&self, disk: DiskId) -> usize;
    /// The owner of every disk, indexed by [`DiskId`].
    pub fn owners(&self) -> &[usize];
    /// The disk `id` is placed on.
    pub fn disk_of(&self, id: &ChunkId) -> DiskId;
    /// The placement over this node's disks.
    pub fn placement(&self) -> &Placement;
    /// Assigns each disk to one of `workers` workers, spreading disks evenly
    /// and preferring a worker on the disk's NUMA node when both `disk_numa`
    /// (per disk) and `worker_numa` (per worker) are known.
    pub fn assign_owners(&mut self, workers: usize, disk_numa: &[Option<usize>], worker_numa: &[Option<usize>]);
    /// Sets the owner of every disk explicitly.
    pub fn set_owners(&mut self, owners: Vec<usize>);
    /// Starts and recovers the assigned disks on each owner worker. Handlers
    /// see only their own disks; callers must route cross-owner requests.
    pub fn start<H: Handler>(
        &self,
        workers: &[WorkerOptions],
        mut make: impl FnMut(usize) -> H,
    ) -> Result<Vec<Worker<H>>, NodeError>;
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}
```

### `core/moat-server/src/placement.rs`

[Source](../../core/moat-server/src/placement.rs)

```rust
/// A disk that can hold chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The disk's identity (its superblock uuid).
    pub uuid: [u8; 16],
    /// Relative weight, typically the capacity in bytes. Zero excludes the
    /// disk.
    pub weight: u64,
}

/// A fixed set of disks to place chunks on.
#[derive(Debug, Clone)]
pub struct Placement {
    // Private fields omitted.
}

impl Placement {
    /// Builds a placement over `targets`; the index into this list is what
    /// [`Placement::disk_of`] returns.
    pub fn new(targets: Vec<Target>) -> Self;
    /// The targets, in index order.
    pub fn targets(&self) -> &[Target];
    /// Number of targets.
    pub fn len(&self) -> usize;
    /// Whether there are no targets.
    pub fn is_empty(&self) -> bool;
    /// The disk `id` belongs on, or `None` if every target has zero weight.
    pub fn disk_of(&self, id: &ChunkId) -> Option<usize>;
}
```

### `core/moat-server/src/storage/device.rs`

[Source](../../core/moat-server/src/storage/device.rs)

```rust
/// A block device.
pub trait Device: Send + Sync + 'static {
    fn capacity(&self) -> u64;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    fn fd(&self) -> Option<BorrowedFd<'_>>;
}

/// A device backed by a regular file or a block device node.
pub struct FileDevice {
    // Private fields omitted.
}

impl FileDevice {
    /// Opens an existing file or block device.
    ///
    /// With `direct` set the file is opened with `O_DIRECT`, bypassing the page
    /// cache. This requires every buffer to be page aligned in memory.
    /// Outside Linux, `direct = true` returns `Unsupported`; use buffered
    /// regular files (`direct = false`) for local development.
    pub fn open(path: impl AsRef<Path>, direct: bool) -> io::Result<Self>;
    /// Creates (or truncates) a regular file of `len` bytes and opens it.
    pub fn create(path: impl AsRef<Path>, len: u64, direct: bool) -> io::Result<Self>;
}

impl Device for FileDevice {
    fn capacity(&self) -> u64;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    fn fd(&self) -> Option<BorrowedFd<'_>>;
}

/// An in-memory device for tests.
///
/// Besides the [`Device`] interface it exposes the raw bytes so tests can
/// simulate torn writes, bit rot and truncated tails between an engine
/// shutdown and the next open, plus a knob to fail writes in a byte range.
/// It has no file descriptor, so it is driven through a
/// synchronous queue.
pub struct MemDevice {
    // Private fields omitted.
}

impl MemDevice {
    /// Creates a zero-filled device of `len` bytes.
    pub fn new(len: u64) -> Self;
    /// Runs `f` with mutable access to the raw device contents.
    pub fn with_data_mut<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R;
    /// Runs `f` with read access to the raw device contents.
    pub fn with_data<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R;
    /// Makes every write that overlaps `range` fail with `EIO` (`None` clears).
    pub fn fail_writes_in(&self, range: Option<Range<u64>>);
}

impl Device for MemDevice {
    fn capacity(&self) -> u64;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    fn fd(&self) -> Option<BorrowedFd<'_>>;
}
```

### `core/moat-server/src/storage/mod.rs`

[Source](../../core/moat-server/src/storage/mod.rs)

```rust
pub use device::{Device, FileDevice, MemDevice};

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
    fn from(error: pipeline::Error) -> Self;
}

impl From<moat_engine::frame::Error> for Error {
    fn from(error: moat_engine::frame::Error) -> Self;
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
    fn default() -> Self;
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
    fn default() -> Self;
}

/// Physical append-only allocation snapshot.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    /// Total segment slots.
    pub segments: u32,
    /// Never-allocated slots; deletion cannot increase this count.
    pub free_segments: u32,
}

/// Cloneable configuration handle. Only one Session may own a handle's device.
/// Callers must also prevent opening the same physical device through separate handles.
#[derive(Clone)]
pub struct Disk(/* private fields */);

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

impl Disk {
    /// Reads engine geometry; index recovery runs on the eventual owner thread.
    pub fn open(device: Arc<dyn Device>, options: Options) -> Result<Self>;
    /// Persisted device layout.
    pub fn layout(&self) -> Layout;
    /// Default upper-layer live-entry limit.
    pub fn index_capacity(&self) -> usize;
    /// Last owner-published allocation snapshot, initialized by Session recovery.
    pub fn usage(&self) -> Usage;
    /// Conservative bytes for one frame, its footer metadata and allocation headers.
    pub fn write_cost(&self, len: u32) -> Result<u64>;
}

/// Formats only in the engine encoding. The identity must be fresh and nonzero.
pub fn format(device: &dyn Device, options: &FormatOptions) -> Result<()>;

/// One exclusive owner. Construct, poll and drop it on the owning thread.
pub struct Session {
    // Private fields omitted.
}

impl Session {
    /// Acquires ownership, initializes buffers and recovers the engine index.
    pub fn open(disk: Disk, options: &QueueOptions, backend: QueueBackend) -> Result<Self>;
    /// Pool backing owned by this session.
    pub fn pool(&self) -> &BufferPool;
    /// Published record version and length.
    pub fn stat(&self, id: &ChunkId) -> Option<(u64, u32)>;
    /// Visits live published records without copying the index.
    pub fn visit(&self, mut visit: impl FnMut(ChunkId, u64, u32));
    /// Outstanding operations, including undelivered completions.
    pub fn in_flight(&self) -> usize;
    /// Drives the owner's queue, preserving native completions and buffers.
    pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize>;
    /// Appends a data record or tombstone and returns its ticket and assigned LSN.
    /// The caller serializes conditional mutations of the same key.
    pub fn write(&mut self, id: ChunkId, value: Option<&[u8]>) -> Result<(Ticket, u64)>;
    /// Reads a published record; the adapter clamps ranges to its logical length.
    pub fn read(&mut self, id: ChunkId, range: Option<Range<u64>>) -> Result<Ticket>;
    /// Orders a durable barrier after earlier writes.
    pub fn flush(&mut self) -> Result<Ticket>;
    /// Seals after all completions have been delivered. This is a cold, blocking operation.
    pub fn seal(&mut self) -> Result<()>;
}

/// Keeps only the pool allocation containing the returned range; no payload copy.
pub fn read_buffer(buffers: ReadBuffers, range: ReadRange) -> (PooledBuf, Range<usize>);
```

### `core/moat-server/src/storage/queue.rs`

[Source](../../core/moat-server/src/storage/queue.rs)

```rust
/// Queue selection for an owner's device.
#[derive(Debug, Clone, Copy, Default)]
pub enum QueueBackend {
    /// io_uring on Linux; synchronous elsewhere.
    #[default]
    Auto,
    /// Synchronous positional I/O, including memory/fault-injection devices.
    Sync,
    /// Registered io_uring on Linux; unsupported elsewhere.
    Uring,
}
```

### `core/moat-server/src/worker.rs`

[Source](../../core/moat-server/src/worker.rs)

```rust
pub use crate::storage::QueueBackend;

/// Index of a disk in a node's disk list.
pub type DiskId = usize;

/// How the worker behaves when it has nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollMode {
    /// Spin: the lowest latency, one core per worker.
    Busy,
    /// Sleep after an idle handler iteration. For deployments that cannot
    /// dedicate cores; polling all disks first avoids waiting on a single disk.
    Adaptive {
        /// How long to sleep when neither I/O nor requests are pending.
        idle_sleep: Duration,
    },
}

/// Configuration of one worker.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// Core to pin the thread to; `None` leaves scheduling to the OS.
    pub core: Option<usize>,
    /// Queue depth and pool allocated separately for each owned disk.
    pub queue: QueueOptions,
    /// Queue implementation.
    pub backend: QueueBackend,
    /// Idle behaviour.
    pub poll_mode: PollMode,
}

impl Default for WorkerOptions {
    fn default() -> Self;
}

/// What a [`Handler::run`] call reports back to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Work was done or is pending; loop again at once.
    Continue,
    /// Nothing to do until I/O completes or new requests arrive.
    Idle,
    /// Shut the worker down after this iteration.
    Stop,
}

/// One disk exclusively owned by this worker.
pub struct DiskSlot {
    /// Stable node-wide disk index.
    pub id: DiskId,
    /// The single-owner engine adapter.
    pub session: Session,
}

/// What a handler sees on each iteration. Only locally owned disks are exposed.
pub struct Context<'a> {
    /// Worker index in the node.
    pub worker: usize,
    /// Owned disks, each with its own engine and queue.
    pub disks: &'a mut [DiskSlot],
    /// Native completions tagged with node-wide disk IDs; drain on each call.
    pub completions: &'a mut Vec<(DiskId, Completion)>,
}

impl Context<'_> {
    /// Whether this worker owns a disk.
    pub fn owns(&self, disk: DiskId) -> bool;
    /// Gets an owned session. Route remote-disk requests to their owner explicitly.
    pub fn disk(&mut self, disk: DiskId) -> Option<&mut Session>;
}

/// The request source of a worker. See the [module docs](self).
pub trait Handler: Send + 'static {
    fn start(&mut self, _cx: &mut Context<'_>); // Default implementation available.
    fn run(&mut self, cx: &mut Context<'_>) -> Step;
    fn stop(&mut self, _cx: &mut Context<'_>); // Default implementation available.
}

/// Errors from a worker thread.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The queue or a pipeline could not be set up, or the queue failed.
    #[error("worker {worker}: {source}")]
    Io {
        /// The worker index.
        worker: usize,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// An engine call failed during setup or shutdown.
    #[error("worker {worker}: {source}")]
    Engine {
        /// The worker index.
        worker: usize,
        /// The cause.
        #[source]
        source: crate::storage::Error,
    },
    /// The handler panicked; the worker thread is gone.
    #[error("worker {0} panicked")]
    Panicked(usize),
}

/// A running worker thread.
pub struct Worker<H: Handler> {
    // Private fields omitted.
}

impl<H: Handler> Worker<H> {
    /// Starts worker `index`, recovers each assigned disk and runs `handler`.
    /// Each disk gets a separate queue and pool. Startup errors are returned
    /// here; runtime and shutdown failures are returned by `join`.
    pub fn spawn(
        index: usize,
        opts: WorkerOptions,
        disks: Vec<(DiskId, Disk)>,
        handler: H,
    ) -> Result<Self, WorkerError>;
    /// The worker's index.
    pub fn index(&self) -> usize;
    /// Asks the worker to stop after its current iteration.
    pub fn stop(&self);
    /// Waits for the worker to finish and returns its handler.
    pub fn join(mut self) -> Result<H, WorkerError>;
}

impl<H: Handler> Drop for Worker<H> {
    fn drop(&mut self);
}

/// Pins the calling thread to `core`.
#[cfg(target_os = "linux")]
pub fn pin_to_core(core: usize) -> io::Result<()>;

/// CPU pinning is unsupported outside Linux; use `WorkerOptions::core = None`.
#[cfg(not(target_os = "linux"))]
pub fn pin_to_core(_core: usize) -> io::Result<()>;
```

## `moat-engine` declaration inventory

### `core/moat-engine/src/engine/completion.rs`

[Source](../../core/moat-engine/src/engine/completion.rs)

```rust
/// Lifecycle operation associated with a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Recover persisted geometry and indexes.
    Open,
    /// Persist and seal the active segment.
    Seal,
    /// Seal and allocate another segment.
    Rollover,
    /// Stop admission, drain, persist, and seal.
    Close,
}

/// Engine operation completion. Buffer ownership follows pipeline completions.
#[derive(Debug)]
pub enum Completion {
    /// A lifecycle request completed; failure does not authorize further writes.
    Lifecycle {
        /// Accepted request identity.
        ticket: Ticket,
        /// Requested transition.
        operation: Lifecycle,
        /// Transition result.
        result: Result<()>,
    },
    /// A frame was published, or failed. Completion alone is not durability.
    Write {
        /// Request identity.
        ticket: Ticket,
        /// Publication result.
        result: pipeline::Result<()>,
        /// Original buffer.
        buffer: Buffer,
    },
    /// A read completed with its owned buffers.
    Read {
        /// Request identity.
        ticket: Ticket,
        /// Result slice location.
        result: pipeline::Result<ReadRange>,
        /// Original buffers.
        buffers: ReadBuffers,
    },
    /// A persistence barrier completed.
    Flush {
        /// Request identity.
        ticket: Ticket,
        /// Persistence result.
        result: pipeline::Result<()>,
    },
    /// A fatal queue failure terminated an accepted operation. Buffers still
    /// accessible by the OS remain with the queue until safe teardown.
    Failed {
        /// Terminated request identity.
        ticket: Ticket,
        /// Shared failure cause.
        error: std::sync::Arc<pipeline::Error>,
    },
}

impl Completion {
    /// Identity scoped to this engine instance.
    pub fn ticket(&self) -> Ticket;
    /// Extracts a data-path completion for adapters that drive lifecycle requests separately.
    pub fn into_pipeline(self) -> Option<pipeline::Completion>;
}

impl From<pipeline::Completion> for Completion {
    fn from(value: pipeline::Completion) -> Self;
}
```

### `core/moat-engine/src/engine/device.rs`

[Source](../../core/moat-engine/src/engine/device.rs)

```rust
/// Positional device operations. The asynchronous queue must address this same
/// device. Implementations must report short I/O as errors and honor `sync`.
pub trait Device {
    fn capacity(&self) -> io::Result<u64>;
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
}

impl Device for File {
    fn capacity(&self) -> io::Result<u64>;
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
}
```

### `core/moat-engine/src/engine/error.rs`

[Source](../../core/moat-engine/src/engine/error.rs)

```rust
/// Device lifecycle and operation failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A shared fatal queue error terminated an accepted operation.
    #[error("engine operation aborted: {0}")]
    Aborted(#[source] std::sync::Arc<pipeline::Error>),
    /// Recovery has not completed; drive the open ticket first.
    #[error("engine is not ready")]
    NotReady,
    /// Shutdown stopped admission or already completed.
    #[error("engine is closing or closed")]
    Closed,
    /// Caller-supplied geometry or segment selection is invalid.
    #[error("invalid device argument: {0}")]
    InvalidArgument(&'static str),
    /// No valid device superblock exists.
    #[error("device is not formatted or both superblocks are damaged")]
    NotFormatted,
    /// Committed metadata is inconsistent.
    #[error("corrupt device metadata: {0}")]
    Corrupt(&'static str),
    /// The persistent device version is not supported.
    #[error("unsupported device version {0}")]
    UnsupportedVersion(u32),
    /// All allocation slots have been used; no implicit reclamation is performed.
    #[error("device has no unused segments")]
    OutOfSpace,
    /// A lifecycle write or persistence barrier failed; reopen before writing.
    #[error("device write lifecycle has failed")]
    Failed,
    /// The requested in-memory index reservation could not be allocated.
    #[error("index reservation failed: {0}")]
    IndexAllocation(#[from] std::collections::TryReserveError),
    /// Positional metadata I/O or a persistence barrier failed.
    #[error("device I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Read/write admission, completion, or verification failed.
    #[error(transparent)]
    Pipeline(#[from] pipeline::Error),
    /// Invalid segment metadata or recovery failure.
    #[error(transparent)]
    Segment(#[from] segment::Error),
    /// Invalid frame limits or recovery failure.
    #[error(transparent)]
    Frame(#[from] frame::Error),
}

/// Device operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// Unaccepted operation with its original input ownership preserved.
pub type Rejected<T> = pipeline::Rejected<T, Error>;
```

### `core/moat-engine/src/engine/layout.rs`

[Source](../../core/moat-engine/src/engine/layout.rs)

```rust
/// Geometry fixed by format and loaded on every reopen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    // Private fields omitted.
}

/// Destructive format parameters. Each format must use a fresh device identity.
#[derive(Debug, Clone, Copy)]
pub struct FormatOptions {
    /// Persistence policy for formatting only; select runtime policy on open too.
    pub sync_mode: super::SyncMode,
    /// Fresh random 128-bit identity supplied by the application.
    /// Required even after an interrupted format to distinguish old frames.
    pub device_id: [u8; 16],
    /// Physical allocation stride, including the allocation header and tail footer.
    pub segment_size: u32,
    /// Persistent frame/value bounds, independent of batching targets.
    pub limits: FrameLimits,
}

impl Layout {
    /// Device identity persisted at format.
    pub fn device_id(self) -> [u8; 16];
    /// Physical capacity recorded at format; trailing partial slots are unused.
    pub fn capacity(self) -> u64;
    /// Physical bytes per allocation, including its header and tail footer.
    pub fn segment_size(self) -> u32;
    /// Number of complete allocation slots.
    pub fn segment_count(self) -> u32;
    /// Format-wide limits loaded from the superblock.
    pub fn limits(self) -> FrameLimits;
    /// Absolute base of one segment, or an error for an out-of-range selection.
    pub fn segment_base(self, number: u32) -> Result<u64>;
    /// Loads matching geometry from either independently checksummed copy.
    pub fn read(device: &impl Device) -> Result<Self>;
}

/// Reinitializes all allocation headers and commits immutable device geometry.
///
/// Destructive: exclusive access is required. This does not erase payloads or
/// discard the device. Epochs prevent old payloads from becoming new frames.
/// An interrupted format requires another format if no superblock was committed.
pub fn format(device: &impl Device, options: FormatOptions) -> Result<Layout>;
```

### `core/moat-engine/src/engine/mod.rs`

[Source](../../core/moat-engine/src/engine/mod.rs)

```rust
pub use crate::pipeline::{PollBudget, ResourceLimits};

pub use completion::{Completion, Lifecycle};

pub use device::Device;

pub use error::{Error, Rejected, Result};

pub use layout::{FormatOptions, Layout, format};

/// Runtime device-sync policy, not persisted or inferred from PLP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncMode {
    #[default]
    Enabled,
    Disabled,
}

/// Runtime resource limits and cooperative work targets, not persistent geometry.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Persistence policy, including explicit flush and close.
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
    // Private fields omitted.
}

/// One exclusive device owner. All online disk I/O goes through its Queue.
/// `open` returns before recovery and must be driven with `poll`. Blocking
/// helpers are explicitly named; no background thread is created.
pub struct Engine<D, Q> {
    // Private fields omitted.
}

impl<D: Device, Q: Queue> Engine<D, Q> {
    /// Accepts recovery without reading device contents. The capacity query and
    /// owner construction are synchronous setup; recovery I/O is poll-driven.
    pub fn open(device: D, queue: Q) -> Result<(Self, Ticket)>;
    /// Opens with explicit index/metadata bounds and progress budgets.
    pub fn open_with_options(device: D, queue: Q, options: Options) -> Result<(Self, Ticket)>;
    /// Explicit blocking recovery helper for tools and synchronous adapters.
    pub fn open_blocking(device: D, queue: Q) -> Result<Self>;
    /// Explicit blocking recovery with caller-selected runtime bounds.
    pub fn open_blocking_with_options(device: D, queue: Q, options: Options) -> Result<Self>;
    /// Current initialization/shutdown state.
    pub fn state(&self) -> State;
    /// Persisted geometry, available once recovery has succeeded.
    pub fn layout(&self) -> Result<Layout>;
    /// Current writable segment, if any.
    pub fn active_segment(&self) -> Option<u32>;
    /// Allocated slots discovered or allocated so far.
    pub fn allocated_segments(&self) -> usize;
    /// Preallocates index buckets within the configured logical entry bound.
    pub fn reserve_index(&mut self, additional: usize) -> Result<()>;
    /// Indexed versions including tombstones; unavailable before recovery completes.
    pub fn indexed_versions(&self) -> Result<usize>;
    /// Operations awaiting completion, including an internal automatic transition.
    pub fn in_flight(&self) -> usize;
    /// Whether a published live record exists.
    pub fn contains(&self, key: &ChunkId) -> Result<bool>;
    /// Published LSN and length; absence is distinct from not-ready.
    pub fn stat(&self, key: &ChunkId) -> Result<Option<(u64, u32)>>;
    /// Synchronously visits all versions. Use the cursor API to bound owner work.
    pub fn visit_versions(&self, visit: impl FnMut(ChunkId, u64, Option<u32>)) -> Result<()>;
    /// Captures the traversal boundary without copying the index.
    pub fn version_cursor(&self) -> Result<VersionCursor>;
    /// Visits at most `limit` keys; returns true at the captured end. The cursor
    /// must belong to this engine and does not prevent concurrent overwrites.
    pub fn visit_versions_batch(
        &self,
        cursor: &mut VersionCursor,
        limit: usize,
        mut visit: impl FnMut(ChunkId, u64, Option<u32>),
    ) -> Result<bool>;
    /// Runs one bounded progress turn. A nonblocking Queue is required for
    /// nonblocking disk I/O; synchronous FileQueue remains an explicit backend.
    pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize>;
    /// Local work remains runnable; poll again before waiting on a descriptor.
    pub fn has_ready(&self) -> bool;
    /// Optional queue readiness descriptor; available with notification-enabled backends.
    pub fn notification_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>>;
    /// Minimum caller-buffer sizes for a published record and verification mode.
    pub fn read_requirements(&self, key: ChunkId, range: Range<u32>, verify: bool) -> Result<ReadRequirements>;
    /// Reads a published snapshot. Reads can continue during sealing/rollover.
    pub fn read(
        &mut self,
        key: ChunkId,
        range: Range<u32>,
        verify: bool,
        buffers: ReadBuffers,
    ) -> std::result::Result<Ticket, Rejected<ReadBuffers>>;
    /// Enqueues a persistence barrier. Retry after an active lifecycle transition.
    pub fn flush(&mut self) -> Result<Ticket>;
    /// Encodes and accepts a frame. On segment pressure, starts asynchronous
    /// rollover and returns Backpressure with the original buffer for retry.
    pub fn write(
        &mut self,
        frame: &FrameBuilder<'_>,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>>;
    /// Accepts a prepared value. Checksum work remains synchronous and is bounded
    /// by the frame limit; rejected buffers retain their prepared payload.
    pub fn write_prepared(
        &mut self,
        key: ChunkId,
        lsn: u64,
        len: u32,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>>;
    /// Asynchronously seals and selects the next unused segment.
    pub fn rollover(&mut self) -> Result<Ticket>;
    /// Asynchronously selects a caller-chosen unused slot; no reuse is authorized.
    pub fn rollover_to(&mut self, number: u32) -> Result<Ticket>;
    /// Asynchronously seals after preceding writes; existing reads may continue.
    pub fn seal(&mut self) -> Result<Ticket>;
    /// Stops admission and asynchronously drains, persists, and seals. Dropping
    /// before completion may block in the queue's buffer-safety fallback.
    pub fn close(&mut self) -> Result<Ticket>;
    /// Explicit blocking seal helper. Requires no outstanding operations so it
    /// cannot consume another caller's completion.
    pub fn seal_blocking(&mut self) -> Result<()>;
    /// Explicit blocking rollover helper for tools.
    pub fn rollover_blocking(&mut self) -> Result<()>;
    /// Explicit blocking selection helper for tools.
    pub fn rollover_to_blocking(&mut self, number: u32) -> Result<()>;
    /// Explicit blocking orderly shutdown helper for tools.
    pub fn close_blocking(&mut self) -> Result<()>;
}
```

### `core/moat-engine/src/frame/builder.rs`

[Source](../../core/moat-engine/src/frame/builder.rs)

```rust
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
    // Private fields omitted.
}

impl<'a> FrameBuilder<'a> {
    /// Starts an empty frame, bounded independently of the batching policy.
    pub fn new(limits: FrameLimits) -> Self;
    /// Admits a data record, including a zero-length value.
    pub fn push(&mut self, key: ChunkId, lsn: u64, value: &'a [u8]) -> Result<()>;
    /// Admits an explicit deletion; it has no value or payload checksums.
    pub fn push_tombstone(&mut self, key: ChunkId, lsn: u64) -> Result<()>;
    /// Number of accepted records, including tombstones.
    pub fn len(&self) -> usize;
    /// Whether no records have been accepted.
    pub fn is_empty(&self) -> bool;
    /// Actual front metadata size, including header, directory, and checksums.
    pub fn metadata_len(&self) -> usize;
    /// Exact page-rounded encoded length; zero when empty. This walks records.
    pub fn encoded_len(&self) -> usize;
    /// Encodes accepted records in directory order using sequential placement.
    ///
    /// The destination may be an aligned heap or registered pool buffer. Only
    /// the first `encoded_len()` bytes are modified. Capacity and segment bounds
    /// are checked before modification; an error preserves inputs for retry.
    /// Buffer address alignment is the submitting I/O layer's responsibility.
    pub fn encode_into(&self, position: FramePosition, bytes: &mut [u8]) -> Result<FrameHeader>;
    /// Drops accepted records while retaining directory capacity for reuse.
    pub fn clear(&mut self);
}

/// A single-record frame exposing the final page-aligned value region.
///
/// The caller supplies the buffer and fills `value_mut()` directly. Finishing
/// writes only metadata and padding; it computes checksums without moving the
/// payload. The returned immutable slice covers exactly the encoded frame.
/// No caller-supplied checksum API is exposed until its trust contract is set.
pub struct PreparedFrame<'a> {
    // Private fields omitted.
}

impl<'a> PreparedFrame<'a> {
    /// Required buffer length for a prepared, page-aligned value.
    pub fn required_len(limits: FrameLimits, value_len: u32) -> Result<usize>;
    /// Borrows a buffer large enough for the final frame, without copying data.
    pub fn new(limits: FrameLimits, value_len: u32, bytes: &'a mut [u8]) -> Result<Self>;
    /// Actual header, descriptor, and checksum bytes reserved for this value.
    pub fn metadata_len(&self) -> usize;
    /// Final payload region; its offset is page-aligned for nonempty values.
    pub fn value_mut(&mut self) -> &mut [u8];
    /// Writes metadata and padding and relinquishes mutable access to the frame.
    /// An error leaves the caller's underlying buffer intact.
    pub fn finish(self, position: FramePosition, key: ChunkId, lsn: u64) -> Result<&'a [u8]>;
}
```

### `core/moat-engine/src/frame/decode.rs`

[Source](../../core/moat-engine/src/frame/decode.rs)

```rust
/// A validated view of the header, directory, and checksum area.
///
/// Payload bytes need not be present. Metadata CRC validation always covers
/// the entire directory and checksum area, including other records' entries.
/// Sequential payload layouts require no allocation. Reordered payloads use a
/// temporary range vector to check overlap in O(N log N), then discard it.
#[derive(Debug, Clone, Copy)]
pub struct Metadata<'a> {
    // Private fields omitted.
}

impl<'a> Metadata<'a> {
    /// Validates all metadata and value geometry without reading any payload.
    pub fn decode(bytes: &'a [u8], limits: FrameLimits, position: FramePosition) -> Result<Self>;
    /// The encoded header, directory, and checksum area, without payload or padding.
    pub fn as_bytes(self) -> &'a [u8];
    /// The validated fixed header.
    pub fn header(self) -> FrameHeader;
    /// Looks up a record by directory index without allocation.
    pub fn record(self, ordinal: u32) -> Option<Record<'a>>;
    /// Iterates in directory order, which need not match value placement or LSN order.
    pub fn records(self) -> impl ExactSizeIterator<Item = Record<'a>>;
}

/// A directory entry and its borrowed, validated checksum array.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    // Private fields omitted.
}

impl Record<'_> {
    /// The record's logical identity and physical geometry.
    pub fn descriptor(self) -> RecordDescriptor;
    /// Returns a checksum by logical 64 KiB block index.
    pub fn checksum(self, block: u32) -> Option<u32>;
    /// Expands an in-bounds value-relative range to complete checksum blocks.
    /// Empty ranges remain empty and require no payload I/O or verification.
    pub fn verification_range(self, range: Range<u32>) -> Result<Range<u32>>;
    /// Verifies complete logical checksum blocks supplied by a range read.
    ///
    /// `range` is value-relative and must equal its `verification_range`;
    /// `bytes` must contain exactly that range. This prevents short input from
    /// accidentally validating as a final partial block. Tombstones and empty
    /// data both accept an empty payload, but remain distinct record kinds.
    pub fn verify(self, range: Range<u32>, bytes: &[u8]) -> Result<()>;
}

/// A complete frame whose metadata and every payload checksum have passed.
///
/// Recovery should accept records only after `decode` succeeds for the whole
/// frame. CRCs detect corruption; they do not make an I/O atomic or durable.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    // Private fields omitted.
}

impl<'a> Frame<'a> {
    /// Validates structure, bounds, and all payloads, without copying values.
    pub fn decode(bytes: &'a [u8], limits: FrameLimits, position: FramePosition) -> Result<Self>;
    /// Metadata view, retaining the lifetime of the original encoded buffer.
    pub fn metadata(self) -> Metadata<'a>;
    /// Encoded bytes covering exactly one frame.
    pub fn as_bytes(self) -> &'a [u8];
    /// Returns a contiguous value; tombstones and out-of-bounds indices return `None`.
    /// A zero-length data record returns `Some(&[])`.
    pub fn value(self, ordinal: u32) -> Option<&'a [u8]>;
}
```

### `core/moat-engine/src/frame/error.rs`

[Source](../../core/moat-engine/src/frame/error.rs)

```rust
/// Errors from frame construction or validation.
///
/// Match variants and numeric fields to choose a response. Diagnostic strings
/// and formatted messages are for humans and are not a stable parsing API.
/// Errors hold only inline numbers and static strings; construction does not
/// allocate or capture a backtrace. The enum may gain variants in later releases.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A caller supplied invalid geometry or arguments.
    #[error("invalid frame argument: {0}")]
    InvalidArgument(&'static str),
    /// A logical value exceeds the configured value limit. Flushing the
    /// pending frame or providing a larger output buffer cannot make it fit.
    #[error("value of {len} bytes exceeds the limit of {max} bytes")]
    ValueTooLarge {
        /// Logical input length in bytes.
        len: u64,
        /// Maximum logical value length allowed by the format.
        max: u32,
    },
    /// Records, metadata, and padding exceed the format's frame limit.
    /// A writer may close a nonempty builder and retry the next record in a
    /// fresh frame. A record that cannot fit by itself must be rejected.
    #[error("frame needs {required} bytes, limit is {limit}")]
    FrameFull {
        /// Required encoded length, including metadata and page padding.
        required: u64,
        /// Format-wide maximum encoded frame length.
        limit: u32,
    },
    /// The encoded frame exceeds the space remaining at the supplied segment
    /// position. The caller must choose a position with enough space.
    #[error("frame needs {required} bytes, segment position has {available}")]
    SegmentFull {
        /// Required encoded frame length.
        required: u64,
        /// Bytes remaining from the frame position to the segment end.
        available: u32,
    },
    /// The frame fits the format, but the caller's output buffer is too small.
    /// Retry with a larger buffer; segment admission is a separate check.
    #[error("output buffer needs {required} bytes, has {available}")]
    BufferTooSmall {
        /// Minimum required output buffer length.
        required: usize,
        /// Supplied output buffer length.
        available: usize,
    },
    /// An encoded structure or payload is incomplete. A streaming caller may
    /// supply more bytes; a recovery scanner must decide whether this is an
    /// unfinished active tail or corruption before a known sealed boundary.
    #[error("truncated frame: need {required} bytes, have {available}")]
    Truncated {
        /// Minimum byte length needed to continue decoding.
        required: usize,
        /// Supplied byte length.
        available: usize,
    },
    /// The encoding is recognized but its version is unsupported.
    #[error("unsupported frame version {0}")]
    UnsupportedVersion(u32),
    /// Encoded metadata is inconsistent or failed its checksum.
    #[error("corrupt frame: {0}")]
    Corrupt(&'static str),
    /// A logical value checksum failed.
    #[error("checksum mismatch in record {record}, block {block}")]
    PayloadChecksum {
        /// Directory index, independent of physical value order.
        record: u32,
        /// Checksum block index within the value.
        block: u32,
    },
}

/// Result of a frame operation.
pub type Result<T> = std::result::Result<T, Error>;
```

### `core/moat-engine/src/frame/header.rs`

[Source](../../core/moat-engine/src/frame/header.rs)

```rust
/// Format-wide decoding bounds, independent of a writer's batching target.
///
/// A future device superblock must persist these bounds. Reopening with a
/// smaller batching target must not reject previously written frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    // Private fields omitted.
}

impl FrameLimits {
    /// Sets the maximum encoded frame and logical value lengths in bytes.
    pub fn new(max_frame_len: u32, max_value_len: u32) -> Result<Self>;
    /// Maximum encoded frame length, including padding.
    pub fn max_frame_len(self) -> u32;
    /// Maximum logical value length.
    pub fn max_value_len(self) -> u32;
}

/// Expected physical identity of a frame in a particular segment allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePosition {
    // Private fields omitted.
}

impl FramePosition {
    /// Binds a frame to a segment incarnation and page-aligned byte offset.
    ///
    /// The first page belongs to the segment header. Segment length and all
    /// frame offsets must fit in the format's 32-bit geometry fields. The
    /// segment allocator must separately reserve space for its eventual footer.
    pub fn new(segment_seq: u64, offset: u32, segment_len: u32) -> Result<Self>;
    /// Allocation incarnation of the containing segment.
    pub fn segment_seq(self) -> u64;
    /// Byte offset relative to the start of the segment.
    pub fn offset(self) -> u32;
}

/// A checksummed and bounds-checked frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    // Private fields omitted.
}

impl FrameHeader {
    /// Validates the fixed header before any variable-sized allocation or read.
    /// The supplied slice may contain just the header, a page, or a whole frame.
    pub fn decode(bytes: &[u8], limits: FrameLimits, position: FramePosition) -> Result<Self>;
    /// Physical identity checked by the decoder.
    pub fn position(self) -> FramePosition;
    /// Encoded byte length, including final page padding.
    pub fn frame_len(self) -> usize;
    /// Number of descriptors in the directory, including tombstones.
    pub fn record_count(self) -> u32;
    /// Actual metadata byte length, without rounding to a page.
    pub fn metadata_len(self) -> usize;
}
```

### `core/moat-engine/src/frame/mod.rs`

[Source](../../core/moat-engine/src/frame/mod.rs)

```rust
pub use builder::{FrameBuilder, PreparedFrame};

pub use decode::{Frame, Metadata, Record};

pub use error::{Error, Result};

pub use header::{FrameHeader, FrameLimits, FramePosition};

pub use record::{RecordDescriptor, RecordKind};

/// Size of an encoded frame header.
pub const HEADER_LEN: usize = 64;

/// Size of an encoded record descriptor.
pub const DESCRIPTOR_LEN: usize = 64;

/// Persistent alpha format version.
pub const FORMAT_VERSION: u32 = 1;

/// Frame identification bytes. Legacy batch encodings are never accepted.
pub const MAGIC: [u8; 8] = *b"MOATFRM1";
```

### `core/moat-engine/src/frame/record.rs`

[Source](../../core/moat-engine/src/frame/record.rs)

```rust
/// The logical meaning of a directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// An immutable value. Zero-length values are valid data.
    Data = 1,
    /// An explicit deletion with no value and no payload checksums.
    Tombstone = 2,
}

/// Decoded fields of a 64-byte record directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordDescriptor {
    /// Full 128-bit chunk identifier.
    pub key: ChunkId,
    /// Logical sequence number, independent of physical placement.
    pub lsn: u64,
    /// Frame-relative value offset; zero for an empty value or tombstone.
    pub value_offset: u32,
    /// Logical value length in bytes.
    pub value_len: u32,
    /// Frame-relative checksum offset; zero when there are no checksums.
    pub checksum_offset: u32,
    /// Number of CRC32C entries, one per logical 64 KiB value block.
    pub checksum_count: u32,
    /// Data or explicit deletion.
    pub kind: RecordKind,
}
```

### `core/moat-engine/src/io/buffer.rs`

[Source](../../core/moat-engine/src/io/buffer.rs)

```rust
/// Owned, page-aligned storage returned unchanged by I/O completion.
/// Pool buffers reuse the common arena allocator and may be registered with
/// a queue. Moving this enum neither copies bytes nor clones the pool owner.
#[derive(Debug)]
pub enum Buffer {
    /// Individually allocated, zero-initialized storage.
    Heap(AlignedBuf),
    /// Reusable arena storage; contents are not necessarily zeroed.
    Pooled(PooledBuf),
}

impl From<AlignedBuf> for Buffer {
    fn from(buffer: AlignedBuf) -> Self;
}

impl From<PooledBuf> for Buffer {
    fn from(buffer: PooledBuf) -> Self;
}

impl Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8];
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8];
}
```

### `core/moat-engine/src/io/file.rs`

[Source](../../core/moat-engine/src/io/file.rs)

```rust
/// Blocking positional file I/O with deferred completion delivery.
///
/// This portable backend is intended for functional tests and local use. It
/// blocks at submission; the Linux `UringQueue` provides asynchronous submission.
/// The file may use direct I/O: transfer offsets, lengths, and buffers are aligned.
pub struct FileQueue {
    // Private fields omitted.
}

impl FileQueue {
    /// Takes ownership of an already opened file and bounds retained completions.
    pub fn new(file: File, depth: usize) -> io::Result<Self>;
}

impl Queue for FileQueue {
    fn has_ready(&self) -> bool;
    fn depth(&self) -> usize;
    fn vacant(&self) -> usize;
    fn try_submit(&mut self, mut request: Request) -> Result<(), Request>;
    fn poll(&mut self, _wait: bool) -> io::Result<()>;
    fn pop(&mut self) -> Option<Completion>;
}
```

### `core/moat-engine/src/io/mod.rs`

[Source](../../core/moat-engine/src/io/mod.rs)

```rust
pub use buffer::Buffer;

#[cfg(unix)]
pub use file::FileQueue;

#[cfg(target_os = "linux")]
pub use uring::UringQueue;

/// Physical operation. A sync is a barrier only after preceding writes complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Read into the owned buffer.
    Read,
    /// Write from the owned buffer.
    Write,
    /// Persist preceding completed writes.
    Sync,
}

/// An operation and the memory that must outlive it.
#[derive(Debug)]
pub struct Request {
    /// Caller token, returned unchanged on completion.
    pub token: u64,
    /// Physical operation.
    pub operation: Operation,
    /// Absolute file/device byte offset.
    pub offset: u64,
    /// Prefix length to transfer; zero for sync.
    pub len: usize,
    /// Exclusively owned storage, absent for sync.
    pub buffer: Option<Buffer>,
}

/// Completed operation, including its original buffer even after an I/O error.
#[derive(Debug)]
pub struct Completion {
    /// Original request and buffer ownership.
    pub request: Request,
    /// Actual transferred bytes; short transfers are not successful full writes.
    pub result: io::Result<usize>,
}

/// A bounded queue driven exclusively through a mutable owner.
///
/// Accepted requests must produce exactly one completion with their original
/// token, operation, offset, length, and buffer. A rejected request must not have
/// reached the device. `depth` includes completions not yet popped. Implementors
/// must retain buffers until the OS stops accessing them, including during drop.
pub trait Queue {
    fn has_ready(&self) -> bool; // Default implementation available.
    fn notification_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>>; // Default implementation available.
    fn depth(&self) -> usize;
    fn vacant(&self) -> usize;
    fn try_submit(&mut self, request: Request) -> Result<(), Request>;
    fn poll(&mut self, wait: bool) -> io::Result<()>;
    fn pop(&mut self) -> Option<Completion>;
}
```

### `core/moat-engine/src/io/uring.rs`

[Source](../../core/moat-engine/src/io/uring.rs)

```rust
/// A Linux io_uring queue with single-owner state and batched submission.
///
/// Requests own aligned buffers until their CQEs arrive. Buffers can be reused
/// by the caller after completion. Pool arenas and the file are registered once;
/// heap buffers still use ordinary READ/WRITE. Block-device requests are split at
/// the device byte limit. Subrequests share the original buffer and produce one
/// logical completion; both accepted requests and in-flight SQEs are bounded by
/// `depth`. Create and drive the queue on the same thread, as required by
/// deferred task execution and the pool allocator.
pub struct UringQueue {
    // Private fields omitted.
}

impl UringQueue {
    /// Creates an asynchronous queue. Initialization errors are never downgraded
    /// silently to blocking I/O. The caller opens the file with its desired flags.
    pub fn new(file: File, depth: usize) -> io::Result<Self>;
    /// Registers the pool's arenas once, including their huge-page backing.
    /// Pool buffers must belong to this pool; heap buffers remain supported.
    /// Registration failures are returned rather than silently disabling fixed I/O.
    pub fn with_pool(file: File, depth: usize, pool: Arc<BufferPool>) -> io::Result<Self>;
    /// Whether SINGLE_ISSUER and DEFER_TASKRUN were enabled together.
    pub fn deferred_taskrun(&self) -> bool;
    /// Maximum bytes per read/write SQE. Block devices supply their queue limit;
    /// regular files use the maximum aligned request length unless capped below.
    pub fn max_io_len(&self) -> usize;
    /// Caps individual SQEs without changing logical request sizes. Call before
    /// submitting work. The cap must be a nonzero multiple of 4 KiB and cannot
    /// raise a previously established limit. Filesystem I/O may still offload.
    pub fn with_max_io_len(mut self, bytes: usize) -> io::Result<Self>;
    /// Creates a completion-notifying queue for epoll/poll integration. Deferred
    /// task execution is deliberately disabled so readiness does not depend on
    /// the sleeping owner entering the ring first. Pool ownership is unchanged.
    pub fn with_notifications(file: File, depth: usize, pool: Option<Arc<BufferPool>>) -> io::Result<Self>;
}

impl Queue for UringQueue {
    fn notification_fd(&self) -> Option<BorrowedFd<'_>>;
    fn has_ready(&self) -> bool;
    fn depth(&self) -> usize;
    fn vacant(&self) -> usize;
    fn try_submit(&mut self, request: Request) -> Result<(), Request>;
    fn poll(&mut self, wait: bool) -> io::Result<()>;
    fn pop(&mut self) -> Option<Completion>;
}

impl Drop for UringQueue {
    fn drop(&mut self);
}
```

### `core/moat-engine/src/lib.rs`

[Source](../../core/moat-engine/src/lib.rs)

```rust
pub mod frame;

pub mod io;

pub mod pipeline;

pub mod segment;

#[cfg(unix)]
pub mod engine;
```

### `core/moat-engine/src/pipeline/driver.rs`

[Source](../../core/moat-engine/src/pipeline/driver.rs)

```rust
impl<Q: Queue> Pipeline<Q> {
    /// Submits/reaps I/O and appends completed operations to the caller's vector.
    /// Write notifications follow submission order. Reads may finish independently.
    /// A fatal queue error requires abandoning this pipeline; pending submitted
    /// buffers remain owned by the queue until it can safely release them.
    pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize>;
    /// Whether local completions/publication remain runnable without waiting.
    /// Callers should poll again before sleeping on a notification descriptor.
    pub fn has_ready(&self) -> bool;
}
```

### `core/moat-engine/src/pipeline/error.rs`

[Source](../../core/moat-engine/src/pipeline/error.rs)

```rust
/// Runtime errors; I/O failures preserve the OS error as their source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Retry after polling completions; no work was accepted.
    #[error("pipeline is full or a flush is pending")]
    Backpressure,
    /// Seal the active segment before accepting more footer metadata.
    #[error("active segment metadata budget exhausted")]
    MetadataFull,
    /// A configured resource bound would be exceeded before admission.
    #[error("engine resource limit exceeded: {0}")]
    ResourceLimit(&'static str),
    /// Memory reservation failed before admitting an operation.
    #[error("metadata allocation failed: {0}")]
    Allocation(#[from] std::collections::TryReserveError),
    /// A recovered pipeline cannot write into its old allocation.
    #[error("pipeline is read-only")]
    ReadOnly,
    /// An earlier write or barrier failed. Stop using this allocation for writes.
    #[error("write path failed at ticket {0}")]
    WriteFailed(u64),
    /// The key is absent or its newest version is a tombstone.
    #[error("chunk not found")]
    NotFound,
    /// Invalid caller geometry or an exhausted ticket counter.
    #[error("invalid pipeline argument: {0}")]
    InvalidArgument(&'static str),
    /// Frame construction or verification error.
    #[error(transparent)]
    Frame(#[from] frame::Error),
    /// Segment allocation or metadata error.
    #[error(transparent)]
    Segment(#[from] segment::Error),
    /// An operation failed at a physical file position.
    #[error("{operation:?} at byte {offset} failed: {source}")]
    Io {
        /// Failed operation.
        operation: Operation,
        /// Absolute byte offset.
        offset: u64,
        /// Original OS failure.
        #[source]
        source: io::Error,
    },
    /// A short transfer is not silently retried as a full operation.
    #[error("short {operation:?} at byte {offset}: expected {expected}, got {actual}")]
    ShortIo {
        /// Operation that completed short.
        operation: Operation,
        /// Absolute byte offset.
        offset: u64,
        /// Requested bytes.
        expected: usize,
        /// Transferred bytes.
        actual: usize,
    },
    /// The queue cannot establish completion state; abandon this pipeline.
    #[error("I/O queue failed: {0}")]
    Queue(#[source] io::Error),
    /// Further operations cannot use a failed queue.
    #[error("I/O queue is no longer usable")]
    QueueFailed,
}

/// Pipeline operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// Rejected admission with input ownership preserved for retry.
#[derive(Debug)]
pub struct Rejected<T, E = Error> {
    /// Why admission failed.
    pub error: E,
    /// Original input buffers, still owned by the caller.
    pub input: T,
}
```

### `core/moat-engine/src/pipeline/maintenance.rs`

[Source](../../core/moat-engine/src/pipeline/maintenance.rs)

```rust
impl<Q: Queue> Pipeline<Q> {
    /// Configures bounded resources before restoring or accepting records.
    pub fn configure(&mut self, resources: ResourceLimits, budget: PollBudget) -> Result<()>;
    /// A Unix descriptor notifying completion readiness, when the queue provides one.
    /// Drive `poll(false)` before sleeping and after each readiness notification.
    pub fn notification_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>>;
}
```

### `core/moat-engine/src/pipeline/mod.rs`

[Source](../../core/moat-engine/src/pipeline/mod.rs)

```rust
pub use error::{Error, Rejected, Result};

pub use options::{PollBudget, ResourceLimits};

pub use read::{ReadBuffers, ReadRange, ReadRequirements};

/// Completion identity, scoped to the pipeline that issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticket(/* private fields */);

impl Ticket {
    /// Monotonically increasing number within this pipeline.
    pub fn number(self) -> u64;
}

/// An operation result and its reusable buffers, including on failure.
#[derive(Debug)]
pub enum Completion {
    /// Frame write is complete and its records have been applied by LSN on success.
    Write {
        /// Admission identity.
        ticket: Ticket,
        /// Publication outcome. Success does not imply persistence.
        result: Result<()>,
        /// Original frame buffer.
        buffer: Buffer,
    },
    /// Read of the version visible at admission, with the requested verification policy.
    Read {
        /// Admission identity.
        ticket: Ticket,
        /// Requested bytes in either buffer, or an I/O/verification failure.
        result: Result<ReadRange>,
        /// Original buffers, returned on both success and failure.
        buffers: ReadBuffers,
    },
    /// All preceding accepted writes and the subsequent persistence barrier finished.
    /// Terminal notification after queue failure. Submitted buffers remain owned
    /// by the queue until safe teardown; this event does not transfer them.
    Failed {
        /// Accepted operation identity.
        ticket: Ticket,
        /// Shared fatal cause.
        error: std::sync::Arc<Error>,
    },
    /// All preceding writes and the persistence barrier completed.
    Flush {
        /// Admission identity.
        ticket: Ticket,
        /// Durability outcome.
        result: Result<()>,
    },
}

impl Completion {
    /// The admission identity regardless of operation kind.
    pub fn ticket(&self) -> Ticket;
}

/// A bounded pipeline whose queue, index, and publication order share one owner.
///
/// Different workers can own independent pipelines without locks. A returned
/// read is a snapshot of the index at admission; newer writes do not invalidate
/// its immutable storage. Segment reuse and concurrent external modification are
/// prohibited while this pipeline or any read into its storage remains live.
pub struct Pipeline<Q> {
    // Private fields omitted.
}

impl<Q: Queue> Pipeline<Q> {
    /// Attaches a newly allocated segment whose active header is already durable.
    ///
    /// The queue must be empty and dedicated to this pipeline. `base` is the
    /// segment's absolute device offset. The caller must persist `limits` with
    /// device geometry and establish a fresh incarnation before calling this.
    /// Never use this constructor to resume an existing active allocation.
    pub fn new(queue: Q, header: SegmentHeader, limits: FrameLimits, base: u64) -> Result<Self>;
    /// Attaches recovered immutable storage; populate its index with `restore`.
    pub fn read_only(queue: Q, header: SegmentHeader, limits: FrameLimits, base: u64) -> Result<Self>;
    /// Rebuilds the index from scanner-validated frames or a validated sealed footer.
    /// Call only during read-only startup, before submitting operations. Tombstones
    /// and older physical records are resolved by LSN, just as during publication.
    pub fn restore(&mut self, metadata: Metadata<'_>) -> Result<()>;
    /// Queues a persistence barrier after preceding writes, blocking new write
    /// admission until it completes. Reads may continue. Poll to drive progress.
    pub fn flush(&mut self) -> Result<Ticket>;
    /// Number of admitted operations whose completions have not been delivered.
    pub fn in_flight(&self) -> usize;
    /// Whether a published data version exists (tombstones return false).
    pub fn contains(&self, key: &ChunkId) -> bool;
}
```

### `core/moat-engine/src/pipeline/options.rs`

[Source](../../core/moat-engine/src/pipeline/options.rs)

```rust
/// Maximum work retired by one poll. A single indivisible frame/read may exceed
/// the byte or record target; subsequent operations wait for the next poll.
#[derive(Debug, Clone, Copy)]
pub struct PollBudget {
    /// Maximum completion/state-machine steps; must be nonzero.
    pub operations: usize,
    /// Encoded/verified bytes processed before yielding; must be nonzero.
    pub bytes: usize,
    /// Records published before yielding; must be nonzero.
    pub records: usize,
}

impl Default for PollBudget {
    fn default() -> Self;
}

/// Hard bounds on logical metadata quantities, independent of I/O pool size.
/// Hash-table allocator overhead and caller-owned buffers are additional.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Maximum encoded frame buffer; bounds indivisible encoding/CRC work.
    pub frame_bytes: usize,
    /// Indexed keys, including tombstones, plus conservative in-flight reservations.
    pub index_entries: usize,
    /// Separate bounds for active-segment metadata, a recovery footer, and
    /// logical device routing entries (allocator capacity overhead is additional).
    pub metadata_bytes: usize,
    /// Maximum in-flight index entries (including overwrites).
    pub pending_records: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self;
}
```

### `core/moat-engine/src/pipeline/read.rs`

[Source](../../core/moat-engine/src/pipeline/read.rs)

```rust
/// Reusable, caller-owned buffers for metadata and the requested value extent.
#[derive(Debug)]
pub struct ReadBuffers {
    /// Space for front metadata when verification is enabled; otherwise optional.
    pub metadata: Option<Buffer>,
    /// Space for requested pages, expanded to checksum blocks when verifying.
    pub value: Buffer,
}

/// Minimum buffer capacities for a read of the currently indexed version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequirements {
    /// Page-rounded front metadata extent; zero when verification is disabled.
    pub metadata_len: usize,
    /// Page-rounded value extent, expanded to checksum blocks when verifying.
    /// Zero for empty ranges or verified bytes already covered by metadata.
    pub value_len: usize,
}

/// Location of requested bytes within the returned buffers.
/// Small values already fetched with metadata need no second read or payload copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadRange {
    /// Requested bytes are in the metadata I/O buffer.
    Metadata(Range<usize>),
    /// Requested bytes are in the separate value I/O buffer.
    Value(Range<usize>),
}

impl ReadRange {
    /// Whether the requested range was empty.
    pub fn is_empty(&self) -> bool;
}

impl ReadBuffers {
    /// Creates buffers for ordinary reads without allocating metadata storage.
    pub fn new(value: impl Into<Buffer>) -> Self;
    /// Borrows returned bytes without copying them.
    pub fn view(&self, range: ReadRange) -> &[u8];
}

impl<Q: Queue> Pipeline<Q> {
    /// Required capacities for the currently indexed version and read policy.
    /// Without verification metadata is unused and ranges only round to pages.
    /// Admission checks capacities again if a write publishes in the meantime.
    pub fn read_requirements(&self, key: ChunkId, range: Range<u32>, verify: bool) -> Result<ReadRequirements>;
    /// Reads a snapshot of the indexed version into reusable buffers.
    ///
    /// With `verify = false`, read only requested value pages and trust the
    /// published index. No metadata is decoded and no CRC is checked. I/O errors,
    /// short transfers, range bounds and segment lifetime rules still apply.
    /// With `verify = true`, also fetch and validate metadata and verify complete
    /// checksum blocks. In both modes buffers return on completion or rejection.
    /// Empty unverified reads complete through `poll` without submitting I/O.
    pub fn read(
        &mut self,
        key: ChunkId,
        range: Range<u32>,
        verify: bool,
        buffers: ReadBuffers,
    ) -> std::result::Result<Ticket, Rejected<ReadBuffers>>;
}
```

### `core/moat-engine/src/pipeline/write.rs`

[Source](../../core/moat-engine/src/pipeline/write.rs)

```rust
impl<Q: Queue> Pipeline<Q> {
    /// Encodes and submits one frame without staging a second payload copy.
    ///
    /// The caller may refill/reuse its builder after admission. On rejection it
    /// retains its original buffer and borrowed values. Caller-supplied LSNs must
    /// uniquely identify logical versions; equal LSNs keep the first occurrence.
    pub fn write(
        &mut self,
        frame: &FrameBuilder<'_>,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>>;
    /// Finalizes and submits an already filled prepared value without copying it.
    ///
    /// Fill `PreparedFrame::new(limits, value_len, &mut buffer)?.value_mut()`
    /// before calling this method. The pipeline writes metadata and checksums,
    /// then transfers ownership of that same allocation to its I/O queue.
    /// Rejection retains the buffer and the prepared payload for retry.
    pub fn write_prepared(
        &mut self,
        key: ChunkId,
        lsn: u64,
        value_len: u32,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>>;
}
```

### `core/moat-engine/src/segment/builder.rs`

[Source](../../core/moat-engine/src/segment/builder.rs)

```rust
/// Sequential frame allocation accounting and construction of the seal footer.
///
/// Call `position` before encoding, then `append` before submitting each frame.
/// Appended metadata includes every allocated frame, even if its write has not
/// completed. The caller owns submission order, completion tracking, and buffers.
/// On a failed frame write, abandon this builder rather than seal its footer.
#[derive(Debug)]
pub struct SegmentBuilder {
    // Private fields omitted.
}

impl SegmentBuilder {
    /// Starts accounting for a newly allocated, empty segment.
    ///
    /// An active header does not prove that a segment is empty. The caller must
    /// durably establish its new incarnation before submitting any frame writes.
    /// It may also collect validated recovery metadata to seal a recovered prefix,
    /// but must never be used to append new frame writes to that old allocation.
    pub fn new(header: SegmentHeader) -> Result<Self>;
    /// Finds the next position while reserving all accumulated footer metadata.
    /// This is a nonmutating check; `append` commits the allocation accounting.
    pub fn position(&self, frame_len: usize, metadata_len: usize) -> Result<FramePosition>;
    /// Records one encoded frame's validated metadata before its I/O submission.
    ///
    /// The metadata must match the next physical position. This copies metadata
    /// only; it neither reads payloads nor verifies that a write has completed.
    /// Failed admission leaves this builder unchanged.
    pub fn append(&mut self, metadata: Metadata<'_>) -> Result<()>;
    /// Accumulated footer metadata bytes, excluding padding and trailer.
    pub fn metadata_len(&self) -> usize;
    /// Current allocated data boundary, including writes not yet completed.
    pub fn data_end(&self) -> u32;
    /// Bytes reserved for the complete page-rounded footer.
    pub fn footer_len(&self) -> usize;
    /// Encodes the footer and stops further admission, returning the sealed header.
    ///
    /// This only constructs bytes. After successful frame writes, the caller
    /// places the footer at the segment end. Persist data and preceding footer
    /// pages before writing and persisting the final page containing the trailer.
    /// The returned sealed header is an in-memory view, never an allocation write.
    /// A short output buffer leaves both the builder and destination unchanged.
    pub fn seal_into(&mut self, bytes: &mut [u8]) -> Result<SegmentHeader>;
}
```

### `core/moat-engine/src/segment/error.rs`

[Source](../../core/moat-engine/src/segment/error.rs)

```rust
/// Segment construction, validation, or recovery failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Metadata allocation failed before admission.
    #[error("segment metadata allocation failed: {0}")]
    Allocation(#[from] std::collections::TryReserveError),
    /// Caller-supplied segment geometry or frame order is invalid.
    #[error("invalid segment argument: {0}")]
    InvalidArgument(&'static str),
    /// A finished builder cannot admit more frames.
    #[error("segment builder is already sealed")]
    Sealed,
    /// Data and the eventual footer do not both fit; choose another segment.
    #[error("segment needs {required} bytes including its footer, capacity is {capacity}")]
    Full {
        /// Total required segment bytes, including its header and footer.
        required: u64,
        /// Physical segment capacity in bytes.
        capacity: u32,
    },
    /// The destination is too short; no output or builder state was changed.
    #[error("output buffer needs {required} bytes, has {available}")]
    BufferTooSmall {
        /// Minimum output size.
        required: usize,
        /// Supplied output size.
        available: usize,
    },
    /// Encoded segment metadata is incomplete.
    #[error("truncated segment metadata: need {required} bytes, have {available}")]
    Truncated {
        /// Minimum input size.
        required: usize,
        /// Supplied input size.
        available: usize,
    },
    /// The segment or footer uses an unsupported encoding version.
    #[error("unsupported segment metadata version {0}")]
    UnsupportedVersion(u32),
    /// A checksum, identity, or structural invariant failed.
    #[error("corrupt segment: {0}")]
    Corrupt(&'static str),
    /// A frame failed validation at a known segment-relative offset.
    #[error("invalid frame at segment offset {offset}: {source}")]
    Frame {
        /// Physical frame offset in bytes.
        offset: u32,
        /// Original frame validation failure.
        #[source]
        source: frame::Error,
    },
}

/// Result of a segment operation.
pub type Result<T> = std::result::Result<T, Error>;
```

### `core/moat-engine/src/segment/footer.rs`

[Source](../../core/moat-engine/src/segment/footer.rs)

```rust
/// Independently validated seal record at the end of a segment's final page.
///
/// Identity and generation must be compared with the allocation header before
/// using its lengths or accepting any embedded metadata.
#[derive(Debug, Clone, Copy)]
pub struct FooterTrailer {
    // Private fields omitted.
}

impl FooterTrailer {
    /// Decodes the final 64 bytes of a tail page against trusted segment geometry.
    /// This does not validate the preceding footer bytes or allocation identity.
    pub fn decode(tail: &[u8], segment_len: u32) -> Result<Self>;
    /// Sealed in-memory segment view; compare its identity with the allocation.
    pub fn header(self) -> SegmentHeader;
}

/// Validated sealed metadata, borrowing the footer without copying its directory.
///
/// A footer summarizes frames, not payload integrity. Verified reads must still
/// fetch and check the requested payload blocks. On footer failure, the caller
/// may scan frames while preserving the sealed header's exact data boundary.
#[derive(Debug, Clone, Copy)]
pub struct Footer<'a> {
    // Private fields omitted.
}

impl<'a> Footer<'a> {
    /// Validates the footer and every embedded frame's metadata and position.
    /// The header must already be validated independently of these footer bytes.
    pub fn decode(bytes: &'a [u8], header: SegmentHeader, limits: FrameLimits) -> Result<Self>;
    /// Iterates validated frame metadata in physical frame order.
    /// Record LSN order is independent; index reconstruction must compare LSNs.
    pub fn frames(self) -> impl ExactSizeIterator<Item = Metadata<'a>>;
}
```

### `core/moat-engine/src/segment/header.rs`

[Source](../../core/moat-engine/src/segment/header.rs)

```rust
/// Physical segment identity, including the allocation incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentId {
    /// Device identity from the device superblock.
    pub device_id: [u8; 16],
    /// Segment number within the device.
    pub segment_no: u32,
    /// Nonzero device-wide allocation sequence; never reuse it on that device.
    pub sequence: u64,
}

/// Validated segment identity, geometry, and optional sealed boundary.
///
/// The on-disk allocation header is immutable and carries no speculative tail.
/// A validated footer trailer adds a sealed boundary to this in-memory view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    // Private fields omitted.
}

impl SegmentHeader {
    /// Creates an active header. Even an empty segment needs header/footer pages.
    pub fn new(id: SegmentId, segment_len: u32) -> Result<Self>;
    /// Validates the complete header page against the containing device geometry.
    ///
    /// The incarnation is discovered from the validated header. The caller must
    /// supply device identity, segment number, and size from trusted geometry.
    pub fn decode(bytes: &[u8], device_id: [u8; 16], segment_no: u32, segment_len: u32) -> Result<Self>;
    /// Writes an unsealed allocation page, zeroing all reserved bytes.
    pub fn encode_into(self, bytes: &mut [u8]) -> Result<()>;
    /// Physical identity, suitable for caller-owned segment selection.
    pub fn id(self) -> SegmentId;
    /// Complete physical segment size in bytes.
    pub fn segment_len(self) -> u32;
    /// Whether a durable sealed boundary is claimed by this header.
    pub fn is_sealed(self) -> bool;
    /// Segment-relative footer extent, present only for sealed headers.
    pub fn footer_range(self) -> Option<Range<u32>>;
    /// End of committed frame data, excluding the gap before the footer.
    pub fn data_end(self) -> Option<u32>;
}
```

### `core/moat-engine/src/segment/mod.rs`

[Source](../../core/moat-engine/src/segment/mod.rs)

```rust
pub use builder::SegmentBuilder;

pub use error::{Error, Result};

pub use footer::{Footer, FooterTrailer};

pub use header::{SegmentHeader, SegmentId};

pub use recovery::Scanner;

/// Segment and footer format version, independent of the frame version.
pub const FORMAT_VERSION: u32 = 1;

/// Identification bytes for the one-page segment header.
pub const HEADER_MAGIC: [u8; 8] = *b"MOATSEG1";

/// Identification bytes for a sealed segment's metadata footer.
pub const FOOTER_MAGIC: [u8; 8] = *b"MOATFTR1";

/// Fixed trailer at the end of a page-rounded footer.
pub const FOOTER_TRAILER_LEN: usize = 64;
```

### `core/moat-engine/src/segment/recovery.rs`

[Source](../../core/moat-engine/src/segment/recovery.rs)

```rust
/// Incremental recovery of complete frames from a trusted segment header.
///
/// Supply complete candidate frame bytes (or the remaining segment slice) at
/// `position`. A damaged active tail ends recovery without searching for another
/// magic value. Sealed segments require every frame before their committed data
/// boundary to validate; footer failure never weakens that requirement.
/// This scanner neither merges LSNs nor authorizes appending to recovered tails.
#[derive(Debug)]
pub struct Scanner {
    // Private fields omitted.
}

impl Scanner {
    /// Starts after the segment header. A damaged header must not reach here.
    pub fn new(header: SegmentHeader, limits: FrameLimits) -> Self;
    /// Expected position of the next frame, or none once recovery has ended.
    /// An I/O caller may validate its fixed frame header first to bound the read.
    pub fn position(&self) -> Option<FramePosition>;
    /// Validates the entire next frame before exposing any of its records.
    ///
    /// `Ok(None)` means a complete sealed extent or the end of an active prefix.
    /// For active tails, `tail_error` explains a rejected candidate. An unsupported
    /// frame version is always returned as an error, never silently discarded.
    /// I/O failures must be handled by the caller, not converted to empty input.
    pub fn next_frame<'a>(&mut self, bytes: &'a [u8]) -> Result<Option<Frame<'a>>>;
    /// End of the fully validated frame prefix, in segment-relative bytes.
    pub fn data_end(&self) -> u32;
    /// Why the first rejected active-tail candidate could not be recovered.
    /// Absence does not prove that an active segment had been cleanly sealed.
    pub fn tail_error(&self) -> Option<&frame::Error>;
}
```

## `moat-cache-store` declaration inventory

### `core/moat-cache-store/src/delivery.rs`

[Source](../../core/moat-cache-store/src/delivery.rs)

```rust
/// Schedules batched completion delivery on an application's existing executor.
/// It must arrange for each task to be polled without blocking `spawn`.
/// Dropping a task falls back to ordered delivery on the producer/drop thread.
pub trait CompletionExecutor: Debug + Send + Sync + 'static {
    fn spawn(&self, task: BoxFuture<'static, ()>);
}
```

### `core/moat-cache-store/src/lib.rs`

[Source](../../core/moat-cache-store/src/lib.rs)

```rust
pub use delivery::CompletionExecutor;

pub use request::{Chunk, DeleteResult, Error, Request, Result};

pub use store::{DiskInfo, InventoryEntry, Options, ReadLocation, Statistics, Store};
```

### `core/moat-cache-store/src/request.rs`

[Source](../../core/moat-cache-store/src/request.rs)

```rust
/// Errors shared by coalesced callers without losing the underlying cause.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// Admission exceeded the configured count or byte budget.
    #[error("store admission budget exhausted")]
    Busy,
    /// The store is closing or its worker stopped before returning a reply.
    #[error("store is closed")]
    Closed,
    /// Invalid adapter configuration or operation argument.
    #[error("invalid store option: {0}")]
    Invalid(&'static str),
    /// A chunk engine operation failed.
    #[error(transparent)]
    Engine(Arc<moat_server::storage::Error>),
    /// Queue initialization or progress failed.
    #[error(transparent)]
    Io(Arc<std::io::Error>),
}

impl From<moat_server::storage::Error> for Error {
    fn from(error: moat_server::storage::Error) -> Self;
}

impl From<moat_engine::pipeline::Error> for Error {
    fn from(error: moat_engine::pipeline::Error) -> Self;
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self;
}

/// The adapter result type.
pub type Result<T> = std::result::Result<T, Error>;

/// A reply to an already admitted operation. Independent of any async runtime.
/// Cancelling this future does not cancel other waiters or an accepted mutation.
#[must_use = "dropping a request discards its reply, not an accepted mutation"]
pub struct Request<T> {
    // Private fields omitted.
}

impl<T> Future for Request<T> {
    type Output = Result<T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}

/// Shared bytes from one physical engine read, with retained-buffer accounting.
/// Keeping this object alive retains both the pool buffer and its byte credits.
pub struct Chunk {
    // Private fields omitted.
}

impl Chunk {
    /// Reserves bounded long-lived retention, preserving at least one maximum
    /// read allocation globally and on this disk. Shared views charge once.
    /// The reservation lasts until the final chunk owner is dropped, including
    /// external owners after eviction. Failure does not invalidate this chunk.
    pub fn try_reserve_retention(&self) -> bool;
    /// Physical pool allocation retained by this chunk, including alignment.
    pub fn allocation_size(&self) -> usize;
    /// The completed logical record LSN at the time the read was submitted.
    pub fn lsn(&self) -> u64;
}

impl Deref for Chunk {
    type Target = [u8];
    fn deref(&self) -> &[u8];
}

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8];
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

/// The result of an unconditional or LSN-conditional deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteResult {
    /// A tombstone completed with this LSN.
    Deleted(u64),
    /// No live chunk existed when the operation reached its ordered position.
    Missing,
    /// A live chunk existed with a different LSN and was left unchanged.
    Changed,
}
```

### `core/moat-cache-store/src/store.rs`

[Source](../../core/moat-cache-store/src/store.rs)

```rust
/// Limits and I/O configuration for the adapter's per-disk workers.
#[derive(Debug, Clone)]
pub struct Options {
    /// Optional delivery on an application's existing executor. Batches read
    /// replies and fences to avoid one cross-thread scheduler wake per result.
    /// None delivers directly on I/O workers and needs no executor.
    pub completion_executor: Option<Arc<dyn crate::CompletionExecutor>>,
    /// Maximum admitted requests across all disks, including coalesced waiters.
    pub max_requests: usize,
    /// Maximum bytes retained by queued writes and pending/completed reads.
    /// Engine write buffers are separately bounded by each fixed I/O pool.
    pub max_bytes: usize,
    /// Per-worker queue and pool. At least sixteen maximum-size buffers are
    /// required; retained reads may use at most half of the configured pool.
    pub queue: QueueOptions,
    /// Queue backend. In-memory devices require explicit `Sync` on Linux.
    pub backend: QueueBackend,
    /// Idle wait between progress attempts. Zero enables busy polling.
    pub idle_wait: std::time::Duration,
    /// Optional CPU per disk worker; empty leaves placement to the scheduler.
    pub worker_cpus: Vec<usize>,
}

impl Default for Options {
    fn default() -> Self;
}

/// Stable disk geometry, in the order supplied to [`Store::new`].
#[derive(Debug, Clone, Copy)]
pub struct DiskInfo {
    /// Persistent disk UUID used for rendezvous placement.
    pub uuid: [u8; 16],
    /// Device capacity in bytes.
    pub capacity: u64,
    /// Engine segment size in bytes.
    pub segment_size: u64,
    /// Maximum encoded chunk size in bytes.
    pub chunk_max: u32,
    /// Default upper-layer live-entry limit; not a hard bound on engine index memory.
    pub index_entries: usize,
}

/// One completed live chunk, without a physical location or decoded cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryEntry {
    /// Disk index in this store's configuration.
    pub disk: usize,
    /// Opaque physical chunk identity.
    pub id: ChunkId,
    /// Completed record LSN.
    pub lsn: u64,
    /// Encoded chunk length.
    pub len: u32,
}

/// Adapter counters and currently charged resources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Statistics {
    /// Physical reads submitted to engines.
    pub physical_reads: usize,
    /// Logical reads joined to a compatible read operation.
    pub coalesced_reads: usize,
    /// Requests admitted but not yet completed.
    pub requests: usize,
    /// Charged queued-write and pending/retained-read bytes.
    pub bytes: usize,
    /// The pending/retained-read part of `bytes`.
    pub read_bytes: usize,
}

/// Cloneable, runtime-independent asynchronous access to exclusively owned
/// engine writers. Dropping the final store handle drains and seals workers
/// in the background; use [`Self::close`] to observe shutdown and its errors.
#[derive(Clone)]
pub struct Store {
    // Private fields omitted.
}

/// A chunk's placement within its originating store. The fixed disk set and
/// borrowed store make this reusable without repeating rendezvous hashing.
#[derive(Clone, Copy)]
pub struct ReadLocation<'a> {
    // Private fields omitted.
}

impl ReadLocation<'_> {
    /// Index in the originating store's disk list.
    pub fn disk(&self) -> usize;
    /// Admits a read using the already resolved placement.
    pub fn get(&self, range: Option<Range<u64>>) -> Request<Option<Arc<Chunk>>>;
}

impl Store {
    /// Whether this is the only handle to the adapter. A consumer that takes
    /// ownership can check this before maintaining an exclusive live catalog.
    /// Previously admitted requests may still be running; use inventory fences
    /// to collect their completed state after transferring the handle.
    pub fn is_unique(&self) -> bool;
    /// Acquires each disk's exclusive session and starts its worker. Returns the
    /// recovered live inventory before any adapter request is admitted.
    ///
    /// The caller owns formatting and passes disk handles. Recovery and pool
    /// creation run on each owner thread; duplicate ownership is rejected.
    pub fn new(engines: Vec<Disk>, options: Options) -> Result<(Self, Vec<InventoryEntry>)>;
    /// Immutable disk geometry. Disk list changes require explicit migration
    /// or rebuilding the cache; opening a different list does not migrate data.
    pub fn disks(&self) -> &[DiskInfo];
    /// The configured disk for a ChunkId, using persistent UUID-based placement.
    pub fn disk_of(&self, id: &ChunkId) -> usize;
    /// Resolves a read location tied to this store's fixed placement.
    pub fn locate(&self, id: ChunkId) -> ReadLocation<'_>;
    /// Current physical usage, for the upper layer's capacity controller.
    pub fn usage(&self, disk: usize) -> Result<Usage>;
    /// Conservative append allocation cost, including frame/footer overhead.
    pub fn write_cost(&self, disk: usize, len: u32) -> Result<u64>;
    /// Collects an approximate resource and operation snapshot.
    pub fn statistics(&self) -> Statistics;
    /// Reads a whole chunk or a range. Only identical range requests are
    /// coalesced, and only without an intervening mutation or disk barrier.
    /// Out-of-bounds range endpoints are clamped by the engine.
    pub fn get(&self, id: ChunkId, range: Option<Range<u64>>) -> Request<Option<Arc<Chunk>>>;
    /// Appends an explicit overwrite. Completion establishes engine visibility;
    /// power-loss durability additionally depends on explicit flush and the
    /// engine/device sync configuration.
    pub fn put(&self, id: ChunkId, value: Arc<[u8]>) -> Request<u64>;
    /// Deletes a chunk. When `expected_lsn` is supplied, a newer or different
    /// version is preserved and reported as [`DeleteResult::Changed`].
    pub fn delete(&self, id: ChunkId, expected_lsn: Option<u64>) -> Request<DeleteResult>;
    /// Takes a completed inventory after all previously admitted operations on
    /// this disk. Later operations wait until the snapshot is collected.
    pub fn inventory(&self, disk: usize) -> Request<Vec<InventoryEntry>>;
    /// The engine is append-only: physical reclamation is explicitly unsupported.
    pub fn reclaim(&self, disk: usize) -> Request<()>;
    /// Flushes all disks after earlier admissions, collecting every disk's
    /// outcome before returning the first error. Admission occurs on first poll.
    pub async fn flush(&self) -> Result<()>;
    /// Stops new admissions, drains accepted requests, seals and releases every
    /// worker, and returns the first failure after observing all workers.
    /// Subsequent close calls return Closed. Admission occurs on first poll.
    pub async fn close(&self) -> Result<()>;
}
```

## `moat-common` declaration inventory

### `core/moat-common/src/align.rs`

[Source](../../core/moat-common/src/align.rs)

```rust
/// The I/O and layout alignment unit: one 4 KiB page, which is also the logical
/// block size of every NVMe device moat targets.
pub const PAGE_SIZE: u64 = 4096;

/// Rounds `value` up to the next multiple of `align`.
///
/// `align` must be a power of two.
#[inline]
pub const fn align_up(value: u64, align: u64) -> u64;

/// Rounds `value` down to the previous multiple of `align`.
///
/// `align` must be a power of two.
#[inline]
pub const fn align_down(value: u64, align: u64) -> u64;

/// Returns whether `value` is a multiple of `align`.
///
/// `align` must be a power of two.
#[inline]
pub const fn is_aligned(value: u64, align: u64) -> bool;
```

### `core/moat-common/src/arena.rs`

[Source](../../core/moat-common/src/arena.rs)

```rust
/// Size of a 2 MiB huge page.
pub const HUGE_PAGE_2M: u64 = 2 << 20;

/// Size of a 1 GiB huge page.
pub const HUGE_PAGE_1G: u64 = 1 << 30;

/// How an [`Arena`] should try to obtain huge pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HugePages {
    /// Plain pages only.
    Disabled,
    /// Try explicit huge pages (1 GiB, then 2 MiB), then transparent huge
    /// pages, then plain pages. Never fails because of huge page availability.
    Preferred,
    /// Require explicit huge pages; fail if none can be mapped.
    Required,
}

/// What an [`Arena`] ended up backed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// Explicit 1 GiB huge pages.
    Huge1G,
    /// Explicit 2 MiB huge pages.
    Huge2M,
    /// Plain pages with `MADV_HUGEPAGE`; the kernel may promote them.
    Transparent,
    /// Plain pages.
    Plain,
}

/// A contiguous, page-aligned, zero-initialised memory region that never moves.
pub struct Arena {
    // Private fields omitted.
}

unsafe impl Send for Arena {

}

unsafe impl Sync for Arena {

}

impl Arena {
    /// Maps `len` bytes (rounded up to the page size the backing uses).
    pub fn new(len: usize, huge: HugePages) -> io::Result<Self>;
    /// The mapping length in bytes (a multiple of the backing page size).
    pub fn len(&self) -> usize;
    /// Whether the arena is empty. Always `false`; provided for API symmetry.
    pub fn is_empty(&self) -> bool;
    /// What the arena is backed by.
    pub fn backing(&self) -> Backing;
    /// The base address.
    pub fn as_ptr(&self) -> *mut u8;
}

impl Drop for Arena {
    fn drop(&mut self);
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}
```

### `core/moat-common/src/buf.rs`

[Source](../../core/moat-common/src/buf.rs)

```rust
/// A zero-initialised heap buffer whose address and length are both multiples
/// of [`PAGE_SIZE`].
///
/// Used for blocking direct I/O during formatting and recovery. Asynchronous
/// queues use [`PooledBuf`](crate::PooledBuf) from registered arenas.
pub struct AlignedBuf {
    // Private fields omitted.
}

unsafe impl Send for AlignedBuf {

}

unsafe impl Sync for AlignedBuf {

}

impl AlignedBuf {
    /// Allocates a zeroed buffer of `len` bytes.
    ///
    /// # Panics
    ///
    /// Panics if `len` is zero or not a multiple of [`PAGE_SIZE`].
    pub fn zeroed(len: usize) -> Self;
    /// Returns the buffer length in bytes.
    pub fn len(&self) -> usize;
    /// Returns whether the buffer is empty. Always `false`; provided for
    /// API symmetry with slices.
    pub fn is_empty(&self) -> bool;
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8];
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8];
}

impl Drop for AlignedBuf {
    fn drop(&mut self);
}

impl std::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}
```

### `core/moat-common/src/checksum.rs`

[Source](../../core/moat-common/src/checksum.rs)

```rust
/// The value checksum granularity: 64 KiB.
pub const CHECKSUM_BLOCK_SIZE: usize = 64 * 1024;

/// Computes the CRC32C (Castagnoli, RFC 3720) of `data`.
#[inline]
pub fn crc32c(data: &[u8]) -> u32;

/// An incremental CRC32C computation over several slices.
///
/// Produces the same value as [`crc32c`] over the concatenation of everything
/// passed to [`Crc32c::update`].
#[derive(Clone, Copy, Debug)]
pub struct Crc32c(/* private fields */);

impl Crc32c {
    /// Starts a new computation.
    pub fn new() -> Self;
    /// Feeds more bytes.
    pub fn update(&mut self, data: &[u8]) -> &mut Self;
    /// Returns the checksum of everything fed so far.
    pub fn finalize(&self) -> u32;
}

impl Default for Crc32c {
    fn default() -> Self;
}

/// Returns how many checksum blocks a value of `len` bytes has.
///
/// A zero-length value has zero blocks.
#[inline]
pub const fn block_count(len: u64) -> u32;

/// Computes the per-block checksums of `data`.
pub fn block_checksums(data: &[u8]) -> Vec<u32>;

/// Computes per-block checksums lazily, without allocating an output buffer.
///
/// Each item covers one logical [`CHECKSUM_BLOCK_SIZE`] block, including the
/// final partial block. An empty value produces no items.
#[inline]
pub fn block_checksums_iter(data: &[u8]) -> impl ExactSizeIterator<Item = u32> + '_;

/// Verifies `data` against `checksums`, where `data` starts at checksum block
/// `first_block` of the original value and must end on a block boundary or at
/// the end of the value.
///
/// Returns the index (relative to the value) of the first mismatching block.
pub fn verify_blocks(data: &[u8], first_block: u32, checksums: &[u32]) -> Result<(), u32>;

/// Like [`verify_blocks`], with the expected checksums supplied by a lookup
/// function (for example a view into an on-disk header, avoiding a copy).
pub fn verify_blocks_with(data: &[u8], first_block: u32, expected: impl Fn(u32) -> Option<u32>) -> Result<(), u32>;
```

### `core/moat-common/src/chunk_id.rs`

[Source](../../core/moat-common/src/chunk_id.rs)

```rust
/// The opaque 128-bit identifier of a chunk.
///
/// The chunkserver never interprets the bytes. Upper layers are free to encode
/// whatever they need (object hash, version, stripe index, ...) as long as the
/// identifier is unique for the content it names. A UUID is a natural choice:
/// it is exactly 128 bits, [`ChunkId::from_bytes`] accepts its bytes directly,
/// [`FromStr`] accepts both the hyphenated `8-4-4-4-12` form and 32 plain hex
/// digits, and with the `uuid` feature `From` conversions to and from
/// `uuid::Uuid` are provided.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChunkId(/* private fields */);

impl ChunkId {
    /// The number of bytes in a chunk identifier.
    pub const LEN: usize = 16;
    /// Wraps raw bytes as a chunk identifier.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self;
    /// Builds a chunk identifier from a `u128` (big-endian byte order).
    pub const fn from_u128(value: u128) -> Self;
    /// Returns the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 16];
    /// Returns the identifier as a `u128` (big-endian byte order).
    pub const fn to_u128(&self) -> u128;
    /// Returns a well-mixed 64-bit hash of the identifier.
    ///
    /// Identifiers are user supplied and may be poorly distributed (sequential
    /// counters, common prefixes). This mix is what index sharding and disk
    /// placement use, so every consumer sees the same uniform distribution.
    pub fn mix(&self) -> u64;
}

impl Hash for ChunkId {
    fn hash<H: Hasher>(&self, state: &mut H);
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result;
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result;
}

/// Error returned when parsing a chunk identifier from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseChunkIdError;

impl fmt::Display for ParseChunkIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result;
}

impl std::error::Error for ParseChunkIdError {

}

impl FromStr for ChunkId {
    type Err = ParseChunkIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err>;
}

#[cfg(feature = "uuid")]
impl From<uuid::Uuid> for ChunkId {
    fn from(uuid: uuid::Uuid) -> Self;
}

#[cfg(feature = "uuid")]
impl From<ChunkId> for uuid::Uuid {
    fn from(id: ChunkId) -> Self;
}

/// A [`Hasher`] that passes the pre-mixed [`ChunkId::mix`] value through
/// unchanged.
///
/// `ChunkId::hash` already produces a uniformly distributed 64-bit value, so
/// hash maps keyed by chunk identifiers do not need a second mixing pass.
#[derive(Default, Clone, Copy)]
pub struct ChunkIdHasher(/* private fields */);

impl Hasher for ChunkIdHasher {
    fn finish(&self) -> u64;
    fn write(&mut self, bytes: &[u8]);
    fn write_u64(&mut self, value: u64);
}

/// The [`std::hash::BuildHasher`] to use for maps keyed by [`ChunkId`].
pub type ChunkIdHashBuilder = BuildHasherDefault<ChunkIdHasher>;
```

### `core/moat-common/src/lib.rs`

[Source](../../core/moat-common/src/lib.rs)

```rust
pub mod align;

pub mod arena;

pub mod buf;

pub mod checksum;

pub mod chunk_id;

pub mod pool;

pub use align::{PAGE_SIZE, align_down, align_up, is_aligned};

pub use arena::{Arena, HugePages};

pub use buf::AlignedBuf;

pub use checksum::{
    CHECKSUM_BLOCK_SIZE, Crc32c, block_checksums, block_checksums_iter, block_count, crc32c, verify_blocks,
    verify_blocks_with,
};

pub use chunk_id::ChunkId;

pub use pool::{BufferPool, PoolOptions, PooledBuf};
```

### `core/moat-common/src/pool.rs`

[Source](../../core/moat-common/src/pool.rs)

```rust
/// Pool configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolOptions {
    /// Total bytes to map. Rounded up to a multiple of `max_class`.
    pub bytes: usize,
    /// Largest buffer the pool hands out (a power of two, at least one page).
    pub max_class: usize,
    /// Huge page policy for the arenas.
    pub huge_pages: HugePages,
}

impl Default for PoolOptions {
    fn default() -> Self;
}

/// A pool of page-aligned buffers. See the [module docs](self).
pub struct BufferPool {
    // Private fields omitted.
}

unsafe impl Sync for BufferPool {

}

unsafe impl Send for BufferPool {

}

impl BufferPool {
    /// Maps the arenas and creates an empty pool owned by the calling thread.
    pub fn new(opts: PoolOptions) -> std::io::Result<Arc<Self>>;
    /// The arenas backing the pool, for registration with io_uring or an RDMA
    /// device. The index into this slice is what [`PooledBuf::arena_index`]
    /// reports.
    pub fn arenas(&self) -> &[Arena];
    /// Total mapped bytes.
    pub fn capacity(&self) -> usize;
    /// Bytes currently handed out (class sizes, not requested sizes).
    pub fn in_use(&self) -> usize;
    /// Largest buffer this pool can allocate.
    pub fn max_class(&self) -> usize;
    /// Whether the calling thread is the pool's home thread.
    pub fn is_home(&self) -> bool;
    /// Size class a request of `len` bytes is served from.
    pub fn class_size(&self, len: usize) -> usize;
    /// Allocates a buffer of at least `len` bytes.
    ///
    /// Returns `None` if `len` exceeds the maximum class or the pool is
    /// exhausted; callers treat that as back-pressure, not as an error.
    ///
    /// # Panics
    ///
    /// If called from a thread other than the one that created the pool.
    pub fn alloc(self: &Arc<Self>, len: usize) -> Option<PooledBuf>;
}

impl std::fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

/// An owning handle to a pool buffer. Dereferences to its full class-sized
/// capacity; the memory is returned to the pool on drop (from any thread).
///
/// Contents are whatever the previous user left (the first bytes of a block
/// hold the allocator's free-list links or return-stack node while it is
/// free): callers overwrite what they use and must not rely on zeroes.
pub struct PooledBuf {
    // Private fields omitted.
}

unsafe impl Send for PooledBuf {

}

unsafe impl Sync for PooledBuf {

}

impl PooledBuf {
    /// Capacity in bytes (the size class, at least the requested length).
    pub fn capacity(&self) -> usize;
    /// Index of the arena this buffer lives in; matches
    /// [`BufferPool::arenas`] and is the fixed-buffer index for io_uring.
    pub fn arena_index(&self) -> u16;
    /// Byte offset of this buffer within its arena.
    pub fn offset_in_arena(&self) -> usize;
    /// The pool this buffer belongs to.
    pub fn pool(&self) -> &Arc<BufferPool>;
    /// Raw pointer to the start of the buffer.
    pub fn as_ptr(&self) -> *const u8;
    /// Raw mutable pointer to the start of the buffer.
    pub fn as_mut_ptr(&mut self) -> *mut u8;
}

impl Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8];
}

impl DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut [u8];
}

impl Drop for PooledBuf {
    fn drop(&mut self);
}

impl std::fmt::Debug for PooledBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}
```
