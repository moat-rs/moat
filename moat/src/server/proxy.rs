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
    borrow::Cow,
    ops::Bound,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use headers::{Header, Range as HeaderRange};

use async_trait::async_trait;
use bytes::Bytes;
use itertools::Itertools;
use poem::Request;

use crate::error::{Error, Result};
use foyer::HybridCache;
use http::{StatusCode, Version, header::CONTENT_TYPE};
use opendal::Operator;
use pingora::{
    http::{RequestHeader, ResponseHeader},
    prelude::{Error as PingoraError, HttpPeer, ProxyHttp, Result as PingoraResult, Session},
};

use crate::{
    api::service::ApiService,
    aws::resigner::AwsSigV4Resigner,
    meta::{
        manager::MetaManager,
        model::{Peer, Role},
    },
    metrics::{Metrics, S3Metrics},
};

#[derive(Debug)]
struct S3GetObjectArgs {
    bucket: String,
    path: Arc<String>,
    range: Option<HeaderRange>,
}

#[derive(Debug)]
enum Category {
    Uncategoried,
    MoatApi,
    S3GetObject(S3GetObjectArgs),
    // TODO(MrCroxx): cache insertion
    // S3PutObject {}
    S3Other,
}

impl Category {
    fn parse(api_prefix: &str, request: &RequestHeader) -> Result<Self> {
        let path = request.uri.path();
        let method = request.method.as_str();

        if path == api_prefix || path.starts_with(&format!("{}/", api_prefix)) {
            return Ok(Category::MoatApi);
        }

        // S3 GetObject schema: GET /{bucket}/{path}
        if method == "GET" && path.len() > 1 {
            if let Some(query) = request.uri.query() {
                let pairs = url::form_urlencoded::parse(query.as_bytes()).collect_vec();
                if pairs.iter().any(|(k, _)| k == "partNumber") {
                    tracing::debug!(
                        uri = ?request.uri,
                        "Cannot handle GetObject with partNumber parameter, skip."
                    );
                    return Ok(Category::S3Other);
                }
            }

            let range = match request.headers.get("range") {
                Some(v) => Some(HeaderRange::decode(&mut std::iter::once(v))?),
                None => None,
            };
            let parts: Vec<&str> = path[1..].splitn(2, '/').collect();
            if parts.len() == 2 {
                return Ok(Category::S3GetObject(S3GetObjectArgs {
                    bucket: parts[0].to_string(),
                    path: Arc::new(parts[1].to_string()),
                    range,
                }));
            }
        }

        Ok(Category::S3Other)
    }
}

#[derive(Debug)]
pub enum UpstreamPeer {
    None,
    S3,
    Peer(Peer),
}

#[derive(Debug)]
pub struct ProxyCtx {
    upstream_peer: UpstreamPeer,
    start: Instant,
    category: Category,
    extra: Cow<'static, str>,
}

pub struct ProxyConfig {
    pub api: ApiService,

    pub resigner: AwsSigV4Resigner,

    pub s3_host: String,
    pub s3_port: u16,
    pub s3_tls: bool,
    pub s3_bucket: String,

    pub role: Role,
    pub cache: HybridCache<Arc<String>, Bytes>,
    pub operator: Operator,
    pub meta_manager: MetaManager,

    pub peer: Peer,
    pub tls: bool,
}

pub struct Proxy {
    api: ApiService,

    resigner: AwsSigV4Resigner,

    s3_host: String,
    s3_port: u16,
    s3_tls: bool,
    s3_bucket: String,

    role: Role,
    cache: HybridCache<Arc<String>, Bytes>,
    operator: Operator,
    meta_manager: MetaManager,

    peer: Peer,
    tls: bool,
}

impl Proxy {
    const MOAT_PEER_HEADER: &str = "X-Moat-Peer";

    pub fn new(config: ProxyConfig) -> Self {
        let ProxyConfig {
            api,
            resigner,
            s3_host,
            s3_port,
            s3_tls,
            s3_bucket,
            role,
            cache,
            operator,
            meta_manager,
            peer,
            tls,
        } = config;

        tracing::info!(?peer, "Proxy initialized with peer");

        Self {
            api,
            resigner,
            s3_host,
            s3_port,
            s3_tls,
            s3_bucket,
            role,
            cache,
            operator,
            meta_manager,
            peer,
            tls,
        }
    }

    async fn handle_get_object(&self, session: &mut Session, ctx: &mut ProxyCtx) -> PingoraResult<bool> {
        let args = match &ctx.category {
            Category::S3GetObject(args) => args,
            _ => unreachable!(),
        };

        tracing::debug!(?args, "Handling S3 GetObject request");

        // TODO(MrCroxx): Agent check cache first, and use fetch for cache refilling.

        // Find the suitable peer for the request.
        let peer = match self.meta_manager.locate(&args.path).await {
            Some(p) => p,
            None => {
                tracing::warn!(?args, "No available peer found, attempting to fetch from S3 directly");
                let bytes = self.s3_get_object_directly(&args.path).await?;
                ctx.extra = S3Metrics::EXTRA_S3.into();
                self.write_get_object_response(session, bytes).await?;
                return Ok(true);
            }
        };

        if peer != self.peer {
            tracing::debug!(?args, ?peer, "Found another peer for S3 GetObject request");
            ctx.upstream_peer = UpstreamPeer::Peer(peer);
            ctx.extra = S3Metrics::EXTRA_PROXIED.into();
            return Ok(false);
        }

        let mut bytes = match self.role {
            Role::Agent => {
                tracing::warn!(
                    ?args,
                    "Agent cannot find available cache peer to serve the request, attempting to fetch from S3 directly"
                );
                ctx.extra = S3Metrics::EXTRA_S3.into();
                self.s3_get_object_directly(&args.path).await?
            }
            Role::Cache => {
                let fetched = Arc::new(AtomicBool::new(false));
                let path = args.path.clone();
                match self
                    .cache
                    .fetch(path.clone(), || {
                        let op = self.operator.clone();
                        let path = args.path.clone();
                        let fetched = fetched.clone();
                        async move {
                            fetched.store(true, Ordering::Relaxed);
                            let res = op.read(&path).await;
                            res.map(|buf| buf.to_bytes()).map_err(foyer::Error::other)
                        }
                    })
                    .await
                {
                    Ok(entry) => {
                        tracing::debug!(?path, "Fetched object from cache");
                        if fetched.load(Ordering::Relaxed) {
                            ctx.extra = S3Metrics::EXTRA_FETCHED.into();
                        } else {
                            ctx.extra = S3Metrics::EXTRA_CACHED.into();
                        }
                        entry.value().clone()
                    }
                    Err(e) => {
                        tracing::warn!(
                            ?e,
                            ?path,
                            "Failed to fetch object from cache, attempting to fetch from S3 directly"
                        );
                        ctx.extra = S3Metrics::EXTRA_S3.into();
                        self.s3_get_object_directly(&path).await?
                    }
                }
            }
        };

        if let Some(r) = &args.range {
            let r = r.satisfiable_ranges(bytes.len() as _).next();
            bytes = match r {
                Some((start, end)) => {
                    let s = match start {
                        Bound::Included(v) => v as usize,
                        Bound::Excluded(v) => v as usize + 1,
                        Bound::Unbounded => 0,
                    };
                    let t = match end {
                        Bound::Included(v) => v as usize + 1,
                        Bound::Excluded(v) => v as usize,
                        Bound::Unbounded => bytes.len(),
                    };
                    bytes.slice(s..t)
                }
                None => Bytes::new(),
            }
        }

        self.write_get_object_response(session, bytes).await?;
        Ok(true)
    }

    async fn handle_moat_api(&self, session: &mut Session, _: &mut ProxyCtx) -> PingoraResult<bool> {
        tracing::debug!("Handling Moat API request");

        let body = session.read_request_body().await?.unwrap_or_default();
        let req = session.req_header();

        let mut builder = Request::builder()
            .method(req.method.clone())
            .uri(req.uri.clone())
            .version(req.version);
        for (key, value) in req.headers.iter() {
            builder = builder.header(key, value);
        }
        let request = builder.body(body);
        let response = self.api.invoke(request).await;

        let mut header = ResponseHeader::build_no_case(response.status(), None)?;
        for (key, value) in response.headers().iter() {
            header.append_header(key, value)?;
        }
        header.set_version(response.version());

        let body = response.into_body().into_bytes().await.map_err(Error::other)?;

        header.set_content_length(body.len())?;
        session.write_response_header(Box::new(header), true).await?;

        session.write_response_body(Some(body), true).await?;

        Ok(true)
    }

    async fn write_get_object_response(&self, session: &mut Session, bytes: Bytes) -> PingoraResult<()> {
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
            .map_err(|e| e.into())
    }
}

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        ProxyCtx {
            upstream_peer: UpstreamPeer::None,
            start: Instant::now(),
            category: Category::Uncategoried,
            extra: S3Metrics::EXTRA_NONE.into(),
        }
    }

    async fn upstream_peer(&self, _: &mut Session, ctx: &mut Self::CTX) -> PingoraResult<Box<HttpPeer>> {
        tracing::debug!(?ctx, "looking up upstream peer");
        let upstream_peer = match &ctx.upstream_peer {
            UpstreamPeer::None => {
                return Err(Error::explain("no upstream peer to route").into());
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

    async fn early_request_filter(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> PingoraResult<()>
    where
        Self::CTX: Send + Sync,
    {
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> PingoraResult<bool>
    where
        Self::CTX: Send + Sync,
    {
        let header = session.req_header();
        ctx.category = Category::parse(self.api.prefix(), header)?;

        tracing::trace!(?header, category = ?ctx.category, "Receive request");

        match &ctx.category {
            Category::MoatApi => {
                return self.handle_moat_api(session, ctx).await;
            }
            Category::S3GetObject(S3GetObjectArgs { bucket, .. }) if bucket == &self.s3_bucket => {
                return self.handle_get_object(session, ctx).await;
            }
            Category::S3GetObject { .. } | Category::S3Other => {
                ctx.upstream_peer = UpstreamPeer::S3;
            }
            Category::Uncategoried => {}
        }

        Ok(false)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<()>
    where
        Self::CTX: Send + Sync,
    {
        tracing::trace!(?upstream_request, "Upstream request before filtering");

        let headers = self.resigner.resign(
            &upstream_request.method,
            &upstream_request.uri,
            upstream_request.headers.iter(),
        );
        for (k, v) in headers {
            upstream_request.insert_header(k, v).unwrap();
        }

        tracing::trace!(?upstream_request, "Upstream request after filtering");

        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()>
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

    async fn logging(&self, session: &mut Session, e: Option<&PingoraError>, ctx: &mut Self::CTX)
    where
        Self::CTX: Send + Sync,
    {
        let metrics = Metrics::global();
        match &ctx.category {
            Category::Uncategoried => {}
            Category::MoatApi => {}
            Category::S3GetObject(_) => {
                let operation = S3Metrics::OPERATION_GET_OBJECT;
                let status = match e {
                    Some(_) => S3Metrics::STATUS_ERR,
                    None => S3Metrics::STATUS_OK,
                };
                metrics
                    .s3
                    .count
                    .add(1, &S3Metrics::labels(operation, status, ctx.extra.clone()));
                metrics.s3.bytes.add(
                    session.body_bytes_sent() as _,
                    &S3Metrics::labels(operation, status, ctx.extra.clone()),
                );
                metrics.s3.duration.record(
                    ctx.start.elapsed().as_secs_f64(),
                    &S3Metrics::labels(operation, status, ctx.extra.clone()),
                );
            }
            Category::S3Other => {}
        }
    }
}
