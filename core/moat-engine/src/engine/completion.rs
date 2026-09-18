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

//! Device-level operations and terminal notifications.

use super::Result;
use crate::{
    io::Buffer,
    pipeline::{self, ReadBuffers, ReadRange, Ticket},
};

/// Lifecycle operation associated with a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Recover persisted geometry and indexes.
    Open,
    /// Seal the active segment, persisting it when sync is enabled.
    Seal,
    /// Seal and allocate another segment.
    Rollover,
    /// Stop admission, drain, and seal using the configured sync policy.
    Close,
}
/// Engine operation completion. Buffer ownership follows pipeline completions.
#[derive(Debug)]
pub enum Completion {
    /// A lifecycle request completed; failure does not authorize further writes.
    Lifecycle {
        /// Accepted request identity.
        ticket: Ticket,
        /// Requested transition.
        operation: Lifecycle,
        /// Transition result.
        result: Result<()>,
    },
    /// A frame was published, or failed. Completion alone is not durability.
    Write {
        /// Request identity.
        ticket: Ticket,
        /// Publication result.
        result: pipeline::Result<()>,
        /// Original buffer.
        buffer: Buffer,
    },
    /// A read completed with its owned buffers.
    Read {
        /// Request identity.
        ticket: Ticket,
        /// Result slice location.
        result: pipeline::Result<ReadRange>,
        /// Original buffers.
        buffers: ReadBuffers,
    },
    /// Preceding writes completed, followed by a persistence barrier if enabled.
    Flush {
        /// Request identity.
        ticket: Ticket,
        /// Write-fence and optional persistence result.
        result: pipeline::Result<()>,
    },
    /// A fatal queue failure terminated an accepted operation. Buffers still
    /// accessible by the OS remain with the queue until safe teardown.
    Failed {
        /// Terminated request identity.
        ticket: Ticket,
        /// Shared failure cause.
        error: std::sync::Arc<pipeline::Error>,
    },
}
impl Completion {
    /// Identity scoped to this engine instance.
    pub fn ticket(&self) -> Ticket {
        match self {
            Self::Lifecycle { ticket, .. }
            | Self::Write { ticket, .. }
            | Self::Read { ticket, .. }
            | Self::Flush { ticket, .. }
            | Self::Failed { ticket, .. } => *ticket,
        }
    }
    /// Extracts a data-path completion for adapters that drive lifecycle requests separately.
    pub fn into_pipeline(self) -> Option<pipeline::Completion> {
        Some(match self {
            Self::Lifecycle { .. } => return None,
            Self::Write { ticket, result, buffer } => pipeline::Completion::Write { ticket, result, buffer },
            Self::Read {
                ticket,
                result,
                buffers,
            } => pipeline::Completion::Read {
                ticket,
                result,
                buffers,
            },
            Self::Flush { ticket, result } => pipeline::Completion::Flush { ticket, result },
            Self::Failed { ticket, error } => pipeline::Completion::Failed { ticket, error },
        })
    }
}
impl From<pipeline::Completion> for Completion {
    fn from(value: pipeline::Completion) -> Self {
        match value {
            pipeline::Completion::Write { ticket, result, buffer } => Self::Write { ticket, result, buffer },
            pipeline::Completion::Read {
                ticket,
                result,
                buffers,
            } => Self::Read {
                ticket,
                result,
                buffers,
            },
            pipeline::Completion::Flush { ticket, result } => Self::Flush { ticket, result },
            pipeline::Completion::Failed { ticket, error } => Self::Failed { ticket, error },
        }
    }
}
