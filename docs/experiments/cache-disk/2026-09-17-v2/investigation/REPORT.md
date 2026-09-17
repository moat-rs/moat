# Investigation of write-throughput changes

**The measurements do not establish an overall engine regression.** The 100-B apparent 15.6% drop is highly sensitive to final open/sync latency in a 40–60-ms burst. Longer paired writes are near parity or faster. A smaller **64-KiB unique-insert concern remains workload/session dependent**: one expanded experiment is about 3% slower, while a later three-way isolation and profiling comparison does not reproduce it. The negative samples are retained; no engine change is justified as a fix by these results.

## All five sizes

The initial rotating schedule did not balance binary order within every value size. Additional reversed-order controls were therefore added. Each row below gives old-first and current-first groups equal weight: it is the arithmetic mean of their median paired throughput changes. All samples, including the excess pairs in the original direction, remain in the CSVs. These estimates are not confidence intervals.

| Value | Short burst change | Longer write change | All longer pair range |
|---|---:|---:|---:|
| 100 B | -5.07% | -0.11% | -1.22% to +2.02% |
| 1 KiB | -1.00% | +0.23% | -0.56% to +1.23% |
| 4 KiB | -3.56% | +0.23% | -2.79% to +5.05% |
| 64 KiB | -0.89% | +1.99% | +0.40% to +4.59% |
| 4 MiB | +0.03% | +1.90% | +0.83% to +3.15% |

Longer writes append the same key set 32 times for 100 B through 64 KiB, and four times for 4 MiB, then sync. Index cardinality stays fixed; this is an overwrite/rollover diagnostic and cannot alone rule out a first-insert regression. The unmodified short-burst controls still include large unfavorable outliers (100 B -23.57%, 4 KiB -9.89%), which is why the report retains the full ranges and phase diagnostics.

[All paired changes](all-pairs.csv), [separate ordering groups](paired-by-order.csv), [equal-order estimates](order-balanced.csv).

## 100 B: the final synchronization interval dominates variance

The original burst contains 2,621,440 records across twenty devices. Historical and initial-current runs both use approximately **0.278 CPU microseconds per record**, even when their wall-time throughput differs by 15%. Fresh unmodified initial controls give 59.888 Mops/s old and 59.737 Mops/s current, versus the earlier current median of 49.927 Mops/s.

The instrumented current binary demonstrates the mechanism directly:

| Current diagnostic | Total | Median owner write loop | Last owner open/sync | Throughput |
|---|---:|---:|---:|---:|
| Round 1 | 39.70 ms | 36.67 ms | 2.92 ms | 66.04 Mops/s |
| Round 2 | 46.90 ms | 37.14 ms | 9.69 ms | 55.89 Mops/s |
| Round 3 | 48.75 ms | 37.12 ms | 11.59 ms | 53.78 Mops/s |

Owner dispatch takes at most 0.05 ms in these samples. Similar write-loop work plus several milliseconds of final open/sync delay produces a large throughput swing. The recorded interval includes opening the device and syncing it; it does not identify which kernel/controller step caused the delay. The original uninstrumented slow runs have no per-owner timestamps, so their exact individual delays cannot be reconstructed. The timing diagnostic reproduces a mechanism of sufficient size, rather than proving every historical outlier has the same cause.

[Per-process timing](timing.csv), [owner measurements](diag-workers.csv), [historical CPU/time samples](historical-writes.csv).

## 64 KiB: expanded unique inserts and isolation

The unique-key follow-up expands the original 8,190 mean records/device by 16 to **131,040**, about 8 GiB of value payload per device. It uses the original unmodified binaries and includes key/value generation, all first insertions, rollover, and final sync. Six paired comparisons, covering both execution orders, give an equal-order estimate of **-2.98%** for current. That negative result is not replaced by the favorable overwrite result.

A later three-way experiment rotates the order of old, current, and a mixed build across three rounds. The mixed build uses the historical benchmark/upper crates with the current engine. It therefore tests whether the apparent difference follows engine changes or harness/upper-crate changes.

| Build | Unique-insert median |
|---|---:|
| Old harness + old engine | 35.192 GiB/s |
| Old harness + current engine | 35.550 GiB/s |
| Current harness + current engine | 35.824 GiB/s |

In this block, current and mixed are not slower than old. The same old binary ranges from 34.513 to 36.961 GiB/s across these three processes. The isolation therefore does not identify an engine or harness regression. It also does not prove that the earlier roughly 3% unique-insert decrease is harmless noise; small workload-dependent differences remain unresolved.

Write-only profile windows show existing payload copies and value initialization/padding accounting for roughly **66–68%** of sampled user cycles on both builds. No new dominant hot path appears. Profiled throughput is 34.442 GiB/s old versus 34.702 GiB/s current; profiling samples are excluded from throughput aggregates. Symbol classification uses call stacks and the corresponding libc copy/fill instructions.

[Unique-key samples](unique-write-samples.csv), [reverse-order samples](order-unique-write-samples.csv), [three-way isolation](isolation.csv), [profile timing/cycles](profiles.csv), [profile categories](profile-hotspots.csv).

## Coverage, reproduction, and decisions

- 141 unprofiled processes: 52 original-size bursts, 52 repeated-write diagnostics, 16 per-owner timing cases, 12 expanded unique-key fills, and nine isolation cases. Two additional processes collect profiles.
- All prefills complete, all prefilled keys pass identity/length verification, and every measured read phase exercises all twenty devices. No write amplification increase is observed: burst physical bytes are identical for 100 B through 64 KiB; footer-layout changes cause only tiny differences in longer fills.
- All cases reuse the authorized 64-GiB window per device and historical CPU assignments, queue depths, pools, checksum policy, and deterministic key routing. Each process verifies the dataset, warms reads for two seconds, and reads at 640 clients for one second after writing. No discard or kernel/CPU tuning is performed.
- Device identities, capacity, system exclusions, partitions, mounts, holders, signatures, open users, idle counters, and cooperative locks are checked. Before/after checks find twenty live controllers and both system RAID mirrors intact. SMART remains unavailable to the account.
- Rust 1.98.0 release, GNU target, `-C target-cpu=x86-64-v3`. [Exact binary/source identities](build.json), [old long-write patch](old-long-workload.patch), [current long-write patch](current-long-workload.patch), [old short timing delta](old-short-from-long.patch), [current short timing delta](current-short-from-long.patch). The normal workspace benchmark binary is restored to its original measured hash.
- No production code was changed during the investigation. Avoid treating sub-second prefill rates as sustained engine capacity or comparing separate sessions as causal evidence. Preserve final sync in durability-inclusive timing, and use adjacent old/current controls with both orders; report unique-insert and overwrite workloads separately.
- This remains a native engine benchmark. It does not exercise the migrated cache-store Session or establish its small-record batching performance.
