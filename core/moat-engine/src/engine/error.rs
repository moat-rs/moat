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

//! Device lifecycle errors, separate from frame and pipeline diagnostics.

use crate::{frame, pipeline, segment};

/// Device lifecycle and operation failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Caller-supplied geometry or segment selection is invalid.
    #[error("invalid device argument: {0}")]
    InvalidArgument(&'static str),
    /// No valid device superblock exists.
    #[error("device is not formatted or both superblocks are damaged")]
    NotFormatted,
    /// Committed metadata is inconsistent.
    #[error("corrupt device metadata: {0}")]
    Corrupt(&'static str),
    /// The persistent device version is not supported.
    #[error("unsupported device version {0}")]
    UnsupportedVersion(u32),
    /// All allocation slots have been used; no implicit reclamation is performed.
    #[error("device has no unused segments")]
    OutOfSpace,
    /// A lifecycle write or persistence barrier failed; reopen before writing.
    #[error("device write lifecycle has failed")]
    Failed,
    /// The requested in-memory index reservation could not be allocated.
    #[error("index reservation failed: {0}")]
    IndexAllocation(#[from] std::collections::TryReserveError),
    /// Positional metadata I/O or a persistence barrier failed.
    #[error("device I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Read/write admission, completion, or verification failed.
    #[error(transparent)]
    Pipeline(#[from] pipeline::Error),
    /// Invalid segment metadata or recovery failure.
    #[error(transparent)]
    Segment(#[from] segment::Error),
    /// Invalid frame limits or recovery failure.
    #[error(transparent)]
    Frame(#[from] frame::Error),
}

/// Device operation result.
pub type Result<T> = std::result::Result<T, Error>;
/// Unaccepted operation with its original input ownership preserved.
pub type Rejected<T> = pipeline::Rejected<T, Error>;
