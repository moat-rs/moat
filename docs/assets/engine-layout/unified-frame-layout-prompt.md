# Detailed layout image prompt

Generated with the built-in imagegen tool. The image illustrates the
[unified immutable Frame proposal](../../design/engine-frame-layout.md), not
the implemented layout. The 64-byte Frame header and descriptors are candidate
sizes. The final image explicitly labels the segment footer; Frames have no
footer. Labeled byte offsets take precedence over schematic widths.

```text
Use case: infographic-diagram.
Create a highly readable dedicated on-disk layout engineering diagram for MOAT. It must show ONLY storage layout, no workers, clients, networking, GET/PUT flowcharts, or decorative hardware. New image, landscape 3:2, request high resolution 3072 x 2048 or larger if available. Prioritize large sharp English text and exact numbers over decorations. White background, navy typography, clean rectangular byte-range strips, generous margins. All labels in English. Widths schematic, exact numeric offsets authoritative.

Title: "MOAT · Unified On-Disk Layout"
Subtitle: "PROPOSED FORMAT · Exact mixed-record example"
Small note: "All example offsets are frame-relative bytes. Ranges are [start, end)."

Organize in FOUR horizontal numbered bands. The two top bands are compact, the third and fourth occupy most of the canvas. Use simple dotted zoom connectors between bands, no crossing arrows.

BAND 1: "01  Device"
One horizontal strip:
"Superblock A | 4 KiB", "Superblock B | 4 KiB", "Reserved", "Segment 0 | 1 GiB", "Segment 1 | 1 GiB", "...".
Under left area: "Superblocks + reserve occupy the first 1 GiB".
Under right area: "One shared pool; all segments use the same format".

BAND 2: "02  Segment"
One strip:
"Header | 4 KiB", "Frame 0", "Frame 1", "...", "Footer at seal", "Unused".
Below in large readable labels:
"Segment = allocation / reclaim unit"
"Frame = immutable write unit; size is a multiple of 4 KiB"
A short note: "Seal when next frame + footer cannot fit. Flush does not seal."

BAND 3: "03  One mixed frame · 72 KiB / 18 pages"
This is the main hero diagram.
A frame page map from left to right, with explicit collapsed repetition:
"Page 0 | Metadata + A + D + padding"
"Page 1 | B: 4 KiB"
"Pages 2–17 | C: 64 KiB contiguous"
For the 16 C pages use a long large orange block with faint internal page ticks and the explicit label "16 pages"; do not put metadata inside C.

Immediately below show a large zoomed Page 0 strip, heading "Page 0 expanded · 4096 B".
Precisely these consecutive blocks:
"Metadata | 336 B"
"A | 100 B"
"Gap | 4 B"
"D | 300 B"
"Padding | 3356 B"
Boundary ticks EXACTLY: 0, 336, 436, 440, 740, 4096.
Use a distinct persistent color for each value: A blue, B teal, C orange, D purple. Metadata slate/light gold. Padding light gray hatch.
Below this strip, show an expanded metadata strip heading "Metadata expanded · 336 B total":
"Frame header | 64 B"
"Desc A | 64 B"
"Desc B | 64 B"
"Desc C | 64 B"
"Desc D | 64 B"
"CRCs | 16 B"
Boundary ticks EXACTLY: 0, 64, 128, 192, 256, 320, 336.
Note: "Directory: 4 x 64 B. Checksums: 4 x 4 B, one per value in this example."
Do not draw each metadata component as its own disk page.

BAND 4: "04  Directory resolves physical placement"
A clean table with columns "Record", "Value bytes", "Value range", "Read pages".
Rows EXACTLY:
"A" | "100" | "[336, 436)" | "1"
"B" | "4096" | "[4096, 8192)" | "1"
"C" | "65536" | "[8192, 73728)" | "16"
"D" | "300" | "[440, 740)" | "1"
Under the table put these exact short explanations, readable rather than tiny:
"Directory order: A, B, C, D     Payload order: A, D, B, C"
"D fills an alignment gap BEFORE submission."
"All four records belong to the same pending frame."
"Read pages assume verification is off."

Bottom callouts:
"64 KiB C is continuous: no per-page record headers."
"Submitted frames are immutable; later arrivals use a new frame."
"Frame bytes: 70032 payload + 336 metadata + 3360 padding = 73728"
Small footer: "64 B header / descriptors are illustrative proposal sizes. Segment footer overhead excluded."

Technical constraints:
Do not invent a separate Small/Big engine or Inline/Framed/Large formats. No record crosses a frame, no frame crosses a segment. Header, directory and checksums share Page 0 with small values. B and C are page aligned. D's earlier physical position is chosen while constructing the frame, NEVER a rewrite of a submitted page. Every descriptor points to exactly one contiguous value. The 64 KiB checksum block size is per value, not per physical device alignment. C has ONE checksum because its value is exactly 65536 bytes. No mandatory frame trailer page; the frame ends exactly at byte 73728. Keep spacing clean and numerals exact.
```

## Footer label clarification

```text
Edit this engineering layout diagram by changing ONLY one label. In band "02 Segment", the pink rectangle currently labeled "Footer at seal" must instead read exactly "Segment footer". Keep its position, size, color and font. Preserve every other word, number, byte offset, table, arrow, border, and color exactly. In particular do not alter the 336 B metadata or 72 KiB example. This clarifies that the footer belongs to the segment, not to any frame. No other changes.
```
