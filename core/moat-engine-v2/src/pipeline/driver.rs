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

use super::{Completion, Error, Pending, Pipeline, ReadBuffers, ReadRange, Result, index};
use crate::io::{self, Operation, Queue};

impl<Q: Queue> Pipeline<Q> {
    /// Submits/reaps I/O and appends completed operations to the caller's vector.
    /// Write notifications follow submission order. Reads may finish independently.
    /// A fatal queue error requires abandoning this pipeline; pending submitted
    /// buffers remain owned by the queue until it can safely release them.
    pub fn poll(&mut self, wait: bool, out: &mut Vec<Completion>) -> Result<usize> {
        if self.queue_failed {
            return Err(Error::QueueFailed);
        }
        let before = out.len();
        self.start_flush(out);
        while let Some(slot) = self.ready_reads.pop_front() {
            let Some(Pending::EmptyRead { ticket, buffers }) = self.slots[slot].take() else {
                unreachable!()
            };
            self.free.push(slot);
            out.push(Completion::Read {
                ticket,
                result: Ok(ReadRange::Value(0..0)),
                buffers,
            });
        }
        if let Err(error) = self.queue.poll(wait && out.len() == before) {
            self.queue_failed = true;
            return Err(Error::Queue(error));
        }
        while let Some(completion) = self.queue.pop() {
            self.complete(completion, out)?;
        }
        self.retire_writes(out);
        self.start_flush(out);
        Ok(out.len() - before)
    }

    fn complete(&mut self, completion: io::Completion, out: &mut Vec<Completion>) -> Result<()> {
        let io::Completion { mut request, result } = completion;
        let slot = request.token as usize;
        let Some(pending) = self.slots.get_mut(slot).and_then(Option::take) else {
            self.queue_failed = true;
            return Err(Error::InvalidArgument("completion token is not live"));
        };
        let result = match result {
            Ok(actual) if actual == request.len => Ok(()),
            Ok(actual) => Err(Error::ShortIo {
                operation: request.operation,
                offset: request.offset,
                expected: request.len,
                actual,
            }),
            Err(source) => Err(Error::Io {
                operation: request.operation,
                offset: request.offset,
                source,
            }),
        };
        match pending {
            Pending::Read(read) => {
                out.push(Completion::Read {
                    ticket: read.ticket,
                    result: result.map(|()| ReadRange::Value(read.range)),
                    buffers: ReadBuffers {
                        metadata: read.metadata,
                        value: request.buffer.take().expect("read returned its buffer"),
                    },
                });
                self.free.push(slot);
            }
            Pending::EmptyRead { .. } => unreachable!("empty reads do not submit I/O"),
            Pending::Write(mut write) => {
                if result.is_err() {
                    self.failed_at = Some(self.failed_at.map_or(write.ticket.0, |old| old.min(write.ticket.0)));
                }
                write.completed = Some((result, request.buffer.take().expect("write returned its buffer")));
                self.slots[slot] = Some(Pending::Write(write));
            }
            Pending::VerifiedRead(mut read) => {
                let metadata_phase = read.metadata.is_none();
                if metadata_phase {
                    read.metadata = request.buffer.take();
                } else {
                    read.value = request.buffer.take();
                }
                let position = self.position(read.location.frame_offset)?;
                let result = result.and_then(|()| {
                    if metadata_phase {
                        read.finish_metadata(self.limits, position)
                    } else {
                        read.verify().map(Some)
                    }
                });
                if matches!(result, Ok(None)) {
                    let buffer = read.value.take().expect("reserved value buffer");
                    let offset = read.io_offset;
                    let len = read.io_len;
                    self.slots[slot] = Some(Pending::VerifiedRead(read));
                    self.submit(slot, Operation::Read, offset, len, Some(buffer));
                } else {
                    let ticket = read.ticket;
                    out.push(Completion::Read {
                        ticket,
                        result: result.map(|range| range.expect("read has completed")),
                        buffers: read.buffers(),
                    });
                    self.free.push(slot);
                }
            }
            Pending::Flush { ticket, .. } => {
                if result.is_err() {
                    self.failed_at = Some(ticket.0);
                }
                out.push(Completion::Flush { ticket, result });
                self.flush = None;
                self.free.push(slot);
            }
        }
        Ok(())
    }

    fn retire_writes(&mut self, out: &mut Vec<Completion>) {
        while let Some(&slot) = self.writes.front() {
            if !matches!(&self.slots[slot], Some(Pending::Write(write)) if write.completed.is_some()) {
                break;
            }
            self.writes.pop_front();
            let Some(Pending::Write(mut write)) = self.slots[slot].take() else {
                unreachable!()
            };
            let (mut result, buffer) = write.completed.take().expect("completed write");
            if let Some(failed) = self.failed_at
                && write.ticket.0 > failed
            {
                result = Err(Error::WriteFailed(failed));
            }
            if result.is_ok() {
                index::apply(&mut self.index, write.entries);
            }
            out.push(Completion::Write {
                ticket: write.ticket,
                result,
                buffer,
            });
            self.free.push(slot);
        }
    }

    fn start_flush(&mut self, out: &mut Vec<Completion>) {
        let Some(slot) = self.flush else {
            return;
        };
        if !self.writes.is_empty() {
            return;
        }
        let Some(Pending::Flush { ticket, submitted }) = self.slots[slot].as_ref() else {
            unreachable!()
        };
        if *submitted {
            return;
        }
        let ticket = *ticket;
        if let Some(failed) = self.failed_at {
            self.slots[slot] = None;
            self.free.push(slot);
            self.flush = None;
            out.push(Completion::Flush {
                ticket,
                result: Err(Error::WriteFailed(failed)),
            });
            return;
        }
        self.slots[slot] = Some(Pending::Flush {
            ticket,
            submitted: true,
        });
        self.submit(slot, Operation::Sync, 0, 0, None);
    }
}
