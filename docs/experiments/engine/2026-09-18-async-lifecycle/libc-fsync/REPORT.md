# System allocator and optional device fsync

The jemalloc experiment has been removed from the active executables at the
user's request. Both current benchmark drivers use the system allocator.
The engine now supports an explicit initialization-time policy that disables
device fsync throughout its lifetime, including explicit `flush`. This policy
does not change the asynchronous ticket/poll interface.

On this twenty-device setup, disabling fsync removes the observed io-wq helpers
and recovers the short 100-B write result. It does **not** resolve the original
owned-input 64-KiB regression. Separate timing and input-reuse controls locate
most of that difference outside the engine adapter's `put`/`poll` calls; the
remaining difference is retained below rather than claiming complete parity.

The subsequent [64-KiB root-cause controls](../64k-root-cause/REPORT.md) directly
fix the trim threshold in both directions and measure input construction/free
time, page faults, and discard syscalls. They isolate the allocator-policy cause
without replacing the allocator or changing the engine binaries.

## Implementation and semantics

- `engine::Options::sync_mode` controls runtime allocation, sealing, rollover,
  explicit flush, and close. `SyncMode::Enabled` is the default.
- `engine::FormatOptions::sync_mode` controls the separate offline format call.
  Callers that disable fsync for both phases must set both options. The setting
  is not stored on disk, and opening does not infer a policy from hardware.
- `SyncMode::Disabled` skips device `sync()` and queue `Operation::Sync` requests.
  It preserves write completion ordering, lifecycle stages, and error propagation.
  Explicit flush remains a write-completion fence, with no added durability
  guarantee beyond the backing device's own completion contract.
- Storage-session options forward the same runtime policy. Online operations
  still use tickets and polling; named blocking helpers and offline formatting
  remain blocking operations.
- The disk driver accepts `moat_sync: false`, applying it to format, open, and
  the driver's final prefill barrier. Its default is true. The engine driver
  accepts `--sync false`; this is the device-fsync policy, not an API mode.

The persistent layout is unchanged. No dummy recovery allocation, global
allocator tuning, kernel setting, or device setting was introduced. Historical
[jemalloc measurements](../jemalloc/REPORT.md) remain archived, but their optional
feature and dependencies are no longer part of the active build.

## Method and evidence

All **65 processes used all twenty authorized devices**, within the existing
64-GiB-per-device window. The main comparison contains 45 processes: five value
sizes, three variants, three repetitions, with rotated ordering. Variants are
the frozen original baseline, current fsync enabled, and the same current
binary with fsync disabled. All use glibc with allocator environment overrides
and preload libraries cleared.

Configuration matches the preceding comparison: 16-byte keys, 2-GiB segments,
512-MiB registered buffer pools per device, queue depth 256, twenty owner
threads, and prefill batch 2560 globally. Records per device are
`max(2048, min(131072, 512 MiB / (16 + value_bytes)))`. Main random reads use
640 global clients, two seconds of warmup and two seconds of measurement.
Small-value write bursts last only tens of milliseconds; these are not
steady-state throughput or per-record durable-latency measurements.

Twelve additional runs compare retained/reused caller input buffers, six collect
per-owner timing, and two trace fsync syscalls. Timing and syscall instrumentation
are excluded from throughput medians. All **102,839,780 prefilled records** passed
the existing identity, size, and sampled-content verification. This is not
exhaustive byte verification or power-loss testing. Every process recorded write
and read activity on all twenty devices.

[samples.csv](samples.csv) retains every run;
[summary.csv](summary.csv) contains medians and ranges;
[build.json](build.json) identifies the binaries and main source archive.
The current disk binary was built before subsequent documentation and separate
engine-driver CLI edits; those do not affect its measured behavior. Raw configs,
identities, paths, traces, source snapshots, and operational scripts remain
private. Final checks found all twenty devices idle with no open users;
controller-state and system-mirror checks passed. SMART was unavailable to the
benchmark account.

## Main write results

All entries are three-run medians from this matrix. The baseline retains its
original fsync behavior; disabling fsync is an explicitly different durability
policy and cannot be generalized to devices with volatile write caches.

| Value | Unit | Original baseline | Current fsync enabled | Current fsync disabled |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 58.720 | 48.720 | 68.914 |
| 1 KiB | Mops/s | 37.480 | 36.975 | 40.157 |
| 4 KiB | Mops/s | 18.864 | 19.894 | 20.301 |
| 64 KiB | GiB/s | 37.099 | 28.531 | 28.600 |
| 4 MiB | GiB/s | 39.176 | 39.252 | 39.055 |

At 100 B, disabling fsync improves the current median by **41.5%** and exceeds
the original median by 17.4%. At 64 KiB it changes throughput by only **0.24%**:
the 22.9% gap to the original remains. The prior write-budget fix continues to
keep 4-MiB results within 0.4% of the original. Random-read median throughput
changes are all within 0.2% of the original in this matrix. Full ranges and
latency measurements are in the CSVs; short bursts and three repetitions do
not establish universal performance guarantees.

## Fsync workers and first-write latency

Across the uninstrumented main runs, sampled task maxima were 21 for the original,
41 for current fsync enabled, and 21 for current fsync disabled. The additional
twenty tasks were named io-wq workers. Sampling does not measure their complete
lifetime or CPU usage. Linux 6.8's
[io_uring fsync implementation](https://github.com/torvalds/linux/blob/v6.8/io_uring/sync.c)
forces fsync into an asynchronous worker context; it does not make the underlying
fsync implementation nonblocking.

Separate per-call clock instrumentation produced these medians across twenty
owners in one diagnostic run per variant. Values are milliseconds. First accept
includes input preparation, framing, and initial segment allocation; it is not
a pure device-fsync latency. `Other` is each owner's elapsed time minus its
measured adapter `put` and `poll` calls. Column medians need not sum exactly.

| Value | Variant | First accept | Writer elapsed | In put | In poll | Other |
|---|---|---:|---:|---:|---:|---:|
| 100 B | Original | 0.104 | 37.816 | 10.895 | 15.890 | 10.915 |
| 100 B | Current enabled | 3.218 | 41.222 | 12.367 | 17.365 | 11.543 |
| 100 B | Current disabled | 0.127 | 38.178 | 10.027 | 16.956 | 11.218 |
| 64 KiB | Original | 2.054 | 278.826 | 153.515 | 58.721 | 65.656 |
| 64 KiB | Current enabled | 3.333 | 340.323 | 92.161 | 54.843 | 190.145 |
| 64 KiB | Current disabled | 1.951 | 331.022 | 90.670 | 53.093 | 183.882 |

[writer-timings.csv](writer-timings.csv) retains every owner. The 100-B first
acceptance delay drops when fsync is disabled, consistent with initial lifecycle
fsync scheduling. This does not decompose every part of the larger uninstrumented
end-to-end difference, which also includes final synchronization and coordination.

The separate syscall trace saw 100 `fdatasync` calls when enabled (four format
barriers plus one final prefill barrier per device) and zero when disabled.
Neither trace saw the ordinary `fsync` syscall.
[trace-summary.json](trace-summary.json) deliberately does **not** count io_uring
FSYNC SQEs: strace's syscall filter cannot establish their absence. Backend tests
that reject every sync request, together with the policy branches and worker
observations, cover that separate path.

## Remaining 64-KiB difference

In the timing sample, current 64-KiB adapter `put` and `poll` calls are faster in
aggregate, while time outside those calls rises from about 66 ms to 184–190 ms.
That interval includes input generation/destruction, harness bookkeeping, clock
overhead, and scheduling; it is not an allocation-only measurement. It shows why
charging the complete end-to-end loss to engine I/O or fsync is unsupported.

A separate benchmark-only control retains up to 64 drained `Put` objects per
owner and resets their contents for reuse. Each record still fills its complete
value buffer, copies its key, stamps identity, constructs the frame, performs
the normal final copy/checksums, and writes through the unchanged engine. Both
variants retain fsync and glibc; no 8-MiB priming allocation is added. This changes
input allocation lifetime, so these results are not substituted into the main
owned-input comparison.

| Value | Unit | Original with input reuse | Current with input reuse | Current / original |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 64.904 | 46.138 | -28.9% |
| 64 KiB | GiB/s | 35.708 | 33.903 | -5.1% |

Input reuse shrinks the 64-KiB median gap from about 23% to 5%; individual paired
changes have mixed signs. It does not recover 100-B performance while fsync is
enabled. Together with the preceding
[allocator-threshold and caller-free trace evidence](../investigation/REPORT.md),
this supports input allocation lifetime as a major contributor to the 64-KiB
result without relying on switching allocators. It does not prove that every
remaining percent is allocator overhead or that all regressions are fixed.
The control remains diagnostic; production does not reintroduce an otherwise
unused recovery buffer or silently change caller ownership semantics.

## Device guarantees and DIO

Read-only inspection after the matrix found all twenty queues reporting
`write through`; all twenty NVMe Identify Controller results had `vwc = 6`, with
the volatile-write-cache-present bit clear. Anonymous observations are retained
in [device-cache.json](device-cache.json). No cache settings were changed.

Linux's [writeback-cache documentation](https://docs.kernel.org/block/writeback_cache_control.html)
states that an empty flush completes before entering the device driver when
the device does not advertise a volatile write cache. This explains why the
hardware flush can be a no-op on this setup while its fsync/worker path still
costs time. These reported properties are not an independent power-loss test.

`O_DIRECT` itself does not guarantee persistence; see
[open(2)](https://man7.org/linux/man-pages/man2/open.2.html). On an exclusive raw
device whose completed writes are already persistent, explicit device flush can
be unnecessary. A PLP marketing label alone does not establish that contract for
every layer, and filesystem-backed direct I/O may still require metadata
synchronization. Disabled mode therefore remains an explicit caller policy.

## Validation

Workspace test logs record 217 passing unit, integration, and documentation
tests; the disk driver records four more. Tests cover disabled sync across format,
explicit flush, seal, rollover, close, and reopen, plus delayed writes and write
failure propagation. Existing enabled-flush ordering tests remain green. Workspace
and both standalone drivers passed all-target Clippy with warnings denied.
