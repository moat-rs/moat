use std::collections::{BTreeMap, BTreeSet};

use moat::meta::model::Peer;

#[derive(Debug)]
pub struct PeerSelector {
    pub peers: BTreeSet<Peer>,
    pub selected: Option<Peer>,
}

impl PeerSelector {
    pub fn new() -> Self {
        Self {
            peers: BTreeSet::new(),
            selected: None,
        }
    }

    pub fn add(&mut self, peer: Peer) {
        self.peers.insert(peer);
    }

    pub fn remove(&mut self, peer: &Peer) {
        self.peers.remove(peer);
    }

    pub fn select(&mut self, peer: Peer) {
        self.selected = Some(peer);
    }

    pub fn deselect(&mut self) {
        self.selected = None;
    }

    pub fn min_width(&self) -> usize {
        2 + self.peers.iter().map(|peer| peer.to_string().len()).max().unwrap_or(0)
    }
}
