# Registered buffers and huge-page policy

With ordinary pages, the QD64 small-record gap narrows from about 12% in the
previous implementation to 1.25% for 100 B, 1.29% for 1 KiB, and 0.27% for 4 KiB.
64-KiB and 4-MiB throughput match the fresh legacy baseline within 0.1%; mixed
reads are 0.47% lower. These are medians of the completed three-run matrix.

This follow-up adds v2 fixed-buffer I/O through the shared `moat-common` arena
pool, fixed files, doubled completion capacity, and `SINGLE_ISSUER` with
`DEFER_TASKRUN`. Buffer ownership moves through completions without a payload
copy or a per-I/O pool-owner clone. Nonblocking polls explicitly drive deferred
completion work. The read API remains `read(key, range, verify, buffers)`.

Both engines are remeasured with registered buffers and matching huge-page
policies. The previous [direct-read report](../2026-09-15-direct/REPORT.md) used
ordinary v2 buffers and ring setup, with huge pages disabled on both sides.
Historical figures are retained at their original revision.

## Method and observed memory

- Date: 2026-09-15. Same enterprise NVMe, NUMA-local application CPU, Linux 6.8.0,
  glibc 2.39, and AMD EPYC x86-64 as the earlier same-day reports. No fio or
  profiling run overlaps this comparison. The earlier fio results remain a
  device reference; both engine baselines are fresh here.
- Engine and harness commit: `5c327ed188c3ed290043006307cbc89b4c958bc5`.
  Executable SHA-256:
  `4ebc73d827b4581a77d2efce4fdf329601844553f75f95d779d40c6b8dd477e1`.
- Rust 1.98.0 release, `x86_64-unknown-linux-gnu`,
  `RUSTFLAGS='-C target-cpu=x86-64-v3'`; GNU/glibc, without musl.
- One application thread, O_DIRECT, io_uring depth 64. Both engines register
  files and their 1-GiB common pool, with an 8-MiB maximum buffer class. Every
  recorded phase reports fixed buffers, fixed files, and deferred taskrun enabled.
  No SQPOLL, IOPOLL, global huge-page reservation, or kernel tuning is applied.
- Two full matrices: `--huge-pages disabled` and `--huge-pages preferred`, each
  with three repetitions alternating engine order. Each read phase warms for
  two seconds and measures for **five seconds**, versus ten in the earlier
  report. Concurrency is 1 and 64, plus 16 for 4-MiB values. Tables report run
  medians; individual samples retain the spread.
- Both sides use `verify = false` for timed reads. Writes compute CRCs and finish
  with a durable flush. The common read callback checks length, up to eight
  starting bytes, and the final byte against deterministic input. Enabled-CRC
  smoke tests ran separately on both engines for small, mixed, and 4-MiB values.
- Each process writes approximately 512 MiB of payload to a fresh 2-GiB segment
  at offset 2 GiB in the bounded 4-GiB view. Batching, mixed-record order, and
  latency sampling follow the [shared methodology](../../README.md).
- `Preferred` selected transparent huge-page backing, with **1 GiB of actual
  anonymous huge pages per pool in every recorded phase**. No explicit hugetlb
  pages were available or used. A successful hint alone is not counted as proof.
- `Disabled` selected ordinary-page arenas. Exact observations report zero
  huge-page bytes; 54 of its 114 phase snapshots report unknown because their
  pool VMA merged with unrelated anonymous memory. These are empty CSV fields,
  not silently treated as measured zeros.

All **228 measured phases** completed **72,756,480 record writes** and
**196,485,183 reads** without observed I/O or sampled-content errors. Warmup
operations are additional. [Disabled samples](disabled/samples.csv),
[Disabled summaries](disabled/summary.csv), [Preferred samples](preferred/samples.csv),
and [Preferred summaries](preferred/summary.csv) contain all measurements.
Operational identifiers and private deployment details are excluded.

## Full-value reads at concurrency 64

### Disabled huge-page policy

| Record | Legacy Kops/s | V2 Kops/s | V2 change | Legacy / V2 GiB/s | Legacy / V2 P99 (us) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 100 B | 808.614 | 798.501 | -1.25% | 0.075 / 0.074 | 132.0 / 133.1 |
| 1 KiB | 841.381 | 830.497 | -1.29% | 0.802 / 0.792 | 129.4 / 129.8 |
| 4 KiB | 855.085 | 852.806 | -0.27% | 3.262 / 3.253 | 128.2 / 128.6 |
| 64 KiB | 190.570 | 190.683 | +0.06% | 11.631 / 11.638 | 1191.0 / 1191.1 |
| 4 MiB | 2.983 | 2.985 | +0.06% | 11.653 / 11.660 | 34105.6 / 31498.0 |
| Mixed | 514.713 | 512.288 | -0.47% | 8.393 / 8.353 | 394.9 / 394.1 |

### Preferred huge-page policy

| Record | Legacy Kops/s | V2 Kops/s | V2 change | Legacy / V2 GiB/s | Legacy / V2 P99 (us) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 100 B | 807.831 | 799.264 | -1.06% | 0.075 / 0.074 | 131.9 / 133.3 |
| 1 KiB | 840.790 | 830.474 | -1.23% | 0.802 / 0.792 | 129.0 / 130.0 |
| 4 KiB | 850.903 | 849.895 | -0.12% | 3.246 / 3.242 | 128.2 / 128.9 |
| 64 KiB | 190.362 | 190.245 | -0.06% | 11.619 / 11.612 | 1196.0 / 1192.6 |
| 4 MiB | 2.974 | 2.981 | +0.24% | 11.616 / 11.644 | 32772.1 / 32368.8 |
| Mixed | 514.850 | 512.125 | -0.53% | 8.395 / 8.350 | 396.3 / 392.1 |

## Low concurrency and large-value scaling

| Policy | Record | Concurrency | Legacy Kops/s | V2 Kops/s | V2 change |
| --- | ---: | ---: | ---: | ---: | ---: |
| Disabled | 100 B | 1 | 17.372 | 17.369 | -0.01% |
| Disabled | 1 KiB | 1 | 17.382 | 17.387 | +0.03% |
| Disabled | 4 KiB | 1 | 17.402 | 17.403 | +0.00% |
| Disabled | 64 KiB | 1 | 8.005 | 8.006 | +0.01% |
| Disabled | 4 MiB | 1 | 1.360 | 1.555 | +14.29% |
| Disabled | 4 MiB | 16 | 2.963 | 2.979 | +0.56% |
| Disabled | Mixed | 1 | 13.738 | 13.578 | -1.17% |
| Preferred | 100 B | 1 | 17.371 | 17.369 | -0.01% |
| Preferred | 1 KiB | 1 | 17.371 | 17.384 | +0.07% |
| Preferred | 4 KiB | 1 | 17.388 | 17.375 | -0.07% |
| Preferred | 64 KiB | 1 | 7.988 | 7.963 | -0.32% |
| Preferred | 4 MiB | 1 | 1.333 | 1.511 | +13.38% |
| Preferred | 4 MiB | 16 | 2.953 | 2.975 | +0.75% |
| Preferred | Mixed | 1 | 13.735 | 13.590 | -1.06% |

## CPU and physical reads

User/system CPU time below is measured with `getrusage` and divided by completed
reads at concurrency 64. It includes harness bookkeeping and callback sampling.
It does not identify individual kernel functions.

| Policy | Record | Legacy user / system (ns/op) | V2 user / system (ns/op) | Legacy / V2 physical KiB/read |
| --- | ---: | ---: | ---: | ---: |
| Disabled | 100 B | 596.8 / 572.1 | 621.3 / 565.1 | 4.00 / 4.00 |
| Disabled | 1 KiB | 506.7 / 600.2 | 526.1 / 599.3 | 4.00 / 4.00 |
| Disabled | 4 KiB | 459.4 / 624.5 | 459.7 / 630.7 | 4.00 / 4.00 |
| Disabled | 64 KiB | 468.1 / 1569.8 | 506.7 / 1497.9 | 64.00 / 64.00 |
| Disabled | 4 MiB | 384.5 / 74897.5 | 293.9 / 73506.6 | 4096.00 / 4096.00 |
| Disabled | Mixed | 481.8 / 949.5 | 446.2 / 971.8 | 19.00 / 19.00 |
| Preferred | 100 B | 594.1 / 575.1 | 616.3 / 567.8 | 4.00 / 4.00 |
| Preferred | 1 KiB | 501.1 / 604.9 | 535.4 / 586.9 | 4.00 / 4.00 |
| Preferred | 4 KiB | 459.9 / 622.9 | 466.3 / 623.0 | 4.00 / 4.00 |
| Preferred | 64 KiB | 481.5 / 1539.1 | 479.6 / 1534.4 | 64.00 / 64.00 |
| Preferred | 4 MiB | 431.3 / 74484.9 | 296.0 / 74402.8 | 4096.00 / 4096.00 |
| Preferred | Mixed | 494.3 / 932.3 | 449.0 / 966.1 | 19.00 / 19.00 |

## Burst writes

| Policy | Record | Legacy GiB/s | V2 GiB/s | V2 change | Legacy / V2 physical bytes per payload byte |
| --- | ---: | ---: | ---: | ---: | ---: |
| Disabled | 100 B | 0.359 | 0.567 | +58.17% | 1.920 / 1.920 |
| Disabled | 1 KiB | 3.603 | 5.446 | +51.16% | 1.375 / 1.125 |
| Disabled | 4 KiB | 7.983 | 8.597 | +7.70% | 1.078 / 1.031 |
| Disabled | 64 KiB | 9.159 | 9.182 | +0.25% | 1.063 / 1.062 |
| Disabled | 4 MiB | 7.434 | 8.783 | +18.15% | 1.001 / 1.001 |
| Disabled | Mixed | 8.984 | 8.795 | -2.10% | 1.082 / 1.060 |
| Preferred | 100 B | 0.358 | 0.582 | +62.58% | 1.920 / 1.920 |
| Preferred | 1 KiB | 3.707 | 5.274 | +42.25% | 1.375 / 1.125 |
| Preferred | 4 KiB | 7.675 | 8.332 | +8.55% | 1.078 / 1.031 |
| Preferred | 64 KiB | 9.159 | 9.182 | +0.25% | 1.063 / 1.062 |
| Preferred | 4 MiB | 7.655 | 8.735 | +14.11% | 1.001 / 1.001 |
| Preferred | Mixed | 8.983 | 8.980 | -0.04% | 1.082 / 1.060 |

The write phases last 0.054–1.408 seconds. They include copying,
CRC generation, index publication, completion processing, and durable flush, but
exclude formatting and initial pool/buffer allocation. V2 index growth remains
inside timing; legacy preallocates its index. These are short bursts, not
sustained ingestion. No seal/footer write is timed.

## Interpretation and limits

Fixed buffers, fixed files, completion capacity, and ring setup change together.
The previous report and this one do not isolate the contribution of each flag.
The disabled-page matrix provides a comparison without requesting huge pages;
the preferred-page matrix compares both engines under observed THP backing.
For v2 100-B reads, system CPU falls from about 886 ns/op in the earlier report
to 565 ns/op here, while user CPU rises from about 417 to 621 ns/op. Total CPU
per operation decreases by about 9%, not by the full system-time reduction.
For 4-MiB reads, system CPU falls from about 154 to 74 microseconds per operation.
These cross-revision observations support a reduction in I/O processing cost,
not attribution to any single flag or kernel function.

Preferred throughput closely tracks the ordinary-page matrix. This workload
does not demonstrate a large additional throughput gain from THP.
The two policy matrices run sequentially, so cross-policy differences can also
include device-state or temporal variation. Small differences should be judged
against individual run ranges, not just rounded medians.

Read geometry and the wire format do not change in this optimization. Unverified
reads still trust the published immutable index, while verified reads retain
metadata and payload CRC checks. This report does not remeasure all verified-read
or partial-range configurations; earlier results for those configurations must
not be presented as measurements of this revision.

The small repeatedly accessed data window can benefit from device-side caching
even under direct I/O. This suite does not establish cold-media latency,
full-device steady state, multi-worker scaling, concurrent mutation, crash
recovery, rollover, or reclamation performance. V2 still serves one explicitly
assigned segment and is not a complete replacement engine.
