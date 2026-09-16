Disk cache comparison
=====================

This standalone workspace compares engine v1, engine v2, and foyer pinned to
[`dd46245c45071d1036331e4e2c48e15386017b96`](https://github.com/foyer-rs/foyer/tree/dd46245c45071d1036331e4e2c48e15386017b96).
Foyer dependencies are confined to the comparison workspace.

`engine: "v1"` and `engine: "v2"` use a common benchmark request adapter:
full-key hashing and disk placement, one owning I/O worker per disk, and the
same channel/completion path. Both store the full key followed by the value,
use registered io_uring buffers, and return pooled read buffers without copying
into a new value vector. No production cache policy or eviction is added to
v2. The historical `engine: "moat"` mode still benchmarks the full `moat-cache`
API; do not equate it with the new v1 adapter.

Foyer uses its normal disk-only HybridCache path, including serialization,
XXHash64 verification, and owned value decoding. The pinned file builder
couples `O_DIRECT` with `O_NOATIME`, which fails for non-owner raw-device users.
The harness opens without those flags, obtains the shared file descriptor
through a zero-length partition, and enables `O_DIRECT` with `fcntl` before
cache initialization. This reserves no bytes and changes no data-path code. Its memory admission filter
rejects all entries. Engine reads use `moat_verify_reads` (default false).
These are application-visible path measurements with different integrity and
ownership semantics, not isolated device or checksum-normalized comparisons.

`engine_segment_bytes` defaults to 2 GiB for v1/v2; foyer retains 16-MiB blocks.
`moat_huge_pages: true` requests preferred huge pages for both engine pools;
it does not change foyer's allocator. Foyer's configured pool budget covers
flush buffers; its read buffers are allocated separately. Equal configured
byte counts are not equal total-memory limits. Prefill creates values in application
workers (at most one task per configured application core per batch), admits up to `prefill_batch` requests (default 256), waits for each
batch to complete/drain, and finally synchronizes every device. Every inserted
key is read and checked before timed random reads. Write measurements include
value generation, routing, allocation, copies, checksums, and batch barriers.
They are bounded-dataset prefill throughput, not steady-state cache churn.

The file smoke runner uses 16-MiB engine segments to exercise rollover. For
4-MiB values, pass `--records 32` to stay within its 1-GiB file window.

The [current twenty-device comparison](reports/2026-09-16-20disk/REPORT.md)
compares foyer, v1, and v2 with independent write repetitions and numeric
read/CPU/I/O results.

The [historical matrix](reports/2026-09-10/REPORT.md) contains single-disk and
20-disk results for four key/value sizes and three matched concurrency levels.
[Methodology and limitations](reports/2026-09-10/README.md) describe the run.
[Identity costs](reports/2026-09-10/IDENTITY.md) and
[the variable-workload follow-up](reports/2026-09-10/RECHECK.md) retain the
negative cases as well as improvements.

Build and validate on Linux
---------------------------

```sh
CARGO_TARGET_DIR=target/cache-disk-compare cargo build --release --locked --manifest-path benchmarks/cache-disk/Cargo.toml
CARGO_TARGET_DIR=target/cache-disk-compare cargo clippy --locked --manifest-path benchmarks/cache-disk/Cargo.toml --all-targets -- -D warnings
python3 benchmarks/cache-disk/run_files.py --binary target/cache-disk-compare/release/moat-cache-disk-compare --seconds 1 --repeats 1
```

The runner creates three new disposable 1-GiB files and uses the same three
available CPUs for all implementations. It rejects an existing output
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
