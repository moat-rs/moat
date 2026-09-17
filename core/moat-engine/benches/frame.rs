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

//! In-memory codec costs; deliberately excludes device I/O and recovery.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use moat_common::{AlignedBuf, ChunkId};
use moat_engine::frame::{Frame, FrameBuilder, FrameLimits, FramePosition, PreparedFrame};

fn measure(name: &str, payload_len: usize, mut operation: impl FnMut()) {
    let start = Instant::now();
    let mut iterations = 0u64;
    while start.elapsed() < Duration::from_millis(200) {
        for _ in 0..128 {
            operation();
        }
        iterations += 128;
    }
    let seconds = start.elapsed().as_secs_f64();
    let ns = seconds * 1e9 / iterations as f64;
    let mib = payload_len as f64 * iterations as f64 / seconds / (1 << 20) as f64;
    println!("{name:32} {ns:12.0} ns/frame {mib:10.0} payload MiB/s");
}

fn main() {
    let limits = FrameLimits::new(8 << 20, 4 << 20).unwrap();
    let position = FramePosition::new(1, 4096, 1 << 30).unwrap();
    for (name, sizes) in [
        ("tiny", vec![100; 128]),
        ("page", vec![4096; 32]),
        ("mixed", vec![100, 4096, 65536, 300]),
        ("large", vec![4 << 20]),
    ] {
        let values: Vec<_> = sizes.iter().map(|&size| vec![0x71; size]).collect();
        let keys: Vec<_> = (0..values.len()).map(|i| ChunkId::from_u128(i as u128)).collect();
        let payload_len: usize = sizes.iter().sum();
        let mut builder = FrameBuilder::new(limits);
        for (&key, value) in keys.iter().zip(&values) {
            builder.push(key, 1, value).unwrap();
        }
        let mut buffer = AlignedBuf::zeroed(builder.encoded_len());
        println!(
            "{name}: {} records, {payload_len} payload bytes, {} encoded bytes",
            values.len(),
            buffer.len()
        );
        measure(&format!("{name}/build-and-encode"), payload_len, || {
            builder.clear();
            for (&key, value) in keys.iter().zip(&values) {
                builder.push(key, 1, black_box(value)).unwrap();
            }
            black_box(builder.encode_into(position, black_box(&mut buffer)).unwrap());
        });
        measure(&format!("{name}/validate-all"), payload_len, || {
            black_box(Frame::decode(black_box(&buffer), limits, position).unwrap());
        });
    }
    let value_len = 4 << 20;
    let mut buffer = AlignedBuf::zeroed(PreparedFrame::required_len(limits, value_len).unwrap());
    // Input filling belongs to the caller. Finalization should only checksum
    // this existing payload and write metadata/padding, without a payload copy.
    PreparedFrame::new(limits, value_len, &mut buffer)
        .unwrap()
        .value_mut()
        .fill(0x71);
    measure("large/prepared-finalize", value_len as usize, || {
        let prepared = PreparedFrame::new(limits, value_len, black_box(&mut buffer)).unwrap();
        black_box(prepared.finish(position, ChunkId::from_u128(0), 1).unwrap());
    });
}
