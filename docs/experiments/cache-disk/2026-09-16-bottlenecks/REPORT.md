Twenty-device bottleneck investigation
======================================

The previous [comparison](../2026-09-16-20disk/REPORT.md) is an application
path measurement, not a limit imposed by the v2 format. Controlled experiments
identify admission batching and payload preparation as substantial constraints.
No engine code, persistent format, checksum policy, pool configuration, or
device settings changed in this investigation. All figures aggregate 20 devices.

Small writes depend on admission batching
-----------------------------------------

These runs use the original executable and change only `prefill_batch`.
The application still spawns at most 40 tasks per batch and waits for the
entire batch before starting the next. Increasing the batch raises the
available work per disk, changes frame packing, amortizes task setup, and
reduces the frequency of global batch barriers. It does not isolate those
effects from one another.

| Value | Global batch | V1 Mops/s | V2 Mops/s | V2 vs V1 |
|---|---:|---:|---:|---:|
| 1 KiB | 2,560 | 4.243 | 4.117 | -3.0% |
| 1 KiB | 10,240 | 4.814 | 5.692 | +18.2% |
| 4 KiB | 2,560 | 3.209 | 3.134 | -2.3% |
| 4 KiB | 10,240 | 3.202 | 3.852 | +20.3% |

Each cell is a median of three freshly initialized processes. Within v2,
the larger batch improves 1-KiB throughput by 38.2% and 4-KiB throughput by
22.9%. The v1/v2 ranking changes without changing either implementation.
A smaller exploratory batch of 640 reduces both engines to approximately
1.9 Mops/s; both repetitions are retained in the numeric artifacts.

The original -7.7% / -6.6% small-write gaps narrow in these fresh controls.
Prefills last less than a second in several cases, so these measurements
support sensitivity to batching, not a universal or sustained engine ranking.
A continuously replenished, bounded admission window and longer fills are
better next steps than treating 10,240 as a universal setting.

The existing 4-KiB CPU profile also shows substantial allocation cost. The
v2 `malloc` samples mostly lead to the one-shot completion allocation and
the full-key envelope/value vectors. They do not establish `FrameBuilder`
allocation as the principal bottleneck. Reusing builder storage and avoiding
redundant decoding of freshly generated metadata remain smaller candidates,
with format-limit validation preserved.

Large writes pay for an unnecessary intermediate copy
------------------------------------------------------

The default engine adapter performs this sequence:

1. Generate an owned value vector on an application worker.
2. Allocate another vector and copy the full key and value into it.
3. Copy the envelope into the registered I/O buffer on the engine worker.
4. Calculate write checksums and submit the buffer.

Foyer serializes its owned key/value directly into its write buffer; it does
not use the engine adapter's intermediate envelope vector. This is a real
difference in the measured input paths, unrelated to v2's on-disk encoding.

`engine_preassembled_input` combines steps 1 and 2: generate the identical
full-key envelope in one allocation. It preserves the final copy, checksums,
physical layout, and durability boundary. Controls and treatments below use
the same executable and three independently initialized processes each.

| Value | Engine | Owned value GiB/s | Preassembled GiB/s | Change |
|---|---|---:|---:|---:|
| 64 KiB | V1 | 10.918 | 10.646 | -2.5% |
| 64 KiB | V2 | 9.844 | 11.042 | +12.3% |
| 4 MiB | V1 | 21.381 | 27.121 | +26.8% |
| 4 MiB | V2 | 20.272 | 27.189 | +34.1% |

Foyer controls in this experiment reach 13.307 GiB/s at 64 KiB and
33.950 GiB/s at 4 MiB. Removing one copy substantially narrows the 4-MiB
gap but does not close it. V1's 64-KiB controls have a wide range and do not
show a repeatable gain from this change; all samples and ranges are retained.

The original complete-process 4-MiB profile attributes 64.33% of v2's sampled
cycles to identified copy sites, 17.44% to fill sites, and 12.50% to the
CRC32C SIMD site. These are sampled self cycles, not wall-time fractions.
The controlled copy experiment provides causal evidence beyond that profile.

Prepared buffers expose further producer-side headroom
-------------------------------------------------------

The existing v1 and v2 prepared APIs let a producer fill the final registered
buffer. `engine_in_place_input` exercises that path: it generates every value
on its I/O worker instead of constructing and copying two heap vectors. It
keeps all write CRCs, the same record bytes, per-batch completion waits, and
the final synchronization. This also changes initialization locality and
the division of work between application and I/O workers.

| Value | Engine | Owned value GiB/s | In-place producer GiB/s | Change |
|---|---|---:|---:|---:|
| 64 KiB | V2 | 9.686 | 16.905 | +74.5% |
| 4 MiB | V1 | 21.115 | 44.331 | +109.9% |
| 4 MiB | V2 | 20.734 | 45.013 | +117.1% |

These are separate three-process medians with controls using the same
executable. The 64-KiB dataset is eight times larger than the preceding copy
experiment; both control and treatment cross segment boundaries. Foyer's
owned-vector controls reach 12.055 and 33.458 GiB/s in this experiment.
The v2 4-MiB in-place range is 44.866–45.232 GiB/s, compared with
20.515–20.949 GiB/s for its owned-vector control. Median process CPU use
falls from 28.83 to 15.35 cores while throughput more than doubles.

This demonstrates headroom in the existing format and prepared API. It is
**not** an API-equivalent claim that v2 now beats foyer for owned-vector
inserts: accepting existing values still needs an efficient ownership/copy
path. Foyer is not given an in-place producer in this experiment, and these
numbers do not replace the default-path comparison. No production engine
optimization is shipped by adding these benchmark modes.

Large-frame batching reduces padding without a format change
------------------------------------------------------------

The default adapter selects one prepared frame for every value of at least
64 KiB, even though the unified format supports multiple large values in a
frame. With already owned inputs, this path still performs a payload copy.

Using the existing `FrameBuilder` for queued 64-KiB values, up to 64 records
within the existing 8-MiB frame bound, lowers measured write amplification
from 1.125x to 1.068x. Foyer is 1.064x in these controls. V2 throughput rises
from 9.844 to 10.323 GiB/s (+4.9%), with three repetitions per setting.

The 16-byte full-key envelope matters here: a 64-KiB logical value occupies
65,552 payload bytes. A separate page-aligned prepared frame adds a metadata
page and rounds the payload to 17 pages, totaling 18 pages (1.125x logical
value bytes). Packing several records amortizes the leading metadata page.
This is a batching-policy cost, not a requirement to change the wire format.
Four-MiB records cannot share an 8-MiB frame once keys and metadata are included.

Read differences and remaining opportunities
---------------------------------------------

The previous independent-dataset v1/v2 read medians differ by at most 3.4%,
and several gaps are smaller than the process-to-process spread. Foyer's
initial 64-KiB peak exceeds both engines, but its independent 640-client
rechecks do not preserve that ordering. The data does not establish a stable
v2-specific read regression in those cases.

The complete-process v2 4-KiB read profile attributes 24.43% of sampled cycles
to a contended standard-library futex mutex, with stacks reaching Tokio's
shared scheduling queue through one-shot completion delivery. At 64 KiB,
the same site accounts for approximately 24% in both engines. This is a
cross-thread adapter/completion bottleneck, not a lock in v2's index. Batching
completion delivery onto application workers or keeping execution local is
a concrete next experiment for both engines. No improvement from that change
is claimed here. Engine read CRC is already disabled in these measurements.

The layout has real space and work costs: a 64-byte frame header, 64-byte
record descriptors, four checksum bytes per logical 64-KiB payload block,
alignment gaps, and metadata copied into the segment footer when sealed.
These costs remain; their existence does not explain an immutable throughput
ceiling at the original results. V2's original 4-KiB write amplification is
slightly lower than v1's, and their 4-MiB amplification is effectively equal.
Changing the fixed 8-byte alignment would not address the measured copy or
scheduler costs.

The implementation priorities suggested by the evidence are to keep write
admission continuously replenished, avoid intermediate payload allocations,
use prepared buffers when the producer can fill them directly, batch frame
construction appropriately, and reduce remote completion wakeups. Preserving
a prepared buffer across backpressure and overlapping encoding with I/O are
additional implementation candidates. They do not require a format redesign;
their individual benefits have not all been measured.

Reproduction and interpretation
--------------------------------

The [numeric samples](samples.csv) retain every completed trial and the
[summary](summary.csv) includes medians, minima, maxima, and sample counts.
Source revisions and executable hashes are listed in [methodology](README.md).
Do not combine settings from separate experiments into an unmeasured result.
The short read phase after each full-key readback is only an activity check
in this investigation, not a replacement for the earlier read matrix.

The 89 completed trials wrote and checked 99,693,360 records. GNU/glibc
builds, harness Clippy, and file-backed functional checks passed; the
in-place checks also enabled read CRC and exercised segment rollover.
The 20 devices were idle and all benchmark locks released after completion.

The original [profiling notes](../2026-09-16-20disk/PROFILING.md) describe
sampling scope, attribution limits, allocation/copy sites, and foyer's
integrity and owned-read differences. Neither this report nor the original
comparison measures eviction, GC, or crash recovery throughput.
