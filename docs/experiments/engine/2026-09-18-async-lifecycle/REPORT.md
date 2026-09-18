# Twenty-device asynchronous lifecycle comparison

The complete asynchronous lifecycle patch preserves random-read throughput in
this workload, but it is **not write-performance-neutral**. The main-matrix
medians show lower write throughput for 100 B, 1 KiB, 64 KiB, and 4 MiB values.
This compares the entire working patch (including index/resource admission and
poll budgets), not an isolated change to lifecycle I/O.

The [follow-up investigation](investigation/REPORT.md) separates write-budget
accounting, allocator trimming, extra index work, and the fsync worker source.
The subsequent [TiKV jemalloc comparison](jemalloc/REPORT.md) tests allocator
selection after the budget and index fixes, without a diagnostic startup buffer.
The later [system-allocator/fsync comparison](libc-fsync/REPORT.md) removes that
optional integration, adds an explicit fsync policy, and rechecks all five sizes
against the original baseline with separate first-write and input-reuse controls.

## Main matrix

All twenty data devices participate in every process. Tables show medians from
three independent paired prefills, alternating baseline/candidate order. Read
tables use 640 global clients (32 per device). Change is the ratio of medians;
all individual samples and throughput ranges are retained in the CSV files.

### Writes

| Value | Unit | Baseline | Async patch | Change |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 59.749 | 47.370 | -20.7% |
| 1 KiB | Mops/s | 36.473 | 33.024 | -9.5% |
| 4 KiB | Mops/s | 18.914 | 19.052 | +0.7% |
| 64 KiB | GiB/s | 35.224 | 29.100 | -17.4% |
| 4 MiB | GiB/s | 38.979 | 35.442 | -9.1% |

Prefill includes value generation/copy, checksums, allocation/rollover, index
publication, draining, and final device sync. Initial format/open and final seal
are outside this interval. Small-value writes are short bursts, not sustained
steady-state throughput or per-record durable latency. The 1-KiB paired write
deltas change sign between repetitions; do not treat its median as a stable
regression magnitude. All three 64-KiB and 4-MiB pairs favor the baseline.

### Random reads, CRC disabled

| Value | Unit | Baseline | Async patch | Change |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 10.004 | 9.993 | -0.1% |
| 1 KiB | Mops/s | 9.929 | 9.917 | -0.1% |
| 4 KiB | Mops/s | 8.678 | 8.676 | -0.0% |
| 64 KiB | GiB/s | 187.566 | 187.671 | +0.1% |
| 4 MiB | GiB/s | 232.807 | 232.798 | -0.0% |

The first repetition also sweeps 160 and 2560 global clients. Those points have
one sample per variant; only 640 clients has three independent prefills. Latency
percentiles and process CPU use are in [samples.csv](samples.csv) and
[summary.csv](summary.csv); median run percentiles are not a merged histogram.

### Write CPU cost

| Value | Baseline CPU us/op | Async CPU us/op | Change |
|---|---:|---:|---:|
| 100 B | 0.278 | 0.319 | +14.5% |
| 1 KiB | 0.496 | 0.519 | +4.7% |
| 4 KiB | 0.978 | 1.002 | +2.4% |
| 64 KiB | 28.824 | 36.278 | +25.9% |
| 4 MiB | 1632.787 | 1833.652 | +12.3% |

The main-matrix physical write counts are almost unchanged while CPU cost rises
for the larger values. This is a diagnostic observation, not a causal profile or
proof that any one part of the patch is responsible. No tuning was applied to
hide the regression; both snapshots remained fixed throughout the run.

## Frequent rollover

These runs use 16-MiB segments and read CRC verification enabled. Each value size
has three independent paired prefills and reads at 640 global clients. The
100-B dataset targets 524,288 records per device, the 4-KiB dataset targets
131,072, and the 4-MiB dataset targets 128. Actual counts vary with deterministic rendezvous
routing; the total is exactly twenty times the configured target. They force
multiple allocations/seals within the write interval. Do not compare their read rates directly with the CRC-disabled table.

### Writes

| Value | Unit | Baseline | Async patch | Change |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 60.761 | 57.952 | -4.6% |
| 4 KiB | Mops/s | 20.879 | 19.936 | -4.5% |
| 4 MiB | GiB/s | 36.139 | 34.208 | -5.3% |

### Random reads, CRC enabled

| Value | Unit | Baseline | Async patch | Change |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 5.719 | 5.680 | -0.7% |
| 4 KiB | Mops/s | 4.094 | 4.087 | -0.2% |
| 4 MiB | GiB/s | 115.790 | 122.631 | +5.9% |

## Runtime thread difference

The baseline has at most 21 sampled tasks and no observed io-wq helpers. The
candidate reaches 41 tasks, including 20 io-wq helpers. Helpers remain visible
in the candidate's read-phase samples as well. No helper was sampled in the
running state, but the 250-ms interval cannot establish zero execution or CPU
cost between observations. Presence during reads does not establish that reads
were offloaded; this run does not attribute helper creation to a specific I/O
operation. The patch therefore does not preserve the baseline's observed
helper-free execution. [Phase observations](runtime-phases.csv) retain these
counts, including running-state observations.

## Configuration and provenance

- Baseline: `f76d1dcf8f290261af2860321d040d3f0b87df93`.
- Candidate: the uncommitted asynchronous-lifecycle working patch on that commit.
  [build.json](build.json) records both executable hashes and the candidate source
  archive/patch hashes; the exact sources and raw operational inputs remain private.
- Rust 1.98.0, GNU/glibc release, `-C target-cpu=x86-64-v3`.
- One AMD EPYC 9A85 socket, 96 physical cores, two NUMA nodes; see
  [platform.json](platform.json). Twenty fixed owner cores, one per device;
  native polling path with no Tokio runtime.
- Twenty assigned raw NVMe devices, each restricted to the same 64-GiB test
  window used by the prior run. This is not a full-device fill. No discard,
  preconditioning, or global kernel tuning was performed.
- Main matrix: 2-GiB segments, 16-byte keys, 2560 global prefill admission budget,
  queue depth 256 per device, 512-MiB pool per device, preferred huge pages.
  Record count per device is `max(2048, min(131072, 512 MiB / (value_bytes + 16)))`.
- Each read level warms for two seconds and measures for five seconds, then drains
  accepted operations. Keys/routing are computed before timing. Every prefilled
  record is checked for key/value identity and size; CRC is enabled only in the
  frequent-rollover suite. This is not an exhaustive corruption test.
- Candidate poll targets: 64 operations, 1 MiB, 4096 records. Native resource
  defaults apply, with index capacity raised as required by the benchmark's
  reservation. Baseline has its original unbudgeted polling behavior.

## Validation and limits

- All **48 prefills and 68 measured read phases** completed, with **127,011,720**
  prefilled records verified. All twenty devices served physical reads in every
  measured read phase. No I/O, admission, enabled-checksum, or sampled-content
  error terminated a run.
- At 250-ms sampling intervals the maximum observed process task count was
  41, and the maximum observed io-wq helper count was 20;
  [runtime.csv](runtime.csv) preserves each process. Sampling cannot exclude
  helpers appearing between observations.
- Preflight matched the target-specific serial/capacity allowlist, excluded system
  devices, checked partitions, mounts, holders, signatures, open users and idle
  counters, and held cooperative device locks throughout the matrix.
- Before/after checks found twenty live controllers and both system RAID mirrors
  intact. SMART reads were unavailable to the benchmark account; no SMART health
  claim is made.
- This measures the native engine, not the server/cache-store/cache APIs. It does
  not measure recovery time, the maximum duration of an individual poll, fairness
  between several engines on one owner, mixed simultaneous reads/writes, physical
  reclamation, long-lived buffer retention, or crash/power-loss safety.

[All samples](samples.csv) · [Medians and ranges](summary.csv) · [Runtime observations](runtime.csv)
