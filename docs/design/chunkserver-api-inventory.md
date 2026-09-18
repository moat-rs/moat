# Current chunkserver API inventory

Reviewed against the working source on 2026-09-17, including the asynchronous
engine lifecycle changes. This describes implemented Rust APIs, not historical
v1 APIs or proposed future interfaces.

The [declaration appendix](chunkserver-api-declarations.md) contains public
types, fields, variants, function signatures, explicit trait implementations,
and reexports extracted from the current source. Start with the tables below
for API review; use the appendix for exact parameter and return types.

## Surface and ownership map

Durability descriptions assume `engine::SyncMode::Enabled`. Formatting and owner
open each accept an explicit sync policy; server storage options propagate the
runtime selection. With `Disabled`, every engine-issued sync is skipped and
`flush` becomes a write-completion fence with error propagation. The runtime
choice is not persisted, and DIO/PLP does not automatically select it.

| Surface | Intended caller | Ownership and progress |
| --- | --- | --- |
| `moat_server::{Node, Worker, Handler, Context}` | A node application supplying its own request source | Workers exclusively own assigned sessions; application routes requests across owners |
| `moat_server::storage::{Disk, Session}` | An owner-thread application | `Disk` is a cloneable configuration/lease handle; `Session` is neither Send nor Sync and owns engine, queue, pool, and LSN allocation |
| `moat_engine::engine::Engine<D, Q>` | A storage integrator controlling I/O and buffers | Exclusive mutable owner; application drives `poll`, supplies LSNs, and matches completions |
| `moat_engine::pipeline::Pipeline<Q>` | A lower-level integrator supplying an allocated segment | Owns queue/index/publication; does not manage device allocation or sealing |
| `moat_engine::{frame, segment, io}` | Format tooling, custom backends, and advanced integrations | Codecs operate on caller memory; queue transfers buffer ownership until completion |
| `moat_cache_store::Store` | An application needing Future-based chunk access | Cloneable async adapter with one worker/session per disk; owns ordering and admission |
| `moat_common` | All layers | Chunk identity, aligned memory, pools, checksums, and alignment helpers |

There is no implemented TCP/RDMA/HTTP chunk RPC surface or network client API
in these crates. `Node` has no `get`/`put` methods or built-in request channel.
`Store` is an adjacent local async adapter, not a network chunkserver.

## Node, workers, placement, and discovery

Sources: [node](../../core/moat-server/src/node.rs),
[worker](../../core/moat-server/src/worker.rs),
[placement](../../core/moat-server/src/placement.rs),
[discovery](../../core/moat-server/src/disk.rs).

| Type/module | All inherent methods or free functions | Contract to review |
| --- | --- | --- |
| `Node` | `open`, `engines`, `owner_of`, `owners`, `disk_of`, `placement`, `assign_owners`, `set_owners`, `start` | `open` validates geometry; recovery happens when owners start. `engines()` returns `&[Disk]`, not Engine instances. No live membership migration |
| `Worker<H>` | `spawn`, `index`, `stop`, `join` | `spawn` waits for session setup/recovery. `stop` signals; `join` blocks and returns the handler or failure. Dropping a worker requests stop and joins |
| `Handler` | `start`, `run`, `stop` | `start` and `stop` have default implementations. `run` returns `Step::{Continue, Idle, Stop}`. Worker collects completions before `run` |
| `Context` | `owns`, `disk` | Public fields: `worker`, mutable `disks`, mutable `completions`; completions carry `(DiskId, Completion)` |
| `DiskSlot` | Public fields `id`, `session` | Access only to disks owned by this worker |
| `Placement` | `new`, `targets`, `len`, `is_empty`, `disk_of` | Weighted rendezvous over persistent UUIDs; `disk_of` returns `Option<usize>`. Zero-weight targets are excluded |
| `Target` | Public fields `uuid`, `weight` | Standalone Placement accepts caller weights; Node derives weights from device capacity |
| `disk` | `discover`, `cpus_of_node`, `online_cpus`, `parse_cpu_list` | NVMe discovery is Linux-only; non-Linux discovery reports unsupported |
| `NvmeDisk` | `is_available` | Fields: `name`, `path`, `serial`, `model`, `capacity`, `block_size`, `numa_node`, `in_use` |
| `InUse` | `Partitioned`, `Holder(String)`, `Mounted(String)`, `Swap` | Discovery's unavailable-device reasons |
| `worker` | `pin_to_core` | Unsupported outside Linux |

`assign_owners(0, ...)`, a wrong-length `set_owners` vector, and out-of-range
`owner_of` access can panic. Invalid owner indices are rejected by `start`.
`Handler::stop` runs before shutdown drains/seals sessions; completions produced
by that final drain are checked by the worker rather than delivered through a
later handler iteration.

## Storage adapter: devices, Disk, and Session

Sources: [storage](../../core/moat-server/src/storage/mod.rs),
[devices](../../core/moat-server/src/storage/device.rs).

| Type/module | All inherent methods or trait operations | Contract to review |
| --- | --- | --- |
| `storage::Device` | `capacity`, `read_at`, `write_at`, `sync`, `fd` | `Send + Sync + 'static`; synchronous device abstraction. `capacity() -> u64`; optional descriptor for async queue |
| `FileDevice` | `open`, `create` | `direct` is explicit; `create` creates/truncates a backing file |
| `MemDevice` | `new`, `with_data_mut`, `with_data`, `fail_writes_in` | In-memory backend and failure injection; synchronous queue support |
| `storage` | `format(device, &FormatOptions)` | Destructive formatting, returns `Result<()>`; caller supplies fresh device identity |
| `Disk` | `open`, `layout`, `index_capacity`, `usage`, `write_cost` | `open` does not recover the index. Lease prevents duplicate sessions sharing this handle, not independent handles/processes opening the same device |
| `Session` | `open`, `pool`, `stat`, `visit`, `in_flight`, `poll`, `write`, `read`, `flush`, `seal` | One engine, queue, and pool per owner; no public underlying Engine accessor |
| `storage` | `read_buffer(buffers, range)` | Consumes read buffers and returns `(PooledBuf, Range<usize>)` containing the result |

The complete Session data-path signatures are:

```rust
pub fn open(disk: Disk, options: &QueueOptions, backend: QueueBackend) -> Result<Self>;
pub fn pool(&self) -> &BufferPool;
pub fn stat(&self, id: &ChunkId) -> Option<(u64, u32)>;
pub fn visit(&self, visit: impl FnMut(ChunkId, u64, u32));
pub fn in_flight(&self) -> usize;
pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize>;
pub fn write(&mut self, id: ChunkId, value: Option<&[u8]>) -> Result<(Ticket, u64)>;
pub fn read(&mut self, id: ChunkId, range: Option<Range<u64>>) -> Result<Ticket>;
pub fn flush(&mut self) -> Result<Ticket>;
pub fn seal(&mut self) -> Result<()>;
```

- `write(Some(bytes))` appends an overwrite; `write(None)` appends a tombstone.
  The returned pair is `(ticket, assigned_lsn)`, not completed-write confirmation.
- Session assigns LSNs after recovering the highest indexed version, including
  tombstones. `stat` returns `(lsn, value_len)` for a published live record;
  `visit` visits live records only.
- A read captures published state, not a future pending write. Direct callers
  must serialize conditional/per-key operations themselves. Session does not
  provide cache-store's per-key operation chains.
- `read(None)` reads the whole value; explicit ranges use `u64`, reject reversed
  endpoints, and clamp endpoints to value length. A missing key is an error.
- Session allocates read/write buffers and uses the Disk's fixed read-verification
  option. There is no per-read verification override or caller-buffer overload.
- Every mutation submits one frame. The >=64 KiB prepared path still copies the
  input into the pool. There is no cross-request batching or public prepared-write
  overload at this layer.
- `seal` is blocking and requires a drained pipeline. Session has no public
  `close`, `rollover_to`, segment statistics, or reclaim operation.

## Device engine

Sources: [Engine](../../core/moat-engine/src/engine/mod.rs),
[Layout](../../core/moat-engine/src/engine/layout.rs),
[Device](../../core/moat-engine/src/engine/device.rs).

| API | Parameters and result | Contract to review |
| --- | --- | --- |
| `engine::format` | `(&impl Device, FormatOptions) -> Result<Layout>` | Destructive offline formatting; synchronous |
| `Engine::open` | `(D, Q) -> Result<(Self, Ticket)>` | Poll-driven recovery; device and queue address the same exclusively owned storage |
| `Engine::open_with_options` | `(D, Q, Options) -> Result<(Self, Ticket)>` | Runtime resource limits and cooperative poll targets |
| `Engine::open_blocking`, `open_blocking_with_options` | `(D, Q[, Options]) -> Result<Self>` | Explicit synchronous recovery helpers |
| `state` | `() -> State` | Opening, Ready, Closing, Closed, or Failed |
| `layout` | `() -> Result<Layout>` | Available after successful recovery |
| `active_segment` | `() -> Option<u32>` | Number only, not allocation incarnation |
| `allocated_segments` | `() -> usize` | Includes recovered active tails |
| `reserve_index` | `(additional: usize) -> Result<()>` | Preallocate within configured logical key bound |
| `indexed_versions` | `() -> Result<usize>` | Includes tombstones |
| `in_flight` | `() -> usize` | Accepted data operations plus active lifecycle job, including automatic rollover |
| `contains` | `(&ChunkId) -> Result<bool>` | Whether a published data record exists |
| `stat` | `(&ChunkId) -> Result<Option<(u64, u32)>>` | Absence is distinct from NotReady |
| `visit_versions` | `(impl FnMut(ChunkId, u64, Option<u32>)) -> Result<()>` | Synchronous full traversal; tombstones as None |
| `version_cursor` | `() -> Result<VersionCursor>` | Captures key boundary without copying index |
| `visit_versions_batch` | `(&mut VersionCursor, usize, callback) -> Result<bool>` | Bounded key batch; true when finished; may observe later overwrites |
| `poll` | `(bool, &mut Vec<engine::Completion>) -> Result<usize>` | Drives data and lifecycle; cooperative work targets |
| `has_ready`, `notification_fd` | `() -> bool`, `() -> Option<BorrowedFd>` | Drain local work before external waits; descriptor is backend-dependent and Unix-only |
| `read_requirements` | `(ChunkId, Range<u32>, bool) -> Result<ReadRequirements>` | Required buffers for published version |
| `read` | `(ChunkId, Range<u32>, bool, ReadBuffers) -> Result<Ticket, Rejected<ReadBuffers>>` | Explicit verification, caller buffers, strict ranges |
| `write` | `(&FrameBuilder, impl Into<Buffer>) -> Result<Ticket, Rejected<Buffer>>` | Batched records, caller LSNs, synchronous bounded encoding |
| `write_prepared` | `(ChunkId, u64, u32, impl Into<Buffer>) -> Result<Ticket, Rejected<Buffer>>` | Prepared value; synchronous bounded CRC, no extra payload copy |
| `flush` | `() -> Result<Ticket>` | Persistence barrier; does not allocate or seal |
| `rollover`, `rollover_to` | `() -> Result<Ticket>`, `(u32) -> Result<Ticket>` | Async drain writes, seal, allocate unused slot; reads can continue |
| `seal` | `() -> Result<Ticket>` | Async footer persistence |
| `close` | `() -> Result<Ticket>` | Stop admission, drain data operations, seal/sync; retry after existing transition |
| `seal_blocking`, `rollover_blocking`, `rollover_to_blocking`, `close_blocking` | `() -> Result<()>` or `(u32) -> Result<()>` | Explicit synchronous wrappers; require no outstanding operations |

`Options { resources: ResourceLimits, poll: PollBudget }` defaults to 64 MiB
frames/segment metadata, 1,048,576 indexed keys/pending records, and poll targets
of 64 operations, 1 MiB, and 4096 records. Targets yield between indivisible
frames/reads and do not bound allocator or callback latency. Resource limits are
logical quantities, not an exact RSS budget. See the [lifecycle contract](engine-device-lifecycle.md).

`engine::Completion` adds `Lifecycle { ticket, operation, result }` to the data
variants, where `Lifecycle` is Open, Seal, Rollover, or Close. `ticket` and
`into_pipeline` are its helpers; the latter discards lifecycle results and is
intended only for adapters that drive lifecycle operations separately.

The two-parameter `Result<T, Rejected<B>>` notation above means
`std::result::Result`; other results use the module's error alias.

`Layout` exposes `read`, `device_id`, `capacity`, `segment_size`, `segment_count`,
`limits`, and `segment_base`. `FormatOptions` exposes `device_id`, `segment_size`,
and `limits`. No `Default` is implemented for FormatOptions.

`engine::Device` exposes `capacity() -> io::Result<u64>`, `read_at`, `write_at`,
and `sync`. It is a different trait from `storage::Device`, has no `fd` method,
and is implemented for `std::fs::File`.

Native engine mutation order is resolved by caller-provided LSN, not a public
put-if-absent or compare-and-swap option. Deletes are records added through
`FrameBuilder::push_tombstone`; there is no standalone Engine `delete` method.
Writes needing allocation/rollover return Backpressure with their input; poll
and retry. The allocation is asynchronous and included in `in_flight`.
No unused segment yields `OutOfSpace`; reopening never resumes the old active
tail. A failed write lifecycle requires reopening before subsequent writes.

## Pipeline, completions, and I/O queues

Sources: [pipeline](../../core/moat-engine/src/pipeline/mod.rs),
[read](../../core/moat-engine/src/pipeline/read.rs),
[write](../../core/moat-engine/src/pipeline/write.rs),
[queue](../../core/moat-engine/src/io/mod.rs).

| Type | Public operations |
| --- | --- |
| `Pipeline<Q>` | `new`, `read_only`, `restore`, `read_requirements`, `read`, `write`, `write_prepared`, `flush`, `poll`, `in_flight`, `contains`, `configure`, `has_ready`, `notification_fd` |
| `Ticket` | `number`; ticket numbers are scoped to the issuing pipeline |
| `pipeline::Completion` | `ticket`; variants `Write { ticket, result, buffer }`, `Read { ticket, result, buffers }`, `Flush { ticket, result }`, `Failed { ticket, error }` |
| `ReadBuffers` | `new`, `view`; public fields `metadata: Option<Buffer>`, `value: Buffer` |
| `ReadRequirements` | Public fields `metadata_len`, `value_len` |
| `ReadRange` | `is_empty`; variants `Metadata(Range<usize>)`, `Value(Range<usize>)` |
| `Rejected<T, E>` | Public fields `error`, `input`; unaccepted input ownership is returned |
| `io::Queue` | `depth`, `vacant`, `try_submit`, `poll`, `pop`, `has_ready`, `notification_fd` |
| `io::Request` | Fields `token`, `operation`, `offset`, `len`, `buffer` |
| `io::Completion` | Fields `request`, `result: io::Result<usize>` |
| `io::Operation` | `Read`, `Write`, `Sync` |
| `Buffer` | `Heap(AlignedBuf)`, `Pooled(PooledBuf)`; From conversions and mutable byte-slice dereference |
| `FileQueue` | `new(file, depth)`; implements Queue, blocking functional backend |
| `UringQueue` | `new`, `with_pool`, `with_notifications`, `deferred_taskrun`, `max_io_len`, `with_max_io_len`; implements Queue, Linux-only |

Pipeline construction requires a previously persisted allocation/header and
format limits; `restore` is only for read-only startup before requests. Public
Pipeline APIs do not expose segment attach/detach, device rollover, or sealing.
Successful write completion publishes data but is not a persistence barrier.
Normal native read/write completions return owned buffers, including I/O errors.
Fatal queue errors instead produce `Failed` terminal events; OS-visible buffers
remain queue-owned until safe teardown.
Queue submission transfers ownership; a full queue returns the original request.

## Frame and segment codecs

Sources: [frame exports](../../core/moat-engine/src/frame/mod.rs),
[segment exports](../../core/moat-engine/src/segment/mod.rs).

| Type | All inherent methods |
| --- | --- |
| `FrameLimits` | `new`, `max_frame_len`, `max_value_len` |
| `FramePosition` | `new`, `segment_seq`, `offset` |
| `FrameHeader` | `decode`, `position`, `frame_len`, `record_count`, `metadata_len` |
| `FrameBuilder` | `new`, `push`, `push_tombstone`, `len`, `is_empty`, `metadata_len`, `encoded_len`, `encode_into`, `clear` |
| `PreparedFrame` | `required_len`, `new`, `metadata_len`, `value_mut`, `finish` |
| `Metadata` | `decode`, `as_bytes`, `header`, `record`, `records` |
| `Record` | `descriptor`, `checksum`, `verification_range`, `verify` |
| `Frame` | `decode`, `metadata`, `as_bytes`, `value` |
| `SegmentHeader` | `new`, `decode`, `encode_into`, `id`, `segment_len`, `is_sealed`, `footer_range`, `data_end` |
| `SegmentBuilder` | `new`, `position`, `append`, `data_end`, `metadata_len`, `footer_len`, `seal_into` |
| `FooterTrailer` | `decode`, `header` |
| `Footer` | `decode`, `frames` |
| `Scanner` | `new`, `position`, `next_frame`, `data_end`, `tail_error` |

Public data types also include `RecordKind::{Data, Tombstone}`,
`RecordDescriptor`, and `SegmentId { device_id, segment_no, sequence }`.
Public format constants are listed in the appendix.

These are memory codecs. `SegmentBuilder::seal_into` constructs bytes; it does
not persist a seal or free storage. `SegmentHeader::new` constructs an identity;
it does not allocate or authorize reuse. `Scanner` consumes supplied frame
bytes, not a device handle. The current engine still has no segment-statistics
enumeration or reclaim executor despite exposing SegmentId.

## Async chunk adapter (adjacent surface)

Sources: [Store](../../core/moat-cache-store/src/store.rs),
[requests and chunks](../../core/moat-cache-store/src/request.rs),
[completion executor](../../core/moat-cache-store/src/delivery.rs).

| API | Result / semantics |
| --- | --- |
| `Store::new(Vec<Disk>, Options)` | `Result<(Store, Vec<InventoryEntry>)>`; starts workers and returns recovered live inventory |
| `is_unique()` | `bool`; handle uniqueness, not a drain barrier |
| `disks()` | `&[DiskInfo]` |
| `disk_of(&ChunkId)` | `usize` |
| `locate(ChunkId)` | `ReadLocation`; caches routing for repeated reads |
| `usage(disk)` | `Result<Usage>` |
| `write_cost(disk, len)` | `Result<u64>`; conservative allocation estimate |
| `statistics()` | Approximate `Statistics` snapshot |
| `get(ChunkId, Option<Range<u64>>)` | `Request<Option<Arc<Chunk>>>`; missing data is `None` |
| `put(ChunkId, Arc<[u8]>)` | `Request<u64>`; unconditional overwrite, completed LSN |
| `delete(ChunkId, Option<u64>)` | `Request<DeleteResult>`; optional expected LSN |
| `inventory(disk)` | `Request<Vec<InventoryEntry>>`; per-disk barrier, live records only |
| `reclaim(disk)` | `Request<()>`; valid/open disk returns Unsupported |
| `flush().await` | `Result<()>`; global fence, observes all disks |
| `close().await` | `Result<()>`; stops admission, drains and seals; second close is Closed |
| `ReadLocation::{disk, get}` | Read through an already resolved placement |
| `Chunk::{try_reserve_retention, allocation_size, lsn}` | Retained-buffer accounting and logical version; Deref/AsRef expose byte slice |
| `CompletionExecutor::spawn` | Schedules `BoxFuture<'static, ()>` on an application executor |

`Request<T>` implements `Future<Output = Result<T>>`. Request-returning methods
admit at call time, before polling; cancellation discards the reply, not an
accepted mutation. The async `flush` and `close` methods admit on first poll.
Per-key operation chains serialize reads/mutations and coalesce identical ranges
without an intervening mutation/fence. Retained Chunk owners retain buffer
credits even after close. Dropping the last Store starts background draining;
explicit close is needed to observe failures.

`DeleteResult` is `Deleted(lsn) | Missing | Changed`. Inventory entries contain
`disk`, `id`, `lsn`, and `len`, with no physical segment location. Statistics
contain physical/coalesced read counts, request count, total charged bytes, and
read bytes. No Store `stat`, batch put, conditional put, caller-selected write
disk, or per-read checksum option exists.

## Configuration and defaults

| Type | Fields / defaults |
| --- | --- |
| `FormatOptions` | `device_id`, `segment_size`, `limits`; all explicit, persistent |
| `FrameLimits` | `new(max_frame_len, max_value_len)`; persistent bounds, not batching targets |
| `storage::Options` | `index_capacity = 1024`, `verify_reads = false` |
| `QueueOptions` | `depth = 64`, `pool = PoolOptions::default()` |
| `PoolOptions` | `bytes = 256 MiB`, `max_class = 8 MiB`, `huge_pages = Preferred` |
| `WorkerOptions` | `core = None`, default queue, `backend = Auto`; Busy on Linux, Adaptive with 1 ms idle sleep elsewhere |
| `PollMode` | `Busy`, `Adaptive { idle_sleep: Duration }` |
| `QueueBackend` | `Auto`, `Sync`, `Uring`; Auto chooses io_uring on Linux, Sync elsewhere |
| `cache_store::Options` | `completion_executor = None`, `max_requests = 4096`, `max_bytes = 256 MiB`, default queue, `backend = Auto`, `idle_wait = 50 us`, `worker_cpus = []` |

Session requires the pool maximum class to cover the persisted maximum frame
size. Store additionally requires at least sixteen maximum-class buffers per
disk and a global byte budget covering one such read. On Linux an in-memory
device requires explicit Sync; Auto does not provide an automatic synchronous
fallback for that device. Pool allocation is owner-thread-only, although returned
buffers may be transferred/dropped on other threads.

## Shared identity and memory APIs

The appendix includes all public common helpers, not only those needed by the
simple Session API:

- `ChunkId`: `from_bytes`, `from_u128`, `as_bytes`, `to_u128`, `mix`; string parsing,
  formatting, hashing, and UUID conversions. Also `ParseChunkIdError`,
  `ChunkIdHasher`, and `ChunkIdHashBuilder`.
- `AlignedBuf`: `zeroed`, `len`, `is_empty`; mutable byte-slice dereference.
- `BufferPool`: `new`, `arenas`, `capacity`, `in_use`, `max_class`, `is_home`,
  `class_size`, `alloc`.
- `PooledBuf`: `capacity`, `arena_index`, `offset_in_arena`, `pool`, `as_ptr`,
  `as_mut_ptr`; mutable byte-slice dereference and return-on-drop.
- `Arena`: `new`, `len`, `is_empty`, `backing`, `as_ptr`; `HugePages` and `Backing`.
- Checksums: `crc32c`, `block_count`, `block_checksums`, `block_checksums_iter`,
  `verify_blocks`, `verify_blocks_with`, `Crc32c::{new, update, finalize}`,
  `CHECKSUM_BLOCK_SIZE`.
- Alignment: `align_up`, `align_down`, `is_aligned`, `PAGE_SIZE`.

## Error surfaces

| Layer | Error variants (fields are in the declaration appendix) |
| --- | --- |
| Node | `Open`, `Worker`, `DuplicateIdentity`, `NoDisks`, `InvalidOwners` |
| Worker | `Io`, `Engine`, `Panicked` |
| Storage | `Busy`, `Invalid`, `Unsupported`, `Io`, `Engine` |
| Engine | `InvalidArgument`, `NotFormatted`, `Corrupt`, `UnsupportedVersion`, `OutOfSpace`, `NotReady`, `Closed`, `Failed`, `Aborted`, `IndexAllocation`, `Io`, `Pipeline`, `Segment`, `Frame` |
| Pipeline | `Backpressure`, `ReadOnly`, `WriteFailed`, `NotFound`, `InvalidArgument`, `Frame`, `Segment`, `Io`, `ShortIo`, `Queue`, `QueueFailed`, `ResourceLimit`, `MetadataFull`, `Allocation` |
| Frame | `InvalidArgument`, `ValueTooLarge`, `FrameFull`, `SegmentFull`, `BufferTooSmall`, `Truncated`, `UnsupportedVersion`, `Corrupt`, `PayloadChecksum` |
| Segment | `InvalidArgument`, `Sealed`, `Full`, `BufferTooSmall`, `Truncated`, `UnsupportedVersion`, `Corrupt`, `Frame`, `Allocation` |
| Cache-store | `Busy`, `Closed`, `Invalid`, `Engine`, `Io` |

The same conceptual failure may be nested through several layers. Storage maps
pipeline Backpressure to Busy; its Busy also covers unavailable pool buffers
and an already-owned Disk. Frame, segment, engine, and pipeline error enums are
non-exhaustive. Diagnostic strings are not stable machine-readable contracts.

## Review decisions exposed by this inventory

1. Which API should be the supported application entry point: Session, native
   Engine, or a general-purpose Future adapter? Capabilities differ today.
2. Should `write(None)`, unnamed `(lsn, len)` / `(ticket, lsn)` tuples, and the
   `engines()` name become explicit operations/result types?
3. Which native controls need to cross Session: frame batches, prepared buffers,
   per-read verification, caller placement, and conditional mutations?
4. Should range clipping and missing-key behavior be consistent across layers?
5. Which synchronous Session setup/shutdown wrappers should expose the native
   engine lifecycle tickets to higher layers?
6. How should segment enumeration/statistics and explicit
   `SegmentId`-based reclaim be exposed? This is already a documented design
   boundary, but no executable reclaim API currently implements it.
7. What resource limits must be hard: indexed versions including tombstones,
   admitted operations/bytes, retained read buffers, and maintenance reserves?
8. Which configuration mistakes should return typed errors instead of panicking?
9. What shutdown and cancellation guarantees should each entry point provide?

Not currently implemented: physical reclaim/reuse; an exact RSS cap;
cross-request Session batching; multi-chunk object manifests; network transports;
live disk membership migration; or a public reclamation scheduler. These are
capability gaps, not additional APIs present in this snapshot.
