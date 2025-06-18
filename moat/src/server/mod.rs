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
use bytes::Bytes;

use foyer::{DirectFsDeviceOptions, Engine, HybridCache, LargeEngineOptions, RuntimeOptions, TokioRuntimeOptions};
use http::{StatusCode, Version, header::CONTENT_TYPE};
use mixtrics::registry::opentelemetry_0_30::OpenTelemetryMetricsRegistry;
use opendal::{Operator, layers::LoggingLayer, services::S3};
use pingora::{
    http::ResponseHeader, prelude::*, proxy::http_proxy_service_with_name, server::configuration::ServerConf,
};

use crate::{
    api::service::ApiService,
    aws::resigner::{AwsSigV4Resigner, AwsSigV4ResignerConfig},
    config::MoatConfig,
    meta::{
        manager::{Gossip, MetaManager, MetaManagerConfig, Observer},
        model::{Peer, Role},
    },
    runtime::Runtime,
};

#[derive(Debug)]
enum MoatRequest {
    MoatApi,
    S3GetObject { bucket: String, path: String },
    // TODO(MrCroxx): cache insertion
    // S3PutObject {}
    S3Other,
}

impl MoatRequest {
    fn parse(request: &RequestHeader) -> Self {
        if request.headers.get(ApiService::MOAT_API_HEADER).is_some() {
            return MoatRequest::MoatApi;
        }

        let path = request.uri.path();
        let method = request.method.as_str();

        // S3 GetObject schema: GET /{bucket}/{path}
        if method == "GET" && path.len() > 1 {
            let parts: Vec<&str> = path[1..].splitn(2, '/').collect();
            if parts.len() == 2 {
                return MoatRequest::S3GetObject {
                    bucket: parts[0].to_string(),
                    path: parts[1].to_string(),
                };
            }
        }

        MoatRequest::S3Other
    }
}

pub struct Moat;

impl Moat {
    pub fn run(config: MoatConfig, runtime: Runtime) -> anyhow::Result<()> {
        let cache = {
            let config = &config;
            runtime.block_on(async move {
                let registry = Box::new(OpenTelemetryMetricsRegistry::new(opentelemetry::global::meter("foyer")));
                let mut builder = HybridCache::builder()
                    .with_name(config.peer.to_string())
                    .with_metrics_registry(registry)
                    .memory(config.cache.mem.as_u64() as _)
                    // TODO(MrCroxx): Count serialized size?
                    .with_weighter(|path: &String, data: &Bytes| path.len() + data.len())
                    .storage(Engine::Large(
                        LargeEngineOptions::new()
                            .with_flushers(config.cache.flushers)
                            .with_reclaimers(config.cache.reclaimers)
                            .with_buffer_pool_size(config.cache.buffer_pool_size.as_u64() as _)
                            .with_recover_concurrency(config.cache.recover_concurrency),
                    ))
                    .with_recover_mode(config.cache.recover_mode)
                    .with_runtime_options(RuntimeOptions::Unified(TokioRuntimeOptions::default()));
                if let Some(dir) = &config.cache.dir {
                    builder = builder.with_device_options(
                        DirectFsDeviceOptions::new(dir)
                            .with_capacity(config.cache.disk.as_u64() as _)
                            .with_file_size(config.cache.file_size.as_u64() as _)
                            .with_throttle(config.cache.throttle.clone()),
                    );
                }
                builder.build().await
            })
        }?;

        let operator = Operator::new(
            S3::default()
                .endpoint(config.s3_config.endpoint.as_str())
                .region(&config.s3_config.region)
                .bucket(&config.s3_config.bucket)
                .access_key_id(&config.s3_config.access_key_id)
                .secret_access_key(&config.s3_config.secret_access_key)
                .disable_config_load()
                .disable_ec2_metadata(),
        )?
        .layer(LoggingLayer::default())
        .finish();

        let meta_manager = MetaManager::new(MetaManagerConfig {
            role: config.role,
            peer: config.peer.clone(),
            bootstrap_peers: config.bootstrap_peers.clone(),
            peer_eviction_timeout: config.peer_eviction_timeout,
            health_check_timeout: config.health_check_timeout,
            health_check_interval: config.health_check_interval,
            health_check_peers: config.health_check_peers,
            sync_timeout: config.sync_timeout,
            sync_interval: config.sync_interval,
            sync_peers: config.sync_peers,
            weight: config.weight,
        });
        match config.role {
            Role::Agent => {
                let observer = Observer::new(runtime.clone(), meta_manager.clone());
                runtime.spawn(async move { observer.run().await });
            }
            Role::Cache => {
                let gossip = Gossip::new(runtime.clone(), meta_manager.clone());
                runtime.spawn(async move { gossip.run().await });
            }
        }

        let api = ApiService::new(meta_manager.clone());
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
        let s3_bucket = config.s3_config.bucket.clone();

        let proxy = Proxy {
            api,
            resigner,
            s3_host,
            s3_port,
            s3_tls,
            s3_bucket,
            role: config.role,
            cache,
            operator,
            meta_manager,
            peer: config.peer.clone(),
            tls: config.tls,
        };
        let mut service = http_proxy_service_with_name(&server.configuration, proxy, "moat");
        service.add_tcp(&config.listen.to_string());
        server.add_service(service);
        server.run_forever();
    }
}

#[derive(Debug, Default)]
pub enum UpstreamPeer {
    #[default]
    None,
    S3,
    Peer(Peer),
}

#[derive(Debug, Default)]
pub struct ProxyCtx {
    upstream_peer: UpstreamPeer,
}

struct Proxy {
    api: ApiService,

    resigner: AwsSigV4Resigner,

    s3_host: String,
    s3_port: u16,
    s3_tls: bool,
    s3_bucket: String,

    role: Role,
    cache: HybridCache<String, Bytes>,
    operator: Operator,
    meta_manager: MetaManager,

    peer: Peer,
    tls: bool,
}

impl Proxy {
    const MOAT_PEER_HEADER: &str = "X-Moat-Peer";

    async fn handle_get_object(
        &self,
        bucket: &str,
        path: &str,
        session: &mut Session,
        ctx: &mut ProxyCtx,
    ) -> Result<bool> {
        tracing::debug!(bucket, path, "Handling S3 GetObject request");

        // TODO(MrCroxx): Agent check cache first, and use fetch for cache refilling.

        // Find the suitable peer for the request.
        let peer = match self.meta_manager.locate(path).await {
            Some(p) => p,
            None => {
                tracing::warn!(path, "No available peer found, attempting to fetch from S3 directly");
                let bytes = self.s3_get_object_directly(path).await?;
                self.write_get_object_response(session, bytes).await?;
                return Ok(true);
            }
        };

        if peer != self.peer {
            tracing::debug!(bucket, path, ?peer, "Found another peer for S3 GetObject request");
            ctx.upstream_peer = UpstreamPeer::Peer(peer);
            return Ok(false);
        }

        let bytes = match self.role {
            Role::Agent => {
                tracing::warn!(
                    path,
                    "Agent cannot find available cache peer to serve the request, attempting to fetch from S3 directly"
                );
                self.s3_get_object_directly(path).await?
            }
            Role::Cache => match self
                .cache
                .fetch(path.to_string(), || {
                    let op = self.operator.clone();
                    let path = path.to_string();
                    async move {
                        let res = op.read(&path).await;
                        res.map(|buf| buf.to_bytes()).map_err(anyhow::Error::from)
                    }
                })
                .await
                .map_err(|e| Error::because(ErrorType::InternalError, "cache get object error", e))
            {
                Ok(entry) => {
                    tracing::debug!(path, "Fetched object from cache");
                    entry.value().clone()
                }
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        path,
                        "Failed to fetch object from cache, attempting to fetch from S3 directly"
                    );
                    self.s3_get_object_directly(path).await?
                }
            },
        };

        self.write_get_object_response(session, bytes).await?;
        Ok(true)
    }

    async fn write_get_object_response(&self, session: &mut Session, bytes: Bytes) -> Result<()> {
        let mut header = ResponseHeader::build_no_case(StatusCode::OK, None)?;

        header.append_header(CONTENT_TYPE, "application/octet-stream")?;
        header.set_version(Version::HTTP_11);
        header.set_content_length(bytes.len())?;

        session.write_response_header(Box::new(header), true).await?;
        session.write_response_body(Some(bytes), true).await?;

        Ok(())
    }

    async fn s3_get_object_directly(&self, path: &str) -> Result<Bytes> {
        self.operator
            .read(path)
            .await
            .map(|buf| buf.to_bytes())
            .map_err(|e| Error::because(ErrorType::InternalError, "s3 get object error", e))
    }
}

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        ProxyCtx::default()
    }

    async fn upstream_peer(&self, _: &mut Session, ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        tracing::debug!(?ctx, "looking up upstream peer");
        let upstream_peer = match &ctx.upstream_peer {
            UpstreamPeer::None => {
                return Err(Error::explain(
                    ErrorType::ConnectProxyFailure,
                    "no upstream peer to route",
                ));
            }
            UpstreamPeer::S3 => Box::new(HttpPeer::new(
                format!("{}:{}", self.s3_host, self.s3_port),
                self.s3_tls,
                self.s3_host.clone(),
            )),
            UpstreamPeer::Peer(peer) => Box::new(HttpPeer::new(peer.to_string(), self.tls, peer.host.clone())),
        };
        Ok(upstream_peer)
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        let header = session.req_header();
        let request = MoatRequest::parse(header);

        tracing::trace!(?header, ?request, "Receive request");

        match request {
            MoatRequest::MoatApi => {
                tracing::debug!("Handling Moat API request");
                self.api.handle(session).await?;
                return Ok(true);
            }
            MoatRequest::S3GetObject { bucket, path } if bucket == self.s3_bucket => {
                return self.handle_get_object(&bucket, &path, session, ctx).await;
            }
            MoatRequest::S3GetObject { .. } | MoatRequest::S3Other => {
                ctx.upstream_peer = UpstreamPeer::S3;
            }
        }

        Ok(false)
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
        tracing::trace!(?upstream_request, "Upstream request before filtering");

        self.resigner.resign(upstream_request);

        tracing::trace!(?upstream_request, "Upstream request after filtering");

        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        match self.role {
            Role::Agent => {
                if let Some(value) = upstream_response.headers.get(Self::MOAT_PEER_HEADER) {
                    tracing::warn!(
                        ?value,
                        "Receive peer redirect hint from cache peer, update memberlist at once."
                    );
                    // TODO(MrCroxx): Observe in another task.
                    self.meta_manager.observe().await;
                    upstream_response.remove_header(Self::MOAT_PEER_HEADER);
                }
            }
            Role::Cache => {
                if let UpstreamPeer::Peer(peer) = &ctx.upstream_peer {
                    tracing::trace!(?peer, "Inserting peer header into response header");
                    upstream_response.insert_header(Self::MOAT_PEER_HEADER, peer.to_string())?;
                }
            }
        }

        Ok(())
    }
}
