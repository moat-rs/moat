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

//! Typed failures from segment metadata and recovery.

use crate::frame;

/// Segment construction, validation, or recovery failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Metadata allocation failed before admission.
    #[error("segment metadata allocation failed: {0}")]
    Allocation(#[from] std::collections::TryReserveError),
    /// Caller-supplied segment geometry or frame order is invalid.
    #[error("invalid segment argument: {0}")]
    InvalidArgument(&'static str),
    /// A finished builder cannot admit more frames.
    #[error("segment builder is already sealed")]
    Sealed,
    /// Data and the eventual footer do not both fit; choose another segment.
    #[error("segment needs {required} bytes including its footer, capacity is {capacity}")]
    Full {
        /// Total required segment bytes, including its header and footer.
        required: u64,
        /// Physical segment capacity in bytes.
        capacity: u32,
    },
    /// The destination is too short; no output or builder state was changed.
    #[error("output buffer needs {required} bytes, has {available}")]
    BufferTooSmall {
        /// Minimum output size.
        required: usize,
        /// Supplied output size.
        available: usize,
    },
    /// Encoded segment metadata is incomplete.
    #[error("truncated segment metadata: need {required} bytes, have {available}")]
    Truncated {
        /// Minimum input size.
        required: usize,
        /// Supplied input size.
        available: usize,
    },
    /// The segment or footer uses an unsupported encoding version.
    #[error("unsupported segment metadata version {0}")]
    UnsupportedVersion(u32),
    /// A checksum, identity, or structural invariant failed.
    #[error("corrupt segment: {0}")]
    Corrupt(&'static str),
    /// A frame failed validation at a known segment-relative offset.
    #[error("invalid frame at segment offset {offset}: {source}")]
    Frame {
        /// Physical frame offset in bytes.
        offset: u32,
        /// Original frame validation failure.
        #[source]
        source: frame::Error,
    },
}

/// Result of a segment operation.
pub type Result<T> = std::result::Result<T, Error>;
