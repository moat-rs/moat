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
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use crate::meta::model::Identity;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaManagerConfig {
    pub identity: Identity,
    pub listen: SocketAddr,
    pub bootstrap_peers: Vec<SocketAddr>,
}

#[derive(Debug)]
struct Inner {
    peers: Vec<SocketAddr>,
}

#[derive(Debug, Clone)]
pub struct MetaManager {
    inner: Arc<Mutex<Inner>>,
}

impl MetaManager {
    pub fn new(config: MetaManagerConfig) -> Self {
        let inner = Inner {
            peers: config.bootstrap_peers,
        };
        let inner = Arc::new(Mutex::new(inner));
        Self { inner }
    }

    pub fn peers(&self) -> Vec<SocketAddr> {
        let inner = self.inner.lock().unwrap();
        inner.peers.clone()
    }
}
