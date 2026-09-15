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

use std::{io, os::fd::BorrowedFd, sync::Arc};

use moat_common::{HugePages, PoolOptions};
use moat_engine::{
    Device, Error, FileDevice, FormatOptions, IoQueue, Options, PutOptions, PutOutcome, QueueBackend, QueueOptions,
    ReadOutcome, Reader, Writer, blocking,
};

use crate::{Backend, CAPACITY, Config, DEPTH, MAX_VALUE, Record, SEGMENT, key};

// The engine derives all async offsets from this capacity. This wrapper also
// bounds the blocking format/recovery path; it never formats the whole disk.
struct Window(FileDevice);

impl Device for Window {
    fn capacity(&self) -> u64 {
        CAPACITY
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        assert!(offset <= CAPACITY && buf.len() as u64 <= CAPACITY - offset);
        self.0.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        assert!(offset <= CAPACITY && buf.len() as u64 <= CAPACITY - offset);
        self.0.write_at(buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.0.sync()
    }
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.0.fd()
    }
}

pub(super) struct Legacy {
    queue: Box<dyn IoQueue>,
    writer: Writer,
    reader: Reader,
    writes: Vec<moat_engine::Completion>,
    reads: Vec<moat_engine::ReadCompletion>,
    acked: u64,
    next: u64,
}

impl Legacy {
    pub(super) fn new(config: &Config) -> Self {
        let device = FileDevice::open(&config.path, true).unwrap();
        assert!(device.capacity() >= CAPACITY);
        let device = Arc::new(Window(device));
        moat_engine::format(
            &*device,
            &FormatOptions {
                segment_size: SEGMENT,
                chunk_max: MAX_VALUE,
                disk_uuid: crate::fresh_identity(),
            },
        )
        .unwrap();
        let sizes = config.sizes();
        let average = sizes.iter().sum::<usize>() / sizes.len();
        let index_capacity = ((config.bytes as usize / average + 64) * 2).next_power_of_two();
        let (engine, _) = moat_engine::open(
            device,
            Options {
                index_capacity,
                verify_reads: true,
                sync_on_flush: true,
                ..Options::default()
            },
        )
        .unwrap();
        let mut queue = QueueOptions {
            depth: DEPTH as u32,
            pool: PoolOptions {
                bytes: 1 << 30,
                max_class: 8 << 20,
                huge_pages: HugePages::Disabled,
            },
            descriptors: 4,
        }
        .build(QueueBackend::Uring)
        .unwrap();
        let writer = engine.writer(&mut *queue).unwrap();
        let reader = engine.reader(&mut *queue).unwrap();
        Self {
            queue,
            writer,
            reader,
            writes: Vec::with_capacity(DEPTH * 64),
            reads: Vec::with_capacity(DEPTH),
            acked: 0,
            next: 0,
        }
    }

    fn reap(&mut self, wait: bool) {
        self.queue.poll(wait).unwrap();
        self.writer.poll(&mut *self.queue, &mut self.writes).unwrap();
        for completion in self.writes.drain(..) {
            completion.result.unwrap();
            self.acked += 1;
        }
    }
}

impl Backend for Legacy {
    fn write_batch(&mut self, records: &[Record]) {
        for record in records {
            loop {
                let result = if record.value.len() >= 65536 {
                    match self.writer.prepare_large(&mut *self.queue, record.value.len() as u32) {
                        Ok(mut value) => {
                            value.value_mut().copy_from_slice(&record.value);
                            // CRC computation is included in the measured write path.
                            self.writer.put_large(
                                &mut *self.queue,
                                key(record.number),
                                value,
                                None,
                                PutOptions::default(),
                            )
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    self.writer.put(
                        &mut *self.queue,
                        key(record.number),
                        &record.value,
                        PutOptions::default(),
                    )
                };
                match result {
                    Ok(PutOutcome::Written { .. }) => break,
                    Err(Error::Busy) => self.reap(true),
                    other => panic!("write failed: {other:?}"),
                }
            }
        }
        self.reap(false);
    }

    fn flush(&mut self, records: u64) {
        let ticket = self.writer.flush(&mut *self.queue).unwrap();
        blocking::wait_with(&mut *self.queue, &mut self.writer, ticket, &mut self.writes).unwrap();
        for completion in self.writes.drain(..) {
            completion.result.unwrap();
            self.acked += 1;
        }
        assert_eq!(self.acked, records);
    }

    fn prepare_reads(&mut self, _: &[usize], _: usize) {}

    fn read(&mut self, number: u64, _: usize) -> u64 {
        let ticket = self.next;
        self.next += 1;
        assert!(matches!(
            self.reader.get(&mut *self.queue, &key(number), None, ticket).unwrap(),
            ReadOutcome::Submitted
        ));
        ticket
    }

    fn poll_reads(&mut self, mut visit: impl FnMut(u64, &[u8])) {
        self.queue.poll(true).unwrap();
        self.reader.poll(&mut *self.queue, &mut self.reads).unwrap();
        for completion in self.reads.drain(..) {
            let data = completion.result.unwrap();
            visit(completion.token, &data);
        }
    }
    fn finish(mut self) {
        self.reader.detach(&mut *self.queue);
        self.writer.detach(&mut *self.queue);
    }
}
