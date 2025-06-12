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

use std::{
    fmt::Debug,
    hash::{BuildHasher, Hash},
    iter::repeat_n,
    sync::Arc,
};

use twox_hash::xxhash64;

/// A consistent hash ring implementation that supports vnodes.
///
/// Optimized for locating performance rather than modification performance.
///
/// Note:
///
/// The space complexity is `O(sum(vnodes))`.
pub struct ConsistentHash<T, S = xxhash64::State> {
    state: S,
    mapping: Vec<Arc<T>>,
}

impl<T, S> Debug for ConsistentHash<T, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsistentHash").finish()
    }
}

impl<T> Default for ConsistentHash<T> {
    fn default() -> Self {
        Self::new(xxhash64::State::with_seed(0))
    }
}

impl<T, S> ConsistentHash<T, S> {
    pub fn new(state: S) -> Self {
        Self { state, mapping: vec![] }
    }
}

impl<T, I> From<I> for ConsistentHash<T>
where
    T: Eq,
    I: IntoIterator<Item = (T, usize)>,
{
    fn from(into_iter: I) -> Self {
        let mut this = Self::default();
        into_iter.into_iter().for_each(|(node, vnode)| {
            // FIXME(MrCroxx): `Eq` for `Arc<T>` compares pointer addresses. The behavior with duplicated nodes may not
            // be expected.
            this.add(node, vnode);
        });
        this
    }
}

impl<T, S> ConsistentHash<T, S>
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
    #[cfg_attr(not(test), expect(unused))]
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
    #[cfg_attr(not(test), expect(unused))]
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
    #[cfg_attr(not(test), expect(unused))]
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
    #[cfg_attr(not(test), expect(unused))]
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

    /// Get the total nodes.
    #[cfg_attr(not(test), expect(unused))]
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
    #[cfg_attr(not(test), expect(unused))]
    pub fn vnodes(&self) -> usize {
        self.mapping.len()
    }

    /// Get the number of vnodes for the given node.
    #[cfg_attr(not(test), expect(unused))]
    pub fn vnodes_by(&self, node: &T) -> usize {
        let lp = self.lpos(node);
        let rp = self.rpos(node);
        match (lp, rp) {
            (Some(l), Some(r)) => r - l + 1,
            (None, None) => 0,
            _ => unreachable!(),
        }
    }

    /// Locate the item in the consistent hash ring.
    ///
    /// Return `None` if the hash ring is empty.
    pub fn locate<H>(&self, item: H) -> Option<&T>
    where
        S: BuildHasher,
        H: Hash,
    {
        if self.mapping.is_empty() {
            return None;
        }
        let hash = self.state.hash_one(item);
        let step = u64::MAX / self.mapping.len() as u64;
        let index = (hash / step).min(self.mapping.len() as u64 - 1) as usize;
        Some(&self.mapping[index])
    }

    fn lpos(&self, node: &T) -> Option<usize> {
        self.mapping.iter().position(|x| x.as_ref() == node)
    }

    fn rpos(&self, node: &T) -> Option<usize> {
        self.mapping.iter().rposition(|x| x.as_ref() == node)
    }
}

#[cfg(test)]
pub mod tests {
    use std::hash::Hasher;

    use super::*;

    /// A hasher return u64 mod result.
    #[derive(Debug, Default)]
    pub struct ModState {
        state: u64,
    }

    impl Hasher for ModState {
        fn finish(&self) -> u64 {
            self.state
        }

        fn write(&mut self, bytes: &[u8]) {
            for byte in bytes {
                self.state = (self.state << 8) + *byte as u64;
            }
        }

        fn write_u8(&mut self, i: u8) {
            self.write(&[i])
        }

        fn write_u16(&mut self, i: u16) {
            self.write(&i.to_be_bytes())
        }

        fn write_u32(&mut self, i: u32) {
            self.write(&i.to_be_bytes())
        }

        fn write_u64(&mut self, i: u64) {
            self.write(&i.to_be_bytes())
        }

        fn write_u128(&mut self, i: u128) {
            self.write(&i.to_be_bytes())
        }

        fn write_usize(&mut self, i: usize) {
            self.write(&i.to_be_bytes())
        }

        fn write_i8(&mut self, i: i8) {
            self.write_u8(i as u8)
        }

        fn write_i16(&mut self, i: i16) {
            self.write_u16(i as u16)
        }

        fn write_i32(&mut self, i: i32) {
            self.write_u32(i as u32)
        }

        fn write_i64(&mut self, i: i64) {
            self.write_u64(i as u64)
        }

        fn write_i128(&mut self, i: i128) {
            self.write_u128(i as u128)
        }

        fn write_isize(&mut self, i: isize) {
            self.write_usize(i as usize)
        }
    }

    impl BuildHasher for ModState {
        type Hasher = Self;

        fn build_hasher(&self) -> Self::Hasher {
            Self::default()
        }
    }

    #[test]
    fn test_consistent_hash_modification() {
        let mut ch = ConsistentHash::default();
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

    #[test]
    fn test_consistent_hash_locate() {
        let mut ch = ConsistentHash::new(ModState::default());

        assert_eq!(ch.locate(0), None);

        let node1 = "node1";
        let node2 = "node2";
        let node3 = "node3";

        ch.insert(node1, 1);
        ch.insert(node2, 2);
        ch.insert(node3, 3);

        let step = u64::MAX / ch.vnodes() as u64;

        assert_eq!(ch.locate(0), Some(&node1));
        assert_eq!(ch.locate(step - 1), Some(&node1));
        assert_eq!(ch.locate(step), Some(&node2));
        assert_eq!(ch.locate(step * 3 - 1), Some(&node2));
        assert_eq!(ch.locate(step * 3), Some(&node3));
        assert_eq!(ch.locate(step * 6 - 1), Some(&node3));
        assert_eq!(ch.locate(step * 6), Some(&node3));
        assert_eq!(ch.locate(u64::MAX), Some(&node3));
    }
}
