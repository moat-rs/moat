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

//! Errors and lossless admission rejection for the single-owner pipeline.

use std::io;

use crate::{frame, io::Operation, segment};

/// Runtime errors; I/O failures preserve the OS error as their source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Retry after polling completions; no work was accepted.
    #[error("pipeline is full or a flush is pending")]
    Backpressure,
    /// A recovered pipeline cannot write into its old allocation.
    #[error("pipeline is read-only")]
    ReadOnly,
    /// An earlier write or barrier failed. Stop using this allocation for writes.
    #[error("write path failed at ticket {0}")]
    WriteFailed(u64),
    /// The key is absent or its newest version is a tombstone.
    #[error("chunk not found")]
    NotFound,
    /// Invalid caller geometry or an exhausted ticket counter.
    #[error("invalid pipeline argument: {0}")]
    InvalidArgument(&'static str),
    /// Frame construction or verification error.
    #[error(transparent)]
    Frame(#[from] frame::Error),
    /// Segment allocation or metadata error.
    #[error(transparent)]
    Segment(#[from] segment::Error),
    /// An operation failed at a physical file position.
    #[error("{operation:?} at byte {offset} failed: {source}")]
    Io {
        /// Failed operation.
        operation: Operation,
        /// Absolute byte offset.
        offset: u64,
        /// Original OS failure.
        #[source]
        source: io::Error,
    },
    /// A short transfer is not silently retried as a full operation.
    #[error("short {operation:?} at byte {offset}: expected {expected}, got {actual}")]
    ShortIo {
        /// Operation that completed short.
        operation: Operation,
        /// Absolute byte offset.
        offset: u64,
        /// Requested bytes.
        expected: usize,
        /// Transferred bytes.
        actual: usize,
    },
    /// The queue cannot establish completion state; abandon this pipeline.
    #[error("I/O queue failed: {0}")]
    Queue(#[source] io::Error),
    /// Further operations cannot use a failed queue.
    #[error("I/O queue is no longer usable")]
    QueueFailed,
}

/// Pipeline operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// Rejected admission with input ownership preserved for retry.
#[derive(Debug)]
pub struct Rejected<T> {
    /// Why admission failed.
    pub error: Error,
    /// Original input buffers, still owned by the caller.
    pub input: T,
}
