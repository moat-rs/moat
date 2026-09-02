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

//! Asynchronous, batched I/O queues.
//!
//! The data path never blocks on a single I/O. A thread owns an [`IoQueue`],
//! submits reads and writes against buffers from the queue's own
//! [`BufferPool`], and later reaps [`IoCompletion`]s. Buffers are *moved* into
//! the queue for the duration of the operation and handed back with the
//! completion, so ownership is always unambiguous and no lifetime crosses the
//! submission boundary.
//!
//! Two implementations exist: io_uring with registered fixed buffers (Linux,
//! the production path) and [`SyncQueue`], which performs each operation
//! immediately on a blocking device and merely defers the completion. The sync
//! queue keeps the engine portable and deterministic under test; every piece of
//! engine logic is identical on both.

use std::{collections::VecDeque, io, sync::Arc};

use moat_common::{BufferPool, PoolOptions, PooledBuf};

/// Configuration of one [`IoQueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueOptions {
    /// Maximum operations in flight.
    pub depth: u32,
    /// The buffer pool owned by the queue.
    pub pool: PoolOptions,
    /// Use the blocking implementation even where io_uring is available.
    pub force_sync: bool,
}

impl Default for QueueOptions {
    fn default() -> Self {
        Self {
            depth: 256,
            pool: PoolOptions::default(),
            force_sync: false,
        }
    }
}

/// A finished operation. `buf` is the buffer the operation was issued with
/// (absent for `fsync`).
pub struct IoCompletion {
    /// The token passed at submission.
    pub token: u64,
    /// Bytes transferred, or the OS error.
    pub result: io::Result<usize>,
    /// The buffer handed back to the caller.
    pub buf: Option<PooledBuf>,
}

/// An asynchronous I/O queue bound to one device and one thread.
///
/// `read`/`write`/`fsync` only enqueue; [`IoQueue::submit`] pushes queued
/// operations to the device and [`IoQueue::poll`] reaps completions (and
/// submits first). Enqueueing fails with [`io::ErrorKind::WouldBlock`] when
/// `depth` operations are already in flight; callers poll and retry.
pub trait IoQueue: Send {
    /// The pool every buffer passed to this queue must come from.
    fn pool(&self) -> &Arc<BufferPool>;

    /// Reads `len` bytes at `offset` into the start of `buf`.
    fn read(&mut self, buf: PooledBuf, len: usize, offset: u64, token: u64) -> io::Result<()>;

    /// Writes the first `len` bytes of `buf` at `offset`.
    fn write(&mut self, buf: PooledBuf, len: usize, offset: u64, token: u64) -> io::Result<()>;

    /// Flushes the device's volatile write cache once all previously
    /// *completed* writes are on the device. Ordering against in-flight writes
    /// is the caller's responsibility (wait for them first).
    fn fsync(&mut self, token: u64) -> io::Result<()>;

    /// Pushes enqueued operations to the device without waiting.
    fn submit(&mut self) -> io::Result<()>;

    /// Submits, then collects finished operations into `out`. With `wait` set
    /// and operations in flight, blocks until at least one completes.
    fn poll(&mut self, out: &mut Vec<IoCompletion>, wait: bool) -> io::Result<usize>;

    /// Operations submitted but not yet reaped.
    fn in_flight(&self) -> usize;

    /// Maximum operations in flight; enqueueing beyond it fails.
    fn depth(&self) -> usize;
}

/// The blocking primitives a [`SyncQueue`] is built on.
pub trait BlockingIo: Send + 'static {
    /// Reads `buf.len()` bytes at `offset`.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// Writes `buf` at `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Flushes the write cache.
    fn sync(&self) -> io::Result<()>;
}

/// How a [`SyncQueue`] orders its deferred completions. Reverse order is a
/// test aid: it exercises every consumer's handling of out-of-order
/// completion, which io_uring produces routinely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionOrder {
    /// Completions are reported in submission order.
    #[default]
    Fifo,
    /// Completions are reported in reverse submission order per poll.
    Reverse,
}

/// An [`IoQueue`] that performs each operation synchronously at submission and
/// reports the completion on the next poll.
pub struct SyncQueue<B: BlockingIo> {
    io: B,
    pool: Arc<BufferPool>,
    depth: usize,
    done: VecDeque<IoCompletion>,
    order: CompletionOrder,
}

impl<B: BlockingIo> SyncQueue<B> {
    /// Creates a queue over `io` with a fresh pool.
    pub fn new(io: B, opts: &QueueOptions, order: CompletionOrder) -> io::Result<Self> {
        Ok(Self {
            io,
            pool: BufferPool::new(opts.pool)?,
            depth: opts.depth.max(1) as usize,
            done: VecDeque::new(),
            order,
        })
    }

    fn check_capacity(&self) -> io::Result<()> {
        if self.done.len() >= self.depth {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "queue full"));
        }
        Ok(())
    }
}

impl<B: BlockingIo> IoQueue for SyncQueue<B> {
    fn pool(&self) -> &Arc<BufferPool> {
        &self.pool
    }

    fn read(&mut self, mut buf: PooledBuf, len: usize, offset: u64, token: u64) -> io::Result<()> {
        self.check_capacity()?;
        let result = self.io.read_at(&mut buf[..len], offset).map(|()| len);
        self.done.push_back(IoCompletion {
            token,
            result,
            buf: Some(buf),
        });
        Ok(())
    }

    fn write(&mut self, buf: PooledBuf, len: usize, offset: u64, token: u64) -> io::Result<()> {
        self.check_capacity()?;
        let result = self.io.write_at(&buf[..len], offset).map(|()| len);
        self.done.push_back(IoCompletion {
            token,
            result,
            buf: Some(buf),
        });
        Ok(())
    }

    fn fsync(&mut self, token: u64) -> io::Result<()> {
        self.check_capacity()?;
        let result = self.io.sync().map(|()| 0);
        self.done.push_back(IoCompletion {
            token,
            result,
            buf: None,
        });
        Ok(())
    }

    fn submit(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn poll(&mut self, out: &mut Vec<IoCompletion>, _wait: bool) -> io::Result<usize> {
        let n = self.done.len();
        match self.order {
            CompletionOrder::Fifo => out.extend(self.done.drain(..)),
            CompletionOrder::Reverse => out.extend(self.done.drain(..).rev()),
        }
        Ok(n)
    }

    fn in_flight(&self) -> usize {
        self.done.len()
    }

    fn depth(&self) -> usize {
        self.depth
    }
}
