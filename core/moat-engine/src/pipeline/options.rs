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

//! Explicit resource bounds and cooperative progress budgets.

/// Maximum work retired by one poll. A single indivisible frame/read may exceed
/// the byte or record target; subsequent operations wait for the next poll.
#[derive(Debug, Clone, Copy)]
pub struct PollBudget {
    /// Maximum completion/state-machine steps; must be nonzero.
    pub operations: usize,
    /// Encoded/verified bytes processed before yielding; must be nonzero.
    pub bytes: usize,
    /// Records published before yielding; must be nonzero.
    pub records: usize,
}
impl Default for PollBudget {
    fn default() -> Self {
        Self {
            operations: 64,
            bytes: 1 << 20,
            records: 4096,
        }
    }
}

/// Hard bounds on logical metadata quantities, independent of I/O pool size.
/// Hash-table allocator overhead and caller-owned buffers are additional.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Maximum encoded frame buffer; bounds indivisible encoding/CRC work.
    pub frame_bytes: usize,
    /// Indexed keys, including tombstones, plus conservative in-flight reservations.
    pub index_entries: usize,
    /// Separate bounds for active-segment metadata, a recovery footer, and
    /// logical device routing entries (allocator capacity overhead is additional).
    pub metadata_bytes: usize,
    /// Maximum in-flight index entries (including overwrites).
    pub pending_records: usize,
}
impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            frame_bytes: 64 << 20,
            index_entries: 1 << 20,
            metadata_bytes: 64 << 20,
            pending_records: 1 << 20,
        }
    }
}
