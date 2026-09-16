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

use std::{fs::OpenOptions, io, ops::Range, os::unix::fs::OpenOptionsExt, sync::Arc};

use moat_common::{BufferPool, PAGE_SIZE, align_up};
use moat_engine_v2::{
    engine::{self, Device, Engine, Error, FormatOptions},
    frame::{FrameBuilder, FrameLimits, PreparedFrame},
    io::{Buffer, UringQueue},
    pipeline::{self, Completion, ReadBuffers},
};

use crate::{Backend, Config, DEPTH, MAX_VALUE, Record, SEGMENT, key};

// The same explicit extent bounds formatting, allocation, and recovery.
struct Window {
    file: std::fs::File,
    capacity: u64,
}
impl Device for Window {
    fn capacity(&self) -> io::Result<u64> {
        Ok(self.capacity)
    }
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        assert!(offset <= self.capacity && bytes.len() as u64 <= self.capacity - offset);
        Device::read_at(&self.file, bytes, offset)
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        assert!(offset <= self.capacity && bytes.len() as u64 <= self.capacity - offset);
        Device::write_at(&self.file, bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

pub(super) struct Unified {
    pipeline: Engine<Window, UringQueue>,
    limits: FrameLimits,
    pool: Arc<BufferPool>,
    deferred: bool,
    buffers: Vec<Buffer>,
    reads: Vec<ReadBuffers>,
    out: Vec<Completion>,
    acked: u64,
    submitted: u64,
    verify: bool,
}

impl Unified {
    pub(super) fn new(config: &Config) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&config.path)
            .unwrap();
        assert!(Device::capacity(&file).unwrap() >= config.capacity());
        let limits = FrameLimits::new(8 << 20, MAX_VALUE).unwrap();
        let device = Window {
            file: file.try_clone().unwrap(),
            capacity: config.capacity(),
        };
        engine::format(
            &device,
            FormatOptions {
                device_id: crate::fresh_identity(),
                segment_size: SEGMENT as u32,
                limits,
            },
        )
        .unwrap();
        let pool = BufferPool::new(config.pool_options()).unwrap();
        let queue = UringQueue::with_pool(file, DEPTH, pool.clone()).unwrap();
        let deferred = queue.deferred_taskrun();
        let mut pipeline = Engine::open(device, queue).unwrap();
        if config.whole_device {
            let average = config.sizes().iter().sum::<usize>() / config.sizes().len();
            pipeline.reserve_index(config.bytes as usize / average + 64).unwrap();
        }
        let sizes = config.sizes();
        let capacity = if sizes.len() == 1 && sizes[0] >= 65536 {
            PreparedFrame::required_len(limits, sizes[0] as u32).unwrap()
        } else {
            // Upper bound including metadata, per-record page gaps, and tail.
            align_up(
                (64 * (sizes.iter().sum::<usize>() / sizes.len() + 4096 + 128)) as u64,
                PAGE_SIZE,
            ) as usize
        };
        let buffers = (0..DEPTH).map(|_| pool.alloc(capacity).unwrap().into()).collect();
        Self {
            pipeline,
            pool,
            deferred,
            limits,
            buffers,
            reads: Vec::new(),
            out: Vec::with_capacity(DEPTH),
            acked: 0,
            submitted: 0,
            verify: config.verify,
        }
    }

    fn reap(&mut self, wait: bool) {
        self.pipeline.poll(wait, &mut self.out).unwrap();
        for completion in self.out.drain(..) {
            match completion {
                Completion::Write { result, buffer, .. } => {
                    result.unwrap();
                    self.buffers.push(buffer);
                    self.acked += 1;
                }
                Completion::Flush { result, .. } => {
                    result.unwrap();
                }
                Completion::Read { .. } => unreachable!(),
            }
        }
    }

    fn buffer(&mut self) -> Buffer {
        while self.buffers.is_empty() {
            self.reap(true);
        }
        self.buffers.pop().unwrap()
    }
}

impl Backend for Unified {
    fn memory(&self) -> serde_json::Value {
        crate::memory::snapshot(&self.pool, self.deferred)
    }

    fn write_batch(&mut self, records: &[Record]) -> usize {
        if records.iter().all(|record| record.value.len() >= 65536) {
            for (written, record) in records.iter().enumerate() {
                let mut buffer = self.buffer();
                PreparedFrame::new(self.limits, record.value.len() as u32, &mut buffer)
                    .unwrap()
                    .value_mut()
                    .copy_from_slice(&record.value);
                loop {
                    match self.pipeline.write_prepared(
                        key(record.number),
                        record.number + 1,
                        record.value.len() as u32,
                        buffer,
                    ) {
                        Ok(_) => {
                            self.submitted += 1;
                            break;
                        }
                        Err(rejected) => {
                            if matches!(rejected.error, Error::OutOfSpace) {
                                return written;
                            }
                            assert!(
                                matches!(rejected.error, Error::Pipeline(pipeline::Error::Backpressure)),
                                "{}",
                                rejected.error
                            );
                            buffer = rejected.input;
                            self.reap(true);
                        }
                    }
                }
            }
        } else {
            let mut frame = FrameBuilder::new(self.limits);
            for record in records {
                frame
                    .push(key(record.number), record.number + 1, &record.value)
                    .unwrap();
            }
            let mut buffer = self.buffer();
            loop {
                match self.pipeline.write(&frame, buffer) {
                    Ok(_) => {
                        self.submitted += 1;
                        break;
                    }
                    Err(rejected) => {
                        assert!(
                            matches!(rejected.error, Error::Pipeline(pipeline::Error::Backpressure)),
                            "{}",
                            rejected.error
                        );
                        buffer = rejected.input;
                        self.reap(true);
                    }
                }
            }
        }
        self.reap(false);
        records.len()
    }

    fn seal(&mut self) {
        self.pipeline.seal().unwrap();
    }

    fn usage(&self) -> serde_json::Value {
        serde_json::json!({"segments":self.pipeline.layout().segment_count(),
            "allocated_segments":self.pipeline.allocated_segments(),"indexed_versions":self.pipeline.indexed_versions()})
    }

    fn flush(&mut self, _: u64) {
        loop {
            match self.pipeline.flush() {
                Ok(_) => break,
                Err(Error::Pipeline(pipeline::Error::Backpressure)) => self.reap(true),
                Err(error) => panic!("flush failed: {error}"),
            }
        }
        while self.pipeline.in_flight() > 0 {
            self.reap(true);
        }
        assert_eq!(self.acked, self.submitted);
    }

    fn prepare_reads(&mut self, config: &Config, sizes: &[usize], qd: usize) {
        let mut metadata = 4096;
        let mut value = 4096;
        for number in 0..sizes.len() {
            let len = sizes[number % sizes.len()];
            let requirements = self
                .pipeline
                .read_requirements(key(number as u64), config.read_range(len), self.verify)
                .unwrap();
            metadata = metadata.max(requirements.metadata_len);
            value = value.max(requirements.value_len);
        }
        // Writes are complete; return their buffers before allocating read slots.
        self.buffers.clear();
        self.reads.clear();
        self.reads = (0..qd)
            .map(|_| ReadBuffers {
                metadata: self.verify.then(|| self.pool.alloc(metadata).unwrap().into()),
                value: self.pool.alloc(value).unwrap().into(),
            })
            .collect();
    }

    fn read(&mut self, number: u64, range: Range<u32>) -> u64 {
        self.pipeline
            .read(
                key(number),
                range,
                self.verify,
                self.reads.pop().expect("read depth exceeded"),
            )
            .map_err(|rejected| rejected.error)
            .unwrap()
            .number()
    }

    fn poll_reads(&mut self, mut visit: impl FnMut(u64, &[u8])) {
        self.pipeline.poll(true, &mut self.out).unwrap();
        for completion in self.out.drain(..) {
            match completion {
                Completion::Read {
                    ticket,
                    result,
                    buffers,
                } => {
                    visit(ticket.number(), buffers.view(result.unwrap()));
                    self.reads.push(buffers);
                }
                _ => unreachable!(),
            }
        }
    }
    fn finish(self) {}
}
