# moat-engine-v2

Independent implementation of the [unified immutable frame proposal](../../docs/design/engine-frame-layout.md), developed alongside `moat-engine` for review before replacement. It does not depend on, wrap, or copy the old engine's pipelines. Shared primitives come from `moat-common`: chunk identifiers, CRC32C, and buffers.

**Stage 1: frame format, construction, and validation.** This crate is not yet a storage engine or a drop-in replacement. Device/superblock encoding, segment footers, I/O queues, indexing, read/write pipelines, recovery, and physical space reclamation remain subsequent stages. The existing engine and its consumers continue to use their current implementation.

## Usage

```rust
use moat_common::{AlignedBuf, ChunkId};
use moat_engine_v2::frame::{Frame, FrameBuilder, FrameLimits, FramePosition};

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
# Ok::<(), moat_engine_v2::frame::Error>(())
```

For a prepared value, allocate `PreparedFrame::required_len(limits, value_len)` bytes, borrow the buffer with `PreparedFrame::new`, fill `value_mut()`, then call `finish(position, key, lsn)`. The payload already occupies its final page-aligned region. Finishing computes checksums and writes metadata and padding without copying the payload. The caller's buffer remains available if finalization fails.

The codec accepts byte slices. The I/O layer must supply an aligned buffer address, retain the buffer until completion, and prevent modification after submission. `AlignedBuf` and registered buffers from `moat-common` provide suitable storage; the codec does not allocate or submit those buffers.

## Persistent encoding

All integer fields are explicitly little-endian; there are no Rust layout casts or unsafe blocks. Frame starts and lengths are multiples of 4096 bytes. The magic is `MOATFRM2` and the version is `2`. The decoder rejects the original engine's batch encoding; this is not a migration reader. These constants identify frames only, not a completed device format.

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

`FrameHeader::decode` checks the fixed header before callers allocate or read the declared extent. Geometry calculations use 64-bit arithmetic and are bounded before conversion to slice indices. `FrameLimits` must eventually be persisted in the device superblock, independently of runtime batching options. `FramePosition` checks segment incarnation and physical offset; the segment allocator must separately reserve footer space.

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
- `Frame::decode` additionally checks every value checksum before returning any accepted frame. A damaged later record rejects the entire frame. This is the primitive for a future active-tail recovery scanner.

Verified reads must eventually fetch metadata and distant payload as separate extents, coalescing only when useful. For 32 records of 1 KiB each, metadata occupies 2240 bytes and the last value begins at offset 34816. The APIs let a reader validate the front metadata and that value without fetching intervening payloads. Actual read planning and I/O submission are not part of this stage.

Checksums detect corruption; they do not make writes atomic or durable. Recovery prefix rules, publication order, persistence barriers, and segment reuse cannot be established by this codec alone.

## Subsequent engine boundaries

The upper layer decides which chunks to delete and controls segment selection, scheduling, placement policy, and maintenance budgets. The engine should expose segment statistics and execute explicitly requested physical reclamation, validating a segment handle that includes its allocation incarnation. It remains responsible for liveness revalidation, conditional index updates, persistence before freeing storage, and reader safety. A default victim-selection heuristic belongs in the caller's policy, not in the only engine execution entry point.

This stage does not fix the number of streams, encode Hot/Cold categories, or implement a reclamation scheduler. Those interfaces will be reviewed when segment management is implemented.

## Review and validation

Start with `src/frame/header.rs` and `record.rs` for the wire format, `builder.rs` for placement and buffer ownership, and `decode.rs` for validation. `tests/frame.rs` covers mixed layouts, multi-page directories, empty values and tombstones, prepared-buffer identity, deterministic admission boundaries, reordered values, partial checksums, truncation, and forged structures with recomputed CRCs. A golden header and metadata vector was generated with an independent bitwise CRC32C encoder.

```sh
cargo test -p moat-engine-v2
cargo clippy -p moat-engine-v2 --all-targets -- -D warnings
cargo bench -p moat-engine-v2 --bench frame
```

The benchmark measures in-memory assembly, full validation, and prepared finalization. It does not measure device throughput, recovery, or end-to-end latency, and does not establish an improvement over the old engine.
