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

use bytesize::ByteSize;
use clap::{Parser, ValueEnum};
use foyer::{RecoverMode, Throttle};
use serde::{Deserialize, Serialize};

use tracing_appender::rolling::Rotation as TracingAppenderRotation;
use url::Url;

use crate::meta::model::{Peer, Role};

#[derive(Debug, Parser, Serialize, Deserialize)]
pub struct MoatConfig {
    #[clap(long, default_value = "127.0.0.1:23456")]
    pub listen: SocketAddr,
    #[clap(long, default_value = "cache")]
    pub role: Role,
    #[clap(long)]
    pub peer: Peer,
    // TODO(MrCroxx): Handle tls configuration.
    #[clap(long, default_value = "false")]
    pub tls: bool,
    #[clap(long, num_args = 1.., value_delimiter = ',')]
    pub bootstrap_peers: Vec<Peer>,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "10s")]
    pub peer_eviction_timeout: Duration,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "3s")]
    pub health_check_timeout: Duration,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "1s")]
    pub health_check_interval: Duration,
    #[clap(long, default_value_t = 3)]
    pub health_check_peers: usize,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "3s")]
    pub sync_timeout: Duration,
    #[clap(long, value_parser = humantime::parse_duration, default_value = "1s")]
    pub sync_interval: Duration,
    #[clap(long, default_value_t = 3)]
    pub sync_peers: usize,
    #[clap(long, default_value_t = 1)]
    pub weight: usize,

    #[clap(flatten)]
    pub s3_config: S3Config,

    #[clap(flatten)]
    pub cache: CacheConfig,

    #[clap(flatten)]
    pub telemetry: TelemetryConfig,

    #[clap(flatten)]
    pub api: ApiConfig,
}

#[derive(Debug, Parser, Serialize, Deserialize)]
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

#[derive(Debug, Parser, Serialize, Deserialize)]
pub struct CacheConfig {
    #[clap(long, default_value = "64MiB")]
    pub mem: ByteSize,

    #[clap(long)]
    pub dir: Option<String>,

    #[clap(long, default_value = "1GiB", requires = "dir")]
    pub disk: ByteSize,

    #[clap(long, default_value = "64MiB")]
    pub file_size: ByteSize,

    #[clap(flatten)]
    pub throttle: Throttle,

    #[clap(long, default_value_t = 4)]
    pub flushers: usize,

    #[clap(long, default_value_t = 2)]
    pub reclaimers: usize,

    #[clap(long, default_value = "16MiB")]
    pub buffer_pool_size: ByteSize,

    #[clap(long, default_value = "quiet")]
    pub recover_mode: RecoverMode,

    #[clap(long, default_value_t = 4)]
    pub recover_concurrency: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum)]
pub enum Rotation {
    Minutely,
    Hourly,
    Daily,
    Never,
}

impl From<Rotation> for TracingAppenderRotation {
    fn from(value: Rotation) -> Self {
        match value {
            Rotation::Minutely => Self::MINUTELY,
            Rotation::Hourly => Self::HOURLY,
            Rotation::Daily => Self::DAILY,
            Rotation::Never => Self::NEVER,
        }
    }
}

#[derive(Debug, Clone, Parser, Serialize, Deserialize)]
pub struct TelemetryConfig {
    #[clap(long = "telemetry-service-name", default_value = "moat")]
    pub service_name: String,

    #[clap(
        long = "telemetry-meter-endpoint",
        // default_value = "http://localhost:9090/api/v1/otlp/v1/metrics"
        default_value = "http://localhost:4317"
    )]
    pub meter_endpoint: String,
    #[clap(long = "telemetry-meter-report-interval", value_parser = humantime::parse_duration, default_value = "1s")]
    pub meter_report_interval: Duration,

    // #[clap(long = "telemetry-logging-endpoint", default_value = "http://localhost:3100/otlp")]
    #[clap(long = "telemetry-logging-endpoint", default_value = "http://localhost:4317")]
    pub logging_endpoint: String,

    #[clap(long = "telemetry-logging-dir", default_value = ".moat/log/")]
    pub logging_dir: String,
    #[clap(long = "telemetry-logging-rotation", default_value = "never")]
    pub logging_rotation: Rotation,
    #[clap(long = "telemetry-logging-color", default_value_t = false)]
    pub logging_color: bool,
}

#[derive(Debug, Clone, Parser, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Prefix for all API endpoints.
    #[clap(long, default_value = "/api")]
    pub prefix: String,
}
