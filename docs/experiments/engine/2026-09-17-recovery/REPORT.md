# Twenty-device mixed-record fill and recovery baseline

Twenty devices were filled in parallel to `OutOfSpace`, with five value sizes each
accounting for 20% of records. After all segments were flushed and sealed, fill
processes exited. The median all-ready time across three recovery rounds using
fresh processes was **19.621 seconds**.

**This is the baseline before the footer-trailer change**, measured with engine
revision `9d90b6b4cb7225103ab304cf7e4553312a8409d8`. It does not establish recovery
performance for the new tail-anchored footer format.

## Results

| Round | All ready (s) | Device min (s) | Device median (s) | Device max (s) | Total read (GiB) | Total process CPU (s) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 19.483 | 16.680 | 19.163 | 19.483 | 90.598 | 251.611 |
| 2 | 19.621 | 16.910 | 19.424 | 19.621 | 90.598 | 255.649 |
| 3 | 19.673 | 16.842 | 19.426 | 19.673 | 90.598 | 255.812 |

- The fill wrote **719,642,100 records**, with unique keys within each device,
  totaling **613,863,347,005,200 payload bytes** (613.863 TB / 558.305 TiB) across
  **286,140 segments**.
- Recovery I/O was identical in all three rounds: **97,278,607,360 bytes**
  (97.279 GB / 90.598 GiB) and **1,430,740 block-device read operations** per round
  across twenty devices. Each device read **4,863,930,368 bytes in 71,537 operations**.
  These are recovery-only deltas from `/sys/class/block/<device>/stat`, converting
  sectors at 512 bytes each. Post-recovery verification is excluded. Read merges
  and write I/O were zero.
- Application-level `Device::read_at` calls totaled **858,460 per round**, or
  42,923 per device, with the same byte totals as the block layer. Requests can
  split in the block layer, so call counts differ from block I/O counts. Recovery
  rebuilt the index for all records without loading all payload into memory.
- The slowest fill took **14111.926 seconds**, approximately 3 hours 55 minutes.
  This includes encoding, checksums, I/O, rollover, and final flush/seal, but
  excludes formatting, opening an empty device, and initial index reservation.
- Every round read and fully compared **10,100 sampled records**, covering all
  five sizes at beginning, middle, and end positions on every device, with read
  CRC enabled. Recovered record and segment counts matched the fill in all rounds.
- The sum of per-process peak RSS during recovery was **117.070–117.075 GiB**.
  This sums process high-water marks; it is not simultaneous aggregate RSS.

## Method and limits

- The [benchmark guide](../../../../benchmarks/engine/RECOVERY.md) describes the
  harness and runner. The baseline used a GNU/glibc release build with
  `RUSTFLAGS=-C target-cpu=x86-64-v3`. [Build provenance](build.json) records the
  source revision and harness, binary, and lockfile SHA-256 hashes.
- Twenty devices ran concurrently, one CPU-pinned process per device, with 2 GiB
  segments, an 8 MiB frame limit, a 512 MiB pool, preferred huge pages, registered
  files and buffers, and io_uring physical depth 256. Writes held at most 32 frame
  buffers concurrently. The harness asserted record counts and full slot allocation.
- Each frame contained one value of each size: 100 B, 1 KiB, 4 KiB, 64 KiB, and
  4 MiB. Independent indexes used the same 128-bit key-number range. Values used
  deterministic pseudorandom templates with the record number in their first
  eight bytes. The 4 MiB class contributed 98.34% of payload bytes. There were no
  overwrites, deletions, or reclamation.
- Fill consumed all allocatable space. Payload totals exclude metadata, footers,
  alignment, segment tails too small for another frame, and device tails too
  small for a complete segment.
- Recovery timing covered exactly `Engine::open`, including layout/header/footer
  reads and validation, index allocation, growth, and reconstruction. File, pool,
  and io_uring initialization and registration were outside timing. All-ready
  time measures from the common monotonic start to the last completion, rather
  than summing per-device durations.
- Each round used fresh processes and `O_DIRECT`. OS page caching was bypassed;
  device caches could still affect results. First and subsequent rounds are
  retained separately and are not claimed as cold-media measurements.
- All segments were sealed normally. Recovery read and validated metadata, not
  every payload byte. These results do not measure full payload integrity scans,
  power-loss recovery, or fallback scans for unsealed segments or corrupt footers.
- Sampling began after all devices recovered, avoiding overlap with recovery on
  other devices. Sampling does not verify every stored record.
- Device identity, capacity, system-device exclusion, partitions, mounts, holders,
  signatures, open users, and idle state were checked before filling. Advisory
  locks were held throughout fill and recovery. Worker exit and lock release were
  checked afterward.

[Per-device samples](samples.csv), [round summaries](summary.csv),
[block-device I/O](block-io.csv), [fill results](fill.csv), and
[fill progress](fill-progress.csv) retain numerical observations. Device ordinals
are anonymous. Hostnames, accounts, device identifiers, CPU assignments, network
addresses, and deployment paths are excluded.
