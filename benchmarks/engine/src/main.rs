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

//! Destructive, bounded comparison of the legacy engine and the v2 pipeline.

mod legacy;
mod unified;

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use moat_common::ChunkId;
use serde_json::json;

const SEGMENT: u64 = 2 << 30;
const CAPACITY: u64 = 2 * SEGMENT;
const DEPTH: usize = 64;
const MAX_VALUE: u32 = 4 << 20;

struct Config {
    path: PathBuf,
    engine: String,
    workload: String,
    bytes: u64,
    seconds: u64,
    qds: Vec<usize>,
    disk_stat: Option<PathBuf>,
}

impl Config {
    fn parse() -> Self {
        let args: Vec<_> = std::env::args().collect();
        assert_eq!(
            args.len(),
            8,
            "usage: moat-engine-compare PATH legacy|v2 SIZE|mixed PAYLOAD_MIB SECONDS QDS --overwrite-first-4g"
        );
        assert_eq!(args[7], "--overwrite-first-4g");
        assert!(matches!(args[2].as_str(), "legacy" | "v2"));
        let path = fs::canonicalize(&args[1]).unwrap();
        let stat = PathBuf::from("/sys/class/block")
            .join(path.file_name().unwrap())
            .join("stat");
        let config = Self {
            path,
            engine: args[2].clone(),
            workload: args[3].clone(),
            bytes: args[4].parse::<u64>().unwrap().checked_mul(1 << 20).unwrap(),
            seconds: args[5].parse().unwrap(),
            qds: args[6].split(',').map(|s| s.parse().unwrap()).collect(),
            disk_stat: stat.exists().then_some(stat),
        };
        assert!((1..=512 << 20).contains(&config.bytes));
        assert!(config.seconds > 0 && config.qds.iter().all(|&qd| (1..=DEPTH).contains(&qd)));
        config
    }

    fn sizes(&self) -> Vec<usize> {
        if self.workload == "mixed" {
            vec![100, 4096, 65536, 300]
        } else {
            let len: usize = self.workload.parse().unwrap();
            assert!((16..=MAX_VALUE as usize).contains(&len));
            vec![len]
        }
    }
}

struct Record {
    number: u64,
    value: Vec<u8>,
}

fn key(number: u64) -> ChunkId {
    ChunkId::from_u128(number as u128)
}

fn fresh_identity() -> [u8; 16] {
    use std::io::Read;
    let mut identity = [0; 16];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut identity)
        .unwrap();
    identity
}

trait Backend {
    fn write_batch(&mut self, records: &[Record]);
    fn flush(&mut self, records: u64);
    fn prepare_reads(&mut self, sizes: &[usize], qd: usize);
    fn read(&mut self, number: u64, len: usize) -> u64;
    fn poll_reads(&mut self, visit: impl FnMut(u64, &[u8]));
    fn finish(self);
}

#[derive(Default)]
struct Counters {
    user: f64,
    system: f64,
    read_bytes: u64,
    write_bytes: u64,
    read_ios: u64,
    write_ios: u64,
}

impl Counters {
    fn sample(config: &Config) -> Self {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the writable output on success.
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
        // SAFETY: the successful call initialized the entire structure.
        let usage = unsafe { usage.assume_init() };
        let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        let mut result = Self {
            user: seconds(usage.ru_utime),
            system: seconds(usage.ru_stime),
            ..Self::default()
        };
        if let Some(path) = &config.disk_stat {
            let text = fs::read_to_string(path).unwrap();
            let fields: Vec<u64> = text.split_whitespace().map(|s| s.parse().unwrap()).collect();
            result.read_ios = fields[0];
            result.read_bytes = fields[2] * 512;
            result.write_ios = fields[4];
            result.write_bytes = fields[6] * 512;
        }
        result
    }
}

struct Measurement {
    start: Instant,
    before: Counters,
    latencies: Vec<u64>,
    completed: u64,
    bytes: u64,
}

impl Measurement {
    fn new(config: &Config) -> Self {
        let latencies = Vec::with_capacity(1 << 20);
        let before = Counters::sample(config);
        Self {
            start: Instant::now(),
            before,
            latencies,
            completed: 0,
            bytes: 0,
        }
    }

    fn report(mut self, config: &Config, phase: &str, qd: usize) {
        let elapsed = self.start.elapsed().as_secs_f64();
        let after = Counters::sample(config);
        self.latencies.sort_unstable();
        let percentile = |p: f64| {
            self.latencies
                .get(((self.latencies.len().saturating_sub(1)) as f64 * p).round() as usize)
                .map(|&ns| ns as f64 / 1000.0)
        };
        println!(
            "{}",
            json!({
                "engine": config.engine, "workload": config.workload, "phase": phase,
                "qd": qd, "seconds": elapsed, "operations": self.completed,
                "payload_bytes": self.bytes, "gib_s": self.bytes as f64 / elapsed / (1u64 << 30) as f64,
                "ops_s": self.completed as f64 / elapsed,
                "p50_us": percentile(0.5), "p99_us": percentile(0.99), "p999_us": percentile(0.999),
                "latency_samples": self.latencies.len(),
                "cpu_user_s": after.user - self.before.user, "cpu_system_s": after.system - self.before.system,
                "device_read_bytes": after.read_bytes - self.before.read_bytes,
                "device_write_bytes": after.write_bytes - self.before.write_bytes,
                "device_read_ios": after.read_ios - self.before.read_ios,
                "device_write_ios": after.write_ios - self.before.write_ios,
                "verified_reads": true, "durable_flush": true,
            })
        );
    }
}

fn read_phase(backend: &mut impl Backend, config: &Config, sizes: &[usize], count: u64, qd: usize, warmup: bool) {
    let mut pending = HashMap::with_capacity(qd);
    let mut rng = 0x9e37_79b9u64;
    let mut issued = 0u64;
    let seconds = if warmup { 2 } else { config.seconds };
    let mut measurement = Measurement::new(config);
    loop {
        let stop = measurement.start.elapsed() >= Duration::from_secs(seconds);
        while !stop && pending.len() < qd {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let number = rng % count;
            let len = sizes[number as usize % sizes.len()];
            let started = issued.is_multiple_of(16).then(Instant::now);
            let ticket = backend.read(number, len);
            assert!(pending.insert(ticket, (number, len, started)).is_none());
            issued += 1;
        }
        if pending.is_empty() {
            break;
        }
        backend.poll_reads(|ticket, data| {
            let (number, len, started) = pending.remove(&ticket).expect("unknown completion");
            assert_eq!(data.len(), len);
            assert_eq!(&data[..8], &number.to_le_bytes());
            assert_eq!(data[len - 1], ((len - 1) % 253) as u8);
            if let Some(started) = started {
                measurement.latencies.push(started.elapsed().as_nanos() as u64);
            }
            measurement.completed += 1;
            measurement.bytes += len as u64;
        });
    }
    assert_eq!(measurement.completed, issued);
    if !warmup {
        measurement.report(config, "read", qd);
    }
}

fn run(mut backend: impl Backend, config: &Config) {
    let sizes = config.sizes();
    // Mixed groups retain input order. Uniform large records use prepared I/O.
    let batch = if sizes.len() == 1 && sizes[0] >= 65536 { 16 } else { 64 };
    let group_bytes: u64 = (0..batch).map(|i| sizes[i % sizes.len()] as u64).sum();
    let groups = (config.bytes / group_bytes).max(1);
    let count = groups * batch as u64;
    let mut records: Vec<_> = (0..batch)
        .map(|i| Record {
            number: 0,
            value: (0..sizes[i % sizes.len()]).map(|j| (j % 253) as u8).collect(),
        })
        .collect();
    let mut measurement = Measurement::new(config);
    for group in 0..groups {
        for (i, record) in records.iter_mut().enumerate() {
            record.number = group * batch as u64 + i as u64;
            record.value[..8].copy_from_slice(&record.number.to_le_bytes());
        }
        backend.write_batch(&records);
    }
    backend.flush(count);
    measurement.completed = count;
    measurement.bytes = groups * group_bytes;
    measurement.report(config, "write", DEPTH);
    for &qd in &config.qds {
        backend.prepare_reads(&sizes, qd);
        read_phase(&mut backend, config, &sizes, count, qd, true);
        read_phase(&mut backend, config, &sizes, count, qd, false);
    }
    backend.finish();
}

fn main() {
    let config = Config::parse();
    match config.engine.as_str() {
        "legacy" => run(legacy::Legacy::new(&config), &config),
        "v2" => run(unified::Unified::new(&config), &config),
        _ => unreachable!(),
    }
}
