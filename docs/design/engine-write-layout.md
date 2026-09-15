# Engine write layout: open inline pages and unified value extents

Status: earlier proposal and executable recovery model. The
[unified immutable Frame proposal](engine-frame-layout.md) supersedes this
document's recommended default. This document remains a reference for the
page-rewrite fault model; its prototype does not validate the new Frame
encoding. No engine layout, writer behavior, device capability, or
format-version constant has changed.

The project is alpha. The eventual implementation may break compatibility without a version increment or migration support. Existing media would need reformatting; this proposal does not perform or authorize device formatting.

## Proposed direction

Replace the three persistent batch kinds with two storage primitives:

- **Inline page:** one 4 KiB page containing very small records. Keep its RAM image open, append records into unused bytes, and close it when the next record does not fit. Rewriting the same disk page requires the explicit storage contract described below.
- **Value extent:** one immutable, page-multiple write containing a compact record directory and one or more values. This handles both the current Framed and Large cases. Value alignment is a placement decision, not a separate batch format.

The segment allocates both primitives sequentially. An inline page reserves exactly one page, not a megabyte-sized batch region. A later update to that open inline page keeps its location; a new value extent is allocated at the segment tail. The index records each value's exact location, length, and LSN.

```text
Allocation order:
  Segment header | Inline page P | Value extent E | ... next allocation ...

Write order:
  P: [A]                -> write page P
  E: [V1, V2]           -> append extent E
  P: [A, B]             -> rewrite page P, preserving A's bytes
  P: [A, B, C, ...]     -> rewrite page P until it cannot fit another record
  Q: [next small key]   -> allocate another inline page at the segment tail
```

This matches continued appends at the same physical location for very small values. The benefits of Framed and Large remain as alignment and landing-buffer optimizations. They do not require separate persistent batch kinds or separate pending pools.

The earlier contiguous-batch alternative reserves a large region for each kind and appends immutable frames inside it. It can preserve existing writes without page rewrites, but leaves unused reservations and adds allocation headers. It is not the recommended default if the actual continuity requirement is a single open inline page.

## What is guaranteed, and under which fault model

Two different statements must not be conflated:

1. If writes leave an old/new mixture of bytes, and the old record prefix is byte-for-byte unchanged in every submitted image, a torn rewrite cannot change that prefix. Per-record CRCs allow recovery to retain it even if the newly appended suffix is incomplete.
2. If rewriting a page can damage previously stored bytes or make the page unreadable, those CRCs can detect damage but cannot recover the old data. The implementation needs an independent copy or a stronger device contract.

The positive inline-rewrite model assumes the first statement: each byte of a torn rewrite comes from either the old or the new image, and I/O targets only the requested page. This is weaker than requiring an atomic whole-page write, but it is still a storage contract. It must not be inferred from 4 KiB alignment, `O_DIRECT`, successful I/O completion, or the presence of CRCs. The current `Device` trait does not express this capability.

The first production implementation must make a deliberate choice:

| Mode | Open RAM page | Same physical page rewritten | Required guarantee / cost |
| --- | --- | --- | --- |
| Contract-backed rewrite | Yes | Yes | Backend explicitly guarantees nondestructive old/new tearing; writes to a page are serialized |
| Immutable snapshots | Yes | No; snapshots use new locations | Retain older copies; extra allocation, write, and reclaim cost |
| Submit once | Until first submission | No | Simpler conservative baseline; does not retain physical-page appendability after submission |

Do not silently enable contract-backed rewriting for all files or block devices. Identifying a supported backend and proving its failure behavior is an implementation gate, not something demonstrated by this Python model.

Ordinary write completion and power-loss durability remain distinct. A durability barrier must cover previous header and data writes. The proposal does not strengthen the existing default PUT durability promise. Bit rot affecting an already durable record is corruption, not an incomplete new suffix; sealed metadata with a known committed boundary must make that an error.

## Inline page format

The page has a 64-byte immutable header followed by individually checksummed records. There is no mutable page-wide count, tail, or whole-page CRC whose failure would invalidate all old records.

| Header offset | Bytes | Field |
| --- | --- | --- |
| 0 | 8 | Inline-page magic |
| 8 | 4 | Header CRC32C |
| 12 | 4 | Reserved, zero |
| 16 | 8 | Segment incarnation |
| 24 | 8 | Page offset within the segment |
| 32 | 32 | Reserved, zero |

The header CRC covers the 64-byte header with its own field zeroed. Rewriting an open page never changes this header. Identity must match the enclosing segment and page position.

Each record begins on an 8-byte boundary:

| Record-relative offset | Bytes | Field |
| --- | --- | --- |
| 0 | 8 | Record magic |
| 8 | 4 | Encoded record length, rounded to 8 B |
| 12 | 4 | Value length |
| 16 | 8 | LSN |
| 24 | 16 | Key |
| 40 | 1 | Data or tombstone |
| 41 | 7 | Reserved, zero |
| 48 | Value length | Value |
| 48 + value length | 4 | Record CRC32C |
| Afterwards | 0 to 7 | Zero padding to 8 B |

The CRC covers a binding of segment incarnation, page offset, and record offset, followed by the entire encoded record with its own CRC field zeroed. This prevents an old record suffix from another segment incarnation from validating under a new page header.

New records are written after the existing prefix. No existing record moves, and overwrites/deletes append a new version or tombstone instead of editing an old record. Each record fits wholly within the page. An oversized record falls back to a value extent.

Example with two 100 B values, using page-relative ranges:

```text
Header           [0, 64)
Record A header  [64, 112)
Value A          [112, 212)
CRC A            [212, 216)
Record B header  [216, 264)
Value B          [264, 364)
CRC B            [364, 368)
Unused           [368, 4096)
```

After A has been submitted, adding B leaves `[0, 216)` unchanged. Recovery validates A independently, then attempts B. A damaged B does not make A invalid. Recovery derives the used prefix by parsing records; it does not need an overwritten count or tail pointer.

The 4 B per-record CRC is useful here even though normal reads do not verify it: it is the independent recovery boundary for a mutable page suffix. This is a different purpose from retaining one checksum per 64 KiB of a large immutable value.

### Buffer ownership and completion

Keep a working RAM image and a frozen submitted image. Do not modify a pooled buffer after handing it to the asynchronous queue. A second image or copy is necessary if new records are accepted while a page write is in flight.

Permit at most one write in flight per inline page. Track the record boundary included in that snapshot and complete only tickets included in a successful snapshot. Newer records remain pending until a later write succeeds. Reordered writes to the same location must not allow an older snapshot to overwrite a newer one.

Existing readers may refer to old record offsets while a rewrite happens. Under the old/new contract their requested value bytes are unchanged, and overwrites append new versions elsewhere in the page. Diagnostic verification must validate only the target record, not the unstable whole-page suffix. The implementation still needs explicit concurrent-reader tests; the recovery prototype does not model concurrent reads.

## Unified value extent format

An extent is immutable after submission and occupies an integer number of pages:

```text
64 B extent header | actual record directory | values and placement padding | final zero padding
```

A directory entry stores key, LSN, value length, value offset, alignment and record kind. Normal reads use the rebuilt/in-memory index directly. Metadata need not immediately precede each value.

| Extent-header offset | Bytes | Field |
| --- | --- | --- |
| 0 | 8 | Extent magic |
| 8 | 4 | Header CRC32C |
| 12 | 4 | Extent length |
| 16 | 8 | Segment incarnation |
| 24 | 8 | Extent offset within the segment |
| 32 | 4 | Record count |
| 36 | 4 | End of the actual record directory |
| 40 | 4 | End of used value data |
| 44 | 4 | Data-area CRC32C |
| 48 | 16 | Reserved, zero |

Header CRC covers the header with its own field zeroed, including the stored data CRC. Data CRC covers every byte after the header through the extent end: directory, values, and padding. Validate header and all bounds before accessing or allocating for the body. There is no separate physical trailer page.

The CRC need not be physically last to detect a partial write. It may arrive before the body, but recovery accepts records only after the complete body checksum and all geometry validate. This does not make the write atomic or prove persistence ordering.

Each 64 B directory entry is encoded as:

```text
magic:8 | lsn:8 | key:16 | value_len:4 | value_offset:4 |
alignment:4 | kind:1 | reserved:19
```

The directory is sized from the actual record count, not pool capacity. Values are assigned positions after that directory. Offsets are explicit, so changing placement configuration does not require recovery to guess the original configuration.

### Alignment policy

Keep placement configurable independently of persistent layout kind:

- Pack very small values when an extent is used for them.
- For a candidate offset, move a value to a page boundary if that reduces the pages spanned by the whole value.
- Force page alignment for prepared landing buffers or large range-readable values, even when packing would save a page of total storage.

The prototype uses an 8 B packed alignment, page alignment when it reduces whole-value page count, and forced page alignment at 64 KiB or when explicitly requested. These are demonstration choices, not benchmark-selected defaults. Minimizing pages for a whole value does not necessarily minimize pages for arbitrary range reads.

Three values of 100 B, 4090 B, and 65536 B can share one extent:

| Region | Extent-relative range |
| --- | --- |
| Header and three directory entries | `[0, 256)` |
| Value A | `[256, 356)` |
| Alignment padding | `[356, 4096)` |
| Value B | `[4096, 8186)` |
| Alignment padding | `[8186, 8192)` |
| Value C | `[8192, 73728)` |

The hybrid writer would normally route the 100 B value to an inline page. The example establishes that alignment does not require three batch encodings.

A single large value can still use a prepared, page-aligned landing buffer and a one-record extent. That is an optimized submission path through the same format. It can submit immediately rather than copying a large buffer just to combine it with unrelated small values. Copying/packing decisions and persistent layout kinds should remain separate.

## Segment scan, index, and reclaim

The sequential allocator advances by one page for an inline page and by `extent_len` for a value extent. Updating an existing open inline page does not advance the allocation tail or touch later allocations.

Recovery validates a primitive's header before trusting its type and bounds:

1. For an inline page, validate records until the first incomplete/invalid record, accept the valid prefix, then advance exactly one page and inspect the next primitive.
2. For an extent, validate the entire directory/data region before accepting any record. A corrupt/incomplete active tail extent ends the sequential scan; do not search arbitrary payload bytes for a plausible next magic value.
3. Merge entries by LSN, rather than physical position. A later append to an earlier inline page can have a larger LSN than an extent located after it. Preserve tombstone rules.
4. Seal recovered active segments and start on fresh storage. This proposal does not reuse an unknown torn suffix from a previous process.

An inline page header is fixed-size and immutable, so a bad new record does not conceal later value extents. A damaged inline header or an extent header with unknown length is different: recovery cannot safely guess its boundary from untrusted data. Before a known sealed committed boundary, that is corruption.

Index publication follows successful snapshot/extent completion. The index records direct value locations. Optional diagnostics need a record location for Inline or an extent offset/length for a value extent. Two additional `u32` fields appear to fit unused space in the current 64 B index slot, but implementation must assert its size and update every seqlock load/store path.

Sealed footers must record each inline page's committed record boundary, plus extent bounds and record entries. A fallback scan must reject damage before those boundaries instead of treating it as an unfinished suffix. Footer capacity must include pending and in-flight records, including new records appended to a page allocated much earlier.

Reclaim validates each live inline record or whole extent before relocation. Copying produces new records/extent checksums and preserves key, LSN and tombstones. Segment pins protect old physical locations until readers finish. Relocation-before-free persistence ordering remains a required proof; this proposal does not silently assume completion implies durability.

## When the rewrite contract is unavailable

An implementable conservative alternative retains the open RAM page but appends immutable snapshots to fresh locations. A snapshot must carry logical page identity, generation, and a complete checksum. Index entries point to immutable snapshots, and recovery selects/merges validated versions. Earlier snapshots cannot be reclaimed until their replacements are durable and readers no longer reference them.

This preserves the contents of prior completed snapshots if a later write is torn. It does not satisfy a strict same-physical-address requirement, and repeated snapshots duplicate old payload. It requires additional index/reclaim work and can use substantially more space before reclamation. Its on-disk snapshot encoding is not specified or validated by the current prototype.

A bounded pair of shadow locations can keep the logical page address stable, but two slots alone are not enough: the last durable copy must remain untouched until the replacement is durable, and an old slot cannot be reused while readers pin it. Those persistence and lifetime gates must be designed before choosing that optimization. A plain alternating A/B counter is not a recovery proof.

## Polling and configuration

Keep `poll()` as a progress driver. Submit dirty inline snapshots and pending extents when the byte threshold, oldest-record deadline, or explicit barrier requires it; polling itself does not close the inline page.

Suggested configuration dimensions, with defaults to be measured:

- `inline_max_value`: eligibility for the open inline page; an encoded record must fit the page regardless of this threshold.
- `submit_bytes`: accumulation target, independent of a physical allocation's capacity.
- `max_batch_delay`: maximum time before unsubmitted work becomes eligible; zero remains an option.
- `value_alignment`: packed/minimum-whole-value-pages versus forced-page behavior.
- A backend capability/policy selecting safe inline persistence behavior; never infer it from alignment.

A deadline starts with the first pending record and is not extended by later arrivals. Expose the earliest deadline to cache-store and server worker waits. Merely checking a timer during `poll()` is insufficient if an idle worker can sleep forever. Flush submits all prior work and waits for the configured persistence barrier without closing an otherwise usable inline page. Seal drains and closes all pages and extents in the segment.

Space for the footer must be reserved at record admission, not just at page/extent allocation. A segment can exhaust footer capacity while its open inline page still has bytes available; then it must stop accepting records, drain and seal.

## Expected benefits and unavoidable costs

| Change | Expected benefit | Remaining cost |
| --- | --- | --- |
| Framed and Large share an extent format | Fewer codecs/scanners and queues of pending records | Single-large-buffer fast path still deserves separate admission logic |
| Actual-size directory | Removes oversized metadata reservation for light workloads | Final assembly may require buffering/copying |
| Per-value placement | Retains page-efficient reads without three batch kinds | Page alignment can waste bytes on writes |
| Open inline page | Reuses space in an already allocated page | Every submitted rewrite still writes 4 KiB |
| Per-inline-record CRC | Recovers a valid small-record prefix | Metadata/checksum CPU and bytes remain |
| One extent body CRC | No persisted per-64-KiB checksum arrays | Explicit diagnostics read/verify the containing extent |

Reusing an inline page reduces allocated-space amplification. It does not automatically reduce submitted-write amplification: ten separately submitted 100 B updates still issue ten 4 KiB writes. Batching before submission is what reduces that traffic. Under the proposed inline encoding, a page can hold 26 records with 100 B values, but repeatedly rewriting it after each record remains expensive.

Likewise, placing a CRC in an extent header avoids a mandatory trailer spill page, but does not remove normal page rounding. Do not claim performance improvement until comparing I/O bytes/count, reserved/live space, latency and CRC/copy CPU under both sparse and saturated traffic.

## Prototype and evidence

Run:

```sh
python3 docs/design/prototypes/append_log.py
```

The [prototype](prototypes/append_log.py) implements the exact inline and extent encodings above and the combined recovery walk. It contains 22 tests, including:

- A known CRC32C vector.
- Mixed-value alignment and explicit byte offsets.
- All 4096 incomplete byte prefixes of a new one-page extent.
- All 4097 old/new byte-prefix boundaries of an inline rewrite.
- All 256 combinations of old/new 512 B sectors for both cases.
- Every page arrival order for a multi-page extent.
- Recovery of a later extent after an incomplete inline suffix.
- Invalid lengths and geometry, stale incarnation, wrong physical location, overwrites and tombstones.
- Retention of the inline prefix under the declared old/new model, and a counterexample when old bytes are damaged.
- Deadline decisions that do not submit early or extend the oldest record's deadline.

The tests passed during proposal development. They model byte images, not device guarantees. They do not implement shadow persistence, superblocks, footers, asynchronous queues, durability barriers, concurrent readers, reclaim, transport integration, or worker wakeups. The engine itself still uses its existing layout and polling behavior.

## Implementation gates

1. Decide whether the target backend supports the declared inline rewrite contract. Otherwise specify immutable snapshots/shadows or choose submit-once semantics explicitly.
2. Implement bounded codecs and recovery first, including sealed committed-boundary checks and fault injection through `MemDevice`.
3. Implement frozen submission buffers, serialized inline writes, exact ticket boundaries, LSN publication, footer accounting, and reader lifetime protection together.
4. Integrate a single extent path for Framed/Large, prepared landing buffers, and optional diagnostic verification. Change or validate existing caller-supplied transport checksum APIs explicitly; do not silently ignore supplied checksums.
5. Wire deadlines into every worker and blocking helper before removing unconditional small-batch submission from `poll()`.
6. Exercise crash/reopen, concurrent range reads during inline rewrites, failed writes, overwrites, deletion, reclaim, barriers, and full segments in engine tests. Run the full workspace checks and device-backed measurements before switching defaults.

## Current-code references

- [Device contract](../../core/moat-engine/src/device.rs)
- [Current batch encoding](../../core/moat-engine/src/writer/batch.rs)
- [Writer polling, placement, and completion](../../core/moat-engine/src/writer.rs)
- [Barriers and sealing](../../core/moat-engine/src/writer/sealing.rs)
- [Layout](../../core/moat-engine/src/layout.rs), [scanning](../../core/moat-engine/src/scan.rs), and [recovery](../../core/moat-engine/src/engine.rs)
- [Index](../../core/moat-engine/src/index.rs) and [reader](../../core/moat-engine/src/reader.rs)
