# Twenty-device ablation and cleanup comparison

The cleanup removes redundant length/capacity checks, an unused lifecycle argument, and duplicate footer encoding. The benchmark retains reusable, page-aligned prepared-write sources. All reported cells use all twenty data devices.

## Complete comparison

Write throughput:

| Value size | Unit | Original, fsync on | First commit, fsync on | Cleanup, fsync on | Cleanup, fsync off |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 58.547 | 63.371 | 63.293 | 62.691 |
| 1 KiB | Mops/s | 45.106 | 45.104 | 45.308 | 46.036 |
| 4 KiB | Mops/s | 25.695 | 25.331 | 26.629 | 26.000 |
| 64 KiB | GiB/s | 53.726 | 55.060 | 52.803 | 55.678 |
| 4 MiB | GiB/s | 55.481 | 54.920 | 54.905 | 55.843 |

Read throughput:

| Value size | Unit | Original, fsync on | First commit, fsync on | Cleanup, fsync on | Cleanup, fsync off |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 9.187 | 8.754 | 8.554 | 8.441 |
| 1 KiB | Mops/s | 9.272 | 9.234 | 9.261 | 9.248 |
| 4 KiB | Mops/s | 8.343 | 8.328 | 8.328 | 8.314 |
| 64 KiB | GiB/s | 187.347 | 187.321 | 187.275 | 187.464 |
| 4 MiB | GiB/s | 233.049 | 233.049 | 232.988 | 233.037 |

The 100-B read cells include memory-probe interference; see the dedicated read-observer diagnostic below before interpreting the revision differences.

Process peak RSS (GiB, including benchmark records and engine pools/indexes):

| Value size | Original | First commit | Cleanup, fsync on | Cleanup, fsync off |
|---|---:|---:|---:|---:|
| 100 B | 258.626 | 265.939 | 265.942 | 265.943 |
| 1 KiB | 218.496 | 227.810 | 227.883 | 227.938 |
| 4 KiB | 74.706 | 79.076 | 79.074 | 79.082 |
| 64 KiB | 14.037 | 14.249 | 14.251 | 14.251 |
| 4 MiB | 10.141 | 10.148 | 10.148 | 10.146 |

The current engine retains an additional `Vec<ChunkId>` to support bounded version cursors while writes continue. Its key payload is 16 bytes per indexed version, before spare capacity. This is a real memory tradeoff of the new traversal contract; whole-process RSS also includes the driver, hash tables, pools, and allocator retention, so the RSS difference is not an isolated allocation measurement. The cleanup preserves the cursor contract and this structure.

## One-change ablations

| Treatment | Value size | First commit | Treatment | Change |
|---|---|---:|---:|---:|
| Reuse computed frame length | 65,536 B | 55.060 GiB/s | 54.783 GiB/s | -0.50% |
| Submit control without vacancy precheck | 65,536 B | 55.060 GiB/s | 55.626 GiB/s | +1.03% |
| Remove prepared-source page alignment | 65,536 B | 55.060 GiB/s | 42.708 GiB/s | -22.43% |
| Always count distinct absent keys | 4,096 B | 25.663 Mops/s | 26.166 Mops/s | +1.96% |
| Always count distinct absent keys | 100 B | 63.371 Mops/s | 51.927 Mops/s | -18.06% |

The two redundant-check removals stay close to the baseline in this single round. They are retained as simplifications, not advertised as measured speedups. Removing source alignment reduces 64-KiB write throughput materially; page-aligned prepared-write sources remain in the benchmark. This sensitivity comes from the caller's source buffers and does not change the engine's direct-I/O buffer requirements.

The index fast path is retained. Always counting distinct absent keys is slightly faster in the 4-KiB observation, but reduces 100-B write throughput from 63.371 to 51.927 Mops/s (18.06%). The exact path performs index probes and builds a temporary distinct-key set for each admitted frame. Its fallback remains necessary near the resource limit so overwrites and duplicate keys are counted correctly; using it unconditionally adds material small-value cost.

## Targeted 64-KiB confirmation

The first main sample put cleanup-enabled throughput 4.10% below the first commit. A fresh adjacent pair checks whether that difference repeats. Both use all twenty devices, 52 GiB of values per device, identical source pools/placement, and fsync enabled. The confirmation does not replace the main table.

| Observation | First commit, GiB/s | Cleanup, GiB/s | Cleanup change |
|---|---:|---:|---:|
| Main comparison | 55.060 | 52.803 | -4.10% |
| Adjacent confirmation | 54.102 | 54.339 | +0.44% |

The adjacent pair measures 54.102 GiB/s for the first commit and 54.339 GiB/s for cleanup (+0.44%). CPU time is also close: 20.430 versus 20.408 microseconds per written record. The initial 4.10% cleanup gap does not repeat in this control, so it is not evidence of a persistent cleanup regression. One extra pair does not establish a confidence interval or a sub-percent improvement; both observations remain published.

## Read-observer diagnostic

The main 100-B read observations range from 8.441 to 9.187 Mops/s despite almost identical CPU cost per read (about 2.18 microseconds) and p99 latency (about 112.5 microseconds). The slower current samples use only about 18.4–19.0 process CPU cores, versus 20 for the original. The throughput controller collected `smaps_rollup` and `numa_maps` after verification, allowing the scan to overlap warmup or the next measured phase. These 100-B main-table readings are not a clean basis for attributing an engine regression.

A separate cleanup-enabled process uses the same twenty devices and 100-B dataset, verifies all 1,006,632,960 records, and performs two consecutive ten-second read phases, each with its own two-second warmup. A single memory scan begins one second after verification is observed. It runs from +1.007 to +6.433 seconds; the estimated first measured read interval is +3.721 to +13.721 seconds. Thus the scan overlaps the first measurement by about 2.7 seconds. Read boundaries are observed at 250-ms intervals; these are approximate overlap times.

| Read phase | Memory scan | Mops/s | Process CPU cores | CPU microseconds/read | p99 microseconds |
|---|---|---:|---:|---:|---:|
| First | Overlaps measurement | 8.809 | 19.207 | 2.1805 | 112.575 |
| Second | None | 9.169 | 19.999 | 2.1811 | 112.575 |

The same binary/process recovers to within 0.2% of the original's 9.187 Mops/s without the scan. This supports observer interference rather than increased CPU cost in the engine read path. The pair is ordered, and it does not trace the exact kernel/allocator blocking mechanism or establish a statistical confidence interval. The original lower readings remain in the main table; the diagnostic does not replace them.

Throughput collection now omits detailed memory-map scans and uses the driver's existing peak-RSS metric. Memory-map collection belongs in a separate diagnostic. See [both read samples](observer-samples.csv), [all twenty device counters](observer-devices.csv), and [probe timing and validation](observer-validation.json). This diagnostic adds one process and two read phases to the twenty-eight matrix/ablation processes.

## References and workload

- **Original:** engine `f76d1dcf8f290261af2860321d040d3f0b87df93`, the unified engine immediately before this asynchronous lifecycle change. This is not an earlier cache design.
- **First commit:** `a55205f664c040b6a2af7eaa206b9336d2b05902`, including page-aligned prepared-write source pools.
- **Cleanup:** the production sources identified in [build.json](build.json), measured with engine fsync enabled and disabled. Disabled mode includes explicit flush: it remains a write-completion fence without issuing fsync.
- All builds use the same input-pool benchmark and system glibc, release optimization, and `x86-64-v3`. Only the original's engine and API adapter use its old interfaces. There is no allocator replacement, preload, allocator tuning, or profiling during these measurements.

Every process uses all twenty authorized data devices, a 64-GiB write window per device, 1-GiB segments, a 512-MiB registered I/O pool per device, queue depth 256, 16-byte keys, and a local write batch of 128. Huge pages use the preferred policy. Reads use the trusted-index path with payload CRC verification disabled, while the driver checks returned keys, lengths, and identity stamps. Read concurrency is 640 globally, with a two-second warmup followed by at least ten measured seconds. All writes also run for at least ten seconds. Each main comparison cell is a single observation, not a median or confidence interval. A separate two-process 64-KiB confirmation checks the cleanup against a fresh first-commit control.

| Value size | Nominal unique records/device | Value bytes/device |
|---|---:|---:|
| 100 B | 50,331,648 | 4.6875 GiB |
| 1 KiB | 33,554,432 | 32 GiB |
| 4 KiB | 14,680,064 | 56 GiB |
| 64 KiB | 851,968 | 52 GiB |
| 4 MiB | 13,312 | 52 GiB |

The separate 4-KiB index ablation uses 13,631,488 records/device (52 GiB) for both sides. The main comparison uses 56 GiB to provide more timing headroom. Never compare these as equal-work timing samples.

Source pools are created and touched before timing, with at most 64 reusable values per owner and an approximately 4-MiB target. Prepared-write source payloads (64 KiB and 4 MiB in this matrix) are page aligned. Smaller packed values use the existing reusable vectors. Timed work still includes source refill, copying into engine buffers, checksums, index publication, physical writes, rollovers, and the final write fence. Device formatting, unique-record preparation, full-record verification, and the final active-segment seal at close are outside write timing. Preparing and verifying roughly one billion records makes the 100-B case much longer in wall-clock time than its measured write phase.

All cases use the same owner placement: five owners in each of four L3 groups, ten owners per NUMA node, and owner-local input preparation. Compared with the older three-group placement, six devices now have remote owners. This is therefore not an isolated L3 experiment, and historical rates from that placement are not interchangeable with this matrix.

The first-commit 64-KiB and 100-B controls are reused from this task's ablation runs: binaries, record counts, placement, fsync policy, and timing match exactly. The main comparison has twenty cells; eight additional diagnostic cells produce twenty-eight matrix/ablation processes. One subsequent 100-B process performs the separate read-observer diagnostic with two read phases. There is no second or third complete round. The execution order is the six initial ablation cases, two 100-B index cases, then the remaining matrix in descending size order, with original, first commit, cleanup enabled, and cleanup disabled within each size (reused cells omitted), followed by the two-process 64-KiB confirmation.

## Cleanup decisions and format coverage

The production cleanup removes the repeated prepared-frame length calculation and the preliminary control-queue vacancy query. The already-computed length is reused; queue submission itself remains authoritative and returns the unchanged request on backpressure. The lifecycle transition's unused input parameter is removed. Both footer encoders now share sealed-header construction and trailer-field encoding.

The footer parity test compares incremental and contiguous output byte for byte for empty, one-page, and multiple-chunk footers, including both sides of padding and 64-KiB chunk boundaries. It also validates offsets, the final commit page, and footer decoding. CRC placement, disk layout, chunk cap, resource bounds, asynchronous progress budgets, and error handling are preserved. Redundant work can be removed for clarity even when its isolated throughput effect is within run-to-run variability; this report does not assign a measured speedup to each such deletion.

The earlier [footer and poll-budget controls](../64k-followup/REPORT.md) found no stable throughput benefit from a 1-MiB footer cap or a larger lifecycle polling budget. Those experimental changes are not included. No cache allocation design is changed here.

For the 64-KiB matrix, all four variants write exactly 1,258,577,674,240 physical bytes across the twenty devices. The original completes 17,060,260 physical write requests; the first commit and both cleanup modes complete 17,077,660. The 17,400-request difference is the existing incremental footer split, not a new layout or cleanup change.

## Reproducing the source variants

Use separate clean checkouts and the generic configuration documented by [the benchmark](../../../../../benchmarks/cache-disk/README.md). The four one-change diagnostic patches apply individually to `a55205f`; do not combine them. To recreate the original engine with the matched harness, start from `a55205f`, replace the complete `core/moat-engine`, `core/moat-server`, and `core/moat-cache-store` trees with their `f76d1dc` versions, then apply [the original adapter patch](patches/original-adapter.patch). The harness main, workload, and input code stay identical to the first commit. Build each variant with `RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --locked --release --target x86_64-unknown-linux-gnu --manifest-path benchmarks/cache-disk/Cargo.toml`. Use separately authorized scratch devices and equivalent topology/resource settings; no deployment configuration is included here.

## Evidence and limitations

[Samples](samples.csv) contain every matrix/ablation process's rates, timing, CPU per record, read p99, process peak RSS, physical write totals, and sampled io-wq count. [Device counters](devices.csv) contain all twenty anonymous device ordinals for every process. [Build identities](build.json) pin original/first-commit revisions, all measured binaries, and cleanup source hashes. The [diagnostic patches](patches/) apply individually to the first commit; they are historical experimental variants, not production code. The natural-alignment diagnostic intentionally changes only source allocation; its inherited driver label is overridden by the variant identity in the published samples.

Each complete run verifies every nominal record's key, length, and identity stamps before measured reads. All twenty devices must have positive write and read counters. The physical write volume is checked against the authorized per-device extent. This is not a power-loss or media-endurance test. Fsync-disabled throughput does not establish durable completion, and PLP is not detected automatically.

Final validation checks device identities, live controllers, no open users, two seconds without I/O, and intact system mirrors. SMART counters are unavailable under the benchmark account's permissions; controller checks do not substitute for SMART or a power-cut test. Public artifacts omit deployment identities, device paths, serials, raw configurations, raw process traces, and remote-control automation.

The workload is a finite unique-insert fill followed by reads. It does not measure steady-state overwrite/reclamation, mixed-size traffic, cache eviction, or power-loss recovery. Near-equal single observations should be treated as parity at this resolution. Read throughput in GiB/s is logical value throughput and excludes keys and format overhead.

## Validation totals

All 28 main-comparison and ablation processes exited successfully, with 560 device/write/read counter pairs. The separate read-observer diagnostic adds one twenty-device process and two measured read phases; its counters and timings are published separately. The shortest measured write was 10.419 seconds and the shortest measured read was 10.000251 seconds.

The maximum sampled io-wq count across cleanup runs is 20 workers with fsync enabled and 0 with fsync disabled. Sampling occurs every 250 ms across the process lifetime; these observations are not a configured kernel worker limit.

The cleanup passed 256 workspace tests (one ignored), six disk-benchmark tests, workspace and benchmark Clippy with warnings denied, rustdoc with warnings denied, Rust formatting, and spelling checks. See [final device validation](validation.json) for release and health-check scope.
