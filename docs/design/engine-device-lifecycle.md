# Append-only device lifecycle

`moat-engine-v2::engine::Engine<D, Q>` owns one device, one I/O queue, and an
in-memory index spanning segments. Normal reads and writes use the asynchronous
pipeline and registered buffers. Allocation, sealing, and recovery use synchronous
positional I/O. Physical reclamation, segment reuse, and GC scheduling are not yet
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

Before allocating a slot, the engine drains pending operations and seals the
current segment. The new allocation header must be written and synced before
submitting frames for that segment. It remains immutable for that allocation.

Sealing drains frame I/O, writes any footer prefix, syncs, writes the final page,
and syncs again. The final page contains both metadata and the trailer; there is
no separate seal-header page. The first sync makes data durable before the seal
record; the second makes that record durable before reporting success.

Lifecycle I/O failure disables further writes by that owner. The existing index
can still serve reads through a healthy queue; reopening determines recoverable
state. `Device` and `Queue` must refer to the same exclusively owned device, and
`Device::sync` must persist completed queue writes.

`rollover`, `rollover_to`, and `seal` require a drained pipeline, otherwise they
return backpressure. Normal writes attempt rollover when a frame will not fit;
rejected inputs retain buffer ownership and contents. `OutOfSpace` means no unused
slot remains. Lifecycle operations pause admission and incur synchronous latency.

## Indexing, recovery, and verification

Full keys map to their latest LSN, kind, segment route, frame, and value location.
Tombstones remain indexed so older values cannot reappear. Physical traversal order
cannot override a higher LSN. The complete index resides in memory; `reserve_index`
reserves capacity without establishing a memory admission budget. Segment routes
grow with discovered allocations rather than reserving space for every unused slot.

Every slot requires its allocation header and final 4 KiB. Recovery validates
allocation identity before comparing trailer generations. A valid same-generation
footer rebuilds the index directly. An older footer is ignored while current
allocation frames are scanned. A future generation or wrong identity is an error.
A single-page footer needs no third read; a larger footer uses one exact allocation
and reads only the prefix preceding the already loaded final page.

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

## Validation scope

Temporary-file and in-memory-image tests cover header/footer layouts, both
checksums, page-boundary accounting, exact read sequences, LSN and tombstone
recovery, stale generations, future-generation rejection, I/O failures, torn tail
pages, sealed scan boundaries, and crashes that discard writes since the last
successful sync. Linux io_uring tests exercise registered buffers. Simulated
failures do not constitute physical power-loss certification.
