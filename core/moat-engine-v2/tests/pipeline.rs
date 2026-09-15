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

//! Small functional I/O and deterministic completion-order tests. No load tests.
#![cfg(unix)]

use std::{cell::RefCell, collections::VecDeque, fs::File, io as stdio, os::unix::fs::FileExt, rc::Rc};

use moat_common::{AlignedBuf, ChunkId, PAGE_SIZE};
use moat_engine_v2::{
    frame::{FrameBuilder, FrameLimits, PreparedFrame},
    io::{self, FileQueue, Operation, Queue, Request},
    pipeline::{Completion, Error, Pipeline, ReadBuffers},
    segment::{Scanner, SegmentHeader, SegmentId},
};

const PAGE: usize = PAGE_SIZE as usize;
const CAPACITY: u32 = 256 * 1024;

fn key(id: u128) -> ChunkId {
    ChunkId::from_u128(id)
}
fn limits() -> FrameLimits {
    FrameLimits::new(128 * 1024, 96 * 1024).unwrap()
}
fn header() -> SegmentHeader {
    SegmentHeader::new(
        SegmentId {
            device_id: [3; 16],
            segment_no: 0,
            sequence: 1,
        },
        CAPACITY,
    )
    .unwrap()
}
fn file() -> File {
    let file = tempfile::tempfile().unwrap();
    file.set_len(CAPACITY as u64).unwrap();
    let mut bytes = AlignedBuf::zeroed(PAGE);
    header().encode_into(&mut bytes).unwrap();
    file.write_all_at(&bytes, 0).unwrap();
    file.sync_data().unwrap();
    file
}
fn frame(id: u128, lsn: u64, value: &[u8]) -> FrameBuilder<'_> {
    let mut frame = FrameBuilder::new(limits());
    frame.push(key(id), lsn, value).unwrap();
    frame
}
fn write<Q: Queue>(pipeline: &mut Pipeline<Q>, id: u128, lsn: u64, value: &[u8]) -> u64 {
    let frame = frame(id, lsn, value);
    pipeline
        .write(&frame, AlignedBuf::zeroed(frame.encoded_len()))
        .unwrap()
        .number()
}
fn buffers() -> ReadBuffers {
    ReadBuffers {
        metadata: AlignedBuf::zeroed(PAGE),
        value: AlignedBuf::zeroed(128 * 1024),
    }
}
fn drain<Q: Queue>(pipeline: &mut Pipeline<Q>) -> Vec<Completion> {
    let mut out = Vec::new();
    for _ in 0..32 {
        if pipeline.in_flight() == 0 {
            return out;
        }
        pipeline.poll(true, &mut out).unwrap();
    }
    panic!("small functional workload did not complete");
}

#[derive(Default)]
struct State {
    pending: VecDeque<Request>,
    done: VecDeque<io::Completion>,
    log: Vec<(Operation, u64, usize)>,
    queue_error: bool,
}
struct ManualQueue {
    depth: usize,
    state: Rc<RefCell<State>>,
}
impl Queue for ManualQueue {
    fn depth(&self) -> usize {
        self.depth
    }
    fn vacant(&self) -> usize {
        let state = self.state.borrow();
        self.depth - state.pending.len() - state.done.len()
    }
    fn try_submit(&mut self, request: Request) -> Result<(), Request> {
        if self.vacant() == 0 {
            return Err(request);
        }
        let mut state = self.state.borrow_mut();
        state.log.push((request.operation, request.offset, request.len));
        state.pending.push_back(request);
        Ok(())
    }
    fn poll(&mut self, _wait: bool) -> stdio::Result<()> {
        if self.state.borrow().queue_error {
            Err(stdio::Error::other("injected queue failure"))
        } else {
            Ok(())
        }
    }
    fn pop(&mut self) -> Option<io::Completion> {
        self.state.borrow_mut().done.pop_front()
    }
}
fn manual(depth: usize) -> (Pipeline<ManualQueue>, Rc<RefCell<State>>, File) {
    let file = file();
    let state = Rc::new(RefCell::new(State::default()));
    let queue = ManualQueue {
        depth,
        state: state.clone(),
    };
    (Pipeline::new(queue, header(), limits(), 0).unwrap(), state, file)
}
fn finish(state: &Rc<RefCell<State>>, file: &File, index: usize, forced: Option<stdio::Result<usize>>) {
    let mut request = state.borrow_mut().pending.remove(index).unwrap();
    let result = forced.unwrap_or_else(|| match request.operation {
        Operation::Read => file.read_at(&mut request.buffer.as_mut().unwrap()[..request.len], request.offset),
        Operation::Write => file.write_at(&request.buffer.as_ref().unwrap()[..request.len], request.offset),
        Operation::Sync => file.sync_data().map(|()| 0),
    });
    state.borrow_mut().done.push_back(io::Completion { request, result });
}

#[test]
fn real_file_write_verified_read_flush_and_reopen() {
    let file = file();
    let queue = FileQueue::new(file.try_clone().unwrap(), 4).unwrap();
    let mut pipeline = Pipeline::new(queue, header(), limits(), 0).unwrap();
    let input = frame(1, 7, b"hello world");
    let buffer = AlignedBuf::zeroed(input.encoded_len());
    let address = buffer.as_ptr();
    let ticket = pipeline.write(&input, buffer).unwrap();
    assert!(!pipeline.contains(&key(1)));
    match drain(&mut pipeline).pop().unwrap() {
        Completion::Write {
            ticket: actual,
            result,
            buffer,
        } => {
            assert_eq!(ticket, actual);
            result.unwrap();
            assert_eq!(buffer.as_ptr(), address);
        }
        _ => panic!("expected write"),
    }
    let read = buffers();
    let address = read.metadata.as_ptr();
    pipeline.read(key(1), 6..11, read).unwrap();
    match drain(&mut pipeline).pop().unwrap() {
        Completion::Read { result, buffers, .. } => {
            assert_eq!(buffers.view(result.unwrap()), b"world");
            assert_eq!(buffers.metadata.as_ptr(), address);
        }
        _ => panic!("expected read"),
    }
    pipeline.flush().unwrap();
    assert!(matches!(
        drain(&mut pipeline).pop().unwrap(),
        Completion::Flush { result: Ok(()), .. }
    ));
    drop(pipeline);
    let mut disk = vec![0; CAPACITY as usize];
    file.read_exact_at(&mut disk, 0).unwrap();
    let recovered = SegmentHeader::decode(&disk, header().id().device_id, 0, CAPACITY).unwrap();
    let mut scanner = Scanner::new(recovered, limits());
    let mut reader = Pipeline::read_only(FileQueue::new(file, 2).unwrap(), recovered, limits(), 0).unwrap();
    while let Some(position) = scanner.position() {
        let Some(frame) = scanner.next_frame(&disk[position.offset() as usize..]).unwrap() else {
            break;
        };
        reader.restore(frame.metadata()).unwrap();
    }
    reader.read(key(1), 0..5, buffers()).unwrap();
    assert!(matches!(&drain(&mut reader)[0], Completion::Read { result: Ok(_), .. }));
    assert!(matches!(
        reader
            .write(&input, AlignedBuf::zeroed(input.encoded_len()))
            .unwrap_err()
            .error,
        Error::ReadOnly
    ));
}

#[test]
fn out_of_order_completions_wait_for_publication_and_flush() {
    let (mut pipeline, state, file) = manual(4);
    let first = write(&mut pipeline, 1, 1, b"one");
    let second = write(&mut pipeline, 2, 2, b"two");
    pipeline.flush().unwrap();
    finish(&state, &file, 1, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    assert!(out.is_empty());
    assert!(!pipeline.contains(&key(2)));
    assert!(state.borrow().log.iter().all(|entry| entry.0 != Operation::Sync));
    let input = frame(3, 3, b"blocked");
    assert!(matches!(
        pipeline
            .write(&input, AlignedBuf::zeroed(input.encoded_len()))
            .unwrap_err()
            .error,
        Error::Backpressure
    ));
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut out).unwrap();
    assert_eq!(
        out.iter().map(|c| c.ticket().number()).collect::<Vec<_>>(),
        [first, second]
    );
    assert_eq!(state.borrow().pending.front().unwrap().operation, Operation::Sync);
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut out).unwrap();
    assert!(matches!(out.last(), Some(Completion::Flush { result: Ok(()), .. })));
}

#[test]
fn short_write_poisons_dependent_writes_and_flush_without_publishing_them() {
    let (mut pipeline, state, file) = manual(4);
    let first = write(&mut pipeline, 1, 1, b"one");
    write(&mut pipeline, 2, 2, b"two");
    pipeline.flush().unwrap();
    finish(&state, &file, 1, None);
    finish(&state, &file, 0, Some(Ok(PAGE - 1)));
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    assert!(matches!(
        &out[0],
        Completion::Write {
            result: Err(Error::ShortIo { .. }),
            ..
        }
    ));
    assert!(matches!(&out[1], Completion::Write { result: Err(Error::WriteFailed(failed)), .. } if *failed == first));
    assert!(matches!(
        &out[2],
        Completion::Flush {
            result: Err(Error::WriteFailed(_)),
            ..
        }
    ));
    assert!(!pipeline.contains(&key(1)) && !pipeline.contains(&key(2)));
    assert!(state.borrow().log.iter().all(|entry| entry.0 != Operation::Sync));
    assert!(matches!(pipeline.flush(), Err(Error::WriteFailed(_))));
    assert_eq!(pipeline.in_flight(), 0);
}

#[test]
fn an_earlier_success_can_publish_after_a_later_write_fails() {
    let (mut pipeline, state, file) = manual(3);
    write(&mut pipeline, 1, 1, b"good");
    write(&mut pipeline, 2, 2, b"bad");
    finish(
        &state,
        &file,
        1,
        Some(Err(stdio::Error::other("injected write failure"))),
    );
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    assert!(out.is_empty());
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut out).unwrap();
    assert!(matches!(&out[0], Completion::Write { result: Ok(()), .. }));
    assert!(
        matches!(&out[1], Completion::Write { result: Err(Error::Io { source, .. }), .. } if source.to_string() == "injected write failure")
    );
    assert!(pipeline.contains(&key(1)) && !pipeline.contains(&key(2)));
}

#[test]
fn full_queue_rejection_preserves_buffer_and_does_not_allocate_a_hole() {
    let (mut pipeline, state, file) = manual(1);
    write(&mut pipeline, 1, 1, b"one");
    let input = frame(2, 2, b"two");
    let mut buffer = AlignedBuf::zeroed(input.encoded_len());
    buffer.fill(0x45);
    let address = buffer.as_ptr();
    let rejected = pipeline.write(&input, buffer).unwrap_err();
    assert!(matches!(rejected.error, Error::Backpressure));
    assert_eq!(rejected.input.as_ptr(), address);
    assert!(rejected.input.iter().all(|&byte| byte == 0x45));
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    pipeline.write(&input, rejected.input).unwrap();
    assert_eq!(state.borrow().pending.front().unwrap().offset, 2 * PAGE as u64);
}

#[test]
fn newest_lsn_wins_and_tombstones_prevent_resurrection() {
    let file = file();
    let mut pipeline = Pipeline::new(FileQueue::new(file, 4).unwrap(), header(), limits(), 0).unwrap();
    write(&mut pipeline, 1, 30, b"new");
    write(&mut pipeline, 1, 10, b"old");
    drain(&mut pipeline);
    pipeline.read(key(1), 0..3, buffers()).unwrap();
    match drain(&mut pipeline).pop().unwrap() {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), b"new"),
        _ => panic!("read"),
    }
    let mut deleted = FrameBuilder::new(limits());
    deleted.push_tombstone(key(1), 40).unwrap();
    pipeline
        .write(&deleted, AlignedBuf::zeroed(deleted.encoded_len()))
        .unwrap();
    write(&mut pipeline, 1, 35, b"late");
    drain(&mut pipeline);
    assert!(!pipeline.contains(&key(1)));
    assert!(matches!(
        pipeline.read(key(1), 0..1, buffers()).unwrap_err().error,
        Error::NotFound
    ));
}

#[test]
fn late_small_record_reads_metadata_and_value_without_intervening_payload() {
    let (mut pipeline, state, file) = manual(3);
    let value = vec![0x67; 1024];
    let mut input = FrameBuilder::new(limits());
    for i in 0..32 {
        input.push(key(i), i as u64, &value).unwrap();
    }
    pipeline.write(&input, AlignedBuf::zeroed(input.encoded_len())).unwrap();
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    pipeline.read(key(31), 0..1024, buffers()).unwrap();
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    let reads: Vec<_> = state
        .borrow()
        .log
        .iter()
        .filter(|entry| entry.0 == Operation::Read)
        .copied()
        .collect();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0], (Operation::Read, PAGE as u64, PAGE));
    assert!(reads[1].1 > reads[0].1 + PAGE as u64);
    assert_eq!(reads[1].2, PAGE);
    finish(&state, &file, 0, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    match out.pop().unwrap() {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), value.as_slice()),
        _ => panic!("read"),
    }
}

#[test]
fn range_crossing_checksum_blocks_reads_and_verifies_both_blocks() {
    let (mut pipeline, state, file) = manual(3);
    let value: Vec<_> = (0..70000).map(|i| (i % 251) as u8).collect();
    write(&mut pipeline, 1, 1, &value);
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    pipeline.read(key(1), 65530..65540, buffers()).unwrap();
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    assert_eq!(state.borrow().pending.front().unwrap().len, 18 * PAGE);
    finish(&state, &file, 0, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    match out.pop().unwrap() {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), &value[65530..65540]),
        _ => panic!("read"),
    }
}

#[test]
fn corrupt_metadata_stops_before_payload_io_and_returns_both_buffers() {
    let (mut pipeline, state, file) = manual(2);
    write(&mut pipeline, 1, 1, b"hello");
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    file.write_all_at(&[0xff], PAGE as u64 + 12).unwrap();
    pipeline.read(key(1), 0..5, buffers()).unwrap();
    finish(&state, &file, 0, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    assert!(matches!(
        &out[0],
        Completion::Read {
            result: Err(Error::Frame(_)),
            ..
        }
    ));
    assert!(state.borrow().pending.is_empty());
    assert_eq!(
        state
            .borrow()
            .log
            .iter()
            .filter(|entry| entry.0 == Operation::Read)
            .count(),
        1
    );
}

#[test]
fn truncated_and_corrupt_payload_reads_fail_without_poisoning_writes() {
    for short in [true, false] {
        let (mut pipeline, state, file) = manual(2);
        write(&mut pipeline, 1, 1, &vec![0x48; PAGE]);
        finish(&state, &file, 0, None);
        pipeline.poll(false, &mut Vec::new()).unwrap();
        pipeline.read(key(1), 0..5, buffers()).unwrap();
        finish(&state, &file, 0, None);
        pipeline.poll(false, &mut Vec::new()).unwrap();
        if !short {
            file.write_all_at(&[0xff], 2 * PAGE as u64).unwrap();
        }
        finish(&state, &file, 0, if short { Some(Ok(PAGE - 1)) } else { None });
        let mut out = Vec::new();
        pipeline.poll(false, &mut out).unwrap();
        assert!(matches!(&out[0], Completion::Read { result: Err(_), .. }));
        write(&mut pipeline, 2, 2, b"still writable");
    }
}

#[test]
fn empty_ranges_and_empty_values_need_metadata_only() {
    for value in [&b"hello"[..], &b""[..]] {
        let (mut pipeline, state, file) = manual(2);
        write(&mut pipeline, 1, 1, value);
        finish(&state, &file, 0, None);
        pipeline.poll(false, &mut Vec::new()).unwrap();
        let end = value.len() as u32;
        pipeline.read(key(1), end..end, buffers()).unwrap();
        finish(&state, &file, 0, None);
        let mut out = Vec::new();
        pipeline.poll(false, &mut out).unwrap();
        assert!(matches!(&out[0], Completion::Read { result: Ok(range), .. } if range.is_empty()));
        assert!(state.borrow().pending.is_empty());
    }
}

#[test]
fn a_read_keeps_its_admission_snapshot_after_an_overwrite() {
    let (mut pipeline, state, file) = manual(3);
    let mut old = vec![0; PAGE];
    old[..3].copy_from_slice(b"old");
    write(&mut pipeline, 1, 1, &old);
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    pipeline.read(key(1), 0..3, buffers()).unwrap();
    write(&mut pipeline, 1, 2, b"new");
    finish(&state, &file, 1, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    finish(&state, &file, 0, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    match out.pop().unwrap() {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), b"old"),
        _ => panic!("read"),
    }
}

#[test]
fn sync_failure_is_reported_and_stops_further_writes() {
    let (mut pipeline, state, file) = manual(2);
    pipeline.flush().unwrap();
    pipeline.poll(false, &mut Vec::new()).unwrap();
    finish(
        &state,
        &file,
        0,
        Some(Err(stdio::Error::other("injected sync failure"))),
    );
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    assert!(matches!(
        &out[0],
        Completion::Flush {
            result: Err(Error::Io {
                operation: Operation::Sync,
                ..
            }),
            ..
        }
    ));
    assert!(matches!(pipeline.flush(), Err(Error::WriteFailed(_))));
}

#[test]
fn fatal_queue_failure_is_sticky_and_retains_submitted_buffers() {
    let (mut pipeline, state, _) = manual(2);
    write(&mut pipeline, 1, 1, b"pending");
    state.borrow_mut().queue_error = true;
    assert!(matches!(pipeline.poll(false, &mut Vec::new()), Err(Error::Queue(_))));
    assert!(state.borrow().pending.front().unwrap().buffer.is_some());
    assert!(matches!(pipeline.poll(false, &mut Vec::new()), Err(Error::QueueFailed)));
    assert!(matches!(pipeline.flush(), Err(Error::QueueFailed)));
}

#[test]
fn queue_rejects_invalid_lengths_before_touching_file() {
    let file = file();
    let mut queue = FileQueue::new(file, 1).unwrap();
    let mut buffer = AlignedBuf::zeroed(PAGE);
    buffer.fill(0x77);
    queue
        .try_submit(Request {
            token: 1,
            operation: Operation::Write,
            offset: 0,
            len: PAGE + 1,
            buffer: Some(buffer),
        })
        .unwrap();
    let completion = queue.pop().unwrap();
    assert_eq!(completion.result.unwrap_err().kind(), stdio::ErrorKind::InvalidInput);
    assert!(completion.request.buffer.unwrap().iter().all(|&byte| byte == 0x77));
}

#[cfg(target_os = "linux")]
#[test]
fn uring_runs_the_same_small_read_write_flush_path() {
    let queue = io::UringQueue::new(file(), 4).unwrap();
    let mut pipeline = Pipeline::new(queue, header(), limits(), 0).unwrap();
    write(&mut pipeline, 1, 1, b"async");
    assert!(matches!(
        &drain(&mut pipeline)[0],
        Completion::Write { result: Ok(()), .. }
    ));
    pipeline.read(key(1), 0..5, buffers()).unwrap();
    match drain(&mut pipeline).pop().unwrap() {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), b"async"),
        _ => panic!("read"),
    }
    pipeline.flush().unwrap();
    assert!(matches!(
        &drain(&mut pipeline)[0],
        Completion::Flush { result: Ok(()), .. }
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn dropping_uring_with_accepted_writes_keeps_buffers_live_until_completion() {
    let file = file();
    let mut queue = io::UringQueue::new(file.try_clone().unwrap(), 2).unwrap();
    let mut buffer = AlignedBuf::zeroed(PAGE);
    buffer.fill(0x55);
    queue
        .try_submit(Request {
            token: 7,
            operation: Operation::Write,
            offset: PAGE as u64,
            len: PAGE,
            buffer: Some(buffer),
        })
        .unwrap();
    drop(queue);
    let mut bytes = vec![0; PAGE];
    file.read_exact_at(&mut bytes, PAGE as u64).unwrap();
    assert!(bytes.iter().all(|&byte| byte == 0x55));
}

#[test]
fn prepared_submission_keeps_the_original_payload_allocation() {
    let (mut pipeline, state, file) = manual(1);
    let value_len = 70000;
    let mut buffer = AlignedBuf::zeroed(PreparedFrame::required_len(limits(), value_len).unwrap());
    let address = buffer.as_ptr();
    let payload_address;
    {
        let mut prepared = PreparedFrame::new(limits(), value_len, &mut buffer).unwrap();
        payload_address = prepared.value_mut().as_ptr();
        prepared.value_mut().fill(0x39);
    }
    write(&mut pipeline, 2, 2, b"occupy slot");
    let rejected = pipeline.write_prepared(key(1), 1, value_len, buffer).unwrap_err();
    assert!(matches!(rejected.error, Error::Backpressure));
    let mut buffer = rejected.input;
    {
        let mut prepared = PreparedFrame::new(limits(), value_len, &mut buffer).unwrap();
        assert_eq!(prepared.value_mut().as_ptr(), payload_address);
        assert!(prepared.value_mut().iter().all(|&byte| byte == 0x39));
    }
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    pipeline.write_prepared(key(1), 1, value_len, buffer).unwrap();
    assert_eq!(
        state
            .borrow()
            .pending
            .front()
            .unwrap()
            .buffer
            .as_ref()
            .unwrap()
            .as_ptr(),
        address
    );
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    pipeline.read(key(1), 69990..70000, buffers()).unwrap();
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    finish(&state, &file, 0, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    match out.pop().unwrap() {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), &[0x39; 10]),
        _ => panic!("read"),
    }
}

#[test]
fn invalid_read_ranges_and_short_buffers_never_submit_io() {
    let (mut pipeline, state, file) = manual(2);
    let value = vec![3; 70000];
    write(&mut pipeline, 1, 1, &value);
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    for range in [70000..70001, std::ops::Range { start: 2, end: 1 }] {
        assert!(pipeline.read(key(1), range, buffers()).is_err());
    }
    let small = ReadBuffers {
        metadata: AlignedBuf::zeroed(PAGE),
        value: AlignedBuf::zeroed(PAGE),
    };
    let address = small.value.as_ptr();
    let rejected = pipeline.read(key(1), 1..2, small).unwrap_err();
    assert!(matches!(rejected.error, Error::Frame(_)));
    assert_eq!(rejected.input.value.as_ptr(), address);
    assert!(state.borrow().pending.is_empty());
    assert_eq!(pipeline.in_flight(), 0);
}

#[test]
fn assignment_uses_absolute_base_for_reads_and_writes() {
    let file = tempfile::tempfile().unwrap();
    let base = 2 * PAGE as u64;
    file.set_len(base + CAPACITY as u64).unwrap();
    let mut page = AlignedBuf::zeroed(PAGE);
    header().encode_into(&mut page).unwrap();
    file.write_all_at(&page, base).unwrap();
    file.sync_data().unwrap();
    let mut pipeline = Pipeline::new(
        FileQueue::new(file.try_clone().unwrap(), 2).unwrap(),
        header(),
        limits(),
        base,
    )
    .unwrap();
    write(&mut pipeline, 1, 1, b"offset");
    drain(&mut pipeline);
    let mut prefix = vec![1; base as usize];
    file.read_exact_at(&mut prefix, 0).unwrap();
    assert!(prefix.iter().all(|&byte| byte == 0));
    pipeline.read(key(1), 0..6, buffers()).unwrap();
    match drain(&mut pipeline).pop().unwrap() {
        Completion::Read { result, buffers, .. } => assert_eq!(buffers.view(result.unwrap()), b"offset"),
        _ => panic!("read"),
    }
}

#[test]
fn assigned_segment_full_returns_input_without_submitting_a_write() {
    let file = file();
    let small_header = SegmentHeader::new(header().id(), 3 * PAGE as u32).unwrap();
    let mut pipeline = Pipeline::new(FileQueue::new(file, 2).unwrap(), small_header, limits(), 0).unwrap();
    write(&mut pipeline, 1, 1, b"first");
    drain(&mut pipeline);
    let input = frame(2, 2, b"full");
    let mut buffer = AlignedBuf::zeroed(input.encoded_len());
    buffer.fill(0x44);
    let rejected = pipeline.write(&input, buffer).unwrap_err();
    assert!(matches!(
        rejected.error,
        Error::Segment(moat_engine_v2::segment::Error::Full { .. })
    ));
    assert!(rejected.input.iter().all(|&byte| byte == 0x44));
    assert_eq!(pipeline.in_flight(), 0);
}

#[test]
fn small_values_already_in_metadata_use_one_read_and_no_copy() {
    let (mut pipeline, state, file) = manual(2);
    write(&mut pipeline, 1, 1, b"inline");
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    let requirements = pipeline.read_requirements(key(1), 0..6).unwrap();
    assert_eq!(requirements.metadata_len, PAGE);
    assert_eq!(requirements.value_len, 0);
    let input = buffers();
    let address = input.metadata.as_ptr();
    pipeline.read(key(1), 0..6, input).unwrap();
    finish(&state, &file, 0, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    match out.pop().unwrap() {
        Completion::Read { result, buffers, .. } => {
            assert_eq!(buffers.metadata.as_ptr(), address);
            let range = result.unwrap();
            assert!(matches!(range, moat_engine_v2::pipeline::ReadRange::Metadata(_)));
            assert_eq!(buffers.view(range), b"inline");
        }
        _ => panic!("read"),
    }
    assert_eq!(
        state
            .borrow()
            .log
            .iter()
            .filter(|entry| entry.0 == Operation::Read)
            .count(),
        1
    );
    assert!(state.borrow().pending.is_empty());
}

#[test]
fn corrupt_inline_payload_is_rejected_without_a_second_read() {
    let (mut pipeline, state, file) = manual(2);
    write(&mut pipeline, 1, 1, b"hello");
    finish(&state, &file, 0, None);
    pipeline.poll(false, &mut Vec::new()).unwrap();
    file.write_all_at(&[0xff], PAGE as u64 + 136).unwrap();
    pipeline.read(key(1), 0..5, buffers()).unwrap();
    finish(&state, &file, 0, None);
    let mut out = Vec::new();
    pipeline.poll(false, &mut out).unwrap();
    assert!(matches!(
        &out[0],
        Completion::Read {
            result: Err(Error::Frame(moat_engine_v2::frame::Error::PayloadChecksum { .. })),
            ..
        }
    ));
    assert!(state.borrow().pending.is_empty());
}
