# V2 twenty-device recheck after migration

**Follow-up:** the [write-regression investigation](investigation/REPORT.md)
adds adjacent old/current controls, reversed-order checks, longer writes,
per-owner timing, expanded unique inserts, and profiles. The original historical
deltas below are retained; they do not establish an overall code regression.
The 100-B bursts are sensitive to final open/sync latency, while a smaller
64-KiB unique-insert concern remains workload/session dependent.

This measures the current native v2 engine on the same twenty raw NVMe devices as the [September 16 split-I/O comparison](../2026-09-16-split/README.md). The server/cache-store/cache migration is present in the source snapshot, but this driver calls the engine directly. It does **not** measure the migrated cache API or its one-frame-per-mutation adapter.

Each table uses medians from three independent processes and prefills. Read tables use 640 global clients (32 per disk). Baseline numbers are historical v2 samples, not a fresh paired control; elapsed time and intervening device workloads may affect differences. Foyer and v1 were not rerun.

## Prefill

| Value | Unit | Previous v2 | Current v2 | Change |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 59.129 | 49.927 | -15.6% |
| 1 KiB | Mops/s | 37.248 | 36.805 | -1.2% |
| 4 KiB | Mops/s | 19.224 | 19.023 | -1.0% |
| 64 KiB | GiB/s | 36.034 | 32.535 | -9.7% |
| 4 MiB | GiB/s | 39.486 | 38.876 | -1.5% |

Prefill includes value generation/copy, encoding, write checksums, draining, and final sync; format/open and key routing are excluded. Small-value writes are brief bounded bursts, not sustained device throughput.

The 100-B prefills last only 43–56 ms and range from 46.477 to 60.402 Mops/s.
The 64-KiB runs range from 32.049 to 34.616 GiB/s. Their lower medians are
retained rather than replaced with the fastest sample. A fresh interleaved
baseline would be needed to attribute these changes to code.

## Random reads

| Value | Unit | Previous v2 | Current v2 | Change | Current p99 |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 10.023 | 10.002 | -0.2% | 112.6 us |
| 1 KiB | Mops/s | 9.964 | 9.938 | -0.3% | 107.7 us |
| 4 KiB | Mops/s | 8.695 | 8.681 | -0.2% | 124.0 us |
| 64 KiB | GiB/s | 187.213 | 187.026 | -0.1% | 550.4 us |
| 4 MiB | GiB/s | 232.858 | 232.871 | +0.0% | 18858.0 us |

## Workload and provenance

- 64 GiB bounded window per device; this is not a full-device fill or a sustained overwrite/GC test.
- Deterministic 16-byte keys; routing and identity computed before timing. Value sizes, record counts, CPU assignments, NUMA placement, queue depth, and memory budgets match the historical split-I/O run.
- Three independent prefills per size. Pass one reads at 160, 640, and 2,560 global clients; passes two and three repeat 640 clients. Each phase warms for 2 seconds and measures for 5 seconds.
- One persistent owner per disk, 20 I/O cores, no Tokio runtime. Queue depth 256, 2-GiB segments, 512-MiB pool per disk, preferred huge pages, 128-KiB device request splitting.
- Engine read CRC disabled; write checksums enabled. Every prefilled key is checked for key/value identity and size before timed reads. Returned buffers are released immediately.
- No discard, global kernel tuning, or device preconditioning. Earlier full-device recovery testing took place between the baseline and this run.
- Rust 1.98.0, GNU/glibc release, `-C target-cpu=x86-64-v3`. Base commit `a512c3dd223e359fa8286a7e0e73da436961eee5` plus the uncommitted migration changes. [Build and source snapshot hashes](build.json) identify the exact measured snapshot; the operator retains its source archive and patch.
- Executable SHA-256: `b794d48dbcd5b5d8b51c078a0e9472f31b3b8b3faf15aee18031d08289555763`.

## Validation and limits

- All 15 prefills and 25 measured read phases completed. All 24,176,580 prefilled records passed verification; all twenty devices served physical reads in every phase.
- Runtime sampling every 250 ms observed at most 21 tasks and 0 io-wq helpers. This is sampled observation, not proof of absence between samples.
- Post-verification anonymous huge-page backing: 10.00–10.00 GiB per process.
- Preflight checked exact host, serials, capacities, system-device exclusion, partitions, mounts, holders, signatures, open users, two seconds of idle counters, and held exclusive cooperative device locks throughout the run.
- Before/after checks found all twenty controllers live and both system RAID mirrors intact. **SMART queries were unavailable** to the benchmark account (`Permission denied`, no passwordless sudo); no SMART health conclusion is claimed.
- No segment reuse or GC is exercised. This run cannot establish cache-layer performance, capacity-pressure behavior, long-lived read-buffer retention, or recovery correctness.

[All read samples](samples.csv) · [Concurrency summaries](summary.csv) · [All prefills](prefill.csv) · [Prefill medians/ranges](prefill-summary.csv) · [Runtime observations](runtime.csv) · [Phase observations](runtime-phases.csv)
