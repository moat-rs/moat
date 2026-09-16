Native twenty-device comparison
================================

The benchmark now drives v1/v2 directly through a synchronous polling adapter. Only foyer uses Tokio. The engine implementations and on-disk formats are unchanged. All numbers below aggregate **20 devices**.

See the [methodology](README.md) for exact source, build identity, timing scope, and differences between the exposed APIs. Reads use 640 global clients (32 per device); write and headline read cells are medians from three separately initialized processes.

Prefill throughput
------------------

| Value | Unit | Foyer | V1 | V2 | V2 vs V1 |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 5.696 | 50.116 | 59.859 | +19.4% |
| 1 KiB | Mops/s | 5.471 | 33.749 | 37.147 | +10.1% |
| 4 KiB | Mops/s | 4.629 | 13.012 | 18.213 | +40.0% |
| 64 KiB | GiB/s | 11.598 | 38.019 | 36.360 | -4.4% |
| 4 MiB | GiB/s | 30.996 | 37.456 | 37.542 | +0.2% |

Prefills take 0.043–5.251 seconds. In particular, small-record writes are brief bounded bursts; these values are not sustained write ceilings. Every batch is drained and all devices are synchronized before stopping the write timer. [All writes and observed ranges](prefill.csv), [write medians](prefill-summary.csv).

Random-read throughput
----------------------

| Value | Unit | Foyer | V1 | V2 |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 0.584 | 9.991 | 10.032 |
| 1 KiB | Mops/s | 0.592 | 10.000 | 9.968 |
| 4 KiB | Mops/s | 0.599 | 8.729 | 8.704 |
| 64 KiB | GiB/s | 39.081 | 186.780 | 186.773 |
| 4 MiB | GiB/s | 39.265 | 229.711 | 229.831 |

Engine reads skip CRC and return pooled buffers; foyer retains XXHash64 verification and owned-vector decoding. Foyer can also coalesce in-flight reads. These differences are part of the measured APIs.

CPU and latency at 640 clients
------------------------------

| Value | Engine | Process cores | CPU µs/op | p99 µs |
|---|---|---:|---:|---:|
| 100 B | foyer | 52.49 | 89.744 | 2109.4 |
| 100 B | v1 | 20.00 | 2.002 | 112.3 |
| 100 B | v2 | 20.00 | 1.993 | 112.4 |
| 1 KiB | foyer | 52.58 | 89.135 | 2070.5 |
| 1 KiB | v1 | 20.00 | 2.000 | 107.1 |
| 1 KiB | v2 | 20.00 | 2.006 | 107.4 |
| 4 KiB | foyer | 52.58 | 87.844 | 2024.4 |
| 4 KiB | v1 | 20.00 | 2.291 | 123.5 |
| 4 KiB | v2 | 20.00 | 2.298 | 123.6 |
| 64 KiB | foyer | 53.80 | 84.019 | 1758.2 |
| 64 KiB | v1 | 20.00 | 6.535 | 557.6 |
| 64 KiB | v2 | 20.00 | 6.535 | 556.5 |
| 4 MiB | foyer | 18.83 | 1907.822 | 168427.5 |
| 4 MiB | v1 | 19.97 | 339.543 | 20922.4 |
| 4 MiB | v2 | 19.97 | 339.438 | 21299.2 |

Concurrency sweep
-----------------

The first process for each engine/size also measures 160 and 2,560 clients. Those endpoints each have one sample; the 640-client cells have three. The following table retains the fastest observed level per engine instead of using one high-concurrency result as a universal peak comparison.

| Value | Unit | Foyer (clients) | V1 (clients) | V2 (clients) |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 0.584 (640) | 14.951 (2560) | 14.403 (2560) |
| 1 KiB | Mops/s | 0.592 (640) | 15.125 (2560) | 14.650 (2560) |
| 4 KiB | Mops/s | 0.599 (640) | 10.062 (2560) | 9.902 (2560) |
| 64 KiB | GiB/s | 39.081 (640) | 218.715 (2560) | 218.820 (2560) |
| 4 MiB | GiB/s | 71.246 (160) | 229.711 (640) | 229.831 (640) |

[All 75 measured read phases](samples.csv) and [all concurrency summaries](summary.csv) retain every completed result, including slower levels.

The remaining 64-KiB write gap is 4.4%; 640-client reads differ by at most 0.5%. The one-sample high-concurrency sweep also retains v2 small-read gaps of approximately 1.6–3.7%. [Separate CPU profiles](PROFILING.md) show bulk-memory work on writes and native polling/index/pool work on reads; they do not isolate a cause for every remaining gap.

Validation and limits
---------------------

The reported matrix contains 45 prefills, 72,529,740 inserted records, and 1,534,927,118 measured reads. Every inserted key passed read-back validation, and every measured read phase accessed all 20 devices. Engine reads of 4 KiB, 64 KiB, and 4 MiB transferred exactly the expected aligned physical byte count for every completed operation.

One additional process was interrupted by the observer racing a disappearing `/proc` task. Its log was retained privately and the case rerun after fixing the observer. No completed result in the reported matrix was discarded. Separate screening and profiling runs are excluded from these throughput tables.

V1/v2 have 21 application threads, including the coordinator, and create no Tokio runtime. Linux may add io-wq helpers; these appear in observed process-task counts. [Runtime observations](runtime.csv) retain their counts and huge-page snapshots. Process CPU measurements are not whole-machine CPU usage.

A [separate read-only investigation](IO_WQ.md) confirms that the devices' 128-KiB request limit triggers io-wq offload for larger SQEs, including with huge pages and fixed buffers. Splitting the same logical read into SQEs within that limit avoided observed helpers in the diagnostic cases. This establishes the offload mechanism, not its share of the reported throughput or CPU cost.

The previous [application/Tokio comparison](../2026-09-16-20disk/REPORT.md) remains valid for its recorded adapter. The native driver also moves hashing/placement out of timing, generates values on device-owner threads, avoids the intermediate large-value envelope copy, fixes per-device concurrency, and removes cross-device batch barriers. Its gains are not isolated lock-removal measurements or engine implementation changes.

GNU/glibc release build, harness Clippy, three driver tests, and multi-device file smoke checks passed. Driver tests cover backpressure, out-of-order completions, and corrupt-data rejection. File checks cover engine read CRC, small-segment rollover, and buffer pressure. All 20 devices were idle, had no open users, and had released their benchmark locks after the final profiles.
