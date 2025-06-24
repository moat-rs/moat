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

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use http::{HeaderName, HeaderValue, Method, Uri};
use sha2::{Digest, Sha256};
use url::Url;

pub struct AwsSigV4ResignerConfig {
    pub endpoint: Url,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

pub struct AwsSigV4Resigner {
    endpoint: Url,
    region: String,
    access_key_id: String,
    secret_access_key: String,
}

impl AwsSigV4Resigner {
    const UNSIGNED_PAYLOAD: &'static str = "UNSIGNED-PAYLOAD";

    pub fn new(config: AwsSigV4ResignerConfig) -> Self {
        Self {
            endpoint: config.endpoint,
            region: config.region,
            access_key_id: config.access_key_id,
            secret_access_key: config.secret_access_key,
        }
    }

    fn urlencode(s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    pub fn resign<'a>(
        &self,
        method: &Method,
        uri: &Uri,
        headers: impl Iterator<Item = (&'a HeaderName, &'a HeaderValue)>,
    ) -> Vec<(HeaderName, HeaderValue)> {
        let mut res = vec![];

        let datetime = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

        res.extend([
            (
                HeaderName::from_static("host"),
                HeaderValue::from_str(self.endpoint.host_str().unwrap()).unwrap(),
            ),
            (
                HeaderName::from_static("x-amz-content-sha256"),
                HeaderValue::from_static(Self::UNSIGNED_PAYLOAD),
            ),
            (
                HeaderName::from_static("x-amz-date"),
                HeaderValue::from_str(&datetime).unwrap(),
            ),
        ]);

        let queries_to_sign: BTreeMap<String, String> = uri
            .query()
            .map(|s| {
                url::form_urlencoded::parse(s.as_bytes())
                    .map(|(key, value)| (Self::urlencode(&key), Self::urlencode(&value)))
                    .collect()
            })
            .unwrap_or_default();
        let headers_to_sign: BTreeMap<String, String> = headers
            .map(|(key, value)| {
                (
                    key.as_str().to_lowercase(),
                    value.to_str().map(|s| s.trim()).unwrap_or("").to_string(),
                )
            })
            .chain(res.iter().map(|(key, value)| {
                (
                    key.as_str().to_lowercase(),
                    value.to_str().map(|s| s.trim()).unwrap_or("").to_string(),
                )
            }))
            .filter(|(key, _)| match key.as_str() {
                "host" | "content-type" => true,
                s if s.starts_with("x-amz-") => true,
                _ => false,
            })
            .collect();

        let http_method = method.as_str();
        let canonical_uri = uri
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

        res.push((
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&authorization).unwrap(),
        ));

        res
    }
}
