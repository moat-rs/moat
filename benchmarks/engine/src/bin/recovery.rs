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

//! Full-extent mixed-record fill and fresh-process recovery measurement.
//! The operator runner must hold exclusive device locks across all phases.

use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{FileTypeExt, OpenOptionsExt},
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use moat_common::{BufferPool, ChunkId, HugePages, PoolOptions};
use moat_engine_v2::{
    engine::{self, Device, Engine, Error, FormatOptions},
    frame::{FrameBuilder, FrameLimits},
    io::{Buffer, UringQueue},
    pipeline::{self, Completion, ReadBuffers},
};
use serde_json::{Value, json};

const SIZES: [usize; 5] = [100, 1024, 4096, 65536, 4 << 20];
const FRAME: usize = 8 << 20;
const DEPTH: usize = 32;
type BenchEngine = Engine<Window, UringQueue>;

#[derive(Default)]
struct Counters {
    calls: Cell<u64>,
    bytes: Cell<u64>,
}
struct Window {
    file: File,
    bytes: u64,
    counters: Rc<Counters>,
}
impl Device for Window {
    fn capacity(&self) -> io::Result<u64> {
        Ok(self.bytes)
    }
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        assert!(offset <= self.bytes && bytes.len() as u64 <= self.bytes - offset);
        Device::read_at(&self.file, bytes, offset)?;
        self.counters.calls.set(self.counters.calls.get() + 1);
        self.counters.bytes.set(self.counters.bytes.get() + bytes.len() as u64);
        Ok(())
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        assert!(offset <= self.bytes && bytes.len() as u64 <= self.bytes - offset);
        Device::write_at(&self.file, bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

fn monotonic() -> f64 {
    // SAFETY: clock_gettime initializes the writable timespec.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: ts is valid writable storage for CLOCK_MONOTONIC.
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) }, 0);
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}
fn usage() -> Value {
    // SAFETY: getrusage initializes the writable structure.
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: r is valid writable storage.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) }, 0);
    json!({"user": r.ru_utime.tv_sec as f64 + r.ru_utime.tv_usec as f64 / 1e6,
        "system": r.ru_stime.tv_sec as f64 + r.ru_stime.tv_usec as f64 / 1e6,
        "max_rss_kib": r.ru_maxrss})
}
fn pin(cpu: usize) {
    assert!(cpu < libc::CPU_SETSIZE as usize);
    // SAFETY: zero is a valid empty CPU set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: cpu is in range and set is valid; pid zero addresses this process.
    unsafe {
        libc::CPU_SET(cpu, &mut set);
        assert_eq!(libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set), 0);
    }
}
fn key(n: u64) -> ChunkId {
    ChunkId::from_u128(n as u128)
}
fn templates() -> Vec<Vec<u8>> {
    SIZES
        .iter()
        .enumerate()
        .map(|(i, &len)| {
            let mut state = 0x1234_5678_9abc_def0_u64 ^ i as u64;
            (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect()
        })
        .collect()
}
fn poll(engine: &mut BenchEngine, buffers: &mut Vec<Buffer>, acked: &mut u64, wait: bool) {
    let mut out = Vec::new();
    engine.poll(wait, &mut out).unwrap();
    for completion in out {
        match completion {
            Completion::Write { result, buffer, .. } => {
                result.unwrap();
                buffers.push(buffer);
                *acked += 1;
            }
            Completion::Flush { result, .. } => result.unwrap(),
            _ => panic!("unexpected completion"),
        }
    }
}
fn fill(mut engine: BenchEngine, pool: &std::sync::Arc<BufferPool>, disk: usize) {
    let limits = engine.layout().limits();
    let expected = engine.layout().capacity() / SIZES.iter().sum::<usize>() as u64 * 5;
    engine.reserve_index(expected as usize + 64).unwrap();
    let mut values = templates();
    let mut buffers: Vec<Buffer> = (0..DEPTH).map(|_| pool.alloc(FRAME).unwrap().into()).collect();
    let mut frames = 0_u64;
    let mut acked = 0;
    let mut bytes = 0_u64;
    let start = Instant::now();
    let mut progress = start;
    loop {
        let first = frames * 5;
        for (i, v) in values.iter_mut().enumerate() {
            v[..8].copy_from_slice(&(first + i as u64).to_le_bytes());
        }
        let mut frame = FrameBuilder::new(limits);
        for (i, v) in values.iter().enumerate() {
            frame.push(key(first + i as u64), first + i as u64 + 1, v).unwrap();
        }
        while buffers.is_empty() {
            poll(&mut engine, &mut buffers, &mut acked, true);
        }
        let mut buffer = buffers.pop().unwrap();
        let full = loop {
            match engine.write(&frame, buffer) {
                Ok(_) => break false,
                Err(rejected) => {
                    buffer = rejected.input;
                    if matches!(rejected.error, Error::OutOfSpace) {
                        buffers.push(buffer);
                        break true;
                    }
                    assert!(
                        matches!(rejected.error, Error::Pipeline(pipeline::Error::Backpressure)),
                        "{}",
                        rejected.error
                    );
                    poll(&mut engine, &mut buffers, &mut acked, true);
                }
            }
        };
        if full {
            break;
        }
        frames += 1;
        bytes += SIZES.iter().sum::<usize>() as u64;
        poll(&mut engine, &mut buffers, &mut acked, false);
        if progress.elapsed().as_secs() >= 30 {
            println!(
                "PROGRESS {}",
                json!({"disk":disk,"seconds":start.elapsed().as_secs_f64(),"payload_bytes":bytes,
                "records":frames*5,"allocated_segments":engine.allocated_segments(),"segments":engine.layout().segment_count()})
            );
            progress = Instant::now();
        }
    }
    while engine.in_flight() > 0 {
        poll(&mut engine, &mut buffers, &mut acked, true);
    }
    assert_eq!(frames, acked);
    engine.flush().unwrap();
    while engine.in_flight() > 0 {
        poll(&mut engine, &mut buffers, &mut acked, true);
    }
    engine.seal().unwrap();
    assert_eq!(engine.allocated_segments(), engine.layout().segment_count() as usize);
    assert_eq!(engine.indexed_versions() as u64, frames * 5);
    println!(
        "FILLED {}",
        json!({"disk":disk,"seconds":start.elapsed().as_secs_f64(),"payload_bytes":bytes,
        "records":frames*5,"frames":frames,"segments":engine.allocated_segments(),"usage":usage()})
    );
}

fn verify(engine: &mut BenchEngine, pool: &std::sync::Arc<BufferPool>, records: u64, disk: usize) {
    assert!(records >= 5 && records.is_multiple_of(5));
    let values = templates();
    let mut checks = 0;
    // Evenly spaced groups include every size, the beginning, and the final frame.
    for group in 0..101_u64 {
        let first = (records / 5 - 1) * group / 100 * 5;
        for (i, template) in values.iter().enumerate() {
            let n = first + i as u64;
            let req = engine
                .read_requirements(key(n), 0..template.len() as u32, true)
                .unwrap();
            engine
                .read(
                    key(n),
                    0..template.len() as u32,
                    true,
                    ReadBuffers {
                        metadata: Some(pool.alloc(req.metadata_len).unwrap().into()),
                        value: pool.alloc(req.value_len).unwrap().into(),
                    },
                )
                .map_err(|r| r.error)
                .unwrap();
            let mut out = Vec::new();
            while out.is_empty() {
                engine.poll(true, &mut out).unwrap();
            }
            assert_eq!(out.len(), 1);
            match out.pop().unwrap() {
                Completion::Read { result, buffers, .. } => {
                    let bytes = buffers.view(result.unwrap());
                    assert_eq!(&bytes[..8], &n.to_le_bytes());
                    assert_eq!(&bytes[8..], &template[8..]);
                }
                _ => panic!("unexpected completion"),
            }
            checks += 1;
        }
    }
    assert!(!engine.contains(&key(records)));
    println!("VERIFIED {}", json!({"disk":disk,"samples":checks}));
}

fn validate(c: &Value, disk: usize) {
    assert_eq!(
        fs::read_to_string("/proc/sys/kernel/hostname").unwrap().trim(),
        c["host"].as_str().unwrap()
    );
    let d = &c["disks"][disk];
    let path = fs::canonicalize(d["path"].as_str().unwrap()).unwrap();
    let meta = fs::metadata(&path).unwrap();
    let capacity = d["expected_capacity"].as_u64().unwrap();
    if meta.file_type().is_block_device() {
        let sys = Path::new("/sys/class/block").join(path.file_name().unwrap());
        let serial = fs::read_to_string(sys.join("device/serial")).unwrap();
        assert!(!serial.trim().is_empty());
        assert_eq!(serial.trim(), d["serial"].as_str().unwrap());
        let forbidden = c["forbidden_serials"].as_array().unwrap();
        assert!(!forbidden.is_empty() && !forbidden.iter().any(|s| s.as_str() == Some(serial.trim())));
        assert_eq!(
            fs::read_to_string(sys.join("size"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap()
                * 512,
            capacity
        );
        assert!(!sys.join("partition").exists());
        assert!(fs::read_dir(sys.join("holders")).unwrap().next().is_none());
        assert!(
            fs::read_dir(&sys)
                .unwrap()
                .all(|p| !p.unwrap().path().join("partition").exists())
        );
        let dev = fs::read_to_string(sys.join("dev")).unwrap();
        assert!(
            fs::read_to_string("/proc/self/mountinfo")
                .unwrap()
                .lines()
                .all(|l| l.split_whitespace().nth(2) != Some(dev.trim()))
        );
    } else {
        assert!(meta.is_file());
        assert_eq!(d["serial"].as_str().unwrap(), "");
        assert_eq!(meta.len(), capacity);
    }
}

fn main() {
    // A worker failure must end immediately; the runner terminates its siblings.
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 5, "usage: recovery CONFIG DISK fill|recover GO_FILE");
    let c: Value = serde_json::from_slice(&fs::read(&args[1]).unwrap()).unwrap();
    let disk: usize = args[2].parse().unwrap();
    let fill_mode = match args[3].as_str() {
        "fill" => true,
        "recover" => false,
        _ => panic!("invalid mode"),
    };
    validate(&c, disk);
    pin(c["io_cpus"][disk].as_u64().unwrap() as usize);
    let file = OpenOptions::new()
        .read(true)
        .write(fill_mode)
        .custom_flags(libc::O_DIRECT)
        .open(c["disks"][disk]["path"].as_str().unwrap())
        .unwrap();
    let counters = Rc::new(Counters::default());
    let device = Window {
        file: file.try_clone().unwrap(),
        bytes: c["disks"][disk]["expected_capacity"].as_u64().unwrap(),
        counters: counters.clone(),
    };
    if fill_mode {
        let mut id = [0; 16];
        File::open("/dev/urandom").unwrap().read_exact(&mut id).unwrap();
        engine::format(
            &device,
            FormatOptions {
                device_id: id,
                segment_size: c["segment_bytes"].as_u64().unwrap().try_into().unwrap(),
                limits: FrameLimits::new(FRAME as u32, SIZES[4] as u32).unwrap(),
            },
        )
        .unwrap();
    }
    let pool = BufferPool::new(PoolOptions {
        bytes: c["pool_bytes"].as_u64().unwrap() as usize,
        max_class: FRAME,
        huge_pages: HugePages::Preferred,
    })
    .unwrap();
    let queue = UringQueue::with_pool(file, 256, pool.clone()).unwrap();
    println!(
        "READY {}",
        json!({"disk":disk,"deferred":queue.deferred_taskrun(),"usage":usage()})
    );
    while !Path::new(&args[4]).exists() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let deadline: f64 = fs::read_to_string(&args[4]).unwrap().trim().parse().unwrap();
    while monotonic() < deadline {
        std::thread::sleep(Duration::from_micros(100));
    }
    counters.calls.set(0);
    counters.bytes.set(0);
    let cpu_start = usage();
    let start = monotonic();
    let mut engine = Engine::open(device, queue).unwrap();
    let end = monotonic();
    if fill_mode {
        fill(engine, &pool, disk);
    } else {
        let cpu_end = usage();
        let records = c["records"][disk].as_u64().unwrap();
        assert_eq!(engine.indexed_versions() as u64, records);
        assert_eq!(engine.allocated_segments(), engine.layout().segment_count() as usize);
        println!(
            "RECOVERED {}",
            json!({"disk":disk,"seconds":end-start,"start":start,"end":end,
            "start_delay_seconds":start-deadline,"records":records,"segments":engine.allocated_segments(),
            "read_calls":counters.calls.get(),"read_bytes":counters.bytes.get(),
            "cpu_before":cpu_start,"cpu_after":cpu_end})
        );
        // Wait until all devices are recovered before introducing validation I/O.
        let verify_go = format!("{}.verify", args[4]);
        while !Path::new(&verify_go).exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        verify(&mut engine, &pool, records, disk);
    }
}
