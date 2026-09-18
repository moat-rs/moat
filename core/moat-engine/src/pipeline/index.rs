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

use std::collections::HashMap;

use moat_common::{ChunkId, chunk_id::ChunkIdHashBuilder};

use crate::frame::{Metadata, RecordKind};

pub(super) type Index = HashMap<ChunkId, Location, ChunkIdHashBuilder>;

#[derive(Debug, Clone, Copy)]
pub(super) struct Location {
    pub segment: u32,
    pub frame_offset: u32,
    pub frame_len: u32,
    pub metadata_len: u32,
    pub ordinal: u32,
    pub lsn: u64,
    pub value_offset: u32,
    pub value_len: u32,
    pub kind: RecordKind,
}

pub(super) fn entries(metadata: Metadata<'_>, segment: u32) -> impl Iterator<Item = (ChunkId, Location)> {
    let header = metadata.header();
    metadata.records().enumerate().map(move |(ordinal, record)| {
        let d = record.descriptor();
        (
            d.key,
            Location {
                segment,
                frame_offset: header.position().offset(),
                frame_len: header.frame_len() as u32,
                metadata_len: header.metadata_len() as u32,
                ordinal: ordinal as u32,
                lsn: d.lsn,
                value_offset: d.value_offset,
                value_len: d.value_len,
                kind: d.kind,
            },
        )
    })
}

pub(super) fn apply(
    index: &mut Index,
    keys: &mut Vec<ChunkId>,
    entries: impl IntoIterator<Item = (ChunkId, Location)>,
) {
    for (key, location) in entries {
        // Physical completion order does not determine logical version order.
        // Keep tombstones so an older late-arriving value cannot resurrect a key.
        match index.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if location.lsn > entry.get().lsn {
                    entry.insert(location);
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                keys.push(key);
                entry.insert(location);
            }
        }
    }
}
