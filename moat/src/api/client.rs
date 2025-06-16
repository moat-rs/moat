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

use std::time::Duration;

use reqwest::Client;

use crate::{
    api::service::ApiService,
    meta::model::{MemberList, Peer},
};

#[derive(Debug, Clone)]
pub struct ApiClient {
    peer: Peer,
    timeout: Option<Duration>,
}

impl ApiClient {
    pub fn new(peer: Peer) -> Self {
        ApiClient { peer, timeout: None }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    fn client(&self) -> Client {
        Client::builder()
            .connection_verbose(cfg!(debug_assertions))
            .timeout(self.timeout.unwrap_or(Duration::ZERO))
            .build()
            .unwrap()
    }

    pub async fn health(&self) -> bool {
        matches! {
            self
            .client()
            .get(format!("http://{peer}/health", peer = self.peer))
            .header(ApiService::MOAT_API_HEADER, "true")
            .send()
            .await,
            Ok(r) if r.status().is_success()
        }
    }

    pub async fn sync(&self, members: MemberList) -> Option<MemberList> {
        let response = match self
            .client()
            .post(format!("http://{peer}/sync", peer = self.peer))
            .header(ApiService::MOAT_API_HEADER, "true")
            .json(&members)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::error!(peer = ?self.peer, status = ?r.status(), "Failed to sync with peer");
                return None;
            }
            Err(e) => {
                tracing::error!(peer = ?self.peer, ?e, "Failed to sync with peer");
                return None;
            }
        };

        match response.json::<MemberList>().await {
            Ok(ms) => Some(ms),
            Err(e) => {
                tracing::error!(peer = ?self.peer, ?e, "Failed to parse synced memberlist");
                None
            }
        }
    }

    pub async fn members(&self) -> Option<MemberList> {
        let response = match self
            .client()
            .get(format!("http://{peer}/members", peer = self.peer))
            .header(ApiService::MOAT_API_HEADER, "true")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::error!(peer = ?self.peer, status = ?r.status(), "Failed to get members from peer");
                return None;
            }
            Err(e) => {
                tracing::error!(peer = ?self.peer, ?e, "Failed to get members from peer");
                return None;
            }
        };

        match response.json::<MemberList>().await {
            Ok(ms) => Some(ms),
            Err(e) => {
                tracing::error!(peer = ?self.peer, ?e, "Failed to parse memberlist");
                None
            }
        }
    }
}
