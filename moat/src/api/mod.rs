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

use pingora::{Error, ErrorType, Result, http::ResponseHeader, proxy::Session};
use poem::{Body, Endpoint, Request, Route, get, handler};

#[handler]
async fn hello() -> &'static str {
    "Hello, Moat!"
}

pub struct ApiService {
    route: Route,
}

impl ApiService {
    pub const MOAT_API_HEADER: &str = "X-Moat-Api";

    pub fn new() -> Self {
        let route = Route::new().at("/hello", get(hello));
        ApiService { route }
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
        let poem_response = self.route.get_response(poem_request).await;

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
