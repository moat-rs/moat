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

//! Multi-segment routing, restart, and lifecycle failure tests on small files.
#![cfg(unix)]

use moat_common::{AlignedBuf, ChunkId, PAGE_SIZE};
use moat_engine_v2::{
    engine::{self, Device, Engine, Error, FormatOptions, Layout},
    frame::{FrameBuilder, FrameLimits, PreparedFrame},
    io::{FileQueue, Queue},
    pipeline::{self, Completion, ReadBuffers},
    segment::SegmentHeader,
};
use std::{
    cell::{Cell, RefCell},
    fs::File,
    io,
    os::unix::fs::FileExt,
    rc::Rc,
};

const PAGE: usize = PAGE_SIZE as usize;
const STRIDE: u32 = 64 * 1024;
type Store = Engine<Disk, FileQueue>;

#[derive(Clone)]
struct Disk {
    file: Rc<File>,
    fail_at: Rc<Cell<usize>>,
    calls: Rc<Cell<usize>>,
    log: Rc<RefCell<Vec<(bool, u64)>>>,
}
impl Disk {
    fn before(&self, write: bool, offset: u64) -> io::Result<()> {
        self.log.borrow_mut().push((write, offset));
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n == self.fail_at.get() {
            return Err(io::Error::other("injected lifecycle failure"));
        }
        Ok(())
    }
    fn fail(&self, at: usize) {
        self.calls.set(0);
        self.fail_at.set(at);
        self.log.borrow_mut().clear();
    }
    fn open(&self) -> engine::Result<Store> {
        Engine::open(self.clone(), FileQueue::new(self.file.try_clone().unwrap(), 4).unwrap())
    }
}
impl Device for Disk {
    fn capacity(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(bytes, offset)
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        self.before(true, offset)?;
        self.file.write_all_at(bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.before(false, 0)?;
        self.file.sync_data()
    }
}
fn options(id: u8) -> FormatOptions {
    FormatOptions {
        device_id: [id; 16],
        segment_size: STRIDE,
        limits: FrameLimits::new(32768, 16384).unwrap(),
    }
}
fn disk(segments: u64) -> Disk {
    let file = tempfile::tempfile().unwrap();
    file.set_len(2 * PAGE_SIZE + segments * STRIDE as u64).unwrap();
    let disk = Disk {
        file: Rc::new(file),
        fail_at: Rc::new(Cell::new(usize::MAX)),
        calls: Rc::new(Cell::new(0)),
        log: Rc::new(RefCell::new(Vec::new())),
    };
    engine::format(&disk, options(7)).unwrap();
    disk
}
fn key(id: u128) -> ChunkId {
    ChunkId::from_u128(id)
}
fn drain<D: Device, Q: Queue>(store: &mut Engine<D, Q>) -> Vec<Completion> {
    let mut out = Vec::new();
    while store.in_flight() != 0 {
        store.poll(true, &mut out).unwrap();
    }
    for c in &out {
        match c {
            Completion::Write { result, .. } | Completion::Flush { result, .. } => {
                assert!(result.is_ok(), "{result:?}")
            }
            Completion::Read { result, .. } => assert!(result.is_ok(), "{result:?}"),
        }
    }
    out
}
fn put(store: &mut Store, id: u128, lsn: u64, value: Option<&[u8]>) {
    let mut frame = FrameBuilder::new(store.layout().limits());
    match value {
        Some(value) => frame.push(key(id), lsn, value).unwrap(),
        None => frame.push_tombstone(key(id), lsn).unwrap(),
    }
    let mut buffer = AlignedBuf::zeroed(frame.encoded_len()).into();
    loop {
        match store.write(&frame, buffer) {
            Ok(_) => break,
            Err(r) if matches!(r.error, Error::Pipeline(pipeline::Error::Backpressure)) => {
                buffer = r.input;
                drain(store);
            }
            Err(r) => panic!("{}", r.error),
        }
    }
    drain(store);
}
fn get<D: Device, Q: Queue>(store: &mut Engine<D, Q>, id: u128, len: u32, verify: bool) -> Vec<u8> {
    let requirements = store.read_requirements(key(id), 0..len, verify).unwrap();
    let buffers = ReadBuffers {
        metadata: verify.then(|| AlignedBuf::zeroed(requirements.metadata_len).into()),
        value: AlignedBuf::zeroed(requirements.value_len.max(PAGE)).into(),
    };
    store.read(key(id), 0..len, verify, buffers).unwrap();
    match drain(store).pop().unwrap() {
        Completion::Read { result, buffers, .. } => buffers.view(result.unwrap()).to_vec(),
        _ => panic!("expected read"),
    }
}

#[test]
fn rollover_routes_old_and_new_records_and_reopens_all_segments() {
    let disk = disk(4);
    let mut store = disk.open().unwrap();
    for id in 0..20 {
        put(&mut store, id, id as u64 + 1, Some(&vec![id as u8; 4096]));
    }
    assert!(store.allocated_segments() > 1);
    for verify in [false, true] {
        for id in 0..20 {
            assert_eq!(get(&mut store, id, 4096, verify), vec![id as u8; 4096]);
        }
    }
    store.seal().unwrap();
    drop(store);
    let mut store = disk.open().unwrap();
    for verify in [false, true] {
        for id in 0..20 {
            assert_eq!(get(&mut store, id, 4096, verify), vec![id as u8; 4096]);
        }
    }
}

#[test]
fn caller_selection_and_lsn_order_survive_physical_recovery_order() {
    let disk = disk(4);
    let mut store = disk.open().unwrap();
    store.rollover_to(3).unwrap();
    put(&mut store, 1, 30, Some(b"new"));
    put(&mut store, 2, 30, None);
    store.rollover_to(0).unwrap();
    put(&mut store, 1, 10, Some(b"old"));
    put(&mut store, 2, 10, Some(b"old"));
    store.seal().unwrap();
    drop(store);
    let mut store = disk.open().unwrap();
    assert_eq!(get(&mut store, 1, 3, true), b"new");
    assert!(!store.contains(&key(2)));
    assert!(matches!(store.rollover_to(3), Err(Error::InvalidArgument(_))));
}

#[test]
fn restart_never_reuses_an_active_tail_and_old_frames_keep_their_routes() {
    let disk = disk(3);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"first"));
    store.flush().unwrap();
    drain(&mut store);
    drop(store);
    let mut store = disk.open().unwrap();
    put(&mut store, 2, 2, Some(b"later"));
    assert_eq!(store.active_segment(), Some(1));
    assert_eq!(get(&mut store, 1, 5, true), b"first");
    assert_eq!(get(&mut store, 2, 5, false), b"later");
}

#[test]
fn full_device_returns_prepared_buffer_and_remains_readable() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    let len = 16384;
    let limits = store.layout().limits();
    for id in 0..2 {
        put(&mut store, id, id as u64 + 1, Some(&vec![id as u8; len]));
    }
    let mut buffer = AlignedBuf::zeroed(PreparedFrame::required_len(limits, len as u32).unwrap());
    let pointer = buffer.as_ptr();
    PreparedFrame::new(limits, len as u32, &mut buffer)
        .unwrap()
        .value_mut()
        .fill(0x59);
    let rejected = store.write_prepared(key(3), 3, len as u32, buffer).unwrap_err();
    assert!(matches!(rejected.error, Error::OutOfSpace));
    assert_eq!(rejected.input.as_ptr(), pointer);
    let mut buffer = rejected.input;
    assert!(
        PreparedFrame::new(limits, len as u32, &mut buffer)
            .unwrap()
            .value_mut()
            .iter()
            .all(|&b| b == 0x59)
    );
    assert_eq!(get(&mut store, 1, len as u32, true), vec![1; len]);
}

#[test]
fn rollover_waits_for_delivery_and_preserves_a_pending_read_snapshot() {
    let disk = disk(3);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"old"));
    store
        .read(key(1), 0..3, false, ReadBuffers::new(AlignedBuf::zeroed(PAGE)))
        .unwrap();
    assert!(matches!(
        store.rollover(),
        Err(Error::Pipeline(pipeline::Error::Backpressure))
    ));
    let out = drain(&mut store);
    match &out[0] {
        Completion::Read {
            result: Ok(range),
            buffers,
            ..
        } => assert_eq!(buffers.view(range.clone()), b"old"),
        _ => panic!("read"),
    }
    store.rollover().unwrap();
    put(&mut store, 1, 2, Some(b"new"));
    assert_eq!(get(&mut store, 1, 3, true), b"new");
}

#[test]
fn seal_order_never_overwrites_the_allocation_header() {
    let disk = disk(2);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"data"));
    let base = store.layout().segment_base(0).unwrap();
    let mut before = vec![0; PAGE];
    disk.read_at(&mut before, base).unwrap();
    disk.fail(usize::MAX);
    store.seal().unwrap();
    assert_eq!(
        &*disk.log.borrow(),
        &[
            (true, base + 2 * PAGE_SIZE),
            (false, 0),
            (true, base + STRIDE as u64 - PAGE_SIZE),
            (false, 0)
        ]
    );
    let mut after = vec![0; PAGE];
    disk.read_at(&mut after, base).unwrap();
    assert_eq!(before, after);
}

#[test]
fn every_seal_failure_stops_writes_and_preserves_previously_flushed_data() {
    for failure in 1..=4 {
        let disk = disk(3);
        let mut store = disk.open().unwrap();
        put(&mut store, 1, 1, Some(b"durable"));
        store.flush().unwrap();
        drain(&mut store);
        disk.fail(failure);
        assert!(matches!(store.seal(), Err(Error::Io(_))));
        assert!(matches!(store.rollover(), Err(Error::Failed)));
        assert_eq!(get(&mut store, 1, 7, false), b"durable");
        drop(store);
        disk.fail(usize::MAX);
        let mut reopened = disk.open().unwrap();
        assert_eq!(get(&mut reopened, 1, 7, true), b"durable");
        put(&mut reopened, 2, 2, Some(b"next"));
        assert_eq!(reopened.active_segment(), Some(1));
    }
}

#[test]
fn torn_seal_and_bad_footer_recover_without_rewriting_headers() {
    for damage in ["seal", "footer"] {
        let disk = disk(2);
        let mut store = disk.open().unwrap();
        put(&mut store, 1, 1, Some(b"recover"));
        store.seal().unwrap();
        let layout = store.layout();
        drop(store);
        let base = layout.segment_base(0).unwrap();
        let offset = if damage == "seal" {
            base + STRIDE as u64 - PAGE_SIZE + 17
        } else {
            base + 2 * PAGE_SIZE + 17
        };
        disk.file.write_all_at(&[0xff], offset).unwrap();
        let mut store = disk.open().unwrap();
        assert_eq!(get(&mut store, 1, 7, true), b"recover");
    }
}

#[test]
fn sealed_damage_remains_an_error_when_footer_fallback_scans_frames() {
    let disk = disk(2);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"data"));
    store.seal().unwrap();
    let base = store.layout().segment_base(0).unwrap();
    drop(store);
    disk.file.write_all_at(&[0xff], base + 2 * PAGE_SIZE + 17).unwrap();
    disk.file.write_all_at(&[0xff], base + PAGE_SIZE + 17).unwrap();
    assert!(matches!(disk.open(), Err(Error::Segment(_))));
}

#[test]
fn superblock_copy_recovers_and_conflicting_valid_copies_fail() {
    let disk = disk(2);
    let expected = Layout::read(&disk).unwrap();
    disk.file.write_all_at(&[0xff], 17).unwrap();
    assert_eq!(Layout::read(&disk).unwrap(), expected);
    engine::format(&disk, options(8)).unwrap();
    let mut alternate = vec![0; PAGE];
    disk.read_at(&mut alternate, 0).unwrap();
    engine::format(&disk, options(9)).unwrap();
    disk.file.write_all_at(&alternate, PAGE_SIZE).unwrap();
    assert!(matches!(Layout::read(&disk), Err(Error::Corrupt(_))));
}

#[test]
fn reformat_changes_epoch_so_unwritten_old_frames_cannot_resurrect() {
    let disk = disk(2);
    let mut store = disk.open().unwrap();
    for id in 0..4 {
        put(&mut store, id, id as u64 + 1, Some(b"stale"));
    }
    store.flush().unwrap();
    drain(&mut store);
    drop(store);
    engine::format(&disk, options(8)).unwrap();
    let mut store = disk.open().unwrap();
    put(&mut store, 99, 1, Some(b"fresh"));
    store.flush().unwrap();
    drain(&mut store);
    drop(store);
    let mut store = disk.open().unwrap();
    assert_eq!(get(&mut store, 99, 5, true), b"fresh");
    for id in 0..4 {
        assert!(!store.contains(&key(id)));
    }
}

#[test]
fn fresh_allocation_header_must_be_durable_before_frames_are_submitted() {
    for failure in [1, 2] {
        let disk = disk(2);
        let mut store = disk.open().unwrap();
        disk.fail(failure);
        let mut frame = FrameBuilder::new(store.layout().limits());
        frame.push(key(1), 1, b"new").unwrap();
        let rejected = store
            .write(&frame, AlignedBuf::zeroed(frame.encoded_len()))
            .unwrap_err();
        assert!(matches!(rejected.error, Error::Io(_)));
        assert_eq!(store.in_flight(), 0);
        assert!(!store.contains(&key(1)));
        assert!(matches!(store.rollover(), Err(Error::Failed)));
    }
}

#[test]
fn sealed_header_is_stored_outside_the_logical_segment_extent() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"x"));
    store.seal().unwrap();
    let layout = store.layout();
    let base = layout.segment_base(0).unwrap();
    let mut page = vec![0; PAGE];
    disk.read_at(&mut page, base + STRIDE as u64 - PAGE_SIZE).unwrap();
    let header = SegmentHeader::decode(&page, layout.device_id(), 0, STRIDE - PAGE as u32).unwrap();
    assert!(header.is_sealed());
    assert!(header.footer_range().unwrap().end <= header.segment_len());
}

#[cfg(target_os = "linux")]
#[test]
fn registered_prepared_io_rolls_over_and_reopens_with_both_read_policies() {
    use moat_common::{BufferPool, HugePages, PoolOptions};
    use moat_engine_v2::io::UringQueue;

    let disk = disk(4);
    let pool = BufferPool::new(PoolOptions {
        bytes: 128 * 1024,
        max_class: 32 * 1024,
        huge_pages: HugePages::Disabled,
    })
    .unwrap();
    let queue = UringQueue::with_pool(disk.file.try_clone().unwrap(), 4, pool.clone()).unwrap();
    let mut store = Engine::open(disk.clone(), queue).unwrap();
    let limits = store.layout().limits();
    let mut buffer: moat_engine_v2::io::Buffer = pool
        .alloc(PreparedFrame::required_len(limits, 4096).unwrap())
        .unwrap()
        .into();
    for id in 0..20 {
        PreparedFrame::new(limits, 4096, &mut buffer)
            .unwrap()
            .value_mut()
            .fill(id as u8);
        store.write_prepared(key(id), id as u64 + 1, 4096, buffer).unwrap();
        buffer = match drain(&mut store).pop().unwrap() {
            Completion::Write { buffer, .. } => buffer,
            _ => panic!("expected write"),
        };
    }
    assert!(store.allocated_segments() > 1);
    store.flush().unwrap();
    drain(&mut store);
    store.seal().unwrap();
    drop(store);
    let queue = UringQueue::with_pool(disk.file.try_clone().unwrap(), 4, pool).unwrap();
    let mut store = Engine::open(disk, queue).unwrap();
    for verify in [false, true] {
        for id in 0..20 {
            assert_eq!(get(&mut store, id, 4096, verify), vec![id as u8; 4096]);
        }
    }
}

#[test]
fn interrupted_format_never_commits_partial_allocation_reset() {
    // Two roots, barrier, two header resets per slot, barrier, then each root
    // and its barrier. Fault injection fails before the selected operation.
    for failure in 1..=12 {
        let disk = disk(2);
        let mut store = disk.open().unwrap();
        put(&mut store, 1, 1, Some(b"old"));
        store.seal().unwrap();
        drop(store);
        disk.fail(failure);
        assert!(matches!(engine::format(&disk, options(8)), Err(Error::Io(_))));
        disk.fail(usize::MAX);
        match disk.open() {
            Ok(store) => {
                if store.layout().device_id() == [7; 16] {
                    assert!(store.contains(&key(1)));
                } else {
                    assert_eq!(store.allocated_segments(), 0);
                    assert!(!store.contains(&key(1)));
                }
            }
            Err(Error::NotFormatted) => {}
            Err(error) => panic!("unexpected recovery error: {error}"),
        }
    }
}
