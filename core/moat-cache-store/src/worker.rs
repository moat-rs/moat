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
    collections::{HashMap, VecDeque},
    sync::{Arc, atomic::Ordering, mpsc},
    thread,
};

use moat_common::ChunkId;
use moat_server::storage::{self, Completion, Disk, Session};

use crate::{
    DeleteResult, Error, InventoryEntry, Options, Result,
    command::{Command, Fence, FenceReply, Operation, Reply, Waiter},
    delivery::Delivery,
    store::Counters,
};

#[derive(Default)]
struct Chain {
    active: Option<Operation>,
    pending: VecDeque<Operation>,
}

pub(crate) fn spawn(
    disk: usize,
    engine: Disk,
    options: Options,
    receiver: mpsc::Receiver<Command>,
    counters: Arc<Counters>,
) -> Result<(Vec<InventoryEntry>, thread::JoinHandle<()>)> {
    let (ready, wait) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name(format!("moat-store-{disk}"))
        .spawn(move || {
            let worker = Worker::new(disk, engine, options, receiver, counters);
            match worker {
                Ok(mut worker) => {
                    let _ = ready.send(Ok(worker.inventory()));
                    let result = worker.run();
                    worker.finish(result);
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            }
        })?;
    match wait.recv().unwrap_or(Err(Error::Closed)) {
        Ok(inventory) => Ok((inventory, handle)),
        Err(error) => {
            let _ = handle.join();
            Err(error)
        }
    }
}

struct Worker {
    delivery: Delivery,
    disk: usize,
    session: Session,
    options: Options,
    receiver: mpsc::Receiver<Command>,
    counters: Arc<Counters>,
    chains: HashMap<ChunkId, Chain>,
    ready: VecDeque<ChunkId>,
    reads: HashMap<u64, ChunkId>,
    writes: HashMap<u64, (ChunkId, u64)>,
    completions: Vec<Completion>,
    fence: Option<Fence>,
    closing: bool,
    close_reply: Option<Reply<()>>,
    write_error: Option<Error>,
}
impl Worker {
    fn new(
        disk: usize,
        engine: Disk,
        options: Options,
        receiver: mpsc::Receiver<Command>,
        counters: Arc<Counters>,
    ) -> Result<Self> {
        if let Some(&cpu) = options.worker_cpus.get(disk) {
            moat_server::worker::pin_to_core(cpu)?;
        }
        let session = Session::open(engine, &options.queue, options.backend)?;
        Ok(Self {
            delivery: Delivery::new(options.completion_executor.as_ref()),
            disk,
            session,
            options,
            receiver,
            counters,
            chains: HashMap::new(),
            ready: VecDeque::new(),
            reads: HashMap::new(),
            writes: HashMap::new(),
            completions: Vec::new(),
            fence: None,
            closing: false,
            close_reply: None,
            write_error: None,
        })
    }
    fn inventory(&self) -> Vec<InventoryEntry> {
        let mut entries = Vec::new();
        self.session.visit(|id, lsn, len| {
            entries.push(InventoryEntry {
                disk: self.disk,
                id,
                lsn,
                len,
            })
        });
        entries
    }

    fn accept(&mut self, command: Command) {
        let (id, operation) = match command {
            Command::Read {
                id,
                range,
                reply,
                permit,
            } => (
                id,
                Operation::Read {
                    range,
                    lsn: 0,
                    waiters: vec![Waiter {
                        reply,
                        permit: Some(permit),
                    }],
                },
            ),
            Command::Put {
                id,
                value,
                reply,
                permit,
            } => (id, Operation::Put { value, reply, permit }),
            Command::Delete {
                id,
                expected_lsn,
                reply,
                permit,
            } => (
                id,
                Operation::Delete {
                    expected_lsn,
                    reply,
                    permit,
                },
            ),
            Command::Fence { reply, permit } => {
                self.fence = Some(Fence {
                    reply,
                    _permit: permit,
                    ticket: None,
                });
                return;
            }
        };
        let chain = self.chains.entry(id).or_default();
        if let Operation::Read { range, waiters, .. } = &operation {
            let last = if chain.pending.is_empty() {
                chain.active.as_mut()
            } else {
                chain.pending.back_mut()
            };
            if let Some(Operation::Read {
                range: previous,
                waiters: previous_waiters,
                ..
            }) = last
                && previous == range
            {
                debug_assert_eq!(waiters.len(), 1);
                let Operation::Read { waiters, .. } = operation else {
                    unreachable!()
                };
                for mut waiter in waiters {
                    waiter.permit.as_mut().expect("new read credit").resize(0);
                    previous_waiters.push(waiter);
                }
                self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        if chain.active.is_none() && chain.pending.is_empty() {
            self.ready.push_back(id);
        }
        chain.pending.push_back(operation);
    }

    fn disconnected(&mut self) {
        self.fence = Some(Fence {
            reply: FenceReply::Close(None),
            _permit: None,
            ticket: None,
        });
    }

    fn run(&mut self) -> Result<()> {
        loop {
            let mut progress = false;
            if self.fence.is_none() && !self.closing {
                // Bound one packing window so a busy producer cannot starve I/O.
                for _ in 0..128 {
                    match self.receiver.try_recv() {
                        Ok(command) => {
                            self.accept(command);
                            progress = true;
                            if self.fence.is_some() {
                                break;
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            self.disconnected();
                            break;
                        }
                    }
                }
            }
            for _ in 0..self.ready.len() {
                let id = self.ready.pop_front().expect("ready chain");
                let mut chain = self.chains.remove(&id).expect("queued chain");
                let operation = chain.pending.pop_front().expect("queued operation");
                match self.start(id, operation)? {
                    Started::Active(operation) => {
                        chain.active = Some(operation);
                        progress = true;
                    }
                    Started::Retry(operation) => {
                        chain.pending.push_front(operation);
                    }
                    Started::Done => {
                        progress = true;
                    }
                }
                if chain.active.is_none() && !chain.pending.is_empty() {
                    self.ready.push_back(id);
                }
                if chain.active.is_some() || !chain.pending.is_empty() {
                    self.chains.insert(id, chain);
                }
            }
            progress |= self.session.poll(false, &mut self.completions)? != 0;
            let mut completions = std::mem::take(&mut self.completions);
            for completion in completions.drain(..) {
                match completion {
                    Completion::Failed { ticket, error } => {
                        let error = Error::from(storage::Error::Engine(moat_engine::engine::Error::Aborted(error)));
                        self.write_error.get_or_insert(error.clone());
                        if let Some(id) = self.reads.remove(&ticket.number()) {
                            self.complete(id).fail(error);
                        } else if let Some((id, _)) = self.writes.remove(&ticket.number()) {
                            self.complete(id).fail(error);
                        } else {
                            assert_eq!(self.fence.as_ref().and_then(|f| f.ticket), Some(ticket.number()));
                            self.finish_fence(Err(error))?;
                        }
                    }
                    Completion::Read {
                        ticket,
                        result,
                        buffers,
                    } => {
                        let id = self.reads.remove(&ticket.number()).expect("submitted read");
                        let operation = self.complete(id);
                        self.delivery.push(move || {
                            operation.read_done(
                                result
                                    .map(|range| storage::read_buffer(buffers, range))
                                    .map_err(Error::from),
                            )
                        });
                    }
                    Completion::Write { ticket, result, .. } => {
                        let (id, lsn) = self.writes.remove(&ticket.number()).expect("submitted mutation");
                        let result = result.map(|()| lsn).map_err(Error::from);
                        if let Err(error) = &result {
                            self.write_error.get_or_insert(error.clone());
                        }
                        self.complete(id).write_done(result);
                    }
                    Completion::Flush { ticket, result } => {
                        assert_eq!(self.fence.as_ref().and_then(|f| f.ticket), Some(ticket.number()));
                        self.finish_fence(result.map_err(Error::from))?;
                    }
                }
            }
            self.completions = completions;
            if self.fence.is_some() && self.chains.is_empty() {
                progress |= self.advance_fence()?;
            }
            self.delivery.flush();
            if self.closing && self.session.in_flight() == 0 {
                return self.write_error.take().map_or(Ok(()), Err);
            }
            if !progress {
                if self.options.idle_wait.is_zero() {
                    std::hint::spin_loop();
                } else if self.fence.is_none() && !self.closing {
                    match self.receiver.recv_timeout(self.options.idle_wait) {
                        Ok(command) => self.accept(command),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => self.disconnected(),
                    }
                } else {
                    thread::sleep(self.options.idle_wait);
                }
            }
        }
    }

    fn start(&mut self, id: ChunkId, mut operation: Operation) -> Result<Started> {
        let result = match &mut operation {
            Operation::Read { range, lsn, waiters } => {
                // Cancellation must release queued requests even while another
                // caller retains every available buffer credit.
                waiters.retain(|waiter| !waiter.reply.is_canceled());
                if waiters.is_empty() {
                    return Ok(Started::Done);
                }
                let Some(stat) = self.session.stat(&id) else {
                    operation.miss();
                    return Ok(Started::Done);
                };
                *lsn = stat.0;
                if !waiters[0]
                    .permit
                    .as_mut()
                    .expect("leader byte credit")
                    .grow(self.options.queue.pool.max_class)
                {
                    return Ok(Started::Retry(operation));
                }
                match self.session.read(id, range.clone()) {
                    Ok(ticket) => {
                        self.reads.insert(ticket.number(), id);
                        self.counters.reads.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            Operation::Put { value, .. } => match self.session.write(id, Some(value)) {
                Ok((ticket, lsn)) => {
                    self.writes.insert(ticket.number(), (id, lsn));
                    Ok(())
                }
                Err(error) => Err(error),
            },
            Operation::Delete { expected_lsn, .. } => {
                let Some(stat) = self.session.stat(&id) else {
                    operation.deleted(DeleteResult::Missing);
                    return Ok(Started::Done);
                };
                if expected_lsn.is_some_and(|expected| expected != stat.0) {
                    operation.deleted(DeleteResult::Changed);
                    return Ok(Started::Done);
                }
                match self.session.write(id, None) {
                    Ok((ticket, lsn)) => {
                        self.writes.insert(ticket.number(), (id, lsn));
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok(()) => Ok(Started::Active(operation)),
            Err(storage::Error::Busy) => Ok(Started::Retry(operation)),
            Err(error) => {
                let error = Error::from(error);
                if !matches!(operation, Operation::Read { .. }) {
                    self.write_error.get_or_insert(error.clone());
                }
                operation.fail(error);
                Ok(Started::Done)
            }
        }
    }

    fn complete(&mut self, id: ChunkId) -> Operation {
        let chain = self.chains.get_mut(&id).expect("in-flight chain");
        let operation = chain.active.take().expect("active operation");
        if chain.pending.is_empty() {
            self.chains.remove(&id);
        } else {
            self.ready.push_back(id);
        }
        operation
    }

    fn advance_fence(&mut self) -> Result<bool> {
        let fence = self.fence.as_ref().expect("fence");
        if fence.ticket.is_some() {
            return Ok(false);
        }
        let ticket = match fence.reply {
            FenceReply::Inventory(_) => {
                let inventory = self.inventory();
                let fence = self.fence.take().expect("fence");
                let FenceReply::Inventory(reply) = fence.reply else {
                    unreachable!()
                };
                self.delivery.push(move || {
                    drop(fence._permit);
                    let _ = reply.send(Ok(inventory));
                });
                return Ok(true);
            }
            FenceReply::Flush(_) => self.session.flush(),
            FenceReply::Close(_) => {
                let result = self.session.seal().map_err(Error::from);
                self.finish_fence(result)?;
                return Ok(true);
            }
        };
        match ticket {
            Ok(ticket) => {
                self.fence.as_mut().expect("fence").ticket = Some(ticket.number());
                Ok(true)
            }
            Err(storage::Error::Busy) => Ok(false),
            Err(error) => {
                self.finish_fence(Err(error.into()))?;
                Ok(true)
            }
        }
    }

    fn finish_fence(&mut self, result: Result<()>) -> Result<()> {
        let Fence {
            reply, _permit: permit, ..
        } = self.fence.take().expect("completed fence");
        match reply {
            FenceReply::Flush(reply) => {
                let result = result.and(self.write_error.take().map_or(Ok(()), Err));
                self.delivery.push(move || {
                    drop(permit);
                    let _ = reply.send(result);
                });
            }
            FenceReply::Close(reply) => {
                drop(permit);
                self.close_reply = reply;
                self.closing = true;
                result?;
            }
            FenceReply::Inventory(_) => unreachable!("inventory has no engine ticket"),
        }
        Ok(())
    }

    fn finish(self, result: Result<()>) {
        let Self {
            mut delivery,
            session,
            receiver,
            chains,
            fence,
            close_reply,
            ..
        } = self;
        drop(session);
        let error = result.clone().err().unwrap_or(Error::Closed);
        for (_, chain) in chains {
            if let Some(operation) = chain.active {
                let error = error.clone();
                delivery.push(move || operation.fail(error));
            }
            for operation in chain.pending {
                let error = error.clone();
                delivery.push(move || operation.fail(error));
            }
        }
        if let Some(fence) = fence {
            let error = error.clone();
            delivery.push(move || {
                drop(fence._permit);
                fence.reply.fail(error);
            });
        }
        for command in receiver.try_iter() {
            let error = error.clone();
            delivery.push(move || command.fail(error));
        }
        if let Some(reply) = close_reply {
            delivery.push(move || {
                let _ = reply.send(result);
            });
        }
    }
}
enum Started {
    Active(Operation),
    Retry(Operation),
    Done,
}

#[cfg(test)]
mod tests {
    use futures_executor::block_on;
    use moat_common::{HugePages, PoolOptions};
    use moat_server::storage::{FormatOptions, FrameLimits, MemDevice, QueueBackend, QueueOptions};

    use super::*;
    use crate::{Request, budget::Budget};

    #[test]
    fn independent_ids_overlap_and_inflight_followers_share_the_read() {
        let device = Arc::new(MemDevice::new(8 << 20));
        storage::format(
            &*device,
            &FormatOptions {
                sync_mode: Default::default(),
                device_id: [41; 16],
                segment_size: 1 << 20,
                limits: FrameLimits::new(128 << 10, 64 << 10).unwrap(),
            },
        )
        .unwrap();
        let disk = Disk::open(device, storage::Options::default()).unwrap();
        let options = Options {
            backend: QueueBackend::Sync,
            queue: QueueOptions {
                depth: 8,
                pool: PoolOptions {
                    bytes: 16 << 20,
                    max_class: 1 << 20,
                    huge_pages: HugePages::Disabled,
                },
            },
            ..Options::default()
        };
        let budget = Budget::new(32, 32 << 20, 8 << 20, 1, 1 << 20);
        let counters = Arc::new(Counters::default());
        let (sender, receiver) = mpsc::channel();
        let mut worker = Worker::new(0, disk, options, receiver, counters.clone()).unwrap();
        let mut out = Vec::new();
        for n in [1, 2] {
            loop {
                match worker.session.write(ChunkId::from_u128(n), Some(&[n as u8])) {
                    Ok(_) => break,
                    Err(storage::Error::Busy) => {
                        worker.session.poll(false, &mut out).unwrap();
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        }
        while worker.session.in_flight() != 0 {
            worker.session.poll(true, &mut out).unwrap();
        }
        for completion in out {
            let Completion::Write { result, .. } = completion else {
                unreachable!()
            };
            result.unwrap();
        }
        let read = |worker: &mut Worker, n| {
            let (reply, receiver) = futures_channel::oneshot::channel();
            worker.accept(Command::Read {
                id: ChunkId::from_u128(n),
                range: None,
                reply,
                permit: budget.reserve(0, Some(0)).unwrap(),
            });
            Request { receiver }
        };
        let first = read(&mut worker, 1);
        let other = read(&mut worker, 2);
        // Admit two independent reads before polling either completion. The
        // synchronous backend retains completions until the owner drives poll.
        while let Some(id) = worker.ready.pop_front() {
            let mut chain = worker.chains.remove(&id).unwrap();
            let Started::Active(operation) = worker.start(id, chain.pending.pop_front().unwrap()).unwrap() else {
                panic!("read admission");
            };
            chain.active = Some(operation);
            worker.chains.insert(id, chain);
        }
        assert_eq!(worker.session.in_flight(), 2);
        let follower = read(&mut worker, 1);
        assert_eq!(counters.reads.load(Ordering::Relaxed), 2);
        assert_eq!(counters.coalesced.load(Ordering::Relaxed), 1);
        sender
            .send(Command::Fence {
                reply: FenceReply::Close(None),
                permit: None,
            })
            .unwrap();
        let result = worker.run();
        worker.finish(result.clone());
        result.unwrap();
        let first = block_on(first).unwrap().unwrap();
        let other = block_on(other).unwrap().unwrap();
        let follower = block_on(follower).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &follower));
        assert_eq!(&**first, &[1]);
        assert_eq!(&**other, &[2]);
        drop((first, other, follower));
        assert_eq!(budget.snapshot().bytes, 0);
        assert_eq!(budget.snapshot().requests, 0);
    }
}
