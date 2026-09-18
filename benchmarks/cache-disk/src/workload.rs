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

use crate::{
    Config, Report, cpu, diskstats,
    engines::{self, Backend, Done, Put, Record},
};
use anyhow::{Context, Result, ensure};
use hdrhistogram::Histogram;
use moat_cache::{
    Bytes,
    identity::{DEFAULT_IDENTITY_VERSION, Fingerprint, Xxh3},
};
use moat_server::{Placement, Target};
use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

// Keep generation/encoding bounded so large writes overlap outstanding I/O.
const ENCODE_BYTES: usize = 4 << 20;
const ENCODE_RECORDS: usize = 64;

struct Inputs {
    count: usize,
    spare: Option<Vec<Put>>,
}
impl Inputs {
    fn new(c: &Config, records: &[Record], batch: usize) -> Self {
        let count = ENCODE_RECORDS
            .min(ENCODE_BYTES.div_ceil(c.key_bytes + c.value_bytes))
            .min(batch)
            .min(records.len());
        // Construct and touch the source buffers on their pinned owner before
        // timing. Foyer takes ownership of values and cannot recycle them here.
        let spare = (c.engine == "moat" && c.moat_input_pool)
            .then(|| records.iter().take(count).map(|r| Put::new(c, r)).collect());
        Self { count, spare }
    }
    fn take(&mut self, c: &Config, record: &Record) -> Put {
        if let Some(spare) = &mut self.spare {
            let mut input = spare.pop().expect("accepted source buffers were recycled");
            input.reset(c, record);
            input
        } else {
            Put::new(c, record)
        }
    }
    fn recycle(&mut self, pending: &mut VecDeque<Put>, count: usize) {
        let accepted = pending.drain(..count);
        if let Some(spare) = &mut self.spare {
            spare.extend(accepted);
        }
        // In owned mode the drain drops inputs; foyer has already moved values.
    }
}

struct Stats {
    operations: u64,
    latency: Histogram<u64>,
}
impl Default for Stats {
    fn default() -> Self {
        Self {
            operations: 0,
            latency: Histogram::new(3).unwrap(),
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Write,
    Verify,
    Read {
        clients: usize,
        seconds: u64,
        repeat: usize,
    },
    Close,
}
struct Worker {
    commands: mpsc::Sender<Phase>,
    results: mpsc::Receiver<Result<Stats>>,
}
struct Workers {
    workers: Vec<Worker>,
    threads: Vec<thread::JoinHandle<()>>,
}
impl Workers {
    fn new(c: &Config, records: Vec<Vec<Record>>) -> Result<Self> {
        // Runtime construction is exclusive to foyer. Engine runs never enter
        // a runtime and never send requests or completions through a channel.
        let runtime = (c.engine == "foyer")
            .then(|| engines::foyer::Runtime::new(c.runtime_cpus.clone()))
            .transpose()?;
        let mut workers = Self {
            workers: Vec::new(),
            threads: Vec::new(),
        };
        for (disk, records) in records.into_iter().enumerate() {
            let (commands, receiver) = mpsc::channel();
            let (results, replies) = mpsc::channel();
            let config = c.clone();
            let runtime = runtime.clone();
            let thread = thread::Builder::new()
                .name(format!("disk-bench-{disk}"))
                .spawn(move || {
                    let start = || -> Result<()> {
                        let core = if config.engine == "foyer" {
                            config.runtime_cpus[disk % config.runtime_cpus.len()]
                        } else {
                            config.io_cpus[disk]
                        };
                        moat_server::worker::pin_to_core(core)?;
                        match config.engine.as_str() {
                            "moat" => worker(
                                engines::moat::Moat::new(&config, disk)?,
                                &config,
                                disk,
                                &records,
                                receiver,
                                &results,
                            ),
                            "foyer" => worker(
                                engines::foyer::Foyer::new(&config, disk, runtime.unwrap())?,
                                &config,
                                disk,
                                &records,
                                receiver,
                                &results,
                            ),
                            _ => unreachable!("validated engine"),
                        }
                    };
                    if let Err(error) = start() {
                        let _ = results.send(Err(error));
                    }
                })?;
            workers.workers.push(Worker {
                commands,
                results: replies,
            });
            workers.threads.push(thread);
            workers
                .workers
                .last()
                .unwrap()
                .results
                .recv()
                .context("worker startup")??;
        }
        Ok(workers)
    }
    // Only phase transitions cross threads. Include their small dispatch cost
    // in the measurement instead of adding a barrier that can strand workers.
    fn start(&self, phase: Phase) -> Result<()> {
        for worker in &self.workers {
            worker.commands.send(phase).context("worker stopped")?;
        }
        Ok(())
    }
    fn finish(&self) -> Result<Stats> {
        let mut total = Stats::default();
        for worker in &self.workers {
            let stats = worker.results.recv().context("worker stopped")??;
            total.operations += stats.operations;
            total.latency.add(stats.latency)?;
        }
        Ok(total)
    }
    fn phase(&self, phase: Phase) -> Result<Stats> {
        self.start(phase)?;
        self.finish()
    }
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.workers.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn share(total: usize, disks: usize, disk: usize) -> usize {
    total / disks + usize::from(disk < total % disks)
}
fn worker(
    mut backend: impl Backend,
    c: &Config,
    disk: usize,
    records: &[Record],
    commands: mpsc::Receiver<Phase>,
    results: &mpsc::Sender<Result<Stats>>,
) -> Result<()> {
    let batch = share(c.prefill_batch, c.disks.len(), disk);
    let mut inputs = Inputs::new(c, records, batch);
    let pooled = inputs.spare.as_ref().map_or(0, Vec::len);
    println!(
        "INPUT {}",
        serde_json::json!({"disk":disk, "pooled_buffers":pooled, "pooled_payload_bytes":pooled * c.value_bytes})
    );
    results
        .send(Ok(Stats::default()))
        .map_err(|_| anyhow::anyhow!("coordinator stopped"))?;
    for phase in commands {
        let stats = match phase {
            Phase::Write => {
                let stats = prefill(&mut backend, c, records, batch, &mut inputs)?;
                if c.engine != "moat" || c.moat_sync {
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&c.disks[disk].path)?
                        .sync_data()?;
                }
                stats
            }
            Phase::Verify => reads(&mut backend, records, c.value_bytes, 128, None, 1)?,
            Phase::Read {
                clients,
                seconds,
                repeat,
            } => reads(
                &mut backend,
                records,
                c.value_bytes,
                share(clients, c.disks.len(), disk),
                Some(Duration::from_secs(seconds)),
                ((repeat * c.disks.len() + disk + 1) as u64).wrapping_mul(0x9e3779b97f4a7c15),
            )?,
            Phase::Close => {
                backend.close()?;
                let _ = results.send(Ok(Stats::default()));
                return Ok(());
            }
        };
        if results.send(Ok(stats)).is_err() {
            break;
        }
    }
    Ok(())
}

fn prefill(
    backend: &mut impl Backend,
    c: &Config,
    records: &[Record],
    batch: usize,
    inputs: &mut Inputs,
) -> Result<Stats> {
    let mut pending = VecDeque::with_capacity(ENCODE_RECORDS);
    let mut writing = HashMap::new();
    let mut out = Vec::with_capacity(256);
    let mut stats = Stats::default();
    for chunk in records.chunks(batch) {
        let mut next = 0;
        while next < chunk.len() || !pending.is_empty() || !writing.is_empty() {
            if pending.is_empty() {
                let end = (next + inputs.count).min(chunk.len());
                pending.extend(chunk[next..end].iter().map(|r| inputs.take(c, r)));
                next = end;
            }
            let mut bytes = 0;
            while !pending.is_empty() {
                let first_len = pending[0].len();
                let Some((ticket, count)) = backend.put(&mut pending)? else {
                    break;
                };
                ensure!(count > 0 && count <= pending.len(), "invalid write batch");
                // Uniform record sizes. Foyer moves the value out on admission.
                bytes += count * first_len;
                inputs.recycle(&mut pending, count);
                ensure!(writing.insert(ticket, count).is_none(), "duplicate write ticket");
                if bytes >= ENCODE_BYTES {
                    break;
                }
            }
            backend.poll(&mut out)?;
            for done in out.drain(..) {
                let Done::Write(ticket, result) = done else {
                    anyhow::bail!("read during prefill")
                };
                result?;
                stats.operations += writing.remove(&ticket).context("unknown write ticket")? as u64;
            }
        }
        // Foyer's insertion completion means admission; wait for disk flushes.
        backend.drain()?;
    }
    ensure!(stats.operations as usize == records.len(), "incomplete prefill");
    Ok(stats)
}

struct Request {
    record: usize,
    start: Instant,
}
fn reads(
    backend: &mut impl Backend,
    records: &[Record],
    len: usize,
    depth: usize,
    duration: Option<Duration>,
    mut rng: u64,
) -> Result<Stats> {
    let deadline = duration.map(|duration| Instant::now() + duration);
    let mut next = 0;
    let mut pending = VecDeque::with_capacity(depth);
    let mut reading = HashMap::<u64, Request>::with_capacity(depth);
    let mut out = Vec::with_capacity(depth);
    let mut stats = Stats::default();
    loop {
        while pending.len() + reading.len() < depth {
            let start = Instant::now();
            let record = if let Some(deadline) = deadline {
                if start >= deadline {
                    break;
                }
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (rng as usize) % records.len()
            } else {
                if next == records.len() {
                    break;
                }
                let record = next;
                next += 1;
                record
            };
            pending.push_back(Request { record, start });
        }
        // Count only I/O actually submitted before the deadline and drain it.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            pending.clear();
        }
        while let Some(request) = pending.front() {
            let Some(ticket) = backend.read(&records[request.record])? else {
                break;
            };
            ensure!(
                reading.insert(ticket, pending.pop_front().unwrap()).is_none(),
                "duplicate read ticket"
            );
        }
        if pending.is_empty() && reading.is_empty() {
            break;
        }
        backend.poll(&mut out)?;
        for done in out.drain(..) {
            let Done::Read(ticket, result) = done else {
                anyhow::bail!("write during reads")
            };
            let request = reading.remove(&ticket).context("unknown read ticket")?;
            result?.check(&records[request.record], len)?;
            if duration.is_some() {
                stats.latency.record(request.start.elapsed().as_nanos().max(1) as u64)?;
            }
            stats.operations += 1;
        }
    }
    Ok(stats)
}

fn records(c: &Config) -> Result<Vec<Vec<Record>>> {
    let placement = Placement::new(
        (0..c.disks.len())
            .map(|i| Target {
                uuid: [i as u8 + 1; 16],
                weight: c.bytes_per_disk,
            })
            .collect(),
    );
    let mut disks: Vec<Vec<Record>> = (0..c.disks.len())
        .map(|_| Vec::with_capacity(c.records_per_disk))
        .collect();
    for number in 0..c.records_per_disk * c.disks.len() {
        let mut key = vec![0x5a; c.key_bytes];
        key[..8].copy_from_slice(&(number as u64).to_le_bytes());
        let id = Xxh3.identify(&[0; 16], DEFAULT_IDENTITY_VERSION, &key);
        disks[placement.disk_of(&id).unwrap()].push(Record {
            id,
            key: Bytes::from(key),
            number,
        });
    }
    ensure!(disks.iter().all(|records| !records.is_empty()), "empty disk dataset");
    Ok(disks)
}

pub(super) fn run(c: Config) -> Result<()> {
    println!("CONFIG {}", c.public_summary());
    println!(
        "DRIVER {}",
        serde_json::json!({"mode":"native-poll", "workers":c.disks.len(), "tokio":c.engine == "foyer", "routing":"before timing", "concurrency":"fixed per disk", "input":if c.engine == "moat" && c.moat_input_pool { "pooled" } else { "owned" }, "input_alignment":if c.engine == "moat" && c.moat_input_pool && c.key_bytes + c.value_bytes >= 65536 { "page" } else { "natural" }})
    );
    let workers = Workers::new(&c, records(&c)?)?;
    let before_disk = diskstats(&c)?;
    let before_cpu = cpu();
    let begin = Instant::now();
    workers.start(Phase::Write)?;
    let stats = workers.finish()?;
    let elapsed = begin.elapsed().as_secs_f64();
    let after_cpu = cpu();
    let after_disk = diskstats(&c)?;
    println!(
        "PREFILL {}",
        serde_json::json!({"operations":stats.operations, "seconds":elapsed,
        "ops_per_second":stats.operations as f64/elapsed,"logical_bytes_per_second":stats.operations as f64*c.value_bytes as f64/elapsed,
        "cpu_cores":(after_cpu.user+after_cpu.system-before_cpu.user-before_cpu.system)/elapsed,
        "disk_before":before_disk,"disk_after":after_disk})
    );
    let verified = workers.phase(Phase::Verify)?.operations;
    ensure!(
        verified as usize == c.records_per_disk * c.disks.len(),
        "incomplete verification"
    );
    println!("VERIFIED {verified}");
    let levels = if c.client_levels.is_empty() {
        vec![c.clients]
    } else {
        c.client_levels.clone()
    };
    for clients in levels {
        workers.phase(Phase::Read {
            clients,
            seconds: 2,
            repeat: 0,
        })?;
        for repeat in 1..=c.repeats {
            let before_disk = diskstats(&c)?;
            let before_cpu = cpu();
            let begin = Instant::now();
            workers.start(Phase::Read {
                clients,
                seconds: c.seconds,
                repeat,
            })?;
            let stats = workers.finish()?;
            let elapsed = begin.elapsed().as_secs_f64();
            let after_cpu = cpu();
            let after_disk = diskstats(&c)?;
            let cpu_seconds = after_cpu.user + after_cpu.system - before_cpu.user - before_cpu.system;
            let report = Report {
                phase: "read".into(),
                repeat,
                clients,
                operations: stats.operations,
                seconds: elapsed,
                ops_per_second: stats.operations as f64 / elapsed,
                logical_bytes_per_second: stats.operations as f64 * c.value_bytes as f64 / elapsed,
                p50_us: stats.latency.value_at_quantile(0.5) as f64 / 1000.0,
                p99_us: stats.latency.value_at_quantile(0.99) as f64 / 1000.0,
                p999_us: stats.latency.value_at_quantile(0.999) as f64 / 1000.0,
                cpu_seconds,
                cpu_cores: cpu_seconds / elapsed,
                max_rss_kib: after_cpu.max_rss_kib,
                disk_delta: after_disk
                    .into_iter()
                    .zip(before_disk)
                    .map(|(a, b)| a.into_iter().zip(b).map(|(a, b)| a - b).collect())
                    .collect(),
            };
            println!("RESULT {}", serde_json::to_string(&report)?);
        }
    }
    workers.phase(Phase::Close)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::Data;

    #[derive(Default)]
    struct Fake {
        next: u64,
        pending: Vec<Done>,
        written: usize,
        seen: Vec<usize>,
        busy: usize,
        drains: usize,
        corrupt: bool,
    }
    impl Fake {
        fn ticket(&mut self) -> Option<u64> {
            if self.pending.len() == 2 {
                self.busy += 1;
                return None;
            }
            self.next += 1;
            Some(self.next)
        }
    }
    impl Backend for Fake {
        fn put(&mut self, batch: &mut VecDeque<Put>) -> Result<Option<(u64, usize)>> {
            let Some(ticket) = self.ticket() else { return Ok(None) };
            let count = batch.len().min(3);
            self.written += count;
            self.pending.push(Done::Write(ticket, Ok(())));
            Ok(Some((ticket, count)))
        }
        fn read(&mut self, record: &Record) -> Result<Option<u64>> {
            let Some(ticket) = self.ticket() else { return Ok(None) };
            assert!(record.number < self.written);
            self.seen.push(record.number);
            let mut data = record.key.to_vec();
            data.resize(data.len() + 100, 0x7c);
            crate::stamp_value(&mut data[record.key.len()..], record.number);
            if self.corrupt {
                data[0] ^= 1;
            }
            self.pending.push(Done::Read(ticket, Ok(Data::Test(data))));
            Ok(Some(ticket))
        }
        fn poll(&mut self, out: &mut Vec<Done>) -> Result<()> {
            // Complete batches out of order to exercise ticket accounting.
            out.extend(self.pending.drain(..).rev());
            Ok(())
        }
        fn drain(&mut self) -> Result<()> {
            assert!(self.pending.is_empty());
            self.drains += 1;
            Ok(())
        }
        fn close(self) -> Result<()> {
            Ok(())
        }
    }
    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "host":"unused", "engine":"moat", "disks":[{"path":"unused","serial":""}],
            "bytes_per_disk":1073741824_u64, "records_per_disk":43, "key_bytes":16, "value_bytes":100,
            "clients":17, "runtime_cpus":[], "io_cpus":[0], "seconds":1, "repeats":1,
            "pool_bytes_per_disk":67108864
        }))
        .unwrap()
    }
    #[test]
    fn public_configuration_omits_operator_identifiers() {
        let mut c = config();
        c.host = "private-host-marker".into();
        c.forbidden_serials = vec!["private-exclusion-marker".into()];
        c.disks[0].path = "/private/device-marker".into();
        c.disks[0].serial = "private-serial-marker".into();
        c.disks[0].expected_capacity = Some(987654321);
        c.io_cpus = vec![12345];
        c.runtime_cpus = vec![23456, 34567];
        let summary = c.public_summary();
        let text = summary.to_string();
        for private in ["private-", "987654321", "12345", "23456", "34567"] {
            assert!(!text.contains(private));
        }
        assert_eq!(summary["disks"], serde_json::json!([0]));
        assert_eq!(summary["io_workers"], 1);
        assert_eq!(summary["runtime_workers"], 2);
        assert_eq!(summary["value_bytes"], 100);
    }

    #[test]
    fn batches_and_reads_survive_backpressure_and_reordering() -> Result<()> {
        for pooled in [false, true] {
            let mut c = config();
            c.moat_input_pool = pooled;
            let records = records(&c)?.pop().unwrap();
            let mut backend = Fake::default();
            let mut inputs = Inputs::new(&c, &records, 13);
            assert_eq!(prefill(&mut backend, &c, &records, 13, &mut inputs)?.operations, 43);
            assert_eq!(inputs.spare.as_ref().map(Vec::len), pooled.then_some(13));
            assert_eq!(backend.drains, 4);
            assert_eq!(reads(&mut backend, &records, 100, 17, None, 1)?.operations, 43);
            backend.seen.sort_unstable();
            assert_eq!(backend.seen, (0..43).collect::<Vec<_>>());
            assert!(backend.busy > 0);
            assert!(backend.pending.is_empty());
        }
        Ok(())
    }
    #[test]
    fn source_pool_is_bounded_and_survives_repeated_prefill() -> Result<()> {
        for (value_bytes, count) in [(65536, 64), (4 << 20, 1)] {
            let mut c = config();
            c.value_bytes = value_bytes;
            c.records_per_disk = 137;
            assert!(c.moat_input_pool);
            let records = records(&c)?.pop().unwrap();
            let mut inputs = Inputs::new(&c, &records, 71);
            assert_eq!(inputs.spare.as_ref().unwrap().len(), count);
            for _ in 0..2 {
                let mut backend = Fake::default();
                assert_eq!(prefill(&mut backend, &c, &records, 71, &mut inputs)?.operations, 137);
                assert_eq!(inputs.spare.as_ref().unwrap().len(), count);
            }
            c.engine = "foyer".into();
            assert!(Inputs::new(&c, &records, 71).spare.is_none());
        }
        Ok(())
    }
    #[test]
    fn corrupt_reads_fail_the_workload() -> Result<()> {
        let c = config();
        let records = records(&c)?.pop().unwrap();
        let mut backend = Fake {
            written: 43,
            corrupt: true,
            ..Default::default()
        };
        let error = reads(&mut backend, &records, 100, 17, None, 1).err().unwrap();
        assert!(error.to_string().contains("full key mismatch"));
        Ok(())
    }
    #[test]
    fn quotas_preserve_the_total_for_uneven_concurrency() {
        for total in [20, 31, 160, 641, 2560] {
            let parts: Vec<_> = (0..20).map(|disk| share(total, 20, disk)).collect();
            assert_eq!(parts.iter().sum::<usize>(), total);
            assert!(parts.iter().max().unwrap() - parts.iter().min().unwrap() <= 1);
        }
    }
}
