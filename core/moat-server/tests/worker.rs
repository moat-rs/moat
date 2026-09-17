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

//! Owner-only v2 workers, shutdown sealing and recovery.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use moat_common::{ChunkId, HugePages, PoolOptions};
use moat_server::{
    Context, Handler, Node, PollMode, QueueBackend, Step, WorkerOptions,
    storage::{self, Completion, Device, Error, FormatOptions, FrameLimits, MemDevice, Options, QueueOptions, Session},
};

const DISKS: usize = 3;
const WORKERS: usize = 4;
const KEYS_PER_DISK: u64 = 200;

fn devices() -> Vec<Arc<dyn Device>> {
    (0..DISKS)
        .map(|d| {
            let dev = MemDevice::new(8 << 20);
            storage::format(
                &dev,
                &FormatOptions {
                    segment_size: 1 << 20,
                    limits: FrameLimits::new(128 << 10, 64 << 10).unwrap(),
                    device_id: [d as u8 + 1; 16],
                },
            )
            .unwrap();
            Arc::new(dev) as Arc<dyn Device>
        })
        .collect()
}

fn worker_options() -> WorkerOptions {
    WorkerOptions {
        core: None,
        queue: QueueOptions {
            depth: 64,
            pool: PoolOptions {
                bytes: 8 << 20,
                max_class: 1 << 20,
                huge_pages: HugePages::Disabled,
            },
        },
        // MemDevice needs Sync on Linux; exercise the default fallback elsewhere.
        backend: if cfg!(target_os = "linux") {
            QueueBackend::Sync
        } else {
            WorkerOptions::default().backend
        },
        poll_mode: PollMode::Adaptive {
            idle_sleep: std::time::Duration::from_micros(50),
        },
    }
}

fn key(disk: usize, i: u64) -> ChunkId {
    ChunkId::from_u128(((disk as u128) << 64) | i as u128)
}

fn value(disk: usize, i: u64) -> Vec<u8> {
    let len = 100 + (i as usize * 37) % 20_000;
    (0..len).map(|b| (b as u64 * 31 + i + disk as u64) as u8).collect()
}

/// Phase 1: every owner writes its disks' keys and waits for the tickets.
/// Phase 2: each owner reads every key of its own disks. Then stop.
struct Load {
    writes_pending: HashMap<(usize, u64), (usize, u64)>,
    written: bool,
    reads_pending: HashMap<(usize, u64), (usize, u64)>,
    next_read: usize,
    reads_done: u64,
    all_written: Arc<AtomicUsize>,
    reads_total: Arc<AtomicUsize>,
    owners: usize,
}

impl Handler for Load {
    fn run(&mut self, cx: &mut Context<'_>) -> Step {
        for (disk, completion) in cx.completions.drain(..) {
            match completion {
                Completion::Write { ticket, result, .. } => {
                    self.writes_pending
                        .remove(&(disk, ticket.number()))
                        .expect("known write");
                    result.unwrap();
                }
                Completion::Read {
                    ticket,
                    result,
                    buffers,
                } => {
                    let (d, i) = self.reads_pending.remove(&(disk, ticket.number())).expect("known read");
                    assert_eq!(d, disk);
                    assert_eq!(buffers.view(result.unwrap()), value(disk, i));
                    self.reads_done += 1;
                }
                Completion::Flush { .. } => unreachable!(),
            }
        }
        if !self.written {
            // Write everything the worker owns, respecting back-pressure.
            let mut issued_all = true;
            for disk in 0..DISKS {
                if !cx.owns(disk) {
                    continue;
                }
                let session = cx.disk(disk).unwrap();
                for i in 0..KEYS_PER_DISK {
                    if self.writes_pending.values().any(|&(d, k)| d == disk && k == i)
                        || session.stat(&key(disk, i)).is_some()
                    {
                        continue;
                    }
                    match session.write(key(disk, i), Some(&value(disk, i))) {
                        Ok((ticket, _)) => {
                            self.writes_pending.insert((disk, ticket.number()), (disk, i));
                        }
                        Err(Error::Busy) => {
                            issued_all = false;
                            break;
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
            }
            if issued_all && self.writes_pending.is_empty() {
                self.written = true;
                if !cx.disks.is_empty() {
                    self.all_written.fetch_add(1, Ordering::AcqRel);
                }
            }
            return Step::Continue;
        }
        if self.all_written.load(Ordering::Acquire) < self.owners {
            return Step::Idle;
        }
        // Each request runs on the owner of its disk.
        let total = DISKS * KEYS_PER_DISK as usize;
        while self.next_read < total && self.reads_pending.len() < 8 {
            let disk = self.next_read / KEYS_PER_DISK as usize;
            let i = (self.next_read % KEYS_PER_DISK as usize) as u64;
            let Some(session) = cx.disk(disk) else {
                self.next_read += 1;
                continue;
            };
            match session.read(key(disk, i), None) {
                Ok(ticket) => {
                    self.reads_pending.insert((disk, ticket.number()), (disk, i));
                    self.next_read += 1;
                }
                Err(Error::Busy) => break,
                Err(e) => panic!("{e}"),
            }
        }
        if self.next_read == total && self.reads_pending.is_empty() {
            self.reads_total.fetch_add(self.reads_done as usize, Ordering::AcqRel);
            return Step::Stop;
        }
        // All available reads have been issued; adaptive mode may wait for I/O.
        Step::Idle
    }
}

#[test]
fn owners_write_and_read_their_disks_and_reopened_sessions_recover() {
    let devices = devices();
    let mut node = Node::open(
        devices.clone(),
        Options {
            index_capacity: 1024,
            ..Default::default()
        },
    )
    .unwrap();
    node.assign_owners(WORKERS, &[None; DISKS], &[None; WORKERS]);
    let owners: std::collections::HashSet<usize> = node.owners().iter().copied().collect();
    assert_eq!(
        owners.len(),
        DISKS,
        "each disk gets a distinct owner with more workers than disks"
    );

    let all_written = Arc::new(AtomicUsize::new(0));
    let reads_total = Arc::new(AtomicUsize::new(0));
    let workers = node
        .start(&vec![worker_options(); WORKERS], |_| Load {
            writes_pending: HashMap::new(),
            written: false,
            reads_pending: HashMap::new(),
            next_read: 0,
            reads_done: 0,
            all_written: all_written.clone(),
            reads_total: reads_total.clone(),
            owners: owners.len(),
        })
        .unwrap();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(reads_total.load(Ordering::Acquire), DISKS * KEYS_PER_DISK as usize);
    drop(node);
    let node = Node::open(devices, Options::default()).unwrap();
    for (disk, handle) in node.engines().iter().enumerate() {
        let session = Session::open(handle.clone(), &worker_options().queue, QueueBackend::Sync).unwrap();
        let mut count = 0;
        session.visit(|id, _, len| {
            assert!(len > 0);
            assert!(session.stat(&id).is_some());
            count += 1;
        });
        assert_eq!(count, KEYS_PER_DISK, "disk {disk}");
    }
    // Placement is deterministic and covers every disk.
    let mut hits = vec![0usize; DISKS];
    for i in 0..3_000u128 {
        hits[node.disk_of(&ChunkId::from_u128(i))] += 1;
    }
    assert!(hits.iter().all(|&h| h > 500), "{hits:?}");
}

struct Idle;
impl Handler for Idle {
    fn run(&mut self, _: &mut Context<'_>) -> Step {
        Step::Idle
    }
}

#[test]
fn invalid_topologies_fail_before_starting_workers() {
    let list = devices();
    assert!(matches!(
        Node::open(Vec::new(), Options::default()),
        Err(moat_server::NodeError::NoDisks)
    ));
    assert!(matches!(
        Node::open(vec![list[0].clone(), list[0].clone()], Options::default()),
        Err(moat_server::NodeError::DuplicateIdentity { .. })
    ));
    let mut node = Node::open(list, Options::default()).unwrap();
    node.set_owners(vec![0, 1, 2]);
    assert!(matches!(
        node.start(&[worker_options()], |_| Idle),
        Err(moat_server::NodeError::InvalidOwners)
    ));
    node.assign_owners(2, &[Some(1), Some(0), Some(1)], &[Some(0), Some(1)]);
    assert_eq!(node.owners(), &[1, 0, 1]);
}

#[test]
fn partial_worker_startup_failure_releases_all_earlier_sessions() {
    let mut node = Node::open(devices(), Options::default()).unwrap();
    node.set_owners(vec![0, 1, 1]);
    let options = worker_options();
    let held = Session::open(node.engines()[2].clone(), &options.queue, options.backend).unwrap();
    assert!(node.start(&[options.clone(), options.clone()], |_| Idle).is_err());
    for disk in &node.engines()[..2] {
        Session::open(disk.clone(), &options.queue, options.backend).unwrap();
    }
    drop(held);
    let workers = node.start(&[options.clone(), options], |_| Idle).unwrap();
    for worker in &workers {
        worker.stop();
    }
    for worker in workers {
        worker.join().unwrap();
    }
}
