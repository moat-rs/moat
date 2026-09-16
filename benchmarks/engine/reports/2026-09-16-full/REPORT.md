# Full-device append and random reads

Both engines fill every available segment with distinct 4-MiB records, then
read uniformly from the entire written key range. This is a complete append-only
fill and full-address-range read comparison; smaller record sizes are not
measured by this run.

## Results

| Phase | QD | Legacy GiB/s | V2 GiB/s | V2 change | Legacy / V2 P99 (us) |
| --- | ---: | ---: | ---: | ---: | ---: |
| write | 64 | 9.767 | 9.143 | -6.39% | — |
| read | 1 | 5.067 | 5.683 | +12.15% | 1137.9 / 987.2 |
| read | 16 | 11.564 | 11.610 | +0.40% | 10328.2 / 10142.9 |
| read | 64 | 11.666 | 11.669 | +0.03% | 33352.1 / 32329.1 |

| Engine | Segments allocated | Distinct records | Payload bytes | Fill seconds |
| --- | ---: | ---: | ---: | ---: |
| legacy | 14,306 | 7,310,366 | 30,661,897,355,264 | 2923.684 |
| v2 | 14,307 | 7,310,877 | 30,664,040,644,608 | 3123.622 |

V2 uses less CPU in this write run despite lower throughput: 2,683.25 CPU-seconds
versus 2,893.85 for legacy, approximately 7.3% less CPU per record. Its wall time
is 199.94 seconds longer. This is consistent with additional waiting in its
synchronous transition path, but the run does not isolate rollover costs or
control for device-history effects; it is not a causal attribution of the gap.

## Method and limits

- Source: `f53cfd24063b1a11f692fa2f6fb88e7b698ac927`. GNU/glibc release binary
  SHA-256: `7d9e86a501ee1fb13023b0b8a5b9ce45a0bf4f96c149e1ced860b11442c946a9`.
- Linux 6.8.0, glibc 2.39, AMD EPYC x86-64, enterprise NVMe with
  30,725,971,992,576 bytes. Application thread pinned to a NUMA-local CPU.
  No profiling or other benchmark overlaps the run.
- One full fill per engine, legacy first. Reads warm for two seconds and measure
  for 60 seconds at each QD. These are individual measurements, not medians or
  a run-to-run confidence interval. Engine order and device history can affect results.
- Both use O_DIRECT, fixed files/buffers, a 1-GiB pool, deferred taskrun, and
  `HugePages::Preferred`. Every phase reports an actual 1 GiB of anonymous huge
  pages per pool and zero hugetlb bytes, rather than relying only on the THP hint.
- Both indexes reserve capacity before timing. The write interval includes
  payload copying, CRC generation, publication, segment rollover, persistence
  barriers, and final sealing. Formatting and opening are outside the interval.
- V2 drains its pipeline and synchronously persists allocation/footer/seal
  transitions. Legacy overlaps new-segment writing with old-segment sealing
  and requests sync barriers on explicit flush/seal calls. These implementation
  differences are included; their individual throughput effects are not isolated.
- Timed reads use `verify = false`. The callback checks length, the key prefix
  and final byte; this is sampled content validation, not a full integrity scan.
- Filling stops only when no unused segment remains. Metadata, alignment, space
  too small for another record, and a trailing partial segment reduce payload
  occupancy. Legacy reserves a segment-sized metadata region; v2 uses two
  superblock pages before the slots. The resulting record counts need not match.
- Random reads cover the full key-selection domain. The timed phases do not
  necessarily visit every record. Direct I/O bypasses the OS page cache but does
  not eliminate device caching. This is not steady-state overwrite/GC testing.
- All latest-version indexes remain in RAM. A full tiny-record distinct-key
  workload may exceed memory; no distributed small dataset is mislabeled as a
  full tiny-record fill.

All 14,621,243 record writes and
879,615 measured reads completed
without reported I/O or sampled-content errors. Warmup reads are additional.

[Samples](samples.csv) retain timings, CPU costs, memory observations and device
counters; [summaries](summary.csv) contain one observation per configuration.
Operational identifiers and private deployment details are excluded.

[Fill progress](fill-progress.csv) preserves approximately 30-second intervals.
These counters track accepted records before the final durable flush, rather
than independently durable batches. The final partial interval is not sampled.
