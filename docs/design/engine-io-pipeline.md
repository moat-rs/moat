# Single-owner segment I/O pipeline

Status: implemented in `moat-engine-v2`. This stage connects frame construction,
segment admission, real file I/O, an in-memory index, optional read verification, ordered
write completion, and flush. It does not yet implement a complete device engine.
Measured comparisons and their limits are maintained in the
[benchmark reports](../../benchmarks/engine/README.md).

## Ownership and scope

`Pipeline<Q>` exclusively owns an I/O queue, one assigned segment, its index,
and all operation state. Methods require `&mut self`. No mutex, shared index,
channel, or atomic reference count is introduced into the pipeline. Independent
workers may own independent pipelines. Cross-worker routing is a caller concern.

The caller supplies trusted device geometry, persisted `FrameLimits`, the
segment's absolute device offset, and its validated `SegmentHeader`. A writable
pipeline requires a freshly allocated incarnation and an already durable active
header. A recovered pipeline is read-only and accepts metadata through `restore`
before serving reads. The assigned storage must not be modified or reused by
another owner. These are explicit preconditions, not properties inferred from
an active header or an open file descriptor.

Segment selection and rollover remain with the upper layer. This stage does
not format device superblocks, allocate incarnations, merge indexes across
segments, execute reclamation, or commit a footer trailer.
It does not reopen an active tail for new writes or overwrite a segment header.

## Bounded I/O and buffer ownership

The `Queue` contract bounds all accepted requests, including completed requests
not yet popped. Queue fullness returns the original `Request`. Requests own
`io::Buffer` storage (`AlignedBuf` or shared `moat-common::PooledBuf`); completion returns the same allocation. Callers can reuse
buffers without allocating on each admission. Buffers need not be shared across
threads or routed through a synchronized pool.

`FileQueue` executes positional file I/O at submission and defers completion
delivery. It is the portable functional backend, not an asynchronous backend.
`UringQueue` performs genuine asynchronous Linux I/O and batches staged SQEs at
poll time. Both expose actual transfer counts. A short transfer becomes an
explicit pipeline failure; no partial write is reported as a full frame.

`UringQueue::with_pool` registers the common pool's arenas once. Requests using
that pool use `READ_FIXED`/`WRITE_FIXED`; heap buffers use ordinary `READ`/`WRITE`.
A buffer from another pool is rejected before I/O, even if its arena index
matches. Both queue constructors register the file and use a completion ring
with twice the submission capacity.

The queue requests `SINGLE_ISSUER` with `DEFER_TASKRUN`, falling back to a basic
ring only when those options are unsupported. `deferred_taskrun()` exposes the
actual result. Deferred nonblocking polls enter with `GETEVENTS` to drive kernel
completion work. No background polling thread or queue mutex is introduced.
Create and drive the queue on the same thread; its type is neither `Send` nor
`Sync`. Queue creation and registration errors are returned to the caller.

Huge-page policy is configured through the shared `PoolOptions`, independently
of frame geometry and CRC policy. The queue retains the registered pool until
after the ring closes. Completion moves the buffer back without copying payload
bytes or cloning its pool owner. Arena allocation and pool accounting reuse
`moat-common`; v2 does not depend on the legacy engine. `Preferred` permits THP
or ordinary-page fallback, while `Required` fails without explicit huge pages.
An arena's `Transparent` backing indicates a hint, not guaranteed promotion.

Dropping the io_uring queue drains outstanding operations before freeing their
buffers. If the ring fails, synchronous cancellation is attempted. If cancellation
cannot establish safety, only possibly live buffer allocations are retained
rather than freed while the kernel may access them. This exceptional leak is
bounded by the accepted requests; it is not the ordinary completion path.

## Write path

```text
Check pipeline capacity and failure/flush state
  -> Select the next frame position with footer reservation
  -> Encode borrowed values, or finalize an already filled prepared value
  -> Validate front metadata and retain index changes
  -> Account for the frame in the segment builder
  -> Transfer the completed buffer to the I/O queue
  -> Receive CQEs, possibly out of order
  -> Publish completed frames in submission order and return buffers
```

One slot represents each admitted operation. A FIFO of write slot identifiers
controls publication. A later completed write retains its slot and buffer while
an earlier write is unresolved. This bounds out-of-order retention by queue
capacity without sorting completion lists or allocating a future/atomic per
record. Normal admission and completion bookkeeping use constant-time slot and
FIFO operations; encoding, metadata copying, and index application remain linear
in the frame's records.

`write` copies borrowed payloads into their final frame once.
`write_prepared` finalizes data already placed through `PreparedFrame::value_mut`
and submits that same allocation. It does not copy the value again. CRC32C is
computed during frame finalization; front metadata is checked before submission,
but payload CRC is not recomputed on successful write completion.

The index uses the existing `ChunkIdHashBuilder` from `moat-common`. It retains
full keys and applies only greater LSNs, independently of physical order.
Tombstones remain indexed so an older late-arriving value cannot resurrect a
key. Callers must assign a distinct LSN to each logical version; equal LSNs keep
the first occurrence. The pipeline does not assign versions or decide deletion
policy.

A write failure immediately blocks further writes to this allocation. Earlier
successful writes can still publish; the failed frame and all later dependent
frames report failure and do not update the index. Buffers return on both
success and failure. A fatal queue error instead makes the whole pipeline
unusable because completion state is unknown; the queue retains submitted
buffers until it can safely release them.

## Reads and verification

`read_requirements(key, range, verify)` reports metadata and value buffer capacities for the
currently indexed version without reserving a slot. Read admission checks those
capacities again, so a changed index cannot make a previously sized buffer unsafe.
A read captures the currently published index location at admission. Its result
remains a snapshot of that version even if a newer write publishes while the
read is in flight. Immutable storage and the prohibition on segment reuse make
this safe without reader locks or index revalidation loops. Future concurrent
reclamation will require its own pin/ownership protocol.

`read(key, range, verify, buffers)` selects verification per request and returns
one `Completion::Read` type in both modes. With `verify = false`, the pipeline
uses the indexed value address and length to read only the pages covering the
requested range. It does not issue a metadata read, decode front metadata, calculate CRCs, or
expand the range to 64-KiB checksum blocks. This trusts the index established by
successful writes or recovery and the caller's exclusive segment ownership.
It detects I/O failures and short transfers, but does not detect silent media
corruption or externally changed record identity. Writing and recovery retain
their checksum checks regardless of this per-read choice.

Unverified reads need only `ReadBuffers::new(value_buffer)`; their metadata
requirement is zero. If a caller supplies an optional metadata buffer anyway,
it returns untouched. Empty ranges consume a pipeline slot and complete on the
next `poll` without disk I/O. These ready completions suppress a blocking queue
wait, and their storage is bounded by the same operation depth.

With `verify = true`, the caller also supplies `metadata: Some(buffer)`.
The pipeline first reads the page-rounded metadata extent. It validates frame
identity, metadata CRC, geometry, and the indexed descriptor's key, LSN, kind,
value range, and frame geometry. It then verifies all complete logical checksum
blocks intersecting the requested range.

If those bytes are already in the metadata I/O buffer, the result references
that buffer directly: there is no second read or payload copy. Otherwise a
second aligned read fetches only the required value extent. It does not read
intervening values. The metadata buffer remains immutable during the payload
read, so payload completion reuses its validated view instead of recalculating
metadata CRC. Both buffers return to the caller, and `ReadBuffers::view` resolves
the result's `ReadRange` into a borrowed slice.

Empty verified ranges validate metadata only. Read I/O or checksum errors return all
buffers and do not poison unrelated writes. Resource or range rejection occurs
before an I/O request is submitted.

## Completion and durability

A successful write notification means its bytes completed and its records are
published. It does not claim persistence. `flush` reserves an operation slot,
stops new write admission, waits for preceding writes to complete and publish,
then submits a data-sync request. A successful flush notification establishes
that barrier; subsequent writes may be admitted again. Reads can continue
throughout the barrier.

If a preceding write fails, flush reports failure without issuing a misleading
sync. A failed sync also disables subsequent writes. Flush does not seal the
segment, rewrite headers, or establish device-level power-loss guarantees beyond
the supplied file/OS persistence operation and the caller's durable initial
metadata.

## Review and functional validation

Read `io/mod.rs`, `pipeline/mod.rs`, `pipeline/write.rs`, `pipeline/driver.rs`,
`pipeline/read.rs`, and `pipeline/verify.rs`, followed by the two queue implementations. Runtime errors
remain in `pipeline/error.rs`; index application is isolated in `pipeline/index.rs`.

`tests/pipeline.rs` uses small temporary files and deterministic queues for
out-of-order CQEs, short/error completions, flush ordering, backpressure,
buffer ownership, LSN/tombstone ordering, snapshot reads, separate extents,
prepared payload identity, and read-only restart. Linux tests exercise a small
real io_uring write/read/flush and drop with outstanding work.

These are functional checks, run serially. The separate benchmark harness
compares both verification modes and full-value or partial-range reads.
