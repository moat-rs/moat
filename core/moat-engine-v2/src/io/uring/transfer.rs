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

use std::io;

use io_uring::{opcode, squeue, types};

use super::super::{Completion, Operation, Request};

/// One logical operation; its buffer remains owned until all subrequests finish.
pub(super) struct Transfer {
    request: Request,
    ptr: *mut u8,
    fixed: Option<u16>,
    submitted: usize,
    in_flight: usize,
    result: io::Result<usize>,
}

impl Transfer {
    pub(super) fn new(mut request: Request, fixed: Option<u16>) -> Self {
        // Capture once, before any DMA starts. Later SQEs must not reborrow the
        // whole buffer while the kernel can be writing another subrange.
        let ptr = request.buffer.as_mut().map_or(std::ptr::null_mut(), |b| b.as_mut_ptr());
        Self {
            request,
            ptr,
            fixed,
            submitted: 0,
            in_flight: 0,
            result: Ok(0),
        }
    }

    pub(super) fn has_remaining(&self) -> bool {
        self.submitted < self.request.len
    }

    pub(super) fn next(&mut self, limit: usize) -> squeue::Entry {
        let len = limit.min(self.request.len - self.submitted) as u32;
        let offset = self.request.offset + self.submitted as u64;
        let ptr = self.ptr.wrapping_add(self.submitted);
        let fd = types::Fixed(0);
        let entry = match (self.request.operation, self.fixed) {
            (Operation::Read, Some(index)) => opcode::ReadFixed::new(fd, ptr, len, index).offset(offset).build(),
            (Operation::Write, Some(index)) => opcode::WriteFixed::new(fd, ptr, len, index).offset(offset).build(),
            (Operation::Read, None) => opcode::Read::new(fd, ptr, len).offset(offset).build(),
            (Operation::Write, None) => opcode::Write::new(fd, ptr, len).offset(offset).build(),
            (Operation::Sync, _) => opcode::Fsync::new(fd).flags(types::FsyncFlags::DATASYNC).build(),
        };
        self.submitted += len as usize;
        self.in_flight += 1;
        entry
    }

    pub(super) fn complete(&mut self, actual: i32) -> bool {
        self.in_flight -= 1;
        // Keep the first observed error, but still drain every subrequest before
        // returning the buffer. Short transfers stay short after aggregation.
        if let Ok(bytes) = &mut self.result {
            if actual < 0 {
                self.result = Err(io::Error::from_raw_os_error(-actual));
            } else {
                *bytes += actual as usize;
            }
        }
        !self.has_remaining() && self.in_flight == 0
    }

    pub(super) fn finish(self) -> Completion {
        Completion {
            request: self.request,
            result: self.result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moat_common::AlignedBuf;

    fn transfer() -> Transfer {
        Transfer::new(
            Request {
                token: 9,
                operation: Operation::Read,
                offset: 4096,
                len: 3 * 4096,
                buffer: Some(AlignedBuf::zeroed(3 * 4096).into()),
            },
            None,
        )
    }

    #[test]
    fn completion_waits_for_unscheduled_and_in_flight_parts() {
        let mut transfer = transfer();
        transfer.next(4096);
        assert!(!transfer.complete(4096), "unscheduled parts still own the buffer");
        transfer.next(4096);
        transfer.next(4096);
        assert!(!transfer.complete(4096));
        assert!(transfer.complete(4096));
        let done = transfer.finish();
        assert_eq!(done.request.token, 9);
        assert_eq!(done.result.unwrap(), 3 * 4096);
    }

    #[test]
    fn error_waits_for_every_completion_and_cannot_be_overwritten() {
        let mut transfer = transfer();
        for _ in 0..3 {
            transfer.next(4096);
        }
        assert!(!transfer.complete(-libc::EIO));
        assert!(!transfer.complete(4096));
        assert!(transfer.complete(-libc::EBADF));
        assert_eq!(transfer.finish().result.unwrap_err().raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn out_of_order_short_completion_is_not_full_success() {
        let mut transfer = transfer();
        for _ in 0..3 {
            transfer.next(4096);
        }
        // A short middle part can arrive before either full part.
        assert!(!transfer.complete(512));
        assert!(!transfer.complete(4096));
        assert!(transfer.complete(4096));
        assert_eq!(transfer.finish().result.unwrap(), 2 * 4096 + 512);
    }
}
