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

//! Shared views remain readable after cache shutdown and across threads.

use std::sync::Arc;

use futures_executor::block_on;
use moat_cache::{Bytes, Cache};
use moat_cache_memory::Cache as Memory;
use moat_cache_store::Store;
use moat_common::{HugePages, PoolOptions};
use moat_server::storage::{self, Disk, FormatOptions, FrameLimits, MemDevice, QueueBackend, QueueOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    block_on(async {
        let device = Arc::new(MemDevice::new(17 << 20));
        storage::format(
            &*device,
            &FormatOptions {
                sync_mode: Default::default(),
                segment_size: (1 << 20) as u32,
                limits: FrameLimits::new(((128 << 10) + 8192u32).next_power_of_two(), 128 << 10).unwrap(),
                device_id: [1; 16],
            },
        )?;
        let engine = Disk::open(
            device,
            storage::Options {
                sync_mode: Default::default(),
                index_capacity: 1024,
                verify_reads: false,
            },
        )?;
        let (store, _) = Store::new(
            vec![engine],
            moat_cache_store::Options {
                max_bytes: 2 << 20,
                backend: QueueBackend::Sync,
                queue: QueueOptions {
                    depth: 8,

                    pool: PoolOptions {
                        bytes: 16 << 20,
                        max_class: 1 << 20,
                        huge_pages: HugePages::Disabled,
                    },
                },
                ..Default::default()
            },
        )?;
        let memory = Memory::<Bytes, Bytes, Bytes>::builder(1 << 20)
            .shards(1)
            .weigher(|key, value, properties| key.len() + value.len() + properties.len());
        let cache = Cache::new(memory, store, Default::default()).await?;
        let key = Bytes::from(b"example".to_vec());
        drop(cache.insert(key.clone(), Bytes::from(b"shared value".to_vec())).await?);
        cache.clear_memory();
        let view = cache.get(&key).await?.expect("disk hit");
        assert!(view.key_view().shares_backing(&view.value_view()));
        let held_value = view.value_view();
        drop(view);
        cache.close().await?;
        std::thread::spawn(move || {
            assert_eq!(held_value.as_ref(), b"shared value");
            println!("Retained {} bytes after cache shutdown", held_value.len());
        })
        .join()
        .expect("reader thread");
        assert_eq!(cache.statistics().store.read_bytes, 0);
        Ok(())
    })
}
