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

//! The io_uring implementation of [`IoQueue`].
//!
//! One ring per queue (and therefore per thread). The queue's buffer pool
//! arenas are registered as fixed buffers once, so every read and write is a
//! `READ_FIXED`/`WRITE_FIXED` with no per-operation page pinning. SQPOLL is
//! deliberately not used: a kernel polling thread per ring competes with the
//! pinned application threads for cores; the application already batches
//! submissions per poll cycle, which achieves the same syscall amortisation.

use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, OwnedFd},
    sync::Arc,
};

use io_uring::{IoUring, opcode, types};
use moat_common::{BufferPool, PooledBuf};

use crate::io::{IoCompletion, IoQueue, QueueOptions};

/// An io_uring backed queue over one file descriptor.
pub struct UringQueue {
    ring: IoUring,
    fd: OwnedFd,
    pool: Arc<BufferPool>,
    /// In-flight operations indexed by the slot number carried in `user_data`:
    /// the caller's token and the buffer to hand back.
    slots: Vec<Option<(u64, Option<PooledBuf>)>>,
    free_slots: Vec<u32>,
    unsubmitted: usize,
}

impl UringQueue {
    /// Creates a ring of `opts.depth` entries over `file` and registers the
    /// pool's arenas as fixed buffers.
    pub fn new(file: File, opts: &QueueOptions) -> io::Result<Self> {
        let depth = opts.depth.clamp(1, 32 * 1024).next_power_of_two();
        let ring = IoUring::builder().setup_cqsize(depth * 2).build(depth)?;
        let pool = BufferPool::new(opts.pool)?;
        let iovecs: Vec<libc::iovec> = pool
            .arenas()
            .iter()
            .map(|a| libc::iovec {
                iov_base: a.as_ptr().cast(),
                iov_len: a.len(),
            })
            .collect();
        // SAFETY: the arenas are owned by `pool`, which this queue keeps alive
        // for as long as the ring exists; they never move or shrink.
        unsafe { ring.submitter().register_buffers(&iovecs)? };
        Ok(Self {
            ring,
            fd: file.into(),
            pool,
            slots: (0..depth).map(|_| None).collect(),
            free_slots: (0..depth).rev().collect(),
            unsubmitted: 0,
        })
    }

    /// Reserves a slot for an operation, or fails if the ring is full.
    fn reserve(&mut self) -> io::Result<u32> {
        self.free_slots
            .pop()
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "ring full"))
    }

    fn push(
        &mut self,
        entry: io_uring::squeue::Entry,
        slot: u32,
        token: u64,
        buf: Option<PooledBuf>,
    ) -> io::Result<()> {
        if self.ring.submission().is_full() {
            self.submit()?;
        }
        // SAFETY: the buffer referenced by `entry` (if any) is stored in
        // `slots` below and stays there until the CQE for `slot` is reaped, so
        // the memory outlives the operation.
        if let Err(e) = unsafe { self.ring.submission().push(&entry) } {
            self.free_slots.push(slot);
            return Err(io::Error::new(io::ErrorKind::WouldBlock, e.to_string()));
        }
        self.slots[slot as usize] = Some((token, buf));
        self.unsubmitted += 1;
        Ok(())
    }

    fn reap(&mut self, out: &mut Vec<IoCompletion>) -> usize {
        let mut n = 0;
        let mut cq = self.ring.completion();
        cq.sync();
        for cqe in &mut cq {
            let slot = cqe.user_data() as u32;
            let res = cqe.result();
            let result = if res < 0 {
                Err(io::Error::from_raw_os_error(-res))
            } else {
                Ok(res as usize)
            };
            let (token, buf) = self.slots[slot as usize].take().expect("completion for a live slot");
            self.free_slots.push(slot);
            out.push(IoCompletion { token, result, buf });
            n += 1;
        }
        n
    }
}

impl IoQueue for UringQueue {
    fn pool(&self) -> &Arc<BufferPool> {
        &self.pool
    }

    fn read(&mut self, mut buf: PooledBuf, len: usize, offset: u64, token: u64) -> io::Result<()> {
        debug_assert!(len <= buf.capacity());
        let slot = self.reserve()?;
        let entry = opcode::ReadFixed::new(
            types::Fd(self.fd.as_raw_fd()),
            buf.as_mut_ptr(),
            len as u32,
            buf.arena_index(),
        )
        .offset(offset)
        .build()
        .user_data(slot as u64);
        self.push(entry, slot, token, Some(buf))
    }

    fn write(&mut self, buf: PooledBuf, len: usize, offset: u64, token: u64) -> io::Result<()> {
        debug_assert!(len <= buf.capacity());
        let slot = self.reserve()?;
        let entry = opcode::WriteFixed::new(
            types::Fd(self.fd.as_raw_fd()),
            buf.as_ptr(),
            len as u32,
            buf.arena_index(),
        )
        .offset(offset)
        .build()
        .user_data(slot as u64);
        self.push(entry, slot, token, Some(buf))
    }

    fn fsync(&mut self, token: u64) -> io::Result<()> {
        let slot = self.reserve()?;
        let entry = opcode::Fsync::new(types::Fd(self.fd.as_raw_fd()))
            .flags(types::FsyncFlags::DATASYNC)
            .build()
            .user_data(slot as u64);
        self.push(entry, slot, token, None)
    }

    fn submit(&mut self) -> io::Result<()> {
        if self.unsubmitted > 0 {
            self.ring.submit()?;
            self.unsubmitted = 0;
        }
        Ok(())
    }

    fn poll(&mut self, out: &mut Vec<IoCompletion>, wait: bool) -> io::Result<usize> {
        if wait && self.in_flight() > 0 {
            self.ring.submit_and_wait(1)?;
            self.unsubmitted = 0;
        } else {
            self.submit()?;
        }
        Ok(self.reap(out))
    }

    fn in_flight(&self) -> usize {
        self.slots.len() - self.free_slots.len()
    }

    fn depth(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use moat_common::{HugePages, PoolOptions};

    use super::*;

    #[test]
    fn fixed_buffer_write_then_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ring.img");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(1 << 20).unwrap();
        let opts = QueueOptions {
            depth: 8,
            pool: PoolOptions {
                bytes: 1 << 20,
                max_class: 64 << 10,
                huge_pages: HugePages::Disabled,
            },
            force_sync: false,
        };
        let mut queue = UringQueue::new(file, &opts).unwrap();
        assert_eq!(queue.depth(), 8);

        let mut buf = queue.pool().alloc(8192).unwrap();
        buf[..8192]
            .iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = (i % 251) as u8);
        queue.write(buf, 8192, 16384, 1).unwrap();
        let read_buf = queue.pool().alloc(8192).unwrap();
        queue.read(read_buf, 8192, 16384, 2).unwrap();
        assert_eq!(queue.in_flight(), 2);

        let mut done = Vec::new();
        while done.len() < 2 {
            queue.poll(&mut done, true).unwrap();
        }
        assert_eq!(queue.in_flight(), 0);
        // Reads and writes on the same range are unordered within a ring, so
        // only check what completed and that the buffer came back.
        for c in done {
            assert_eq!(c.result.unwrap(), 8192);
            assert!(c.buf.is_some());
        }
        // A second read after everything completed sees the written bytes.
        let read_buf = queue.pool().alloc(8192).unwrap();
        queue.read(read_buf, 8192, 16384, 3).unwrap();
        let mut done = Vec::new();
        queue.poll(&mut done, true).unwrap();
        let buf = done.pop().unwrap().buf.unwrap();
        assert!(buf[..8192].iter().enumerate().all(|(i, &b)| b == (i % 251) as u8));

        queue.fsync(4).unwrap();
        let mut done = Vec::new();
        queue.poll(&mut done, true).unwrap();
        assert!(done[0].buf.is_none());
        done[0].result.as_ref().unwrap();
    }
}
