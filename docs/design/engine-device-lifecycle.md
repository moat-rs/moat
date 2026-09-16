# Append-only device lifecycle

`moat-engine-v2::engine::Engine<D, Q>` owns one device, one I/O queue, and one
resident index across all allocated segments. The owner serializes mutation
through `&mut self`; ordinary reads and writes use the existing asynchronous
pipeline and registered buffers. No background thread, new mutex, or atomic
index coordination is introduced.

## Physical layout

```text
0             4096          8192
+-------------+-------------+-----------------------------------------------+
| superblock  | superblock  | fixed-stride segment slots ...                |
| copy A      | copy B      | trailing incomplete slot is unused            |
+-------------+-------------+-----------------------------------------------+

One physical slot (segment_size bytes):
+-------------+-----------------------+--------------------+----------------+
| allocation  | immutable frames      | footer at seal     | seal header    |
| header, 4K  | grows forward         | then unused space  | final 4K       |
+-------------+-----------------------+--------------------+----------------+
|<----- logical SegmentHeader.segment_len = segment_size - 4096 ---------->|
```

The logical extent excludes the final seal page. Existing frame and segment
encodings are unchanged. The footer is a **segment footer** containing each
frame's original metadata; frames do not have their own footer. Admission
reserves enough space for the growing footer before accepting a frame.

The immutable superblocks use magic `MOATDEV2`, version `1`, and a CRC32C over
the entire 4096-byte page with its checksum field zeroed. Fields are explicitly
little-endian:

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
| 64 | 4032 | Reserved, zero |

Either intact copy can restore geometry. Two valid copies must agree. Unknown
versions, conflicting geometry, and device truncation are errors. Format first
invalidates both roots and syncs, clears all allocation/seal pages and syncs,
then commits and syncs each root independently. It does not erase payloads or
issue discard. Formatting is destructive and requires exclusive device access.

Every format requires a fresh identity. With a readable prior layout, the new
sequence range begins after the old range. Without one, the first sequence is
derived from the fresh random identity. Callers must supply a fresh random
128-bit identity, including after an interrupted format; a deterministic or
reused identity can collide with old frame incarnations. Slot `n` uses the
persisted first sequence plus `n`. This version never reuses a slot.

## Allocation and sealing

1. Drain outstanding operations and deliver their completions.
2. To seal, write the footer and sync the device, persisting preceding frames.
3. Write the independent seal header and sync again. Keep the original active
   allocation header unchanged.
4. To allocate, write and sync a fresh allocation header before submitting any
   frame in the new slot.

An I/O failure during allocation or sealing prevents further writes from that
owner. Already published records remain readable where the queue is healthy.
Reopen determines the recoverable state. A torn seal can fall back to scanning
using the unchanged allocation header. A valid seal preserves its committed
boundary even if its footer is damaged; scanning damage before that boundary
is an error.

Lifecycle operations use synchronous positional I/O. `rollover`, `rollover_to`,
and `seal` require a drained pipeline, returning backpressure otherwise. Normal
writes attempt rollover when the next frame does not fit; rejected inputs retain
their original allocation. `OutOfSpace` means no unused slot remains. A frame
that cannot fit an empty segment is rejected without consuming more segments.

This deliberately keeps cold transitions simple. Each transition incurs sync
latency and pauses admission; full-device throughput includes this overhead.
It does not offer an asynchronous rollover latency guarantee. `Device` and
`Queue` must address the same exclusively owned storage, and `Device::sync`
must order and persist completed queue writes.

## Index, routing, and recovery

The index is a single-owner hash table mapping a full key to its latest LSN,
record kind, segment route, frame position, ordinal, value range, and metadata
length. Values stay on storage. A tombstone remains indexed to suppress older
versions during recovery. Physical scan order cannot override a newer LSN.
Each route stores its absolute base and allocation header, so a verified read
uses the original segment identity even after the writer has rolled over.

All indexed latest versions reside in memory. There is no paged disk index.
`reserve_index` reserves hash-table capacity but does not impose an admission
budget. Memory scales with distinct keys and tombstones, not only device bytes;
a device full of tiny distinct records can exceed available RAM.

Opening scans allocation and seal pages. Valid sealed footers rebuild the index
without reading payloads. Active allocations and damaged footers use the frame
scanner with payload CRC validation. Recovery accepts a valid active prefix and
never appends to its old tail. Subsequent writes allocate an unused slot. This
can leave unused capacity in recovered tails; exhausting all slots requires a
future explicit reclamation mechanism, not implicit overwriting.

`read(key, range, verify, buffers)` retains the existing policy: `false` trusts
the published location and reads requested pages without CRC or metadata I/O;
`true` validates metadata and intersecting logical checksum blocks. Writes and
active-tail recovery still compute or validate CRCs.

## Policy boundary and validation

Default rollover selects the next unused slot. `rollover_to(number)` permits
caller-selected unused slots and rejects occupied slots before sealing the
current one. There is no victim selection, segment recycling, or GC scheduler.
An upper layer remains responsible for placement and maintenance policy.

Small-file tests cover routing, registered prepared writes, both read policies,
nonmonotonic slot selection, LSN ordering and tombstones, reopen, full-device
rejection, pending reads, torn seal/footer recovery, and failure at every format,
allocation, and sealing I/O boundary. Failure injection and file corruption are
functional checks; they do not establish behavior of a particular device during
physical power loss.
