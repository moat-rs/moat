// Copyright 2025 Moat Project Authors
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

// TODO(MrCroxx): REMOVE ME!!!
#![expect(unused)]

use std::{fmt::Debug, iter::repeat_n, sync::Arc};

use twox_hash::xxhash64;

/// A consistent hash ring implementation that supports vnodes.
///
/// Optimized for locating performance rather than modification performance.
///
/// Note:
///
/// The space complexity is `O(sum(vnodes))`.
pub struct ConsistentHash<T> {
    state: xxhash64::State,
    mapping: Vec<Arc<T>>,
}

impl<T> Debug for ConsistentHash<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsistentHash").finish()
    }
}

impl<T> ConsistentHash<T> {
    pub fn new() -> Self {
        Self::with_seed(0)
    }

    pub fn with_seed(seed: u64) -> Self {
        ConsistentHash {
            state: xxhash64::State::with_seed(seed),
            mapping: vec![],
        }
    }
}

impl<T> ConsistentHash<T>
where
    T: Eq,
{
    /// Insert a node into the consistent hash ring with the specified number of virtual nodes (vnodes).
    ///
    /// If the node already exists, it will add the specified number of virtual nodes (vnodes) to the existing node.
    ///
    /// Return the new virtual node count for the node.
    pub fn add(&mut self, node: T, vnodes: usize) -> usize {
        let node = Arc::new(node);
        let lp = self.lpos(&node);
        let rp = self.rpos(&node);

        match (lp, rp) {
            (Some(l), Some(r)) => {
                self.mapping.splice(l..l, repeat_n(node, vnodes));
                r - l + 1 + vnodes
            }
            (None, None) => {
                self.mapping.extend(repeat_n(node, vnodes));
                vnodes
            }
            _ => unreachable!(),
        }
    }

    /// Remove a node form the consistent hash ring with the specified number of virtual nodes (vnodes).
    ///
    /// If the node already exists, it will sub the specified number of virtual nodes (vnodes) from the existing node.
    ///
    /// Return the new virtual node count for the node.
    pub fn sub(&mut self, node: &T, vnodes: usize) -> usize {
        let lp = self.lpos(node);
        let rp = self.rpos(node);

        match (lp, rp) {
            (Some(l), Some(r)) => {
                let start = l;
                let end = (start + vnodes).min(r + 1);
                self.mapping.splice(start..end, []);
                r - l + 1 - (end - start)
            }
            (None, None) => 0,
            _ => unreachable!(),
        }
    }

    /// Insert a node into the consistent hash ring with the specified number of virtual nodes (vnodes).
    ///
    /// If the node already exists, it will replace the existing node with the new one.
    pub fn insert(&mut self, node: T, vnodes: usize) {
        let lp = self.lpos(&node);
        let rp = self.rpos(&node);

        match (lp, rp) {
            (Some(l), Some(r)) => {
                self.mapping.splice(l..r + 1, repeat_n(Arc::new(node), vnodes));
            }
            (None, None) => {
                self.mapping.extend(repeat_n(Arc::new(node), vnodes));
            }
            _ => unreachable!(),
        }
    }

    /// Update a node in the consistent hash ring with the specified number of virtual nodes (vnodes).
    pub fn update(&mut self, node: T, vnodes: usize) {
        let lp = self.lpos(&node);
        let rp = self.rpos(&node);

        match (lp, rp) {
            (Some(l), Some(r)) => {
                self.mapping.splice(l..r + 1, repeat_n(Arc::new(node), vnodes));
            }
            (None, None) => {
                self.mapping.extend(repeat_n(Arc::new(node), vnodes));
            }
            _ => unreachable!(),
        }
    }

    /// Remove a node from the consistent hash ring.
    pub fn remove(&mut self, node: &T) {
        let lp = self.lpos(node);
        let rp = self.rpos(node);

        match (lp, rp) {
            (Some(l), Some(r)) => {
                self.mapping.splice(l..r + 1, []);
            }
            (None, None) => {}
            _ => unreachable!(),
        }
    }

    pub fn nodes(&self) -> usize {
        self.mapping
            .iter()
            .fold((0, None), |(nodes, last), node| {
                if Some(node) != last {
                    (nodes + 1, Some(node))
                } else {
                    (nodes, last)
                }
            })
            .0
    }

    /// Get the total vnodes.
    pub fn vnodes(&self) -> usize {
        self.mapping.len()
    }

    pub fn vnodes_by(&self, node: &T) -> usize {
        let lp = self.lpos(node);
        let rp = self.rpos(node);
        match (lp, rp) {
            (Some(l), Some(r)) => r - l + 1,
            (None, None) => 0,
            _ => unreachable!(),
        }
    }

    fn lpos(&self, node: &T) -> Option<usize> {
        self.mapping.iter().position(|x| x.as_ref() == node)
    }

    fn rpos(&self, node: &T) -> Option<usize> {
        self.mapping.iter().rposition(|x| x.as_ref() == node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consistent_hash() {
        let mut ch = ConsistentHash::new();
        let node1 = "node1";
        let node2 = "node2";
        let node3 = "node3";

        let v1 = ch.add(node1, 2);
        assert_eq!(v1, 2);
        let v2 = ch.add(node2, 3);
        assert_eq!(v2, 3);

        let v1 = ch.add(node1, 3);
        assert_eq!(v1, 5);
        let v3 = ch.add(node3, 4);
        assert_eq!(v3, 4);

        let v2 = ch.sub(&node2, 1);
        assert_eq!(v2, 2);
        let v1 = ch.sub(&node1, 100);
        assert_eq!(v1, 0);

        assert_eq!(ch.nodes(), 2);
        assert_eq!(ch.vnodes(), 6);
        assert_eq!(ch.vnodes_by(&node1), 0);
        assert_eq!(ch.vnodes_by(&node2), 2);
        assert_eq!(ch.vnodes_by(&node3), 4);

        ch.insert(node1, 3);
        ch.insert(node2, 4);
        ch.insert(node3, 5);
        assert_eq!(ch.vnodes_by(&node1), 3);
        assert_eq!(ch.vnodes_by(&node2), 4);
        assert_eq!(ch.vnodes_by(&node3), 5);

        ch.update(node1, 5);
        ch.update(node2, 4);
        ch.update(node3, 3);
        assert_eq!(ch.vnodes_by(&node1), 5);
        assert_eq!(ch.vnodes_by(&node2), 4);
        assert_eq!(ch.vnodes_by(&node3), 3);

        ch.remove(&node1);
        ch.remove(&node3);
        assert_eq!(ch.nodes(), 1);
        assert_eq!(ch.vnodes(), 4);
        assert_eq!(ch.vnodes_by(&node1), 0);
        assert_eq!(ch.vnodes_by(&node2), 4);
        assert_eq!(ch.vnodes_by(&node3), 0);
    }
}
