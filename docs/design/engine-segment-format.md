# Segments, footer trailers, and recovery

The current implementation is in `moat-engine-v2`. Device, segment, and frame
format versions are all **1**. This alpha format does not read or migrate earlier
alpha layouts. See [device lifecycle](engine-device-lifecycle.md) for allocation
and formatting rules.

## Physical layout

```text
0              4096               data_end             S - footer_len       S
| Allocation   | Frames ...        | Unused gap         | Metadata | Pad | T |
| header, 4K   |                   |                    |<---- Footer ----->|
                                                                    T = 64 B trailer
```

`S = segment_size = SegmentHeader::segment_len()` includes the allocation header
and footer. Frames grow from offset 4096. The footer ends at `S` and grows toward
lower offsets. `data_end` marks the end of sealed frames, independently of the
footer start. The trailer occupies the final 64 bytes; metadata can use the rest
of the final page.

`SegmentBuilder::position` reserves the complete footer before accepting a
frame. `append` copies validated frame metadata; it does not establish write
completion. `seal_into` writes the footer and returns an in-memory sealed
`SegmentHeader`. That sealed view cannot be encoded as an allocation page,
preventing accidental replacement of immutable allocation information.

## Allocation header

The header occupies 4096 bytes. Offsets below are page-relative; integers are
little-endian.

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `MOATSEG1` |
| 8 | 4 | Version `1` |
| 12 | 4 | Whole-page CRC32C, with this field zeroed |
| 16 | 16 | Device ID |
| 32 | 4 | Segment number |
| 36 | 4 | Complete segment length `S` |
| 40 | 8 | Nonzero allocation sequence / generation |
| 48 | 4048 | Reserved, all zero |

The allocation header is authoritative for the current allocation identity. It
must become durable before frame writes and remains immutable during that
allocation. Frames carry the allocation sequence and segment-relative offset.
Sequences must be unique across device allocations so that stale frames cannot
be interpreted as new data. The engine has no reclamation or reuse API yet;
recovery nevertheless handles stale footers from earlier generations.

## Footer and 64-byte trailer

```text
| Frame 0 metadata | ... | Frame N metadata | Zero padding | 64 B trailer |
```

Metadata retains each frame's original header, record directory, and checksum
array, packed in physical frame order. It excludes values and original frame
alignment padding. This change does not introduce compact record summaries.

The following offsets are relative to the trailer, not the footer:

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `MOATFTR1` |
| 8 | 4 | Version `1` |
| 12 | 4 | Trailer CRC32C |
| 16 | 16 | Device ID |
| 32 | 4 | Segment number |
| 36 | 4 | `data_end` |
| 40 | 8 | Allocation sequence / generation |
| 48 | 4 | Frame count |
| 52 | 4 | Total packed metadata bytes |
| 56 | 4 | Page-aligned complete footer length |
| 60 | 4 | Complete footer CRC32C |

The complete footer CRC treats both trailer CRC fields as zero. All other bytes,
including metadata, zero padding, and trailer fields, participate. After storing
that CRC, the trailer CRC is calculated with only its own field at offset 12
zeroed. The trailer CRC therefore protects the stored complete footer CRC without
a circular dependency.

`FooterTrailer::decode` checks the final 64 bytes for checksum, version, and
geometry. Callers must compare identity and generation against a trusted
allocation header before using the seal information. `Footer::decode` validates
the complete CRC, zero padding, frame metadata, counts, and data coverage.
Unsupported versions and underlying I/O errors are not treated as incomplete tails.

## Space accounting and write ordering

```text
M_i          = 64 + 64 * record_count_i + 4 * checksum_count_i
footer_len   = align_up(sum(M_i) + 64, 4096)
footer_start = S - footer_len
required     = next_data_end + next_footer_len
required    <= S
```

Even an empty footer occupies one page. The write buffer is allocated once using
`builder.footer_len()`. A single-page footer needs only the final page write.
Sealing proceeds as follows:

1. Drain frame I/O and confirm all writes succeeded.
2. Write the footer prefix, excluding its final page; skip this for one page.
3. Sync frame data and the footer prefix.
4. Write the final page containing the trailer.
5. Sync again before reporting seal success.

The allocation header is never rewritten during sealing. Failure at any step
disables subsequent writes by that owner. This protocol does not assume atomic
4 KiB writes across power loss.

## Recovery

Recovery reads the allocation header and final 4 KiB of every slot. A small
footer is already complete in the final page and needs no further read or footer
allocation. For a larger footer, recovery allocates the validated complete size,
copies the final page, and reads only the preceding bytes.

| Allocation / trailer state | Action |
| --- | --- |
| Allocation and final page both zero | Unallocated |
| Allocation zero, final page nonzero | Error; do not infer allocation or reuse |
| Corrupt allocation | Error; do not substitute footer identity |
| Same generation, valid complete footer | Rebuild the index without reading payload |
| Older footer generation | Ignore the stale footer and scan current allocation frames |
| Future footer generation or wrong identity | Error |
| Missing or torn trailer | Scan the valid frame prefix using the allocation header |
| Valid same-generation trailer, corrupt complete footer | Retain `data_end` and counts; strictly scan the sealed range |

Scanning verifies full frame metadata and payload checksums. An active tail stops
at the first invalid or stale frame; it does not search past damage for another
magic value. Corruption inside a sealed range is an error. Pipeline routing uses
`data_end`, not the later footer start, as its frame boundary. Recovered active
allocations are not reopened for appends.

Footer recovery is metadata recovery, not a full-device payload integrity scan.
Device commit logging, distributed repair, and garbage collection remain separate
work.

## Validation

Tests cover independent CRC vectors, corruption of every header/footer byte,
forged lengths and counts, page-boundary reservations, empty footers, stale and
future generations, corrupt allocations, partial replacement of tail pages,
sealed scan boundaries after footer corruption, and exact read sequences for
large footer prefixes. Seal fault tests cover syscall failures and simulated
crashes that restore the last durable image while discarding unsynced writes.
These validate the protocol and fault model; they are not physical power-loss tests.
