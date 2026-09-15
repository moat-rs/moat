# Single-NVMe engine pipeline measurements

This report measures reads with verification enabled. The later
[direct-read follow-up](../2026-09-15-direct/REPORT.md) remeasures both engines
with verification disabled and includes partial-range reads.

The unified pipeline improves small-record burst writes and verified 4-KiB reads, but its
current read implementation regresses for 100-B, 1-KiB, 64-KiB and mixed records.
At concurrency 64, both engines reach approximately the same bandwidth for
4-MiB reads. These measurements support retaining the format while addressing
read scheduling and metadata validation before replacing the existing engine.

## Scope and reproducibility

- Date: 2026-09-15. One enterprise NVMe namespace on an AMD EPYC x86-64 host;
  one NUMA-local CPU, one application thread. The fio, comparison and profiling
  phases ran sequentially.
- Linux 6.8.0, glibc 2.39; Rust 1.98.0 release, GNU target
  `x86_64-unknown-linux-gnu`, `-C target-cpu=x86-64-v3`. No musl build.
- Engine implementation: `5d54bf135066c1e03a9c79d88125106e3623e365`.
  The standalone harness is documented in [the methodology](../../README.md).
- Measured executable SHA-256:
  `7ebe0089bde8852ec2cd369b980abef90f1ce506ce50737634ef4b4a79b20abe`.
- A bounded 4-GiB device view, with the same 2-GiB data segment at offset 2 GiB.
  Every run formats a fresh identity. Raw-device access was unprivileged.
- Direct io_uring, queue depth 64. Logical read concurrency 1 and 64, plus 16
  for 4-MiB values. Two seconds of warmup and ten measured seconds per read phase.
- At most 512 MiB of payload per write pass, rounded to whole groups. Groups
  contain 64 small/mixed records or 16 uniform large records. Mixed values cycle
  through 100 B, 4 KiB, 64 KiB and 300 B at equal record counts.
- Both engines compute write CRCs inside the timer, verify read checksums, and
  finish each write pass with a durable flush. No seal/footer write is timed.
- Three fresh repetitions, alternating engine order. All tables show medians;
  latency columns are medians of sampled run percentiles, not pooled percentiles.
  [All 114 samples](samples.csv) and [38 configuration summaries](summary.csv)
  retain variation and physical device counters.
- The measured phases completed 36,378,240 record writes and 108,970,843 verified
  reads without observed I/O, checksum or returned-content errors. Warmup and
  profiling operations are additional and excluded from these totals.

Hostnames, addresses, serials, mount tables, deployment paths and original
operational logs are omitted from this public report.

## fio reference

fio 3.36 used the same CPU and 2-GiB data window, direct io_uring, one job,
three seconds of ramp time, twenty measured seconds and three repetitions.
A sequential fill initialized the window first. Each repetition then ran the
five workloads below in order. Cache invalidation was disabled because direct
I/O bypasses the OS page cache; end-of-job fsync was enabled. No whole-device
discard or preconditioning was performed. [Numeric fio samples](fio-samples.csv)
are included. fio uses a different submission loop and latency definition, so
its IOPS are a reference rather than a strict engine API ceiling.

| Workload | GiB/s | IOPS | Completion P50 (us) | Completion P99 (us) |
| --- | ---: | ---: | ---: | ---: |
| 4 MiB sequential write, QD16 | 9.778 | 2,502 | 6324.2 | 6324.2 |
| 4 MiB sequential read, QD16 | 11.657 | 2,983 | 5210.1 | 8716.3 |
| 4 KiB random read, QD1 | 0.066 | 17,365 | 55.0 | 62.7 |
| 4 KiB random read, QD64 | 2.846 | 746,108 | 81.4 | 138.2 |
| 64 KiB random read, QD64 | 11.643 | 190,755 | 255.0 | 1253.4 |

## Burst writes

These short passes include payload copies, CRCs, completion processing, index
publication and the final flush. Formatting, initial buffer allocation and
opening are excluded. Legacy preallocates its concurrent index; v2 grows its
single-owner index during the timed pass. Legacy uses registered buffers, while
v2 uses ordinary aligned buffers. These are current-implementation comparisons,
not isolated measurements of the frame encoding or locking strategy.

| Record | Legacy GiB/s | V2 GiB/s | V2 change | Legacy physical bytes / payload | V2 physical bytes / payload |
| --- | ---: | ---: | ---: | ---: | ---: |
| 100 B | 0.358 | 0.573 | +60.1% | 1.920 | 1.920 |
| 1 KiB | 3.602 | 5.091 | +41.4% | 1.375 | 1.125 |
| 4 KiB | 8.084 | 8.394 | +3.8% | 1.078 | 1.031 |
| 64 KiB | 9.154 | 9.184 | +0.3% | 1.063 | 1.062 |
| 4 MiB | 7.646 | 8.405 | +9.9% | 1.001 | 1.001 |
| Mixed | 8.984 | 8.863 | -1.3% | 1.082 | 1.060 |

Individual write passes lasted 0.054–1.399 seconds. They do not
establish sustained ingestion, steady-state write amplification, or durable
per-record latency. Reserved footer space is not physically written by this suite.

## Verified random reads

Concurrency below counts logical full-value reads, not physical I/O submissions.
All sizes and both engines use the same random selection and harness validation.

| Record | Concurrency | Legacy Kops/s | V2 Kops/s | V2 change | Legacy GiB/s | V2 GiB/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 B | 1 | 17.346 | 12.512 | -27.9% | 0.002 | 0.001 |
| 100 B | 64 | 778.883 | 372.123 | -52.2% | 0.073 | 0.035 |
| 1 KiB | 1 | 17.309 | 8.480 | -51.0% | 0.017 | 0.008 |
| 1 KiB | 64 | 751.412 | 310.620 | -58.7% | 0.717 | 0.296 |
| 4 KiB | 1 | 4.469 | 8.299 | +85.7% | 0.017 | 0.032 |
| 4 KiB | 64 | 81.225 | 302.480 | +272.4% | 0.310 | 1.154 |
| 64 KiB | 1 | 7.474 | 5.235 | -30.0% | 0.456 | 0.320 |
| 64 KiB | 64 | 179.087 | 153.437 | -14.3% | 10.931 | 9.365 |
| 4 MiB | 1 | 1.161 | 1.088 | -6.3% | 4.537 | 4.249 |
| 4 MiB | 16 | 2.880 | 2.749 | -4.6% | 11.251 | 10.739 |
| 4 MiB | 64 | 2.977 | 2.975 | -0.05% | 11.628 | 11.622 |
| Mixed | 1 | 11.227 | 7.600 | -32.3% | 0.183 | 0.124 |
| Mixed | 64 | 350.702 | 243.462 | -30.6% | 5.719 | 3.972 |

Latency and physical byte cost at concurrency 64:

| Record | Legacy P50 / P99 (us) | V2 P50 / P99 (us) | Legacy physical KiB/read | V2 physical KiB/read | Legacy / V2 CPU (% of one CPU) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 100 B | 76.5 / 135.6 | 149.7 / 301.9 | 4.00 | 9.75 | 95.1 / 99.9 |
| 1 KiB | 80.1 / 139.4 | 203.9 / 298.1 | 4.00 | 11.81 | 95.7 / 99.4 |
| 4 KiB | 606.1 / 3054.6 | 206.5 / 295.9 | 149.81 | 12.00 | 34.2 / 99.4 |
| 64 KiB | 284.3 / 1236.0 | 389.5 / 834.8 | 68.00 | 68.00 | 74.5 / 96.0 |
| 4 MiB | 20923.3 / 33822.8 | 20883.0 / 35417.8 | 4100.00 | 4100.00 | 64.8 / 73.3 |
| Mixed | 156.7 / 600.1 | 249.2 / 475.5 | 32.50 | 26.94 | 79.8 / 98.8 |

## What explains the differences

The verified legacy read constructs one contiguous extent covering the record's
metadata and requested value. For a 4-KiB framed value late in a batch, that
extent also contains intervening values. The observed average is about 150 KiB
per logical 4-KiB read. V2 instead reads 8 KiB of frame metadata and 4 KiB of
payload, reducing this to 12 KiB. This is the clearest format/path benefit in
the measured matrix.

For 100-B and 1-KiB values, legacy normally obtains the record metadata and value
in one 4-KiB read. The 64-record v2 frame has more than one page of front metadata,
so it reads 8 KiB first and often issues a second 4-KiB read. The observed costs
are about 9.75 KiB and 11.81 KiB per operation respectively. The extra dependent
I/O increases low-concurrency latency, while metadata decoding and I/O handling
consume almost one full CPU at concurrency 64.

For 64-KiB and 4-MiB values, both engines read the same total bytes, including
metadata. V2 still waits for metadata completion before submitting payload I/O.
This serial dependency is consistent with its lower QD1 throughput. At QD64,
4-MiB reads have enough outstanding work to reach approximately 11.63 GiB/s on
both implementations. Ordinary versus registered buffers is another implementation
difference; this experiment does not isolate its cost.

Mixed records reduce physical bytes per read in v2, but logical throughput drops.
P99 improves for mixed and 64-KiB reads even though throughput drops; their median
latencies and CPU costs also need to be considered. Fewer bytes alone are
insufficient to predict the faster read path.

## Independent CPU profiles

After the three comparison repetitions, separate QD64 runs were sampled with
`perf record -e cycles:u -F 199 --call-graph dwarf,16384`. They use the same
binary, workload and verification settings. These runs are excluded from the
throughput tables. Profiles cover process startup, the write pass, warmup and
the ten-second read phase, so the percentages do not isolate reads alone.
Some samples are unresolved; the table retains perf's reported self percentages
for selected symbols. No samples were reported lost.

| Engine / record | Selected function | Self samples (%) |
| --- | --- | ---: |
| Legacy / 100 B | `Index::get_and_pin` | 20.67 |
| Legacy / 100 B | `Reader::poll` | 17.21 |
| V2 / 100 B | `Metadata::decode` | 19.60 |
| V2 / 100 B | `RecordDescriptor::decode` | 7.17 |
| V2 / 100 B | CRC fast path | 18.85 |
| V2 / mixed | `Metadata::decode` | 15.70 |
| V2 / mixed | `RecordDescriptor::decode` | 6.14 |
| V2 / mixed | CRC fast path, combined symbol entries | 47.64 |

Together with the physical I/O counters, these profiles identify repeated
metadata parsing and checksum work as substantial costs in the current v2
path. They do not justify disabling payload verification: retain verification
while investigating safe reuse of validated immutable metadata. Kernel buffer
registration costs were not isolated by these profiles.

## Follow-up priorities and limits

1. Reduce dependent read I/O while retaining identity and checksum validation:
   evaluate bounded metadata retention, metadata/payload submission in parallel,
   and bounded coalescing when the metadata-to-value gap is small.
2. Avoid repeating whole-directory work for every record read when validity can
   be safely retained for immutable frame metadata. Preserve the full decoder's
   validation contract and the caller's segment-lifetime guarantees.
3. Compare registered buffer/file I/O after addressing read scheduling, then
   evaluate batching policies across record distributions and memory budgets.

The data window is small and warm. Direct I/O does not eliminate device-side
caching. These runs do not test crash recovery, bit-corruption injection, segment
rollover, concurrent mutation, multiple workers/devices or space reclamation.
The current v2 implementation serves one explicitly assigned segment and is not
yet a complete replacement for the legacy engine.
