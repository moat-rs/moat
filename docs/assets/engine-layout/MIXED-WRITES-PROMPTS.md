# Mixed-write lifecycle diagram

Generated with the built-in image_gen tool and checked against Writer::poll, close_all_pending, close_pending, enqueue_batch, write_large, and ensure_room.

## Initial prompt

Use case: infographic-diagram.
Create a precise English educational flow diagram of the CURRENT moat-engine writer. White background, navy text, clean readable flat technical style. Use blue for Inline, purple for Framed, teal for Large, gray for unused space. Large landscape or portrait layout with ample room for text. Title "MIXED WRITES: ONE SEGMENT, SEPARATE BATCHES".
Subtitle "Pending batches can grow. Finalized batches cannot be reopened."
Assumptions banner exactly "One Writer; foreground puts; enough segment space and buffers; no intervening poll."
Use three numbered panels, sequential top to bottom. This diagram explains state and ordering, not byte offsets. Do not invent numeric offsets, sizes or arbitrary operations.

Panel 1 title "1. Accept writes before poll()"
Show this exact left-to-right call sequence: "put I1" -> "put F1" -> "put L1" -> "put I2" -> "put F2".
Legend text "I = Inline value; F = Framed value; L = Large value".
Below show three separate lanes:
Blue lane "Inline pending (RAM)" with boxes "I1" and "I2", note "Still appendable".
Purple lane "Framed pending (RAM)" with boxes "F1" and "F2", note "Still appendable".
Teal lane "Large batch" with a single "L1", arrow labeled "Finalize immediately; assign segment space".
At bottom of panel show "Active Hot segment: assigned layout" strip with exactly "Segment header" | "Large [L1]" | "Unused space".
Important: label this ASSIGNED layout, not durable/on-disk completed data. Pending Inline and Framed batches do not yet have segment positions. Do not show their boxes occupying this segment strip.

Panel 2 title "2. poll() finalizes pending batches, even when not full"
Draw arrows or numbered callouts "Close Inline pending" then "Close Framed pending".
Show "Same Hot segment: assigned layout" strip:
"Segment header" | "Large [L1]" | "Inline [I1, I2]" | "Framed [F1, F2]" | "Unused space".
Caption "Batch order follows space assignment, not value arrival order."
Caption "Each batch has one layout kind and occupies whole 4 KiB pages."
No footer in this active segment strip. No shared pages between batches.

Panel 3 title "3. A later put I3 starts a NEW Inline batch"
Show small blue box "New Inline pending [I3]" -> arrow labeled "next poll()" -> a final segment strip:
"Segment header" | "Large [L1]" | "Inline [I1, I2]" | "Framed [F1, F2]" | "Inline [I3]" | "Unused space".
A clear callout pointing to the OLD "Inline [I1, I2]" block says "Finalized: no appends, including into padding".
Footer note exactly "Finalized or submitted does not imply durable. Completion and durability are separate."
Small final note "If the active segment lacks room for a batch plus footer, the writer switches segments."
Keep every label English. Do not suggest the batches wait until full. Do not draw one mixed-kind batch. Do not claim poll waits for I/O completion. The sample assumes pending batches remain below their limits until poll.

## Correction prompt

Edit this technical diagram with only the following precise corrections. Preserve all batch ordering, colors, labels and panels unless specified.
1. In the top assumptions banner use: "One Writer; foreground puts; enough space and buffers; pending batches stay below limits until poll." Allow two lines. The call sequence in panel 1 has no intervening poll.
2. In panel 1 replace the sentence "This is the ASSIGNED layout, not durable/on-disk data." with "Space assignment does not imply I/O completion or durability." The data could already be written; the diagram does not assert either outcome.
3. In panel 3 rename "Final segment layout (after next poll())" to "Assigned segment layout (after next poll())".
4. In panel 3 separate the flow arrow from the annotation. "New Inline pending [I3]" must have an arrow labeled "next poll()" going to the NEW "Inline [I3]" block on the right of the bottom strip. It must NOT point at the "Finalized: no appends, including into padding." annotation. Position that annotation above the OLD "Inline [I1, I2]" block and keep its own short callout arrow pointing only to that old block.
5. Remove "No footer in this active segment strip. No shared pages between batches." from panel 2; the immediately preceding whole-pages caption is sufficient.
All text English. Keep everything else unchanged.
