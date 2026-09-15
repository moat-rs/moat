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

//! Small file-backed pipeline example, not a benchmark or a device formatter.

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{fs::OpenOptions, os::unix::fs::FileExt};

    use moat_common::{AlignedBuf, ChunkId, PAGE_SIZE};
    use moat_engine_v2::{
        frame::{FrameBuilder, FrameHeader, FrameLimits},
        io::FileQueue,
        pipeline::{Completion, Pipeline, ReadBuffers},
        segment::{Scanner, SegmentHeader, SegmentId},
    };

    let path = std::env::args_os().nth(1).ok_or("usage: segment_io NEW_FILE_PATH")?;
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
    let segment_len = 256 * 1024;
    file.set_len(segment_len as u64)?;
    // This example fixes its geometry here. A complete device opener must instead
    // load format limits and device identity from durable device metadata.
    let limits = FrameLimits::new(128 * 1024, 96 * 1024)?;
    let id = SegmentId {
        device_id: [1; 16],
        segment_no: 0,
        sequence: 1,
    };
    let header = SegmentHeader::new(id, segment_len)?;
    let mut page = AlignedBuf::zeroed(PAGE_SIZE as usize);
    header.encode_into(&mut page)?;
    file.write_all_at(&page, 0)?;
    file.sync_data()?;

    let mut pipeline = Pipeline::new(FileQueue::new(file.try_clone()?, 4)?, header, limits, 0)?;
    let key = ChunkId::from_u128(1);
    let mut frame = FrameBuilder::new(limits);
    frame.push(key, 1, b"hello from a frame")?;
    pipeline
        .write(&frame, AlignedBuf::zeroed(frame.encoded_len()))
        .map_err(|rejected| rejected.error)?;
    pipeline.flush()?;
    let mut completions = Vec::new();
    while pipeline.in_flight() != 0 {
        pipeline.poll(true, &mut completions)?;
    }
    for completion in completions.drain(..) {
        match completion {
            Completion::Write { result, .. } | Completion::Flush { result, .. } => result?,
            Completion::Read { .. } => unreachable!(),
        }
    }
    drop(pipeline);

    // Recover without rewriting the active header or appending to its old tail.
    file.read_exact_at(&mut page, 0)?;
    let recovered = SegmentHeader::decode(&page, id.device_id, id.segment_no, segment_len)?;
    let mut scanner = Scanner::new(recovered, limits);
    let mut reader = Pipeline::read_only(FileQueue::new(file.try_clone()?, 4)?, recovered, limits, 0)?;
    while let Some(position) = scanner.position() {
        file.read_exact_at(&mut page, position.offset() as u64)?;
        let header = match FrameHeader::decode(&page, limits, position) {
            Ok(header) => header,
            Err(_) => {
                scanner.next_frame(&page)?;
                break;
            }
        };
        let mut bytes = AlignedBuf::zeroed(header.frame_len());
        bytes[..page.len()].copy_from_slice(&page);
        if bytes.len() > page.len() {
            file.read_exact_at(&mut bytes[page.len()..], position.offset() as u64 + PAGE_SIZE)?;
        }
        if let Some(frame) = scanner.next_frame(&bytes)? {
            reader.restore(frame.metadata())?;
        }
    }
    let buffers = ReadBuffers {
        metadata: Some(AlignedBuf::zeroed(PAGE_SIZE as usize).into()),
        value: AlignedBuf::zeroed(PAGE_SIZE as usize).into(),
    };
    reader
        .read(key, 0..5, true, buffers)
        .map_err(|rejected| rejected.error)?;
    while reader.in_flight() != 0 {
        reader.poll(true, &mut completions)?;
    }
    for completion in completions {
        if let Completion::Read { result, buffers, .. } = completion {
            assert_eq!(buffers.view(result?), b"hello");
        }
    }
    println!("Write, flush, recovery, and verified read succeeded.");
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("This file I/O example requires Unix.");
}
