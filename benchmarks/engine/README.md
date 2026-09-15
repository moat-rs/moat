# Engine pipeline comparison

This standalone crate compares `moat-engine` and `moat-engine-v2` on the same
Linux direct-I/O device. Neither engine depends on this harness or on the other
engine. The v2 side exercises its current single-segment pipeline; this is not
a comparison of complete storage services.

The [2026-09-15 report](reports/2026-09-15/REPORT.md) includes three repetitions,
fresh fio baselines, numeric samples and independent CPU profiles. That report
used `--verify true`; use the same flag to reproduce its read policy.

## Build and run

Build for GNU/Linux with glibc. `x86-64-v3` requires a compatible destination CPU;
do not use the build machine's `target-cpu=native` for an unrelated test machine.

```sh
CARGO_TARGET_DIR=target/engine-compare \
  RUSTFLAGS='-C target-cpu=x86-64-v3' \
  cargo build --locked --release --target x86_64-unknown-linux-gnu \
  --manifest-path benchmarks/engine/Cargo.toml
```

The raw-device runner **destroys contents in the first 4 GiB**. Use only an
explicitly assigned, idle scratch device. It requires the expected serial,
checks for partitions, mounts, signatures, holders and observed activity, and
uses an advisory lock to exclude another instance. It needs raw device access
and sufficient memlock for the legacy engine's registered 1-GiB buffer pool.
It does not require sudo or discard/format the entire NVMe namespace.

```sh
python3 benchmarks/engine/run.py \
  --device /dev/REVIEWED_SCRATCH_DEVICE --serial EXPECTED_SERIAL \
  --binary target/engine-compare/x86_64-unknown-linux-gnu/release/moat-engine-compare \
  --output /path/outside/repository/results --cpu NUMA_LOCAL_CPU \
  --repeats 3 --seconds 10 --payload-mib 512 --overwrite-first-4g
```

Both engines use a 2-GiB data segment beginning at offset 2 GiB. The legacy
formatter sees a bounded 4-GiB device and writes its superblocks in the first
segment-sized region. The v2 harness persists a fresh active segment header;
its geometry and format bounds are fixed by the harness. Every run uses a new
device identity. V2 device formatting, allocation and rollover remain outside
the pipeline's implemented scope.

For a short functional exercise, explicitly create a disposable sparse file
of 4 GiB, then invoke the binary directly. The binary opens an existing path
and never creates or truncates a file. Direct I/O must be supported.

```sh
truncate -s 4G /path/to/new-disposable.img
target/engine-compare/x86_64-unknown-linux-gnu/release/moat-engine-compare \
  /path/to/new-disposable.img v2 mixed 64 1 1,64 --overwrite-first-4g --verify false
```

## Matched workload

- One thread, fixed CPU, io_uring depth 64, direct I/O. No SQPOLL or IOPOLL.
- Uniform records: 100 B, 1 KiB, 4 KiB, 64 KiB and 4 MiB. Mixed records cycle
  through 100 B, 4 KiB, 64 KiB and 300 B in that order, at equal record counts.
- Writes generate consecutive distinct keys. Groups contain 64 records for
  small/mixed workloads and 16 for uniform large workloads. Both sides poll
  after each group and reap on backpressure. V2 puts a small/mixed group in one
  frame; uniform large values use a prepared frame each. Legacy retains its
  existing packing and large-record policy.
- The write interval includes copying values, computing CRCs, index publication,
  completion processing and the final durable flush. No precomputed checksums
  are passed to the legacy prepared-write path. Allocation of initial I/O
  buffers, input patterns, formatting and opening occur before the timer.
- Each dataset contains at most 512 MiB of payload, rounded down to complete
  groups, with a minimum of one group. This is **burst write throughput**, not
  sustained, full-device steady-state ingestion or per-record durable latency.
- Uniform random full-value reads use logical concurrency 1 and 64; 4-MiB
  records also use concurrency 16. Each phase warms for two seconds, measures
  for ten seconds by default, and drains accepted requests before stopping.
- Both sides use the same verification policy: `--verify false` (default) or
  `--verify true`. Writes always compute CRCs and end with a durable flush.
  Reads additionally check length, the first eight returned bytes and the last
  byte against the deterministic input pattern. The key prefix is checked only
  when included in the requested range. These samples are not a full integrity
  scan. Any I/O, enabled checksum, admission or sampled content error terminates
  the run.
- `--range START:END` selects a logical byte range instead of the full value.
  Use `--sizes` on the runner to select values large enough to contain it, for
  example `--sizes 65536 4194304 --range 4096:8192`. Range reads use concurrency
  1 and 64. Keep different range/mode runs in separate output directories.
- Three fresh write/read repetitions alternate engine order. Read selection
  uses the same fixed random seed. Latency samples cover one request in 16,
  from read admission through delivery to the common verification callback.

## Metrics and interpretation

JSON lines contain payload throughput, completed API operations, sampled
latencies, process user/system CPU time and Linux block-device counters.
The runner never overwrites an existing sample file. Operational stderr stays
separate. Store raw files outside the repository; publish only sanitized data.

```sh
python3 benchmarks/engine/analyze.py /path/to/results /path/to/summary
```

The analyzer groups by verification mode and range as well as workload, engine
and concurrency. It retains each numeric sample and reports medians and the minimum
and maximum throughput per configuration. Median latency columns are medians
of run percentiles, not percentiles of a combined latency histogram.

Device counters count physical requests after kernel merging/splitting, not
engine submissions. Derive read/write byte amplification by dividing physical
bytes by completed payload bytes. Compare logical operation counts separately
from fio physical IOPS. A 4-MiB operation may split into many device requests.

The two implementations have intentionally retained differences: legacy uses
registered buffers and a preallocated concurrent index; v2 uses ordinary
io_uring buffers and a growing single-owner index. Initial legacy index
allocation is outside the timer; v2 index growth is inside it. The v2 builder
and completion metadata allocate in the measured write path. No huge-page
policy is requested for either side. These results compare the current code,
and do not isolate only the on-disk format or the benefit of removing locks.

The data window is small and repeatedly accessed. Direct I/O bypasses the OS
page cache but does not eliminate device-side caching or prove cold-media
latency. This suite does not test recovery, crashes, bit corruption,
rollover, multi-worker scaling, concurrent mutation or space reclamation.
