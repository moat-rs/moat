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

//! Registered-buffer ownership and completion tests on small temporary files.
#![cfg(target_os = "linux")]

use std::{
    io,
    os::unix::fs::FileExt,
    sync::Arc,
    time::{Duration, Instant},
};

use moat_common::{AlignedBuf, BufferPool, HugePages, PAGE_SIZE, PoolOptions};
use moat_engine_v2::io::{Buffer, Completion, Operation, Queue, Request, UringQueue};

const PAGE: usize = PAGE_SIZE as usize;

fn pool() -> Arc<BufferPool> {
    BufferPool::new(PoolOptions {
        bytes: 128 * 1024,
        max_class: 64 * 1024,
        huge_pages: HugePages::Disabled,
    })
    .unwrap()
}

fn request(operation: Operation, buffer: impl Into<Buffer>) -> Request {
    Request {
        token: 37,
        operation,
        offset: PAGE as u64,
        len: PAGE,
        buffer: Some(buffer.into()),
    }
}

fn complete(queue: &mut UringQueue) -> Completion {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // DEFER_TASKRUN must make progress even without a blocking poll.
        queue.poll(false).unwrap();
        if let Some(completion) = queue.pop() {
            return completion;
        }
        assert!(Instant::now() < deadline, "nonblocking completion timed out");
        std::thread::yield_now();
    }
}

#[test]
fn registered_subranges_reuse_the_same_storage_and_keep_pool_alive() {
    let pool = pool();
    let weak = Arc::downgrade(&pool);
    let prefix = pool.alloc(PAGE).unwrap();
    let mut buffer = pool.alloc(PAGE).unwrap();
    assert_ne!(buffer.offset_in_arena(), 0);
    let address = buffer.as_ptr();
    buffer.fill(0x59);
    let file = tempfile::tempfile().unwrap();
    let mut queue = UringQueue::with_pool(file, 1, pool.clone()).unwrap();
    queue.try_submit(request(Operation::Write, buffer)).unwrap();
    let rejected = queue.try_submit(request(Operation::Read, prefix)).unwrap_err();
    assert_eq!(queue.vacant(), 0);
    let completion = complete(&mut queue);
    assert_eq!(completion.result.unwrap(), PAGE);
    assert_eq!(completion.request.token, 37);
    let mut buffer = completion.request.buffer.unwrap();
    assert_eq!(buffer.as_ptr(), address);
    buffer.fill(0);
    queue.try_submit(request(Operation::Read, buffer)).unwrap();
    let completion = complete(&mut queue);
    assert_eq!(completion.result.unwrap(), PAGE);
    let buffer = completion.request.buffer.unwrap();
    assert_eq!(buffer.as_ptr(), address);
    assert!(buffer.iter().all(|&b| b == 0x59));
    drop(rejected);
    drop(pool);
    drop(queue);
    assert!(weak.upgrade().is_some());
    drop(buffer);
    assert!(weak.upgrade().is_none());
}

#[test]
fn foreign_pool_is_rejected_without_touching_the_file() {
    let pool = pool();
    let foreign = self::pool();
    let mut buffer = foreign.alloc(PAGE).unwrap();
    buffer.fill(0x72);
    let address = buffer.as_ptr();
    let file = tempfile::tempfile().unwrap();
    let mut queue = UringQueue::with_pool(file.try_clone().unwrap(), 1, pool).unwrap();
    queue.try_submit(request(Operation::Write, buffer)).unwrap();
    let completion = complete(&mut queue);
    assert_eq!(completion.result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    let buffer = completion.request.buffer.unwrap();
    assert_eq!(buffer.as_ptr(), address);
    assert!(buffer.iter().all(|&b| b == 0x72));
    assert_eq!(file.metadata().unwrap().len(), 0);
}

#[test]
fn registered_queue_also_accepts_heap_buffers() {
    let file = tempfile::tempfile().unwrap();
    let mut queue = UringQueue::with_pool(file, 1, pool()).unwrap();
    let mut buffer = AlignedBuf::zeroed(PAGE);
    buffer.fill(0x38);
    queue.try_submit(request(Operation::Write, buffer)).unwrap();
    let completion = complete(&mut queue);
    assert_eq!(completion.result.unwrap(), PAGE);
    let mut buffer = completion.request.buffer.unwrap();
    buffer.fill(0);
    queue.try_submit(request(Operation::Read, buffer)).unwrap();
    let completion = complete(&mut queue);
    assert_eq!(completion.result.unwrap(), PAGE);
    assert!(completion.request.buffer.unwrap().iter().all(|&b| b == 0x38));
}

#[test]
fn short_and_failed_io_return_registered_buffers() {
    let pool = pool();
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut queue = UringQueue::with_pool(std::fs::File::open(file.path()).unwrap(), 1, pool.clone()).unwrap();
    let buffer = pool.alloc(PAGE).unwrap();
    let address = buffer.as_ptr();
    queue.try_submit(request(Operation::Read, buffer)).unwrap();
    let completion = complete(&mut queue);
    assert_eq!(completion.result.unwrap(), 0);
    let buffer = completion.request.buffer.unwrap();
    assert_eq!(buffer.as_ptr(), address);
    queue.try_submit(request(Operation::Write, buffer)).unwrap();
    let completion = complete(&mut queue);
    assert!(completion.result.is_err());
    assert_eq!(completion.request.buffer.unwrap().as_ptr(), address);
    assert_eq!(pool.in_use(), 0);
}

#[test]
fn dropping_pending_registered_writes_drains_before_releasing_memory() {
    let pool = pool();
    let file = tempfile::tempfile().unwrap();
    let mut queue = UringQueue::with_pool(file.try_clone().unwrap(), 2, pool.clone()).unwrap();
    let mut buffer = pool.alloc(PAGE).unwrap();
    buffer.fill(0x91);
    queue.try_submit(request(Operation::Write, buffer)).unwrap();
    drop(queue);
    assert_eq!(pool.in_use(), 0);
    let mut bytes = vec![0; PAGE];
    file.read_exact_at(&mut bytes, PAGE as u64).unwrap();
    assert!(bytes.iter().all(|&b| b == 0x91));
}

#[test]
fn registration_requires_the_pool_owner_thread() {
    let pool = pool();
    std::thread::spawn(move || {
        let error = UringQueue::with_pool(tempfile::tempfile().unwrap(), 1, pool)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    })
    .join()
    .unwrap();
}

const CHUNK: usize = 128 << 10;
const LARGE: usize = (4 << 20) + PAGE;

fn large_pool() -> Arc<BufferPool> {
    BufferPool::new(PoolOptions {
        bytes: 16 << 20,
        max_class: 8 << 20,
        huge_pages: HugePages::Disabled,
    })
    .unwrap()
}

fn large_request(operation: Operation, buffer: impl Into<Buffer>) -> Request {
    Request {
        len: LARGE,
        ..request(operation, buffer)
    }
}

#[test]
fn split_io_round_trips_with_fewer_ring_slots_than_parts() {
    for depth in [1, 2, 7] {
        let pool = large_pool();
        let file = tempfile::tempfile().unwrap();
        let mut queue = UringQueue::with_pool(file, depth, pool.clone())
            .unwrap()
            .with_max_io_len(CHUNK)
            .unwrap();
        let mut buffer = pool.alloc(LARGE).unwrap();
        let address = buffer.as_ptr();
        for (index, page) in buffer[..LARGE].chunks_mut(PAGE).enumerate() {
            page.fill((index % 251) as u8);
        }
        queue.try_submit(large_request(Operation::Write, buffer)).unwrap();
        let done = complete(&mut queue);
        assert_eq!(done.result.unwrap(), LARGE);
        assert_eq!(done.request.offset, PAGE as u64);
        let mut buffer = done.request.buffer.unwrap();
        buffer.fill(0xff);
        queue.try_submit(large_request(Operation::Read, buffer)).unwrap();
        let done = complete(&mut queue);
        assert_eq!(done.result.unwrap(), LARGE);
        assert_eq!(done.request.len, LARGE);
        assert_eq!(done.request.token, 37);
        let buffer = done.request.buffer.unwrap();
        assert_eq!(buffer.as_ptr(), address);
        for (index, page) in buffer[..LARGE].chunks(PAGE).enumerate() {
            assert!(page.iter().all(|b| *b == (index % 251) as u8));
        }
        assert!(queue.pop().is_none(), "one completion per logical request");
        assert_eq!(queue.vacant(), depth);
    }
}

#[test]
fn split_queue_preserves_logical_backpressure_and_interleaves_requests() {
    let file = tempfile::tempfile().unwrap();
    file.set_len((2 * LARGE + PAGE) as u64).unwrap();
    let mut queue = UringQueue::new(file, 2).unwrap().with_max_io_len(CHUNK).unwrap();
    queue
        .try_submit(large_request(Operation::Read, AlignedBuf::zeroed(LARGE)))
        .unwrap();
    let small = Request {
        token: 91,
        offset: (LARGE + PAGE) as u64,
        ..request(Operation::Read, AlignedBuf::zeroed(PAGE))
    };
    queue.try_submit(small).unwrap();
    assert_eq!(queue.vacant(), 0);
    let extra = request(Operation::Read, AlignedBuf::zeroed(PAGE));
    let address = extra.buffer.as_ref().unwrap().as_ptr();
    let extra = queue.try_submit(extra).unwrap_err();
    assert_eq!(extra.buffer.as_ref().unwrap().as_ptr(), address);
    let a = complete(&mut queue);
    let b = complete(&mut queue);
    let mut done = [a, b];
    done.sort_by_key(|c| c.request.token);
    assert_eq!(done[0].request.token, 37);
    assert_eq!(*done[0].result.as_ref().unwrap(), LARGE);
    assert_eq!(done[1].request.token, 91);
    assert_eq!(*done[1].result.as_ref().unwrap(), PAGE);
    assert!(queue.pop().is_none());
    assert_eq!(queue.vacant(), 2);
}

#[test]
fn split_short_reads_and_write_errors_return_the_original_buffer() {
    let file = tempfile::NamedTempFile::new().unwrap();
    file.as_file().set_len((PAGE + CHUNK + PAGE) as u64).unwrap();
    let pool = large_pool();
    let mut queue = UringQueue::with_pool(std::fs::File::open(file.path()).unwrap(), 2, pool.clone())
        .unwrap()
        .with_max_io_len(CHUNK)
        .unwrap();
    let buffer = pool.alloc(LARGE).unwrap();
    let address = buffer.as_ptr();
    queue.try_submit(large_request(Operation::Read, buffer)).unwrap();
    let done = complete(&mut queue);
    assert_eq!(done.result.unwrap(), CHUNK + PAGE);
    let buffer = done.request.buffer.unwrap();
    assert_eq!(buffer.as_ptr(), address);
    queue.try_submit(large_request(Operation::Write, buffer)).unwrap();
    let done = complete(&mut queue);
    assert!(done.result.is_err());
    assert_eq!(done.request.buffer.as_ref().unwrap().as_ptr(), address);
    drop(done);
    assert_eq!(pool.in_use(), 0);
    assert!(queue.pop().is_none());
}

#[test]
fn drop_drains_split_writes_that_have_not_been_submitted_yet() {
    let file = tempfile::tempfile().unwrap();
    let pool = large_pool();
    let mut queue = UringQueue::with_pool(file.try_clone().unwrap(), 1, pool.clone())
        .unwrap()
        .with_max_io_len(CHUNK)
        .unwrap();
    let mut buffer = pool.alloc(LARGE).unwrap();
    buffer[..LARGE].fill(0x63);
    queue.try_submit(large_request(Operation::Write, buffer)).unwrap();
    drop(queue);
    assert_eq!(pool.in_use(), 0);
    let mut bytes = vec![0; LARGE];
    file.read_exact_at(&mut bytes, PAGE as u64).unwrap();
    assert!(bytes.iter().all(|b| *b == 0x63));
}

#[test]
fn io_limit_is_aligned_and_cannot_be_raised() {
    for invalid in [0, 1, PAGE + 1] {
        assert!(
            UringQueue::new(tempfile::tempfile().unwrap(), 1)
                .unwrap()
                .with_max_io_len(invalid)
                .is_err()
        );
    }
    let queue = UringQueue::new(tempfile::tempfile().unwrap(), 1)
        .unwrap()
        .with_max_io_len(CHUNK)
        .unwrap()
        .with_max_io_len(2 * CHUNK)
        .unwrap();
    assert_eq!(queue.max_io_len(), CHUNK);
}
