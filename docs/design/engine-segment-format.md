# Unified engine segment metadata and recovery

Status: implemented in `moat-engine-v2` as the next review stage after the frame
codec. This specifies segment metadata and allocation accounting, with recovery
from caller-supplied byte slices. Device superblocks, I/O submission, persistence
barriers, index publication, and physical reclamation are subsequent work.

The [frame proposal](engine-frame-layout.md) remains the architectural reference.
The existing frame encoding is unchanged. This document resolves the initial
segment-footer encoding and its admission cost.

## Layout and ownership

```text
Segment-relative byte offsets

0             4096                             data_end
| Header page | Frame 0 | Frame 1 | ... | Frame N | Footer | Unused tail |
                                                   ^
                         only a sealed header commits to this boundary

Footer
| 64-byte prefix | Frame 0 metadata | ... | Frame N metadata | Zero padding |
                  header + directory + checksums, without any value bytes
```

The caller selects a segment and establishes a fresh allocation incarnation.
`SegmentId` contains the device identity, segment number, and incarnation.
`SegmentHeader::decode` checks the device identity, number, and length against
trusted device geometry and discovers the incarnation from the checked header.
Allocation sequences must be unique across the device, including all prior
incarnations, because frame headers carry the sequence without a segment number.
Frames remain
bound to their segment incarnation and segment-relative offset.

`SegmentBuilder` accounts for sequential allocations. Before encoding, call
`position(frame_len, metadata_len)` to check space and obtain `FramePosition`.
After encoding, validate the metadata and call `append(metadata)` before I/O
submission. The builder checks the physical position and copies only the
metadata into its footer staging vector. The data tail and footer reservation
therefore include writes that have been allocated but have not yet completed.
Only `append` advances accounting; repeated `position` calls reserve nothing.
An asynchronous writer must serialize these operations before submitting frames.

Neither `append` nor `seal_into` proves I/O completion or durability. The caller
owns buffer lifetimes and completion order. Any failed frame write abandons
that allocated tail; it must not be followed by sealing the speculative footer
or acknowledging dependent later writes.

## Segment header

The header is one 4096-byte page. All integers use explicit little-endian
encoding. CRC32C covers the entire page, with bytes 12 through 15 treated as
zero. All reserved bytes must be zero.

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `MOATSEG2` |
| 8 | 4 | Segment format version `2` |
| 12 | 4 | Page CRC32C |
| 16 | 16 | Opaque device identity |
| 32 | 4 | Segment number |
| 36 | 4 | Complete segment length |
| 40 | 8 | Nonzero allocation incarnation |
| 48 | 4 | State: active `0`, sealed `1` |
| 52 | 4 | Sealed data end / footer offset |
| 56 | 4 | Sealed page-rounded footer length |
| 60 | 4 | Sealed frame count |
| 64 | 4 | Total embedded frame metadata bytes |
| 68 | 4028 | Reserved, zero |

Active headers have zeroes in every seal field. They do not persist a speculative
write cursor. Sealed fields must describe a page-aligned data boundary and a
footer fitting within the segment. Counts and metadata lengths are bounded by
the data extent before reading the footer. An empty segment may seal with
`data_end = 4096`, zero frames, zero embedded metadata, and a one-page footer.

Frame limits still belong to the future device superblock, independently of
runtime batching options. They are supplied to frame and footer decoding and
are not duplicated in each segment header.

## Footer encoding and cost

The footer stores exact copies of each frame's header, record directory, and
checksum area, in physical frame order. This preserves keys, LSNs, tombstones,
frame positions, value locations, and logical block checksums. Payloads and
frame padding are omitted. Stored value offsets still refer to the original
frame, not the footer. The frame header gives the length of each embedded
metadata region, so no second record-summary codec or offset directory is needed.

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `MOATFTR2` |
| 8 | 4 | Segment format version `2` |
| 12 | 4 | CRC32C of the entire page-rounded footer, this field zeroed |
| 16 | 16 | Device identity |
| 32 | 4 | Segment number |
| 36 | 4 | Data end / physical footer offset |
| 40 | 8 | Allocation incarnation |
| 48 | 4 | Frame count |
| 52 | 4 | Total embedded metadata bytes |
| 56 | 4 | Page-rounded footer length |
| 60 | 4 | Reserved, zero |
| 64 | Variable | Packed frame metadata, followed by zero page padding |

Footer identity and geometry must match the sealed header. Validation reuses
`Metadata::decode` for each embedded frame, requires consecutive original frame
positions, and requires exact coverage through the sealed data boundary. The
embedded byte count must also match exactly. It does not read original payloads.
After validation, iteration borrows metadata in place and rechecks only each
small frame header; it does not repeat directory CRC or overlap validation.
Ordered layouts allocate nothing while decoding; reordered layouts retain the
frame decoder's temporary overlap-check allocation.

For frame `i`, let `N_i` be its record count and `B_i` the total number of value
checksum blocks:

```text
M_i          = 64 + 64 * N_i + 4 * B_i
footer_bytes = align_up(64 + sum(M_i), 4096)
required     = allocated_data_end + next_frame_bytes
               + align_up(64 + sum(M_i) + next_metadata_bytes, 4096)
required    <= segment_size
```

The builder checks this before accepting the next frame. Its staging vector
retains metadata only, proportional to the segment's reserved footer bytes
(with normal vector capacity growth); it does not retain payloads. Footer
finalization copies that metadata once into the caller's output buffer and
computes the footer checksum.

The tradeoff is explicit: a one-block value costs 68 footer bytes per record,
plus 64 bytes per frame, before footer prefix and page rounding. A compact
48-byte summary would be smaller. For 32 one-block records in one frame, the
embedded metadata is 2240 bytes versus 1536 bytes for 48-byte summaries. Both
fit a one-page footer for that single frame, but the difference accumulates
across a segment. The four-record mixed example contributes 336 metadata bytes.

This initial encoding favors one validated representation and retains checksums
for future verified reads. It does not claim minimum footer space or a measured
recovery speedup. A compact footer remains a possible later format revision;
its exact size must still participate in admission accounting.

## Sealing and recovery

`seal_into` writes a footer into the caller's buffer, prevents further builder
admission, and returns the sealed header to encode separately. A short buffer
preserves both builder and destination. Actual I/O must follow this order:

1. Stop allocating frames to this segment and complete all its frame writes.
2. Write the footer at the allocated data end.
3. Persist the frame data and footer.
4. Write the sealed header, then persist it before reporting a durable seal.

A footer written while the durable header remains active is not a committed
seal. Recovery scans the active prefix and stops when it reaches that footer.
A validated sealed footer can reconstruct record locations without reading
payloads. A later verified read must still check the requested payload blocks.

`Scanner` walks complete frames from offset 4096. It exposes a frame only after
all its metadata and payload checksums pass. Record LSNs remain unsorted; the
future index layer must compare versions and preserve tombstones. The scanner
holds no payload buffer or index and does not allocate a segment-sized buffer.
The caller can read each frame separately, first validating its fixed header
to bound the frame read, then supplying the complete frame bytes.

For active segments, an invalid or incomplete candidate ends the accepted
prefix and records a typed `tail_error`. Recovery never searches beyond that
hole for another magic value. An unsupported frame version is a fatal error,
including in an active segment. Device I/O errors must be propagated separately,
not converted into empty or truncated byte slices.

For sealed segments, corruption before the declared data end is fatal. Frame
counts and metadata totals must match the sealed header. If footer validation
fails, a caller may fall back to `Scanner` using the same validated sealed
header; it must not discard the known boundary or treat this as an active tail.
A damaged segment header is an error, not permission to guess geometry.

A recovered active prefix can feed its validated metadata into a fresh
`SegmentBuilder` for the same header to construct a seal footer. Its frames are
not rewritten. After sealing, subsequent writes need a new allocation; recovery
does not authorize appending new frames behind the recovered prefix.

## Remaining persistence work

These APIs construct and validate bytes; the tests exercise in-memory write
images and corruption, not device persistence. In particular, a torn in-place
segment-header update is detected but cannot be repaired by this codec. The
persistent I/O integration must establish a recoverable header-update protocol
before claiming power-loss safety. Device superblock generations, durable
incarnation allocation, barriers, and ordered completion remain implementation
gates. No garbage-collection policy or victim-selection heuristic is added here.

## Review and validation

Read `segment/header.rs`, `builder.rs`, `footer.rs`, then `recovery.rs`.
Errors remain in `segment/error.rs`. The shared integer/CRC codec moved from
`frame/codec.rs` to crate-private `src/codec.rs`; existing frame bytes are unchanged.

`tests/segment.rs` checks independent CRC vectors, every-byte checksum coverage,
forged fields with repaired checksums, footer page-growth admission, allocation
identity, retryable buffer failures, torn active frames, sealed fallback, and
sealing a recovered prefix without rewriting its frames.
