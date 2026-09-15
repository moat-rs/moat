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

use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom},
    os::unix::fs::{FileExt, OpenOptionsExt},
};

use moat_common::{AlignedBuf, PAGE_SIZE, align_up};
use moat_engine_v2::{
    frame::{FrameBuilder, FrameLimits, PreparedFrame},
    io::UringQueue,
    pipeline::{Completion, Error, Pipeline, ReadBuffers},
    segment::{SegmentHeader, SegmentId},
};

use crate::{Backend, CAPACITY, Config, DEPTH, MAX_VALUE, Record, SEGMENT, key};

pub(super) struct Unified {
    pipeline: Pipeline<UringQueue>,
    limits: FrameLimits,
    buffers: Vec<AlignedBuf>,
    reads: Vec<ReadBuffers>,
    out: Vec<Completion>,
    acked: u64,
    submitted: u64,
}

impl Unified {
    pub(super) fn new(config: &Config) -> Self {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&config.path)
            .unwrap();
        assert!(file.seek(SeekFrom::End(0)).unwrap() >= CAPACITY);
        let id = SegmentId {
            device_id: crate::fresh_identity(),
            segment_no: 0,
            sequence: 1,
        };
        let header = SegmentHeader::new(id, SEGMENT as u32).unwrap();
        let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
        header.encode_into(&mut page).unwrap();
        file.write_all_at(&page, SEGMENT).unwrap();
        file.sync_data().unwrap();
        let limits = FrameLimits::new(8 << 20, MAX_VALUE).unwrap();
        let pipeline = Pipeline::new(UringQueue::new(file, DEPTH).unwrap(), header, limits, SEGMENT).unwrap();
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
        let buffers = (0..DEPTH).map(|_| AlignedBuf::zeroed(capacity)).collect();
        Self {
            pipeline,
            limits,
            buffers,
            reads: Vec::new(),
            out: Vec::with_capacity(DEPTH),
            acked: 0,
            submitted: 0,
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

    fn buffer(&mut self) -> AlignedBuf {
        while self.buffers.is_empty() {
            self.reap(true);
        }
        self.buffers.pop().unwrap()
    }
}

impl Backend for Unified {
    fn write_batch(&mut self, records: &[Record]) {
        if records.iter().all(|record| record.value.len() >= 65536) {
            for record in records {
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
                            assert!(matches!(rejected.error, Error::Backpressure), "{}", rejected.error);
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
                        assert!(matches!(rejected.error, Error::Backpressure), "{}", rejected.error);
                        buffer = rejected.input;
                        self.reap(true);
                    }
                }
            }
        }
        self.reap(false);
    }

    fn flush(&mut self, _: u64) {
        loop {
            match self.pipeline.flush() {
                Ok(_) => break,
                Err(Error::Backpressure) => self.reap(true),
                Err(error) => panic!("flush failed: {error}"),
            }
        }
        while self.pipeline.in_flight() > 0 {
            self.reap(true);
        }
        assert_eq!(self.acked, self.submitted);
    }

    fn prepare_reads(&mut self, sizes: &[usize], qd: usize) {
        let mut metadata = 4096;
        let mut value = 4096;
        for number in 0..sizes.len() {
            let len = sizes[number % sizes.len()];
            let requirements = self
                .pipeline
                .read_requirements(key(number as u64), 0..len as u32)
                .unwrap();
            metadata = metadata.max(requirements.metadata_len);
            value = value.max(requirements.value_len);
        }
        self.reads = (0..qd)
            .map(|_| ReadBuffers {
                metadata: AlignedBuf::zeroed(metadata),
                value: AlignedBuf::zeroed(value),
            })
            .collect();
    }

    fn read(&mut self, number: u64, len: usize) -> u64 {
        self.pipeline
            .read(
                key(number),
                0..len as u32,
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
