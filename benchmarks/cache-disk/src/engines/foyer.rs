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
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll, Waker},
};

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
    cache: HybridCache<Vec<u8>, Vec<u8>, Hasher>,
    runtime: Arc<Runtime>,
    reads: FuturesUnordered<BoxFuture<'static, Done>>,
    writes: Vec<Done>,
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
        Ok(Self {
            cache,
            runtime,
            reads: FuturesUnordered::new(),
            writes: Vec::new(),
            next: 1,
        })
    }
}
impl Backend for Foyer {
    fn put(&mut self, batch: &mut VecDeque<Put>) -> Result<Option<(u64, usize)>> {
        let Value::Parts { key, value } = &mut batch[0].value else {
            unreachable!("foyer accepts an owned key and value")
        };
        let _guard = self.runtime.0.enter();
        self.cache.insert(key.to_vec(), std::mem::take(value));
        let ticket = self.next;
        self.next += 1;
        self.writes.push(Done::Write(ticket, Ok(())));
        Ok(Some((ticket, 1)))
    }
    fn read(&mut self, record: &Record) -> Result<Option<u64>> {
        let _guard = self.runtime.0.enter();
        let future = self.cache.get(record.key.as_ref());
        let ticket = self.next;
        self.next += 1;
        // Poll foyer's returned future directly; do not spawn another request
        // task or add a oneshot bridge around its internal Tokio scheduling.
        self.reads.push(Box::pin(async move {
            Done::Read(
                ticket,
                async { Ok(Data::Foyer(future.await?.context("unexpected foyer disk miss")?)) }.await,
            )
        }));
        Ok(Some(ticket))
    }
    fn poll(&mut self, out: &mut Vec<Done>) -> Result<()> {
        let _guard = self.runtime.0.enter();
        out.append(&mut self.writes);
        // The owner actively polls. FuturesUnordered retains readiness and
        // uses its own wake queue; no thread notification is needed here.
        let mut context = TaskContext::from_waker(Waker::noop());
        while let Poll::Ready(Some(done)) = self.reads.poll_next_unpin(&mut context) {
            out.push(done);
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
            self.reads.is_empty() && self.writes.is_empty(),
            "undelivered foyer operations"
        );
        ensure!(self.cache.memory().usage() == 0, "foyer memory residency changed");
        Ok(self.runtime.0.block_on(self.cache.close())?)
    }
}
