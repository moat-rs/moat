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

//! Disk discovery, placement and exclusive v2 engine owner threads.
//! Each worker owns its assigned devices and their individual queues.

pub mod disk;
pub mod node;
pub mod placement;
pub mod storage;
pub mod worker;

pub use node::{Node, NodeError};
pub use placement::{Placement, Target};
pub use worker::{
    Context, DiskId, DiskSlot, Handler, PollMode, QueueBackend, Step, Worker, WorkerError, WorkerOptions,
};
