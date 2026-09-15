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

//! Bounded, single-owner I/O with buffers returned on completion.
//!
//! No queue-internal mutex or background worker is required. A caller must poll
//! the queue to submit/reap work. Queue fullness returns the original request.

#[cfg(unix)]
mod file;
#[cfg(target_os = "linux")]
mod uring;

use std::io;

#[cfg(unix)]
pub use file::FileQueue;
use moat_common::{AlignedBuf, PAGE_SIZE, is_aligned};
#[cfg(target_os = "linux")]
pub use uring::UringQueue;

/// Physical operation. A sync is a barrier only after preceding writes complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Read into the owned buffer.
    Read,
    /// Write from the owned buffer.
    Write,
    /// Persist preceding completed writes.
    Sync,
}

/// An operation and the memory that must outlive it.
#[derive(Debug)]
pub struct Request {
    /// Caller token, returned unchanged on completion.
    pub token: u64,
    /// Physical operation.
    pub operation: Operation,
    /// Absolute file/device byte offset.
    pub offset: u64,
    /// Prefix length to transfer; zero for sync.
    pub len: usize,
    /// Exclusively owned storage, absent for sync.
    pub buffer: Option<AlignedBuf>,
}

impl Request {
    pub(super) fn validate(&self) -> io::Result<()> {
        let valid = match self.operation {
            Operation::Sync => self.buffer.is_none() && self.len == 0 && self.offset == 0,
            Operation::Read | Operation::Write => {
                self.len > 0
                    && self.len <= i32::MAX as usize
                    && is_aligned(self.len as u64, PAGE_SIZE)
                    && is_aligned(self.offset, PAGE_SIZE)
                    && self.offset.checked_add(self.len as u64).is_some()
                    && self.buffer.as_ref().is_some_and(|buffer| self.len <= buffer.len())
            }
        };
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid aligned I/O request",
            ));
        }
        Ok(())
    }
}

/// Completed operation, including its original buffer even after an I/O error.
#[derive(Debug)]
pub struct Completion {
    /// Original request and buffer ownership.
    pub request: Request,
    /// Actual transferred bytes; short transfers are not successful full writes.
    pub result: io::Result<usize>,
}

/// A bounded queue driven exclusively through a mutable owner.
///
/// Accepted requests must produce exactly one completion with their original
/// token, operation, offset, length, and buffer. A rejected request must not have
/// reached the device. `depth` includes completions not yet popped. Implementors
/// must retain buffers until the OS stops accessing them, including during drop.
pub trait Queue {
    /// Maximum accepted requests, including completed requests not yet popped.
    fn depth(&self) -> usize;
    /// Slots available right now. With exclusive ownership, a request must be
    /// accepted whenever this is nonzero.
    fn vacant(&self) -> usize;
    /// Accepts a request or returns it unchanged when full.
    fn try_submit(&mut self, request: Request) -> Result<(), Request>;
    /// Drives submission/completion; waits for progress only when work is pending.
    fn poll(&mut self, wait: bool) -> io::Result<()>;
    /// Removes one completion and releases its queue capacity.
    fn pop(&mut self) -> Option<Completion>;
}

pub(super) fn check_depth(depth: usize) -> io::Result<()> {
    if depth == 0 || depth > 32768 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "queue depth must be in 1..=32768",
        ));
    }
    Ok(())
}
