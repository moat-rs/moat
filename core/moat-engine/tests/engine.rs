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

use std::{
    cell::{Cell, RefCell},
    fs::File,
    io,
    os::unix::fs::FileExt,
    rc::Rc,
};

use moat_common::{AlignedBuf, ChunkId, PAGE_SIZE};
use moat_engine::{
    engine::{self, Device, Engine, Error, FormatOptions, Layout},
    frame::{FrameBuilder, FrameLimits, FramePosition, PreparedFrame},
    io::{FileQueue, Queue},
    pipeline::{self, Completion, ReadBuffers},
    segment::{FooterTrailer, SegmentHeader},
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
    reads: Rc<RefCell<Vec<(u64, usize)>>>,
    fail_read: Rc<Cell<Option<u64>>>,
    durable: Rc<RefCell<Vec<u8>>>,
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
    fn crash(&self) {
        self.file.write_all_at(&self.durable.borrow(), 0).unwrap();
        self.file.sync_data().unwrap();
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
        self.reads.borrow_mut().push((offset, bytes.len()));
        if self.fail_read.get() == Some(offset) {
            return Err(io::Error::other("injected read failure"));
        }
        self.file.read_exact_at(bytes, offset)
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        self.before(true, offset)?;
        self.file.write_all_at(bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.before(false, 0)?;
        self.file.sync_data()?;
        self.file.read_exact_at(&mut self.durable.borrow_mut(), 0)?;
        Ok(())
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
        reads: Rc::new(RefCell::new(Vec::new())),
        fail_read: Rc::new(Cell::new(None)),
        durable: Rc::new(RefCell::new(vec![
            0;
            (2 * PAGE_SIZE + segments * STRIDE as u64) as usize
        ])),
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
        &[(false, 0), (true, base + STRIDE as u64 - PAGE_SIZE), (false, 0)]
    );
    let mut after = vec![0; PAGE];
    disk.read_at(&mut after, base).unwrap();
    assert_eq!(before, after);
}

#[test]
fn every_seal_failure_stops_writes_and_preserves_previously_flushed_data() {
    for failure in 1..=3 {
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
            base + STRIDE as u64 - 64 + 17
        } else {
            base + STRIDE as u64 - PAGE_SIZE + 17
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
    disk.file
        .write_all_at(&[0xff], base + STRIDE as u64 - PAGE_SIZE + 17)
        .unwrap();
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
fn footer_trailer_is_stored_in_the_final_page_of_the_segment() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"x"));
    store.seal().unwrap();
    let layout = store.layout();
    let base = layout.segment_base(0).unwrap();
    let mut page = vec![0; PAGE];
    disk.read_at(&mut page, base + STRIDE as u64 - PAGE_SIZE).unwrap();
    let header = FooterTrailer::decode(&page, STRIDE).unwrap().header();
    assert!(header.is_sealed());
    assert_eq!(header.footer_range().unwrap().end, STRIDE);
    assert_eq!(header.data_end(), Some(2 * PAGE as u32));
}

#[cfg(target_os = "linux")]
#[test]
fn registered_prepared_io_rolls_over_and_reopens_with_both_read_policies() {
    use moat_common::{BufferPool, HugePages, PoolOptions};
    use moat_engine::io::UringQueue;

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
    let mut buffer: moat_engine::io::Buffer = pool
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

fn put_batch(store: &mut Store, count: u32) {
    let mut frame = FrameBuilder::new(store.layout().limits());
    for n in 0..count {
        frame.push(key(n as u128), n as u64, b"x").unwrap();
    }
    store.write(&frame, AlignedBuf::zeroed(frame.encoded_len())).unwrap();
    drain(store);
}

fn repair_header_crc(bytes: &mut [u8]) {
    let crc = moat_common::Crc32c::new()
        .update(&bytes[..12])
        .update(&[0; 4])
        .update(&bytes[16..])
        .finalize();
    bytes[12..16].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn recovery_reads_small_footer_once_and_only_the_prefix_of_large_footer() {
    for (records, footer_pages) in [(1, 1), (70, 2), (180, 4)] {
        let disk = disk(1);
        let mut store = disk.open().unwrap();
        put_batch(&mut store, records);
        store.seal().unwrap();
        let base = store.layout().segment_base(0).unwrap();
        drop(store);
        disk.reads.borrow_mut().clear();
        let mut store = disk.open().unwrap();
        let mut expected = vec![
            (0, PAGE),
            (PAGE_SIZE, PAGE),
            (base, PAGE),
            (base + STRIDE as u64 - PAGE_SIZE, PAGE),
        ];
        if footer_pages > 1 {
            expected.push((
                base + STRIDE as u64 - (footer_pages * PAGE) as u64,
                (footer_pages - 1) * PAGE,
            ));
        }
        assert_eq!(*disk.reads.borrow(), expected);
        assert_eq!(store.indexed_versions(), records as usize);
        for n in 0..records {
            assert_eq!(get(&mut store, n as u128, 1, true), b"x");
        }
    }
}

#[test]
fn multipage_seal_persists_prefix_before_committing_the_tail_page() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    put_batch(&mut store, 70);
    let base = store.layout().segment_base(0).unwrap();
    disk.fail(usize::MAX);
    store.seal().unwrap();
    assert_eq!(
        *disk.log.borrow(),
        vec![
            (true, base + STRIDE as u64 - 2 * PAGE_SIZE),
            (false, 0),
            (true, base + STRIDE as u64 - PAGE_SIZE),
            (false, 0),
        ]
    );
}

#[test]
fn every_multipage_seal_failure_preserves_previously_flushed_records() {
    for failure in 1..=4 {
        let disk = disk(1);
        let mut store = disk.open().unwrap();
        put_batch(&mut store, 70);
        store.flush().unwrap();
        drain(&mut store);
        disk.fail(failure);
        assert!(matches!(store.seal(), Err(Error::Io(_))));
        assert!(matches!(store.rollover(), Err(Error::Failed)));
        drop(store);
        disk.fail(usize::MAX);
        let mut store = disk.open().unwrap();
        assert_eq!(store.indexed_versions(), 70);
        assert_eq!(get(&mut store, 69, 1, true), b"x");
    }
}

#[test]
fn older_footer_is_ignored_after_allocation_generation_changes() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"old"));
    put(&mut store, 2, 2, Some(b"stale"));
    store.seal().unwrap();
    let layout = store.layout();
    let base = layout.segment_base(0).unwrap();
    drop(store);
    let mut page = vec![0; PAGE];
    disk.read_at(&mut page, base).unwrap();
    let old = SegmentHeader::decode(&page, layout.device_id(), 0, STRIDE).unwrap();
    let new = SegmentHeader::new(
        moat_engine::segment::SegmentId {
            sequence: old.id().sequence + 1,
            ..old.id()
        },
        STRIDE,
    )
    .unwrap();
    new.encode_into(&mut page).unwrap();
    disk.write_at(&page, base).unwrap();
    disk.sync().unwrap();
    // Allocation alone must not resurrect either old record.
    assert_eq!(disk.open().unwrap().indexed_versions(), 0);
    let mut frame = FrameBuilder::new(layout.limits());
    frame.push(key(99), 99, b"fresh").unwrap();
    let mut bytes = AlignedBuf::zeroed(frame.encoded_len());
    frame
        .encode_into(
            FramePosition::new(new.id().sequence, PAGE as u32, STRIDE).unwrap(),
            &mut bytes,
        )
        .unwrap();
    disk.write_at(&bytes, base + PAGE_SIZE).unwrap();
    disk.sync().unwrap();
    let mut store = disk.open().unwrap();
    assert_eq!(store.indexed_versions(), 1);
    assert_eq!(get(&mut store, 99, 5, true), b"fresh");
    assert!(!store.contains(&key(1)));
    assert!(!store.contains(&key(2)));
}

#[test]
fn future_footer_and_wrong_allocation_identity_are_errors() {
    for field in [16, 32, 40] {
        let disk = disk(1);
        let mut store = disk.open().unwrap();
        put(&mut store, 1, 1, Some(b"x"));
        store.seal().unwrap();
        let base = store.layout().segment_base(0).unwrap();
        drop(store);
        let at = base + STRIDE as u64 - 64;
        let mut trailer = vec![0; 64];
        disk.read_at(&mut trailer, at).unwrap();
        if field == 40 {
            let sequence = u64::from_le_bytes(trailer[40..48].try_into().unwrap());
            trailer[40..48].copy_from_slice(&(sequence + 1).to_le_bytes());
        } else {
            trailer[field] ^= 1;
        }
        repair_header_crc(&mut trailer);
        disk.file.write_all_at(&trailer, at).unwrap();
        assert!(matches!(disk.open(), Err(Error::Corrupt(_))));
    }
}

#[test]
fn valid_footer_cannot_hide_a_corrupt_allocation_header() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"x"));
    store.seal().unwrap();
    let base = store.layout().segment_base(0).unwrap();
    drop(store);
    disk.file.write_all_at(&[0xff], base + 12).unwrap();
    assert!(matches!(disk.open(), Err(Error::Segment(_))));
}

#[test]
fn torn_tail_page_recovers_with_partial_metadata_or_partial_trailer() {
    for records in [1, 70] {
        let disk = disk(1);
        let mut store = disk.open().unwrap();
        put_batch(&mut store, records);
        store.flush().unwrap();
        drain(&mut store);
        store.seal().unwrap();
        let at = store.layout().segment_base(0).unwrap() + STRIDE as u64 - PAGE_SIZE;
        drop(store);
        let mut tail = vec![0; PAGE];
        disk.read_at(&mut tail, at).unwrap();
        let cuts = [
            0,
            1,
            511,
            512,
            1024,
            2048,
            3584,
            PAGE - 65,
            PAGE - 64,
            PAGE - 63,
            PAGE - 52,
            PAGE - 48,
            PAGE - 4,
            PAGE - 1,
            PAGE,
        ];
        for cut in cuts {
            for keep_prefix in [false, true] {
                let mut torn = tail.clone();
                if keep_prefix {
                    torn[cut..].fill(0);
                } else {
                    torn[..cut].fill(0);
                }
                disk.file.write_all_at(&torn, at).unwrap();
                let mut reopened = disk.open().unwrap();
                assert_eq!(
                    reopened.indexed_versions(),
                    records as usize,
                    "cut={cut}, prefix={keep_prefix}"
                );
                assert_eq!(get(&mut reopened, (records - 1) as u128, 1, true), b"x");
            }
        }
    }
}

#[test]
fn same_generation_bad_footer_keeps_the_committed_frame_boundary() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    put_batch(&mut store, 70);
    store.seal().unwrap();
    let base = store.layout().segment_base(0).unwrap();
    drop(store);
    // Damage the first footer page while retaining the independently valid trailer.
    disk.file
        .write_all_at(&[0xff], base + STRIDE as u64 - 2 * PAGE_SIZE)
        .unwrap();
    assert_eq!(disk.open().unwrap().indexed_versions(), 70);
    disk.file.write_all_at(&[0xff], base + PAGE_SIZE + 12).unwrap();
    assert!(matches!(disk.open(), Err(Error::Segment(_))));
}

#[test]
fn recovery_read_errors_are_not_treated_as_missing_footers() {
    let disk = disk(1);
    let mut store = disk.open().unwrap();
    put_batch(&mut store, 70);
    store.seal().unwrap();
    let base = store.layout().segment_base(0).unwrap();
    drop(store);
    for at in [
        base,
        base + STRIDE as u64 - PAGE_SIZE,
        base + STRIDE as u64 - 2 * PAGE_SIZE,
    ] {
        disk.fail_read.set(Some(at));
        assert!(matches!(disk.open(), Err(Error::Io(_))));
    }
}

#[test]
fn seal_crash_discards_unsynced_writes_without_losing_durable_records() {
    for (records, operations) in [(1, 3), (70, 4)] {
        for failure in 1..=operations {
            let disk = disk(1);
            let mut store = disk.open().unwrap();
            put_batch(&mut store, records);
            disk.sync().unwrap();
            disk.fail(failure);
            assert!(matches!(store.seal(), Err(Error::Io(_))));
            drop(store);
            disk.crash();
            disk.fail(usize::MAX);
            let mut reopened = disk.open().unwrap();
            assert_eq!(reopened.indexed_versions(), records as usize);
            assert_eq!(get(&mut reopened, (records - 1) as u128, 1, true), b"x");
        }
    }
}

#[test]
fn reformat_advances_beyond_recovered_allocation_generations() {
    let disk = disk(1);
    let layout = Layout::read(&disk).unwrap();
    let mut store = disk.open().unwrap();
    put(&mut store, 1, 1, Some(b"old"));
    let base = layout.segment_base(0).unwrap();
    drop(store);
    let mut page = vec![0; PAGE];
    disk.read_at(&mut page, base).unwrap();
    let old = SegmentHeader::decode(&page, layout.device_id(), 0, STRIDE).unwrap();
    let newer = SegmentHeader::new(
        moat_engine::segment::SegmentId {
            sequence: old.id().sequence + 1,
            ..old.id()
        },
        STRIDE,
    )
    .unwrap();
    newer.encode_into(&mut page).unwrap();
    disk.write_at(&page, base).unwrap();
    let mut frame = FrameBuilder::new(layout.limits());
    frame.push(key(2), 2, b"stale").unwrap();
    let mut bytes = AlignedBuf::zeroed(frame.encoded_len());
    frame
        .encode_into(
            FramePosition::new(newer.id().sequence, 2 * PAGE as u32, STRIDE).unwrap(),
            &mut bytes,
        )
        .unwrap();
    disk.write_at(&bytes, base + 2 * PAGE_SIZE).unwrap();
    disk.sync().unwrap();
    engine::format(&disk, options(8)).unwrap();
    let mut store = disk.open().unwrap();
    put(&mut store, 99, 99, Some(b"fresh"));
    disk.sync().unwrap();
    drop(store);
    let mut store = disk.open().unwrap();
    assert_eq!(store.indexed_versions(), 1);
    assert_eq!(get(&mut store, 99, 5, true), b"fresh");
    assert!(!store.contains(&key(2)));
}
