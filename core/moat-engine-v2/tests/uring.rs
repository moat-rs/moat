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
