# Engine optimization and experiment archive

This directory preserves the designs, measured revisions, complete numeric
samples, profiles, negative results, and interpretation limits from the engine
work. The active comparison drivers remain under `benchmarks/`; historical
results belong here instead of beside executable code.

## Experiment chronology

| Record | Question and outcome |
|---|---|
| [Initial cache comparison](cache-disk/2026-09-10/REPORT.md), [identity costs](cache-disk/2026-09-10/IDENTITY.md), [rechecks](cache-disk/2026-09-10/RECHECK.md) | Retains the original single-device and twenty-device comparisons, identity work, and workload sensitivity. |
| [Verified engine reads](engine/2026-09-15/REPORT.md) | Establishes the initial v1/v2 pipeline comparison with read verification enabled, including the regressions and fio reference. |
| [Direct and range reads](engine/2026-09-15-direct/REPORT.md) | Measures trusted-index reads and partial ranges separately from verified reads. Removing mandatory read CRC substantially changes the comparison. |
| [Registered I/O and huge pages](engine/2026-09-15-registered/REPORT.md) | Measures both engines with fixed buffers/files and matching huge-page policies; retains actual backing observations and both policy matrices. |
| [Full-device fill](engine/2026-09-16-full/REPORT.md) | Records a complete single-device fill and random reads, including v2's lower write throughput and transition-cost limits. This is distinct from the twenty-device experiments. |
| [Twenty-device application adapters](cache-disk/2026-09-16-20disk/REPORT.md), [profiles](cache-disk/2026-09-16-20disk/PROFILING.md) | Retains the original foyer/v1/v2 application comparison and scheduler/allocation/copy profiles. |
| [Batching and input experiments](cache-disk/2026-09-16-bottlenecks/REPORT.md) | Separates admission batching, intermediate copies, prepared in-place producers, and large-frame packing. All 89 completed trials remain available. These producer modes are not equivalent APIs. |
| [Native polling adapters](cache-disk/2026-09-16-native/REPORT.md), [profiles](cache-disk/2026-09-16-native/PROFILING.md) | Drives v1/v2 on their owning threads, with Tokio used only by foyer. Retains 45 prefills and 75 measured read phases. The adapter changes more than lock overhead. |
| [io-wq boundary investigation](cache-disk/2026-09-16-native/IO_WQ.md) | Reproduces offload at the device request byte limit and validates the actual split queue. Huge pages do not remove that byte limit. Probe results establish behavior, not throughput. |
| [Twenty-device request splitting](cache-disk/2026-09-16-split/REPORT.md) | Reruns all three engines after v2 splits oversized requests. Retains 45 prefills, 75 reads, phase observations, and the weaker results. |
| [V2 twenty-device migration recheck](cache-disk/2026-09-17-v2/REPORT.md) | Measures native v2 after migration: 15 prefills and 25 reads, historical comparison, write variability, and explicit cache API and SMART coverage limits. |
| [Write-regression investigation](cache-disk/2026-09-17-v2/investigation/REPORT.md) | Rechecks all five sizes with order controls, longer writes, per-owner timing, expanded unique inserts, engine/harness isolation, and profiles; preserves the unresolved smaller 64-KiB result. |
| [V2 ablation and cleanup](engine/2026-09-16-ablation/REPORT.md) | Removes one feature at a time, brackets treatments with fresh controls, and records the resulting implementation decisions. |
| [Footer recovery ablation](engine/2026-09-17-footer-ablation/REPORT.md) | Removes route over-reservation and bulky baseline attachments; retains measured allocation/read savings and fault coverage. |
| [Asynchronous lifecycle comparison](engine/2026-09-18-async-lifecycle/REPORT.md) | Paired twenty-device runs: 48 prefills and 68 reads, unchanged main read throughput, write regressions, and frequent-rollover results. |
| [Async lifecycle regression investigation](engine/2026-09-18-async-lifecycle/investigation/REPORT.md) | 87 twenty-device diagnostic runs distinguish write-budget accounting, temporary-value allocator trimming, index overhead, and asynchronous fsync workers; retain negative results and attribution limits. |
| [TiKV jemalloc experiment](engine/2026-09-18-async-lifecycle/jemalloc/REPORT.md) | Historical optional allocator integration, subsequently removed; twenty-device comparisons including small-value regressions, RSS, and reclamation traces. |
| [System allocator and optional fsync](engine/2026-09-18-async-lifecycle/libc-fsync/REPORT.md) | Removes jemalloc integration, adds initialization-time fsync policy, and compares the original/current enabled/current disabled on all twenty devices; retains first-write timing and input-reuse controls for remaining regressions. |
| [64-KiB root-cause controls](engine/2026-09-18-async-lifecycle/64k-root-cause/REPORT.md) | Bidirectional trim-threshold controls reproduce the loss in the original and recover the current binary; per-owner timers, minor faults, and syscall traces identify temporary input page reclamation and repopulation. |
| [Preallocated benchmark inputs](engine/2026-09-18-async-lifecycle/input-pool/REPORT.md) | Adds bounded source pools before timing; both original/current use the same harness in the fifty-five-process twenty-device recheck, with owned-input controls and reclamation/fault evidence. |
| [Longer twenty-device comparison](engine/2026-09-18-async-lifecycle/large-batch/REPORT.md) | One complete fifteen-case round with at least ten measured seconds per write/read phase; larger unique datasets and matching 1-GiB segments retain a smaller 64-KiB write gap. |
| [64-KiB lifecycle and input-layout follow-up](engine/2026-09-18-async-lifecycle/64k-followup/REPORT.md) | Boundary timers exclude direct footer/sync waits as the main gap; all-twenty-device controls isolate copy costs and remove the observed revision gap with page-aligned source pools. |

## Optimization inventory

| Mechanism | Implementation or evidence | Scope |
|---|---|---|
| Unified immutable frames and mixed-size placement | [Frame design](../design/engine-frame-layout.md), [engine crate](../../core/moat-engine/README.md) | Fixed 8-byte value alignment and checksummed metadata/payload blocks are format contracts. The proposed gap-filling arrangement remains unimplemented. |
| Bounded incremental frame admission | `FrameBuilder` and frame tests | Common admission uses a conservative constant-time bound with an exact fallback. Correct format limits are independent of the batching target. |
| Direct range reads with optional verification | [Direct-read report](engine/2026-09-15-direct/REPORT.md) | The caller selects verification through `read(..., verify, ...)`. Checked reads and recovery retain validation. |
| Single-owner index, requests, and publication | [I/O pipeline design](../design/engine-io-pipeline.md) | Ordering, backpressure, buffer lifetime, and failure propagation are correctness requirements. The native driver does not add per-request runtime/channel bridges. |
| Common pool, huge-page policy, and registered I/O | [Registration report](engine/2026-09-15-registered/REPORT.md) | Pools reuse `moat-common`; actual backing must be measured separately from allocation hints. |
| Deferred ring work and batched submission | [V2 ablation](engine/2026-09-16-ablation/REPORT.md) | Supported-kernel behavior and compatibility fallback are separate concerns. A result on one supported kernel does not justify deleting the fallback. |
| Prepared writes and ownership-preserving retries | [Input experiments](cache-disk/2026-09-16-bottlenecks/REPORT.md) | The prepared API remains available to producers that can fill the final allocation. Current owned-input comparisons retain the required final copy. |
| Native caller and batched foyer adapter | [Native report](cache-disk/2026-09-16-native/REPORT.md) | Only foyer uses Tokio. Routing and generation placement changed as well, so historical gains are not an isolated mutex measurement. |
| Device-limit splitting and completion aggregation | [Split report](cache-disk/2026-09-16-split/REPORT.md) | Disjoint SQEs share a buffer; it remains owned until every part completes. Physical depth is bounded independently of the number of chunks per logical request. |
| Device rollover, allocation identity, and sealing | [Lifecycle design](../design/engine-device-lifecycle.md), [full fill](engine/2026-09-16-full/REPORT.md) | These preserve append/recovery semantics. Reclamation policy and physical reuse remain outside this stage. |

## Reproduction and preservation

Every historical report retains its measured source revision, build identity,
workload, and interpretation limits. To reproduce retired input diagnostics,
check out the revision named by that report; do not pass retired options to the
current driver. The current owned-input comparison is intentionally a single
path. Frame construction and the engine's prepared-write APIs still support
those producer choices.

The [archive manifest](archive-manifest.csv) maps all 63 pre-existing artifacts
from revision `3464483d3547286ba35669c56693d59f69fdc75d` to their new locations.
All historical CSV files are byte-identical. Markdown changes relocate links
and generalize deployment inventory; they do not revise measured outcomes.
Original and archived SHA-256 values make those changes explicit.

Public records contain generic build/workload parameters and numeric results.
Operational hostnames, accounts, absolute local paths, network addresses,
device identities, CPU identifiers, allowlists, raw configurations, and
remote-control scripts stay outside the repository. System inventory is
omitted; workload resource counts, test extents, and relevant I/O limits are
retained because they define the experiment. Normal output from the current
cache comparison uses anonymous device ordinals and an explicit parameter
allowlist. Source revisions remain historical references, not a request to
reuse an operator's configuration on another machine.

Different API ownership, checksum, batching, and durability semantics remain
explicit in each report. Do not combine separate experiments into a result
that was never measured. Small prefills are bounded bursts, process CPU is
not whole-machine CPU, and sampled thread counts are not kernel traces.
