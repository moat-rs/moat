# V2 recovery after a mixed-record full-device fill

The [twenty-device baseline report](../../docs/experiments/engine/2026-09-17-recovery/REPORT.md)
contains fill results, three recovery rounds, and numerical samples. That baseline
predates the footer-trailer change; it does not measure the current format.

The standalone `recovery` binary assigns one CPU-pinned process to each device.
Workers fill all segments in parallel, flush and seal, and exit. Three rounds of
fresh processes then recover the devices in parallel.

Each frame contains one record of each size: 100 B, 1 KiB, 4 KiB, 64 KiB, and
4 MiB. Keys are unique 128-bit identifiers within each device; independent device
indexes use the same identifier range. Each size accounts for 20% of records;
4 MiB values account for approximately 98.34% of payload bytes. Values use
deterministic pseudorandom templates with the record number in their first eight
bytes. There are no overwrites, deletions, or reclamation.

Timing covers `Engine::open`: device layout, allocation headers, footer trailers
and prefixes, metadata validation, and index reconstruction. File opening, CPU
pinning, pool and io_uring initialization, and buffer/file registration occur
outside the timed interval. Recovery index allocation and growth are included.
The all-ready interval uses a shared monotonic start. Content verification begins
only after every device has recovered, avoiding interference with recovery I/O.

Each round checks index record and segment counts, then reads all five sizes at
101 positions spanning the beginning, middle, and end: 505 records per device.
Reads enable CRC verification and compare full returned contents. This is sampling,
not a full payload scan. Sealed recovery timings do not describe power-loss
recovery, corrupt footers, or unsealed segment scans. `O_DIRECT` bypasses the OS
page cache but does not eliminate device caching.

## Running

The following command overwrites all allocatable space on the configured devices.
Use reviewed, unmounted, unused scratch devices. Keep configuration and raw output
outside the repository because they contain deployment information.

```sh
CARGO_TARGET_DIR=target/engine-compare \
  RUSTFLAGS='-C target-cpu=x86-64-v3' \
  cargo build --locked --release --target x86_64-unknown-linux-gnu \
  --manifest-path benchmarks/engine/Cargo.toml --bin recovery

python3 benchmarks/engine/recovery_run.py \
  --config /path/to/private-config.json \
  --binary target/engine-compare/x86_64-unknown-linux-gnu/release/recovery \
  --output /path/to/new-private-output \
  --overwrite-entire-devices
```

The example configuration uses placeholders. `disks` and `io_cpus` have matching
order and lengths. `expected_capacity` must match the actual capacity in bytes;
a smaller window is not a full-device fill. Include system-device serial numbers
in `forbidden_serials`.

```json
{
  "host": "REVIEWED_TEST_HOST",
  "disks": [
    {
      "path": "/dev/REVIEWED_SCRATCH_DEVICE",
      "serial": "REVIEWED_SERIAL",
      "expected_capacity": 1099511627776
    }
  ],
  "forbidden_serials": ["SYSTEM_DEVICE_SERIAL"],
  "io_cpus": [0],
  "segment_bytes": 2147483648,
  "pool_bytes": 536870912
}
```

Before writing, the runner checks device identity, capacity, partitions, holders,
mounts, signatures, open users, and idle state. It holds advisory locks using the
same naming convention as the existing benchmark throughout all phases. These
locks only coordinate cooperating programs. A failure stops all workers started
by that runner. The host needs sufficient locked-memory allowance and memory for
all distinct-key indexes.

For file smoke tests, create blank files in advance, set `serial` to an empty
string, and match `expected_capacity` to the file length. Replace the destructive
block-device flag with `--smoke-files`, which rejects block devices. A 128 MiB file
with 16 MiB segments exercises rollover, exhaustion, and complete recovery.

`status.json` tracks the current phase. Each `recovery-N.json` retains per-device
timing, cold-path read counts and bytes, CPU usage, peak RSS, and all-ready time.
`fill.json` records actual record counts, payload bytes, and segment counts. Publish
only anonymous device ordinals and numerical parameters; exclude private configs,
device paths, and machine identities.
