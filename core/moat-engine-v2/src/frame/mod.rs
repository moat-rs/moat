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

//! A single, versioned encoding for packed values, large values, and tombstones.
//!
//! Frames start and end on 4 KiB boundaries. A 64-byte header precedes a
//! variable-length record directory and checksum area; each value occupies one
//! contiguous range. All offsets are frame-relative unless stated otherwise.
//! See the crate README for the encoding and buffer ownership contract.

mod builder;
mod codec;
mod decode;
mod error;
mod header;
mod record;

pub use builder::{FrameBuilder, PreparedFrame};
pub use decode::{Frame, Metadata, Record};
pub use error::{Error, Result};
pub use header::{FrameHeader, FrameLimits, FramePosition};
pub use record::{RecordDescriptor, RecordKind};

/// Size of an encoded frame header.
pub const HEADER_LEN: usize = 64;
/// Size of an encoded record descriptor.
pub const DESCRIPTOR_LEN: usize = 64;
/// Persistent format version, distinct from the original engine's version 1.
pub const FORMAT_VERSION: u32 = 2;
/// Frame identification bytes. Legacy batch encodings are never accepted.
pub const MAGIC: [u8; 8] = *b"MOATFRM2";

const PAGE: usize = moat_common::PAGE_SIZE as usize;
// Fixed format invariant. The small-value CRC path can consume aligned u64
// words without first processing an unaligned bytewise prefix.
const VALUE_ALIGN: usize = 8;

// All layout calculations use u64, including on 32-bit hosts. Callers check
// against the u32 format bound before converting back to slice indices.
fn align_up(value: u64, alignment: usize) -> u64 {
    value.div_ceil(alignment as u64) * alignment as u64
}
