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

//! Common types shared by every moat component.
//!
//! This crate deliberately has no I/O, no networking and no policy. It only
//! defines the vocabulary the other crates agree on:
//!
//! - [`ChunkId`]: the opaque 128-bit identifier of a chunk.
//! - [`checksum`]: CRC32C helpers and the fixed 64 KiB checksum block size used end to end (client, wire, disk).
//! - [`align`]: 4 KiB page alignment helpers.
//! - [`AlignedBuf`]: a heap buffer whose address and length are page aligned, suitable for `O_DIRECT` I/O and RDMA
//!   registration.

pub mod align;
pub mod arena;
pub mod buf;
pub mod checksum;
pub mod chunk_id;
pub mod pool;

pub use align::{PAGE_SIZE, align_down, align_up, is_aligned};
pub use arena::{Arena, HugePages};
pub use buf::AlignedBuf;
pub use checksum::{
    CHECKSUM_BLOCK_SIZE, Crc32c, block_checksums, block_count, crc32c, verify_blocks, verify_blocks_with,
};
pub use chunk_id::ChunkId;
pub use pool::{BufferPool, PoolOptions, PooledBuf};
