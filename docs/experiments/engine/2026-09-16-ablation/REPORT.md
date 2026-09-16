# V2 ablation and cleanup

The final implementation retains the optimizations that have measured or
structural value, skips the initial CQ pass on deferred rings, and removes
retired input-diagnostic branches from the active comparison driver. It does
not adopt the metadata-reuse prototype: improvements for smaller values did
not carry over to prepared writes.

The complete [methodology](README.md) describes the controls, order, checks,
limits, and build identities. The common baseline is `3464483`; the cleaned
source is `1e49809`. No ablation switches are added to the engine API.

## Implementation decisions

| Feature or candidate | Decision | Evidence and reason |
|---|---|---|
| First CQ pass | Skip only on deferred rings | Removing the pass improved the repeated 4-KiB write check by 3.25% (range +2.91% to +3.67%). Ordinary rings retain the initial pass because completions can arrive between polls and release physical submission capacity. The final combined implementation is checked separately below. |
| Metadata reuse | Reject the prototype | Repeated 100-B and 1-KiB writes improved, but the 64-KiB prepared-write follow-up fell 2.63%. Keeping the existing decoder also keeps one validation boundary for caller and persisted limits. The measured regression does not establish a compiler, cache, or scheduling cause. The full candidate patch remains archived. |
| Registered buffers | Retain as a queue option | Removing registration hurts 100-B/1-KiB reads and 4-MiB writes, but improves 4-KiB reads. The counterexample and its follow-up are retained below. `UringQueue::new` already permits ordinary I/O; no workload-specific size heuristic is added. |
| Registered files | Retain | Small read differences do not justify replacing a constant fixed-file index with descriptor plumbing on every submission. Registration is performed once. |
| Deferred task work | Retain, including the compatibility fallback | Removing it reduces small-record read throughput by about 1.4–2.0% at 640 clients. A supported-kernel experiment does not justify deleting fallback behavior. |
| Device-limit splitting | Retain | Kernel splitting reduces 4-MiB write/read throughput and increases p99 and observed io-wq activity. Logical budgets are equal; physical concurrency is not. |
| Huge-page policy | Retain in the shared pool | Fixed-buffer reads barely change in this workload, while the 4-MiB write median falls 1.60% when disabled. Actual backing differs as requested. This is a workload observation, not evidence that huge pages never help. |
| Historical producer switches | Remove from the active harness | Remove `engine_preassembled_input`, `v2_batch_large_records`, `engine_in_place_input`, their validation, the generated-value variant, and alternate adapter branches. The earlier experiments remain reproducible at their pinned source revisions. Prepared and mixed-record engine APIs remain available. |

The accepted engine change adds no locks, allocations, format changes, or new
public options. Checksums, physical splitting, completion aggregation,
publication ordering, recovery, and buffer-lifetime handling are unchanged.

## Main read ablations

At 640 clients, each cell is the median of three treatment/control throughput
ratios. A negative percentage means that removing the feature was slower.
The two candidate rows describe proposed changes rather than disabled features.
Controls bracket each size/round group; [all pairs and ranges](pairs.csv) are
retained, including unfavorable results.

| Treatment | 100 B | 1 KiB | 4 KiB | 64 KiB | 4 MiB |
|---|---:|---:|---:|---:|---:|
| `no_huge_pages` | -0.04% | -0.12% | +0.01% | -0.08% | -0.01% |
| `no_registered_buffers` | -1.69% | -1.82% | +1.55% | +0.13% | +0.08% |
| `no_registered_files` | -0.35% | -0.36% | -0.23% | +0.06% | +0.00% |
| `no_defer_taskrun` | -1.82% | -2.00% | -1.37% | -0.16% | -0.02% |
| `kernel_split` | -0.05% | -0.17% | -0.08% | -0.19% | -1.26% |
| `single_reap` | -0.06% | -0.26% | -0.06% | -0.06% | +0.00% |
| `reuse_encoded_metadata` | -0.04% | -0.02% | -0.05% | -0.16% | +0.02% |

Absolute control medians at 640 clients (six samples per size):

| Value | Unit | Write burst | Read | Read p99 (us) |
|---|---|---:|---:|---:|
| 100 B | Mops/s | 61.998 | 10.022 | 112.383 |
| 1 KiB | Mops/s | 37.969 | 9.964 | 107.359 |
| 4 KiB | Mops/s | 19.332 | 8.697 | 123.711 |
| 64 KiB | GiB/s | 34.384 | 186.706 | 556.799 |
| 4 MiB | GiB/s | 39.205 | 232.876 | 18759.679 |

These prefills are bounded bursts, not sustained device limits. Main-matrix
100-B controls range from roughly 46 to 66 Mops/s, despite stable timed reads.
This is why the shorter write samples are preserved but do not alone determine
whether a candidate is adopted. [Absolute summaries](summary.csv) and
[paired summaries](paired-summary.csv) include every write ablation as well.

## Longer write checks

Each process appends the same key set 32 times, then synchronizes. This retains
value generation and checksums while reducing short-burst variance. Index
cardinality remains fixed; this is an overwrite diagnostic, not a unique-key
fill or reclamation benchmark. The range is the three observed paired results,
not a confidence interval.

| Candidate | Value | Median change | Observed range |
|---|---|---:|---:|
| `single_reap` | 100 B | +0.59% | -0.57% to +0.60% |
| `single_reap` | 1 KiB | +0.98% | +0.29% to +1.08% |
| `single_reap` | 4 KiB | +3.25% | +2.91% to +3.67% |
| `reuse_encoded_metadata` | 100 B | +2.58% | +1.14% to +3.45% |
| `reuse_encoded_metadata` | 1 KiB | +1.41% | +1.27% to +1.56% |
| `reuse_encoded_metadata` | 4 KiB | +0.78% | +0.21% to +6.62% |
| `reuse_encoded_metadata` | 64 KiB | -2.63% | -3.39% to +0.08% |

The 64-KiB follow-up was added because the original short-write results were
negative and variable: median -2.91%, range -11.15% to +8.10%. Its longer checks
also regress in two of three pairs. The prototype is therefore left out of the
final code. Its patch includes the stricter pipeline-limit regression test;
rejecting the optimization does not change the original validation behavior.

## Buffer registration counterexample

At 640 clients, ordinary I/O is 1.55% faster for 4-KiB values, but about 1.7–1.8%
slower for 100-B and 1-KiB values. At 2,560 clients, the first sweep showed a
larger 4-KiB difference. A separate three-round follow-up reproduces it:

| 4-KiB read at 2,560 clients | Mops/s | p99 (us) | CPU us/op |
|---|---:|---:|---:|
| Registered buffers | 9.994 | 323.967 | 2.001 |
| Ordinary I/O | 12.209 | 302.079 | 1.638 |

The median paired throughput gain is 22.11%, with a +22.07% to +22.49% range;
p99 is about 7% lower. The same follow-up's write burst falls 2.63%. Main-matrix
4-MiB writes fall 4.85% without registration. Registration is therefore a
workload tradeoff, not a universal speedup.

This treatment also changes pool prefaulting: sampled anonymous huge pages are
2,640 MiB for the unregistered small-record cases, 2,800 MiB at 64 KiB, and
7,680 MiB at 4 MiB, versus 10,240 MiB in the registered preferred-huge-page
controls. The allocation budget and policy are identical, but committed memory
is not. These results do not isolate fixed-opcode overhead from pinning,
prefaulting, or memory-backing effects. A size-dependent dispatch rule is not
justified from one device/kernel configuration.

## Request splitting, huge pages, and worker observations

For 4-MiB values, leaving splitting to the kernel changes write throughput by
-3.05%, read throughput by -1.26%, and read p99 by +12.10% in the three 640-client
pairs. The single 2,560-client sweep has a -3.08% read difference and +16.14% p99.
It is one endpoint sample, not a repeated estimate.

The kernel-split treatment reaches 5,120 observed io-wq workers; the peak number
simultaneously runnable is 117. These peaks occur in the high-concurrency read
phase. Sleeping helpers can remain from earlier work. Registered-file removal
also has a small sampled helper count in one case. The main-matrix default split controls
have no observed helpers. Phase counts and runnable counts are kept separately
in [runtime-phases.csv](runtime-phases.csv). The cleaned final samples have
no observed helpers; one final control retains one sleeping helper, with no
runnable helper observed in its sampled phases.

The huge-page toggle is effective: all disabled cases sample zero anonymous huge
pages, versus 10,240 MiB for the corresponding controls. Its 640-client read
changes are between -0.12% and +0.01%. Recycled, registered buffers in this
particular workload do not show a large read benefit. Both settings keep the
same 128-KiB physical request cap and avoid observed read offload in these
samples. Huge pages do not remove a device byte limit.

## Final combined implementation

The cleaned build contains the conditional early-CQ removal, the single owned
input path, and public configuration output. It retains the baseline metadata
decoder. Compare it with fresh original controls; do not add isolated candidate
percentages to predict the combined result.

| Value | Unit | Baseline write | Cleaned write | Paired write change | Baseline read | Cleaned read | Paired read change |
|---|---|---:|---:|---:|---:|---:|---:|
| 100 B | Mops/s | 59.826 | 59.471 | +0.08% | 10.020 | 10.021 | +0.00% |
| 1 KiB | Mops/s | 37.118 | 39.161 | +2.27% | 9.967 | 9.960 | -0.07% |
| 4 KiB | Mops/s | 19.281 | 19.489 | +0.71% | 8.694 | 8.698 | -0.02% |
| 64 KiB | GiB/s | 35.289 | 35.515 | -1.41% | 187.040 | 187.005 | +0.03% |
| 4 MiB | GiB/s | 39.159 | 39.480 | +1.20% | 232.889 | 232.851 | -0.02% |

Reads use 640 clients with three cleaned samples and six controls per size.
Paired ratios are computed within rounds and need not equal ratios of the pooled
medians. The unfavorable 64-KiB write result is retained: -1.41% median, with an
observed -7.49% to -0.98% range. It does not support a general write-speedup claim.
Writes retain the original burst boundary and its variability; the longer
individual-candidate results above are not measurements of the final binary.
All endpoint sweeps, p99, CPU, physical I/O, and per-process observations remain
in the CSVs. The final build does not establish a new foyer/v1/v2 ranking;
[the previous three-engine comparison](../../cache-disk/2026-09-16-split/REPORT.md)
retains its original source and results.

## Validation and preservation

The main matrix has 135 prefills and 225 read phases. The repeated-write checks
add 36 processes; the prepared-write and buffer-read follow-ups add nine each.
The final comparison adds 45 prefills and 75 reads. Across all five suites,
234 processes and 354 read phases are retained, including 45 one-second sanity
reads. Every process verifies every latest key before timed reads. Read-byte
accounting must match every completed aligned extent, and every read phase
must access all 20 devices. No sample is dropped because it is slow.

The final workspace passes 288 tests/doctests; the standalone harness passes
four tests. Clippy with warnings denied, formatting, documentation generation,
Python syntax, archive hashes, relative links, and whitespace checks pass.
Release builds use GNU/glibc, with CRC-enabled disposable-file checks before
raw-device measurements. A post-run check confirms that all 20 devices are idle, no device users remain,
and the exclusive locks are released. A no-I/O setup probe accepts the
single-issuer/deferred flags used by the queue. Functional tests cover split depths, short/error
completion, buffer identity, drain-on-drop, recovery, publication, and rollover.
These checks do not simulate hardware power loss.

[The archive index](../../README.md) retains all prior optimization experiments,
including slower results and differences in workload/API boundaries. The 44
historical CSVs are byte-identical; 19 Markdown reports have only link relocation
and deployment-inventory redaction. Public data excludes hostnames, accounts,
paths, device identities, CPU identifiers, private allowlists, raw logs, and
remote-control scripts. Exact source revisions, diagnostic patches, binary
hashes, public workload parameters, and derived analysis remain available.
