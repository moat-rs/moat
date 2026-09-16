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

use moat_common::ChunkId;

use super::{DESCRIPTOR_LEN, Error, Result, codec::*};

/// The logical meaning of a directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// An immutable value. Zero-length values are valid data.
    Data = 1,
    /// An explicit deletion with no value and no payload checksums.
    Tombstone = 2,
}

/// Decoded fields of a 64-byte record directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordDescriptor {
    /// Full 128-bit chunk identifier.
    pub key: ChunkId,
    /// Logical sequence number, independent of physical placement.
    pub lsn: u64,
    /// Frame-relative value offset; zero for an empty value or tombstone.
    pub value_offset: u32,
    /// Logical value length in bytes.
    pub value_len: u32,
    /// Frame-relative checksum offset; zero when there are no checksums.
    pub checksum_offset: u32,
    /// Number of CRC32C entries, one per logical 64 KiB value block.
    pub checksum_count: u32,
    /// Data or explicit deletion.
    pub kind: RecordKind,
}

impl RecordDescriptor {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        let kind = match bytes[40] {
            1 => RecordKind::Data,
            2 => RecordKind::Tombstone,
            _ => return Err(Error::Corrupt("record kind")),
        };
        if bytes[41..DESCRIPTOR_LEN].iter().any(|&byte| byte != 0) {
            return Err(Error::Corrupt("unsupported record flags or reserved bytes"));
        }
        Ok(Self {
            key: ChunkId::from_bytes(bytes[..16].try_into().expect("validated descriptor length")),
            lsn: u64_at(bytes, 16),
            value_offset: u32_at(bytes, 24),
            value_len: u32_at(bytes, 28),
            checksum_offset: u32_at(bytes, 32),
            checksum_count: u32_at(bytes, 36),
            kind,
        })
    }

    pub(super) fn encode(self, bytes: &mut [u8]) {
        bytes[..DESCRIPTOR_LEN].fill(0);
        bytes[..16].copy_from_slice(self.key.as_bytes());
        put_u64(bytes, 16, self.lsn);
        put_u32(bytes, 24, self.value_offset);
        put_u32(bytes, 28, self.value_len);
        put_u32(bytes, 32, self.checksum_offset);
        put_u32(bytes, 36, self.checksum_count);
        bytes[40] = self.kind as u8;
    }
}
