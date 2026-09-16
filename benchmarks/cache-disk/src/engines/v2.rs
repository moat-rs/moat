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

use super::{Backend, Data, Done, Put};
use crate::Config;
use anyhow::Result;
use moat_common::{BufferPool, ChunkId, HugePages, PoolOptions};
use moat_engine_v2::{
    engine::{self, Device, Engine, Error},
    frame::{FrameBuilder, FrameLimits, PreparedFrame},
    io::UringQueue,
    pipeline::{self, Completion, ReadBuffers},
};
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    sync::Arc,
};

struct Window {
    file: File,
    bytes: u64,
}
impl Device for Window {
    fn capacity(&self) -> io::Result<u64> {
        Ok(self.bytes)
    }
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        if offset > self.bytes || bytes.len() as u64 > self.bytes - offset {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Device::read_at(&self.file, bytes, offset)
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        if offset > self.bytes || bytes.len() as u64 > self.bytes - offset {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Device::write_at(&self.file, bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}
pub(super) struct V2 {
    engine: Engine<Window, UringQueue>,
    pool: Arc<BufferPool>,
    limits: FrameLimits,
    len: u32,
    verify: bool,
    lsn: u64,
    out: Vec<Completion>,
}
impl V2 {
    pub fn new(c: &Config, disk: usize) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&c.disks[disk].path)?;
        let device = Window {
            file: file.try_clone()?,
            bytes: c.bytes_per_disk,
        };
        let limits = FrameLimits::new(8 << 20, (4 << 20) + 4096)?;
        engine::format(
            &device,
            engine::FormatOptions {
                device_id: crate::fresh_identity()?,
                segment_size: c.engine_segment_bytes as u32,
                limits,
            },
        )?;
        let pool = BufferPool::new(PoolOptions {
            bytes: c.pool_bytes_per_disk,
            max_class: 8 << 20,
            huge_pages: if c.moat_huge_pages {
                HugePages::Preferred
            } else {
                HugePages::Disabled
            },
        })?;
        let queue = UringQueue::with_pool(file, 256, pool.clone())?;
        let mut engine = Engine::open(device, queue)?;
        engine.reserve_index(c.records_per_disk * 2)?;
        Ok(Self {
            engine,
            pool,
            limits,
            len: (c.key_bytes + c.value_bytes) as u32,
            verify: c.moat_verify_reads,
            lsn: 1,
            out: Vec::with_capacity(256),
        })
    }
}
impl Backend for V2 {
    fn put(&mut self, batch: &VecDeque<Put>) -> Result<Option<(u64, usize)>> {
        let first = &batch[0];
        let (result, count) = if first.value.len() >= 65536 {
            let Some(mut buffer) = self.pool.alloc(PreparedFrame::required_len(self.limits, self.len)?) else {
                return Ok(None);
            };
            PreparedFrame::new(self.limits, self.len, &mut buffer)?
                .value_mut()
                .copy_from_slice(&first.value);
            (self.engine.write_prepared(first.id, self.lsn, self.len, buffer), 1)
        } else {
            let mut frame = FrameBuilder::new(self.limits);
            let mut count = 0;
            for p in batch.iter().take(64) {
                if frame.push(p.id, self.lsn + count as u64, &p.value).is_err() {
                    break;
                }
                count += 1;
            }
            let Some(buffer) = self.pool.alloc(frame.encoded_len()) else {
                return Ok(None);
            };
            (self.engine.write(&frame, buffer), count)
        };
        match result {
            Ok(ticket) => {
                self.lsn += count as u64;
                Ok(Some((ticket.number(), count)))
            }
            Err(r) if matches!(r.error, Error::Pipeline(pipeline::Error::Backpressure)) => Ok(None),
            Err(r) => Err(r.error.into()),
        }
    }
    fn read(&mut self, id: ChunkId) -> Result<Option<u64>> {
        let r = self.engine.read_requirements(id, 0..self.len, self.verify)?;
        let Some(value) = self.pool.alloc(r.value_len.max(4096)) else {
            return Ok(None);
        };
        let metadata = if self.verify {
            let Some(buffer) = self.pool.alloc(r.metadata_len) else {
                return Ok(None);
            };
            Some(buffer.into())
        } else {
            None
        };
        match self.engine.read(
            id,
            0..self.len,
            self.verify,
            ReadBuffers {
                value: value.into(),
                metadata,
            },
        ) {
            Ok(ticket) => Ok(Some(ticket.number())),
            Err(r) if matches!(r.error, Error::Pipeline(pipeline::Error::Backpressure)) => Ok(None),
            Err(r) => Err(r.error.into()),
        }
    }
    fn poll(&mut self, wait: bool, out: &mut Vec<Done>) -> Result<()> {
        self.engine.poll(wait, &mut self.out)?;
        for c in self.out.drain(..) {
            out.push(match c {
                Completion::Write { ticket, result, .. } => Done::Write(ticket.number(), result.map_err(Into::into)),
                Completion::Read {
                    ticket,
                    result,
                    buffers,
                } => Done::Read(
                    ticket.number(),
                    result.map(|range| Data::V2 { buffers, range }).map_err(Into::into),
                ),
                Completion::Flush { .. } => unreachable!("benchmark uses the common device sync boundary"),
            });
        }
        Ok(())
    }
    fn close(mut self) -> Result<()> {
        Ok(self.engine.seal()?)
    }
}
