# 64-KiB write follow-up: lifecycle waits and memory traffic

Page-aligning the preallocated source payloads removes the observed 64-KiB
revision gap in matched twenty-device controls, without changing the engine,
allocator, or disk format. The earlier pool removed allocation/free work from
timing but left source address layout dependent on initialization allocations.

The footer request-count difference is real, but measured lifecycle time is too
small to explain the previously reported 4.1% write gap. Copy costs and CPU/cache
topology are substantial benchmark sensitivities. These controls identify an
effective input-layout correction; they do not distinguish the exact hardware
contribution of address aliasing, cache-set conflicts, or copy-instruction behavior.

No production engine or allocator change was made for this follow-up.
Experimental variants were built from isolated frozen source copies. The
validated source-pool correction was then incorporated into the benchmark and
checked with matched baseline/current builds. The allocator remains system
glibc. The disk format is unchanged.

## Scope and reference

The original engine is commit `f76d1dcf8f290261af2860321d040d3f0b87df93`,
with the same preallocated-input benchmark harness as the current engine.
This is a comparison against the unified engine before the asynchronous
lifecycle patch, not against an earlier engine design or cache allocation design.
The frozen binaries match the [longer comparison](../large-batch/REPORT.md).

Every process uses all twenty authorized data devices, a 64-GiB per-device
write window, 1-GiB segments, 512-MiB registered I/O pools per device, queue
depth 256, 16-byte keys, 64-KiB values, and at most 64 reusable input buffers
per owner. Source refill, copy, checksums, data writes, and rollover are timed.
The final active-segment seal at close is outside write timing. Each write and
measured read phase lasts at least ten seconds. Read concurrency is 640 globally,
with a two-second warmup and ten-second measurement.

Cases below are single observations, not repeated-run medians. Diagnostic and
profiled throughput must not be pooled with uninstrumented throughput. The
experiments target specific hypotheses; this is not another five-size matrix.

## Fresh controls and footer ablation

These cases insert 524,288 nominal records (32 GiB of value bytes) per device,
using the original owner placement: eight owners share one 32-MiB L3, eight
share a second, and four share a third. The first two groups are on one NUMA
node and the third on the other.

| Case | Fsync | Write GiB/s | Write seconds | Physical write requests |
|---|---|---:|---:|---:|
| Original | on | 36.598 | 17.487 | 10,498,740 |
| Current | on | 35.829 | 17.863 | 10,509,540 |
| Current | off | 34.655 | 18.468 | 10,509,540 |
| Current, 1-MiB footer chunks | on | 35.806 | 17.874 | 10,498,740 |
| Current, 1-MiB footer chunks | off | 36.504 | 17.532 | 10,498,740 |

Increasing the footer-body cap from 64 KiB to 1 MiB removes the 10,800 extra
physical write requests. It preserves final commit-page and sync ordering.
With fsync enabled, measured throughput barely changes. The disabled sample
improves, but the fresh disabled control itself was slower than the enabled
control. This variability prevents attributing that improvement to the footer
change from one observation. Physical bytes remain the same.

## Lifecycle boundary timing

Separate diagnostic binaries time lifecycle jobs and their control requests,
not every record. Values below are medians across twenty owners in one process,
not medians across repeated processes. Each owner performs 36 actual seals and
37 allocation/rollover jobs, including the first allocation.

| Variant | Entire rollover time, ms | Sync wait, ms | Footer-body I/O wait, ms | Lifecycle CPU step time, ms |
|---|---:|---:|---:|---:|
| Original | 68.690 | 0.125 | 14.644 | not separately measured |
| Current, fsync on | 60.610 | 4.403 | 18.844 | 26.329 |
| Current, fsync off | 54.125 | 0 | 18.319 | 24.818 |
| 1-MiB footer chunks, fsync on | 73.022 | 3.748 | 11.958 | 46.194 |
| 1-MiB footer chunks, fsync off | 69.183 | 0 | 11.777 | 45.546 |

The slowest owner's write phase is about 18 seconds. Current-enabled rollover
time is at most 64.683 ms per owner; direct sync wait is at most 5.012 ms.
These intervals cannot directly account for a several-hundred-millisecond gap.
Do not sum times across twenty concurrent owners and compare the sum with
whole-process wall time. Indirect effects outside a job's measured interval
are not ruled out by this measurement.

The original timer starts after its idle check, whereas the current job timer
includes its drain. Sync/body/page wait categories overlap the encompassing job
interval; they are not additional costs. The driver's final common sync is
inside the whole write phase but outside the lifecycle sync category. Larger
footer allocations reduce I/O waits while increasing lifecycle step time.

These measurements supersede the earlier suggestion that footer serialization
or direct fsync waiting was the leading explanation for the full gap.

## Poll budget and sampled CPU work

A diagnostic variant changes only `Options.poll.operations` from 64 to 256.
The unprofiled fsync-on case reaches 34.625 GiB/s, providing no evidence of
recovery relative to the 35.829-GiB/s fresh current control. This single negative
result does not prove that poll accounting has zero cost.

The fsync-off budget case was sampled with `perf record`, at 199 Hz for five
seconds during the write phase. Its 32.124-GiB/s result is explicitly excluded
from throughput comparisons because of sampling overhead. Approximately 44.38%
of sampled user-cycle weight falls at libc's `rep movsb` instruction, and
15.61% at `rep stosb`, confirmed by disassembling the installed libc. These are
copy/fill work, not evidence of heap allocation or allocator reclamation.

A further pair samples one in every 67 prepared-write calls, separately timing
pool allocation, prepared-buffer copy, and engine submission. Poll calls are
sampled independently at the same interval. Sampling is inside each owner and
reported once before verification. These binaries also record each owner's
whole write duration. They do not modify the engine's algorithms.

| Version / owners sharing L3 | Pool alloc, ns | Prepared copy, ns | Engine submit, ns | Poll, ns |
|---|---:|---:|---:|---:|
| Baseline / 8 | 135 | 16,183 | 3,360 | 357 |
| Baseline / 4 | 77 | 3,089 | 1,437 | 146 |
| Current / 8 | 132 | 15,715 | 2,378 | 399 |
| Current / 4 | 75 | 3,130 | 1,157 | 178 |

Cells are medians of per-owner sampled means. The copy interval includes
`PreparedFrame::new` and payload copying. The submission interval includes
checksum/metadata work; original submission can also include synchronous
rollover. Periodic sampling is approximate and can perturb code generation
and timing. It is not valid to subtract these instrumented process throughputs
to claim an engine improvement. In fact, this pair reverses the throughput
ordering: current 36.352 versus original 33.717 GiB/s.

The roughly fivefold copy-time difference between dense and sparse L3 groups
is much larger than the allocation cost. Independent lifecycle probes likewise
show the four sparse owners finishing in roughly 6.4 seconds while the sixteen
dense owners need roughly 17–18 seconds. Registered pool mappings are distributed
16/4 across the same NUMA nodes; there is no evidence that all pools accidentally
landed on one remote node. Both explicit huge-page inventories are empty and
startup uses the transparent-huge-page fallback. NUMA-map inspection does not
by itself establish the exact transparent-huge-page coverage.

This identifies CPU/memory work as the dominant measured hot path. Cache
contention is a supported explanation of the owner split, not a measurement
of cache-miss cost or a complete attribution of the revision difference.

## Placement and input-layout controls

The next group distributes twenty owners evenly across four L3 groups, ten
owners per NUMA node. Six device owners consequently change NUMA locality
relative to their device; this is not a pure L3-only intervention. The dataset
increases to 851,968 nominal records (52 GiB of value bytes) per device to keep
writes over ten seconds. Baseline and both current policies use identical
placement and counts within this group. Comparing this group's absolute
throughput with the 32-GiB group does not isolate placement alone.

| Case | Fsync | Write GiB/s | Write seconds | Physical write requests |
|---|---|---:|---:|---:|
| Original, distributed owners | on | 44.592 | 23.323 | 17,060,260 |
| Current, distributed owners | on | 42.879 | 24.254 | 17,077,660 |
| Current, distributed owners | off | 44.772 | 23.229 | 17,077,660 |

The disabled-policy sample is within 0.5% of the original, while the enabled
sample is still about 3.8% lower. Direct lifecycle waiting measured in the
original placement cannot by itself explain that enabled-policy difference.

An additional paired control changes only the reusable 64-KiB source payloads
from ordinary `Vec` allocations to page-aligned `AlignedBuf` allocations made
before timing. Both still use glibc; payload refill, key/payload copies,
checksums, I/O pools, and on-disk encoding remain unchanged. This tests
sensitivity to source address layout without changing the engine. Placement
and 52-GiB dataset sizes match the immediately preceding group.

| Case | Fsync | Write GiB/s | Write seconds | Physical write requests |
|---|---|---:|---:|---:|
| Original, aligned sources | on | 54.828 | 18.968 | 17,060,260 |
| Current, aligned sources | on | 55.238 | 18.828 | 17,077,660 |
| Current, aligned sources | off | 55.370 | 18.783 | 17,077,660 |

The enabled-policy gap disappears in this matched page-aligned comparison. This changes
source placement/alignment and associated allocation layout, not just a logical
buffer-alignment flag; the exact microarchitectural cause is not proven.

The final benchmark generalizes aligned sources to all pooled prepared writes,
rounds backing allocations to pages, and excludes padding from logical lengths.
It reports `DRIVER.input_alignment`; the analyzer separates historical natural
alignment from page alignment. Full refill and payload copies stay timed.
Small combined sources and the owned-input/Foyer paths retain their behavior.

The final implementation is checked below using fresh matching builds of both
engine revisions, the same four-L3 owner placement, and the same 52-GiB dataset.
These are uninstrumented results from the source-pool implementation now in the
repository, rather than the earlier 64-KiB-only diagnostic variant.

| Case | Fsync | Write GiB/s | Write seconds | Physical write requests |
|---|---|---:|---:|---:|
| Original, final source pool | on | 53.359 | 19.491 | 17,060,260 |
| Current, final source pool | on | 55.300 | 18.807 | 17,077,660 |
| Current, final source pool | off | 53.935 | 19.283 | 17,077,660 |

The six benchmark unit tests pass, including retained allocations, page
alignment, full refilling, and non-page-aligned logical lengths. Clippy with
warnings denied and formatting checks pass. An analyzer integration check
confirms historical natural-alignment and new page-alignment logs remain in
separate groups.

## Coverage and artifacts

All 23 processes completed, with 460 positive-write/positive-read
process/device pairs and 300,154,880 records checked for full key,
value length, prefix, and suffix before timed reads. These are not exhaustive
value-byte checks or power-loss tests. The shortest measured write is
17.487 seconds. Every measured read is at least ten seconds.

All twenty devices were idle with no open users after completion. Device
controllers remained live and both system mirrors remained intact. SMART was
unavailable to the benchmark account. No kernel, global allocator, or device
cache configuration was changed. Private host identities, device paths,
allowlists, raw logs, and operational scripts remain outside the repository.

- [samples.csv](samples.csv): every observation, with diagnostic/profile marking.
- [devices.csv](devices.csv): all twenty devices per case, using anonymous ordinals.
- [probes.csv](probes.csv): per-owner lifecycle timings and request counts.
- [hotpath.csv](hotpath.csv): sampled component timings, counts, and owner durations.
- [build.json](build.json): exact frozen and experimental binary hashes.
- [validation.json](validation.json): completion, duration, and coverage checks.

The production change is limited to benchmark source storage and reporting.
Footer chunk sizing and poll budgets remain unchanged: their diagnostic variants
did not justify an engine change. The result supports comparing revisions with
controlled source alignment and CPU placement, rather than assigning the earlier
small gap to the asynchronous lifecycle design. Single observations do not
establish sub-percent significance or performance for other value sizes.
