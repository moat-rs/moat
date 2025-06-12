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
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures::future::join_all;
use itertools::Itertools;
use rand::{rng, seq::IndexedRandom};
use tokio::sync::RwLock;

use crate::{
    api::client::ApiClient,
    meta::model::{Identity, MemberList, Membership, Peer},
    runtime::Runtime,
};

#[derive(Debug)]
pub struct MetaManagerConfig {
    pub identity: Identity,
    pub peer: Peer,
    pub bootstrap_peers: Vec<Peer>,
    pub provider_eviction_timeout: Duration,
    pub health_check_timeout: Duration,
    pub health_check_interval: Duration,
    pub health_check_peers: usize,
    pub sync_timeout: Duration,
    pub sync_interval: Duration,
    pub sync_peers: usize,
    pub weight: usize,
}

#[derive(Debug)]
struct Mutable {
    providers: BTreeMap<Peer, Membership>,
}

#[derive(Debug)]
struct Inner {
    mutable: RwLock<Mutable>,
    identity: Identity,
    peer: Peer,
    provider_eviction_timeout: Duration,
    health_check_timeout: Duration,
    health_check_interval: Duration,
    health_check_peers: usize,
    sync_timeout: Duration,
    sync_interval: Duration,
    sync_peers: usize,
    weight: usize,
}

#[derive(Debug, Clone)]
pub struct MetaManager {
    inner: Arc<Inner>,
}

impl MetaManager {
    pub fn new(config: MetaManagerConfig) -> Self {
        let mut providers: BTreeMap<Peer, Membership> = config
            .bootstrap_peers
            .into_iter()
            .map(|peer| {
                (
                    peer,
                    Membership {
                        last_seen: SystemTime::now(),
                        weight: 0,
                    },
                )
            })
            .collect();
        if config.identity == Identity::Provider {
            providers.insert(
                config.peer.clone(),
                Membership {
                    last_seen: SystemTime::now(),
                    weight: config.weight,
                },
            );
        }
        let mutable = RwLock::new(Mutable { providers });
        let inner = Arc::new(Inner {
            mutable,
            identity: config.identity,
            peer: config.peer,
            provider_eviction_timeout: config.provider_eviction_timeout,
            health_check_timeout: config.health_check_timeout,
            health_check_interval: config.health_check_interval,
            health_check_peers: config.health_check_peers,
            sync_timeout: config.sync_timeout,
            sync_interval: config.sync_interval,
            sync_peers: config.sync_peers,
            weight: config.weight,
        });
        Self { inner }
    }

    pub async fn providers(&self) -> BTreeMap<Peer, Membership> {
        let mutable = self.inner.mutable.read().await;
        mutable
            .providers
            .iter()
            .map(|(peer, membership)| (peer.clone(), membership.clone()))
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
        let mut merged: BTreeMap<Peer, Membership> = BTreeMap::new();
        for (peer, membership) in current.iter().chain(other.iter()) {
            let now = SystemTime::now();
            // Remove stale providers.
            if now.duration_since(membership.last_seen).unwrap_or(Duration::ZERO) > self.inner.provider_eviction_timeout
            {
                continue;
            }
            // Update provider last seen time.
            match merged.entry(peer.clone()) {
                Entry::Occupied(mut o) => {
                    if o.get().last_seen < membership.last_seen {
                        o.insert(membership.clone());
                    }
                }
                Entry::Vacant(v) => {
                    v.insert(membership.clone());
                }
            }
        }
        if self.inner.identity == Identity::Provider {
            merged.insert(
                self.inner.peer.clone(),
                Membership {
                    last_seen: SystemTime::now(),
                    weight: self.inner.weight,
                },
            );
        }
        // Give evicted providers another chance to respond.
        let futures = mutable
            .providers
            .iter()
            .filter(|(peer, _)| !merged.contains_key(peer))
            .map(|(peer, membership)| async {
                ApiClient::new(peer.clone())
                    .with_timeout(self.inner.health_check_timeout)
                    .health()
                    .await
                    .then_some((
                        peer.clone(),
                        Membership {
                            last_seen: SystemTime::now(),
                            weight: membership.weight,
                        },
                    ))
            });
        join_all(futures)
            .await
            .into_iter()
            .flatten()
            .for_each(|(peer, membership)| {
                merged.insert(peer, membership);
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
            .filter(|(peer, _)| *peer != self.inner.peer)
            .collect_vec();
        providers.sort_by(|(p1, m1), (p2, m2)| m1.last_seen.cmp(&m2.last_seen).then_with(|| p1.cmp(p2)));
        let futures =
            providers
                .into_iter()
                .take(self.inner.health_check_peers)
                .map(|(peer, mut membership)| async move {
                    ApiClient::new(peer.clone())
                        .with_timeout(self.inner.health_check_timeout)
                        .health()
                        .await
                        .then_some((peer, {
                            membership.last_seen = SystemTime::now();
                            membership
                        }))
                });
        let res = join_all(futures).await;
        let mut mutable = self.inner.mutable.write().await;
        // TODO(MrCroxx): handle unhealthy peers with SWIM algorithm
        res.into_iter().flatten().for_each(|(peer, membership)| {
            mutable.providers.insert(peer, membership);
        });
        mutable.providers.insert(
            self.inner.peer.clone(),
            Membership {
                last_seen: SystemTime::now(),
                weight: self.inner.weight,
            },
        );
    }

    pub async fn sync(&self) {
        let providers = self
            .providers()
            .await
            .into_iter()
            .filter_map(|(peer, membership)| {
                if SystemTime::now()
                    .duration_since(membership.last_seen)
                    .unwrap_or(Duration::ZERO)
                    < self.inner.provider_eviction_timeout
                {
                    Some(peer)
                } else {
                    None
                }
            })
            .filter(|peer| *peer != self.inner.peer)
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
                ApiClient::new(peer.clone())
                    .with_timeout(self.inner.sync_timeout)
                    .sync(members)
                    .await
            }
        });
        let res = join_all(futures).await;
        for members in res.into_iter().flatten() {
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
