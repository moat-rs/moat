Disk cache comparison
=====================

This standalone workspace compares moat and foyer pinned to
[`dd46245c45071d1036331e4e2c48e15386017b96`](https://github.com/foyer-rs/foyer/tree/dd46245c45071d1036331e4e2c48e15386017b96).
Foyer dependencies are confined to the comparison workspace.

The executable uses the system allocator.

`moat_sync` defaults to `true`. Set it to `false` for a Moat run that skips
format and engine lifecycle syncs as well as the driver's final prefill
`sync_data`. Pending writes still complete before verification. This changes
the durability contract and must be reported explicitly in comparisons;
Foyer's synchronization is unaffected. It does not alter device cache settings.

`moat_input_pool` defaults to `true`. Each pinned Moat owner allocates and touches
a bounded pool of source values before reporting startup complete, then retains
it through the measured phases. The pool contains at most 64 values and about
4 MiB of payload per owner (one value can exceed the byte target), also capped
by the local batch and dataset size. Source storage is recycled after successful
admission because Moat has copied it into its registered I/O buffer. Every record
still fills its complete source value and stamps its identity inside timing.
Pooled sources for prepared writes use page-aligned allocations, with padding
excluded from the logical value. This stabilizes payload-copy alignment across
engine revisions even when initialization allocations differ. Smaller combined
key/value sources retain their existing representation.
Set `moat_input_pool: false` to retain the historical allocating-input control.

Foyer takes ownership of source values, so its existing owned-input mode remains
in effect regardless of this Moat option. `DRIVER.input` reports `pooled` or
`owned`; per-owner `INPUT` records report buffer counts and payload capacity.
`DRIVER.input_alignment` distinguishes page-aligned sources from natural
allocation alignment. The analyzer keeps input modes, source alignments, and
fsync policies in separate result groups; historical logs default to natural
alignment.
Compare engine revisions with matching input modes, and report this ownership
difference when comparing Moat's default pooled mode with Foyer.

The [twenty-device comparison](../../docs/experiments/engine/2026-09-18-async-lifecycle/ablation/REPORT.md)
compares the original engine and asynchronous lifecycle implementation with
matching source pools, all five value sizes, and at least ten measured seconds
per write/read phase. It includes cleanup ablations and measurement limitations.

The [earlier twenty-device v2 recheck](../../docs/experiments/cache-disk/2026-09-17-v2/REPORT.md)
records the native engine after migration, with all samples and comparison limits.

The common workload driver uses a synchronous `Backend` interface: submit
writes or reads, poll completions, drain a write batch, and close. It runs one
persistent caller thread per device. For moat, request generation, engine calls,
read validation, and buffer recycling all happen on that thread. Its phase
commands cross threads only when starting or finishing a benchmark phase.

`engine: "moat"` **does not create or enter a Tokio runtime**.
It uses registered io_uring buffers and returns pooled read buffers without
copying into new value vectors. No request tasks, oneshots, completion drivers,
or per-record channels surround the engine. The engine crate remains independent
of Tokio; this comparison executable links Tokio for foyer only.

The foyer adapter is the sole runtime boundary. It creates a shared Tokio
runtime and one async driver per disk. Requests and completions cross that
boundary in batches; read futures are polled with `FuturesUnordered`, without
an additional task per request. Calls to foyer originate on runtime workers,
so its internal tasks can use local scheduling. Synchronous callers wait for
completion batches instead of occupying CPUs in a polling loop. They run on
`runtime_cpus`, sharing those CPUs with foyer's runtime,
and its io_uring workers use `io_cpus`. Engine owner loops run on `io_cpus`.
Report actual process CPU use as well as throughput: foyer uses more software
threads and available application CPUs than the native engine path.

Both adapters use the same keys, values, device placement, concurrency
budget, and workload loop. Full-key Xxh3 identity and weighted rendezvous
placement happen **before timing**, producing one dataset per device. Random
reads choose uniformly within each device's dataset. The global `clients`
budget is divided between devices (with at most one extra request per device);
a completion admits a replacement request without waiting for the other reads.
Backpressure retains the unsubmitted request. Latency includes time waiting
for admission, and the timed phase drains every accepted request before reporting.

Prefill divides `prefill_batch` between devices and drains each local batch.
There is no cross-device barrier between batches. Value generation, encoding,
checksums, I/O, and the configured final device sync are timed. Source-value
allocation is outside timing in pooled mode and inside timing in owned mode.
Keys and routing are prepared outside timing. Small engine values use a contiguous key/value
envelope; large prepared writes copy the separately owned key and value directly
into the registered buffer. Generation and encoding are bounded to 64 records
or about 4 MiB between polls (one record can exceed the byte budget). Moat retains
rejected prepared buffers across backpressure. Every inserted key is read and
checked before timed random reads.

Keep `smaps_rollup` and `numa_maps` collection outside warmup and timed reads.
Scanning a large process can overlap the next phase and reduce throughput
without changing CPU time per read or p99 latency. Use the driver's reported
peak RSS for throughput runs; collect detailed memory maps in a separate
diagnostic. The [read-observer control](../../docs/experiments/engine/2026-09-18-async-lifecycle/ablation/REPORT.md#read-observer-diagnostic)
retains the affected samples and a subsequent read phase without the scan.

Foyer uses its disk-only HybridCache path, including serialization, XXHash64
verification, and owned value decoding. Its memory filter rejects all entries;
its adapter waits for storage flushes after each prefill batch. The pinned file
builder couples `O_DIRECT` with `O_NOATIME`, which requires device-node ownership.
The adapter enables `O_DIRECT` through the shared file descriptor before cache
initialization instead. This reserves no bytes and changes no foyer data-path code.
Engine reads use `moat_verify_reads` (default false); all writes retain checksums.
These integrity and ownership semantics differ and are reported explicitly.

`engine_segment_bytes` defaults to 2 GiB for moat; foyer retains 16-MiB blocks.
`moat_huge_pages: true` requests preferred huge pages for engine pools.
Foyer's configured pool budget covers flush buffers and excludes its separate
read allocations, so equal pool settings are not equal total-memory limits.

Historical input-copy, in-place producer, and large-frame batching diagnostics
are archived with their measured source revisions in the
[experiment record](../../docs/experiments/README.md). The current driver keeps
explicit pooled-input and owned-input modes without removing the final copy
into registered engine storage. Retired or misspelled configuration fields
are rejected instead of silently selecting a different workload.

The `CONFIG` output contains an explicit allowlist of workload parameters and
anonymous device ordinals. Hostnames, paths, serials, exclusion lists, physical
capacities, and CPU identifiers remain in the operator's private input file.
Worker counts in this summary are configured CPU budgets. The separate `DRIVER`
record reports the actual owner count, driver mode, and whether Tokio is used.

This driver replaces the historical Tokio application workload, including its
`moat` mode and `moat_batched_completions` setting. Old reports retain their
source revisions for reproduction. Native results are a separate comparison:
they also change routing scope, per-device queue depths, generation placement,
and batch barriers, so a historical-to-native speedup is not a pure engine gain.

The file smoke runner uses 16-MiB engine segments to exercise rollover. Use
`--disks 2 --verify-reads` to check multiple devices with engine CRC verification;
for 4-MiB values, pass `--records 32` to fit its 1-GiB files.

The [v2 ablation and cleanup](../../docs/experiments/engine/2026-09-16-ablation/REPORT.md)
compares individual feature removals with bracketing controls, then checks the
final implementation. It retains exact patches, repeated-write follow-ups,
the buffer-registration counterexample, and rejected optimizations.

The [twenty-device comparison after request splitting](../../docs/experiments/cache-disk/2026-09-16-split/REPORT.md)
reruns all three engines with v2's device-limit splitting enabled. It retains
45 prefills, 75 measured read phases, and phase-level io-wq observations.

The [previous native twenty-device comparison](../../docs/experiments/cache-disk/2026-09-16-native/REPORT.md)
uses the historical v1/v2 polling adapters, with 45 prefills, 75 measured read phases,
and separate CPU profiles. Its CSVs retain run identifiers, exact operation
counts and durations, CPU costs, and physical I/O totals.

The [historical application/Tokio comparison](../../docs/experiments/cache-disk/2026-09-16-20disk/REPORT.md)
retains its original source, workload, and all three implementations.

The [bottleneck investigation](../../docs/experiments/cache-disk/2026-09-16-bottlenecks/REPORT.md)
separates admission batching, envelope copying, large-frame packing, and
in-place producers with controlled twenty-device experiments. It preserves
the default comparison and labels changes in producer semantics explicitly.

The [historical matrix](../../docs/experiments/cache-disk/2026-09-10/REPORT.md) contains single-disk and
20-disk results for four key/value sizes and three matched concurrency levels.
[Methodology and limitations](../../docs/experiments/cache-disk/2026-09-10/README.md) describe the run.
[Identity costs](../../docs/experiments/cache-disk/2026-09-10/IDENTITY.md) and
[the variable-workload follow-up](../../docs/experiments/cache-disk/2026-09-10/RECHECK.md) retain the
negative cases as well as improvements.

Build and validate on Linux
---------------------------

```sh
CARGO_TARGET_DIR=target/cache-disk-compare cargo build --release --locked --manifest-path benchmarks/cache-disk/Cargo.toml
CARGO_TARGET_DIR=target/cache-disk-compare cargo clippy --locked --manifest-path benchmarks/cache-disk/Cargo.toml --all-targets -- -D warnings
python3 benchmarks/cache-disk/run_files.py --binary target/cache-disk-compare/release/moat-cache-disk-compare --seconds 1 --repeats 1
```

The runner creates new disposable 1-GiB files for each implementation and
uses the same configured I/O and application CPU sets. It rejects an existing output
directory. By default, configurations, files and logs stay in ignored `local/`.
It never selects a raw device. File-backed smoke results validate API paths;
they do not reproduce raw-NVMe performance. Hostname and paths in generated
configurations are runtime inputs and must not be committed.

Use `--key-bytes`, `--value-bytes`, `--records`, `--seconds` and `--repeats` to adjust the
file workload. Physical block-device counters are unavailable for regular
files. Choose CPU affinity explicitly for controlled measurements.

Raw-device configuration
-------------------------

The Rust binary also accepts an operator-supplied JSON configuration. Raw
targets require an exact host match, a serial allowlist in `disks`, an exact
`expected_capacity` per disk, and a nonempty `forbidden_serials` list covering
system devices. Partitions, mounted targets and devices with holders or
partitions are rejected. All listed targets must be disposable: the benchmark
formats and overwrites the configured `bytes_per_disk` window.

Provision and audit raw devices outside this repository. No machine-specific
allowlist, device names, serials, SSH automation or host inventory is shipped.
The published measurements used additional before/after array and SMART checks;
the generic harness does not replace those operator checks. Historical reports
retain their original source revisions and settings; the current adapter and
workload changes are described above.

For completed raw-device logs, `python3 benchmarks/cache-disk/analyze.py DIRECTORY`
generates numeric sample, summary and prefill CSVs. It retains every concurrency
level and slow sample and requires reads on every configured disk. Review and
sanitize outputs before publishing them. Private raw runs and profiling files
remain excluded through `.gitignore`.
