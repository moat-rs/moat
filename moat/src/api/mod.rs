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

use std::{net::SocketAddr, sync::Arc};

use pingora::{Error, ErrorType, Result, http::ResponseHeader, proxy::Session};
use poem::{
    Body, Endpoint, EndpointExt, IntoEndpoint, IntoResponse, Request, Response, Route,
    endpoint::{DynEndpoint, ToDynEndpoint},
    get, handler,
    middleware::AddData,
    web::{Data, Json, Query},
};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::meta::manager::MetaManager;

#[handler]
async fn hello() -> &'static str {
    "Hello, Moat!"
}

#[derive(Debug, Deserialize)]
struct HealthParams {
    peer: Option<SocketAddr>,
}

#[handler]
async fn health(Query(params): Query<HealthParams>) -> Response {
    tracing::debug!(?params, "check health for peer");

    if let Some(peer) = params.peer {
        let peer = peer.to_string();
        match Client::new()
            .get(format!("http://{peer}/health"))
            .header(ApiService::MOAT_API_HEADER, "true")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => ().into_response(),
            _ => Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(format!("Cannot reach peer {peer}"))
                .into_response(),
        }
    } else {
        ().into_response()
    }
}

#[handler]
async fn peers(Data(meta_manager): Data<&MetaManager>) -> Response {
    tracing::debug!("get peers");
    let peers = meta_manager.peers();
    Json(peers).into_response()
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
            .at("/peers", get(peers))
            .with(AddData::new(meta_manager.clone()));
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
