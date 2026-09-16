Bottleneck experiment methodology
=================================

These are follow-up experiments on the same 20 concurrent disposable devices
as the [original matrix](../2026-09-16-20disk/README.md). Each trial formats a
fresh dataset. Device windows, CPU placement, 40 application workers, one
owning I/O worker per disk, 512-MiB engine pools, preferred huge pages,
registered I/O, 2-GiB engine segments, and 16-MiB foyer blocks are unchanged.
No kernel setting or engine implementation changes between these experiments.

Every value includes the same generated body and per-record prefix/suffix;
engines also store the same 16-byte full key. All write checksums remain
enabled. Engine reads skip CRC, and foyer retains normal verification.
Prefill timing includes value generation, routing, admission, completion,
batch barriers, rollover if reached, and final common device synchronization.
Formatting/opening and final close are excluded. Every written key is read
back and checked for full key, value length, and value endpoints.

All processes finish with a two-second read warmup and a one-second random
read phase at 640 clients. These short reads validate continued activity;
they are not used as read-performance evidence. The measured write and
validation phases run on all 20 devices. Trials run sequentially, with
engine/treatment order rotated or reversed across repetitions. Raw-device
identity checks and cooperative locks remain outside the repository.

Executables
------------

| Experiment | Source commit | SHA-256 |
|---|---|---|
| Admission batch | `a446e72d7d9b5989ba378623ecffece7fead939e` | `3e54d638c074e807124afd41a501f79143e15e7b512fdbde407c6c8f2752fc26` |
| Envelope copy / large-frame batching | `791925577d6e5d2ecd00ad4a6adf456291c02194` | `c041aadc829540afdbed62dc456c9b93103e468578bf2c1d1efe60940cf0e0a5` |
| In-place producer | `c948449939eb5c7ca43a9342ee9d343e67a6c7c5` | `eb5d9eda1c73970fa4b12572046618fbeecfa955dc52dc5048cbed6891d0031b` |

The baseline executable is preserved unchanged. Each new diagnostic has
fresh controls using its own executable; no throughput from profiling runs
is included. Foyer remains pinned to `dd46245c45071d1036331e4e2c48e15386017b96`.
The opt-in modes are confined to `benchmarks/cache-disk`, and all default to
false. Public engine APIs and their implementation are unchanged.

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=target/cache-disk-compare \
RUSTFLAGS='-C target-cpu=x86-64-v3' \
cargo build --release --locked --manifest-path benchmarks/cache-disk/Cargo.toml \
  --target x86_64-unknown-linux-gnu
```

This is a GNU/glibc build, not musl. For temporary-file functional checks,
the runner exposes `--engine-preassembled-input`, `--v2-batch-large-records`,
and `--engine-in-place-input`. In-place input is mutually exclusive with the
other modes and requires values of at least 64 KiB. Local in-place functional
checks also enabled read CRC for both engines and exercised 16-MiB rollover.
GNU release compilation, harness Clippy with warnings denied, and the
functional paths completed successfully.

Independent variables and sizes
--------------------------------

| Experiment | Logical value | Records per disk (mean) | Global prefill batch | Independent variable |
|---|---:|---:|---:|---|
| Admission batch | 1 KiB | 131,072 | 640 / 2,560 / 10,240 | Batch size only |
| Admission batch | 4 KiB | 130,561 | 640 / 2,560 / 10,240 | Batch size only |
| Envelope copy | 64 KiB | 8,190 | 2,560 | `engine_preassembled_input` |
| Envelope copy | 4 MiB | 2,048 | 620 | `engine_preassembled_input` |
| Large-frame batching | 64 KiB | 8,190 | 2,560 | `v2_batch_large_records` |
| In-place producer | 64 KiB | 65,520 | 2,560 | `engine_in_place_input` |
| In-place producer | 4 MiB | 2,048 | 620 | `engine_in_place_input` |

Each primary group has three independent processes. The batch-size-640
screening groups have two, explicitly reported in `summary.csv`. Key routing
is shared, deterministic, and approximately uniform; the records-per-disk
column is the configured mean rather than an exact count for every device.

The longer 64-KiB producer experiment increases the dataset eightfold for
both controls and treatments, including foyer, and crosses segment boundaries.
Its throughput must not be substituted into the shorter envelope-copy trial.
Four-MiB input diagnostics keep the original dataset size. An in-place
producer avoids two heap payload allocations/copies and changes the thread
that initializes bytes, including its memory locality. That is a compound
input-path experiment, not an isolated memcpy speedup or an API-equivalent
comparison with foyer's owned-vector insert.

Interpreting the artifacts
--------------------------

`samples.csv` contains every measured prefill, source revision, input mode,
batch size, operation count, CPU use, and physical I/O counters. `summary.csv`
groups only identical settings and retains median/minimum/maximum values.
CPU cores are process user-plus-system CPU seconds divided by elapsed time;
they are not a hardware utilization ceiling. Physical write amplification
divides block-device written bytes by logical value bytes, including stored
keys, padding, lifecycle writes, and any footer written during the timed fill.
The mean block write size reflects kernel splits/merges, not frame length.

All 89 trials completed 99,693,360 writes and checked every inserted key.
No I/O or sampled-content error was reported. All 20 devices were idle,
without open users, and all cooperative locks were available and released
after completion. No operating-system device was included.

These are bounded distinct-key fills, not long-duration steady-state storage
limits. Several small-value and original 64-KiB fills take less than one
second; process-to-process variation matters. The experiments identify
specific avoidable work and configuration sensitivity. They do not establish
how much every proposed production integration change will improve a general
workload, nor do they change the previous comparison's historical results.
