# Append-only device lifecycle

`moat-engine::engine::Engine<D, Q>` owns one device, one I/O queue, and an
in-memory index spanning segments. Normal reads and writes use the asynchronous
pipeline and registered buffers. Recovery, allocation, sealing, and orderly
shutdown are ticketed state machines using that same queue. Physical reclamation, segment reuse, and GC scheduling are not yet
exposed.

## Device layout

```text
0               4096            8192
| Superblock A  | Superblock B  | Fixed-stride segment slots ... |

One slot, S = segment_size:
| Allocation header | Frames ... | Unused gap | Metadata | Padding | Trailer |
| 4 KiB             |            |            |<------- Footer ----------->|
```

`SegmentHeader::segment_len()` is the complete physical stride, including header
and footer. The footer ends at the segment boundary, with its trailer in the last
64 bytes. See the [segment format](engine-segment-format.md) for fields and dual
CRC rules. Footers retain original frame metadata in full.

Superblocks use magic `MOATDEV1`, version `1`, and a whole-page CRC32C calculated
with its own field zeroed. All integers are explicitly little-endian.

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic |
| 8 | 4 | Version |
| 12 | 4 | CRC32C |
| 16 | 16 | Device identity |
| 32 | 8 | Device capacity |
| 40 | 4 | Physical segment stride |
| 44 | 4 | Number of complete slots |
| 48 | 4 | Maximum encoded frame length |
| 52 | 4 | Maximum value length |
| 56 | 8 | First allocation sequence |
| 64 | 4032 | Reserved, all zero |

Either valid copy can recover geometry; two valid copies must agree. Unsupported
versions, conflicting copies, and device truncation are errors. Formatting first
zeros and syncs both superblocks, then zeros and syncs each slot's allocation and
final pages, and finally writes and syncs each superblock separately. It neither
erases all payload nor issues discard. All alpha format versions are 1, with no
migration from previous layouts.

Formatting requires a fresh random 128-bit device identity. If an old layout is
readable, the new sequence range starts beyond both its reserved range and the
largest sequence in existing allocation headers. Invalid old allocation pages
cause an error before modifying the device. Without a readable layout, the fresh
identity determines the initial sequence. New slot `n` currently uses the initial
sequence plus `n`; slots are not reused. A future reuse protocol must keep sequences
unique across allocations. A valid footer CRC alone cannot establish ownership.

## Allocation and sealing

Before allocating a slot, the engine drains preceding writes and flushes and
seals the current segment. Outstanding reads retain their original immutable
segment routes and do not prevent rollover. The new allocation header must be written and synced before
submitting frames for that segment. It remains immutable for that allocation.

Sealing drains frame I/O, writes any footer prefix, syncs, writes the final page,
and syncs again. The final page contains both metadata and the trailer; there is
no separate seal-header page. The first sync makes data durable before the seal
record; the second makes that record durable before reporting success.

Lifecycle I/O failure disables further writes by that owner. The existing index
can still serve reads through a healthy queue; reopening determines recoverable
state. `Device` and `Queue` must refer to the same exclusively owned device, and
the queue must implement a persistence barrier for `Operation::Sync`.
`Device` positional I/O is used only by explicit offline formatting/layout tools.

`open` returns `(Engine, Ticket)` before recovery I/O. Drive `poll` until its
`Completion::Lifecycle { operation: Open, result, .. }` arrives. Index and geometry
queries return `NotReady` during recovery. `rollover`, `rollover_to`, `seal`, and
`close` also return tickets. A concurrent transition returns backpressure.
Explicit `*_blocking` helpers are available for tools; transition helpers require
no outstanding operations so they cannot swallow another caller's completion.

The first write, a full segment, or its metadata limit starts automatic rollover
and returns `Backpressure` with the caller's buffer. Drive `poll` and retry; no
write ticket was accepted. Automatic transitions have no public ticket and are
included in `in_flight`; failure makes the owner failed and rejects subsequent
writes. `OutOfSpace` means no unused slot remains. A pending flush prevents a
first write from allocating a segment until that barrier completes.

Rollover pauses writes, while reads continue subject to queue capacity. One queue
slot is reserved for lifecycle progress, including at depth one. `close` stops
admission, drains all accepted data operations, and seals/syncs before completing.
It returns backpressure during open or another transition; finish that transition
before closing. Premature Drop still uses the queue's buffer-safety drain, which
can block. A failed close does not guarantee a drained underlying kernel queue.
There is no background cleanup thread or unsafe early buffer release.

Fatal queue errors produce exactly one terminal completion per accepted ticket:
`Completion::Failed` for data operations and an error lifecycle result. OS-visible
buffers remain queue-owned until safe teardown. Normal read/write completions,
including operation-level errors, return the original buffers.

## Indexing, recovery, and verification

Full keys map to their latest LSN, kind, segment route, frame, and value location.
Tombstones remain indexed so older values cannot reappear. Physical traversal order
cannot override a higher LSN. The complete index resides in memory. `Options::resources` bounds indexed keys
(including tombstones), pending record metadata, frame buffers, and accumulated
segment metadata. `reserve_index` must fit the configured key bound. Pending new
keys are conservatively reserved before I/O and publication uses that capacity.
Metadata pressure rolls over early. Temporary in-flight reservation pressure
returns Backpressure; requests that cannot fit the configured bound return
ResourceLimit. Hash-table allocator overhead, growth copies,
and caller buffers are additional; these are logical quantity limits, not an
exact resident-memory cap. Segment routes are bounded by device geometry and the
metadata budget and grow with discovered allocations.

Every slot requires its allocation header and final 4 KiB. Recovery validates
allocation identity before comparing trailer generations. A valid same-generation
footer rebuilds the index directly. An older footer is ignored while current
allocation frames are scanned. A future generation or wrong identity is an error.
A single-page footer needs no third read; a larger footer uses one exact allocation
and reads only the prefix preceding the already loaded final page. A footer
larger than the runtime metadata bound falls back to frame scanning. Footer CRC
and frame validation yield between windows/frames, and all footer validation
finishes before its entries are published.

A torn trailer falls back to scanning with the immutable allocation header. If a
valid same-generation trailer accompanies a corrupt complete footer, recovery
retains the sealed `data_end`; corruption cannot silently downgrade the segment
to an active tail. Frame scanning verifies full payload CRCs. Active scanning stops
at incomplete or stale frames. Subsequent writes select a new unused slot instead
of appending to a recovered active slot.

`read(..., verify=false, ...)` trusts the index and reads the necessary data pages;
`verify=true` validates original frame metadata and intersecting logical checksum
blocks. Footer recovery is not a full payload integrity scan. This change neither
alters flush durability semantics nor introduces a device commit log.

## Optional device synchronization

The durability statements above assume `SyncMode::Enabled`, the default.
`FormatOptions::sync_mode` selects formatting barriers, and
`Options::sync_mode` selects the runtime owner's policy. `SyncMode::Disabled`
skips every device sync, including allocation, seal/rollover, explicit flush,
and close, while retaining write completion dependencies and failure propagation.
An explicit flush ticket then represents a write fence only. The policy is not
persisted or inferred from DIO/PLP; the caller selects it on each open.

## Validation scope

Temporary-file and in-memory-image tests cover header/footer layouts, both
checksums, page-boundary accounting, exact read sequences, LSN and tombstone
recovery, stale generations, future-generation rejection, I/O failures, torn tail
pages, sealed scan boundaries, and crashes that discard writes since the last
successful sync. Linux io_uring tests exercise registered buffers. Simulated
failures do not constitute physical power-loss certification.

## Cooperative progress and external runtimes

`Options::poll` supplies operation, byte, and record targets. Publication yields
between whole frames; recovery advances at most two lifecycle steps per call.
Footer encoding/checksumming uses 64-KiB windows. A frame, verified read, allocator
operation, or caller callback is indivisible, so these are cooperative targets,
not a hard wall-clock latency guarantee. Encoding and prepared-value checksums
still execute on the owner, bounded by the persisted/runtime frame limit.
`version_cursor` and `visit_versions_batch` bound traversal by key count, without
copying the index; new keys after cursor creation are excluded, while later
versions of existing keys may be observed.

`UringQueue::with_notifications` registers an eventfd and avoids `DEFER_TASKRUN`
so external readiness waits can drive progress. Call `poll(false)` and drain local
work while `has_ready()` is true before waiting on `notification_fd()`. Existing
queue constructors retain owner-driven polling. FileQueue and the adapter's Sync
backend remain deliberately blocking functional backends. Capacity queries,
queue construction/registration, formatting, and explicit blocking helpers are
synchronous setup/tool operations.
