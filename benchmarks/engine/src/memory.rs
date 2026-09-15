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

//! Memory configuration and observed huge-page backing, outside timed work.

use moat_common::BufferPool;
use serde_json::{Value, json};

pub(super) fn snapshot(pool: &BufferPool, deferred: bool) -> Value {
    let arenas: Vec<_> = pool
        .arenas()
        .iter()
        .map(|arena| {
            let start = arena.as_ptr() as usize;
            start..start + arena.len()
        })
        .collect();
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
    let mut selected = false;
    let mut exact = true;
    let mut covered = 0;
    let mut anon_huge = 0u64;
    let mut hugetlb = 0u64;
    for line in smaps.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if let Some((start, end)) = fields[0].split_once('-') {
            let start = usize::from_str_radix(start, 16).unwrap();
            let end = usize::from_str_radix(end, 16).unwrap();
            selected = arenas.iter().any(|arena| start < arena.end && end > arena.start);
            if selected {
                // Do not attribute huge pages from a merged, unrelated mapping.
                exact &= arenas.iter().any(|arena| start >= arena.start && end <= arena.end);
                covered += end - start;
            }
        } else if selected {
            match fields[0] {
                "AnonHugePages:" => anon_huge += fields[1].parse::<u64>().unwrap() * 1024,
                "Private_Hugetlb:" | "Shared_Hugetlb:" => hugetlb += fields[1].parse::<u64>().unwrap() * 1024,
                _ => {}
            }
        }
    }
    exact &= covered == pool.arenas().iter().map(|a| a.len()).sum::<usize>();
    json!({
        "pool_bytes": pool.capacity(),
        "arena_backing": pool.arenas().iter().map(|a| format!("{:?}", a.backing())).collect::<Vec<_>>().join(","),
        "registered_buffers": true,
        "fixed_files": true,
        "deferred_taskrun": deferred,
        "observed_anon_huge_bytes": exact.then_some(anon_huge),
        "observed_hugetlb_bytes": exact.then_some(hugetlb),
    })
}
