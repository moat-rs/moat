Native adapter CPU profiles
============================

Two separate complete-process profiles use the same measured executable, one
per engine, with 64-KiB values and 640 total read requests. Each writes and
verifies 65,520 records per device (80 GiB of values across 20 devices), then
warms reads for two seconds and measures for eight. This larger write dataset
helps sample more than the short main-matrix prefill. Profiling results are
excluded from the throughput tables.

Collection starts the executable under `perf record`, sampling `cycles:u` at
99 Hz with 8-KiB DWARF call chains. Write and read windows use the monotonic
clock and the harness's reported durations. Both profiles report zero lost
samples. [Retained hotspots](profile-hotspots.csv) include all original symbol
rows at or above 0.5%, aggregated after demangling. Unresolved addresses are
retained as separate categories, not assigned guessed names.

The profiler still reports kernel-address samples for this event, particularly
around syscall boundaries. These percentages are the observed self-cycle
sample distribution, not wall-time fractions or a precise user/kernel split.
They are not absolute CPU-work comparisons between engines.

| 64-KiB writes: sampled work | V1 | V2 |
|---|---:|---:|
| Bulk copying | 50.87% | 48.52% |
| Bulk filling | 10.66% | 17.47% |
| Value CRC routine | 6.65% | 6.60% |

The bulk-copy and bulk-fill instructions were confirmed against the sampled
libc's disassembly (`rep movsb` and `rep stosb`); call chains connect them to
payload preparation in the adapter. Both write paths still spend much of their
sampled execution copying or initializing bytes. Metadata checksums, allocation,
and engine bookkeeping are visible too. These profiles do not isolate the cause
of the main matrix's 4.4% v2 write gap at 64 KiB, and do not establish a format
limit or a dominant engine mutex.

The read profiles show native polling, index/extent lookup, buffer allocation
and return, validation, and clock/syscall work. V1 also spends samples checking
its writer during the common poll operation. The old Tokio completion path is
absent from this adapter by construction; no runtime is created for either
engine. This removes that previously measured scheduling bottleneck without
changing the engine library or layout.
