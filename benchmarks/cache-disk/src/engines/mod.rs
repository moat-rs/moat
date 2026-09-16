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

//! Matched benchmark-only request routing for the two engine implementations.
//! This is an append/read adapter, not a replacement for moat-cache policies.

mod v1;
mod v2;

use super::Config;
use anyhow::{Context, Result, ensure};
use moat_cache::{
    Bytes,
    identity::{DEFAULT_IDENTITY_VERSION, Fingerprint, Xxh3},
};
use moat_common::ChunkId;
use moat_server::{Placement, Target};
use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc,
    thread,
};
use tokio::sync::oneshot;

type Reply<T> = oneshot::Sender<Result<T>>;

pub(super) enum Data {
    V1(moat_engine::ChunkData),
    V2 {
        buffers: moat_engine_v2::pipeline::ReadBuffers,
        range: moat_engine_v2::pipeline::ReadRange,
    },
}
impl Data {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::V1(data) => data,
            Self::V2 { buffers, range } => buffers.view(range.clone()),
        }
    }
}
struct Put {
    id: ChunkId,
    value: Vec<u8>,
    reply: Reply<()>,
}
enum Command {
    Put(Put),
    Read { id: ChunkId, reply: Reply<Data> },
    Close(Reply<()>),
}
enum Done {
    Write(u64, Result<()>),
    Read(u64, Result<Data>),
}
trait Backend {
    // None means no operation was accepted; poll before retrying.
    fn put(&mut self, batch: &VecDeque<Put>) -> Result<Option<(u64, usize)>>;
    fn read(&mut self, id: ChunkId) -> Result<Option<u64>>;
    fn poll(&mut self, wait: bool, out: &mut Vec<Done>) -> Result<()>;
    fn close(self) -> Result<()>;
}

pub(super) struct Engines {
    placement: Placement,
    senders: Vec<mpsc::Sender<Command>>,
    workers: Vec<thread::JoinHandle<Result<()>>>,
}
impl Engines {
    pub fn open(c: &Config, targets: Vec<Target>) -> Result<Self> {
        let mut store = Self {
            placement: Placement::new(targets),
            senders: Vec::new(),
            workers: Vec::new(),
        };
        for disk in 0..c.disks.len() {
            let (sender, receiver) = mpsc::channel();
            let (ready, started) = mpsc::sync_channel(1);
            let config = c.clone();
            let worker = thread::Builder::new()
                .name(format!("engine-bench-{disk}"))
                .spawn(move || {
                    moat_server::worker::pin_to_core(config.io_cpus[disk])?;
                    // Pools and rings are created and driven on their home thread.
                    if config.engine == "v1" {
                        start(v1::V1::new(&config, disk), receiver, ready)
                    } else {
                        start(v2::V2::new(&config, disk), receiver, ready)
                    }
                })?;
            store.senders.push(sender);
            store.workers.push(worker);
            started.recv().context("engine worker startup")??;
        }
        Ok(store)
    }
    fn route(&self, key: &[u8]) -> (ChunkId, &mpsc::Sender<Command>) {
        let id = Xxh3.identify(&[0; 16], DEFAULT_IDENTITY_VERSION, key);
        (id, &self.senders[self.placement.disk_of(&id).unwrap()])
    }
    pub async fn put(&self, key: Bytes, value: Vec<u8>, preassembled: bool) -> Result<()> {
        let (id, sender) = self.route(&key);
        // Both engines store exactly the same full-key envelope. Field lengths
        // are fixed by this workload; no production cache policy is benchmarked.
        let bytes = if preassembled {
            value
        } else {
            let mut bytes = Vec::with_capacity(key.len() + value.len());
            bytes.extend_from_slice(&key);
            bytes.extend_from_slice(&value);
            bytes
        };
        let (reply, wait) = oneshot::channel();
        sender
            .send(Command::Put(Put {
                id,
                value: bytes,
                reply,
            }))
            .map_err(|_| anyhow::anyhow!("engine worker closed"))?;
        wait.await.context("engine write completion")?
    }
    pub async fn get(&self, key: &Bytes) -> Result<Data> {
        let (id, sender) = self.route(key);
        let (reply, wait) = oneshot::channel();
        sender
            .send(Command::Read { id, reply })
            .map_err(|_| anyhow::anyhow!("engine worker closed"))?;
        let data = wait.await.context("engine read completion")??;
        ensure!(data.bytes().get(..key.len()) == Some(key.as_ref()), "full key mismatch");
        Ok(data)
    }
    pub async fn close(&self) -> Result<()> {
        let mut waits = Vec::new();
        for sender in &self.senders {
            let (reply, wait) = oneshot::channel();
            sender
                .send(Command::Close(reply))
                .map_err(|_| anyhow::anyhow!("engine worker closed"))?;
            waits.push(wait);
        }
        for wait in waits {
            wait.await.context("engine shutdown")??;
        }
        Ok(())
    }
}
impl Drop for Engines {
    fn drop(&mut self) {
        self.senders.clear();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
fn start<B: Backend>(
    backend: Result<B>,
    receiver: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    match backend {
        Ok(backend) => {
            let _ = ready.send(Ok(()));
            drive(backend, receiver)
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            Ok(())
        }
    }
}
fn drive(mut backend: impl Backend, receiver: mpsc::Receiver<Command>) -> Result<()> {
    let mut puts = VecDeque::new();
    let mut reads = VecDeque::new();
    let mut writing: HashMap<u64, Vec<Reply<()>>> = HashMap::new();
    let mut reading: HashMap<u64, Reply<Data>> = HashMap::new();
    let mut out = Vec::with_capacity(256);
    let mut close = None;
    let mut disconnected = false;
    loop {
        let idle = puts.is_empty() && reads.is_empty() && writing.is_empty() && reading.is_empty();
        let first = if idle && close.is_none() && !disconnected {
            match receiver.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => {
                    disconnected = true;
                    None
                }
            }
        } else {
            None
        };
        for cmd in first.into_iter().chain(receiver.try_iter().take(64)) {
            match cmd {
                Command::Put(put) => puts.push_back(put),
                Command::Read { id, reply } => reads.push_back((id, reply)),
                Command::Close(reply) => close = Some(reply),
            }
        }
        while !puts.is_empty() {
            let Some((ticket, count)) = backend.put(&puts)? else {
                break;
            };
            let replies = puts.drain(..count).map(|p| p.reply).collect();
            assert!(writing.insert(ticket, replies).is_none());
        }
        while let Some((id, _)) = reads.front() {
            let Some(ticket) = backend.read(*id)? else { break };
            let (_, reply) = reads.pop_front().unwrap();
            assert!(reading.insert(ticket, reply).is_none());
        }
        backend.poll(false, &mut out)?;
        for completion in out.drain(..) {
            match completion {
                Done::Write(ticket, result) => {
                    result?;
                    for reply in writing.remove(&ticket).context("unknown write ticket")? {
                        let _ = reply.send(Ok(()));
                    }
                }
                Done::Read(ticket, result) => {
                    let reply = reading.remove(&ticket).context("unknown read ticket")?;
                    let _ = reply.send(result);
                }
            }
        }
        if puts.is_empty()
            && reads.is_empty()
            && writing.is_empty()
            && reading.is_empty()
            && (close.is_some() || disconnected)
        {
            let result = backend.close();
            if let Some(reply) = close {
                let _ = reply.send(result);
                return Ok(());
            }
            return result;
        }
    }
}
