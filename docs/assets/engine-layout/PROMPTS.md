# Image generation prompts

Generated with the built-in image_gen tool. All images use English labels and depict the implementation reviewed on 2026-09-11. Diagrams are schematic, not to scale.

## Figure 1

```text
Use case: infographic-diagram. Create a precise technical educational diagram for the moat-engine open-source storage engine. Wide landscape canvas, high-resolution, crisp flat vector-like raster, white background, restrained navy text, blue metadata, teal data, light gray padding, amber checksums. Generous whitespace, consistent thin outlines, clean sans-serif typography and monospaced byte labels. All visible text in English. No perspective, 3D, decorative hardware, gradients, watermark, or invented logo. Diagrams are schematic, not byte-proportional; clearly label "Not to scale". Prioritize correct topology and legible text over ornament.
Title: "moat-engine / Device and segment layout"
Subtitle: "Format v1 · Little-endian encoding · 4 KiB page alignment"
Top panel label "Device". A horizontal address bar split into "Reserved region", "Segment 0", "Segment 1", "...". Show equal conceptual segment-sized allocation regions. Under Reserved region an expansion containing "Superblock A / 4 KiB / offset 0", "Superblock B / 4 KiB / offset 4096", and "Reserved space". A bracket across the entire reserved region reads "segment_size (default: 1 GiB)". Under Segment 0 show "offset = segment_size". Below the device bar put formula "segment_offset(n) = (n + 1) × segment_size".
Middle panel label "One sealed segment". Expand Segment 0 with connector lines into a horizontal bar containing, IN THIS ORDER, "Segment header / 4 KiB", "Batch 0", "Batch 1", "...", "Footer", "Unused space". A subtle bracket covers whole bar and reads "segment_size". Footer must be clearly BEFORE unused space, not fixed at segment end. Callout pointing to footer: "footer_offset and footer_len are stored in the segment header".
Bottom left compact card "Segment header" with four readable lines "disk_uuid + seg_no: identity", "state: Free / Active / Sealed", "kind: Hot / Cold", "seq: allocation incarnation". Bottom right compact state cycle "Free → Active → Sealed → Free" with a note "A new allocation gets a new seq". Final small note "Active segments recover by scanning batches; sealed segments normally recover from the footer."
```

## Figure 2

```text
Use case: infographic-diagram. Create a precise technical educational diagram for the moat-engine open-source storage engine. Wide landscape canvas, high-resolution, crisp flat vector-like raster, white background, restrained navy text, blue metadata, teal data, light gray padding, amber checksums. Generous whitespace, consistent thin outlines, clean sans-serif typography and monospaced byte labels. All visible text in English. No perspective, 3D, decorative hardware, gradients, watermark, or invented logo. Diagrams are schematic, not byte-proportional; clearly label "Not to scale". Prioritize correct topology and legible text over ornament.
Title: "moat-engine / Three batch layouts"
Subtitle: "Every batch begins with a 64 B header and ends on a 4 KiB boundary"
Three clearly separated horizontal panels with shared color legend: "Batch header", "Record metadata", "Value", "Padding". Metadata is 64 B record header plus per-value checksums.
Panel 1 label "INLINE · many small records". Horizontal bar with "Batch header", "Meta A", "Value A", "gap", "Meta B", "Value B", "padding". Small markers at record metadata starts, annotated "Record starts: 8 B aligned". Note "Metadata immediately precedes each value. Zero-filled gaps can avoid an extra page crossing." Values must visibly not all start at page boundaries.
Panel 2 label "FRAMED · metadata first, values page-aligned". Horizontal bar with an outlined initial region labeled "Header area: page-aligned, may span several pages", containing "Batch header", "Meta A", "Meta B", "reserved padding". After that region show "Value A", "padding", "Value B", "padding". A dashed vertical line at the start of each Value A and Value B labels "4 KiB boundary". Note "A 4 KiB value needs one data page when read verification is disabled." No implication whole batch is 4 KiB.
Panel 3 label "LARGE · exactly one record". Horizontal bar "Batch header / 64 B", "Record metadata", "padding", "Value", "tail padding"; mark the start of Value at a page boundary. Note "value_offset = align_up(64 + metadata_size, 4096)".
Bottom two short information cards: "Default routing" with "value >= 64 KiB → Large" and "smaller values → Inline or Framed"; "Record metadata" with "64 B + 4 B per 64 KiB of value". Small footer "4 KiB values use Framed with default settings. Layouts are schematic, not to scale."
```

## Figure 3

```text
Use case: infographic-diagram. Create a precise technical educational diagram for the moat-engine open-source storage engine. Wide landscape canvas, high-resolution, crisp flat vector-like raster, white background, restrained navy text, blue metadata, teal data, light gray padding, amber checksums. Generous whitespace, consistent thin outlines, clean sans-serif typography and monospaced byte labels. All visible text in English. No perspective, 3D, decorative hardware, gradients, watermark, or invented logo. Diagrams are schematic, not byte-proportional; clearly label "Not to scale". Prioritize correct topology and legible text over ornament.
Title: "moat-engine / Checksums and range reads"
Subtitle: "4 KiB I/O pages · 64 KiB value checksum blocks"
Upper half label "Record metadata and value integrity". Show one horizontal metadata strip with four consecutive subregions "Magic", "Metadata CRC", "Header fields", "Block CRC array". Below strip show a bracket ONLY spanning Header fields and Block CRC array labeled "Metadata CRC coverage". Magic and the Metadata CRC field themselves must be OUTSIDE coverage bracket. To right or below show three value blocks "Block 0 / 64 KiB", "Block 1 / 64 KiB", "Final block / up to 64 KiB"; three arrows from small crc0, crc1, crc2 cells in the checksum array to their corresponding blocks. Note "One CRC32C per value block. Batch header CRC covers only the batch header."
Lower half label "Same requested byte range, different I/O extent". Two aligned horizontal schematic tracks representing a large record, each track starting with "Metadata", a "gap" segment, then "Value block 0", then "Value block 1", then "Value block 2". Inside Value block 2, mark a narrow requested span with a strong teal vertical band. In first row label "verify_reads = false"; highlight ONLY one 4 KiB page in block 2 containing the requested span, leaving metadata, gap and other blocks unhighlighted. Caption "Read the pages covering the requested bytes."
In second row label "verify_reads = true"; highlight ONE CONTINUOUS extent from metadata through the end of Value block 2, including the gap and intervening value blocks. Caption "Expand to checksum blocks and include metadata in one contiguous read." Additional small note "Only the touched value checksum blocks need validation; the I/O extent may include intervening data."
Bottom reminder "The in-memory index stores both header and value offsets." Keep clear distinction between requested bytes, read extent, and checksum coverage using legend. No formula implying arbitrary range always reads entire value; this is a specific example with request in block 2.
```

## Figure 2 correction

The final batch diagram uses this edit of the initial output to remove incorrect generated address ticks.

```text
Edit this moat-engine technical batch-layout diagram. Preserve the overall canvas, three-panel layout, English text, colors and all correct formulas. Correct only misleading byte-offset annotations and text artifacts.
1. In the INLINE panel, replace the garbled annotation above the bar with the exact clean text "Record starts: 8 B aligned".
2. Remove ALL numeric offset tick labels under ALL THREE bars: remove 0, 64, 164, 264, 320, 420, 128, 192, 4096, 8192 and the under-bar "64 + metadata_size". Leave those spaces blank. These are schematic bars, not exact byte examples. Keep "64 B" size labels INSIDE Batch header blocks and the correct value_offset formula BELOW the Large panel.
3. Inside INLINE Value A and Value B remove "(e.g. 100 B)" and "(e.g. 300 B)"; keep just "Value A" and "Value B".
4. Inside FRAMED Value A and Value B remove "(e.g. 4 KiB)" and "(e.g. 8 KiB)"; keep just "Value A" and "Value B". Keep the correctly positioned "4 KiB boundary" annotations at both value starts.
5. Keep the Large value start boundary arrow, its formula "value_offset = align_up(64 + metadata_size, 4096)", and all bottom information cards unchanged.
Do NOT add any numerical address ticks, example sizes, or additional text. All labels must be crisp English. This correction avoids invented offsets and is essential for correctness.
```
