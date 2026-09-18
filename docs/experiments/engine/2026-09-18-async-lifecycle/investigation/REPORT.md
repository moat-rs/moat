# Asynchronous lifecycle regression investigation

Follow-up: [allocator root cause and initial performance fixes](followup/REPORT.md)
isolates the previously unresolved recovery-allocation trigger and records the
subsequent code changes and remaining small-value regression.

The write regressions have different causes. A write-completion byte budget
accounts for the measured 4-MiB gap. For 64-KiB values, allocator trimming of the
benchmark's temporary input values accounts for most of the gap. Small values
pay extra index-admission work, but the isolated index change does not explain
the entire short-prefill regression. The additional io-wq workers come from
asynchronous fsync and are not the main cause of either large-value regression.

These are diagnostic experiments, not a merged performance fix. Production
engine and benchmark source files were left unchanged during this investigation.

## Method and coverage

All **87 processes used all twenty assigned devices** and the same 64-GiB
per-device window, owner cores, pool settings, and 2-GiB segment size as the
[parent experiment](../REPORT.md). All **157,857,920 prefilled records** passed the
existing identity/size/sampled-content verification, followed by one measured
read phase per process. This is not exhaustive byte or corruption validation.
The final twenty-device idle/open-user check passed; controller and system-mirror
checks passed. SMART remained unavailable to the benchmark account.

There were 75 uninstrumented runs, eight perf runs, and four strace runs. Reported
throughput comparisons use three uninstrumented repetitions, with variant order
rotated or alternated. Diagnostic read phases used 640 clients, two seconds of
warmup and one second of measurement. Their purpose was validation, not another
read-performance claim. No global allocator or kernel settings were changed.

The unchanged baseline and candidate executables are the exact hashes from the
parent experiment. Each compiled ablation starts from the candidate source
archive and changes only the described factor. Build hashes and variant
specifications are in [build.json](build.json). Raw operational inputs, traces,
and perf recordings remain private. [samples.csv](samples.csv) preserves every
prefill; [summary.csv](summary.csv) includes ranges as well as medians.

## 4-MiB values: write-completion byte accounting

`Pipeline::poll` charges `completion.request.len` when it dequeues a completion.
`retire_writes` then charges the entire buffer capacity again when publishing the
index and returning the buffer. Neither step scans the write payload. The default
byte target is 1 MiB; a single 4-MiB write therefore exhausts it at completion,
and again at retirement on the next poll. Buffer capacity can exceed actual
encoded length because pool classes are powers of two.

The `budget` variant removes those two charges for writes, preserving the
operation and record budgets, asynchronous lifecycle, CRCs, and persistence
barriers. It does not disable all poll bounds.

| Variant | Write GiB/s | CPU microseconds/record |
|---|---:|---:|
| Baseline | 38.776 | 1663.552 |
| Candidate | 34.654 | 1856.122 |
| Candidate with corrected write byte accounting | 38.709 | 1654.900 |
| Candidate with synchronous fsync only | 34.768 | 1841.491 |

The budget variant improved throughput in all three pairs: approximately 10.9%,
14.5%, and 11.4%. Its median is within 0.2% of baseline. This identifies the
accounting policy as a causal factor in this workload. Delayed publication and
buffer recycling are a plausible mechanism; the experiment did not directly
measure cache misses or buffer residence time. CPU samples remained dominated
by data copying, filling, and CRC rather than the budget arithmetic itself.

The same byte-accounting change did **not** recover the 64-KiB gap. Increasing
the operation budget from 64 to 512 in addition also failed to close that gap.

## 64-KiB values: temporary input allocation and page reclamation

The driver constructs `Vec` values in `Put::new`, copies them into prepared I/O
buffers, and frees those temporary values as requests are accepted. That work is
inside prefill timing. The 64-KiB case contains 8,190 records per device, roughly
10 GiB of logical values across the machine, and lasts only about 0.3 seconds.

Profiling the **original, unchanged binaries** found additional samples on a
kernel path reached from the value-initialization `memset`. Kernel symbols were
unavailable, so that stack alone is not a named-kernel-function attribution.
A subsequent syscall trace and same-binary allocator ablation provide the
stronger evidence:

| Variant | Prefill `MADV_DONTNEED` calls | Bytes discarded |
|---|---:|---:|
| Baseline, default allocator | 0 | 0 |
| Candidate, default allocator | 2,462 | 9,831,927,808 |
| Baseline, raised trim threshold | 0 | 0 |
| Candidate, raised trim threshold | 0 | 0 |

Discarded bytes are the sum of syscall lengths, not unique pages or retained
RSS. Phase boundaries use the timestamp at which an external stdout reader saw
`PREFILL`, minus the reported elapsed time; they have a small observation delay.
Trace runs are mechanism evidence and are excluded from throughput medians.
See [trace-summary.json](trace-summary.json).

The allocator ablation launches the exact same binaries with only
`MALLOC_TRIM_THRESHOLD_=1073741824` in the child environment. The glibc setting
controls when releasable arena memory is returned to the OS; explicitly setting
it also disables dynamic threshold adjustment. It is a process-local diagnostic,
not a recommended production default. See the
[glibc allocator documentation](https://sourceware.org/glibc/manual/latest/html_node/Memory-Allocation-Tunables.html).

| Variant | Default allocator GiB/s | Raised trim threshold GiB/s |
|---|---:|---:|
| Baseline | 35.754 | 34.593 |
| Candidate | 28.507 | 35.355 |

The candidate improves in every pair, approximately 27.1%, 7.1%, and 16.5%; its
median improves 24.0%. The default-allocator gap largely disappears under this
controlled allocation change. Thus this result cannot be interpreted as a
17–20% loss of raw engine/device write capability. It is still a real regression
of the measured allocation-heavy input path and should remain visible.

The exact engine allocation/lifetime change that makes this heap pattern trim
more often is **not isolated**. Replacing the pending-entry vector's exact
reservation with a minimum capacity of four did not recover performance
(29.592 versus 29.817 GiB/s medians). That negative result rules out treating
that one allocation-size edit as an established fix. A larger, instrumented
64-KiB dataset also reduced the gap, but duration, allocation history and
instrumentation all changed, so it is supporting context, not an independent
proof of steady-state equivalence.

## Small values: extra index work, plus short-run sensitivity

Each inserted key now has an admission `contains_key` lookup in
`reserve_entries`, then another `contains_key` for the cursor key vector, and
finally an `entry` lookup to publish its location. The original publication
path needed only the last lookup. In the enlarged 100-B diagnostic run,
`reserve_entries` accounted for 9.8% of the reported cycle-sample weight.

The `index` variant combines publication into one `Entry` lookup. Admission
uses an inexpensive conservative upper bound while comfortably below the hard
limit, retaining exact lookup/deduplication near the limit. Limits, tombstones,
version order, and the cursor key vector remain present.

| Records/device | Baseline Mops/s | Candidate Mops/s | Index variant Mops/s |
|---|---:|---:|---:|
| 131,072 | 56.409 | 46.358 | 49.236 |
| 524,288 | 61.472 | 58.974 | 60.129 |

The short-run index variant gains 6.2% by ratio of medians, but one of its three
pairs is slightly slower. The larger dataset's candidate/baseline gap is only
4.1%, and its index ablation also has mixed paired signs. Extra lookup work is a
measured hotspot and an avoidable cost; **the original 20.7% short-prefill loss
is not wholly explained by it**. Asynchronous initial allocation introduces
retries, and the short benchmark is sensitive to startup and allocation history.
Do not publish the largest observed percentage as a stable per-record engine
cost. The 1-KiB case was not independently ablated in this investigation.

[profile-counters.json](profile-counters.json) retains per-phase CPU, poll,
admission, and rejection counts from separate instrumented binaries. Counter
runs are not mixed into uninstrumented throughput comparisons. Perf was configured
with `cycles:u`; interrupt skid still produced some kernel-address samples.

## io-wq: asynchronous fsync, not evidence of payload offload

Lifecycle allocation and sealing now submit `Operation::Sync`, which becomes
`IORING_OP_FSYNC` with `DATASYNC`. In the upstream Linux 6.8 implementation,
`io_fsync_prep` sets `REQ_F_FORCE_ASYNC`; `io_fsync` requires a blocking context.
See the [kernel implementation](https://raw.githubusercontent.com/torvalds/linux/v6.8/io_uring/sync.c).
The test host runs a distribution Linux 6.8 kernel, so this source explains the
mechanism; the local ablation supplies the runtime evidence.

Across the nine `sync` diagnostic processes, making only that operation call
`sync_data` synchronously reduces the sampled maximum from 41 tasks / 20 io-wq
workers to 21 tasks / zero io-wq workers. No fsync barriers are skipped. This
change fails to recover the large-value throughput and violates the desired
nonblocking owner contract, so it is not a fix to adopt.

Keep three distinct requirements explicit: asynchronous completion at the API,
nonblocking engine-owner execution, and absence of kernel worker threads. The
first two do not imply the third with this persistence primitive. Sampling at
250 ms cannot prove that workers never appeared between samples, and worker
presence alone does not establish that payload reads/writes were offloaded.

## Implementation direction

1. Charge poll byte budgets for actual CPU byte work, and use operation/record
   bounds for write completion and index publication. Preserve cooperative
   progress and terminal-completion guarantees; validate fairness separately.
2. Remove redundant index lookups while preserving exact hard-limit behavior,
   duplicate-key handling, latest-version/tombstone semantics, and cursor bounds.
3. Add a reusable-input or direct-producer benchmark alongside the current
   allocation-heavy path. Keep the existing path and allocator/fault evidence
   visible; do not silently tune malloc to hide it. Further allocation-lifetime
   profiling is needed before naming a specific engine-side heap-layout fix.
4. Retain nonblocking persistence semantics. If zero io-wq workers is a required
   contract, investigate a different backend persistence primitive with explicit
   durability and portability validation rather than reverting to blocking fsync.

No production fix or fairness/power-loss certification is claimed by these
ablations. They identify the budget error, the allocator interaction, the fsync
worker source, and the remaining attribution limits.
