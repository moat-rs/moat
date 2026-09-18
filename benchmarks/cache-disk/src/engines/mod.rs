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

//! Polling adapters. Engine operations stay on the calling thread; only foyer
//! creates a Tokio runtime. No per-request channel or task wraps engine I/O.

pub(super) mod foyer;
pub(super) mod moat;

use crate::{Config, Hasher};
use anyhow::{Result, ensure};
use moat_cache::Bytes;
use moat_common::{AlignedBuf, ChunkId, align::PAGE_SIZE};
use std::collections::VecDeque;

pub(super) struct Record {
    pub id: ChunkId,
    pub key: Bytes,
    pub number: usize,
}

pub(super) enum Data {
    Moat {
        buffers: moat_engine::pipeline::ReadBuffers,
        range: moat_engine::pipeline::ReadRange,
    },
    Foyer(::foyer::HybridCacheEntry<Vec<u8>, Vec<u8>, Hasher>),
    #[cfg(test)]
    Test(Vec<u8>),
}
impl Data {
    pub fn check(&self, record: &Record, len: usize) -> Result<()> {
        let bytes = match self {
            Self::Moat { buffers, range } => buffers.view(range.clone()),
            Self::Foyer(entry) => {
                ensure!(entry.key().as_slice() == record.key.as_ref(), "full key mismatch");
                return check_value(entry.value(), record.number, len);
            }
            #[cfg(test)]
            Self::Test(bytes) => bytes,
        };
        ensure!(
            bytes.get(..record.key.len()) == Some(record.key.as_ref()),
            "full key mismatch"
        );
        check_value(&bytes[record.key.len()..], record.number, len)
    }
}
fn check_value(value: &[u8], number: usize, len: usize) -> Result<()> {
    ensure!(value.len() == len, "wrong value length");
    ensure!(value[..8] == (number as u64).to_le_bytes(), "wrong value prefix");
    ensure!(
        value[len - 8..] == (!(number as u64)).to_le_bytes(),
        "wrong value suffix"
    );
    Ok(())
}

pub(super) struct Put {
    id: ChunkId,
    value: Value,
}
impl Put {
    pub fn new(c: &Config, record: &Record) -> Self {
        let split = c.engine == "foyer" || c.key_bytes + c.value_bytes >= 65536;
        if c.engine == "moat" && c.moat_input_pool && split {
            // Stabilize source alignment across engine revisions: initialization
            // allocations must not determine the timed payload-copy alignment.
            let mut value = AlignedBuf::zeroed(c.value_bytes.div_ceil(PAGE_SIZE as usize) * PAGE_SIZE as usize);
            value[..c.value_bytes].fill(0x7c);
            crate::stamp_value(&mut value[..c.value_bytes], record.number);
            return Self {
                id: record.id,
                value: Value::AlignedParts {
                    key: record.key.clone(),
                    value,
                    len: c.value_bytes,
                },
            };
        }
        let prefix = if split { 0 } else { c.key_bytes };
        let mut value = vec![0x7c; prefix + c.value_bytes];
        if prefix > 0 {
            value[..prefix].copy_from_slice(&record.key);
        }
        crate::stamp_value(&mut value[prefix..], record.number);
        let value = if split {
            Value::Parts {
                key: record.key.clone(),
                value,
            }
        } else {
            Value::Bytes(value)
        };
        Self { id: record.id, value }
    }
    // Moat copies the input on admission, so its caller can reuse this storage.
    // Keep full value generation timed, including bytes outside identity stamps.
    pub fn reset(&mut self, c: &Config, record: &Record) {
        self.id = record.id;
        match &mut self.value {
            Value::Bytes(bytes) => {
                debug_assert_eq!(bytes.len(), c.key_bytes + c.value_bytes);
                bytes.fill(0x7c);
                bytes[..c.key_bytes].copy_from_slice(&record.key);
                crate::stamp_value(&mut bytes[c.key_bytes..], record.number);
            }
            Value::Parts { key, value } => {
                debug_assert_eq!(value.len(), c.value_bytes);
                *key = record.key.clone();
                value.fill(0x7c);
                crate::stamp_value(value, record.number);
            }
            Value::AlignedParts { key, value, len } => {
                debug_assert_eq!(*len, c.value_bytes);
                *key = record.key.clone();
                let value = &mut value[..*len];
                value.fill(0x7c);
                crate::stamp_value(value, record.number);
            }
        }
    }
    pub fn len(&self) -> usize {
        self.value.len()
    }
}
pub(super) enum Value {
    Bytes(Vec<u8>),
    Parts { key: Bytes, value: Vec<u8> },
    AlignedParts { key: Bytes, value: AlignedBuf, len: usize },
}
impl Value {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::Parts { key, value } => key.len() + value.len(),
            Self::AlignedParts { key, len, .. } => key.len() + len,
        }
    }
    fn copy_into(&self, output: &mut [u8]) {
        match self {
            Self::Bytes(bytes) => output.copy_from_slice(bytes),
            Self::Parts { key, value } => {
                let (prefix, payload) = output.split_at_mut(key.len());
                prefix.copy_from_slice(key);
                payload.copy_from_slice(value);
            }
            Self::AlignedParts { key, value, len } => {
                let (prefix, payload) = output.split_at_mut(key.len());
                prefix.copy_from_slice(key);
                payload.copy_from_slice(&value[..*len]);
            }
        }
    }
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Parts { .. } | Self::AlignedParts { .. } => unreachable!("split input requires prepared writes"),
        }
    }
}

pub(super) enum Done {
    Write(u64, Result<()>),
    Read(u64, Result<Data>),
}

pub(super) trait Backend {
    // None leaves the front request intact: poll, then retry the same request.
    // On success the caller removes exactly the reported number of requests.
    fn put(&mut self, batch: &mut VecDeque<Put>) -> Result<Option<(u64, usize)>>;
    fn read(&mut self, record: &Record) -> Result<Option<u64>>;
    fn poll(&mut self, out: &mut Vec<Done>) -> Result<()>;
    // Complete a write batch, including asynchronous admission in foyer.
    fn drain(&mut self) -> Result<()> {
        Ok(())
    }
    fn close(self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_input_preserves_allocation_and_replaces_contents() {
        for value_bytes in [100, 65520, 65536, 65537, 4 << 20] {
            let c: Config = serde_json::from_value(serde_json::json!({
                "host":"unused", "engine":"moat", "disks":[], "bytes_per_disk":1073741824_u64,
                "records_per_disk":1, "key_bytes":16, "value_bytes":value_bytes,
                "clients":1, "runtime_cpus":[], "io_cpus":[], "seconds":1, "repeats":1,
                "pool_bytes_per_disk":67108864
            }))
            .unwrap();
            let first = Record {
                id: ChunkId::from_u128(1),
                key: Bytes::from(vec![1; 16]),
                number: 1,
            };
            let second = Record {
                id: ChunkId::from_u128(2),
                key: Bytes::from(vec![2; 16]),
                number: 17,
            };
            let mut input = Put::new(&c, &first);
            let allocation = match &mut input.value {
                Value::Bytes(bytes) | Value::Parts { value: bytes, .. } => {
                    let allocation = (bytes.as_ptr(), bytes.capacity());
                    bytes.fill(0);
                    allocation
                }
                Value::AlignedParts { value, len, .. } => {
                    assert_eq!(*len, value_bytes);
                    assert_eq!(value.as_ptr() as usize % PAGE_SIZE as usize, 0);
                    let allocation = (value.as_ptr(), value.len());
                    value.fill(0);
                    allocation
                }
            };
            input.reset(&c, &second);
            let reused = match &input.value {
                Value::Bytes(bytes) | Value::Parts { value: bytes, .. } => (bytes.as_ptr(), bytes.capacity()),
                Value::AlignedParts { value, .. } => (value.as_ptr(), value.len()),
            };
            assert_eq!(reused, allocation);
            assert_eq!(input.len(), c.key_bytes + value_bytes);
            assert_eq!(input.id, second.id);
            let mut output = vec![0; input.len()];
            input.value.copy_into(&mut output);
            assert_eq!(&output[..16], second.key.as_ref());
            check_value(&output[16..], second.number, value_bytes).unwrap();
            assert!(output[24..output.len() - 8].iter().all(|&byte| byte == 0x7c));
        }
    }
}
