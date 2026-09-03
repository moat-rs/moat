// Copyright 2026- Moat Project Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Engine throughput and latency on a real file or block device.
//!
//! ```sh
//! cargo bench -p moat-engine
//! MOAT_BENCH_DEVICE=/path/to/block-device MOAT_BENCH_BYTES=$((64<<30)) cargo bench -p moat-engine
//! ```
//!
//! Without `MOAT_BENCH_DEVICE` a 4 GiB file is created in the temp directory,
//! opened with `O_DIRECT` when the filesystem allows it. The device is
//! formatted: **never point this at a device holding data you need.**
//!
//! Knobs: `MOAT_BENCH_BYTES` (device bytes to use; a quarter goes to each
//! workload), `MOAT_BENCH_READERS` (reader threads, each with its own ring),
//! `MOAT_BENCH_LARGE` / `MOAT_BENCH_SMALL` (value sizes), `MOAT_BENCH_SYNC`
//! (use the blocking queue instead of io_uring).

use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use moat_common::{ChunkId, HugePages, PoolOptions, block_checksums};
use moat_engine::{
    FileDevice, FormatOptions, Options, PutOptions, PutOutcome, QueueOptions, ReadOutcome, ReadRing, Writer,
};

const SEGMENT: u64 = 256 << 20;
const CHUNK_MAX: u32 = 4 << 20;

fn queue_options() -> QueueOptions {
    QueueOptions {
        depth: 256,
        pool: PoolOptions {
            bytes: 512 << 20,
            max_class: 8 << 20,
            huge_pages: HugePages::Preferred,
        },
        force_sync: std::env::var_os("MOAT_BENCH_SYNC").is_some(),
    }
}

fn gib_per_s(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / elapsed.as_secs_f64() / (1u64 << 30) as f64
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn id(n: u64) -> ChunkId {
    ChunkId::from_u128(n as u128)
}

/// Writes `count` values of `len` bytes through the zero-copy path with the
/// pipeline kept full, returning aggregate throughput.
fn bench_puts(writer: &mut Writer, first_id: u64, count: u64, len: usize, label: &str) {
    let pattern: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
    let sums = block_checksums(&pattern);
    let start = Instant::now();
    let mut done = Vec::new();
    let mut acked = 0u64;
    for i in 0..count {
        let outcome = if len >= 64 << 10 {
            let mut large = writer.prepare_large(len as u32).unwrap();
            large.value_mut().copy_from_slice(&pattern);
            writer
                .put_large(id(first_id + i), large, Some(&sums), PutOptions::default())
                .unwrap()
        } else {
            writer.put(id(first_id + i), &pattern, PutOptions::default()).unwrap()
        };
        assert!(matches!(outcome, PutOutcome::Written { .. }));
        // Keep submitting; reap opportunistically so completions do not pile up.
        if i % 8 == 7 {
            writer.poll(&mut done, false).unwrap();
            acked += done.len() as u64;
            done.clear();
        }
    }
    writer.flush().unwrap();
    let elapsed = start.elapsed();
    black_box(acked);
    println!(
        "{label:<34} {:>8.2} GiB/s {:>10.0} ops/s  ({count} x {len} B in {:.2?})",
        gib_per_s(count * len as u64, elapsed),
        count as f64 / elapsed.as_secs_f64(),
        elapsed
    );
}

/// Random reads with `inflight` outstanding on one ring. Returns per-request
/// latencies.
fn read_loop(
    ring: &mut ReadRing,
    first_id: u64,
    count: u64,
    len: usize,
    inflight: usize,
    total: u64,
    seed: u64,
) -> Vec<Duration> {
    let mut rng = seed | 1;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut latencies = Vec::with_capacity(total as usize);
    let mut started: Vec<Option<Instant>> = vec![None; inflight];
    let mut out = Vec::new();
    let mut issued = 0u64;
    let mut completed = 0u64;
    let mut free_slots: Vec<usize> = (0..inflight).collect();
    while completed < total {
        while issued < total
            && let Some(slot) = free_slots.pop()
        {
            let key = id(first_id + next() % count);
            started[slot] = Some(Instant::now());
            match ring.get(&key, None, slot as u64) {
                Ok(ReadOutcome::Submitted) => issued += 1,
                Ok(ReadOutcome::Miss) => panic!("missing key"),
                Err(moat_engine::Error::Busy) => {
                    free_slots.push(slot);
                    break;
                }
                Err(e) => panic!("{e}"),
            }
        }
        ring.poll(&mut out, true).unwrap();
        for c in out.drain(..) {
            let slot = c.token as usize;
            let data = c.result.unwrap().expect("not expired");
            assert_eq!(data.len(), len);
            black_box(&*data);
            latencies.push(started[slot].take().unwrap().elapsed());
            free_slots.push(slot);
            completed += 1;
        }
    }
    latencies
}

/// Random reads spread over `readers` threads, each with its own ring and
/// `inflight` outstanding requests.
#[allow(clippy::too_many_arguments)]
fn bench_reads(
    reader: &moat_engine::Reader,
    readers: usize,
    first_id: u64,
    count: u64,
    len: usize,
    inflight: usize,
    total: u64,
    label: &str,
) {
    let per_thread = total / readers as u64;
    // Rings (and their pools) are created before the clock starts: mapping and
    // registering a pool pins and clears hundreds of megabytes, which is
    // start-up cost, not read-path cost.
    let rings: Vec<ReadRing> = (0..readers).map(|_| reader.ring(&queue_options()).unwrap()).collect();
    let start = Instant::now();
    let handles: Vec<_> = rings
        .into_iter()
        .enumerate()
        .map(|(t, mut ring)| {
            std::thread::spawn(move || {
                read_loop(
                    &mut ring,
                    first_id,
                    count,
                    len,
                    inflight,
                    per_thread,
                    0x9e37_79b9 + t as u64,
                )
            })
        })
        .collect();
    let mut latencies: Vec<Duration> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let elapsed = start.elapsed();
    latencies.sort();
    let done = latencies.len() as u64;
    println!(
        "{label:<34} {:>8.2} GiB/s {:>10.0} ops/s  p50 {:>8.1?} p99 {:>8.1?} p999 {:>8.1?}",
        gib_per_s(done * len as u64, elapsed),
        done as f64 / elapsed.as_secs_f64(),
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.99),
        percentile(&latencies, 0.999),
    );
}

fn main() {
    let bytes: u64 = std::env::var("MOAT_BENCH_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 << 30);
    let dir = tempfile::tempdir().unwrap();
    let device = match std::env::var("MOAT_BENCH_DEVICE") {
        Ok(path) => FileDevice::open(&path, true).expect("open device"),
        Err(_) => {
            let path = dir.path().join("bench.img");
            FileDevice::create(&path, bytes, true)
                .or_else(|_| FileDevice::create(&path, bytes, false))
                .unwrap()
        }
    };
    let device = Arc::new(device);
    moat_engine::format(
        &*device,
        &FormatOptions {
            segment_size: SEGMENT,
            chunk_max: CHUNK_MAX,
            disk_uuid: [7; 16],
        },
    )
    .unwrap();
    let opened = moat_engine::open(
        device,
        Options {
            queue: queue_options(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut writer = opened.writer;
    let reader = opened.reader;
    let env =
        |name: &str, default: u64| -> u64 { std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default) };
    let readers = env("MOAT_BENCH_READERS", 1) as usize;
    let large = env("MOAT_BENCH_LARGE", 1 << 20) as usize;
    let small = env("MOAT_BENCH_SMALL", 4 << 10) as usize;

    // Budget: roughly a quarter of the device per size class so nothing
    // triggers reclaim during the measurement.
    let budget = bytes / 4;
    let large_count = budget / large as u64;
    let small_count = (budget / small as u64).min(2_000_000);

    println!(
        "device {} GiB, segment {} MiB, chunk max {} MiB, {readers} reader thread(s), io_uring={}",
        bytes >> 30,
        SEGMENT >> 20,
        CHUNK_MAX >> 20,
        !queue_options().force_sync
    );
    bench_puts(
        &mut writer,
        0,
        large_count,
        large,
        &format!("put {} KiB (zero-copy)", large >> 10),
    );
    bench_puts(
        &mut writer,
        1 << 40,
        small_count,
        small,
        &format!("put {} KiB (packed)", small >> 10),
    );
    bench_reads(
        &reader,
        readers,
        0,
        large_count,
        large,
        16,
        large_count.min(16_384),
        &format!("get {} KiB, 16 in flight/thread", large >> 10),
    );
    bench_reads(
        &reader,
        readers,
        1 << 40,
        small_count,
        small,
        64,
        small_count.min(2_000_000),
        &format!("get {} KiB, 64 in flight/thread", small >> 10),
    );
    bench_reads(
        &reader,
        1,
        1 << 40,
        small_count,
        small,
        1,
        50_000,
        &format!("get {} KiB, 1 in flight, 1 thread", small >> 10),
    );
    writer.close().unwrap();
}
