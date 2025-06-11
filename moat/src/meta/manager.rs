// Copyright 2025 Moat Project Authors
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
    collections::btree_map::{BTreeMap, Entry},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures::future::join_all;

use itertools::Itertools;
use rand::{rng, seq::IndexedRandom};
use tokio::sync::RwLock;

use crate::{
    api::client::ApiClient,
    meta::model::{Identity, MemberList},
    runtime::Runtime,
};

#[derive(Debug)]
pub struct MetaManagerConfig {
    pub identity: Identity,
    pub listen: SocketAddr,
    pub bootstrap_peers: Vec<SocketAddr>,
    pub provider_eviction_timeout: Duration,
    pub health_check_timeout: Duration,
    pub health_check_interval: Duration,
    pub health_check_peers: usize,
    pub sync_timeout: Duration,
    pub sync_interval: Duration,
    pub sync_peers: usize,
}

#[derive(Debug)]
struct Mutable {
    providers: BTreeMap<SocketAddr, SystemTime>,
}

#[derive(Debug)]
struct Inner {
    mutable: RwLock<Mutable>,
    identity: Identity,
    listen: SocketAddr,
    provider_eviction_timeout: Duration,
    health_check_timeout: Duration,
    health_check_interval: Duration,
    health_check_peers: usize,
    sync_timeout: Duration,
    sync_interval: Duration,
    sync_peers: usize,
}

#[derive(Debug, Clone)]
pub struct MetaManager {
    inner: Arc<Inner>,
}

impl MetaManager {
    pub fn new(config: MetaManagerConfig) -> Self {
        let mut providers: BTreeMap<SocketAddr, SystemTime> = config
            .bootstrap_peers
            .into_iter()
            .map(|peer| (peer, SystemTime::now()))
            .collect();
        if config.identity == Identity::Provider {
            providers.insert(config.listen, SystemTime::now());
        }
        let mutable = RwLock::new(Mutable { providers });
        let inner = Arc::new(Inner {
            mutable,
            identity: config.identity,
            listen: config.listen,
            provider_eviction_timeout: config.provider_eviction_timeout,
            health_check_timeout: config.health_check_timeout,
            health_check_interval: config.health_check_interval,
            health_check_peers: config.health_check_peers,
            sync_timeout: config.sync_timeout,
            sync_interval: config.sync_interval,
            sync_peers: config.sync_peers,
        });
        Self { inner }
    }

    pub async fn providers(&self) -> BTreeMap<SocketAddr, SystemTime> {
        let mutable = self.inner.mutable.read().await;
        mutable
            .providers
            .iter()
            .map(|(peer, last_seen)| (*peer, *last_seen))
            .collect()
    }

    pub async fn members(&self) -> MemberList {
        let providers = self.providers().await;
        MemberList { providers }
    }

    pub async fn merge(&self, members: MemberList) -> MemberList {
        let mut mutable = self.inner.mutable.write().await;
        let current = &mutable.providers;
        let other = &members.providers;
        let mut merged: BTreeMap<SocketAddr, SystemTime> = BTreeMap::new();
        for (peer, last_seen) in current.iter().chain(other.iter()) {
            let now = SystemTime::now();
            // Remove stale providers.
            if now.duration_since(*last_seen).unwrap_or(Duration::ZERO) > self.inner.provider_eviction_timeout {
                continue;
            }
            // Update provider last seen time.
            match merged.entry(*peer) {
                Entry::Occupied(mut o) => {
                    let last_seen = std::cmp::max(*o.get(), *last_seen);
                    o.insert(last_seen);
                }
                Entry::Vacant(v) => {
                    v.insert(*last_seen);
                }
            }
        }
        if self.inner.identity == Identity::Provider {
            merged.insert(self.inner.listen, SystemTime::now());
        }
        // Give evicted providers another chance to respond.
        let futures = mutable
            .providers
            .keys()
            .filter(|peer| !merged.contains_key(peer))
            .map(|peer| async {
                ApiClient::new(*peer)
                    .with_timeout(self.inner.health_check_timeout)
                    .health()
                    .await
                    .then_some((*peer, SystemTime::now()))
            });
        join_all(futures)
            .await
            .into_iter()
            .filter_map(|v| v)
            .for_each(|(peer, last_seen)| {
                merged.insert(peer, last_seen);
            });
        mutable.providers = merged;
        MemberList {
            providers: mutable.providers.clone(),
        }
    }

    pub async fn health_check(&self) {
        let mut providers = self
            .providers()
            .await
            .into_iter()
            .filter(|(peer, _)| *peer != self.inner.listen)
            .collect_vec();
        providers.sort_by(|(p1, l1), (p2, l2)| l1.cmp(l2).then_with(|| p1.cmp(p2)));
        let futures = providers
            .into_iter()
            .take(self.inner.health_check_peers)
            .map(|(peer, _)| async move {
                ApiClient::new(peer)
                    .with_timeout(self.inner.health_check_timeout)
                    .health()
                    .await
                    .then_some((peer, SystemTime::now()))
            });
        let res = join_all(futures).await;
        let mut mutable = self.inner.mutable.write().await;
        res.into_iter().filter_map(|v| v).for_each(|(peer, last_seen)| {
            mutable.providers.insert(peer, last_seen);
        });
        mutable.providers.insert(self.inner.listen, SystemTime::now());
    }

    pub async fn sync(&self) {
        let providers = self
            .providers()
            .await
            .into_iter()
            .filter_map(|(peer, last_seen)| {
                if SystemTime::now().duration_since(last_seen).unwrap_or(Duration::ZERO)
                    < self.inner.provider_eviction_timeout
                {
                    Some(peer)
                } else {
                    None
                }
            })
            .filter(|peer| *peer != self.inner.listen)
            .collect_vec();

        let peers = {
            providers
                .choose_multiple(&mut rng(), self.inner.sync_peers)
                .collect_vec()
        };
        let members = self.members().await;
        let futures = peers.into_iter().map(|peer| {
            let members = members.clone();
            async move {
                ApiClient::new(*peer)
                    .with_timeout(self.inner.sync_timeout)
                    .sync(members)
                    .await
            }
        });
        let res = join_all(futures).await;
        for members in res.into_iter().filter_map(|v| v) {
            self.merge(members).await;
        }
    }

    pub fn health_check_timeout(&self) -> Duration {
        self.inner.health_check_timeout
    }
}

#[derive(Debug)]
pub struct Gossip {
    runtime: Runtime,
    meta_manager: MetaManager,
}

impl Gossip {
    pub fn new(runtime: Runtime, meta_manager: MetaManager) -> Self {
        Self { runtime, meta_manager }
    }

    pub async fn run(self) {
        let meta = self.meta_manager.clone();
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(meta.inner.health_check_interval).await;
                meta.health_check().await;
            }
        });

        let meta = self.meta_manager.clone();
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(meta.inner.sync_interval).await;
                meta.sync().await;
            }
        });
    }
}
