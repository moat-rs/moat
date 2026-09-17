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

//! Matched disk workloads driven by synchronous, per-device polling loops.

mod engines;
mod workload;

use std::{
    fs,
    hash::{BuildHasherDefault, DefaultHasher},
    os::unix::fs::FileTypeExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

type Hasher = BuildHasherDefault<DefaultHasher>;
const SEGMENT: u64 = 16 << 20;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    path: String,
    serial: String,
    #[serde(default)]
    expected_capacity: Option<u64>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    host: String,
    #[serde(default)]
    forbidden_serials: Vec<String>,
    engine: String,
    disks: Vec<Disk>,
    bytes_per_disk: u64,
    records_per_disk: usize,
    key_bytes: usize,
    value_bytes: usize,
    #[serde(default)]
    client_levels: Vec<usize>,
    clients: usize,
    runtime_cpus: Vec<usize>,
    io_cpus: Vec<usize>,
    seconds: u64,
    repeats: usize,
    pool_bytes_per_disk: usize,
    #[serde(default)]
    moat_verify_reads: bool,
    #[serde(default)]
    moat_huge_pages: bool,
    #[serde(default = "engine_segment_bytes")]
    engine_segment_bytes: u64,
    #[serde(default = "prefill_batch")]
    prefill_batch: usize,
}

impl Config {
    // Explicit allowlist: operator host, device, and CPU identities stay private.
    fn public_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": self.engine,
            "disks": (0..self.disks.len()).collect::<Vec<_>>(),
            "bytes_per_disk": self.bytes_per_disk,
            "records_per_disk": self.records_per_disk,
            "key_bytes": self.key_bytes,
            "value_bytes": self.value_bytes,
            "client_levels": self.client_levels,
            "clients": self.clients,
            "runtime_workers": self.runtime_cpus.len(),
            "io_workers": self.io_cpus.len(),
            "seconds": self.seconds,
            "repeats": self.repeats,
            "pool_bytes_per_disk": self.pool_bytes_per_disk,
            "moat_verify_reads": self.moat_verify_reads,
            "moat_huge_pages": self.moat_huge_pages,
            "engine_segment_bytes": self.engine_segment_bytes,
            "prefill_batch": self.prefill_batch,
        })
    }
}

fn validate(config: &Config) -> Result<()> {
    ensure!(
        fs::read_to_string("/proc/sys/kernel/hostname")?.trim() == config.host,
        "wrong benchmark host"
    );
    ensure!(
        !config.disks.is_empty() && config.io_cpus.len() == config.disks.len(),
        "invalid disk/CPU list"
    );
    ensure!(
        config.clients >= config.disks.len() && config.repeats > 0 && config.seconds > 0,
        "empty runtime/workload"
    );
    ensure!(
        config.key_bytes >= 8 && config.value_bytes >= 16 && config.records_per_disk > 0,
        "invalid record size"
    );
    ensure!(config.bytes_per_disk.is_multiple_of(SEGMENT), "unaligned device window");
    ensure!(matches!(config.engine.as_str(), "v2" | "foyer"), "unknown engine");
    ensure!(
        config.engine != "foyer" || !config.runtime_cpus.is_empty(),
        "foyer needs runtime CPUs"
    );
    ensure!(
        config.prefill_batch >= config.disks.len(),
        "prefill budget is smaller than disk count"
    );
    for &clients in &config.client_levels {
        ensure!(
            clients >= config.disks.len() && clients <= config.clients,
            "invalid client level"
        );
    }
    ensure!(
        config.engine_segment_bytes >= 16 << 20
            && config.engine_segment_bytes <= u32::MAX as u64
            && config.engine_segment_bytes.is_multiple_of(4096),
        "invalid engine segment size"
    );
    ensure!(
        config
            .key_bytes
            .checked_add(config.value_bytes)
            .is_some_and(|len| len <= (4 << 20) + 4096),
        "entry exceeds engine value bound"
    );
    let mut seen = std::collections::HashSet::new();
    for disk in &config.disks {
        let path = fs::canonicalize(&disk.path)?;
        ensure!(seen.insert(path.clone()), "duplicate device");
        if fs::metadata(&path)?.file_type().is_block_device() {
            let name = path
                .file_name()
                .context("device filename")?
                .to_str()
                .context("device UTF-8")?;
            let sys = Path::new("/sys/class/block").join(name);
            ensure!(!disk.serial.is_empty(), "raw device needs a serial allowlist");
            ensure!(
                fs::read_to_string(sys.join("device/serial"))?.trim() == disk.serial,
                "device serial changed"
            );
            ensure!(
                !config.forbidden_serials.is_empty(),
                "raw devices require an explicit system-device serial denylist"
            );
            ensure!(
                !config.forbidden_serials.contains(&disk.serial),
                "forbidden system device"
            );
            ensure!(!sys.join("partition").exists(), "partition is forbidden");
            ensure!(
                fs::read_dir(sys.join("holders"))?.next().is_none(),
                "device has holders"
            );
            for child in fs::read_dir(&sys)? {
                ensure!(!child?.path().join("partition").exists(), "device has partitions");
            }
            let dev = fs::read_to_string(sys.join("dev"))?;
            for mount in fs::read_to_string("/proc/self/mountinfo")?.lines() {
                ensure!(mount.split_whitespace().nth(2) != Some(dev.trim()), "mounted device");
            }
            let capacity = fs::read_to_string(sys.join("size"))?.trim().parse::<u64>()? * 512;
            ensure!(
                capacity >= config.bytes_per_disk && disk.expected_capacity == Some(capacity),
                "device capacity does not match the allowlist"
            );
        } else {
            ensure!(
                disk.serial.is_empty() && fs::metadata(&path)?.is_file(),
                "invalid benchmark file"
            );
            ensure!(
                fs::metadata(&path)?.len() >= config.bytes_per_disk,
                "benchmark file too small"
            );
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct Cpu {
    user: f64,
    system: f64,
    max_rss_kib: i64,
}
fn cpu() -> Cpu {
    // SAFETY: getrusage initializes the provided writable rusage structure.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: usage is valid writable storage for the RUSAGE_SELF query.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
    Cpu {
        user: usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1e6,
        system: usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1e6,
        max_rss_kib: usage.ru_maxrss,
    }
}
fn diskstats(c: &Config) -> Result<Vec<Vec<u64>>> {
    c.disks
        .iter()
        .map(|d| {
            let name = Path::new(&d.path).file_name().unwrap();
            let path = Path::new("/sys/class/block").join(name).join("stat");
            if !path.exists() {
                return Ok(vec![0; 17]);
            }
            fs::read_to_string(path)?
                .split_whitespace()
                .map(|v| Ok(v.parse()?))
                .collect()
        })
        .collect()
}
#[derive(Serialize)]
struct Report {
    phase: String,
    repeat: usize,
    clients: usize,
    operations: u64,
    seconds: f64,
    ops_per_second: f64,
    logical_bytes_per_second: f64,
    p50_us: f64,
    p99_us: f64,
    p999_us: f64,
    cpu_seconds: f64,
    cpu_cores: f64,
    max_rss_kib: i64,
    disk_delta: Vec<Vec<u64>>,
}
fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: moat-cache-disk-compare CONFIG.json")?;
    let config: Config = serde_json::from_slice(&fs::read(path)?)?;
    validate(&config)?;
    moat_server::worker::pin_to_core(config.runtime_cpus.first().copied().unwrap_or(config.io_cpus[0]))?;
    workload::run(config)
}

fn fresh_identity() -> Result<[u8; 16]> {
    use std::io::Read;
    let mut bytes = [0; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn stamp_value(value: &mut [u8], number: usize) {
    value[..8].copy_from_slice(&(number as u64).to_le_bytes());
    let len = value.len();
    value[len - 8..].copy_from_slice(&(!(number as u64)).to_le_bytes());
}

fn engine_segment_bytes() -> u64 {
    2 << 30
}
fn prefill_batch() -> usize {
    256
}
