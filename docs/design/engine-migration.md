# Engine migration and future rebuild boundaries

## Current implementation

`moat-engine` is the only storage engine. The v1 crate, benchmark adapter,
and selection paths have been removed. Historical experiments retain their
original results; reproducing v1 requires the revision identified in each report.

The migration preserves the roles of server, cache-store, and cache. The
replaceable transition adapter lives in `moat-server::storage` and gives each
owner exclusive access to an engine.

| Layer | Responsibilities | Outside its scope |
|---|---|---|
| `moat-engine` | Format, index, I/O, recovery, allocation, rollover, flush, and seal | Threads, request channels, and cache policies |
| `storage::Disk` | Device configuration, persistent geometry, exclusive lease, and allocation snapshots | Shared engine or index access |
| `storage::Session` | One owner's engine, queue, pool, LSN allocation, buffers, and request conversion | Cache keys, logical eviction, and GC |
| Server worker | Drive assigned sessions and receive native completions | Direct readers across owners |
| Cache-store | Per-key ordering, bounded admission, read coalescing, conditional deletion, fences, and async delivery | Another physical index or storage format |
| Cache | Key identity, logical catalog, shared views, conditional population, and logical eviction | Segment reuse |

`Disk` is cloneable, but handles sharing its lease can create only one active
`Session`. The session and pool are created, used, and destroyed on their owner
thread; the session type prevents transfer to another thread. Callers must
still prevent separately created handles from opening the same physical
device. The lease does not replace cross-process file locking.

## API and behavior changes

- `Disk::open` validates geometry. Index recovery happens in `Session::open`
  during worker startup. Node no longer returns the old `RecoveryReport`.
- `Context` exposes only the current worker's `DiskSlot` values. Session lookup
  uses global disk IDs; requests for another owner require explicit routing.
  Each disk has its own queue and pool, and pool capacity is configured per disk.
- The pool's largest buffer class must cover the persistent maximum frame size.
  An undersized configuration fails at startup.
- `Session::write` returns a native ticket and allocated LSN. Deletions append
  tombstones. Recovery visits the latest versions, including tombstones,
  to keep LSN allocation monotonic without duplicating the physical index.
- Reads coalesce only for the same key and range without an intervening mutation
  or fence. Completion retains the pooled buffer containing the result and
  immediately releases the other metadata/value buffer. Returning data adds
  no payload copy.
- Flush always issues a persistence barrier, including immediately after open,
  and does not allocate a segment. Shutdown drains and seals before releasing
  the lease. Partial startup failure waits for started workers to clean up.
- A engine write-path failure requires close and reopen before further writes.
  Previously published data remains readable. Reopen does not reuse the old
  active tail.

## Remaining limitations

1. **No legacy-format reads or automatic data migration.** Changing dependencies
   does not convert a v1 device. Existing data needs a separate migration or
   cache rebuild; formatting it is not data migration.
2. **No physical GC or segment reuse.** `Store::reclaim` returns `Unsupported`.
   Logical eviction changes visibility only. Cache returns `NoSpace` when
   append headroom is exhausted, without repeatedly invoking GC or deleting
   more live entries in an attempt to reclaim unavailable physical space.
   Existing entries remain readable; callers must arrange cache rebuilding.
3. **Logical resource bounds are not an exact RSS cap.** Native engine options
   bound indexed versions (including tombstones), pending metadata, frame buffers,
   and segment metadata. Allocator overhead and caller-owned buffers are additional.
4. **One frame per mutation in the transition adapter.** Large values use
   prepared buffers; small values are not batched across requests. Small-write
   throughput and space amplification cannot inherit v1 packing assumptions.
   The native benchmark directly uses the engine's batching APIs.
5. **Adapter setup/shutdown wrappers remain synchronous.** Native open, rollover,
   seal, and close are ticketed and poll-driven. Session startup and its seal helper
   explicitly drive blocking wrappers. A Sync queue backend still blocks on I/O.
6. **Reopen consumes new segments.** The engine does not continue appending to an old
   active tail, even when that tail has unused space.

## Future rebuild priorities

First implement safe physical reclamation and segment reuse, covering read pins,
tombstone lifetimes, and crash recovery. Then
rebuild owner scheduling and routing, followed by frame batching and prepared
buffer retries. Adjust cache capacity controls using measurements of those paths.

The retained tests for per-key ordering, cancellation, conditional deletion,
read coalescing, and shared-buffer lifetimes provide the behavioral baseline
for later replacement. Keep the transition adapter replaceable instead of
expanding it to emulate the old shared reader/writer API.
