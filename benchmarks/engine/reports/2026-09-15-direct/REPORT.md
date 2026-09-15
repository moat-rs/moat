# Random reads with optional CRC verification

A later [registered-buffer comparison](../2026-09-15-registered/REPORT.md)
remeasures both engines after v2 gains fixed buffers/files and deferred ring
completion work. The figures below describe the earlier implementation.

The pipeline now exposes `read(key, range, verify, buffers)`. With `verify = false`,
it uses the published index to read only requested pages. With `verify = true`,
it retains metadata validation and complete checksum-block verification. Writing
and recovery keep their checksum checks in either case.

At concurrency 64, v2 is within 0.1% of legacy for 64-KiB and 4-MiB full reads,
2.2% slower for mixed records, and about 12% slower for 100-B through 4-KiB
records. Physical read sizes now match. The earlier large metadata-related
regressions shrink substantially; small-read system CPU cost remains higher.

This report compares both engines with **read verification disabled**. The earlier
[verification-enabled results](../2026-09-15/REPORT.md) describe a different read
policy and are not the baseline for the percentages below.

## Method and scope

- Date: 2026-09-15. Same enterprise NVMe, NUMA-local application CPU and bounded
  data window as the earlier report. Linux 6.8.0, glibc 2.39, AMD EPYC x86-64.
  No fio run overlaps these measurements. The earlier same-day fio results
  remain a reference; both engine baselines were remeasured here.
- Engine and harness commit: `48398b57f641db75cf656886a93e868512cd6fa5`.
  Measured executable SHA-256:
  `c599a08dc2094b81608b0d25301440590683072e9150e4110f3e949a96270441`.
- Rust 1.98.0 release, `x86_64-unknown-linux-gnu`,
  `RUSTFLAGS='-C target-cpu=x86-64-v3'`. GNU/glibc, without musl.
- One thread, direct io_uring, depth 64, ordinary aligned buffers for v2 and
  registered buffers for legacy. Every process writes a fresh dataset in the
  same 2-GiB segment at offset 2 GiB inside a bounded 4-GiB device view.
- Three repetitions alternate engine order. Each full-value read phase has two
  warmup seconds and ten measured seconds. Concurrency is 1 and 64, plus 16 for
  4-MiB full-value reads. All tables use medians of runs; latency percentiles
  are not pooled across runs.
- Both engines compute write CRCs during timing and finish writes with a durable
  flush. Read CRC is disabled on both sides. The harness checks returned length,
  up to eight bytes at the beginning of each result and its final byte against
  the deterministic input. This is sampled content validation, not a full
  integrity scan. The key prefix is checked only if the requested range includes it.
- The full-value matrix completed 36,378,240 record writes and 187,511,701 measured
  reads without observed I/O or sampled-content errors. Warmup operations are
  excluded. [114 full-value samples](full/samples.csv) and
  [38 summaries](full/summary.csv) retain per-run variation and device counters.
- Dataset and batching remain as described in the [methodology](../../README.md):
  approximately 512 MiB of payload, groups of 64 small/mixed or 16 uniform large
  records. Mixed records cycle through 100 B, 4 KiB, 64 KiB and 300 B.

Across the full and range matrices, all 168 measured phases completed 36,428,928
record writes and 342,605,808 reads without observed I/O or sampled-content errors.
Warmup operations are additional. Every unverified read had the expected physical
byte count, including exactly one 8-KiB I/O for each boundary-crossing request.

Operational identifiers and private deployment details are excluded.

## Full-value random reads

| Record | Concurrency | Legacy Kops/s | V2 Kops/s | V2 change | Legacy GiB/s | V2 GiB/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 B | 1 | 17.360 | 17.306 | -0.31% | 0.002 | 0.002 |
| 100 B | 64 | 805.071 | 708.544 | -11.99% | 0.075 | 0.066 |
| 1 KiB | 1 | 17.372 | 17.321 | -0.29% | 0.017 | 0.017 |
| 1 KiB | 64 | 839.491 | 736.677 | -12.25% | 0.801 | 0.703 |
| 4 KiB | 1 | 17.384 | 17.325 | -0.34% | 0.066 | 0.066 |
| 4 KiB | 64 | 850.140 | 752.351 | -11.50% | 3.243 | 2.870 |
| 64 KiB | 1 | 7.874 | 7.840 | -0.44% | 0.481 | 0.479 |
| 64 KiB | 64 | 189.878 | 189.814 | -0.03% | 11.589 | 11.585 |
| 4 MiB | 1 | 1.319 | 1.267 | -3.91% | 5.152 | 4.950 |
| 4 MiB | 16 | 2.952 | 2.959 | +0.25% | 11.531 | 11.560 |
| 4 MiB | 64 | 2.966 | 2.966 | +0.01% | 11.586 | 11.587 |
| Mixed | 1 | 13.623 | 13.387 | -1.74% | 0.222 | 0.218 |
| Mixed | 64 | 511.902 | 500.544 | -2.22% | 8.345 | 8.161 |

Latency, physical bytes and CPU consumption at concurrency 64:

| Record | Legacy P50 / P99 (us) | V2 P50 / P99 (us) | Legacy / V2 physical KiB/read | Legacy / V2 CPU (% of one CPU) |
| --- | ---: | ---: | ---: | ---: |
| 100 B | 73.1 / 132.1 | 82.8 / 143.2 | 4.00 / 4.00 | 94.3 / 92.4 |
| 1 KiB | 70.5 / 128.8 | 80.2 / 139.9 | 4.00 / 4.00 | 93.0 / 90.8 |
| 4 KiB | 69.8 / 128.7 | 78.9 / 137.8 | 4.00 / 4.00 | 92.6 / 90.0 |
| 64 KiB | 264.1 / 1212.7 | 265.4 / 1192.4 | 64.00 / 64.00 | 38.7 / 54.2 |
| 4 MiB | 21071.8 / 31929.2 | 21065.2 / 33105.7 | 4096.00 / 4096.00 | 22.3 / 45.8 |
| Mixed | 99.6 / 403.8 | 103.1 / 392.2 | 19.00 / 19.00 | 73.5 / 82.0 |

CPU time per completed operation at concurrency 64, computed from process
`getrusage` counters and reported as the median across runs:

| Record | Legacy user / system (ns/op) | V2 user / system (ns/op) |
| --- | ---: | ---: |
| 100 B | 589.4 / 581.7 | 417.0 / 885.8 |
| 1 KiB | 509.6 / 598.4 | 346.5 / 882.1 |
| 4 KiB | 455.8 / 633.1 | 307.7 / 888.2 |
| 64 KiB | 430.1 / 1595.7 | 290.0 / 2567.8 |
| 4 MiB | 305.5 / 75025.4 | 374.7 / 154293.5 |
| Mixed | 484.5 / 951.3 | 330.3 / 1309.0 |

## Partial ranges

Range phases use the same 512-MiB dataset, two warmup seconds, ten measured
seconds and three fresh repetitions. Each logical request returns only the
specified range, so operations/s and physical bytes/read are more informative
than payload bandwidth for very small ranges. Range ends are exclusive.
Only the selected pages form the repeated read working set: 32 MiB for 4-KiB
ranges in 64-KiB values, 512 KiB for 4-KiB ranges in 4-MiB values, and 1 MiB for
the boundary-crossing range in 4-MiB values. Device-side caching can therefore
be especially significant; these are not cold, full-device random reads.

| Value size | Requested range | Concurrency | Legacy Kops/s | V2 Kops/s | V2 change | Legacy / V2 physical KiB/read |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 64 KiB | `4096:8192` | 1 | 17.772 | 17.714 | -0.33% | 4.00 / 4.00 |
| 64 KiB | `4096:8192` | 64 | 865.348 | 774.257 | -10.53% | 4.00 / 4.00 |
| 4 MiB | `4096:8192` | 1 | 38.586 | 47.271 | +22.51% | 4.00 / 4.00 |
| 4 MiB | `4096:8192` | 64 | 860.132 | 942.963 | +9.63% | 4.00 / 4.00 |
| 4 MiB | `65530:65540` | 1 | 29.045 | 36.715 | +26.41% | 8.00 / 8.00 |
| 4 MiB | `65530:65540` | 64 | 754.993 | 850.737 | +12.68% | 8.00 / 8.00 |

[4-KiB range samples](range-4k/samples.csv), [summaries](range-4k/summary.csv),
[checksum-boundary range samples](range-cross/samples.csv) and
[summaries](range-cross/summary.csv) retain the individual measurements.
The 10-byte range straddles a 64-KiB checksum boundary, but unverified reads only
need the two intersecting 4-KiB pages. Neither engine expands it to checksum blocks.

The boundary-crossing QD64 measurements vary substantially: legacy spans
519.058–857.700 Kops/s, versus 816.037–854.547 for v2. Legacy P99 spans
352–1791 us, versus 200–203 us for v2, and the first repetition favors legacy.
The median +12.68% therefore does not establish a stable throughput advantage.
Every run records one 8-KiB physical read per logical request, so extra read
amplification does not explain the spread. The source of the long-tail variation
was not isolated; retain the individual runs when assessing this result.

## Burst writes

These passes remeasure the existing write paths while preparing the read data.
No production write logic changes in this follow-up. They include CRC generation,
copying, index publication, completion handling and the final flush. Legacy
preallocates its index before the timer; v2 index growth remains inside it.

| Record | Legacy GiB/s | V2 GiB/s | V2 change | Legacy / V2 physical bytes per payload byte |
| --- | ---: | ---: | ---: | ---: |
| 100 B | 0.358 | 0.588 | +64.05% | 1.920 / 1.920 |
| 1 KiB | 3.642 | 5.146 | +41.32% | 1.375 / 1.125 |
| 4 KiB | 7.842 | 8.565 | +9.21% | 1.078 / 1.031 |
| 64 KiB | 9.156 | 9.186 | +0.33% | 1.063 / 1.062 |
| 4 MiB | 7.645 | 8.756 | +14.54% | 1.001 / 1.001 |
| Mixed | 8.986 | 8.870 | -1.29% | 1.082 / 1.060 |

Full-matrix write passes lasted 0.054–1.403 seconds.
They measure short bursts, not sustained ingestion or per-record durable latency.
No seal/footer update is timed.

## Interpretation and limits

Removing verification changes more than CRC CPU time: it also removes dependent
metadata I/O, whole-directory decoding and 64-KiB range expansion from v2 reads.
The physical-byte counters directly measure that reduction. The unverified
legacy path also uses its index directly, so its previously measured verified
4-KiB read gap disappears. The earlier +272% verified-read advantage must not be
carried over to this policy.

For 100-B reads, v2 uses a median 417 ns of user CPU per operation versus 589 ns
for legacy, but 886 ns of system CPU versus 582 ns. Similar shifts occur for
1-KiB and 4-KiB reads. This locates a remaining cost in system time rather than
in repeated frame/CRC decoding. It does not identify a single kernel function.

Differences that remain are not isolated to one cause by this experiment.
Legacy registers I/O buffers; v2 currently uses ordinary buffers and its own
submission/completion bookkeeping. Equal physical byte counts do not imply equal
kernel or userspace work. Fixed buffers/files and queue bookkeeping remain
candidates for separate measurement, without adding locks to the read path.

The index and immutable segment ownership are trusted in unverified mode. Silent
payload corruption and unexpected external changes are not detected by these
reads; callers requiring those checks must pass `verify = true`. Range validation,
short-I/O errors, snapshot semantics, buffer ownership, write checksums and
recovery validation remain enforced. Functional tests separately cover corrupt
metadata/payload, both verification modes and concurrent overwrite snapshots.

The window is small and warm. Direct I/O bypasses the OS page cache but not
device-side caching. Neither this suite nor the earlier suite establishes
full-device steady-state behavior, multi-worker scaling, crash durability,
rollover or reclamation performance. V2 still serves one explicitly assigned
segment and is not a complete replacement engine.
