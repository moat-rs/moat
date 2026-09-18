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

//! Owner-only lifecycle I/O and metadata reservations.

use super::{Error, Pipeline, PollBudget, ResourceLimits, Result, Ticket, index::Location};
use crate::{
    frame::FrameLimits,
    io::{self, Buffer, Operation, Queue, Request},
};
use moat_common::ChunkId;

pub(crate) const CONTROL_TOKEN: u64 = u64::MAX;

impl<Q: Queue> Pipeline<Q> {
    pub(crate) fn set_sync_enabled(&mut self, enabled: bool) {
        self.sync_enabled = enabled;
    }
    /// Configures bounded resources before restoring or accepting records.
    pub fn configure(&mut self, resources: ResourceLimits, budget: PollBudget) -> Result<()> {
        if self.serving
            || self.in_flight() != 0
            || !self.index.is_empty()
            || self.control_active
            || self.control.is_some()
        {
            return Err(Error::InvalidArgument("configure before recovery or admission"));
        }
        if resources.frame_bytes < 4096
            || resources.index_entries == 0
            || resources.metadata_bytes < 4096
            || resources.pending_records == 0
            || budget.operations == 0
            || budget.bytes == 0
            || budget.records == 0
        {
            return Err(Error::InvalidArgument(
                "resource limits and poll budgets must be positive",
            ));
        }
        self.resources = resources;
        self.budget = budget;
        Ok(())
    }

    /// A Unix descriptor notifying completion readiness, when the queue provides one.
    /// Drive `poll(false)` before sleeping and after each readiness notification.
    #[cfg(unix)]
    pub fn notification_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.queue.notification_fd()
    }

    pub(crate) fn route_capacity(metadata_bytes: usize) -> usize {
        metadata_bytes / (std::mem::size_of::<super::Extent>() + std::mem::size_of::<bool>())
    }
    pub(crate) fn set_limits(&mut self, limits: FrameLimits) {
        self.limits = limits;
    }
    pub(crate) fn maintenance(&mut self, enabled: bool) {
        self.maintenance = enabled;
    }
    pub(crate) fn writes_pending(&self) -> bool {
        !self.writes.is_empty() || self.flush.is_some()
    }
    pub(crate) fn queue_failed(&self) -> bool {
        self.queue_failed
    }
    pub(crate) fn next_ticket(&mut self) -> Result<Ticket> {
        if self.next_ticket == u64::MAX {
            return Err(Error::InvalidArgument("ticket counter exhausted"));
        }
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        Ok(ticket)
    }
    pub(crate) fn control_io(
        &mut self,
        operation: Operation,
        offset: u64,
        len: usize,
        buffer: Option<Buffer>,
    ) -> Result<()> {
        if self.queue_failed {
            return Err(Error::QueueFailed);
        }
        if self.control.is_some() || self.control_active || self.control_done.is_some() {
            return Err(Error::Backpressure);
        }
        self.control = Some(Request {
            token: CONTROL_TOKEN,
            operation,
            offset,
            len,
            buffer,
        });
        Ok(())
    }
    pub(crate) fn take_control(&mut self) -> Option<io::Completion> {
        self.control_done.take()
    }
    pub(super) fn start_control(&mut self) {
        if self.queue_failed || self.queue.vacant() == 0 {
            return;
        }
        if let Some(request) = self.control.take() {
            match self.queue.try_submit(request) {
                Ok(()) => self.control_active = true,
                Err(request) => self.control = Some(request),
            }
        }
    }
    pub(crate) fn key_count(&self) -> usize {
        self.keys.len()
    }
    pub(crate) fn visit_batch(&self, start: usize, end: usize, visit: &mut impl FnMut(ChunkId, u64, Option<u32>)) {
        for key in &self.keys[start..end] {
            let location = self.index[key];
            visit(
                *key,
                location.lsn,
                (location.kind == crate::frame::RecordKind::Data).then_some(location.value_len),
            );
        }
    }
    pub(super) fn reserve_entries(&mut self, entries: &[(ChunkId, Location)]) -> Result<usize> {
        // Avoid probing the index while a conservative reservation fits. Near
        // the bound, count distinct absent keys so overwrites still fit.
        let mut new = entries.len();
        // The normal unique-key path needs no additional temporary allocation.
        // Near the limit, duplicate keys in one frame must not reject recovery.
        if self.index.len().saturating_add(self.pending_new).saturating_add(new) > self.resources.index_entries {
            let mut unique = std::collections::HashSet::new();
            unique.try_reserve(entries.len().min(self.resources.index_entries))?;
            for (key, _) in entries {
                if !self.index.contains_key(key) {
                    if unique.len() == self.resources.index_entries && !unique.contains(key) {
                        return Err(Error::ResourceLimit("index entries"));
                    }
                    unique.insert(*key);
                }
            }
            new = unique.len();
        }
        let additional = self
            .pending_new
            .checked_add(new)
            .ok_or(Error::ResourceLimit("index entries"))?;
        if self.index.len().saturating_add(new) > self.resources.index_entries {
            return Err(Error::ResourceLimit("index entries including tombstones"));
        }
        if self.index.len().saturating_add(additional) > self.resources.index_entries {
            return Err(Error::Backpressure);
        }
        self.index.try_reserve(additional)?;
        self.keys.try_reserve(additional)?;
        Ok(new)
    }
}
