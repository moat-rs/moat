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

//! Small deterministic async lifecycle tests. No disks, load generation, or sleeps.
#![cfg(unix)]

use moat_common::{AlignedBuf, ChunkId, PAGE_SIZE};
use moat_engine::{
    engine::{self, Completion, Device, Engine, Error, FormatOptions, Options, State},
    frame::{FrameBuilder, FrameLimits},
    io::{self as mio, Operation, Queue, Request},
    pipeline::{self, ReadBuffers, Ticket},
};
use std::{cell::RefCell, collections::VecDeque, io, rc::Rc};
const PAGE: usize = PAGE_SIZE as usize;
const STRIDE: u32 = 256 << 10;
#[derive(Default)]
struct Data {
    bytes: Vec<u8>,
    durable: Vec<u8>,
    pending: VecDeque<Request>,
    completed: VecDeque<mio::Completion>,
    cold_io: usize,
    hold_reads: bool,
    hold_writes: bool,
    fail_queue: bool,
    io_count: usize,
    syncs: usize,
    forbid_sync: bool,
    fail_writes: bool,
}
#[derive(Clone)]
struct Disk(Rc<RefCell<Data>>);
impl Device for Disk {
    fn capacity(&self) -> io::Result<u64> {
        Ok(self.0.borrow().bytes.len() as u64)
    }
    fn read_at(&self, out: &mut [u8], offset: u64) -> io::Result<()> {
        let mut d = self.0.borrow_mut();
        d.cold_io += 1;
        out.copy_from_slice(&d.bytes[offset as usize..offset as usize + out.len()]);
        Ok(())
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        let mut d = self.0.borrow_mut();
        d.cold_io += 1;
        d.bytes[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    fn sync(&self) -> io::Result<()> {
        let mut d = self.0.borrow_mut();
        assert!(!d.forbid_sync, "unexpected device sync");
        d.syncs += 1;
        d.cold_io += 1;
        d.durable = d.bytes.clone();
        Ok(())
    }
}
struct TestQueue {
    disk: Disk,
    depth: usize,
}
impl Queue for TestQueue {
    fn depth(&self) -> usize {
        self.depth
    }
    fn vacant(&self) -> usize {
        let d = self.disk.0.borrow();
        self.depth - d.pending.len() - d.completed.len()
    }
    fn has_ready(&self) -> bool {
        !self.disk.0.borrow().completed.is_empty()
    }
    fn try_submit(&mut self, request: Request) -> Result<(), Request> {
        if self.vacant() == 0 {
            return Err(request);
        }
        self.disk.0.borrow_mut().pending.push_back(request);
        Ok(())
    }
    fn poll(&mut self, _wait: bool) -> io::Result<()> {
        let mut d = self.disk.0.borrow_mut();
        if d.fail_queue {
            return Err(io::Error::other("injected queue failure"));
        }
        let Some(at) = d.pending.iter().position(|r| {
            (!d.hold_reads || r.operation != Operation::Read) && (!d.hold_writes || r.operation != Operation::Write)
        }) else {
            return Ok(());
        };
        let mut request = d.pending.remove(at).unwrap();
        let start = request.offset as usize;
        let fail_write = d.fail_writes && request.operation == Operation::Write;
        match request.operation {
            Operation::Read => {
                request.buffer.as_mut().unwrap()[..request.len].copy_from_slice(&d.bytes[start..start + request.len])
            }
            Operation::Write => {
                if !fail_write {
                    d.bytes[start..start + request.len]
                        .copy_from_slice(&request.buffer.as_ref().unwrap()[..request.len])
                }
            }
            Operation::Sync => {
                assert!(!d.forbid_sync, "unexpected queued sync");
                d.durable = d.bytes.clone();
                d.syncs += 1;
            }
        }
        d.io_count += 1;
        let len = request.len;
        d.completed.push_back(mio::Completion {
            request,
            result: if fail_write {
                Err(io::Error::other("injected write failure"))
            } else {
                Ok(len)
            },
        });
        Ok(())
    }
    fn pop(&mut self) -> Option<mio::Completion> {
        self.disk.0.borrow_mut().completed.pop_front()
    }
}
type TestEngine = Engine<Disk, TestQueue>;
fn key(n: u128) -> ChunkId {
    ChunkId::from_u128(n)
}
fn formatted() -> Disk {
    formatted_with_sync(engine::SyncMode::Enabled)
}
fn formatted_with_sync(sync_mode: engine::SyncMode) -> Disk {
    let disk = Disk(Rc::new(RefCell::new(Data {
        bytes: vec![0; 2 * PAGE + 4 * STRIDE as usize],
        forbid_sync: sync_mode == engine::SyncMode::Disabled,
        ..Data::default()
    })));
    engine::format(
        &disk,
        FormatOptions {
            sync_mode,
            device_id: [9; 16],
            segment_size: STRIDE,
            limits: FrameLimits::new(32768, 16384).unwrap(),
        },
    )
    .unwrap();
    disk.0.borrow_mut().cold_io = 0;
    disk
}
fn begin(disk: &Disk, options: Options, depth: usize) -> (TestEngine, Ticket) {
    Engine::open_with_options(
        disk.clone(),
        TestQueue {
            disk: disk.clone(),
            depth,
        },
        options,
    )
    .unwrap()
}
fn until(engine: &mut TestEngine, ticket: Ticket) -> Completion {
    let mut out = Vec::new();
    for _ in 0..2000 {
        engine.poll(false, &mut out).unwrap();
        if let Some(at) = out.iter().position(|c| c.ticket() == ticket) {
            return out.remove(at);
        }
    }
    panic!("bounded functional operation did not complete");
}
fn lifecycle(engine: &mut TestEngine, ticket: Ticket) {
    match until(engine, ticket) {
        Completion::Lifecycle { result, .. } => result.unwrap(),
        other => panic!("{other:?}"),
    }
}
fn opened(disk: &Disk, options: Options, depth: usize) -> TestEngine {
    let (mut e, t) = begin(disk, options, depth);
    lifecycle(&mut e, t);
    e
}
fn submit(engine: &mut TestEngine, n: u128, lsn: u64, value: Option<&[u8]>) -> Ticket {
    let mut frame = FrameBuilder::new(engine.layout().unwrap().limits());
    match value {
        Some(v) => frame.push(key(n), lsn, v).unwrap(),
        None => frame.push_tombstone(key(n), lsn).unwrap(),
    }
    let mut buffer = AlignedBuf::zeroed(frame.encoded_len()).into();
    for _ in 0..100 {
        match engine.write(&frame, buffer) {
            Ok(t) => return t,
            Err(r) if matches!(r.error, Error::Pipeline(pipeline::Error::Backpressure)) => {
                buffer = r.input;
                engine.poll(false, &mut Vec::new()).unwrap();
            }
            Err(r) => panic!("{}", r.error),
        }
    }
    panic!("write admission stalled");
}
fn put(engine: &mut TestEngine, n: u128, lsn: u64, value: Option<&[u8]>) {
    let t = submit(engine, n, lsn, value);
    match until(engine, t) {
        Completion::Write { result, .. } => result.unwrap(),
        other => panic!("{other:?}"),
    }
}
#[test]
fn open_allocation_seal_close_use_only_the_queue() {
    let disk = formatted();
    let (mut e, t) = begin(&disk, Options::default(), 4);
    assert_eq!(e.state(), State::Opening);
    assert_eq!(disk.0.borrow().io_count, 0);
    assert!(matches!(e.layout(), Err(Error::NotReady)));
    assert!(matches!(e.stat(&key(1)), Err(Error::NotReady)));
    lifecycle(&mut e, t);
    put(&mut e, 1, 1, Some(b"value"));
    let t = e.seal().unwrap();
    lifecycle(&mut e, t);
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    assert_eq!(e.state(), State::Closed);
    assert!(
        matches!(e.read(key(1),0..1,false,ReadBuffers::new(AlignedBuf::zeroed(PAGE))),Err(r) if matches!(r.error,Error::Closed))
    );
    assert_eq!(disk.0.borrow().cold_io, 0);
    assert!(disk.0.borrow().pending.is_empty());
    drop(e);
    let e = opened(&disk, Options::default(), 4);
    assert_eq!(e.stat(&key(1)).unwrap(), Some((1, 5)));
    assert_eq!(disk.0.borrow().cold_io, 0);
}
#[test]
fn held_reads_do_not_prevent_rollover_and_keep_the_old_snapshot() {
    let disk = formatted();
    let mut e = opened(&disk, Options::default(), 4);
    put(&mut e, 1, 1, Some(b"old"));
    disk.0.borrow_mut().hold_reads = true;
    let read = e
        .read(key(1), 0..3, false, ReadBuffers::new(AlignedBuf::zeroed(PAGE)))
        .unwrap();
    let transition = e.rollover().unwrap();
    lifecycle(&mut e, transition);
    assert_eq!(e.active_segment(), Some(1));
    assert_eq!(e.in_flight(), 1);
    put(&mut e, 1, 2, Some(b"new"));
    disk.0.borrow_mut().hold_reads = false;
    match until(&mut e, read) {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), b"old"),
        other => panic!("{other:?}"),
    }
}
#[test]
fn depth_one_lifecycle_retains_progress() {
    let disk = formatted();
    let mut e = opened(&disk, Options::default(), 1);
    put(&mut e, 1, 1, Some(b"a"));
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    put(&mut e, 2, 2, Some(b"b"));
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    assert!(disk.0.borrow().pending.is_empty());
}
#[test]
fn close_drains_accepted_reads_and_refuses_new_admission() {
    let disk = formatted();
    let mut e = opened(&disk, Options::default(), 4);
    put(&mut e, 1, 1, Some(b"x"));
    disk.0.borrow_mut().hold_reads = true;
    let read = e
        .read(key(1), 0..1, false, ReadBuffers::new(AlignedBuf::zeroed(PAGE)))
        .unwrap();
    let close = e.close().unwrap();
    for _ in 0..5 {
        assert_eq!(e.poll(false, &mut Vec::new()).unwrap(), 0);
    }
    assert_eq!(e.state(), State::Closing);
    assert!(matches!(e.flush(), Err(Error::Closed)));
    disk.0.borrow_mut().hold_reads = false;
    assert!(matches!(until(&mut e, read), Completion::Read { result: Ok(_), .. }));
    lifecycle(&mut e, close);
    assert_eq!(e.state(), State::Closed);
}
#[test]
fn fatal_queue_error_finishes_every_accepted_ticket_once_and_retains_buffers() {
    let disk = formatted();
    let mut options = Options::default();
    options.poll.operations = 1;
    let mut e = opened(&disk, options, 4);
    put(&mut e, 1, 1, Some(b"x"));
    let first = e
        .read(key(1), 0..1, false, ReadBuffers::new(AlignedBuf::zeroed(PAGE)))
        .unwrap();
    let second = e
        .read(key(1), 0..1, false, ReadBuffers::new(AlignedBuf::zeroed(PAGE)))
        .unwrap();
    disk.0.borrow_mut().fail_queue = true;
    let mut out = Vec::new();
    for _ in 0..4 {
        let before = out.len();
        e.poll(false, &mut out).unwrap();
        assert!(out.len() - before <= 1);
    }
    let mut tickets = out.iter().map(Completion::ticket).collect::<Vec<_>>();
    tickets.sort();
    assert_eq!(tickets, vec![first, second]);
    assert!(out.iter().all(|c| matches!(c, Completion::Failed { .. })));
    assert_eq!(e.in_flight(), 0);
    assert_eq!(e.state(), State::Failed);
    assert_eq!(disk.0.borrow().pending.len(), 2);
    assert!(disk.0.borrow().pending.iter().all(|r| r.buffer.is_some()));
}
#[test]
fn index_limit_includes_tombstones_and_recovery_obeys_the_same_bound() {
    let disk = formatted();
    let mut options = Options::default();
    options.resources.index_entries = 2;
    let mut e = opened(&disk, options, 4);
    put(&mut e, 1, 1, Some(b"a"));
    put(&mut e, 2, 2, None);
    let before = disk.0.borrow().io_count;
    let mut frame = FrameBuilder::new(e.layout().unwrap().limits());
    frame.push(key(3), 3, b"b").unwrap();
    let rejected = e.write(&frame, AlignedBuf::zeroed(frame.encoded_len())).unwrap_err();
    assert!(matches!(
        rejected.error,
        Error::Pipeline(pipeline::Error::ResourceLimit(_))
    ));
    assert_eq!(disk.0.borrow().io_count, before);
    put(&mut e, 1, 4, Some(b"c"));
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    drop(e);
    let e = opened(&disk, options, 4);
    assert_eq!(e.indexed_versions().unwrap(), 2);
    drop(e);
    options.resources.index_entries = 1;
    let (mut e, t) = begin(&disk, options, 4);
    assert!(matches!(until(&mut e, t), Completion::Lifecycle { result: Err(_), .. }));
    assert!(matches!(e.stat(&key(1)), Err(Error::Failed)));
    let t = e.close().unwrap();
    assert!(matches!(until(&mut e, t), Completion::Lifecycle { result: Err(_), .. }));
    assert_eq!(e.state(), State::Closed);
}
#[test]
fn cursor_is_bounded_excludes_new_keys_and_rejects_another_engine() {
    let disk = formatted();
    let mut e = opened(&disk, Options::default(), 4);
    put(&mut e, 1, 1, Some(b"a"));
    put(&mut e, 2, 2, Some(b"b"));
    let mut cursor = e.version_cursor().unwrap();
    let mut found = Vec::new();
    assert!(
        !e.visit_versions_batch(&mut cursor, 1, |id, _, _| found.push(id))
            .unwrap()
    );
    put(&mut e, 3, 3, Some(b"c"));
    assert!(
        e.visit_versions_batch(&mut cursor, 1, |id, _, _| found.push(id))
            .unwrap()
    );
    assert_eq!(found, vec![key(1), key(2)]);
    let other = formatted();
    let other = opened(&other, Options::default(), 4);
    assert!(other.visit_versions_batch(&mut cursor, 1, |_, _, _| {}).is_err());
}
#[test]
fn pending_record_limit_rejects_before_io_and_recovers_after_completion() {
    let disk = formatted();
    let mut options = Options::default();
    options.resources.pending_records = 1;
    let mut e = opened(&disk, options, 4);
    put(&mut e, 1, 1, Some(b"x"));
    let first = submit(&mut e, 1, 2, Some(b"y"));
    let mut frame = FrameBuilder::new(e.layout().unwrap().limits());
    frame.push(key(2), 3, b"z").unwrap();
    assert!(
        matches!(e.write(&frame,AlignedBuf::zeroed(frame.encoded_len())),Err(r) if matches!(r.error,Error::Pipeline(pipeline::Error::Backpressure)))
    );
    assert!(matches!(until(&mut e, first), Completion::Write { result: Ok(_), .. }));
    put(&mut e, 2, 3, Some(b"z"));
}

#[test]
fn metadata_pressure_rolls_over_before_the_physical_segment_is_full() {
    let disk = formatted();
    let mut options = Options::default();
    options.resources.metadata_bytes = 4096;
    let mut e = opened(&disk, options, 4);
    for n in 0..32 {
        put(&mut e, n, n as u64 + 1, Some(b"small"));
    }
    assert_eq!(e.allocated_segments(), 2);
    assert_eq!(e.indexed_versions().unwrap(), 32);
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    drop(e);
    let e = opened(&disk, options, 4);
    assert_eq!(e.indexed_versions().unwrap(), 32);
}
#[test]
fn oversized_recovery_footer_uses_bounded_frame_scan() {
    let disk = formatted();
    let mut e = opened(&disk, Options::default(), 4);
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    let mut frame = FrameBuilder::new(e.layout().unwrap().limits());
    for n in 0..70 {
        frame.push(key(n), n as u64, b"x").unwrap();
    }
    let t = e.write(&frame, AlignedBuf::zeroed(frame.encoded_len())).unwrap();
    assert!(matches!(until(&mut e, t), Completion::Write { result: Ok(_), .. }));
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    drop(e);
    let mut options = Options::default();
    options.resources.metadata_bytes = 4096;
    let e = opened(&disk, options, 4);
    assert_eq!(e.indexed_versions().unwrap(), 70);
}
#[test]
fn duplicate_versions_in_one_frame_fit_a_single_index_entry_budget() {
    let disk = formatted();
    let mut options = Options::default();
    options.resources.index_entries = 1;
    let mut e = opened(&disk, options, 4);
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    let mut frame = FrameBuilder::new(e.layout().unwrap().limits());
    frame.push(key(1), 1, b"old").unwrap();
    frame.push(key(1), 2, b"new").unwrap();
    let t = e.write(&frame, AlignedBuf::zeroed(frame.encoded_len())).unwrap();
    assert!(matches!(until(&mut e, t), Completion::Write { result: Ok(_), .. }));
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    drop(e);
    let e = opened(&disk, options, 4);
    assert_eq!(e.stat(&key(1)).unwrap(), Some((2, 3)));
}

#[test]
fn record_budget_yields_between_frames_without_losing_local_readiness() {
    let disk = formatted();
    let mut options = Options::default();
    options.poll.records = 1;
    let mut e = opened(&disk, options, 4);
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    let mut tickets = Vec::new();
    for n in [1, 3] {
        let mut frame = FrameBuilder::new(e.layout().unwrap().limits());
        frame.push(key(n), 1, b"a").unwrap();
        frame.push(key(n + 1), 1, b"b").unwrap();
        tickets.push(e.write(&frame, AlignedBuf::zeroed(frame.encoded_len())).unwrap());
    }
    // Make both I/Os ready before the owner polls. Publication is atomic per
    // frame, so a two-record frame overshoots the target and then must yield.
    let mut backend = TestQueue {
        disk: disk.clone(),
        depth: 4,
    };
    backend.poll(false).unwrap();
    backend.poll(false).unwrap();
    let mut out = Vec::new();
    e.poll(false, &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].ticket(), tickets[0]);
    assert_eq!(e.indexed_versions().unwrap(), 2);
    assert!(e.has_ready());
    out.clear();
    e.poll(false, &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].ticket(), tickets[1]);
    assert_eq!(e.indexed_versions().unwrap(), 4);
    assert!(!e.has_ready());
}

#[test]
fn fatal_retirement_respects_operation_budget_even_with_sparse_slots() {
    let disk = formatted();
    let mut options = Options::default();
    options.poll.operations = 1;
    let mut e = opened(&disk, options, 8);
    put(&mut e, 1, 1, Some(b"a"));
    let ticket = e
        .read(key(1), 0..1, false, ReadBuffers::new(AlignedBuf::zeroed(PAGE)))
        .unwrap();
    disk.0.borrow_mut().fail_queue = true;
    let mut out = Vec::new();
    for _ in 0..8 {
        e.poll(false, &mut out).unwrap();
        if !out.is_empty() {
            break;
        }
        assert!(e.has_ready());
    }
    assert_eq!(out.len(), 1);
    assert!(matches!(&out[0], Completion::Failed { ticket: got, .. } if *got == ticket));
    assert_eq!(e.in_flight(), 0);
    e.poll(false, &mut out).unwrap();
    assert_eq!(out.len(), 1);
}

#[test]
fn disabled_sync_covers_format_flush_seal_rollover_and_close() {
    let disk = formatted_with_sync(engine::SyncMode::Disabled);
    let options = Options {
        sync_mode: engine::SyncMode::Disabled,
        ..Options::default()
    };
    let mut e = opened(&disk, options, 4);
    put(&mut e, 1, 1, Some(b"first"));
    let t = e.flush().unwrap();
    assert!(matches!(until(&mut e, t), Completion::Flush { result: Ok(()), .. }));
    let t = e.seal().unwrap();
    lifecycle(&mut e, t);
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    put(&mut e, 2, 2, Some(b"second"));
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    drop(e);
    let mut e = opened(&disk, options, 4);
    assert_eq!(e.stat(&key(1)).unwrap(), Some((1, 5)));
    assert_eq!(e.stat(&key(2)).unwrap(), Some((2, 6)));
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    assert_eq!(disk.0.borrow().syncs, 0);
    assert!(disk.0.borrow().durable.is_empty());
}

#[test]
fn disabled_flush_still_waits_for_writes_and_propagates_failure() {
    for fail in [false, true] {
        let disk = formatted_with_sync(engine::SyncMode::Disabled);
        let options = Options {
            sync_mode: engine::SyncMode::Disabled,
            ..Options::default()
        };
        let mut e = opened(&disk, options, 4);
        let t = e.rollover().unwrap();
        lifecycle(&mut e, t);
        disk.0.borrow_mut().hold_writes = true;
        let write = submit(&mut e, 1, 1, Some(b"value"));
        let flush = e.flush().unwrap();
        let mut out = Vec::new();
        for _ in 0..4 {
            e.poll(false, &mut out).unwrap();
            assert!(out.is_empty());
        }
        disk.0.borrow_mut().hold_writes = false;
        disk.0.borrow_mut().fail_writes = fail;
        for _ in 0..8 {
            e.poll(false, &mut out).unwrap();
            if out.iter().any(|c| c.ticket() == flush) {
                break;
            }
        }
        assert_eq!(out.len(), 2);
        assert!(
            matches!(&out[0], Completion::Write { ticket, result, .. } if *ticket == write && result.is_err() == fail)
        );
        assert!(
            matches!(&out[1], Completion::Flush { ticket, result, .. } if *ticket == flush && result.is_err() == fail)
        );
        assert_eq!(disk.0.borrow().syncs, 0);
    }
}

#[test]
fn flush_waits_for_write_completion_before_submitting_sync() {
    let disk = formatted();
    let mut e = opened(&disk, Options::default(), 4);
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    let syncs = disk.0.borrow().syncs;
    disk.0.borrow_mut().hold_writes = true;
    let write = submit(&mut e, 1, 1, Some(b"durable value"));
    let flush = e.flush().unwrap();
    let mut out = Vec::new();
    for _ in 0..4 {
        e.poll(false, &mut out).unwrap();
        let d = disk.0.borrow();
        assert_eq!(d.syncs, syncs);
        assert!(d.pending.iter().all(|r| r.operation != Operation::Sync));
        assert!(out.is_empty());
    }
    disk.0.borrow_mut().hold_writes = false;
    for _ in 0..8 {
        e.poll(false, &mut out).unwrap();
        if out.iter().any(|c| c.ticket() == flush) {
            break;
        }
    }
    assert_eq!(out.len(), 2);
    assert!(matches!(&out[0], Completion::Write { ticket, result: Ok(_), .. } if *ticket == write));
    assert!(matches!(&out[1], Completion::Flush { ticket, result: Ok(_), .. } if *ticket == flush));
    let d = disk.0.borrow();
    assert_eq!(d.syncs, syncs + 1);
    assert_eq!(d.durable, d.bytes);
}

#[test]
fn write_completion_does_not_spend_the_payload_byte_budget() {
    let disk = formatted();
    let mut options = Options::default();
    options.poll.bytes = 1;
    let mut e = opened(&disk, options, 4);
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    let mut tickets = Vec::new();
    for n in 1..=2 {
        let mut frame = FrameBuilder::new(e.layout().unwrap().limits());
        frame.push(key(n), 1, b"value").unwrap();
        tickets.push(e.write(&frame, AlignedBuf::zeroed(frame.encoded_len())).unwrap());
    }
    let mut backend = TestQueue { disk, depth: 4 };
    backend.poll(false).unwrap();
    backend.poll(false).unwrap();
    let mut out = Vec::new();
    e.poll(false, &mut out).unwrap();
    assert_eq!(out.len(), 2);
    for (completion, ticket) in out.iter().zip(tickets) {
        assert!(matches!(completion, Completion::Write { ticket: got, result: Ok(_), .. } if *got == ticket));
    }
    assert_eq!(e.in_flight(), 0);
}

#[test]
fn full_index_accepts_overwrites_without_duplicating_cursor_keys() {
    let disk = formatted();
    let mut options = Options::default();
    options.resources.index_entries = 2;
    let mut e = opened(&disk, options, 4);
    put(&mut e, 1, 1, Some(b"old"));
    put(&mut e, 2, 1, Some(b"old"));
    put(&mut e, 1, 2, None);
    put(&mut e, 1, 1, Some(b"stale"));
    put(&mut e, 2, 2, Some(b"new"));
    assert_eq!(e.stat(&key(1)).unwrap(), None);
    assert_eq!(e.stat(&key(2)).unwrap(), Some((2, 3)));
    let mut cursor = e.version_cursor().unwrap();
    let mut versions = Vec::new();
    while !e
        .visit_versions_batch(&mut cursor, 1, |key, lsn, len| versions.push((key, lsn, len)))
        .unwrap()
    {}
    versions.sort_by_key(|entry| entry.0);
    assert_eq!(versions, [(key(1), 2, None), (key(2), 2, Some(3))]);
}

#[test]
fn incremental_footer_crosses_crc_windows_and_matches_the_public_decoder() {
    let disk = formatted();
    let mut e = opened(&disk, Options::default(), 4);
    let t = e.rollover().unwrap();
    lifecycle(&mut e, t);
    let layout = e.layout().unwrap();
    for batch in 0..5 {
        let mut frame = FrameBuilder::new(layout.limits());
        for n in 0..220 {
            frame.push_tombstone(key(batch * 220 + n), 1).unwrap();
        }
        let t = e.write(&frame, AlignedBuf::zeroed(frame.encoded_len())).unwrap();
        assert!(matches!(until(&mut e, t), Completion::Write { result: Ok(_), .. }));
    }
    let t = e.close().unwrap();
    lifecycle(&mut e, t);
    {
        let d = disk.0.borrow();
        let base = layout.segment_base(0).unwrap() as usize;
        let bytes = &d.bytes[base..base + STRIDE as usize];
        let header = moat_engine::segment::FooterTrailer::decode(&bytes[bytes.len() - PAGE..], STRIDE)
            .unwrap()
            .header();
        let range = header.footer_range().unwrap();
        assert!(range.len() > 64 << 10);
        let footer = moat_engine::segment::Footer::decode(
            &bytes[range.start as usize..range.end as usize],
            header,
            layout.limits(),
        )
        .unwrap();
        assert_eq!(footer.frames().len(), 5);
    }
    drop(e);
    let e = opened(&disk, Options::default(), 4);
    assert_eq!(e.indexed_versions().unwrap(), 1100);
    assert!(!e.contains(&key(0)).unwrap());
}
