# moat-engine

The sole storage engine in this repository implements [unified immutable frames](../../docs/design/engine-frame-layout.md), owner-driven I/O, and an append-only multi-segment device lifecycle. Shared IDs, CRC32C, aligned memory, and pools come from `moat-common`.

`engine::SyncMode` controls explicit device persistence barriers. Set
`FormatOptions::sync_mode` when formatting and `Options::sync_mode` when opening
an owner; these are runtime choices and are not recorded in the device format.
`Options::default()` uses `SyncMode::Enabled`, preserving existing durability.
Server adapters propagate `storage::Options::sync_mode` to their engine owner.
This selects fsync/fdatasync barriers, not synchronous versus asynchronous APIs:
online operations retain their ticket/poll interface in both modes.

`SyncMode::Disabled` skips **all** engine-issued device syncs: format,
allocation, sealing, rollover, explicit flush, and close. Writes and lifecycle
dependencies still wait for successful I/O completion. Explicit `flush()` remains
an ordered write fence with a ticket and reports preceding write failures, but
does not submit a sync operation. Disabled mode is appropriate for disposable
cache contents or a backing device whose completion contract already guarantees
durability. It does not detect PLP or change kernel/device write-cache settings.
Without that backing guarantee, reinitialize disposable contents after an
unclean shutdown; disabled flush completion alone is not durability.

```rust,ignore
let sync_mode = moat_engine::engine::SyncMode::Disabled;
let options = moat_engine::engine::Options {
    sync_mode,
    ..Default::default()
};
// Use the same sync_mode in FormatOptions when creating the device.
let (engine, open_ticket) = moat_engine::engine::Engine::open_with_options(device, queue, options)?;
```

Server, cache-store, and cache use the engine through [`moat-server::storage`](../moat-server/src/storage/mod.rs). The legacy v1 implementation has been removed; the engine does not read its format. Physical reclamation and segment reuse remain unimplemented. See the [device lifecycle](../../docs/design/engine-device-lifecycle.md) and [migration guide](../../docs/design/engine-migration.md) for current boundaries.

## Usage

```rust
use moat_common::{AlignedBuf, ChunkId};
use moat_engine::frame::{Frame, FrameBuilder, FrameLimits, FramePosition};

// Format-wide bounds, not the runtime batching target.
let limits = FrameLimits::new(8 << 20, 4 << 20)?;
let position = FramePosition::new(17, 4096, 1 << 30)?;
let mut builder = FrameBuilder::new(limits);
builder.push(ChunkId::from_u128(1), 10, b"hello")?;
builder.push_tombstone(ChunkId::from_u128(2), 11)?;

let mut buffer = AlignedBuf::zeroed(builder.encoded_len());
let header = builder.encode_into(position, &mut buffer)?;
let frame = Frame::decode(&buffer[..header.frame_len()], limits, position)?;
assert_eq!(frame.value(0), Some(&b"hello"[..]));
assert_eq!(frame.value(1), None); // Tombstone, distinct from empty data.
# Ok::<(), moat_engine::frame::Error>(())
```

For a prepared value, allocate `PreparedFrame::required_len(limits, value_len)` bytes, borrow the buffer with `PreparedFrame::new`, fill `value_mut()`, then call `finish(position, key, lsn)`. The payload already occupies its final page-aligned region. Finishing computes checksums and writes metadata and padding without copying the payload. The caller's buffer remains available if finalization fails.

The codec accepts byte slices. The I/O layer must supply an aligned buffer address, retain the buffer until completion, and prevent modification after submission. `AlignedBuf` and registered buffers from `moat-common` provide suitable storage; the codec does not allocate or submit those buffers.

## Engine progress API

`Engine::open(device, queue)` returns `Result<(Engine, Ticket)>`. Drive
`poll(wait, &mut Vec<engine::Completion>)` until the open lifecycle completion
succeeds; layout/index queries return `NotReady` before that. `seal`, `rollover`,
`rollover_to`, and `close` return tickets through the same completion stream.
`open_with_options` configures `ResourceLimits` and `PollBudget`; explicitly named
blocking helpers support synchronous tools.

A first write or segment/metadata exhaustion starts allocation/rollover and returns
`Backpressure` with the original buffer. Poll and retry. Rollover drains writes
and flushes while admitted reads retain their original storage. `close` rejects
new work and drains/seals asynchronously; dropping early can still block to keep
kernel-visible buffers alive. Fatal queue failures terminate accepted tickets
with `Completion::Failed`, retaining buffers that cannot yet be safely released.

Resource limits include tombstones and pending metadata, independently of the
buffer pool. Poll budgets yield between frames; encoding and checksumming one
frame remain synchronous and bounded by the frame limit. Cursor traversal yields
between caller-selected key batches. For external event loops, use
`UringQueue::with_notifications`, `has_ready`, and `notification_fd`; notification
mode avoids deferred kernel task work. See the [lifecycle contract](../../docs/design/engine-device-lifecycle.md)
for limits, shutdown, recovery, and blocking-backend details.

## Persistent encoding

All integer fields are explicitly little-endian; there are no Rust layout casts or unsafe blocks. Frame starts and lengths are multiples of 4096 bytes. The magic is `MOATFRM1` and the version is `1`. The decoder rejects the original engine's batch encoding; this is not a migration reader. These constants identify frames; device superblocks have a separate magic and version.

| Header offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic |
| 8 | 4 | Format version |
| 12 | 4 | Header CRC32C, computed with this field zeroed |
| 16 | 8 | Segment allocation incarnation |
| 24 | 4 | Frame offset within segment |
| 28 | 4 | Page-rounded frame length |
| 32 | 4 | Record count, at least one |
| 36 | 4 | Directory length, exactly `64 * record_count` |
| 40 | 4 | Checksum area length |
| 44 | 4 | CRC32C of directory plus checksum area |
| 48 | 16 | Reserved, zero |

Each directory entry occupies 64 bytes:

| Descriptor offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 16 | Full chunk key |
| 16 | 8 | LSN |
| 24 | 4 | Frame-relative value offset |
| 28 | 4 | Value length |
| 32 | 4 | Frame-relative checksum offset |
| 36 | 4 | Checksum count |
| 40 | 1 | Kind: data `1`, tombstone `2` |
| 41 | 1 | Flags, currently zero |
| 42 | 22 | Reserved, zero |

Checksums follow the complete directory, consecutively in directory order. Each value has one CRC32C per logical 64 KiB block, including the final partial block. Empty data and tombstones have zero value/checksum offsets and zero checksums. Their kinds remain distinct.

Metadata is variable-length: `64 + 64 * records + 4 * checksum_count`. It is not padded to a dedicated page. Nonempty values start outside metadata at an 8-byte boundary and occupy non-overlapping contiguous ranges within the frame. The decoder supports arbitrary physical value order and arbitrary LSN order. It validates flags, reserved bytes, bounds, sentinels, and the complete checksum-area coverage.

Eight-byte value-start alignment is a fixed format invariant enforced by both construction and validation, with no configuration switch. The current CRC backend's small-value path can benefit from aligned `u64` processing without a bytewise prefix. Basic alignment adds 0–7 padding bytes before each nonempty value; value lengths are unchanged. Page-placement rules may introduce larger gaps. This choice does not imply a performance improvement for every value size or workload.

`FrameHeader::decode` checks the fixed header before callers allocate or read the declared extent. Geometry calculations use 64-bit arithmetic and are bounded before conversion to slice indices. `Engine` persists `FrameLimits` in the device superblocks, independently of runtime batching options. `FramePosition` checks segment incarnation and physical offset; the segment allocator must separately reserve footer space.

## Construction and ownership

`FrameBuilder` borrows values and stores a bounded record list. Successful admission guarantees that the records fit the format's frame limit. A failed admission preserves previously accepted records; a short output buffer or insufficient segment space leaves both the builder and destination unchanged. The caller may retry at a new position or with a larger buffer.

Finalization determines the directory size before choosing value offsets. Values are placed sequentially with 8-byte alignment. A value moves to the next page when that reduces its whole-value read page count; values of at least 64 KiB always start on a page. Empty records consume metadata only. Padding and reserved bytes are zeroed, including when reusing dirty buffers. No separate frame footer is written.

For values of 100 B, 4 KiB, 64 KiB, and 300 B:

| Record | Value offset | Value bytes |
| --- | ---: | ---: |
| A | 336 | 100 |
| B | 4096 | 4096 |
| C | 8192 | 65536 |
| D | 73728 | 300 |

The frame occupies **76 KiB**. The proposal's **72 KiB** example additionally fills an earlier alignment gap with D. The decoder accepts that arrangement, but the initial builder deliberately implements only sequential placement.

The common admission path uses a constant-time upper bound. Near capacity, it walks records for exact placement instead of prematurely rejecting a frame that fits. Encoding is linear and copies each borrowed payload once. The caller owns any earlier request staging; this codec's copy count does not imply that a future asynchronous write pipeline has no staging copy. Prepared values bypass payload assembly entirely. No API accepts caller-supplied checksums yet.

## Validation boundaries

- `Metadata::decode` needs only the header, directory, and checksum bytes. Ordered payload layouts allocate nothing; reordered layouts temporarily sort ranges to rule out overlap.
- `Record::verification_range` expands a requested range to complete logical checksum blocks. `Record::verify` requires the exact expanded bytes, so a short read cannot silently validate as a partial final block.
- `Frame::decode` additionally checks every value checksum before returning any accepted frame. A damaged later record rejects the entire frame. The segment recovery scanner uses this to accept complete frames only.

Verified reads must eventually fetch metadata and distant payload as separate extents, coalescing only when useful. For 32 records of 1 KiB each, metadata occupies 2240 bytes and the last value begins at offset 34816. The APIs let a reader validate the front metadata and that value without fetching intervening payloads. The pipeline now implements this read planning and I/O submission, including reusing payload bytes already fetched with metadata.

Checksums detect corruption; they do not make writes atomic or durable. The segment scanner implements prefix validation. The pipeline implements ordered publication and flush barriers; `Engine` implements allocation and sealing, while safe segment reuse remains unimplemented.

## Segment metadata and recovery

The [segment format document](../../docs/design/engine-segment-format.md) specifies exact header/footer fields, admission costs, recovery rules, and remaining persistence work. Segment errors live separately in `src/segment/error.rs`.

`SegmentHeader` binds device identity, segment number, and allocation incarnation. `SegmentBuilder::position` checks both the next frame and the growing footer; `append` records its validated metadata before I/O submission. This accounting includes allocated writes that have not completed. `seal_into` constructs a tail-anchored footer with a 64-byte trailer and returns a sealed in-memory segment view; it does not submit I/O or overwrite the allocation header.

The footer contains each frame's original metadata rather than a separate record summary. It reuses frame validation and retains the checksums needed by verified reads, at the cost of larger footer entries. `Footer::frames` returns borrowed metadata without reading payloads. `Scanner` validates payloads frame by frame, stops at a damaged active tail, and reports corruption before a sealed boundary. A bad footer can fall back to scanning with the sealed boundary preserved.

```rust
use moat_common::ChunkId;
use moat_engine::{
    frame::{FrameBuilder, FrameLimits, Metadata},
    segment::{Footer, SegmentBuilder, SegmentHeader, SegmentId},
};

let limits = FrameLimits::new(8 << 20, 4 << 20)?;
let id = SegmentId { device_id: [1; 16], segment_no: 0, sequence: 1 };
let active = SegmentHeader::new(id, 1 << 30)?;
let mut segment = SegmentBuilder::new(active)?;
let mut frame = FrameBuilder::new(limits);
frame.push(ChunkId::from_u128(7), 42, b"hello")?;
let position = segment.position(frame.encoded_len(), frame.metadata_len())?;
let mut bytes = vec![0; frame.encoded_len()];
frame.encode_into(position, &mut bytes)?;
segment.append(Metadata::decode(&bytes, limits, position)?)?;

// Construct metadata only. An I/O caller must persist frames and footer before
// writing the sealed header, and must retain all submitted buffers until done.
let mut footer_bytes = vec![0; segment.footer_len()];
let sealed = segment.seal_into(&mut footer_bytes)?;
let footer = Footer::decode(&footer_bytes, sealed, limits)?;
assert_eq!(footer.frames().next().unwrap().record(0).unwrap().descriptor().lsn, 42);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Segment codec tests use memory images; pipeline tests also use small temporary files and Linux io_uring. They establish validation, I/O ordering, and reservation behavior, not power-loss safety. `Engine` preserves the original allocation header, persists data and preceding footer pages, then writes and persists the final footer page containing its trailer. Recovery compares both allocation generations, reuses the initial tail-page read, and fetches only preceding bytes for a multi-page footer. Lifecycle tests cover failed calls, torn metadata, and discarding writes since the last simulated persistence barrier; they do not establish actual hardware power-loss behavior.

## Error contract

`frame::Error` and `frame::Result` are defined in `src/frame/error.rs` and re-exported from `frame`. The error is a non-exhaustive enum: callers match variants and numeric fields, with a fallback for future variants. Diagnostic strings and `Display` text are for humans and are not a stable parsing interface.

Capacity failures identify the resource that needs attention:

| Variant | Meaning and caller response |
| --- | --- |
| `ValueTooLarge { len, max }` | The logical value exceeds the format limit; changing buffers or flushing does not help. |
| `FrameFull { required, limit }` | The record set exceeds the encoded frame limit. Close a nonempty builder and retry the next record separately; reject a record that cannot fit by itself. |
| `SegmentFull { required, available }` | The supplied segment position lacks room; choose another position. This does not report device-wide free space. |
| `BufferTooSmall { required, available }` | The output buffer is too small; supply a larger buffer. |

Invalid arguments, incomplete input, unsupported versions, corrupt metadata, and payload checksum failures have separate variants. `Truncated` describes the supplied bytes, not an unconditional retry decision: an incremental reader may fetch more, while recovery must distinguish an unfinished active tail from damage before a sealed boundary. The codec therefore exposes no universal `is_retryable()` flag.

The design borrows OpenDAL's emphasis on actionable error categories and separate diagnostics. This codec expresses categories directly as enum variants; a second enum mirroring every variant would add no information. Its errors carry inline numeric fields and static strings, with no heap allocation or automatic backtrace capture. Tests bound the representation to 24 bytes without making that size a public ABI guarantee. `thiserror` generates the standard error and formatting implementations; it does not impose a boxed error representation or expose its own error type to callers.

The pipeline I/O boundary preserves the original `std::io::Error` as a source and attaches typed operation and physical-location context. Routine backpressure must stay cheap, and replay safety must depend on the operation's submission state. A generic string context collection, backtrace policy, or blanket retry flag is not introduced in this stage.

## Single-owner I/O pipeline

The [pipeline document](../../docs/design/engine-io-pipeline.md) describes ownership, publication, read verification, failure handling, and durability. `Pipeline<Q>` requires an exclusive queue and one segment whose initial metadata and format limits are already persisted by the caller. It does not allocate or reuse segments automatically.

On Linux block devices, `UringQueue` reads the maximum request byte size through `BLKSECTGET` at initialization and splits larger reads and writes into aligned SQEs. For a 128-KiB limit, a 4-MiB operation uses 32 SQEs, or 33 when its aligned extent includes another page. All SQEs reference disjoint ranges of the original buffer; splitting adds no payload copies, locks, or per-subrequest allocations. Requests are scheduled round-robin, with at most `depth` logical requests and `depth` in-flight SQEs. Even a depth-one queue can complete a larger operation over successive polls.

The caller receives one completion after every subrequest finishes, retaining the original token, offset, length, and buffer. Short transfers remain short; the first observed I/O error is retained while the other parts drain. Sync still requires preceding writes to complete. Regular files retain their existing transfer limit; `with_max_io_len(bytes)` can cap SQEs before submission, and `max_io_len()` reports the effective limit. Splitting at the byte limit does not prevent all kernel offload: segment-count limits, filesystem work, and sync may still require io-wq.

- `write(&builder, buffer)` encodes and submits borrowed records. `write_prepared(key, lsn, value_len, buffer)` submits an already filled prepared value without another payload copy.
- `poll(wait, &mut completions)` drives I/O and returns frame/read/flush results with reusable buffers. No channels, mutexes, or per-ticket atomics are needed inside the pipeline.
- `read_requirements(key, range, verify)` reports the needed buffer capacities. `read(key, range, verify, buffers)` selects verification per request. With `verify = false`, it trusts the published index and fetches only requested pages into `ReadBuffers::new(value_buffer)`, without CRC checks or metadata I/O. With `verify = true`, supply `metadata: Some(buffer)` as well; the pipeline validates metadata and complete intersecting checksum blocks. `buffers.view(result?)` exposes either result without copying. Empty unverified reads complete through `poll` without I/O, while still consuming a bounded operation slot.
- `flush()` waits for preceding writes and a data-sync operation. Write completion alone does not imply durability. A write or sync failure blocks further writes to the assigned allocation.
- `read_only` and `restore` accept recovered storage and scanner/footer metadata without authorizing new writes to the recovered tail.

`io::FileQueue` is a blocking functional backend. On Linux, `io::UringQueue::with_pool(file, depth, pool)` registers the shared `moat-common::BufferPool` arenas and uses fixed-buffer reads/writes for their buffers. `UringQueue::new(file, depth)` supports ordinary aligned buffers. Both constructors register the file, batch submissions, and request `SINGLE_ISSUER` with `DEFER_TASKRUN`; unsupported kernels fall back to a basic ring, observable through `deferred_taskrun()`. Registration failures remain errors. The index, operation slots, and write publication queue have a single mutable owner. `Engine::rollover_to` exposes caller-directed selection of unused slots; reclamation safety remains separate work.

`io::Buffer` owns either an `AlignedBuf` or a `PooledBuf`. Write methods and `ReadBuffers::new` accept either through `Into<Buffer>`; explicit `ReadBuffers` fields take `.into()`. Normal completion and rejection return the same allocation without copying its contents or cloning its pool owner. Fatal queue failures instead terminate tickets while retaining OS-visible buffers until safe teardown. A registered queue rejects buffers from another pool before I/O, while heap buffers use ordinary reads/writes. Registered storage stays alive until the ring is closed, and accepted requests are drained before their memory is released.

Create the pool and queue on the thread that drives I/O. `UringQueue` is neither `Send` nor `Sync`, enforcing the kernel's issuer constraint even when a particular kernel falls back to a basic ring. Allocate buffers during setup and recycle completions in the hot path. The queue adds no locks; pool allocation and release retain the shared allocator's existing accounting.

```no_run
# #[cfg(target_os = "linux")]
# fn registered_queue(file: std::fs::File) -> std::io::Result<()> {
use moat_common::{BufferPool, HugePages, PoolOptions};
use moat_engine::io::UringQueue;

let pool = BufferPool::new(PoolOptions {
    bytes: 64 << 20,
    max_class: 8 << 20,
    huge_pages: HugePages::Preferred,
})?;
let queue = UringQueue::with_pool(file, 64, pool.clone())?;
let buffer = pool.alloc(4096).expect("reserved pool capacity");
// Pass queue to Pipeline and buffer to write/read; recycle completion buffers.
# let _ = (queue, buffer);
# Ok(())
# }
```

Huge-page policy belongs to `PoolOptions`: `Disabled` uses ordinary pages, `Preferred` tries explicit huge pages and then transparent huge pages, and `Required` requires explicit huge pages. `Arena::backing() == Transparent` records a successful `MADV_HUGEPAGE` request, not proof that the kernel promoted the memory. Buffer registration works with each backing and requires sufficient locked-memory allowance. No global kernel settings are changed by the queue.

`examples/segment_io.rs` demonstrates the small file-backed write, verified-read, flush, and read-only recovery path. It creates a new file and never overwrites an existing path. This is a functional example, not a device formatter or performance workload.

## Subsequent engine boundaries

The upper layer decides which chunks to delete and controls segment selection, scheduling, placement policy, and maintenance budgets. The engine should expose segment statistics and execute explicitly requested physical reclamation, validating a segment handle that includes its allocation incarnation. It remains responsible for liveness revalidation, conditional index updates, persistence before freeing storage, and reader safety. A default victim-selection heuristic belongs in the caller's policy, not in the only engine execution entry point.

This stage does not fix the number of streams, encode Hot/Cold categories, or implement a reclamation scheduler. `SegmentId` supplies physical allocation identity; live statistics, reader pins, and reclamation execution remain later work.

## Review and validation

Start with `src/frame/header.rs` and `record.rs` for the wire format, `builder.rs` for placement and buffer ownership, and `decode.rs` for validation. `tests/frame.rs` covers mixed layouts, multi-page directories, empty values and tombstones, prepared-buffer identity, deterministic admission boundaries, reordered values, partial checksums, truncation, and forged structures with recomputed CRCs. A golden header and metadata vector was generated with an independent bitwise CRC32C encoder.

```sh
cargo test -p moat-engine
cargo clippy -p moat-engine --all-targets -- -D warnings
cargo bench -p moat-engine --bench frame
```

The benchmark measures in-memory assembly, full validation, and prepared finalization. It does not measure device throughput, recovery, or end-to-end latency, and does not establish an improvement over the old engine.

Pipeline functional checks can be run with `cargo test -p moat-engine --test pipeline -- --test-threads=1`. The [experiment archive](../../docs/experiments/README.md) preserves the measured revisions, complete samples, profiles, and ablation decisions. The active [engine driver](../../benchmarks/engine/README.md) and [application comparison](../../benchmarks/cache-disk/README.md) document their different measurement boundaries.
