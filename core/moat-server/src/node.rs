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

//! Device handles and deterministic owner assignment. Recovery runs on owner threads.

use std::sync::Arc;

use moat_common::ChunkId;

use crate::{
    placement::{Placement, Target},
    storage::{Device, Disk, Options},
    worker::{DiskId, Handler, Worker, WorkerError, WorkerOptions},
};

/// Errors from assembling a node.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// A disk failed to open.
    #[error("disk {disk}: {source}")]
    Open {
        /// Index of the disk in the list passed to `open`.
        disk: DiskId,
        /// The cause.
        #[source]
        source: crate::storage::Error,
    },
    /// A worker failed to start.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// Two disks have the same persistent placement identity.
    #[error("disks {first} and {second} have the same UUID")]
    DuplicateIdentity {
        /// First disk using the identity.
        first: DiskId,
        /// Second disk using the identity.
        second: DiskId,
    },
    /// The node has no disks.
    #[error("no disks")]
    NoDisks,
    /// An assigned owner does not exist in the worker list.
    #[error("disk owner is outside the worker list")]
    InvalidOwners,
}

/// The opened disks of a machine.
pub struct Node {
    engines: Vec<Disk>,
    owners: Vec<usize>,
    placement: Placement,
}

impl Node {
    /// Validates device geometry. Use [`Self::assign_owners`] before starting
    /// workers, which recover indexes on their owner threads.
    pub fn open(devices: Vec<Arc<dyn Device>>, options: Options) -> Result<Self, NodeError> {
        if devices.is_empty() {
            return Err(NodeError::NoDisks);
        }
        let mut engines = Vec::new();
        for (disk, device) in devices.into_iter().enumerate() {
            engines.push(Disk::open(device, options.clone()).map_err(|source| NodeError::Open { disk, source })?);
        }
        let mut identities = std::collections::HashMap::new();
        for (disk, engine) in engines.iter().enumerate() {
            if let Some(first) = identities.insert(engine.layout().device_id(), disk) {
                return Err(NodeError::DuplicateIdentity { first, second: disk });
            }
        }
        let placement = Placement::new(
            engines
                .iter()
                .map(|e| Target {
                    uuid: e.layout().device_id(),
                    weight: e.layout().capacity(),
                })
                .collect(),
        );
        let owners = vec![0; engines.len()];
        Ok(Self {
            engines,
            owners,
            placement,
        })
    }

    /// Device handles, indexed by [`DiskId`]; no shared engine state.
    pub fn engines(&self) -> &[Disk] {
        &self.engines
    }

    /// The worker assigned exclusive ownership of `disk`.
    pub fn owner_of(&self, disk: DiskId) -> usize {
        self.owners[disk]
    }

    /// The owner of every disk, indexed by [`DiskId`].
    pub fn owners(&self) -> &[usize] {
        &self.owners
    }

    /// The disk `id` is placed on.
    pub fn disk_of(&self, id: &ChunkId) -> DiskId {
        self.placement.disk_of(id).expect("a node has at least one disk")
    }

    /// The placement over this node's disks.
    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// Assigns each disk to one of `workers` workers, spreading disks evenly
    /// and preferring a worker on the disk's NUMA node when both `disk_numa`
    /// (per disk) and `worker_numa` (per worker) are known.
    pub fn assign_owners(&mut self, workers: usize, disk_numa: &[Option<usize>], worker_numa: &[Option<usize>]) {
        assert!(workers > 0, "at least one worker");
        let mut load = vec![0usize; workers];
        for disk in 0..self.engines.len() {
            let node = disk_numa.get(disk).copied().flatten();
            let local = |w: usize| node.is_some() && worker_numa.get(w).copied().flatten() == node;
            // Least loaded local worker, else least loaded worker.
            let pick = (0..workers)
                .filter(|&w| local(w))
                .min_by_key(|&w| (load[w], w))
                .or_else(|| (0..workers).min_by_key(|&w| (load[w], w)))
                .expect("workers > 0");
            self.owners[disk] = pick;
            load[pick] += 1;
        }
    }

    /// Sets the owner of every disk explicitly.
    pub fn set_owners(&mut self, owners: Vec<usize>) {
        assert_eq!(owners.len(), self.engines.len());
        self.owners = owners;
    }

    /// Starts and recovers the assigned disks on each owner worker. Handlers
    /// see only their own disks; callers must route cross-owner requests.
    pub fn start<H: Handler>(
        &self,
        workers: &[WorkerOptions],
        mut make: impl FnMut(usize) -> H,
    ) -> Result<Vec<Worker<H>>, NodeError> {
        if self.owners.iter().any(|&owner| owner >= workers.len()) {
            return Err(NodeError::InvalidOwners);
        }
        let mut started = Vec::with_capacity(workers.len());
        for (w, opts) in workers.iter().enumerate() {
            let disks = self
                .engines
                .iter()
                .enumerate()
                .filter(|(d, _)| self.owners[*d] == w)
                .map(|(d, e)| (d, e.clone()))
                .collect();
            started.push(Worker::spawn(w, opts.clone(), disks, make(w))?);
        }
        Ok(started)
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("disks", &self.engines.len())
            .field("owners", &self.owners)
            .finish()
    }
}
