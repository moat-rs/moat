Twenty-device results: foyer, v1, v2
===================================

All figures below are **totals across 20 concurrent NVMe devices**, using the
current engine implementations and freshly measured foyer. Read results are
medians across three independently initialized datasets at 640 clients; write
results are medians of three independent bounded-dataset prefills.
See [methodology](README.md) for the exact source, settings, and limitations.

V1/v2 use the same benchmark request adapter, not the production `moat-cache`
API. Engine reads skip CRC; foyer retains its normal verification and owned
value decoding. The comparison includes these differences.

V2 improves median prefill throughput by 32.9% for 100-B values and 25.0% for
64-KiB values, but trails v1 by 7.7% at 1 KiB and 6.6% at 4 KiB. Its 4-MiB
write result is within the overlapping run ranges. V1/v2 fixed-concurrency
read medians differ by at most 3.4%; process-to-process spread exceeds several
of those gaps. The 4-MiB read medians are approximately 221 GiB/s for both.

Matched-concurrency reads
-------------------------

The primary table fixes global concurrency at 640 (mean 32 requests per disk).
Values are GiB/s of logical value bytes. It does not select a different
concurrency for each implementation.

| Value | Foyer | V1 | V2 | V2 vs V1 |
|---|---:|---:|---:|---:|
| 100 B | 0.058 | 0.070 | 0.071 | +0.92% |
| 1 KiB | 0.588 | 0.740 | 0.715 | -3.35% |
| 4 KiB | 2.399 | 2.859 | 2.899 | +1.40% |
| 64 KiB | 38.294 | 46.390 | 45.858 | -1.15% |
| 4 MiB | 23.079 | 220.716 | 220.496 | -0.10% |

Prefill writes
---------------

Same logical byte units and input per implementation. The timing includes
allocation, generated payloads, routing, checksums, batch barriers, and the
final device sync. Opening/formatting and final close are excluded. These
three independent prefills are short tests, not sustained-write ceilings.

| Value | Foyer GiB/s | V1 GiB/s | V2 GiB/s | V2 vs V1 |
|---|---:|---:|---:|---:|
| 100 B | 0.249 | 0.396 | 0.527 | +32.90% |
| 1 KiB | 2.424 | 4.059 | 3.746 | -7.70% |
| 4 KiB | 7.995 | 12.224 | 11.420 | -6.57% |
| 64 KiB | 13.363 | 7.865 | 9.834 | +25.03% |
| 4 MiB | 34.255 | 21.210 | 21.576 | +1.72% |

All concurrency levels
----------------------

Throughput is thousands of reads per second. Parentheses show median phase
p99 latency in microseconds; they are not a pooled histogram percentile.
This sweep uses three phases on the initial dataset for every concurrency.
Its 640-client rows can differ from the independent-dataset headline table.

| Value | Global clients | Foyer kops/s (p99 us) | V1 kops/s (p99 us) | V2 kops/s (p99 us) |
|---|---:|---:|---:|---:|
| 100 B | 160 | 702.9 (609) | 759.1 (582) | 734.0 (601) |
| 100 B | 640 | 725.9 (4143) | 777.2 (9634) | 751.8 (8413) |
| 100 B | 2560 | 724.0 (23167) | 773.5 (58753) | 745.4 (51708) |
| 1 KiB | 160 | 714.0 (602) | 755.5 (586) | 730.3 (599) |
| 1 KiB | 640 | 737.5 (3768) | 776.0 (9093) | 750.0 (5075) |
| 1 KiB | 2560 | 731.9 (32391) | 767.9 (59736) | 743.9 (58655) |
| 4 KiB | 160 | 707.7 (582) | 716.6 (602) | 844.6 (473) |
| 4 KiB | 640 | 733.4 (3774) | 735.9 (9429) | 871.4 (5534) |
| 4 KiB | 2560 | 731.5 (36471) | 724.3 (63603) | 863.4 (52232) |
| 64 KiB | 160 | 788.5 (339) | 707.3 (424) | 706.2 (423) |
| 64 KiB | 640 | 852.6 (2509) | 741.4 (7520) | 729.3 (7516) |
| 64 KiB | 2560 | 850.2 (12673) | 728.3 (61407) | 722.7 (59933) |
| 4 MiB | 160 | 21.5 (16294) | 48.5 (9961) | 48.6 (9921) |
| 4 MiB | 640 | 4.1 (695206) | 56.4 (49283) | 56.4 (49545) |
| 4 MiB | 2560 | 3.8 (1431306) | 55.4 (217186) | 55.4 (216662) |


Best observed read concurrency in the initial sweep
--------------------------------------------------

This table selects each implementation's highest three-phase median from the
first dataset. It is separate from the independent-dataset table above.
Foyer's 4-MiB result falls sharply as concurrency increases: its best result
is 84.0 GiB/s at 160 clients, compared with a 23.1-GiB/s independent-dataset
median at 640. The fixed-concurrency gap is not a universal peak-speed ratio.

| Value | Foyer GiB/s (clients) | V1 GiB/s (clients) | V2 GiB/s (clients) |
|---|---:|---:|---:|
| 100 B | 0.068 (640) | 0.072 (640) | 0.070 (640) |
| 1 KiB | 0.703 (640) | 0.740 (640) | 0.715 (640) |
| 4 KiB | 2.798 (640) | 2.807 (640) | 3.324 (640) |
| 64 KiB | 52.037 (640) | 45.249 (640) | 44.513 (640) |
| 4 MiB | 84.003 (160) | 220.402 (640) | 220.496 (640) |

Resource and physical I/O costs
-------------------------------

Read CPU is process CPU microseconds per logical operation at 640 clients.
Write amplification divides measured physical write bytes by value bytes;
it includes stored keys, padding, metadata and lifecycle writes.

| Value | Foyer read CPU us/op | V1 read CPU us/op | V2 read CPU us/op | Foyer write amplification | V1 write amplification | V2 write amplification |
|---|---:|---:|---:|---:|---:|---:|
| 100 B | 65.38 | 49.05 | 49.01 | 42.48x | 6.22x | 7.60x |
| 1 KiB | 66.07 | 48.48 | 48.81 | 4.14x | 1.49x | 1.54x |
| 4 KiB | 64.41 | 50.43 | 48.80 | 2.03x | 1.10x | 1.10x |
| 64 KiB | 69.26 | 49.12 | 49.36 | 1.06x | 1.13x | 1.13x |
| 4 MiB | 1886.94 | 357.04 | 356.30 | 1.00x | 1.00x | 1.00x |

Bottleneck evidence
-------------------

Separate [CPU profiles and waiting-path observations](PROFILING.md) identify
Tokio completion-queue contention in small reads, substantial value generation
and copying in writes, and checksum/copy work plus mapping/page-pinning waits
in foyer's high-concurrency large reads. These are integration-path costs;
the results are not isolated engine or device ceilings. The remaining small
write regressions have not been reduced to one independently verified cause.

The subsequent [controlled bottleneck investigation](../2026-09-16-bottlenecks/REPORT.md)
tests admission batches and input preparation without changing the engine
format. Its diagnostic producer modes are reported separately from this matrix.

Validation and artifacts
------------------------

The matrix and supplementary write runs completed 72,529,740 writes.
The primary read matrix completed 414,890,516 measured reads.
All 135 primary read phases and 30 independent 640-client rechecks recorded
physical reads on all 20 devices.
Every prefilled key passed read-back validation; no reported I/O or sampled
content error occurred in the completed matrix. Supplementary write-run,
warmup and verification reads are additional and excluded from the primary
measured-read total. All devices were idle with no open users and all
cooperative locks released after all measurements and profiling runs.

- [Read samples](samples.csv): every repetition, including slower samples.
- [Initial read sweep](summary.csv): all concurrency levels, three phases per group.
- [Independent 640-client datasets](read-matched-samples.csv): initial medians and both rechecks.
- [Matched read summary](read-matched-summary.csv): medians across the three datasets and ranges.
- [Prefill samples](prefill.csv): all 45 independent fill timings and physical I/O costs.
- [Prefill summary](prefill-summary.csv): three-run medians and ranges.

The separate [single-device complete-fill report](../../../engine/reports/2026-09-16-full/REPORT.md)
does not measure 20-disk throughput. Its -6.39% fill result must not be used
as the scaling result for this matrix.

Engine pool snapshots in the final profiles show 10 GiB of anonymous huge
pages per v1/v2 process and zero explicit hugetlb pages. Foyer's sampled
process shows zero anonymous huge pages. RSS includes all application memory,
not just the configured pools. No kernel huge-page settings were changed.
