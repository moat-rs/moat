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
mod header;
mod record;

pub use builder::{FrameBuilder, PreparedFrame};
pub use decode::{Frame, Metadata, Record};
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
const VALUE_ALIGN: usize = 8;

/// Errors from frame construction or validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A caller supplied invalid geometry or arguments.
    #[error("invalid frame argument: {0}")]
    InvalidArgument(&'static str),
    /// A value, frame, or destination exceeds its byte limit.
    #[error("frame needs {required} bytes, limit is {limit}")]
    Full {
        /// Required size, including metadata and alignment.
        required: usize,
        /// Available capacity.
        limit: usize,
    },
    /// An encoded structure or payload is incomplete.
    #[error("truncated frame: need {required} bytes, have {available}")]
    Truncated {
        /// Minimum byte length needed to continue decoding.
        required: usize,
        /// Supplied byte length.
        available: usize,
    },
    /// The encoding is recognized but its version is unsupported.
    #[error("unsupported frame version {0}")]
    UnsupportedVersion(u32),
    /// Encoded metadata is inconsistent or failed its checksum.
    #[error("corrupt frame: {0}")]
    Corrupt(&'static str),
    /// A logical value checksum failed.
    #[error("checksum mismatch in record {record}, block {block}")]
    PayloadChecksum {
        /// Directory index, independent of physical value order.
        record: u32,
        /// Checksum block index within the value.
        block: u32,
    },
}

/// Result of a frame operation.
pub type Result<T> = std::result::Result<T, Error>;

// All layout calculations use u64, including on 32-bit hosts. Callers check
// against the u32 format bound before converting back to slice indices.
fn align_up(value: u64, alignment: usize) -> u64 {
    value.div_ceil(alignment as u64) * alignment as u64
}
