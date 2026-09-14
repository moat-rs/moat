# Exact inline layout redraws

Generated with the built-in image_gen tool. The prompts below supply byte ranges computed from the current implementation. The rendered ranges, lengths, record boundaries, and page counts were visually checked.

## No inter-record gap

Use case: infographic-diagram.
Create a precise English technical diagram for moat-engine, landscape, crisp flat vector-like raster artwork, white background, dark navy text, large readable monospace byte offsets. Title "INLINE BATCH: NO INTER-RECORD GAP". Subtitle "Exact batch-relative byte ranges [start, end). Block widths are schematic."
Use blue for batch header, light blue for record metadata, teal for values, gray for zero padding. Metadata includes the 64 B record header plus one 4 B value-block checksum for each record in THIS example.

Main centerpiece: a single horizontal strip of exactly SIX adjacent blocks, in this exact order, with no gaps and no additional blocks. Each block must show its exact label, size and range:
1 "Batch header" / "64 B" / "[0, 64)"
2 "Meta A" / "68 B" / "[64, 132)"
3 "Value A" / "100 B" / "[132, 232)"
4 "Meta B" / "68 B" / "[232, 300)"
5 "Value B" / "300 B" / "[300, 600)"
6 "Tail padding" / "3496 B, zero-filled" / "[600, 4096)"
Do not add a numeric axis; ranges are printed within blocks. Allow enough block width for all labels, since widths are explicitly schematic.
Draw a single clear callout arrow to the shared boundary between Value A and Meta B, labeled "No gap: 232 is already 8 B aligned". This arrow must not point inside Value A or at any other boundary.
Below, add exactly three neat notes:
"Record starts: A = 64, B = 232 (both multiples of 8)"
"Each Meta = 64 B header + 4 B checksum"
"Batch length = 4096 B; record_count = 2"
No invented offsets, no gap between metadata and its value, no 264 or 320 labels. This is an exact correction of an earlier erroneous illustration. Accuracy is the primary goal. All text English.

## Alignment and page-skip gaps

Use case: infographic-diagram.
Create a precise English technical diagram for moat-engine, landscape or square with generous space, flat crisp vector-like raster art, white background, dark navy text, readable monospace ranges.
Title "INLINE BATCH: TWO DIFFERENT GAPS"
Subtitle "Exact batch-relative byte ranges [start, end). Block widths are schematic."
Use blue batch header, light blue metadata, teal values, amber inter-record gaps, gray tail padding.
Main diagram consists of TWO horizontal rows. Row 1 is exactly Page 0, [0,4096). Row 2 is exactly Page 1, [4096,8192). They represent consecutive pages of the SAME batch. Both rows share equal left and right extents but individual block widths are schematic. No numerical axis; print exact ranges and sizes inside each block, using sufficient width. The first row has exactly SEVEN blocks:
"Batch header" / "64 B" / "[0, 64)"
"Meta A" / "68 B" / "[64, 132)"
"Value A" / "101 B" / "[132, 233)"
"8 B alignment gap" / "7 B, zero-filled" / "[233, 240)"
"Meta B" / "68 B" / "[240, 308)"
"Value B" / "3500 B" / "[308, 3808)"
"Page-skip gap" / "288 B, zero-filled" / "[3808, 4096)"
The second row has exactly THREE blocks:
"Meta C" / "68 B" / "[4096, 4164)"
"Value C" / "300 B" / "[4164, 4464)"
"Tail padding" / "3728 B, zero-filled" / "[4464, 8192)"
Below the diagram add a neat comparison box with the title "Why move record C?" and exactly two lines:
"Without page skip: Meta + Value C = [3808, 4176), spans 2 pages"
"Actual placement: Meta + Value C = [4096, 4464), spans 1 page"
Footer notes exactly:
"Each Meta = 64 B header + 4 B checksum"
"Record starts: 64, 240, 4096 (all multiples of 8)"
"Batch length = 8192 B; record_count = 3"
Do not imply that every record requires a gap. Do not add gaps between metadata and its own value. No added invented numbers or decorative charts. English text only. Prioritize all exact labels and accurate range arithmetic.
