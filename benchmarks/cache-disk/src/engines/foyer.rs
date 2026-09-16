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

use super::{Backend, Data, Done, Put, Record, Value};
use crate::{Config, Hasher, SEGMENT};
use anyhow::{Context, Result, ensure};
use foyer::{DeviceBuilder, HybridCache, HybridCacheBuilder, HybridCachePolicy};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};

type Cache = HybridCache<Vec<u8>, Vec<u8>, Hasher>;

enum Request {
    Write { ticket: u64, key: Vec<u8>, value: Vec<u8> },
    Read { ticket: u64, key: moat_cache::Bytes },
}

async fn drive(
    cache: Cache,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<Vec<Request>>,
    replies: mpsc::Sender<Vec<Done>>,
) {
    let mut reads = FuturesUnordered::<BoxFuture<'static, Done>>::new();
    let mut closed = false;
    while !closed || !reads.is_empty() {
        let mut ready = Vec::new();
        tokio::select! {
            batch = requests.recv(), if !closed => {
                if let Some(batch) = batch {
                    for request in batch {
                        match request {
                            Request::Write { ticket, key, value } => {
                                cache.insert(key, value);
                                ready.push(Done::Write(ticket, Ok(())));
                            }
                            Request::Read { ticket, key } => {
                                let future = cache.get(key.as_ref());
                                reads.push(Box::pin(async move {
                                    Done::Read(ticket, async {
                                        Ok(Data::Foyer(future.await?.context("unexpected foyer disk miss")?))
                                    }.await)
                                }));
                            }
                        }
                    }
                } else { closed = true; }
            }
            Some(done) = reads.next(), if !reads.is_empty() => ready.push(done),
        }
        while let Some(Some(done)) = reads.next().now_or_never() {
            ready.push(done);
        }
        if !ready.is_empty() && replies.send(ready).is_err() {
            break;
        }
    }
}

pub(crate) struct Runtime(tokio::runtime::Runtime);
impl Runtime {
    pub fn new(cpus: Vec<usize>) -> Result<Arc<Self>> {
        let next = AtomicUsize::new(0);
        Ok(Arc::new(Self(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(cpus.len())
                .thread_name("foyer-runtime")
                .enable_all()
                .on_thread_start(move || {
                    let core = cpus[next.fetch_add(1, Ordering::Relaxed) % cpus.len()];
                    moat_server::worker::pin_to_core(core).expect("foyer runtime CPU affinity");
                })
                .build()?,
        )))
    }
}

pub(crate) struct Foyer {
    cache: Cache,
    runtime: Arc<Runtime>,
    requests: tokio::sync::mpsc::UnboundedSender<Vec<Request>>,
    replies: mpsc::Receiver<Vec<Done>>,
    batch: Vec<Request>,
    pending: usize,
    driver: tokio::task::JoinHandle<()>,
    next: u64,
}
impl Foyer {
    pub fn new(c: &Config, disk: usize, runtime: Arc<Runtime>) -> Result<Self> {
        let cache = runtime.0.block_on(async {
            let device = foyer::FileDeviceBuilder::new(&c.disks[disk].path)
                .with_capacity(c.bytes_per_disk as usize)
                .build()?;
            // The pinned builder couples O_DIRECT to O_NOATIME (which requires
            // device-node ownership). Set only O_DIRECT on the shared fd.
            let probe = device.create_partition(0)?;
            let (fd, _) = probe.translate(0);
            // SAFETY: probe keeps the fd alive throughout both calls.
            let flags = unsafe { libc::fcntl(fd.0, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd.0, libc::F_SETFL, flags | libc::O_DIRECT) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok::<_, anyhow::Error>(
                HybridCacheBuilder::new()
                    .with_name(format!("foyer-disk-{disk}"))
                    .with_policy(HybridCachePolicy::WriteOnInsertion)
                    .with_flush_on_close(false)
                    .memory((1 << 20) / c.disks.len())
                    .with_shards(16)
                    .with_hash_builder(Hasher::default())
                    .with_eviction_config(foyer::FifoConfig::default())
                    .with_weighter(|key: &Vec<u8>, value: &Vec<u8>| key.len() + value.len())
                    .with_filter(|_, _| false)
                    .storage()
                    .with_io_engine_config(Box::new(
                        foyer::UringIoEngineConfig::new()
                            .with_threads(1)
                            .with_cpus(vec![c.io_cpus[disk] as u32])
                            .with_io_depth(256),
                    ) as Box<dyn foyer::IoEngineConfig>)
                    .with_engine_config(
                        foyer::BlockEngineConfig::new(device)
                            .with_block_size(SEGMENT as usize)
                            .with_flushers(2)
                            .with_reclaimers(1)
                            .with_buffer_pool_size(c.pool_bytes_per_disk)
                            .with_submit_queue_size_threshold(c.pool_bytes_per_disk)
                            .with_indexer_shards(64)
                            .with_tombstone_log(false),
                    )
                    .with_recover_mode(foyer::RecoverMode::None)
                    .with_compression(foyer::Compression::None)
                    .build()
                    .await?,
            )
        })?;
        let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
        let (sender, replies) = mpsc::channel();
        // One driver per disk initiates foyer's tasks on runtime workers.
        let driver = runtime.0.spawn(drive(cache.clone(), receiver, sender));
        Ok(Self {
            cache,
            runtime,
            requests,
            replies,
            batch: Vec::new(),
            pending: 0,
            driver,
            next: 1,
        })
    }
}
impl Backend for Foyer {
    fn put(&mut self, batch: &mut VecDeque<Put>) -> Result<Option<(u64, usize)>> {
        let Value::Parts { key, value } = &mut batch[0].value else {
            unreachable!("foyer accepts an owned key and value")
        };
        let ticket = self.next;
        self.next += 1;
        self.pending += 1;
        self.batch.push(Request::Write {
            ticket,
            key: key.to_vec(),
            value: std::mem::take(value),
        });
        Ok(Some((ticket, 1)))
    }
    fn read(&mut self, record: &Record) -> Result<Option<u64>> {
        let ticket = self.next;
        self.next += 1;
        self.pending += 1;
        self.batch.push(Request::Read {
            ticket,
            key: record.key.clone(),
        });
        Ok(Some(ticket))
    }
    fn poll(&mut self, out: &mut Vec<Done>) -> Result<()> {
        if !self.batch.is_empty() {
            self.requests
                .send(std::mem::take(&mut self.batch))
                .map_err(|_| anyhow::anyhow!("foyer driver stopped"))?;
        }
        if self.pending > 0 {
            // Yield the application CPU while Tokio and the I/O worker run.
            // Drain all ready replies per wake, not one wake per record.
            let mut batch = self.replies.recv().context("foyer driver stopped")?;
            self.pending -= batch.len();
            out.append(&mut batch);
            while let Ok(mut batch) = self.replies.try_recv() {
                self.pending -= batch.len();
                out.append(&mut batch);
            }
        }
        Ok(())
    }
    fn drain(&mut self) -> Result<()> {
        self.runtime.0.block_on(self.cache.storage().wait());
        ensure!(self.cache.memory().usage() == 0, "foyer memory residency changed");
        Ok(())
    }
    fn close(self) -> Result<()> {
        ensure!(
            self.pending == 0 && self.batch.is_empty(),
            "undelivered foyer operations"
        );
        ensure!(self.cache.memory().usage() == 0, "foyer memory residency changed");
        drop(self.requests);
        self.runtime.0.block_on(self.driver)?;
        Ok(self.runtime.0.block_on(self.cache.close())?)
    }
}
