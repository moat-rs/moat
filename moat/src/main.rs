use std::collections::BTreeMap;

use async_trait::async_trait;
use clap::Parser;
use hmac::{Hmac, Mac};
use pingora::prelude::*;
use pingora::proxy::http_proxy_service_with_name;
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Clone)]
pub struct S3ProxyConfig {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

pub struct S3ProxyApp {
    config: S3ProxyConfig,
}

impl S3ProxyApp {
    pub fn new(config: S3ProxyConfig) -> Self {
        Self { config }
    }

    fn parse_s3_request(&self, req: &RequestHeader) -> S3Request {
        let path = req.uri.path();
        let method = req.method.as_str();

        // S3 GetObject schema: GET /{bucket}/{key}
        if method == "GET" && path.len() > 1 {
            let parts: Vec<&str> = path[1..].splitn(2, '/').collect();
            if parts.len() == 2 {
                return S3Request::GetObject {
                    bucket: parts[0].to_string(),
                    key: parts[1].to_string(),
                };
            }
        }

        S3Request::Other
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
}

#[derive(Debug)]
enum S3Request {
    GetObject { bucket: String, key: String },
    Other,
}

#[derive(Debug)]
pub struct S3ProxyCtx;

impl Default for S3ProxyCtx {
    fn default() -> Self {
        Self
    }
}

impl S3ProxyApp {
    const UNSIGNED_PAYLOAD: &'static str = "UNSIGNED-PAYLOAD";
}

#[async_trait]
impl ProxyHttp for S3ProxyApp {
    type CTX = S3ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        S3ProxyCtx
    }

    async fn upstream_peer(&self, _: &mut Session, _: &mut Self::CTX) -> Result<Box<HttpPeer>> {
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
        let req = session.req_header();

        let s3req = self.parse_s3_request(req);
        match s3req {
            S3Request::GetObject { bucket, key } => match self.handle_get_object(&bucket, &key, session).await {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(e) => tracing::error!(?e, "Error handling GetObject"),
            },
            S3Request::Other => {}
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
        tracing::debug!(?upstream_request, "Upstream request before filtering");

        let request_date_time = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

        // normalize request headrs
        let endpoint = Url::parse(&self.config.endpoint).expect("Invalid endpoint URL");
        upstream_request
            .insert_header("Host", endpoint.host_str().expect("Endpoint must have a host"))
            .expect("Failed to insert Host header");
        upstream_request
            .insert_header("X-Amz-Content-Sha256", "UNSIGNED-PAYLOAD".to_string())
            .expect("Failed to insert X-Amz-Content-Sha256 header");
        upstream_request
            .insert_header("X-Amz-Date", request_date_time.clone())
            .expect("Failed to insert X-Amz-Date header");

        let urlencode = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();

        let queries: BTreeMap<String, String> = upstream_request
            .uri
            .query()
            .map(|s| {
                url::form_urlencoded::parse(s.as_bytes())
                    .map(|(key, value)| (urlencode(&key), urlencode(&value)))
                    .collect()
            })
            .unwrap_or_default();

        let headers: BTreeMap<String, String> = upstream_request
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

        let http_method = upstream_request.method.as_str();
        let canonical_uri = upstream_request
            .uri
            .path()
            .split('/')
            .map(urlencode)
            .collect::<Vec<String>>()
            .join("/");
        let canonical_query_string = queries
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<String>>()
            .join("&");
        let canonical_headers = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect::<String>();
        let signed_headers = headers.keys().map(|s| s.to_string()).collect::<Vec<String>>().join(";");

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
            YYYYMMDD = &request_date_time[..8],
            region = self.config.region,
            service = "s3",
        );

        let string_to_sign =
            format!("{algorithm}\n{request_date_time}\n{credential_scope}\n{hashed_canonical_request}");
        tracing::debug!(string_to_sign, "String to Sign");

        let hmac_sha256 = |key: &[u8], data: &[u8]| {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 key must be valid");
            mac.update(data);
            mac.finalize().into_bytes()
        };

        let date_key = hmac_sha256(
            format!("AWS4{}", self.config.secret_access_key).as_bytes(),
            &request_date_time.as_bytes()[..8],
        );
        let date_region_key = hmac_sha256(&date_key, self.config.region.as_bytes());
        let date_region_service_key = hmac_sha256(&date_region_key, "s3".as_bytes());
        let signing_key = hmac_sha256(&date_region_service_key, b"aws4_request");
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
        tracing::debug!(signature, "Signature");

        upstream_request
            .insert_header(
                "Authorization",
                format!(
                    "{algorithm} Credential={access_key_id}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
                    algorithm = algorithm,
                    access_key_id = self.config.access_key_id,
                    credential_scope = credential_scope,
                    signed_headers = signed_headers,
                    signature = signature
                ),
            )
            .expect("Failed to insert Host header");

        tracing::debug!(?upstream_request, "Upstream request after filtering");

        Ok(())
    }
}

#[derive(Debug, Parser)]
struct Args {
    /// S3 proxy listening host.
    #[clap(long, default_value = "127.0.0.1")]
    host: String,
    /// S3 proxy listening port.
    #[clap(long, default_value = "23456")]
    port: u16,
    #[clap(long)]
    endpoint: String,
    #[clap(long)]
    access_key_id: String,
    #[clap(long)]
    secret_access_key: String,
    #[clap(long)]
    region: String,
}

fn main() {
    let args = Args::parse();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let mut server = Server::new(Some(Opt::default())).unwrap();
    server.bootstrap();

    let app = S3ProxyApp::new(S3ProxyConfig {
        endpoint: args.endpoint,
        access_key_id: args.access_key_id,
        secret_access_key: args.secret_access_key,
        region: args.region,
    });

    let mut service = http_proxy_service_with_name(&server.configuration, app, "S3 Proxy");

    let listen = format!("{}:{}", args.host, args.port);
    service.add_tcp(&listen);

    server.add_service(service);

    server.run_forever();
}

// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[test]
//     fn test_parse_s3_request() {
//         let app = S3ProxyApp::new();

//         let mut req = RequestHeader::build("GET", b"/my-bucket/path/to/file.txt", None).unwrap();

//         if let Some(S3Request::GetObject { bucket, key }) = app.parse_s3_request(&req) {
//             assert_eq!(bucket, "my-bucket");
//             assert_eq!(key, "path/to/file.txt");
//         } else {
//             panic!("Should parse as GetObject request");
//         }
//     }
// }
