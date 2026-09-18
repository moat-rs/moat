# Longer twenty-device comparison

This rerun addresses the short write bursts and large spread in the preceding
[source-pool experiment](../input-pool/REPORT.md). Every published main sample
contains at least **10 seconds of timed writes** and **10 seconds of measured
random reads**, followed by complete request draining. Startup, dataset generation,
routing, verification, and teardown do not count toward the write duration.

In this single round, current 64-KiB writes remain **4.1% below the original
with fsync enabled**, and **1.4% below with fsync disabled**. The earlier large
allocating-input regression is not reproduced at its former magnitude, but this
does not establish parity. Small-value writes are faster in this round, while
4-MiB writes and random reads remain close to the original.

## Comparison contract

All twenty authorized devices participate in every process. One fresh process
per size and variant produces 15 main samples, as requested.
For each size the variant order is original/enabled/disabled. No repetition-based
variability or statistical significance claim is made from this single round.
Variants use the same dataset size, placement,
source-pool implementation, CPU placement, and I/O concurrency. The original retains
its original fsync policy. Current enabled and disabled use one binary with the
policy selected at initialization. Disabling fsync changes the durability contract.

The binaries are exactly those used in the preceding source-pool experiment,
with the system allocator and no allocator overrides. Both original and current
use the same pooled-input harness. Build hashes are in [build.json](build.json).
No engine or benchmark implementation changed for this rerun.

To fit sufficiently long small-value writes inside the existing **64-GiB/device
window**, all main variants use **1-GiB segments**. The previous experiment used
2-GiB segments. Current retains its default 64-MiB active-segment metadata limit;
with small values it can seal segments before their payload area is full. Smaller
segments provide more allocatable slots within the same authorized window. This
is an explicit workload change for both revisions and increases rollover frequency.
The original has no corresponding active-metadata resource limit. These results
must not be combined with the earlier 2-GiB samples as repetitions of one workload.

Other settings remain twenty pinned native owners, a 512-MiB registered I/O pool
per device, queue depth 256 per owner, 16-byte keys, and a global prefill batch of
2560 (128 records per owner). The source pool remains capped at 64 records and
about 4 MiB of payload per owner, prepared before timing. Each timed write still
fills the full source value, copies it into registered storage, constructs/checksums
frames, completes I/O, and performs the configured final fsync. Random reads use
640 global clients (32/device), two seconds of warmup, and ten measured seconds.
Read CRC verification remains disabled; all prefills retain write checksums.

## Dataset and measured write durations

Record counts below are the configured nominal per-device count. Deterministic
placement distributes exactly twenty times that many records across all devices;
individual device counts can differ slightly. Larger index working sets and
segment transitions are part of this longer workload.

| Value | Nominal records/device | Value payload/device | Original write s | Current enabled s | Current disabled s |
|---|---:|---:|---:|---:|---:|
| 100 B | 41,943,040 | 3.906 GiB | 15.58 | 14.41 | 14.22 |
| 1 KiB | 25,165,824 | 24.000 GiB | 14.22 | 13.54 | 13.44 |
| 4 KiB | 10,485,760 | 40.000 GiB | 14.02 | 11.17 | 10.81 |
| 64 KiB | 524,288 | 32.000 GiB | 17.57 | 18.31 | 17.81 |
| 4 MiB | 8,192 | 32.000 GiB | 16.46 | 16.52 | 16.36 |

## Timed write throughput

Each cell is **one measured sample**. Small values
use million records/s; large values use GiB/s of application value payload,
excluding keys, frame metadata, alignment, and footer bytes. Relative changes
are ratios of individual samples, not confidence intervals.

| Value | Unit | Original | Current fsync enabled | Current fsync disabled | Enabled vs original | Disabled vs original |
|---|---|---:|---:|---:|---:|---:|
| 100 B | Mops/s | 53.847 | 58.200 | 58.990 | +8.1% | +9.6% |
| 1 KiB | Mops/s | 35.393 | 37.162 | 37.452 | +5.0% | +5.8% |
| 4 KiB | Mops/s | 14.958 | 18.779 | 19.408 | +25.5% | +29.7% |
| 64 KiB | GiB/s | 36.429 | 34.951 | 35.926 | -4.1% | -1.4% |
| 4 MiB | GiB/s | 38.880 | 38.743 | 39.115 | -0.4% | +0.6% |

The 64-KiB fsync-disabled sample is 2.8% faster than current enabled, but still
below the original. All three have identical timed physical write-byte counts.
This does not isolate the cost of fsync: the processes run sequentially, and
there is only one sample per configuration. Likewise, the larger 4-KiB gain
is an observation of the complete revisions under this workload, not an
attribution to one lifecycle change. Longer timing alone does not measure
run-to-run variability.

### Follow-up inspection of the 64-KiB gap

Read-only inspection of the existing samples and both measured source snapshots
finds a concrete footer-I/O difference; no additional throughput run was made.

| Metric, timed writes | Original | Current enabled | Current disabled |
|---|---:|---:|---:|
| Wall seconds | 17.568 | 18.312 | 17.814 |
| Process CPU us/record | 28.337 | 29.832 | 28.817 |
| Physical write GiB | 721.329422 | 721.329422 | 721.329422 |
| Physical write requests | 10,498,740 | 10,509,540 | 10,509,540 |
| Sampled maximum io-wq helpers | 0 | 20 | 0 |

At this record size, a full 1-GiB segment contains 14,536 frames of 73,728 bytes
and a 1,978,368-byte footer. Excluding its final 4-KiB commit page, the footer
body is 1,974,272 bytes. The original submits that body as one positional write;
the observed device limit of 128 KiB gives sixteen physical requests. Current
`FooterEncoder::next` caps each body chunk at 64 KiB, producing 31 requests.
`Engine::drive_job` waits for the control completion before requesting the next
chunk, so these chunks do not form a concurrent footer-write batch. Each chunk
also allocates and zeroes an aligned buffer. This is separate from the pooled
benchmark source values.

For every device, the request-count difference is exactly 540, matching 36
rollovers times fifteen additional footer-body requests. Reconstructed record
counts, frame bytes, footer bytes, and allocation headers exactly match each
device's observed physical byte count. Across twenty devices the difference is
**10,800 requests**, entirely accounted for by this footer geometry. This
identifies extra requests and serial completion dependencies, not their share
of the measured elapsed-time gap.

The current enabled/disabled wall-time difference is 0.497 seconds; the disabled
sample remains 0.246 seconds behind the original. Current lifecycle barriers
use the queue, whereas the original called device sync directly; the worker
observations are consistent with that implementation difference. They do not
measure worker wakeup latency or isolate fsync overhead. Additional process CPU
can include polling while awaiting I/O, not just extra computation per record.

Footer-chunk serialization and lifecycle-sync waiting were the initial follow-up
hypotheses. The subsequent [64-KiB investigation](../64k-followup/REPORT.md)
measures only tens of milliseconds in all rollover jobs per owner and a few
milliseconds of direct sync waiting, ruling out those intervals as the main
explanation for this wall-time gap. It identifies large copy-time differences
between CPU/cache groups and removes the observed revision gap by page-aligning
both revisions' preallocated source payloads. The original numbers above remain
unchanged; they use the earlier naturally aligned input pool. This result does
not support reusing the earlier allocating-input `MADV_DONTNEED` attribution.

## Random-read throughput and latency

Throughput and P99 come from the same single process for each variant. P99 includes
admission waiting and the driver's identity/length validation path. The read
phase drains outstanding requests, so measured wall time can slightly exceed
ten seconds.

| Value | Unit | Original | Current enabled | Current disabled | P99 us: original / enabled / disabled |
|---|---|---:|---:|---:|---|
| 100 B | Mops/s | 8.909 | 8.871 | 8.880 | 114.687 / 114.815 / 114.751 |
| 1 KiB | Mops/s | 9.048 | 9.022 | 9.051 | 113.663 / 113.791 / 113.599 |
| 4 KiB | Mops/s | 8.086 | 8.068 | 8.073 | 129.471 / 129.599 / 129.535 |
| 64 KiB | GiB/s | 186.831 | 186.952 | 186.811 | 541.183 / 540.159 / 540.159 |
| 4 MiB | GiB/s | 233.048 | 233.042 | 233.028 | 19054.591 / 19202.047 / 19120.127 |

## Coverage, artifacts, and limits

All **15 main processes** completed successfully and all **4,687,626,240 inserted
records** passed full-key, value-length, prefix, and suffix checks before timed
random reads. Every sample records positive write and read activity on each of
twenty devices. These checks are not exhaustive value-byte verification or
power-loss testing. The earlier shorter pilots are excluded from every table.
The controller rejects any main process with a write or measured-read phase
shorter than ten seconds.

The main round took **41.73 minutes** including untimed dataset preparation,
verification, and process teardown. Summed timed writes and measured reads total
224.45 and 150.03 seconds respectively, or about 6.24 minutes together.
In particular, each 100-B process prepares
and subsequently verifies 838,860,800 records. This repeated preparation is the
main reason that wall time is much longer than the measured I/O intervals.
The requested second and third rounds were canceled; a next-round process that
had begun preparing records was stopped before any owner initialized or any
write/read measurement began.

Final checks found all twenty devices idle with no open users, all device
controllers live, and both system mirrors intact. SMART was unavailable to the
benchmark account. Sanitized completion and duration checks are retained in
[validation.json](validation.json).

[samples.csv](samples.csv) retains every main measurement, including write/read
seconds, throughput, CPU, RSS, and sampled io-wq thread counts.
[summary.csv](summary.csv) retains one-sample summaries with repetition counts;
variation fields are empty.
[devices.csv](devices.csv) retains physical request and byte counts for all
**300 sample/device pairs**, using anonymous device ordinals. These are kernel
block statistics and are distinct from logical application throughput.

Physical I/O pools remain fixed; larger record/index working sets increase process
RSS. Segment allocation and rollover sealing are included in timed writes; the final
active-segment seal at close remains outside write timing. This is a
bounded-window unique-insert and random-read experiment, not an indefinite
reclaim/reuse workload or a full-device steady-state endurance test. Fsync-off
results apply to the explicitly selected policy; they do not independently
establish power-loss safety of a device.

Private host identities, device allowlists, raw logs, and control scripts remain
outside the repository. No kernel, device-cache, or global allocator settings
were modified.
