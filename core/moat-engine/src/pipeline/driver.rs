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
        let before = out.len();
        let mut work = Work::default();
        if self.queue_failed {
            self.retire_failed(out, &mut work);
            return Ok(out.len() - before);
        }
        self.retire_writes(out, &mut work);
        self.start_flush(out);
        self.start_control();
        while !work.exhausted(self.budget) {
            let Some(slot) = self.ready_reads.pop_front() else {
                break;
            };
            let Some(Pending::EmptyRead { ticket, buffers }) = self.slots[slot].take() else {
                unreachable!()
            };
            self.free.push(slot);
            out.push(Completion::Read {
                ticket,
                result: Ok(ReadRange::Value(0..0)),
                buffers,
            });
            work.operations += 1;
        }
        if let Err(error) = self.queue.poll(wait && out.len() == before && !self.has_ready()) {
            self.queue_failed = true;
            self.fatal = Some(std::sync::Arc::new(Error::Queue(error)));
            self.retire_failed(out, &mut work);
            return Ok(out.len() - before);
        }
        while !work.exhausted(self.budget) {
            let Some(completion) = self.queue.pop() else { break };
            work.operations += 1;
            // Write completion transfers ownership; it does not scan payload bytes.
            if completion.request.operation != Operation::Write {
                work.bytes = work.bytes.saturating_add(completion.request.len);
            }
            if completion.request.token == super::maintenance::CONTROL_TOKEN {
                self.control_active = false;
                self.control_done = Some(completion);
            } else if let Err(error) = self.complete(completion, out) {
                self.queue_failed = true;
                self.fatal = Some(std::sync::Arc::new(error));
                break;
            }
        }
        if self.queue_failed {
            self.retire_failed(out, &mut work);
        } else {
            self.retire_writes(out, &mut work);
            self.start_flush(out);
            self.start_control();
        }
        Ok(out.len() - before)
    }

    /// Whether local completions/publication remain runnable without waiting.
    /// Callers should poll again before sleeping on a notification descriptor.
    pub fn has_ready(&self) -> bool {
        if self.queue_failed {
            return self.in_flight() != 0;
        }
        self.queue.has_ready()
            || (self.control.is_some() && self.queue.vacant() != 0)
            || !self.ready_reads.is_empty()
            || self.control_done.is_some()
            || self
                .writes
                .front()
                .is_some_and(|&slot| matches!(&self.slots[slot], Some(Pending::Write(w)) if w.completed.is_some()))
    }

    fn retire_failed(&mut self, out: &mut Vec<Completion>, work: &mut Work) {
        while self.failed_cursor < self.slots.len() && !work.exhausted(self.budget) {
            let slot = self.failed_cursor;
            self.failed_cursor += 1;
            work.operations += 1;
            let Some(pending) = self.slots[slot].take() else {
                continue;
            };
            let ticket = match pending {
                Pending::Read(r) => r.ticket,
                Pending::VerifiedRead(r) => r.ticket,
                Pending::Write(w) => {
                    self.pending_entries -= w.entries.len();
                    self.pending_new -= w.reserved_new;
                    w.ticket
                }
                Pending::EmptyRead { ticket, .. } | Pending::Flush { ticket, .. } => ticket,
            };
            self.free.push(slot);
            out.push(Completion::Failed {
                ticket,
                error: self.fatal.as_ref().expect("fatal cause").clone(),
            });
        }
        self.ready_reads.clear();
        self.writes.clear();
        self.flush = None;
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
                let result = result.and_then(|()| {
                    let position = self.position_in(read.location.segment as usize, read.location.frame_offset)?;
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

    fn retire_writes(&mut self, out: &mut Vec<Completion>, work: &mut Work) {
        while !work.exhausted(self.budget) {
            let Some(&slot) = self.writes.front() else { break };
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
            self.pending_entries -= write.entries.len();
            self.pending_new -= write.reserved_new;
            work.operations += 1;
            work.records = work.records.saturating_add(write.entries.len());

            if result.is_ok() {
                index::apply(&mut self.index, &mut self.keys, write.entries);
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
        if !self.sync_enabled {
            self.slots[slot] = None;
            self.free.push(slot);
            self.flush = None;
            out.push(Completion::Flush { ticket, result: Ok(()) });
            return;
        }
        self.slots[slot] = Some(Pending::Flush {
            ticket,
            submitted: true,
        });
        self.submit(slot, Operation::Sync, 0, 0, None);
    }
}

#[derive(Default)]
struct Work {
    operations: usize,
    bytes: usize,
    records: usize,
}
impl Work {
    fn exhausted(&self, budget: super::PollBudget) -> bool {
        self.operations >= budget.operations || self.bytes >= budget.bytes || self.records >= budget.records
    }
}
