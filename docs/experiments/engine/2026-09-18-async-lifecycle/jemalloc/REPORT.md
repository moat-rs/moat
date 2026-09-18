# TiKV jemalloc integration and twenty-device trial

Historical experiment: the optional jemalloc integration was subsequently removed
at the user's request. Current executables use the system allocator. The source
and binary hashes below identify the measured experiment; its build instructions
apply to that snapshot, not the current working tree.

TiKV jemalloc recovers the short 64-KiB write workload without the diagnostic
8-MiB startup allocation. It does not improve every size: 1-KiB and 4-KiB writes
regress with the tested defaults. The integration is therefore an **optional
executable feature**, not a new allocator requirement for the engine library.

## Integration and builds

Both `benchmarks/cache-disk` and `benchmarks/engine` now accept
`--features jemalloc`. This installs `tikv_jemallocator::Jemalloc` as the Rust
global allocator in the executable. Omitting the feature selects the system
allocator. Core libraries remain allocator-neutral, C allocation symbols are
not interposed, and registered I/O buffers still use their direct mmap arenas.

The lockfiles resolve `tikv-jemallocator` and `tikv-jemalloc-ctl` to 0.7.0, and
`tikv-jemalloc-sys` to
`0.7.1+5.3.1-0-g81034ce1f1373e37dc865038e1bc8eeecf559ce8`.
The measured executable reports the matching jemalloc 5.3.1 revision at runtime.
Default settings were retained: dirty decay 10,000 ms, muzzy decay 0 ms, and
background threads disabled. No allocator environment overrides or preload
libraries were used. Statistics support is enabled in the disk comparison.

The two current binaries were built from the same source, including the
[write-budget and index fixes](../investigation/followup/REPORT.md), with only
the allocator feature changed. Neither restores the old recovery allocation,
adds a dummy allocation, or moves initial segment allocation outside timing.
The historical baseline is the frozen executable from the parent experiment.
Builds use release mode, the GNU x86-64 target, `target-cpu=x86-64-v3`, and eight
build jobs. [build.json](build.json) records source and binary hashes.

```sh
RUSTFLAGS='-C target-cpu=x86-64-v3' cargo build --locked --release \
  --target x86_64-unknown-linux-gnu \
  --manifest-path benchmarks/cache-disk/Cargo.toml --features jemalloc
```

Copy the resulting executable before rebuilding the system-allocator control
without `--features jemalloc`. Both builds have the same executable name.

## Method and coverage

All **53 processes used all twenty authorized data devices**, each within the
existing 64-GiB window. Owners, pool settings, queue depth, batch size, keys,
and 2-GiB segments match the parent experiment. No global kernel or allocator
settings changed. All **104,314,620 prefilled records** passed the existing
identity/size/sampled-content verification; this is not exhaustive byte or
power-loss testing. Every read phase recorded activity on all twenty devices.

The main matrix has five sizes, three binaries, and three repetitions with
rotated ordering: 45 processes. Each has two seconds of read warmup and three
seconds of measured random reads at 640 clients. Six additional processes
alternate the current system/jemalloc builds with 262,144 records per device
at 64 KiB, or 16 GiB of value payload per device. Two separate syscall traces
are excluded from throughput summaries. Final checks found all twenty data
devices idle with no open users; controller and system-mirror checks passed.
SMART remained unavailable to the benchmark account.

Full measurements are in [samples.csv](samples.csv), with medians and ranges in
[summary.csv](summary.csv). Host identities, device paths/serials, operational
inputs, and raw traces remain private.

## Write results

All entries below are three-run medians. Percentage changes compare jemalloc
with the **current system-allocator build**, not the historical baseline.

| Value size | Unit | Old baseline | Current system | Current jemalloc | Allocator change |
|---|---|---:|---:|---:|---:|
| 100 B | Mops/s | 50.929 | 43.603 | 46.210 | +6.0% |
| 1 KiB | Mops/s | 36.649 | 35.650 | 29.070 | -18.5% |
| 4 KiB | Mops/s | 19.543 | 19.180 | 15.980 | -16.7% |
| 64 KiB | GiB/s | 35.246 | 27.976 | 36.497 | +30.5% |
| 4 MiB | GiB/s | 39.817 | 39.632 | 38.724 | -2.3% |

The 100-B runs are especially variable: jemalloc ranges from 41.266 to 56.605
Mops/s, and one paired comparison is slower than the system allocator. Its
median gain is not evidence of a stable per-record improvement or parity with
the old engine. The 1-KiB comparison has mixed paired signs; all three 4-KiB
jemalloc runs are slower than their system controls. These negative results
remain part of the recommendation, rather than selecting only the 64-KiB win.

In the enlarged 64-KiB workload, system and jemalloc medians are **34.852 and
37.146 GiB/s**, respectively: **+6.6%**, with jemalloc faster in all three
pairs. Timed writes last about 9.18 and 8.61 seconds. This is an enlarged burst,
not a long-lived steady-state/fragmentation test. Dataset size also changes
startup allocation history, so the smaller gain cannot be attributed to
duration alone. Do not generalize the short workload's 30.5% gain to all loads.

## Reclamation and the 8-MiB question

The preceding investigation identified an accidental glibc threshold change
caused by the old recovery buffer's allocation/free. That allocation was never
required for correct empty-device recovery. The current incremental recovery
continues allocating real scan buffers only when needed.

In separate 64-KiB traces, the timed prefill contains:

| Current allocator | `MADV_DONTNEED` calls | Sum of advised bytes |
|---|---:|---:|
| System / glibc | 2,460 | 9,831,829,504 |
| TiKV jemalloc | 16 | 2,445,312 |

See [trace-summary.json](trace-summary.json). Phase windows use the external
observation of the `PREFILL` line minus its reported elapsed time, with a small
observation delay. Counts describe traced runs, not timing-neutral samples;
byte sums are not unique pages or retained RSS.

Jemalloc substantially reduces the repeated discard pattern without an 8-MiB
warmup. It does **not** eliminate all reclamation calls, nor should that be a
requirement. Real recovery buffers still need allocation and initialization
when scanning requires them. The new allocator is not a reason to remove
necessary buffer initialization or weaken I/O ownership rules.

## Reads, memory, and threads

Read throughput medians differ by less than 0.1% between the current builds at
every size. P99 medians are equal at 100 B through 64 KiB; at 4 MiB they are
19.104 ms for system and 18.940 ms for jemalloc. These short read runs do not
certify tail behavior under allocator pressure.

Median process peak RSS, including the roughly 10-GiB configured buffer pools:

| Workload | System MiB | Jemalloc MiB |
|---|---:|---:|
| 100 B | 11,283.7 | 11,470.6 |
| 1 KiB | 11,283.7 | 11,486.9 |
| 4 KiB | 11,283.7 | 11,466.2 |
| 64 KiB, original dataset | 10,376.1 | 10,424.3 |
| 4 MiB | 10,342.5 | 10,344.8 |
| 64 KiB, enlarged dataset | 12,168.1 | 12,137.0 |

Peak RSS covers setup, prefill, verification, and reads; it is not attributed to
a single phase. [allocator.json](allocator.json) separately preserves jemalloc
allocated/active/resident/retained counters outside timed phases. Those counters
exclude the direct mmap pools. Retained virtual address space is not physical
RSS, and `after_close` is not a guarantee that all worker-owned objects have
already been destroyed.

Runtime allocator reports confirm background purging threads stayed disabled.
The io-wq workers used by asynchronous fsync remain; changing the heap allocator
does not change that I/O implementation or durability policy.

## Validation and decision

The disk driver's four functional tests pass with and without the feature.
Both standalone workspaces pass all-target clippy with warnings denied in both
configurations, and formatting/diff checks pass. The device runs additionally
exercise allocation, aligned I/O buffers, writes, verification, reads, and close
with the selected allocator.

Keep the explicit feature for further deployment-specific evaluation. This
trial supports using jemalloc without a dummy startup buffer, but does not
support switching every workload to jemalloc by default. Investigating the
1-KiB/4-KiB write losses and testing sustained mixed loads with idle intervals
remain necessary before making that broader performance claim.
