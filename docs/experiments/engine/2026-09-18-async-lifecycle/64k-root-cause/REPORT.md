# 64-KiB regression: bidirectional allocator and page-fault controls

The dominant cause of this workload's 64-KiB regression is **repeated reclamation
and repopulation of the benchmark's temporary input pages**. The async recovery
change removed an unused 8-MiB allocation that had incidentally altered glibc's
process-wide allocation thresholds. With the default allocation history, the
current owned-input benchmark repeatedly discards arena pages when freeing its
temporary values, then faults them back in while constructing subsequent values.

This investigation strengthens the preceding
[system-allocator/fsync comparison](../libc-fsync/REPORT.md): forcing a low trim
threshold makes the original engine slow too; forcing a high threshold recovers
the current engine. Both keep glibc, fsync, checksums, the same input ownership,
and the same engine binaries. Page-fault counts, syscall traces, and separate
timers reproduce and remove the associated costs in both directions.

The subsequent [preallocated-input implementation](../input-pool/REPORT.md)
adds explicit source reuse to the benchmark and rechecks both engine revisions
with the same harness, retaining allocating-input controls and fault/trace data.

## Controlled throughput comparison

All measurements below use all twenty authorized data devices. Each cell is the
median of three uninstrumented processes. Only the child process's
`MALLOC_TRIM_THRESHOLD_` environment setting changes; no global allocator or
kernel configuration changes. Fsync remains enabled in every case.

| Allocator trim policy | Original GiB/s | Current GiB/s | Current / original |
|---|---:|---:|---:|
| Default, dynamically adjusted | 36.533 | 28.469 | -22.1% |
| Fixed 128 KiB | 29.674 | 29.221 | -1.5% |
| Fixed 16 MiB | 35.746 | 35.516 | -0.6% |

The low-threshold control removes the original engine's advantage. The
high-threshold control restores current throughput while preserving the input
allocation/free pattern. All three high-threshold pairs differ by 0.6–1.4%.
Those small differences are not a formal equivalence test, but they do not
support a fixed 20–23% loss of engine write capability. The previous input-reuse
experiment's residual 5% likewise cannot be treated as a stable intrinsic engine
cost: this more direct control leaves a 0.6% ratio-of-medians difference.

Explicit trim settings also disable glibc's dynamic threshold adjustment. The
fixed-low versus fixed-high comparison holds that behavior constant and changes
the trim threshold itself. These settings are diagnostic controls, not a proposed
library-wide malloc policy.

## Where the time goes

Separate instrumented builds time four regions of each owner's prefill loop:
constructing `Put` inputs, dropping accepted inputs, adapter `put`, and adapter
`poll`. Timing uses one diagnostic process per condition, with the median of
twenty owners shown below in milliseconds. Input construction includes allocation,
full value initialization, and identity stamping; input destruction includes the
underlying frees. Owner elapsed time excludes the driver's final fsync.

| Condition | Input construction | Input destruction | Adapter put | Adapter poll | Owner elapsed |
|---|---:|---:|---:|---:|---:|
| Original, default | 58.257 | 0.618 | 158.207 | 47.502 | 280.390 |
| Current, default | 136.667 | 39.298 | 93.282 | 44.181 | 329.120 |
| Original, fixed 128 KiB | 138.478 | 40.072 | 92.887 | 42.386 | 325.916 |
| Current, fixed 16 MiB | 56.434 | 0.642 | 153.750 | 50.028 | 277.716 |

The extra cost reproduces in the original engine under the low threshold and
disappears in the current engine under the high threshold. Both construction and
destruction contribute. The adapter `put` time also changes with allocation
policy, even for an unchanged engine binary; it includes payload copying and
checksumming and must not be interpreted as an allocation-independent measure
of engine speed. Independent column medians need not add to the elapsed median.

`getrusage(RUSAGE_THREAD)` brackets the same prefill loop. Fault counts below are
sums across twenty owners; they are from the untraced diagnostic processes.

| Condition | Minor faults | Major faults |
|---|---:|---:|
| Original, default | 35,822 | 0 |
| Current, default | 2,408,064 | 0 |
| Original, fixed 128 KiB | 2,403,442 | 0 |
| Current, fixed 16 MiB | 36,159 | 0 |

The current default has approximately **67 times** the original's minor faults.
These faults do not require disk reads, as distinguished by
[getrusage(2)](https://man7.org/linux/man-pages/man2/getrusage.2.html).
Separate syscall traces count only calls issued by each owner during its measured
prefill interval:

| Condition | MADV_DONTNEED calls | Sum of advised lengths |
|---|---:|---:|
| Original, default | 0 | 0 |
| Current, default | 2,426 | 9.148 GiB |
| Original, fixed 128 KiB | 2,412 | 9.095 GiB |
| Current, fixed 16 MiB | 0 | 0 |

Advised lengths are cumulative, not unique pages. Traces include both ordinary
and unfinished/resumed syscall entries, counting each invocation once; no syscall
failure was observed. Strace timing is excluded from throughput and timer tables.
For private anonymous memory, subsequent accesses after `MADV_DONTNEED` use
zero-fill-on-demand pages; see
[madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html). The trace and
fault measurements therefore explain both the expensive free and the extra
work when the next inputs are initialized.

## Trigger and call path

The source-level sequence is:

1. The original `recover()` allocated an 8-MiB `AlignedBuf` before examining
   segments, including on a freshly formatted empty device. Incremental recovery
   now allocates frame storage only when needed.
2. Glibc 2.39 can raise its dynamic mmap threshold when a sufficiently large
   mmap-backed chunk is freed, and sets the trim threshold to twice that size.
   The removed allocation could therefore leave a trim threshold near 16 MiB.
   Explicitly setting a trim threshold disables that dynamic adjustment. See
   [glibc malloc.c](https://raw.githubusercontent.com/bminor/glibc/glibc-2.39/malloc/malloc.c).
3. The benchmark's `Put::new` creates a fresh 64-KiB value `Vec`. It prepares up
   to 64 inputs at a time, around 4 MiB. After a prepared I/O buffer receives its
   copy and the engine accepts the write, `pending.drain(..count)` frees the
   temporary input. The registered I/O buffer has a separate lifetime.
4. The earlier stack trace located `MADV_DONTNEED` under
   `VecDeque::Drain<Put>::drop` and `__libc_free`. Glibc's non-main-arena trimming
   can issue that advice from `shrink_heap`; see
   [glibc arena.c](https://raw.githubusercontent.com/bminor/glibc/glibc-2.39/malloc/arena.c).
5. The next input batch writes fresh contents into the reclaimed addresses,
   repeatedly paying page population and initialization costs.

The prior [opposite source ablations](../investigation/followup/REPORT.md) already
showed that removing the unconditional allocation slows the original, while
adding an equivalent diagnostic allocation speeds the async version and removes
the discards. The new experiment independently controls the allocator policy and
measures the missing page-fault and input-time evidence. Glibc's internal `mp_`
values were not read directly; their dynamic values are inferred from the source
and these controlled interventions, rather than presented as sampled variables.

The 64-KiB size is also relevant to glibc's free path: its consolidation/trim check
uses a 65,536-byte threshold after coalescing. A 64-KiB allocation itself satisfies
that gate. Smaller allocations can also reach it through coalescing, so this is
not a claim that every sub-64-KiB workload avoids reclamation or that 64 KiB is a
universal performance boundary.

## What this establishes and what to change

The observed 64-KiB loss is a real end-to-end regression of this input path,
triggered by a recovery-allocation change and expressed through libc reclamation.
Fsync is not its dominant cause: the preceding fsync-off comparison barely
changed 64-KiB throughput, and the controls here retain fsync. Recovery does not
run inside the timed writes; its allocator-history side effect survives into
them. The roughly 512-MiB payload per owner also fits within the 2-GiB segment,
so repeated segment rollover is not the source of the measured gap.

The allocation is temporary caller input, not a registered buffer still owned by
in-flight I/O. Engine payload-buffer ownership is unchanged. The benchmark
already uses the prepared-write path for this size and retains prepared payloads
across backpressure; repeated frame construction is not supported as the dominant
explanation by these measurements.

Preparation has three different scopes in this driver: record keys/IDs/routing
and registered I/O pools are prepared before timing, but `Put::new` constructs
each value inside the timed prefill. `prepared` in the adapter describes the
destination frame API; it does not mean the source value was preallocated.
The timed path is therefore input allocation/fill, copy into a pooled frame,
checksumming/submission, input free, and I/O completion.

A suitable remedy is to make temporary input reuse explicit where the producer
allows it, or let a producer fill prepared pooled storage directly. Retaining a
separate owned-input benchmark keeps the real allocation-sensitive behavior
visible. A library-wide `mallopt` policy affects unrelated callers, and restoring
an unused 8-MiB allocation would depend on allocator history rather than the
engine contract. Neither was introduced by this investigation. Whether to change
the producer API or add bounded producer-side reuse is a separate implementation
decision; this report does not silently substitute a different workload.

## Reproduction and coverage

The successful matrix comprises **26 full twenty-device processes**: eighteen
uninstrumented comparisons, four detailed timing runs, and four syscall traces.
All **4,258,800 records** passed existing identity/size/sampled-content checks;
every process recorded writes and random reads on all twenty devices. An initial
launch rejected a new configuration field in the historical binary before opening
devices; it produced no workload measurement and is excluded from those counts.

The setup preserves the authorized 64-GiB/device window, twenty pinned owners,
512-MiB buffer pool/device, 2-GiB segments, queue depth 256, 16-byte keys, 8,190
configured records/device, and prefill batch 2560 globally. Read verification is
followed by two seconds of warmup and one second of measured random reads at
640 clients; read measurements here validate the workload, not a new performance
claim. Process order is rotated across three repetitions. These are short write
bursts, not long-term fragmentation or steady-state endurance tests.

Exact unchanged binary identities and diagnostic build hashes are in
[build.json](build.json). Per-run results and ranges are in
[samples.csv](samples.csv) and [summary.csv](summary.csv); all owner counters and
timers are in [writer-details.csv](writer-details.csv), and syscall aggregates in
[traces.json](traces.json). The benchmark-only instrumentation is retained as
[instrumentation.patch](instrumentation.patch). No production code was changed
in this investigation. Final checks found all twenty devices idle and without
open users; controller and system-mirror checks passed. SMART remained unavailable
to the benchmark account. Operational identities, raw configurations, and raw
traces remain private.
