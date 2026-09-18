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

//! Persistence and ownership contracts of the transitional engine adapter.
use std::sync::Arc;

use moat_common::{ChunkId, HugePages, PoolOptions};
use moat_server::storage::{
    self, Completion, Disk, FormatOptions, FrameLimits, MemDevice, Options, QueueBackend, QueueOptions, Session,
};

fn setup() -> (Disk, QueueOptions) {
    let device = Arc::new(MemDevice::new(8 << 20));
    storage::format(
        &*device,
        &FormatOptions {
            sync_mode: Default::default(),
            device_id: [42; 16],
            segment_size: 1 << 20,
            limits: FrameLimits::new(128 << 10, 64 << 10).unwrap(),
        },
    )
    .unwrap();
    let disk = Disk::open(
        device,
        Options {
            sync_mode: Default::default(),
            index_capacity: 16,
            verify_reads: true,
        },
    )
    .unwrap();
    let queue = QueueOptions {
        depth: 4,
        pool: PoolOptions {
            bytes: 2 << 20,
            max_class: 128 << 10,
            huge_pages: HugePages::Disabled,
        },
    };
    (disk, queue)
}
fn drain(session: &mut Session) {
    let mut completions = Vec::new();
    while session.in_flight() != 0 {
        session.poll(true, &mut completions).unwrap();
    }
    for completion in completions {
        match completion {
            Completion::Failed { error, .. } => panic!("{error}"),
            Completion::Write { result, .. } | Completion::Flush { result, .. } => result.unwrap(),
            Completion::Read { result, .. } => {
                result.unwrap();
            }
        }
    }
}
fn write(session: &mut Session, id: ChunkId, value: Option<&[u8]>) -> (moat_engine::pipeline::Ticket, u64) {
    loop {
        match session.write(id, value) {
            Ok(accepted) => return accepted,
            Err(storage::Error::Busy) => drain(session),
            Err(error) => panic!("{error}"),
        }
    }
}
#[test]
fn empty_and_recovered_flushes_allocate_no_segment_and_lsn_includes_tombstones() {
    let (disk, queue) = setup();
    let mut first = Session::open(disk.clone(), &queue, QueueBackend::Sync).unwrap();
    first.flush().unwrap();
    assert!(matches!(
        first.write(ChunkId::from_u128(1), Some(b"deferred")),
        Err(storage::Error::Busy)
    ));
    drain(&mut first);
    assert_eq!(disk.usage().free_segments, disk.usage().segments);
    let id = ChunkId::from_u128(1);
    let (_, written) = write(&mut first, id, Some(b"old"));
    assert_eq!(written, 1, "rejected writes must not consume LSNs");
    drain(&mut first);
    let (_, deleted) = write(&mut first, id, None);
    drain(&mut first);
    assert!(deleted > written);
    first.seal().unwrap();
    drop(first);
    let free = disk.usage().free_segments;
    let mut second = Session::open(disk.clone(), &queue, QueueBackend::Sync).unwrap();
    second.flush().unwrap();
    drain(&mut second);
    assert_eq!(disk.usage().free_segments, free);
    assert!(second.stat(&id).is_none());
    let (_, next) = write(&mut second, ChunkId::from_u128(2), Some(b"new"));
    assert!(next > deleted);
    drain(&mut second);
    second.seal().unwrap();
    drop(second);
    let third = Session::open(disk, &queue, QueueBackend::Sync).unwrap();
    assert_eq!(third.stat(&ChunkId::from_u128(2)), Some((next, 3)));
}
#[test]
fn ownership_is_exclusive_and_failed_setup_releases_the_lease() {
    let (disk, queue) = setup();
    let mut bad = queue.clone();
    bad.depth = 0;
    assert!(Session::open(disk.clone(), &bad, QueueBackend::Sync).is_err());
    let first = Session::open(disk.clone(), &queue, QueueBackend::Sync).unwrap();
    assert!(matches!(
        Session::open(disk.clone(), &queue, QueueBackend::Sync),
        Err(storage::Error::Busy)
    ));
    drop(first);
    Session::open(disk, &queue, QueueBackend::Sync).unwrap();
}
#[test]
fn full_append_only_device_keeps_reads_available() {
    let (disk, queue) = setup();
    let mut session = Session::open(disk.clone(), &queue, QueueBackend::Sync).unwrap();
    let value = vec![0x37; 60 << 10];
    let mut filled = false;
    let id = ChunkId::from_u128(1);
    for _ in 0..1000 {
        match session.write(id, Some(&value)) {
            Ok(_) | Err(storage::Error::Busy) => drain(&mut session),
            Err(storage::Error::Engine(moat_engine::engine::Error::OutOfSpace)) => {
                filled = true;
                break;
            }
            Err(error) => panic!("unexpected admission error: {error}"),
        }
    }
    assert!(filled);
    assert_eq!(disk.usage().free_segments, 0);
    session.read(id, None).unwrap();
    let mut completions = Vec::new();
    while session.in_flight() != 0 {
        session.poll(true, &mut completions).unwrap();
    }
    assert!(matches!(&completions[..], [Completion::Read { result: Ok(_), .. }]));
    for completion in completions {
        if let Completion::Read { buffers, result, .. } = completion {
            assert_eq!(buffers.view(result.unwrap()), value);
        }
    }
}
