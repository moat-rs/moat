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

//! Typed failures from frame construction and validation.

/// Errors from frame construction or validation.
///
/// Match variants and numeric fields to choose a response. Diagnostic strings
/// and formatted messages are for humans and are not a stable parsing API.
/// Errors hold only inline numbers and static strings; construction does not
/// allocate or capture a backtrace. The enum may gain variants in later releases.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A caller supplied invalid geometry or arguments.
    #[error("invalid frame argument: {0}")]
    InvalidArgument(&'static str),
    /// A logical value exceeds the configured value limit. Flushing the
    /// pending frame or providing a larger output buffer cannot make it fit.
    #[error("value of {len} bytes exceeds the limit of {max} bytes")]
    ValueTooLarge {
        /// Logical input length in bytes.
        len: u64,
        /// Maximum logical value length allowed by the format.
        max: u32,
    },
    /// Records, metadata, and padding exceed the format's frame limit.
    /// A writer may close a nonempty builder and retry the next record in a
    /// fresh frame. A record that cannot fit by itself must be rejected.
    #[error("frame needs {required} bytes, limit is {limit}")]
    FrameFull {
        /// Required encoded length, including metadata and page padding.
        required: u64,
        /// Format-wide maximum encoded frame length.
        limit: u32,
    },
    /// The encoded frame exceeds the space remaining at the supplied segment
    /// position. The caller must choose a position with enough space.
    #[error("frame needs {required} bytes, segment position has {available}")]
    SegmentFull {
        /// Required encoded frame length.
        required: u64,
        /// Bytes remaining from the frame position to the segment end.
        available: u32,
    },
    /// The frame fits the format, but the caller's output buffer is too small.
    /// Retry with a larger buffer; segment admission is a separate check.
    #[error("output buffer needs {required} bytes, has {available}")]
    BufferTooSmall {
        /// Minimum required output buffer length.
        required: usize,
        /// Supplied output buffer length.
        available: usize,
    },
    /// An encoded structure or payload is incomplete. A streaming caller may
    /// supply more bytes; a recovery scanner must decide whether this is an
    /// unfinished active tail or corruption before a known sealed boundary.
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
