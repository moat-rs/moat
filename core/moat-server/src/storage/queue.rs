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

use std::{collections::VecDeque, io, sync::Arc};

use moat_common::BufferPool;
use moat_engine_v2::{
    engine,
    io::{Completion, Operation, Queue, Request},
};

use super::Device;

/// Queue selection for an owner's device.
#[derive(Debug, Clone, Copy, Default)]
pub enum QueueBackend {
    /// io_uring on Linux; synchronous elsewhere.
    #[default]
    Auto,
    /// Synchronous positional I/O, including memory/fault-injection devices.
    Sync,
    /// Registered io_uring on Linux; unsupported elsewhere.
    Uring,
}

pub(super) struct DeviceRef<'a>(pub &'a dyn Device);
impl engine::Device for DeviceRef<'_> {
    fn capacity(&self) -> io::Result<u64> {
        Ok(self.0.capacity())
    }
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.read_at(bytes, offset)
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_at(bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.0.sync()
    }
}
pub(super) struct OwnedDevice(pub Arc<dyn Device>);
impl engine::Device for OwnedDevice {
    fn capacity(&self) -> io::Result<u64> {
        Ok(self.0.capacity())
    }
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.read_at(bytes, offset)
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_at(bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.0.sync()
    }
}
pub(super) struct SyncQueue {
    device: Arc<dyn Device>,
    depth: usize,
    completed: VecDeque<Completion>,
}
impl Queue for SyncQueue {
    fn depth(&self) -> usize {
        self.depth
    }
    fn vacant(&self) -> usize {
        self.depth - self.completed.len()
    }
    fn try_submit(&mut self, mut request: Request) -> Result<(), Request> {
        if self.vacant() == 0 {
            return Err(request);
        }
        let result = match request.operation {
            Operation::Read => self
                .device
                .read_at(
                    &mut request.buffer.as_mut().expect("read buffer")[..request.len],
                    request.offset,
                )
                .map(|()| request.len),
            Operation::Write => self
                .device
                .write_at(
                    &request.buffer.as_ref().expect("write buffer")[..request.len],
                    request.offset,
                )
                .map(|()| request.len),
            Operation::Sync => self.device.sync().map(|()| 0),
        };
        self.completed.push_back(Completion { request, result });
        Ok(())
    }
    fn poll(&mut self, _wait: bool) -> io::Result<()> {
        Ok(())
    }
    fn pop(&mut self) -> Option<Completion> {
        self.completed.pop_front()
    }
}
pub(super) enum DeviceQueue {
    Sync(SyncQueue),
    #[cfg(target_os = "linux")]
    Uring(Box<moat_engine_v2::io::UringQueue>),
}
impl DeviceQueue {
    pub fn new(
        device: Arc<dyn Device>,
        depth: usize,
        backend: QueueBackend,
        _pool: Arc<BufferPool>,
    ) -> io::Result<Self> {
        if !(1..=32768).contains(&depth) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "queue depth must be in 1..=32768",
            ));
        }
        let sync = matches!(backend, QueueBackend::Sync)
            || (matches!(backend, QueueBackend::Auto) && !cfg!(target_os = "linux"));
        if sync {
            return Ok(Self::Sync(SyncQueue {
                device,
                depth,
                completed: VecDeque::with_capacity(depth),
            }));
        }
        #[cfg(target_os = "linux")]
        {
            let fd = device.fd().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "io_uring requires a file descriptor; select Sync for memory devices",
                )
            })?;
            let file = std::fs::File::from(fd.try_clone_to_owned()?);
            Ok(Self::Uring(Box::new(moat_engine_v2::io::UringQueue::with_pool(
                file, depth, _pool,
            )?)))
        }
        #[cfg(not(target_os = "linux"))]
        Err(io::Error::new(io::ErrorKind::Unsupported, "io_uring requires Linux"))
    }
    fn queue(&self) -> &dyn Queue {
        match self {
            Self::Sync(q) => q,
            #[cfg(target_os = "linux")]
            Self::Uring(q) => &**q,
        }
    }
    fn queue_mut(&mut self) -> &mut dyn Queue {
        match self {
            Self::Sync(q) => q,
            #[cfg(target_os = "linux")]
            Self::Uring(q) => &mut **q,
        }
    }
}
impl Queue for DeviceQueue {
    fn depth(&self) -> usize {
        self.queue().depth()
    }
    fn vacant(&self) -> usize {
        self.queue().vacant()
    }
    fn try_submit(&mut self, request: Request) -> Result<(), Request> {
        self.queue_mut().try_submit(request)
    }
    fn poll(&mut self, wait: bool) -> io::Result<()> {
        self.queue_mut().poll(wait)
    }
    fn pop(&mut self) -> Option<Completion> {
        self.queue_mut().pop()
    }
}
