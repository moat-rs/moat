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

use std::{fmt::Debug, sync::Arc};

use bytes::Bytes;
use foyer::{HybridCache, HybridCacheProperties, Location};
use opendal::Operator;
use poem::{
    Body, Endpoint, EndpointExt, IntoEndpoint, IntoResponse, Request, Response, Route,
    endpoint::{DynEndpoint, ToDynEndpoint},
    get, handler, post,
    web::{Data, Json, Query},
};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::{
    api::client::ApiClient,
    config::ApiConfig,
    meta::{
        manager::MetaManager,
        model::{MemberList, Peer},
    },
    metrics::{ApiMetrics, Metrics},
};

#[handler]
async fn hello() -> &'static str {
    "Hello, Moat!"
}

#[derive(Debug, Deserialize)]
struct HealthParams {
    peer: Option<Peer>,
}

#[handler]
async fn health(
    Query(params): Query<HealthParams>,
    Data(prefix): Data<&String>,
    Data(meta_manager): Data<&MetaManager>,
) -> Response {
    tracing::debug!(?params, "check health for peer");

    if let Some(peer) = params.peer {
        if ApiClient::new(prefix.clone(), peer.clone())
            .with_timeout(meta_manager.health_check_timeout())
            .health()
            .await
        {
            ().into_response()
        } else {
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(format!("cannot reach peer {peer}"))
        }
    } else {
        ().into_response()
    }
}

#[handler]
async fn members(Data(meta_manager): Data<&MetaManager>) -> Response {
    tracing::debug!("get members req");
    let members = meta_manager.members().await;
    tracing::debug!(?members, "get members resp");

    let metrics = Metrics::global();
    let labels = ApiMetrics::labels("members", "get", "ok");
    metrics.api.count.add(1, &labels);

    Json(members).into_response()
}

#[handler]
async fn sync(Json(ms): Json<MemberList>, Data(meta_manager): Data<&MetaManager>) -> Response {
    tracing::debug!(members = ?ms, "sync members req");
    let merged = meta_manager.merge(ms).await;
    tracing::debug!(?merged, "sync members resp");
    Json(merged).into_response()
}

#[handler]
async fn locate(body: Body, Data(meta_manager): Data<&MetaManager>) -> Response {
    tracing::debug!(?body, "locate");

    let bytes = match body.into_bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            return Response::builder().status(StatusCode::BAD_REQUEST).body(e.to_string());
        }
    };

    if let Some(peer) = meta_manager.locate(bytes).await {
        Response::builder().status(StatusCode::OK).body(peer.to_string())
    } else {
        Response::builder().status(StatusCode::OK).finish()
    }
}

#[derive(Debug, Deserialize)]
struct FetchParams {
    path: String,
    location: Option<Location>,
}

#[handler]
async fn fetch(
    Query(params): Query<FetchParams>,
    Data(cache): Data<&HybridCache<Arc<String>, Bytes>>,
    Data(op): Data<&Operator>,
) -> Response {
    tracing::debug!(?params, "fetch");

    let op = op.clone();
    let cache = cache.clone();
    let location = params.location.unwrap_or_default();

    tokio::spawn(async move {
        let buf = match op.read(&params.path).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, "failed to read from S3 with fetch API");
                return;
            }
        };
        let bytes = buf.to_bytes();
        cache.insert_with_properties(
            Arc::new(params.path.to_string()),
            bytes,
            HybridCacheProperties::default().with_location(location),
        );
    });

    ().into_response()
}

struct Inner {
    endpoint: Arc<dyn DynEndpoint<Output = Response>>,
    prefix: String,
}

#[derive(Clone)]
pub struct ApiService {
    inner: Arc<Inner>,
}

impl Debug for ApiService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiService").finish()
    }
}

impl ApiService {
    pub fn new(
        config: &ApiConfig,
        meta_manager: MetaManager,
        cache: HybridCache<Arc<String>, Bytes>,
        operator: Operator,
    ) -> Self {
        let prefix = config.prefix.clone();
        let endpoint = Route::new()
            .at(format!("{prefix}/hello"), get(hello))
            .at(format!("{prefix}/health"), get(health))
            .at(format!("{prefix}/members"), get(members))
            .at(format!("{prefix}/sync"), post(sync))
            .at(format!("{prefix}/locate"), post(locate))
            .at(format!("{prefix}/fetch"), get(fetch))
            .data(meta_manager.clone())
            .data(prefix.clone())
            .data(cache)
            .data(operator);
        let endpoint = Arc::new(ToDynEndpoint(endpoint.into_endpoint().map_to_response()));

        let inner = Arc::new(Inner { prefix, endpoint });
        Self { inner }
    }

    pub async fn invoke(&self, request: Request) -> Response {
        tracing::debug!("Invoking Moat API service");
        self.inner.endpoint.get_response(request).await
    }

    pub fn prefix(&self) -> &str {
        &self.inner.prefix
    }
}
