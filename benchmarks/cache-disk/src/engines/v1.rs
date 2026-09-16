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
use crate::{Config, Window};
use anyhow::{Result, ensure};
use moat_common::{ChunkId, HugePages, PoolOptions};
use moat_engine::{FileDevice, IoQueue, PutOutcome, ReadOutcome};
use std::{collections::VecDeque, sync::Arc};

pub(super) struct V1 {
    queue: Box<dyn IoQueue>,
    writer: moat_engine::Writer,
    reader: moat_engine::Reader,
    writes: Vec<moat_engine::Completion>,
    reads: Vec<moat_engine::ReadCompletion>,
    next: u64,
}
impl V1 {
    pub fn new(c: &Config, disk: usize) -> Result<Self> {
        let device = Arc::new(Window {
            inner: FileDevice::open(&c.disks[disk].path, true)?,
            bytes: c.bytes_per_disk,
        });
        moat_engine::format(
            &*device,
            &moat_engine::FormatOptions {
                segment_size: c.engine_segment_bytes,
                chunk_max: (4 << 20) + 4096,
                disk_uuid: crate::fresh_identity()?,
            },
        )?;
        let (engine, _) = moat_engine::open(
            device,
            moat_engine::Options {
                index_capacity: (c.records_per_disk * 4).max(1024).next_power_of_two(),
                verify_reads: c.moat_verify_reads,
                ..Default::default()
            },
        )?;
        let mut queue: Box<dyn IoQueue> = Box::new(moat_engine::uring::UringQueue::new(&moat_engine::QueueOptions {
            depth: 256,
            descriptors: 8,
            pool: PoolOptions {
                bytes: c.pool_bytes_per_disk,
                max_class: 8 << 20,
                huge_pages: if c.moat_huge_pages {
                    HugePages::Preferred
                } else {
                    HugePages::Disabled
                },
            },
        })?);
        let writer = engine.writer(&mut *queue)?;
        let reader = engine.reader(&mut *queue)?;
        Ok(Self {
            queue,
            writer,
            reader,
            writes: Vec::with_capacity(256),
            reads: Vec::with_capacity(256),
            next: 1,
        })
    }
}
impl Backend for V1 {
    fn put(&mut self, batch: &VecDeque<Put>) -> Result<Option<(u64, usize)>> {
        let record = &batch[0];
        let result = if record.value.len() >= 65536 {
            match self.writer.prepare_large(&mut *self.queue, record.value.len() as u32) {
                Ok(mut value) => {
                    record.value.copy_into(value.value_mut());
                    self.writer
                        .put_large(&mut *self.queue, record.id, value, None, Default::default())
                }
                Err(error) => Err(error),
            }
        } else {
            self.writer
                .put(&mut *self.queue, record.id, record.value.bytes(), Default::default())
        };
        match result {
            Ok(PutOutcome::Written { ticket, .. }) => Ok(Some((ticket, 1))),
            Err(moat_engine::Error::Busy) => Ok(None),
            other => anyhow::bail!("v1 put failed: {other:?}"),
        }
    }
    fn read(&mut self, id: ChunkId) -> Result<Option<u64>> {
        match self.reader.get(&mut *self.queue, &id, None, self.next) {
            Ok(ReadOutcome::Submitted) => {
                let ticket = self.next;
                self.next += 1;
                Ok(Some(ticket))
            }
            Err(moat_engine::Error::Busy) => Ok(None),
            other => anyhow::bail!("v1 read failed: {other:?}"),
        }
    }
    fn poll(&mut self, wait: bool, out: &mut Vec<Done>) -> Result<()> {
        self.queue.poll(wait)?;
        self.writer.poll(&mut *self.queue, &mut self.writes)?;
        self.reader.poll(&mut *self.queue, &mut self.reads)?;
        out.extend(
            self.writes
                .drain(..)
                .map(|c| Done::Write(c.ticket, c.result.map(|_| ()).map_err(Into::into))),
        );
        out.extend(
            self.reads
                .drain(..)
                .map(|c| Done::Read(c.token, c.result.map(Data::V1).map_err(Into::into))),
        );
        Ok(())
    }
    fn close(mut self) -> Result<()> {
        let ticket = self.writer.seal(&mut *self.queue)?;
        moat_engine::blocking::wait_with(&mut *self.queue, &mut self.writer, ticket, &mut self.writes)?;
        ensure!(self.writes.is_empty(), "undelivered writes during close");
        self.reader.detach(&mut *self.queue);
        self.writer.detach(&mut *self.queue);
        Ok(())
    }
}
