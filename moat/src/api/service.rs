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

use std::sync::Arc;

use pingora::{Error, ErrorType, Result, http::ResponseHeader, proxy::Session};
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
async fn health(Query(params): Query<HealthParams>, Data(meta_manager): Data<&MetaManager>) -> Response {
    tracing::debug!(?params, "check health for peer");

    if let Some(peer) = params.peer {
        if ApiClient::new(peer.clone())
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

pub struct ApiService {
    endpoint: Arc<dyn DynEndpoint<Output = Response>>,
}

impl ApiService {
    pub const MOAT_API_HEADER: &str = "X-Moat-Api";

    pub fn new(meta_manager: MetaManager) -> Self {
        let endpoint = Route::new()
            .at("/hello", get(hello))
            .at("/health", get(health))
            .at("/members", get(members))
            .at("/sync", post(sync))
            .at("/locate", post(locate))
            .data(meta_manager.clone());
        let endpoint = Arc::new(ToDynEndpoint(endpoint.into_endpoint().map_to_response()));

        ApiService { endpoint }
    }

    pub async fn handle(&self, session: &mut Session) -> Result<()> {
        tracing::debug!("Handling Moat API request");

        let header = session.req_header();

        let mut builder = Request::builder()
            .method(header.method.clone())
            .uri(header.uri.clone())
            .version(header.version);

        for (key, value) in header.headers.iter() {
            builder = builder.header(key, value);
        }

        let body = match session.read_request_body().await? {
            Some(bytes) => Body::from(bytes),
            None => Body::empty(),
        };

        let poem_request = builder.body(body);
        let poem_response = self.endpoint.get_response(poem_request).await;

        let mut header = ResponseHeader::build_no_case(poem_response.status(), None)?;
        for (key, value) in poem_response.headers().iter() {
            header.append_header(key, value)?;
        }
        header.set_version(poem_response.version());

        let body = poem_response
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| Error::because(ErrorType::InternalError, "poem service error", e))?;

        header.set_content_length(body.len())?;
        session.write_response_header(Box::new(header), true).await?;

        session.write_response_body(Some(body), true).await?;

        Ok(())
    }
}
