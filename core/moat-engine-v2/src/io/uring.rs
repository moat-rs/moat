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

use std::{collections::VecDeque, fs::File, io, os::fd::AsRawFd};

use io_uring::{IoUring, opcode, types};

use super::{Completion, Operation, Queue, Request, check_depth};

/// A Linux io_uring queue with single-owner state and batched submission.
///
/// Requests own aligned buffers until their CQEs arrive. Buffers can be reused
/// by the caller after completion. This first backend uses ordinary READ/WRITE;
/// fixed-buffer registration and shared device routing remain later extensions.
pub struct UringQueue {
    ring: IoUring,
    file: File,
    pending: usize,
    slots: Vec<Option<Request>>,
    free: Vec<usize>,
    staged: Vec<io_uring::squeue::Entry>,
    completed: VecDeque<(usize, Completion)>,
}

impl UringQueue {
    /// Creates an asynchronous queue. Initialization errors are never downgraded
    /// silently to blocking I/O. The caller opens the file with its desired flags.
    pub fn new(file: File, depth: usize) -> io::Result<Self> {
        check_depth(depth)?;
        let ring = IoUring::new(depth.next_power_of_two() as u32)?;
        Ok(Self {
            ring,
            file,
            pending: 0,
            slots: (0..depth).map(|_| None).collect(),
            free: (0..depth).rev().collect(),
            staged: Vec::with_capacity(depth),
            completed: VecDeque::with_capacity(depth),
        })
    }

    fn reap(&mut self) {
        for cqe in &mut self.ring.completion() {
            let slot = cqe.user_data() as usize;
            self.pending -= 1;
            let request = self.slots[slot].take().expect("CQE for an owned request");
            let result = if cqe.result() < 0 {
                Err(io::Error::from_raw_os_error(-cqe.result()))
            } else {
                Ok(cqe.result() as usize)
            };
            self.completed.push_back((slot, Completion { request, result }));
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

    fn try_submit(&mut self, mut request: Request) -> Result<(), Request> {
        let Some(slot) = self.free.pop() else {
            return Err(request);
        };
        if let Err(error) = request.validate() {
            self.completed.push_back((
                slot,
                Completion {
                    request,
                    result: Err(error),
                },
            ));
            return Ok(());
        }
        let fd = types::Fd(self.file.as_raw_fd());
        let entry = match request.operation {
            Operation::Read => opcode::Read::new(
                fd,
                request.buffer.as_mut().expect("validated buffer").as_mut_ptr(),
                request.len as u32,
            )
            .offset(request.offset)
            .build(),
            Operation::Write => opcode::Write::new(
                fd,
                request.buffer.as_ref().expect("validated buffer").as_ptr(),
                request.len as u32,
            )
            .offset(request.offset)
            .build(),
            Operation::Sync => opcode::Fsync::new(fd).flags(types::FsyncFlags::DATASYNC).build(),
        }
        .user_data(slot as u64);
        self.pending += 1;
        self.slots[slot] = Some(request);
        self.staged.push(entry);
        Ok(())
    }

    fn poll(&mut self, wait: bool) -> io::Result<()> {
        {
            let mut sq = self.ring.submission();
            for entry in self.staged.drain(..) {
                // SAFETY: the request owns the referenced allocation in `slots`
                // until its CQE. Bounds and alignment were validated at admission.
                // At most `depth` slots exist, including entries still in the SQ.
                unsafe {
                    sq.push(&entry).expect("ring capacity covers accepted slots");
                }
            }
        }
        self.reap();
        if self.pending == 0 {
            return Ok(());
        }
        let want = usize::from(wait && self.completed.is_empty() && self.pending != 0);
        match self.ring.submit_and_wait(want) {
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
        while self.pending != 0 {
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
                    for request in self.slots.iter_mut().filter_map(Option::take) {
                        if let Some(buffer) = request.buffer {
                            std::mem::forget(buffer);
                        }
                    }
                }
                break;
            }
        }
    }
}
