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

use async_trait::async_trait;
use pingora::{prelude::*, proxy::http_proxy_service_with_name, server::configuration::ServerConf};
use url::Url;

use crate::{
    api::ApiService,
    aws::{AwsSigV4Resigner, AwsSigV4ResignerConfig},
};

#[derive(Debug)]
enum MoatRequest {
    MoatApi,
    S3GetObject { bucket: String, key: String },
    S3Other,
}

impl MoatRequest {
    fn parse(request: &RequestHeader) -> Self {
        if request.headers.get(ApiService::MOAT_API_HEADER).is_some() {
            return MoatRequest::MoatApi;
        }

        let path = request.uri.path();
        let method = request.method.as_str();

        // S3 GetObject schema: GET /{bucket}/{key}
        if method == "GET" && path.len() > 1 {
            let parts: Vec<&str> = path[1..].splitn(2, '/').collect();
            if parts.len() == 2 {
                return MoatRequest::S3GetObject {
                    bucket: parts[0].to_string(),
                    key: parts[1].to_string(),
                };
            }
        }

        MoatRequest::S3Other
    }
}

#[derive(Debug, Clone)]
pub struct MoatConfig {
    pub endpoint: String,
    pub s3_endpoint: String,
    pub s3_access_key_id: String,
    pub s3_secret_access_key: String,
    pub s3_region: String,
}

pub struct Moat;

impl Moat {
    pub fn run(config: MoatConfig) {
        // let _runtime = tokio::runtime::Builder::new_multi_thread()
        //     .enable_all()
        //     .build()
        //     .expect("Failed to create Tokio runtime");

        let api = ApiService::new();
        let resigner = AwsSigV4Resigner::new(AwsSigV4ResignerConfig {
            endpoint: Url::parse(&config.s3_endpoint).expect("Invalid endpoint URL"),
            region: config.s3_region.clone(),
            access_key_id: config.s3_access_key_id.clone(),
            secret_access_key: config.s3_secret_access_key.clone(),
        });

        let mut conf = ServerConf::new().unwrap();
        conf.grace_period_seconds = Some(0);

        let mut server = Server::new_with_opt_and_conf(None, conf);
        server.bootstrap();

        let proxy = Proxy {
            api,
            resigner,
            config: config.clone(),
        };
        let mut service = http_proxy_service_with_name(&server.configuration, proxy, "moat");
        service.add_tcp(&config.endpoint);
        server.add_service(service);
        server.run_forever();
    }
}

#[derive(Debug)]
pub struct ProxyCtx;

impl Default for ProxyCtx {
    fn default() -> Self {
        Self
    }
}

struct Proxy {
    api: ApiService,

    resigner: AwsSigV4Resigner,

    config: MoatConfig,
}

impl Proxy {
    async fn handle_get_object(&self, bucket: &str, key: &str, _: &mut Session) -> Result<bool> {
        tracing::debug!(bucket, key, "Handling S3 GetObject request");

        Ok(true)
    }
}

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        ProxyCtx
    }

    async fn upstream_peer(&self, session: &mut Session, _: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        tracing::debug!(header = ?session.req_header(), "looking up upstream peer");

        let url = Url::parse(&self.config.s3_endpoint)
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("{e}")))?;

        let host = url
            .host_str()
            .ok_or("Invalid host in endpoint URL")
            .map_err(|e| Error::explain(ErrorType::InternalError, e.to_string()))?;
        let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
        let use_tls = url.scheme() == "https";

        let peer = Box::new(HttpPeer::new(format!("{}:{}", host, port), use_tls, host.to_string()));
        Ok(peer)
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        let request = session.req_header();
        let request = MoatRequest::parse(request);
        match request {
            MoatRequest::MoatApi => {
                tracing::debug!("Handling Moat API request");
                self.api.handle(session).await?;
                return Ok(true);
            }
            MoatRequest::S3GetObject { bucket, key } => match self.handle_get_object(&bucket, &key, session).await {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(e) => tracing::error!(?e, "Error handling GetObject"),
            },
            MoatRequest::S3Other => {}
        }

        Ok(true)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        tracing::debug!(?upstream_request, "Upstream request before filtering");

        self.resigner.resign(upstream_request);

        tracing::debug!(?upstream_request, "Upstream request after filtering");

        Ok(())
    }
}
