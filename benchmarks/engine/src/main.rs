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

//! Destructive comparison of the legacy engine and the v2 device engine.

mod legacy;
mod memory;
mod unified;

use std::{
    collections::HashMap,
    fs,
    ops::Range,
    path::PathBuf,
    time::{Duration, Instant},
};

use moat_common::{ChunkId, HugePages, PoolOptions};
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
    capacity: u64,
    whole_device: bool,
    seconds: u64,
    qds: Vec<usize>,
    disk_stat: Option<PathBuf>,
    verify: bool,
    huge_pages: HugePages,
    range: Option<Range<u32>>,
}

impl Config {
    fn parse() -> Self {
        let args: Vec<_> = std::env::args().collect();
        assert!(
            args.len() >= 8 && (args.len() - 8).is_multiple_of(2),
            "usage: moat-engine-compare PATH legacy|v2 SIZE|mixed PAYLOAD_MIB SECONDS QDS --overwrite-first-4g|--overwrite-entire-device [--verify true|false] [--range full|START:END] [--huge-pages disabled|preferred|required]"
        );
        assert!(matches!(
            args[7].as_str(),
            "--overwrite-first-4g" | "--overwrite-entire-device"
        ));
        assert!(matches!(args[2].as_str(), "legacy" | "v2"));
        let path = fs::canonicalize(&args[1]).unwrap();
        let stat = PathBuf::from("/sys/class/block")
            .join(path.file_name().unwrap())
            .join("stat");
        let whole_device = args[7] == "--overwrite-entire-device";
        let capacity = if whole_device {
            use std::io::{Seek, SeekFrom};
            fs::File::open(&path).unwrap().seek(SeekFrom::End(0)).unwrap()
        } else {
            CAPACITY
        };
        let mut config = Self {
            path,
            capacity,
            whole_device,
            engine: args[2].clone(),
            workload: args[3].clone(),
            bytes: args[4].parse::<u64>().unwrap().checked_mul(1 << 20).unwrap(),
            seconds: args[5].parse().unwrap(),
            qds: args[6].split(',').map(|s| s.parse().unwrap()).collect(),
            disk_stat: stat.exists().then_some(stat),
            verify: false,
            huge_pages: HugePages::Preferred,
            range: None,
        };
        for option in args[8..].as_chunks::<2>().0 {
            match option[0].as_str() {
                "--huge-pages" => {
                    config.huge_pages = match option[1].as_str() {
                        "disabled" => HugePages::Disabled,
                        "preferred" => HugePages::Preferred,
                        "required" => HugePages::Required,
                        _ => panic!("huge-pages must be disabled, preferred, or required"),
                    }
                }
                "--verify" => config.verify = option[1].parse().expect("verify must be true or false"),
                "--range" if option[1] == "full" => config.range = None,
                "--range" => {
                    let (start, end) = option[1].split_once(':').expect("range must be START:END");
                    config.range = Some(start.parse().unwrap()..end.parse().unwrap());
                }
                _ => panic!("unknown option: {}", option[0]),
            }
        }
        for size in config.sizes() {
            let range = config.read_range(size);
            assert!(
                range.start < range.end && range.end <= size as u32,
                "range must fit every value"
            );
        }
        if whole_device {
            // Full distinct-key workloads require a feasible resident index.
            // Do not silently turn a tiny-record run into a repeated-key workload.
            assert!(
                config.sizes().iter().all(|&size| size >= 65536),
                "full distinct-key mode requires values >= 64 KiB; use a separately labeled distributed workload for smaller values"
            );
            config.bytes = capacity;
        } else {
            assert!((1..=512 << 20).contains(&config.bytes));
        }
        assert!(config.seconds > 0 && config.qds.iter().all(|&qd| (1..=DEPTH).contains(&qd)));
        config
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn pool_options(&self) -> PoolOptions {
        PoolOptions {
            bytes: 1 << 30,
            max_class: 8 << 20,
            huge_pages: self.huge_pages,
        }
    }

    fn read_range(&self, len: usize) -> Range<u32> {
        self.range.clone().unwrap_or(0..len as u32)
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
    fn memory(&self) -> serde_json::Value;
    fn write_batch(&mut self, records: &[Record]) -> usize;
    fn seal(&mut self);
    fn usage(&self) -> serde_json::Value;
    fn flush(&mut self, records: u64);
    fn prepare_reads(&mut self, config: &Config, sizes: &[usize], qd: usize);
    fn read(&mut self, number: u64, range: Range<u32>) -> u64;
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

    fn report(mut self, config: &Config, phase: &str, qd: usize, backend: &impl Backend) {
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
                "scope": if config.whole_device { "whole-device" } else { "first-4g" },
                "device_capacity": config.capacity, "usage": backend.usage(),
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
                "memory": backend.memory(), "huge_pages": format!("{:?}", config.huge_pages),
                "verified_reads": config.verify, "durable_flush": true,
                "read_range": config.range.as_ref().map_or_else(|| "full".to_owned(), |r| format!("{}:{}", r.start, r.end)),
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
            let range = config.read_range(len);
            let ticket = backend.read(number, range.clone());
            assert!(pending.insert(ticket, (number, range, started)).is_none());
            issued += 1;
        }
        if pending.is_empty() {
            break;
        }
        backend.poll_reads(|ticket, data| {
            let (number, range, started) = pending.remove(&ticket).expect("unknown completion");
            assert_eq!(data.len(), range.len());
            // Sample both ends without scanning the payload in the timed callback.
            // The key is checked only when the requested range includes its prefix.
            let expected = |offset: usize| {
                if offset < 8 {
                    number.to_le_bytes()[offset]
                } else {
                    (offset % 253) as u8
                }
            };
            for (i, &byte) in data.iter().take(8).enumerate() {
                assert_eq!(byte, expected(range.start as usize + i));
            }
            assert_eq!(data[data.len() - 1], expected(range.end as usize - 1));
            if let Some(started) = started {
                measurement.latencies.push(started.elapsed().as_nanos() as u64);
            }
            measurement.completed += 1;
            measurement.bytes += data.len() as u64;
        });
    }
    assert_eq!(measurement.completed, issued);
    if !warmup {
        measurement.report(config, "read", qd, backend);
    }
}

fn run(mut backend: impl Backend, config: &Config) {
    let sizes = config.sizes();
    // Mixed groups retain input order. Uniform large records use prepared I/O.
    let batch = if sizes.len() == 1 && sizes[0] >= 65536 { 16 } else { 64 };
    let group_bytes: u64 = (0..batch).map(|i| sizes[i % sizes.len()] as u64).sum();
    let groups = if config.whole_device {
        u64::MAX / batch as u64
    } else {
        (config.bytes / group_bytes).max(1)
    };
    let mut count = 0u64;
    let mut records: Vec<_> = (0..batch)
        .map(|i| Record {
            number: 0,
            value: (0..sizes[i % sizes.len()]).map(|j| (j % 253) as u8).collect(),
        })
        .collect();
    let mut measurement = Measurement::new(config);
    let mut progress = Instant::now();
    for group in 0..groups {
        for (i, record) in records.iter_mut().enumerate() {
            record.number = group * batch as u64 + i as u64;
            record.value[..8].copy_from_slice(&record.number.to_le_bytes());
        }
        let written = backend.write_batch(&records);
        count += written as u64;
        measurement.bytes += records[..written].iter().map(|r| r.value.len() as u64).sum::<u64>();
        if config.whole_device && progress.elapsed() >= Duration::from_secs(30) {
            eprintln!(
                "{}",
                json!({"phase":"fill-progress", "records":count,
                "payload_bytes":measurement.bytes,"seconds":measurement.start.elapsed().as_secs_f64(),
                "usage":backend.usage()})
            );
            progress = Instant::now();
        }
        if written != records.len() {
            assert!(config.whole_device, "bounded dataset did not fit");
            break;
        }
    }
    backend.flush(count);
    if config.whole_device {
        backend.seal();
    }
    assert!(count > 0, "device did not fit any records");
    if config.whole_device {
        let usage = backend.usage();
        assert_eq!(
            usage["segments"], usage["allocated_segments"],
            "fill stopped before all segments were allocated"
        );
    }
    measurement.completed = count;
    measurement.report(config, "write", DEPTH, &backend);
    for &qd in &config.qds {
        backend.prepare_reads(config, &sizes, qd);
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
