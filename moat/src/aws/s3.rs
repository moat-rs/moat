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

use clap::Args;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Args, Serialize, Deserialize)]
pub struct S3Config {
    /// Endpoint must be full uri, e.g.
    ///
    /// AWS S3: https://s3.amazonaws.com or https://s3.{region}.amazonaws.com
    /// Cloudflare R2: https://<ACCOUNT_ID>.r2.cloudflarestorage.com
    /// Aliyun OSS: https://{region}.aliyuncs.com
    /// Tencent COS: https://cos.{region}.myqcloud.com
    /// Minio: http://127.0.0.1:9000
    #[clap(long = "s3-endpoint")]
    pub endpoint: Url,
    #[clap(long = "s3-access-key-id")]
    pub access_key_id: String,
    #[clap(long = "s3-secret-access-key")]
    pub secret_access_key: String,
    #[clap(long = "s3-region")]
    pub region: String,
    #[clap(long = "s3-bucket")]
    pub bucket: String,
}
