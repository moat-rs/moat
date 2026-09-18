# Engine pipeline benchmark

This standalone workspace measures the moat engine with Linux direct I/O: append writes, multi-segment allocation, recovered indexes, and random reads. The current runner accepts only `moat`; reproduce historical v1/legacy comparisons at the revisions recorded in their reports.

The [2026-09-15 report](../../docs/experiments/engine/2026-09-15/REPORT.md) includes three repetitions,
fresh fio baselines, numeric samples and independent CPU profiles. That report
used `--verify true`; use the same flag to reproduce its read policy. Historical
reports identify their source revision; the current harness adds registered
buffers to the engine and defaults to `--huge-pages preferred`.

The [direct-read follow-up](../../docs/experiments/engine/2026-09-15-direct/REPORT.md) remeasures both
engines with `--verify false`, adds partial ranges, and separates user/system CPU
cost. It includes 168 engine samples and records the large boundary-range
variation rather than treating its median as a stable throughput advantage.

The [registered-buffer follow-up](../../docs/experiments/engine/2026-09-15-registered/REPORT.md) uses
fixed buffers/files and deferred completion work on both sides. It compares
matching `Disabled` and `Preferred` huge-page policies, records observed memory
backing, and retains 228 measured phases across two three-repetition matrices.

The [full-device comparison](../../docs/experiments/engine/2026-09-16-full/REPORT.md) fills one data device
with distinct 4-MiB records under both engines, then measures
random reads across the entire written key range. It includes rollover and
sealing, preserves fill-progress samples, and states the limits of one fill per
engine. Tiny-record full fills are not part of that report.

## Build and run

The executable uses the system allocator. `--sync false` disables format and
runtime persistence barriers, including explicit flush; the default is true.
Disabled flush still waits for preceding writes. Startup output records the
selected sync policy; comparisons must distinguish these durability contracts.

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
and sufficient memlock for the engine's registered 1-GiB buffer pool.
It does not require sudo or discard/format the entire NVMe namespace.

```sh
python3 benchmarks/engine/run.py \
  --device /dev/REVIEWED_SCRATCH_DEVICE --serial EXPECTED_SERIAL \
  --binary target/engine-compare/x86_64-unknown-linux-gnu/release/moat-engine-compare \
  --output /path/outside/repository/results --cpu NUMA_LOCAL_CPU \
  --repeats 3 --seconds 10 --payload-mib 512 --overwrite-first-4g
```

The engine uses 2-GiB segments after two 4-KiB device superblocks. Each segment has an allocation header and a footer trailer at its tail. Formatting, allocation, and recovery stay within the configured device window, and every run uses a fresh random device identity.

### Whole-device mode

`--overwrite-entire-device` **destroys data throughout the assigned device**.
The runner additionally requires its exact byte capacity and checks a
conservative index-memory estimate. Use a separate output directory:

```sh
python3 benchmarks/engine/run.py \
  --device /dev/REVIEWED_SCRATCH_DEVICE --serial EXPECTED_SERIAL \
  --expected-capacity CAPACITY_BYTES --overwrite-entire-device \
  --binary /path/to/moat-engine-compare --output /path/to/full-results \
  --cpu NUMA_LOCAL_CPU --sizes 4194304 --repeats 1 --seconds 60
```

The engine writes distinct consecutive keys until admission reports no free
segments, drains writes, flushes, and seals the final segment. The harness
requires all available segments to have been allocated. Progress is emitted to
stderr every 30 seconds. The measured write interval includes rollover and
sealing. The index reserves capacity before timing in this mode.

Random reads select from the entire written key range, including all allocated
segments. They do not mean that every record is read during a timed phase.
Logical payload occupancy is less than raw capacity because of metadata,
page padding, space too small for another record, and a trailing partial slot.
This is one complete append-only fill, not a steady-state overwrite/GC workload
or a claim of device preconditioning. One repeat is one full fill;
additional repeats repeat the full fill and must be budgeted accordingly.

Distinct-key full-device workloads currently require values of at least 64 KiB.
The indexes are resident; filling a large device with tiny distinct records can
exceed RAM. Smaller datasets spread across a device would be a different
workload and must not be reported as a full tiny-record fill.

For a short functional exercise, explicitly create a disposable sparse file
of 4 GiB, then invoke the binary directly. The binary opens an existing path
and never creates or truncates a file. Direct I/O must be supported.

```sh
truncate -s 4G /path/to/new-disposable.img
target/engine-compare/x86_64-unknown-linux-gnu/release/moat-engine-compare \
  /path/to/new-disposable.img moat mixed 64 1 1,64 --overwrite-first-4g --verify false
```

## Matched workload

- One thread, fixed CPU, io_uring depth 64, direct I/O. No SQPOLL or IOPOLL.
- The engine registers its pool arenas and file and requests `SINGLE_ISSUER`
  with `DEFER_TASKRUN`. It uses a 1-GiB common pool with an 8-MiB maximum class.
  `--huge-pages disabled|preferred|required` selects the pool policy;
  the default is `preferred`. Keep policies in separate output directories.
  Registration errors stop the run. No system huge-page reservation is changed.
- Uniform records: 100 B, 1 KiB, 4 KiB, 64 KiB and 4 MiB. Mixed records cycle
  through 100 B, 4 KiB, 64 KiB and 300 B in that order, at equal record counts.
- Writes generate consecutive distinct keys. Groups contain 64 records for
  small/mixed workloads and 16 for uniform large workloads. The driver polls
  after each group and reap on backpressure. Moat puts a small/mixed group in one
  frame; uniform large values use a prepared frame each.
- The write interval includes copying values, computing CRCs, index publication,
  completion processing and the final durable flush. No precomputed checksums
  are passed to the prepared-write path. Allocation of initial I/O
  buffers, input patterns, formatting and opening occur before the timer.
- In bounded mode, each dataset contains at most 512 MiB of payload, rounded down to complete
  groups, with a minimum of one group. This is **burst write throughput**, not
  sustained, full-device steady-state ingestion or per-record durable latency.
- Uniform random full-value reads use logical concurrency 1 and 64; 4-MiB
  records also use concurrency 16. Each phase warms for two seconds, measures
  for ten seconds by default, and drains accepted requests before stopping.
- The engine uses the selected verification policy: `--verify false` (default) or
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
- The runner defaults to three fresh write/read repetitions using moat. Read selection
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

The analyzer groups by device scope/capacity, huge-page policy, verification mode and range as well as workload, engine
and concurrency. It retains each numeric sample and reports medians and the minimum
and maximum throughput per configuration. Median latency columns are medians
of run percentiles, not percentiles of a combined latency histogram.

Device counters count physical requests after kernel merging/splitting, not
engine submissions. Derive read/write byte amplification by dividing physical
bytes by completed payload bytes. Compare logical operation counts separately
from fio physical IOPS. A 4-MiB operation may split into many device requests.

Each sample records requested huge-page policy, arena backing, fixed-buffer/file
registration and actual deferred-taskrun status. `/proc/self/smaps` supplies
observed anonymous huge-page and hugetlb bytes outside the measured interval.
If a pool mapping merges with unrelated memory, these observations are `null`
rather than attributing unrelated huge pages to the pool. `Transparent` alone
means a successful hint, not guaranteed promotion.

Moat uses an owner-local hash index. Bounded-dataset runs include index growth, frame construction, and completion metadata allocation in write timing; full-device runs reserve index capacity before timing. Historical v1 comparisons remain in the experiment archive, and the current runner contains no legacy implementation.

The bounded data window is small and repeatedly accessed. Direct I/O bypasses the OS
page cache but does not eliminate device-side caching or prove cold-media
latency. This suite does not test recovery, crashes, bit corruption,
multi-worker scaling, concurrent mutation or space reclamation. Whole-device
mode additionally exercises rollover and routing across all allocated segments.
