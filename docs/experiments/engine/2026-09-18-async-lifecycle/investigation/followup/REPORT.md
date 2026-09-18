# Allocator root cause and initial performance fixes

This follow-up identifies the allocation change behind the 64-KiB regression
and applies two independently measured engine improvements. It does **not**
claim performance parity for every workload. The preceding
[investigation](../REPORT.md) remains a record of the earlier diagnostic stage.

## Coverage

All 84 processes used all twenty assigned data devices, the same bounded 64-GiB
window per device, pinned owners, queue depth, input batching, pool configuration,
and 2-GiB segments as the parent experiment. There were 81 uninstrumented runs
and three syscall traces. All 137,901,300 prefilled records passed the existing
identity/size/sampled-content verification. Each process also completed a read
phase with activity on all twenty devices. Read timing here is validation, not
a new read-performance comparison.

Each uninstrumented comparison has three repetitions with rotated or alternating
variant order. Raw measurements and ranges are in [samples.csv](samples.csv)
and [summary.csv](summary.csv). Binary and modified-source hashes are in
[build.json](build.json). Raw host/device identities, inputs, and traces stay
private. No global allocator or kernel settings were changed. The final check
found all twenty devices idle with no open users; controller and system-mirror
checks passed. SMART was unavailable to the benchmark account.

## Sync ordering

An asynchronous write followed by a synchronous `sync_data` is valid when the
caller has already observed successful completion of the relevant writes.
Submission order alone is insufficient: a write can still be pending when a
synchronous barrier executes. Using an asynchronous fsync does not by itself
order independent operations either. The
[liburing fsync documentation](https://raw.githubusercontent.com/axboe/liburing/master/man/io_uring_prep_fsync.3)
explicitly discusses ordering and `IOSQE_IO_DRAIN`.

The old lifecycle checked that the pipeline was idle before synchronous sealing.
The current lifecycle drains pending writes before sealing, and each control
write completes before the next control operation. `Pipeline::start_flush`
does not submit sync while the write queue is nonempty; failed writes fail the
flush. The earlier synchronous-fsync diagnostic changed only how `Sync` was
executed and preserved this ordering. It did not omit barriers.

A deterministic test now holds write completions while allowing other queue
operations to execute. Repeated polls neither submit nor complete a sync until
the writes are released; successful flush then captures the written bytes in
the test device's durable image. This checks engine ordering, not physical
power-loss behavior.

## Exact origin of `MADV_DONTNEED`

The old `recover()` unconditionally created an aligned, zeroed buffer of
`max_frame_len` before examining segments. In this benchmark that is **8 MiB**,
even when the newly formatted device has no frames to scan. The new incremental
recovery allocates frame storage on demand and no longer performs that unused
allocation/free on empty-device startup.

On the test host's glibc 2.39, freeing a sufficiently large mmap-backed chunk
can raise the dynamic mmap threshold to that chunk size and the trim threshold
to twice that size. Thus the old recovery's temporary allocation can leave the
allocator with a trim threshold around 16 MiB. Without it, subsequent bursts of
temporary benchmark input values can trigger arena trimming. This threshold
explanation comes from the matching
[glibc malloc implementation](https://raw.githubusercontent.com/bminor/glibc/glibc-2.39/malloc/malloc.c);
the internal threshold variables were not directly sampled.

The trace identifies the actual caller chain: `VecDeque::Drain<Put>::drop`
releases temporary input values, `__libc_free` enters allocator internals, and
`__madvise` issues `MADV_DONTNEED`. Intermediate hidden libc symbols are not
assigned names from strace's nearest-exported-symbol labels. The matching
[glibc arena implementation](https://raw.githubusercontent.com/bminor/glibc/glibc-2.39/malloc/arena.c)
uses `MADV_DONTNEED` while shrinking releasable arena pages. These are no longer
live value buffers; this is allocator reclamation, not discarding pending I/O
buffers. Repeatedly allocating and touching replacement pages adds work.

Two opposite source ablations establish the trigger:

- `prime`: unchanged candidate plus one 8-MiB aligned allocation/free in setup,
  before the timed prefill. Recovery and engine I/O behavior are unchanged.
- `lazy`: old baseline with the recovery buffer allocation moved to the branch
  that actually scans a frame. Fresh empty-device startup skips the allocation.

| Variant | Median 64-KiB write GiB/s | Prefill discard calls | Discard bytes |
|---|---:|---:|---:|
| Baseline | 34.731 | 0 in preceding investigation | 0 |
| Candidate | 30.035 | 2,463 | 9,831,985,152 |
| Candidate + setup allocation (`prime`) | 35.453 | 0 | 0 |
| Baseline with lazy allocation (`lazy`) | 30.066 | 2,449 | 9,759,010,816 |

Throughput uses uninstrumented runs; discard counts use separate traces and
must not be interpreted as timing-neutral measurements. Discard bytes sum
syscall lengths, not unique memory. The candidate trace includes call stacks;
its large tracing overhead is excluded from throughput comparisons. Phase
boundaries use externally observed stdout timing, as in the preceding report.
See [trace-summary.json](trace-summary.json).

Removing the allocation makes the old baseline slower; adding it makes the
candidate faster and eliminates the discards. The async recovery change exposed
an allocator-history dependency in the input path. This is a real end-to-end
regression, even though it is not a loss of device bandwidth. An artificial
startup allocation or global malloc tuning is not an engine fix and was not
added to production code. A reusable input-buffer path would address this
allocation pattern explicitly and should be measured alongside the current
allocation-heavy benchmark.

## Engine changes applied

1. Write completion and buffer return no longer charge payload size against the
   poll byte budget: those operations do not scan the payload. Operation and
   record budgets remain active. Read processing retains byte accounting.
2. Index publication uses one `Entry` lookup. Admission uses a conservative
   entry-count upper bound away from the hard limit, retaining exact key lookup
   and deduplication near the limit. Cursor keys, version/tombstone semantics,
   resource bounds, and backpressure remain covered by tests.

The `combined` binary contains exactly these two behavioral changes against the
frozen candidate. Production source additionally has explanatory comments and
regression tests. The earlier isolated budget variant recovered the 4-MiB
median to within 0.2% of baseline; this new combined matrix reports:

| Value size | Baseline | Combined | Change |
|---|---:|---:|---:|
| 100 B, Mops/s | 59.670 | 43.832 | -26.5% |
| 1 KiB, Mops/s | 33.723 | 35.391 | +4.9% |
| 4 KiB, Mops/s | 18.346 | 18.980 | +3.5% |
| 64 KiB, GiB/s | 35.156 | 28.708 | -18.3% |
| 4 MiB, GiB/s | 39.581 | 39.060 | -1.3% |

These are ratios of medians of short prefills, not statistical equivalence
tests. For example, baseline 1-KiB results range from 30.680 to 38.987 Mops/s.
The 64-KiB gap remains without changing allocator history. `combined-prime`
uses the same diagnostic setup allocation and reaches 36.241 GiB/s against
35.156 GiB/s baseline, supporting the allocation explanation after both fixes.

## Remaining small-value attribution

At 100 B, `combined-prime` improves the median from 43.832 to 54.452 Mops/s,
still below baseline's 59.670. An additional diagnostic pair performs initial
segment allocation before timing for both versions. `combined-warm` includes
both this warm allocation and `prime`; `baseline-warm` includes warm allocation
and naturally retains the old recovery allocation. Their 100-B medians are
58.671 and 59.952 Mops/s, a 2.1% difference, but individual pairs have mixed
signs and wide variation. This changes the measured startup boundary; it is
not proof that the original end-to-end regression is fixed.

A separate `retry` variant skips reconstructing a frame while the engine has
no active segment and an outstanding operation. Initial allocation remains
inside timing. Against `combined`, 100-B median throughput falls from 45.514
to 42.571 Mops/s, while 1-KiB improves from 33.815 to 35.633. This guard does
not explain or fix the 100-B regression and was not applied to production.

The earlier larger 100-B dataset reduced the gap substantially, and these
controls confirm sensitivity to setup and allocation history. Remaining startup
scheduling and allocation costs have not been fully separated. Do not claim
all-size parity or blame fsync alone based on these results.

## Validation

The two engine fixes passed 214 functional tests across engine, server,
cache-store, and cache, plus workspace all-target clippy with warnings denied.
The subsequently added sync-ordering test passed with all 17 lifecycle tests;
engine all-target clippy and formatting passed again. No on-disk format,
durability option, or fsync policy changed in these performance fixes.
