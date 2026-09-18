# Preallocated benchmark source buffers

The disk benchmark now prepares a bounded source-buffer pool on each pinned
Moat owner before timing. It recycles inputs after successful admission, when
the adapter has copied their contents into the engine's registered I/O buffers.
This removes repeated temporary value allocation/free from the default measured
path while retaining full value generation, the final copy, CRCs, engine work,
I/O completion, and the configured final fsync.

With the same source-pool implementation in both engine revisions, the current
64-KiB median is **35.752 GiB/s**, versus **35.169 GiB/s** for the original engine.
Separate diagnostics find no timed `MADV_DONTNEED` calls in pooled mode, compared
with 2,426 in the same current binary's owned-input probe. This implements the
producer-side remedy motivated by the [root-cause controls](../64k-root-cause/REPORT.md).

## Implementation and comparison contract

`moat_input_pool` defaults to `true`. Each owner creates and touches its source
values before sending the worker-ready notification. The pool is capped by the
local batch and dataset sizes, 64 records, and the existing roughly 4-MiB
generation target. One value may exceed the byte target. At 64 KiB there are
64 buffers per owner: 4 MiB of source payload per owner, or 80 MiB across twenty
owners, in addition to registered engine storage and metadata.

Every use still overwrites the complete value and updates key/identity data.
Backpressure retains the pending input; partial admission recycles only the
accepted prefix. Source buffers remain allocated across all measured phases,
so destruction is not moved to the end of the timed write burst. The engine's
I/O buffer ownership and recycling are unchanged. This is a benchmark change;
the engine does not require allocating an intermediate source `Vec` per write.

`moat_input_pool: false` preserves the allocating-input control. Foyer takes
ownership of values and retains its existing owned-input path; this option does
not enable premature reuse of Foyer values. `DRIVER.input` records `pooled` or
`owned`, and twenty `INPUT` records describe each owner's source-pool capacity.
The standard analyzer separates input modes and fsync policies, treating older
logs without the pool option as owned-input measurements.

The original engine was rebuilt with exactly the same `main.rs`, `workload.rs`,
and `engines/mod.rs` benchmark code as current. Its engine and engine adapter
remain the original versions. Thus **both columns use the new input pool**;
the original column is not the unchanged historical executable with allocating
inputs. Source and binary identities are in [build.json](build.json).
All builds use glibc, with no dummy startup allocation or allocator tuning.

## Twenty-device results

The main matrix uses five sizes, three variants, and three repetitions with
rotated ordering. All numbers below are medians of uninstrumented processes.
The original retains its original fsync behavior; current enabled/disabled use
one binary with explicit policy selection. Disabled fsync changes the persistence
contract and is not generally interchangeable with enabled fsync.

| Value | Unit | Original, pooled input | Current pooled, fsync enabled | Current pooled, fsync disabled |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 49.205 | 51.081 | 76.714 |
| 1 KiB | Mops/s | 33.882 | 37.440 | 42.369 |
| 4 KiB | Mops/s | 19.920 | 20.322 | 21.025 |
| 64 KiB | GiB/s | 35.169 | 35.752 | 34.942 |
| 4 MiB | GiB/s | 38.807 | 38.722 | 38.960 |

The 64-KiB enabled ratio of medians is +1.7%. Paired changes have mixed signs,
including one current run about 4.6% slower than its original control. This
supports removal of the preceding large regression, not a stable small speedup
or statistical equivalence certification. Small writes remain short bursts:
the original 100-B results range from 45.271 to 67.437 Mops/s, and 1-KiB paired
changes also have mixed signs. Do not present their median gains as established
per-record engine improvements. Random-read median throughput differs from
the original by at most 0.23% across the five sizes in this matrix.

Six additional uninstrumented runs use the same rebuilt binaries with the input
pool disabled at 64 KiB, alternating engine order over three repetitions:

| Input policy | Original GiB/s | Current fsync-enabled GiB/s |
|---|---:|---:|
| Owned, allocate/free each value | 34.800 | 30.484 |
| Preallocated pool | 35.169 | 35.752 |

Current throughput increases by 17.3% by ratio of these medians. The owned
controls follow the main matrix rather than interleaving both input policies;
retain that temporal limitation. Their values also should not replace the
earlier experiment's allocating-input samples, which used different harness
binaries. The independent faults and syscall controls below establish the
intended mechanism directly.

## Reclamation and faults

One diagnostic build adds only writer-phase timestamps and thread resource
counters. It runs once per input mode without tracing, then once per mode under
strace; every process still uses all twenty devices. Timing windows exclude
startup and teardown. Major faults are zero in both untraced probes.

| Current input policy | Minor faults, twenty-owner sum | Timed MADV_DONTNEED calls | Cumulative advised bytes |
|---|---:|---:|---:|
| Owned | 2,412,015 | 2,426 | 9,823,002,624 |
| Pooled | 15,717 | 0 | 0 |

Fault counts come from the untraced processes; syscall counts come from separate
traced processes. Advised bytes are summed lengths, not unique memory.
Unfinished/resumed trace entries count once. These instrumented processes are
excluded from throughput summaries. Remaining faults include other benchmark
and engine allocations: pooled mode does not claim that the entire process
performs no allocations or incurs no faults.

## Coverage and validation

All **55 completed processes used all twenty authorized data devices** within
the existing 64-GiB/device window: 45 pooled matrix runs, six owned controls,
two untraced fault probes, and two syscall traces. All **74,167,740 prefilled
records** passed identity/size/sampled-content checks; every process recorded
write and read activity on every device. This is not exhaustive byte verification
or power-loss testing. Final checks found all devices idle with no open users;
controller-state and system-mirror checks passed. SMART remained unavailable
to the benchmark account.

The setup retains twenty pinned owners, 512-MiB registered engine pools per
device, queue depth 256, 2-GiB segments, 16-byte keys, and prefill batch 2560
globally. Records per device are
`max(2048, min(131072, 512 MiB / (16 + value_bytes)))`. Main random reads use
640 global clients with two seconds of warmup and two seconds of measurement;
owned controls and probes use one measured second. No kernel, device-cache, or
global allocator settings change.

Six benchmark tests passed, including bounded pool reuse across repeated
prefills, partial admission/backpressure/reordered completion, and replacement
of every value byte without reallocating the source buffer across both input
representations and their boundary. All-target Clippy with warnings denied,
formatting, and diff checks passed. An analyzer check confirms that historical
owned-input, pooled-input, and fsync-disabled samples form distinct groups.

Every measurement and range is retained in [samples.csv](samples.csv) and
[summary.csv](summary.csv); diagnostics are in [faults.json](faults.json) and
[traces.json](traces.json). Private operational inputs, device identities, raw
traces, and host paths stay outside the repository.
