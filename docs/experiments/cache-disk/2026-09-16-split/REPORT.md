Twenty-device comparison after request splitting
================================================

All three engines are rebuilt and rerun on the same **20 devices**. V2 now divides requests larger than the device's 128-KiB byte limit before submission. V1 and foyer retain their previous implementations. The workload and native adapters match the [previous comparison](../2026-09-16-native/REPORT.md).

The following write and 640-client read cells are medians of three independently initialized processes. See the [methodology](README.md) for source/build identity, timing scope, sampling, and API differences.

Prefill throughput
------------------

| Value | Unit | Foyer | V1 | V2 | V2 vs V1 |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 5.877 | 50.640 | 59.129 | +16.8% |
| 1 KiB | Mops/s | 5.589 | 33.935 | 37.248 | +9.8% |
| 4 KiB | Mops/s | 4.538 | 12.997 | 19.224 | +47.9% |
| 64 KiB | GiB/s | 11.043 | 36.893 | 36.034 | -2.3% |
| 4 MiB | GiB/s | 31.240 | 37.584 | 39.486 | +5.1% |

Prefills take 0.042–5.178 seconds. Small-record writes are bounded bursts, not sustained write ceilings. Timers include generation, checksums, draining, and final sync. [All write samples](prefill.csv), [medians and ranges](prefill-summary.csv).

Random reads at 640 clients
---------------------------

| Value | Unit | Foyer | V1 | V2 | V2 vs V1 |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 0.605 | 9.981 | 10.023 | +0.4% |
| 1 KiB | Mops/s | 0.609 | 9.988 | 9.964 | -0.2% |
| 4 KiB | Mops/s | 0.622 | 8.718 | 8.695 | -0.3% |
| 64 KiB | GiB/s | 39.411 | 187.385 | 187.213 | -0.1% |
| 4 MiB | GiB/s | 32.395 | 229.661 | 232.858 | +1.4% |

Each device has 32 logical clients. V1/v2 skip read CRC and return pooled buffers; foyer retains XXHash64 verification, owned-vector decoding, and possible in-flight coalescing. These are differences in the measured APIs.

Foyer's three 4-MiB samples at 640 clients are 73.356, 23.414, and 32.395 GiB/s. Its headline median is not a stable throughput ceiling. V1 spans 229.369–229.752 GiB/s and v2 spans 232.848–232.920 GiB/s in the corresponding samples.

CPU and latency at 640 clients
------------------------------

| Value | Engine | Process cores | CPU µs/op | p99 µs |
|---|---|---:|---:|---:|
| 100 B | foyer | 52.28 | 86.542 | 2025.5 |
| 100 B | v1 | 20.00 | 2.004 | 112.3 |
| 100 B | v2 | 20.00 | 1.995 | 112.4 |
| 1 KiB | foyer | 52.96 | 87.158 | 2002.9 |
| 1 KiB | v1 | 20.00 | 2.002 | 107.2 |
| 1 KiB | v2 | 20.00 | 2.007 | 107.4 |
| 4 KiB | foyer | 52.98 | 85.216 | 1950.7 |
| 4 KiB | v1 | 20.00 | 2.294 | 123.5 |
| 4 KiB | v2 | 20.00 | 2.300 | 123.7 |
| 64 KiB | foyer | 54.07 | 83.873 | 1738.8 |
| 64 KiB | v1 | 20.00 | 6.514 | 550.9 |
| 64 KiB | v2 | 20.00 | 6.520 | 551.9 |
| 4 MiB | foyer | 15.72 | 1952.937 | 186122.2 |
| 4 MiB | v1 | 19.97 | 340.064 | 21037.1 |
| 4 MiB | v2 | 19.99 | 335.358 | 18759.7 |

Concurrency sweep
-----------------

The first process for each engine/size also measures 160 and 2,560 clients. These endpoints have one sample each; 640 clients has three. The following cells use the fastest observed level per engine. [All 75 read phases](samples.csv) and [all concurrency summaries](summary.csv) retain the slower levels as well.

| Value | Unit | Foyer (clients) | V1 (clients) | V2 (clients) |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 0.605 (640) | 14.970 (2560) | 14.388 (2560) |
| 1 KiB | Mops/s | 0.609 (640) | 15.132 (2560) | 14.558 (2560) |
| 4 KiB | Mops/s | 0.622 (640) | 10.140 (2560) | 10.016 (2560) |
| 64 KiB | GiB/s | 39.411 (640) | 219.116 (2560) | 219.165 (2560) |
| 4 MiB | GiB/s | 74.945 (160) | 229.661 (640) | 232.858 (640) |

V2 against the previous measured revision
-----------------------------------------

The earlier source is `78ce9a2`; the current source is `a0aa85f`. Rates come from separate complete matrices, so these are observed changes, not an isolated instruction-cost experiment.

| Value | Write change | Read change at 640 clients |
|---|---:|---:|
| 100 B | -1.2% | -0.1% |
| 1 KiB | +0.3% | -0.0% |
| 4 KiB | +5.6% | -0.1% |
| 64 KiB | -0.9% | +0.2% |
| 4 MiB | +5.2% | +1.3% |

V2 now bounds physical SQEs at 256 per device. V1 still submits a whole large extent in one SQE, which the kernel can split into many physical requests. Identical ring depths therefore do not establish identical physical concurrency. Splitting also moves submission and completion handling onto the application owner. Throughput and process CPU must be considered alongside helper counts.

Observed io-wq activity
-----------------------

The following maxima cover warmup, measured reads, and drain at 640 clients across all three processes. They are sampled task counts, not counts of helpers created by that particular phase. State `R` means runnable or running; retained sleeping helpers are counted only in the total column.

| Value | Engine | Maximum helpers | Maximum runnable helpers |
|---|---|---:|---:|
| 100 B | foyer | 20 | 0 |
| 100 B | v1 | 0 | 0 |
| 100 B | v2 | 0 | 0 |
| 1 KiB | foyer | 20 | 1 |
| 1 KiB | v1 | 0 | 0 |
| 1 KiB | v2 | 0 | 0 |
| 4 KiB | foyer | 20 | 0 |
| 4 KiB | v1 | 20 | 0 |
| 4 KiB | v2 | 0 | 0 |
| 64 KiB | foyer | 20 | 0 |
| 64 KiB | v1 | 0 | 0 |
| 64 KiB | v2 | 0 | 0 |
| 4 MiB | foyer | 1377 | 18 |
| 4 MiB | v1 | 2279 | 68 |
| 4 MiB | v2 | 0 | 0 |

[Per-process observations](runtime.csv) and [phase observations](runtime-phases.csv) retain task counts, helper states, observer CPU, and actual huge-page backing. Sampling every 250 ms can miss brief activity; these observations are not a kernel trace. Read-phase boundaries include warmup and drain. Process CPU is not whole-machine CPU.

Across all 15 v2 processes, the observed task maximum is 21 and no io-wq helpers are observed. V1 reaches 5,125 tasks, including 5,104 helpers, during the first 4-MiB process; this maximum occurs at 2,560 clients, outside the 640-client table above. Both native engines have 10 GiB of actual anonymous huge pages in every post-verification snapshot. All measured 4-MiB native phases report exactly 33 physical device reads per logical operation, so splitting changes where that work is submitted rather than its byte or physical-request count.

Validation and limits
---------------------

The matrix contains 45 prefills, 72,529,740 inserted records, and 1,536,116,502 measured reads. Every inserted key passed read-back validation, and every measured read phase accessed all 20 devices. Every native read phase at 4 KiB, 64 KiB, and 4 MiB transferred exactly the expected aligned physical byte count. All 45 processes completed successfully; no completed sample was discarded.

The executable is a GNU/glibc release build. V1/v2 use no Tokio runtime; registered buffers/files and preferred huge pages remain enabled. Only a bounded 64-GiB window per device is permitted, and these datasets do not fill the devices. This measures host-visible I/O without establishing a sustained full-device media limit or power-loss behavior. Public artifacts exclude operational identifiers.
