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

use std::{collections::VecDeque, fs::File, io, os::unix::fs::FileExt};

use super::{Completion, Operation, Queue, Request, check_depth};

/// Blocking positional file I/O with deferred completion delivery.
///
/// This portable backend is intended for functional tests and local use. It
/// blocks at submission; the Linux `UringQueue` provides asynchronous submission.
/// The file may use direct I/O: transfer offsets, lengths, and buffers are aligned.
pub struct FileQueue {
    file: File,
    depth: usize,
    completed: VecDeque<Completion>,
}

impl FileQueue {
    /// Takes ownership of an already opened file and bounds retained completions.
    pub fn new(file: File, depth: usize) -> io::Result<Self> {
        check_depth(depth)?;
        Ok(Self {
            file,
            depth,
            completed: VecDeque::with_capacity(depth),
        })
    }
}

impl Queue for FileQueue {
    fn has_ready(&self) -> bool {
        !self.completed.is_empty()
    }
    fn depth(&self) -> usize {
        self.depth
    }
    fn vacant(&self) -> usize {
        self.depth - self.completed.len()
    }
    fn try_submit(&mut self, mut request: Request) -> Result<(), Request> {
        if self.completed.len() == self.depth {
            return Err(request);
        }
        let result = request.validate().and_then(|()| match request.operation {
            Operation::Read => self.file.read_at(
                &mut request.buffer.as_mut().expect("validated buffer")[..request.len],
                request.offset,
            ),
            Operation::Write => self.file.write_at(
                &request.buffer.as_ref().expect("validated buffer")[..request.len],
                request.offset,
            ),
            Operation::Sync => self.file.sync_data().map(|()| 0),
        });
        self.completed.push_back(Completion { request, result });
        Ok(())
    }
    fn poll(&mut self, _wait: bool) -> io::Result<()> {
        Ok(())
    }
    fn pop(&mut self) -> Option<Completion> {
        self.completed.pop_front()
    }
}
