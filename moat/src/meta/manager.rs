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
    hash::Hash,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures::future::join_all;
use itertools::Itertools;
use rand::{rng, seq::IndexedRandom};
use tokio::sync::RwLock;

use crate::{
    api::client::ApiClient,
    meta::{
        hash::ConsistentHash,
        model::{MemberList, Membership, Peer, Role},
    },
    runtime::Runtime,
};

#[derive(Debug)]
pub struct MetaManagerConfig {
    pub role: Role,
    pub peer: Peer,
    pub bootstrap_peers: Vec<Peer>,
    pub peer_eviction_timeout: Duration,
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
    members: MemberList,
    locator: ConsistentHash<Peer>,
}

impl Mutable {
    fn update_locator(&mut self) {
        let weights = self
            .members
            .caches
            .iter()
            .map(|(peer, membership)| (peer.clone(), membership.weight));
        self.locator = weights.into();
    }
}

#[derive(Debug)]
struct Inner {
    mutable: RwLock<Mutable>,
    role: Role,
    peer: Peer,
    peer_eviction_timeout: Duration,
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
        let mut caches: BTreeMap<Peer, Membership> = config
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
        if config.role == Role::Cache {
            caches.insert(
                config.peer.clone(),
                Membership {
                    last_seen: SystemTime::now(),
                    weight: config.weight,
                },
            );
        }
        let members = MemberList { caches };
        let locator = ConsistentHash::default();
        let mut mutable = Mutable { members, locator };
        mutable.update_locator();
        let mutable = RwLock::new(mutable);
        let inner = Arc::new(Inner {
            mutable,
            role: config.role,
            peer: config.peer,
            peer_eviction_timeout: config.peer_eviction_timeout,
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

    pub async fn members(&self) -> MemberList {
        let mutable = self.inner.mutable.read().await;
        mutable.members.clone()
    }

    pub async fn merge(&self, members: MemberList) -> MemberList {
        let mut mutable = self.inner.mutable.write().await;
        let current = &mutable.members.caches;
        let other = &members.caches;
        tracing::trace!(?current, ?other, "Merging members");
        let mut merged: BTreeMap<Peer, Membership> = BTreeMap::new();
        for (peer, membership) in current.iter().chain(other.iter()) {
            let now = SystemTime::now();
            // Remove stale caches.
            if now.duration_since(membership.last_seen).unwrap_or(Duration::ZERO) > self.inner.peer_eviction_timeout {
                continue;
            }
            // Update cache last seen time.
            match merged.entry(peer.clone()) {
                Entry::Occupied(mut o) => {
                    if o.get().last_seen < membership.last_seen {
                        tracing::trace!(?peer, ?membership, "Updating membership");
                        o.insert(membership.clone());
                    } else {
                        tracing::trace!(?peer, ?membership, "Skip updating stale membership.");
                    }
                }
                Entry::Vacant(v) => {
                    v.insert(membership.clone());
                }
            }
        }
        if self.inner.role == Role::Cache {
            merged.insert(
                self.inner.peer.clone(),
                Membership {
                    last_seen: SystemTime::now(),
                    weight: self.inner.weight,
                },
            );
        }
        // Give evicted caches another chance to respond.
        let futures = mutable
            .members
            .caches
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
        mutable.members.caches = merged;
        mutable.update_locator();
        MemberList {
            caches: mutable.members.caches.clone(),
        }
    }

    pub async fn replace(&self, members: MemberList) {
        tracing::trace!(?members, "Replacing members");
        let mut mutable = self.inner.mutable.write().await;
        mutable.members = members;
        mutable.update_locator();
    }

    pub async fn health_check(&self) {
        let mut caches = self
            .members()
            .await
            .caches
            .into_iter()
            .filter(|(peer, _)| *peer != self.inner.peer)
            .collect_vec();
        caches.sort_by(|(p1, m1), (p2, m2)| m1.last_seen.cmp(&m2.last_seen).then_with(|| p1.cmp(p2)));
        let futures = caches
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
            mutable.members.caches.insert(peer, membership);
        });
        if self.inner.role == Role::Cache {
            mutable.members.caches.insert(
                self.inner.peer.clone(),
                Membership {
                    last_seen: SystemTime::now(),
                    weight: self.inner.weight,
                },
            );
        }
    }

    pub async fn sync(&self) {
        let caches = self
            .members()
            .await
            .caches
            .into_iter()
            .filter_map(|(peer, membership)| {
                if SystemTime::now()
                    .duration_since(membership.last_seen)
                    .unwrap_or(Duration::ZERO)
                    < self.inner.peer_eviction_timeout
                {
                    Some(peer)
                } else {
                    None
                }
            })
            .filter(|peer| *peer != self.inner.peer)
            .collect_vec();

        let peers = { caches.choose_multiple(&mut rng(), self.inner.sync_peers).collect_vec() };
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

    pub async fn observe(&self) {
        let caches = self.members().await.caches.into_iter().collect_vec();
        if caches.is_empty() {
            tracing::warn!("No caches available.");
            return;
        }
        let peer = {
            let (peer, _) = caches.choose(&mut rng()).unwrap();
            peer.clone()
        };
        if let Some(members) = ApiClient::new(peer.clone())
            .with_timeout(self.inner.sync_timeout)
            .members()
            .await
        {
            tracing::trace!(?peer, ?members, "Observed members from peer");
            self.replace(members).await;
        } else {
            tracing::warn!("Failed to fetch members from peer: {}", peer);
        }
    }

    pub fn health_check_timeout(&self) -> Duration {
        self.inner.health_check_timeout
    }

    // TODO(MrCroxx): Use `ArcSwap` to optimize the locator access.
    pub async fn locate<H>(&self, item: H) -> Option<Peer>
    where
        H: Hash,
    {
        let mutable = self.inner.mutable.read().await;
        mutable.locator.locate(item).cloned()
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

#[derive(Debug)]
pub struct Observer {
    runtime: Runtime,
    meta_manager: MetaManager,
}

impl Observer {
    pub fn new(runtime: Runtime, meta_manager: MetaManager) -> Self {
        Self { runtime, meta_manager }
    }

    pub async fn run(self) {
        // let meta = self.meta_manager.clone();
        // self.runtime.spawn(async move {
        //     loop {
        //         tokio::time::sleep(meta.inner.health_check_interval).await;
        //         meta.health_check().await;
        //     }
        // });

        let meta = self.meta_manager.clone();
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(meta.inner.health_check_interval).await;
                meta.observe().await;
            }
        });
    }
}
