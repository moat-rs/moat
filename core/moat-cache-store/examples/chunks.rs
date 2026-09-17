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

//! Runtime-independent async chunk operations and explicit shutdown.

use std::sync::Arc;

use futures_executor::block_on;
use moat_cache_store::{DeleteResult, Options, Store};
use moat_common::{ChunkId, HugePages, PoolOptions};
use moat_server::storage::{self, Disk, FormatOptions, FrameLimits, MemDevice, QueueBackend, QueueOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    block_on(async {
        let device = Arc::new(MemDevice::new(17 << 20));
        storage::format(
            &*device,
            &FormatOptions {
                segment_size: (1 << 20) as u32,
                limits: FrameLimits::new(((128 << 10) + 8192u32).next_power_of_two(), 128 << 10).unwrap(),
                device_id: [1; 16],
            },
        )?;
        let engine = Disk::open(
            device,
            storage::Options {
                index_capacity: 128,
                verify_reads: false,
            },
        )?;
        let (store, inventory) = Store::new(
            vec![engine],
            Options {
                backend: QueueBackend::Sync,
                queue: QueueOptions {
                    depth: 16,

                    pool: PoolOptions {
                        bytes: 16 << 20,
                        max_class: 1 << 20,
                        huge_pages: HugePages::Disabled,
                    },
                },
                ..Default::default()
            },
        )?;
        assert!(inventory.is_empty());
        let id = ChunkId::from_u128(42);
        let lsn = store.put(id, Arc::from(&b"hello"[..])).await?;
        let value = store.get(id, None).await?.expect("completed write");
        assert_eq!(&**value, b"hello");
        assert_eq!(value.lsn(), lsn);
        assert!(matches!(store.delete(id, Some(lsn)).await?, DeleteResult::Deleted(_)));
        assert!(store.get(id, None).await?.is_none());
        store.flush().await?;
        store.close().await?;
        assert_eq!(&**value, b"hello");
        Ok(())
    })
}
