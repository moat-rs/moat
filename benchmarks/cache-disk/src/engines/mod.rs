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
pub(super) mod v2;

use crate::{Config, Hasher};
use anyhow::{Result, ensure};
use moat_cache::Bytes;
use moat_common::ChunkId;
use std::collections::VecDeque;

pub(super) struct Record {
    pub id: ChunkId,
    pub key: Bytes,
    pub number: usize,
}

pub(super) enum Data {
    V2 {
        buffers: moat_engine_v2::pipeline::ReadBuffers,
        range: moat_engine_v2::pipeline::ReadRange,
    },
    Foyer(::foyer::HybridCacheEntry<Vec<u8>, Vec<u8>, Hasher>),
    #[cfg(test)]
    Test(Vec<u8>),
}
impl Data {
    pub fn check(&self, record: &Record, len: usize) -> Result<()> {
        let bytes = match self {
            Self::V2 { buffers, range } => buffers.view(range.clone()),
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
    pub fn len(&self) -> usize {
        self.value.len()
    }
}
pub(super) enum Value {
    Bytes(Vec<u8>),
    Parts { key: Bytes, value: Vec<u8> },
}
impl Value {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::Parts { key, value } => key.len() + value.len(),
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
        }
    }
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Parts { .. } => unreachable!("split input requires prepared writes"),
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
