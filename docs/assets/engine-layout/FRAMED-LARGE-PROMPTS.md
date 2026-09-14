# Exact framed and large batch diagrams

Generated with the built-in image_gen tool. Example byte ranges were computed from the implementation before generation. Rendered sizes, ranges, padding, checksum coverage, and BatchHeader fields were visually checked against those calculations.

## Framed batch

Use case: infographic-diagram.
Create an exact English moat-engine storage diagram, matching clean flat technical diagrams: white background, navy headings, readable large monospace byte ranges, blue batch header, light-blue metadata, teal values, gray zero padding. Landscape, generous margins.
Title "FRAMED BATCH: METADATA FIRST, VALUES PAGE-ALIGNED"
Subtitle "Exact batch-relative ranges [start, end). Block widths are schematic."
Assumption banner "Example: buffer capacity = 1048576 B (1 MiB); Value A = Value B = 4090 B"

Draw THREE stacked horizontal strips of consecutive bytes from the SAME batch. Each block prints its name, size, and range exactly; no additional numeric axes.
First strip label "Reserved header area: [0, 20480), 5 pages"
Four adjacent blocks:
"Batch header" / "64 B" / "[0, 64)"
"Meta A" / "68 B" / "[64, 132)"
"Meta B" / "68 B" / "[132, 200)"
"Unused header area" / "20280 B, zero-filled" / "[200, 20480)"
Second strip label "Value page A: [20480, 24576)"
Two adjacent blocks:
"Value A" / "4090 B" / "[20480, 24570)"
"Alignment padding" / "6 B, zero-filled" / "[24570, 24576)"
Third strip label "Value page B: [24576, 28672)"
Two adjacent blocks:
"Value B" / "4090 B" / "[24576, 28666)"
"Tail padding" / "6 B, zero-filled" / "[28666, 28672)"

Below, an exact compact calculation box:
"Header reservation"
"max_records = 1048576 / 4096 = 256"
"header_len = align_up(64 + 256 * 68, 4096) = 20480"
Footer notes:
"Each Meta = 64 B record header + 4 B value-block checksum"
"Metadata is packed: Meta B starts at 132, with no 8 B alignment."
"Values start at 20480 and 24576, both 4 KiB aligned."
"BatchHeader: kind = Framed, header_len = 20480, record_count = 2, batch_len = 28672"
Strictly preserve all numbers and half-open ranges. Headroom depends on buffer capacity, not actual record count. Never label it universally one page. Do not insert any gap between Meta A and Meta B. No Chinese. Do not draw direct I/O extents or claim anything about read counts.

## Large batch

Use case: infographic-diagram.
Create an exact English moat-engine storage diagram, clean flat technical art, white background, navy headings, large readable monospace byte ranges. Blue batch header, light blue record header, amber checksum array, teal value data, gray zero padding. Landscape generous space.
Title "LARGE BATCH: ONE RECORD, PAGE-ALIGNED VALUE"
Subtitle "Exact batch-relative ranges [start, end). Block widths are schematic."
Assumption banner "Example: value_len = 65537 B (64 KiB + 1 B)"

Draw TWO horizontal strips showing consecutive parts of ONE batch.
First strip label "Header page: [0, 4096)"
Exactly four adjacent blocks, each containing these exact labels:
"Batch header" / "64 B" / "[0, 64)"
"Record header" / "64 B" / "[64, 128)"
"Checksum array" / "8 B: two CRC32Cs" / "[128, 136)"
"Header padding" / "3960 B, zero-filled" / "[136, 4096)"
Second strip label "Value and tail: [4096, 73728)"
Exactly three adjacent blocks:
"Value block 0" / "65536 B" / "[4096, 69632)"
"Value block 1" / "1 B" / "[69632, 69633)"
"Tail padding" / "4095 B, zero-filled" / "[69633, 73728)"
A bracket or note groups ONLY the first two blocks in the second strip, labeled "One value: [4096, 69633), 65537 B". The second strip spans multiple pages; do NOT label it a single page.
Below strips, a clear checksum mapping box:
"CRC[0] at [128, 132) covers value bytes [4096, 69632)"
"CRC[1] at [132, 136) covers value bytes [69632, 69633)"
No separate checksum bytes inserted inside or after value blocks.
A calculation box:
"meta_len = 64 + 2 * 4 = 72"
"value_offset = align_up(64 + 72, 4096) = 4096"
"batch_len = align_up(4096 + 65537, 4096) = 73728"
Footer:
"BatchHeader: kind = Large, header_len = 0, record_count = 1, batch_len = 73728"
"header_len is 0 for Large; the value offset is computed from value_len."
Keep all numbers exactly. Do not imply header_len equals4096. No Chinese. Do not add new fields, offsets, or read amplification claims.
