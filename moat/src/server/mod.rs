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

use std::{net::SocketAddr, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use bytesize::ByteSize;
use clap::{Args, Parser};
use foyer::{
    DirectFsDeviceOptions, Engine, HybridCache, LargeEngineOptions, RecoverMode, RuntimeOptions, Throttle,
    TokioRuntimeOptions,
};
use pingora::{prelude::*, proxy::http_proxy_service_with_name, server::configuration::ServerConf};
use serde::{Deserialize, Serialize};

use crate::{
    api::service::ApiService,
    aws::{
        resigner::{AwsSigV4Resigner, AwsSigV4ResignerConfig},
        s3::S3Config,
    },
    meta::{
        manager::{Gossip, MetaManager, MetaManagerConfig},
        model::{Identity, Peer},
    },
    runtime::Runtime,
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

#[derive(Debug, Args, Serialize, Deserialize)]
pub struct CacheConfig {
    #[clap(long, default_value = "64MiB")]
    mem: ByteSize,

    #[clap(long)]
    dir: Option<String>,

    #[clap(long, default_value = "1GiB", requires = "dir")]
    disk: ByteSize,

    #[clap(long, default_value = "64MiB")]
    file_size: ByteSize,

    #[clap(flatten)]
    throttle: Throttle,

    #[clap(long, default_value_t = 4)]
    flushers: usize,

    #[clap(long, default_value_t = 2)]
    reclaimers: usize,

    #[clap(long, default_value = "16MiB")]
    buffer_pool_size: ByteSize,

    #[clap(long, default_value = "quiet")]
    recover_mode: RecoverMode,

    #[clap(long, default_value_t = 4)]
    recover_concurrency: usize,
}

#[derive(Debug, Parser, Serialize, Deserialize)]
pub struct MoatConfig {
    #[clap(long, default_value = "127.0.0.1:23456")]
    listen: SocketAddr,
    #[clap(long, default_value = "provider")]
    identity: Identity,
    #[clap(long)]
    peer: Peer,
    #[clap(long, num_args = 1.., value_delimiter = ',')]
    bootstrap_peers: Vec<Peer>,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "10s")]
    provider_eviction_timeout: Duration,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "3s")]
    health_check_timeout: Duration,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "1s")]
    health_check_interval: Duration,
    #[clap(long, default_value_t = 3)]
    health_check_peers: usize,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "3s")]
    sync_timeout: Duration,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "1s")]
    sync_interval: Duration,
    #[clap(long, default_value_t = 3)]
    sync_peers: usize,
    #[clap(long, default_value_t = 1)]
    weight: usize,

    #[clap(flatten)]
    s3_config: S3Config,

    #[clap(flatten)]
    cache: CacheConfig,
}

pub struct Moat;

impl Moat {
    pub fn run(config: MoatConfig) -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");
        let runtime = Runtime::new(runtime);

        let cache = {
            let config = config.cache;
            runtime.block_on(async move {
                let mut builder = HybridCache::builder()
                    // TODO(MrCroxx): introduce metrics system.
                    // .with_metrics_registry(registry)
                    .memory(config.mem.as_u64() as _)
                    // TODO(MrCroxx): Count serialized size?
                    .with_weighter(|path: &String, data: &Bytes| path.len() + data.len())
                    .storage(Engine::Large(
                        LargeEngineOptions::new()
                            .with_flushers(config.flushers)
                            .with_reclaimers(config.reclaimers)
                            .with_buffer_pool_size(config.buffer_pool_size.as_u64() as _)
                            .with_recover_concurrency(config.recover_concurrency),
                    ))
                    .with_recover_mode(config.recover_mode)
                    .with_runtime_options(RuntimeOptions::Unified(TokioRuntimeOptions::default()));
                if let Some(dir) = config.dir {
                    builder = builder.with_device_options(
                        DirectFsDeviceOptions::new(dir)
                            .with_capacity(config.disk.as_u64() as _)
                            .with_file_size(config.file_size.as_u64() as _)
                            .with_throttle(config.throttle),
                    );
                }
                builder.build().await
            })
        }?;

        let meta_manager = MetaManager::new(MetaManagerConfig {
            identity: config.identity,
            peer: config.peer.clone(),
            bootstrap_peers: config.bootstrap_peers.clone(),
            provider_eviction_timeout: config.provider_eviction_timeout,
            health_check_timeout: config.health_check_timeout,
            health_check_interval: config.health_check_interval,
            health_check_peers: config.health_check_peers,
            sync_timeout: config.sync_timeout,
            sync_interval: config.sync_interval,
            sync_peers: config.sync_peers,
            weight: config.weight,
        });
        let gossip = Gossip::new(runtime.clone(), meta_manager.clone());
        runtime.spawn(async move { gossip.run().await });

        let api = ApiService::new(meta_manager);
        let resigner = AwsSigV4Resigner::new(AwsSigV4ResignerConfig {
            endpoint: config.s3_config.endpoint.clone(),
            region: config.s3_config.region.clone(),
            access_key_id: config.s3_config.access_key_id.clone(),
            secret_access_key: config.s3_config.secret_access_key.clone(),
        });

        let mut conf = ServerConf::new().unwrap();
        conf.grace_period_seconds = Some(0);

        let mut server = Server::new_with_opt_and_conf(None, conf);
        server.bootstrap();

        let s3_host = config.s3_config.endpoint.host_str().unwrap_or("localhost").to_string();
        let s3_port = config
            .s3_config
            .endpoint
            .port()
            .unwrap_or(if config.s3_config.endpoint.scheme() == "https" {
                443
            } else {
                80
            });
        let s3_tls = config.s3_config.endpoint.scheme() == "https";

        let proxy = Proxy {
            api,
            resigner,
            s3_host,
            s3_port,
            s3_tls,
            _cache: cache,
        };
        let mut service = http_proxy_service_with_name(&server.configuration, proxy, "moat");
        service.add_tcp(&config.listen.to_string());
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

    s3_host: String,
    s3_port: u16,
    s3_tls: bool,

    _cache: HybridCache<String, Bytes>,
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
        let s3_upstream_peer = Box::new(HttpPeer::new(
            format!("{}:{}", self.s3_host, self.s3_port),
            self.s3_tls,
            self.s3_host.clone(),
        ));
        Ok(s3_upstream_peer.clone())
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
