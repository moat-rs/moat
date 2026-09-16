# V2 ablation methodology

This experiment changes one implementation feature at a time in v2. It is
separate from the historical foyer/v1/v2 rankings. The objective is to identify
useful optimizations and redundant execution without weakening format,
recovery, publication, or buffer-lifetime guarantees.

## Source and builds

The common source is `3464483d3547286ba35669c56693d59f69fdc75d`. Each variant is
built from an isolated checkout plus its exact [patch](patches/). No ablation
switch is added to the production API. [Build identities](builds.csv) retain
the base revision, patch SHA-256, executable SHA-256, target, and flags.

All binaries use Rust 1.98.0, release mode, `x86_64-unknown-linux-gnu`, and
`RUSTFLAGS='-C target-cpu=x86-64-v3'`. They use GNU/glibc. Each executable passes
a two-device disposable-file check with read CRC enabled before raw-device
measurement. The longer-write variants use smaller records for that check
so repeated appends fit the disposable extent.

| Variant | Change from the common source |
|---|---|
| `control_before`, `control_after` | The unmodified v2 executable, bracketing each treatment group. |
| `no_huge_pages` | Same executable and pool budget; set the existing pool policy to disabled. Actual backing is sampled. |
| `no_registered_buffers` | Keep the pool and its huge-page policy, but use the queue constructor without buffer registration. File registration and request splitting remain enabled. |
| `no_registered_files` | Keep fixed buffers; remove file registration and submit the owned descriptor directly. |
| `no_defer_taskrun` | Keep single-issuer setup but omit deferred task work; use the existing ordinary submission path. |
| `kernel_split` | Retain the current queue and completion state machine but lift the SQE byte cap, leaving oversized bio splitting to the kernel. |
| `single_reap` | Remove the CQ pass before staging, retaining the pass after entering the kernel. This tests whether the first pass is redundant under queue pressure. |
| `reuse_encoded_metadata` | Reuse the checked encoder's header and metadata instead of decoding its fresh output. Actual frame/value limits remain checked. All write CRCs are generated; disk reads and recovery retain their decoders. |

A disabled optimization is not automatically a simplification. For example,
using ordinary file descriptors requires plumbing a descriptor into each
submission, while the registered-file path uses a constant index. Shared
pool capabilities also serve users outside v2. Decisions consider code and
API responsibility as well as measured differences.

## Main matrix

All processes drive 20 devices concurrently. Each uses the same permitted
64-GiB extent per device, 2-GiB engine segment slots, depth-256 queues,
512-MiB pools, and 8-MiB maximum buffer class. Device-local I/O workers own
the engine, queue, index, and workload. No Tokio runtime is constructed.
The logical read concurrency is divided evenly across the 20 owners.

| Value | Mean records/device | Total records | Global prefill budget |
|---|---:|---:|---:|
| 100 B | 131,072 | 2,621,440 | 2,560 |
| 1 KiB | 131,072 | 2,621,440 | 2,560 |
| 4 KiB | 130,561 | 2,611,220 | 2,560 |
| 64 KiB | 8,190 | 163,800 | 2,560 |
| 4 MiB | 2,048 | 40,960 | 620 |

For each size and each of three rounds, run a fresh control, the seven
treatments in a shuffled order, then another fresh control. Order is fixed
before execution using seed `20260916 + round * 100000 + value_bytes` for
each treatment shuffle. No completed samples are excluded by their outcome.
This produces 135 independently initialized processes and prefills.

Round one measures 160, 640, and 2,560 clients. Rounds two and three measure
640 clients. Each level has two seconds of warmup and a five-second timed
read phase, including drain time and accepted completions. There are 225
measured read phases. Each treatment has three samples at 640 clients and
one at either sweep endpoint; controls have twice those counts. Endpoint
observations alone do not establish repeatability.

Keys are deterministic 16-byte full keys. Identity and routing happen before
timing. Prefills include owned-value generation, copying, checksums,
admission, completion draining, and final device synchronization. Every key
is read back before timing reads. Read checks compare full keys, value lengths,
and stamped endpoints. Timed engine reads skip CRC, as in the native reports.
All write CRCs remain enabled in every variant.

The adapter packs at most 64 queued small values into a borrowed frame. An
engine value includes the full-key envelope; envelopes of at least 64 KiB use
one prepared frame each. The default comparison generates owned input and
copies it into the final I/O buffer. Format bounds remain 8 MiB per frame and
4 MiB plus 4 KiB per value. These are the same paths and bounds as the common
source; retired producer diagnostics are not enabled in this matrix.

Throughput ratios compare each treatment with the geometric mean of its two
surrounding controls in the same size/round/phase/concurrency group. Summaries
retain the three paired ratios and their observed range, alongside absolute
rates. These are observations, not confidence intervals. Raw samples preserve
both controls, which expose drift and short-prefill variance.

The `kernel_split` variant can expose more physical device requests than the
same number of userspace SQEs. All variants retain the same logical budget;
this does not imply matched physical concurrency for oversized requests.

## Repeated-write check

The main small-record prefills are too short to justify deleting code based
on a small percentage change alone. A separate bounded check applies
[`repeat_prefill.patch`](patches/repeat_prefill.patch) to the control and to
each of `single_reap` and `reuse_encoded_metadata`.

It appends the same key set 32 times with increasing LSNs and syncs after all
passes. Values are generated and checksummed on every pass. Index cardinality
stays fixed while data and segment metadata continue growing. It therefore
measures repeated overwrites, not unique-key insertion or reclamation.
No segment is reused and the writes remain within the permitted extent.

The values are 100 B, 1 KiB, and 4 KiB. Each size has three rounds with two
bracketing controls and the two treatments, producing 36 processes. Each
process verifies the latest value for every key, then runs a two-second read
warmup and one one-second activity check at 640 clients. Those reads are
sanity checks, not replacements for the main read matrix. The combined
`long_*` patch identities in `builds.csv` represent the repeat patch plus the
named treatment, or just the repeat patch for `long_baseline`.

Two targeted follow-ups investigate unfavorable or unexpected main-matrix
observations. They are retained separately rather than replacing those samples:

- `prepared_write`: three rounds of before-control, metadata reuse, and
  after-control at 64 KiB, using the same 32-pass write binaries. This adds
  nine processes and checks the negative short-prefill observations. Each
  process writes 5,241,600 values and checks all latest keys before its one-second
  sanity read. These appends fit within the existing 64-GiB extent per device.
- `buffer_reads`: three rounds of before-control, ordinary buffers, and
  after-control at 4 KiB and 2,560 clients. This adds nine processes and repeats
  the single sweep observation where ordinary reads were faster than fixed-buffer
  reads. It uses the original five-second read timing and unchanged main binaries.

## Final implementation check

The `final` suite compares the cleaned implementation with the original
baseline, using the main workloads and the same three-round concurrency plan.
Each group has a before-control, cleaned implementation, and after-control:
45 processes, 45 prefills, and 75 read phases. This checks the combined engine
and driver changes. It is separate from the one-feature treatments. Its source
and executable identities are included in `builds.csv`.

The cleaned source is `1e49809d7d22cc060e058c64626c06eef654a6e7`. It skips the
initial CQ pass only when deferred task work is enabled; the ordinary-ring
path retains its initial pass to recover capacity from asynchronous completions.
The exact engine change is also retained as
[`deferred_single_reap.patch`](patches/deferred_single_reap.patch).
The unconditional `single_reap` treatment and the conditional final change
are distinct. The metadata-reuse candidate is not adopted: its prepared-write
follow-up regressed, despite improvements for smaller values.

## Observations and privacy

Raw-device execution uses private identity/capacity allowlists, system-device
exclusions, mount/partition/holder/signature/open-user checks, and exclusive
cooperative locks. The same approved extents and workload affinity are used
throughout. No global kernel tuning, discard, or full-device preconditioning
is performed. Workload parameters and required I/O behavior are public;
operational identifiers and deployment inventory are not.

An independent observer samples process tasks every 250 ms. It retains total
and runnable io-wq counts separately, and samples actual anonymous huge pages
after verification. Phase boundaries come from existing log markers; read
observations include warmup and drain. A retained sleeping helper is not
evidence of offload in that phase, and zero sampled helpers is not a trace-level
guarantee. Observer CPU is outside the benchmark's process CPU measurement.

Removing buffer registration also changes prefaulting and committed pool
memory. The pool budget and huge-page policy remain equal, but sampled anonymous
huge-page backing is smaller in the unregistered cases. That treatment measures
the combined effect of removing registration, not an isolated opcode or TLB
cost. Actual backing and RSS are retained for each process in `runtime.csv`.

All measured read phases must access all 20 devices. Physical byte counts
must match the expected aligned extent for every completed native read phase.
Write counts, key verification counts, exits, build identities, and pool
backing are checked before producing public CSVs. Raw configurations, logs,
process identifiers, command lines, serials, paths, CPU identifiers, and
remote-control scripts are kept outside this archive.

## Data files

- `matrix.csv`: execution order within each suite and public workload parameters.
- `prefill.csv` and `samples.csv`: every completed write/read sample, including
  controls, physical I/O, CPU cost, latency, and memory observations.
- `runtime.csv` and `runtime-phases.csv`: sampled process and io-wq counts,
  runnable counts, actual pool backing, and observer cost.
- `summary.csv`: absolute medians and rate ranges, grouped by suite, variant,
  value size, phase, and concurrency. Controls are grouped as `baseline`.
- `pairs.csv` and `paired-summary.csv`: individual control-normalized comparisons,
  their medians, and observed ranges. Throughput ratios above one are faster;
  p99 and CPU-time ratios above one are more expensive.

Run `python3 analyze.py` in this directory to regenerate the three derived
summary/pair files from the public samples. Only write/read samples are input;
the script needs no private configuration, logs, or device access. Read results
from `long_write` and `prepared_write` are sanity checks, not headline throughput.

## Reproducing a variant

Use an isolated checkout at the common source, apply one patch from `patches/`,
and build the comparison harness with the target and flags above. Huge-page
ablation uses the baseline executable with `moat_huge_pages` set to false.
For a longer-write variant, apply the repeat patch as well. The active
[harness guide](../../../../benchmarks/cache-disk/README.md) explains private
operator configuration; historical schemas belong to their pinned revision.
All source patches are diagnostic artifacts, not production feature toggles.
