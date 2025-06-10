use std::collections::BTreeMap;

use async_trait::async_trait;
use clap::Parser;
use hmac::{Hmac, Mac};
use pingora::proxy::http_proxy_service_with_name;
use pingora::server::configuration::ServerConf;
use pingora::{http::ResponseHeader, prelude::*};
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;
use url::Url;

// use crate::meta::{MetaManager, MetaServiceConfig};

const MOAT_API_HEADER: &str = "x-moat-api";

use poem::{get, handler};

#[handler]
async fn hello() -> &'static str {
    "Hello, Moat!"
}

#[derive(Debug, Clone)]
pub struct S3ProxyConfig {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

pub struct S3ProxyApp {
    resigner: S3RequestResigner,

    config: S3ProxyConfig,
}

impl S3ProxyApp {
    pub fn new(config: S3ProxyConfig) -> Self {
        let resigner = S3RequestResigner {
            endpoint: Url::parse(&config.endpoint).expect("Invalid endpoint URL"),
            region: config.region.clone(),
            access_key_id: config.access_key_id.clone(),
            secret_access_key: config.secret_access_key.clone(),
        };
        Self { resigner, config }
    }

    async fn handle_get_object(&self, bucket: &str, key: &str, _session: &mut Session) -> Result<bool, Error> {
        tracing::info!(bucket, key, "handle get object");

        Ok(false)

        // // Example:

        // println!(
        //     "Special handling for GetObject: bucket={}, key={}",
        //     bucket, key
        // );

        // if key == "special-file.txt" {
        //     let response = "This is a special file handled by proxy!";
        //     let mut header = ResponseHeader::build(200, None).unwrap();
        //     header.insert_header("Content-Type", "text/plain").unwrap();
        //     header
        //         .insert_header("Content-Length", response.len().to_string())
        //         .unwrap();

        //     session.write_response_header(Box::new(header)).await?;
        //     session
        //         .write_response_body(response.as_bytes().into())
        //         .await?;
        //     return Ok(true);
        // }

        // Ok(false)
    }

    async fn handle_moat_api(&self, session: &mut Session) -> Result<bool> {
        tracing::debug!("Handling Moat API request");

        use poem::Endpoint;

        let route: poem::Route = poem::Route::new().at("/hello", get(hello));

        let header = session.req_header();

        let mut builder = poem::Request::builder()
            .method(header.method.clone())
            .uri(header.uri.clone())
            .version(header.version);

        for (key, value) in header.headers.iter() {
            builder = builder.header(key, value);
        }

        let body = match session.read_request_body().await? {
            Some(bytes) => poem::Body::from(bytes),
            None => poem::Body::empty(),
        };

        let poem_request = builder.body(body);
        let poem_response = route.get_response(poem_request).await;

        let mut header = ResponseHeader::build_no_case(poem_response.status(), None)?;
        for (key, value) in poem_response.headers().iter() {
            header.append_header(key, value)?;
        }
        header.set_version(poem_response.version());

        let body = poem_response
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| pingora::Error::because(ErrorType::InternalError, "", e))?;

        header.set_content_length(body.len())?;
        session.write_response_header(Box::new(header), true).await?;

        session.write_response_body(Some(body), true).await?;

        Ok(true)
    }
}

#[derive(Debug)]
enum MoatRequest {
    MoatApi,
    S3GetObject { bucket: String, key: String },
    S3Other,
}

impl MoatRequest {
    fn parse(request: &RequestHeader) -> Self {
        if request.headers.get(MOAT_API_HEADER).is_some() {
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

struct S3RequestResigner {
    endpoint: Url,
    region: String,
    access_key_id: String,
    secret_access_key: String,
}

impl S3RequestResigner {
    const UNSIGNED_PAYLOAD: &'static str = "UNSIGNED-PAYLOAD";

    fn urlencode(s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    fn resign(&self, request: &mut RequestHeader) {
        let datetime = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

        self.rewrite_request_headers(request, datetime.clone());

        let queries_to_sign: BTreeMap<String, String> = request
            .uri
            .query()
            .map(|s| {
                url::form_urlencoded::parse(s.as_bytes())
                    .map(|(key, value)| (Self::urlencode(&key), Self::urlencode(&value)))
                    .collect()
            })
            .unwrap_or_default();

        let headers_to_sign: BTreeMap<String, String> = request
            .headers
            .iter()
            .map(|(key, value)| {
                (
                    key.as_str().to_lowercase(),
                    value.to_str().map(|s| s.trim()).unwrap_or("").to_string(),
                )
            })
            .filter(|(key, _)| match key.as_str() {
                "host" | "content-type" => true,
                s if s.starts_with("x-amz-") => true,
                _ => false,
            })
            .collect();

        let http_method = request.method.as_str();
        let canonical_uri = request
            .uri
            .path()
            .split('/')
            .map(Self::urlencode)
            .collect::<Vec<String>>()
            .join("/");
        let canonical_query_string = queries_to_sign
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<String>>()
            .join("&");
        let canonical_headers = headers_to_sign
            .iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect::<String>();
        let signed_headers = headers_to_sign
            .keys()
            .map(|s| s.to_string())
            .collect::<Vec<String>>()
            .join(";");

        let canonical_request = format!(
            "{http_method}\n{canonical_uri}\n{canonical_query_string}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            payload_hash = Self::UNSIGNED_PAYLOAD,
        );
        tracing::debug!(canonical_request, "Canonical Request");

        let hashed_canonical_request = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        tracing::debug!(hashed_canonical_request, "Hashed Canonical Request");

        let algorithm = "AWS4-HMAC-SHA256";
        let credential_scope = format!(
            "{YYYYMMDD}/{region}/{service}/aws4_request",
            YYYYMMDD = &datetime[..8],
            region = self.region,
            service = "s3",
        );

        let string_to_sign = format!("{algorithm}\n{datetime}\n{credential_scope}\n{hashed_canonical_request}");
        tracing::debug!(string_to_sign, "String to Sign");

        let hmac_sha256 = |key: &[u8], data: &[u8]| {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 key must be valid");
            mac.update(data);
            mac.finalize().into_bytes()
        };

        let date_key = hmac_sha256(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            &datetime.as_bytes()[..8],
        );
        let date_region_key = hmac_sha256(&date_key, self.region.as_bytes());
        let date_region_service_key = hmac_sha256(&date_region_key, "s3".as_bytes());
        let signing_key = hmac_sha256(&date_region_service_key, b"aws4_request");
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
        tracing::debug!(signature, "Signature");

        let authorization = format!(
            "{algorithm} Credential={access_key_id}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            algorithm = algorithm,
            access_key_id = self.access_key_id,
            credential_scope = credential_scope,
            signed_headers = signed_headers,
            signature = signature
        );

        self.rewrite_authorization(request, authorization);
    }

    /// Rewrites the request headers if necessary.
    fn rewrite_request_headers(&self, request: &mut RequestHeader, datetime: String) {
        request
            .insert_header("Host", self.endpoint.host_str().expect("Endpoint must have a host"))
            .expect("Failed to insert Host header");
        request
            .insert_header("X-Amz-Content-Sha256", "UNSIGNED-PAYLOAD".to_string())
            .expect("Failed to insert X-Amz-Content-Sha256 header");
        request
            .insert_header("X-Amz-Date", datetime)
            .expect("Failed to insert X-Amz-Date header");
    }

    fn rewrite_authorization(&self, request: &mut RequestHeader, authorization: String) {
        request
            .insert_header("Authorization", authorization.to_string())
            .expect("Failed to insert Authorization header");
    }
}

#[derive(Debug)]
pub struct S3ProxyCtx;

impl Default for S3ProxyCtx {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl ProxyHttp for S3ProxyApp {
    type CTX = S3ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        S3ProxyCtx
    }

    async fn upstream_peer(&self, session: &mut Session, _: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        tracing::debug!(header = ?session.req_header(), "looking up upstream peer");

        let url =
            Url::parse(&self.config.endpoint).map_err(|e| Error::explain(ErrorType::InternalError, format!("{e}")))?;

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
                return self.handle_moat_api(session).await;
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

#[derive(Debug, Parser)]
struct Args {
    #[clap(long, default_value = "127.0.0.1:23456")]
    endpoint: String,

    #[clap(long)]
    s3_endpoint: String,
    #[clap(long)]
    s3_access_key_id: String,
    #[clap(long)]
    s3_secret_access_key: String,
    #[clap(long)]
    s3_region: String,
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let args = Args::parse();
    tracing::info!(?args, "Start Moat");

    let _runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    let mut conf = ServerConf::new().unwrap();
    conf.grace_period_seconds = Some(0);

    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();

    let app = S3ProxyApp::new(S3ProxyConfig {
        endpoint: args.s3_endpoint,
        access_key_id: args.s3_access_key_id,
        secret_access_key: args.s3_secret_access_key,
        region: args.s3_region,
    });

    let mut service = http_proxy_service_with_name(&server.configuration, app, "S3 Proxy");

    service.add_tcp(&args.endpoint);

    server.add_service(service);

    server.run_forever();
}
