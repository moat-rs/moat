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

use std::ops::{Deref, DerefMut};

use moat_common::{AlignedBuf, PooledBuf};

/// Owned, page-aligned storage returned unchanged by I/O completion.
/// Pool buffers reuse the common arena allocator and may be registered with
/// a queue. Moving this enum neither copies bytes nor clones the pool owner.
#[derive(Debug)]
pub enum Buffer {
    /// Individually allocated, zero-initialized storage.
    Heap(AlignedBuf),
    /// Reusable arena storage; contents are not necessarily zeroed.
    Pooled(PooledBuf),
}

impl From<AlignedBuf> for Buffer {
    fn from(buffer: AlignedBuf) -> Self {
        Self::Heap(buffer)
    }
}

impl From<PooledBuf> for Buffer {
    fn from(buffer: PooledBuf) -> Self {
        Self::Pooled(buffer)
    }
}

impl Deref for Buffer {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            Self::Heap(buffer) => buffer,
            Self::Pooled(buffer) => buffer,
        }
    }
}

impl DerefMut for Buffer {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Heap(buffer) => buffer,
            Self::Pooled(buffer) => buffer,
        }
    }
}
