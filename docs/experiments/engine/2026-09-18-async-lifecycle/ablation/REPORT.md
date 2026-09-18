# Twenty-device asynchronous lifecycle comparison

The original engine is `f76d1dc`; the first asynchronous lifecycle commit is
`a55205f`; the measured cleanup is `94860fc`. All three use the same pooled-input
benchmark and system glibc. Cleanup preserves the disk format and removes
redundant length/capacity checks, an unused argument, and duplicate footer encoding.

## Method

Each process uses all twenty data devices, a 64-GiB write window per device,
1-GiB segments, 512-MiB registered I/O pools per device, queue depth 256,
16-byte keys, and a local write batch of 128. Owners use the same four L3 groups,
ten owners per NUMA node. Huge pages use the preferred policy. Builds use Rust
1.98.0, the release profile, and `-C target-cpu=x86-64-v3`.

Source pools are prepared before timing. Refill, copy, checksums, index publication,
writes, rollovers, and the final write fence remain timed. Formatting, record
preparation, verification, and the final active-segment seal at close are outside
write timing. Read concurrency is 640, with a two-second warmup and at least ten
measured seconds. Writes also last at least ten seconds. Reads use the trusted-index
path without payload CRC verification; returned keys, lengths, and identity stamps
are checked. Disabled fsync changes durability semantics, including explicit flush.

| Value size | Nominal records/device | Value bytes/device |
|---|---:|---:|
| 100 B | 50,331,648 | 4.6875 GiB |
| 1 KiB | 33,554,432 | 32 GiB |
| 4 KiB | 14,680,064 | 56 GiB |
| 64 KiB | 851,968 | 52 GiB |
| 4 MiB | 13,312 | 52 GiB |

The main matrix has one observation per cell. Matching first-commit 64-KiB and
100-B controls are reused from the ablations. There are 28 matrix/ablation processes
and one additional read-observer process. These are not repeated-run medians or
steady-state overwrite/eviction measurements. Small differences are inconclusive.

## Throughput

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

The 100-B read row is affected by memory-map collection; see the read-observer
control below before attributing those differences to the engine.

## Memory

Process peak RSS (GiB, including benchmark records and engine pools/indexes):

| Value size | Original | First commit | Cleanup, fsync on | Cleanup, fsync off |
|---|---:|---:|---:|---:|
| 100 B | 258.626 | 265.939 | 265.942 | 265.943 |
| 1 KiB | 218.496 | 227.810 | 227.883 | 227.938 |
| 4 KiB | 74.706 | 79.076 | 79.074 | 79.082 |
| 64 KiB | 14.037 | 14.249 | 14.251 | 14.251 |
| 4 MiB | 10.141 | 10.148 | 10.148 | 10.146 |

Bounded version cursors retain an additional `Vec<ChunkId>`: 16 bytes per indexed
version before spare capacity. This is a real memory cost of the traversal contract.
Whole-process RSS also includes the driver, hash tables, pools, and allocator
retention, so these differences are not isolated allocation measurements.

## Cleanup ablations

| Treatment | Value size | First commit | Treatment | Change |
|---|---|---:|---:|---:|
| Reuse computed frame length | 65,536 B | 55.060 GiB/s | 54.783 GiB/s | -0.50% |
| Submit control without vacancy precheck | 65,536 B | 55.060 GiB/s | 55.626 GiB/s | +1.03% |
| Remove prepared-source page alignment | 65,536 B | 55.060 GiB/s | 42.708 GiB/s | -22.43% |
| Always count distinct absent keys | 4,096 B | 25.663 Mops/s | 26.166 Mops/s | +1.96% |
| Always count distinct absent keys | 100 B | 63.371 Mops/s | 51.927 Mops/s | -18.06% |

The separate 4-KiB index ablation uses 52 GiB/device on both sides. The index fast
path and prepared-source alignment are retained because removing them causes
material regressions. Redundant checks are removed for clarity; their small rate
differences do not establish speedups. Footer parity tests compare incremental and
contiguous encodings across padding/chunk boundaries. Resource limits, progress
budgets, error handling, and final commit-page ordering remain intact.

A fresh adjacent 64-KiB control measures **54.102 GiB/s** for the first commit and
**54.339 GiB/s** for cleanup. The main table's 4.10% cleanup gap does not repeat;
both observations are retained. All four main 64-KiB variants write the same
physical byte count. The asynchronous versions retain the existing footer request
splitting; cleanup does not alter it.

## Read-observer diagnostic

The throughput controller scanned `smaps_rollup` and `numa_maps` after verification.
A targeted 100-B cleanup process measures two consecutive read phases on the same
data, each with a two-second warmup. Its one memory scan overlaps the first measured
phase by approximately 2.7 seconds. The second phase has no scan.

| Read phase | Memory scan | Mops/s | CPU cores | CPU microseconds/read | p99 microseconds |
|---|---|---:|---:|---:|---:|
| First | Overlaps measurement | 8.809 | 19.207 | 2.1805 | 112.575 |
| Second | None | 9.169 | 19.999 | 2.1811 | 112.575 |

Without the scan, the same process returns to within 0.2% of the original's
9.187 Mops/s. This supports observer interference; the ordered pair does not trace
the exact blocking mechanism or establish statistical confidence. Original readings
remain in the main table. Throughput collection now uses the driver's peak-RSS
metric; detailed memory maps are collected separately.

## Validation and reproduction

All 29 processes verify every nominal record and show activity on all twenty
devices. All measured writes and reads exceed ten seconds. Final checks confirm
idle devices with no remaining open users. SMART counters were unavailable; this
is not a power-loss test. The implementation passed 256 workspace tests (one
ignored), six benchmark tests, Clippy and rustdoc with warnings denied, and formatting.

Use the [benchmark instructions](../../../../../benchmarks/cache-disk/README.md)
with the workload above and matching input, sync, and placement settings. Original
comparisons require its engine/API adapter with the same input-pool harness.
Historical diagnostic dumps and operational configuration are outside this PR;
this report retains the complete comparison and material interpretation limits.
