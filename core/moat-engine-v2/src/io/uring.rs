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

use std::{
    collections::VecDeque,
    fs::File,
    io,
    marker::PhantomData,
    os::{fd::AsRawFd, unix::fs::FileTypeExt},
    rc::Rc,
    sync::Arc,
};

use io_uring::{IoUring, types};

use moat_common::{BufferPool, PAGE_SIZE, align_down, is_aligned};

use super::{Buffer, Completion, Queue, Request, check_depth};

mod transfer;
use transfer::Transfer;

// Linux UAPI: IORING_ENTER_GETEVENTS.
const ENTER_GETEVENTS: u32 = 1;

/// A Linux io_uring queue with single-owner state and batched submission.
///
/// Requests own aligned buffers until their CQEs arrive. Buffers can be reused
/// by the caller after completion. Pool arenas and the file are registered once;
/// heap buffers still use ordinary READ/WRITE. Block-device requests are split at
/// the device byte limit. Subrequests share the original buffer and produce one
/// logical completion; both accepted requests and in-flight SQEs are bounded by
/// `depth`. Create and drive the queue on the same thread, as required by
/// deferred task execution and the pool allocator.
pub struct UringQueue {
    ring: IoUring,
    // The ring must be destroyed before registered storage and the file.
    pool: Option<Arc<BufferPool>>,
    _file: File,
    deferred: bool,
    _owner: PhantomData<Rc<()>>,
    pending: usize,
    max_io_len: usize,
    slots: Vec<Option<Transfer>>,
    free: Vec<usize>,
    ready: VecDeque<usize>,
    completed: VecDeque<(usize, Completion)>,
}

impl UringQueue {
    /// Creates an asynchronous queue. Initialization errors are never downgraded
    /// silently to blocking I/O. The caller opens the file with its desired flags.
    pub fn new(file: File, depth: usize) -> io::Result<Self> {
        Self::build(file, depth, None)
    }

    /// Registers the pool's arenas once, including their huge-page backing.
    /// Pool buffers must belong to this pool; heap buffers remain supported.
    /// Registration failures are returned rather than silently disabling fixed I/O.
    pub fn with_pool(file: File, depth: usize, pool: Arc<BufferPool>) -> io::Result<Self> {
        if !pool.is_home() || pool.arenas().len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid pool owner or arena count",
            ));
        }
        Self::build(file, depth, Some(pool))
    }

    /// Whether SINGLE_ISSUER and DEFER_TASKRUN were enabled together.
    pub fn deferred_taskrun(&self) -> bool {
        self.deferred
    }

    /// Maximum bytes per read/write SQE. Block devices supply their queue limit;
    /// regular files use the maximum aligned request length unless capped below.
    pub fn max_io_len(&self) -> usize {
        self.max_io_len
    }

    /// Caps individual SQEs without changing logical request sizes. Call before
    /// submitting work. The cap must be a nonzero multiple of 4 KiB and cannot
    /// raise a previously established limit. Filesystem I/O may still offload.
    pub fn with_max_io_len(mut self, bytes: usize) -> io::Result<Self> {
        if bytes == 0 || !is_aligned(bytes as u64, PAGE_SIZE) || self.vacant() != self.depth() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid I/O limit or nonempty queue",
            ));
        }
        self.max_io_len = self.max_io_len.min(bytes);
        Ok(self)
    }

    fn build(file: File, depth: usize, pool: Option<Arc<BufferPool>>) -> io::Result<Self> {
        check_depth(depth)?;
        let max_io_len = device_io_len(&file)?;
        let entries = depth.next_power_of_two() as u32;
        let (ring, deferred) = match IoUring::builder()
            .setup_cqsize(entries * 2)
            .setup_single_issuer()
            .setup_defer_taskrun()
            .build(entries)
        {
            Ok(ring) => (ring, true),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                (IoUring::builder().setup_cqsize(entries * 2).build(entries)?, false)
            }
            Err(error) => return Err(error),
        };
        ring.submitter().register_files(&[file.as_raw_fd()])?;
        if let Some(pool) = &pool {
            let iovecs: Vec<_> = pool
                .arenas()
                .iter()
                .map(|arena| libc::iovec {
                    iov_base: arena.as_ptr().cast(),
                    iov_len: arena.len(),
                })
                .collect();
            // SAFETY: the queue retains the pool until after the ring is closed.
            // Arenas never move, and each submitted buffer exclusively owns its
            // subrange. The temporary iovec array is copied by registration.
            unsafe { ring.submitter().register_buffers(&iovecs)? };
        }
        Ok(Self {
            ring,
            pool,
            _file: file,
            deferred,
            _owner: PhantomData,
            pending: 0,
            max_io_len,
            slots: (0..depth).map(|_| None).collect(),
            free: (0..depth).rev().collect(),
            ready: VecDeque::with_capacity(depth),
            completed: VecDeque::with_capacity(depth),
        })
    }

    fn buffer_index(&self, request: &Request) -> io::Result<Option<u16>> {
        match (&self.pool, &request.buffer) {
            (Some(pool), Some(Buffer::Pooled(buffer))) => {
                if !Arc::ptr_eq(pool, buffer.pool()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "buffer belongs to another pool",
                    ));
                }
                Ok(Some(buffer.arena_index()))
            }
            _ => Ok(None),
        }
    }

    fn stage(&mut self) {
        let mut sq = self.ring.submission();
        while self.pending < self.slots.len() {
            let Some(slot) = self.ready.pop_front() else { break };
            let transfer = self.slots[slot].as_mut().expect("queued transfer");
            let entry = transfer.next(self.max_io_len).user_data(slot as u64);
            // SAFETY: the transfer retains the original allocation until every
            // subrequest completes. Subranges are disjoint and bounds-checked at
            // admission. Staged and submitted SQEs together cannot exceed depth.
            unsafe {
                sq.push(&entry).expect("ring capacity covers in-flight SQEs");
            }
            self.pending += 1;
            if transfer.has_remaining() {
                // Round-robin scheduling keeps large requests from monopolizing SQEs.
                self.ready.push_back(slot);
            }
        }
    }

    fn reap(&mut self) {
        for cqe in &mut self.ring.completion() {
            let slot = cqe.user_data() as usize;
            self.pending -= 1;
            let transfer = self.slots[slot].as_mut().expect("CQE for an owned transfer");
            if transfer.complete(cqe.result()) {
                let transfer = self.slots[slot].take().expect("completed transfer");
                self.completed.push_back((slot, transfer.finish()));
            }
        }
    }
}

impl Queue for UringQueue {
    fn depth(&self) -> usize {
        self.slots.len()
    }
    fn vacant(&self) -> usize {
        self.free.len()
    }

    fn try_submit(&mut self, request: Request) -> Result<(), Request> {
        let Some(slot) = self.free.pop() else {
            return Err(request);
        };
        let fixed = match request.validate().and_then(|()| self.buffer_index(&request)) {
            Ok(index) => index,
            Err(error) => {
                self.completed.push_back((
                    slot,
                    Completion {
                        request,
                        result: Err(error),
                    },
                ));
                return Ok(());
            }
        };
        self.slots[slot] = Some(Transfer::new(request, fixed));
        self.ready.push_back(slot);
        Ok(())
    }

    fn poll(&mut self, wait: bool) -> io::Result<()> {
        // Deferred task work is driven by the enter below. Other rings may have
        // completed work since the last poll; reclaim its SQE budget first.
        if !self.deferred {
            self.reap();
        }
        self.stage();
        if self.pending == 0 {
            return Ok(());
        }
        let want = usize::from(wait && self.completed.is_empty() && self.pending != 0);
        let result = if self.deferred {
            let to_submit = self.ring.submission().len() as u32;
            // SAFETY: no extended arguments or userspace pointers are passed.
            // GETEVENTS is needed even for nonblocking polls with DEFER_TASKRUN.
            unsafe {
                self.ring
                    .submitter()
                    .enter::<libc::sigset_t>(to_submit, want as u32, ENTER_GETEVENTS, None)
            }
        } else {
            self.ring.submit_and_wait(want)
        };
        match result {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        self.reap();
        Ok(())
    }

    fn pop(&mut self) -> Option<Completion> {
        self.completed.pop_front().map(|(slot, completion)| {
            self.free.push(slot);
            completion
        })
    }
}

impl Drop for UringQueue {
    fn drop(&mut self) {
        // A ring fd closing is not a substitute for retaining userspace buffers.
        // Drain accepted work before allocations can be freed, even if the owner
        // drops the pipeline without polling its final completions.
        while self.pending != 0 || !self.ready.is_empty() {
            if self.poll(true).is_err() {
                let cancelled = self
                    .ring
                    .submitter()
                    .register_sync_cancel(None, types::CancelBuilder::any())
                    .is_ok();
                if !cancelled {
                    // On a broken ring or a kernel without synchronous cancellation,
                    // retain only possibly-live allocations rather than risk UAF.
                    // This exceptional leak is bounded by queue depth and buffers.
                    for transfer in self.slots.iter_mut().filter_map(Option::take) {
                        if let Some(buffer) = transfer.finish().request.buffer {
                            std::mem::forget(buffer);
                        }
                    }
                }
                break;
            }
        }
    }
}

fn device_io_len(file: &File) -> io::Result<usize> {
    if !file.metadata()?.file_type().is_block_device() {
        return Ok(align_down(i32::MAX as u64, PAGE_SIZE) as usize);
    }
    let mut sectors: libc::c_ushort = 0;
    // SAFETY: BLKSECTGET writes one unsigned short to the live output variable.
    // It reports the queue's byte limit in 512-byte sectors, including partitions.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::_IO(0x12, 103), &mut sectors) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = align_down(u64::from(sectors) * 512, PAGE_SIZE) as usize;
    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "device I/O limit is below 4 KiB",
        ));
    }
    Ok(bytes)
}
