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
#[command(author, version, about, long_about = None)]
pub struct MoatConfig {
    #[clap(long, env = "MOAT_LISTEN", default_value = "127.0.0.1:23456")]
    pub listen: SocketAddr,
    #[clap(long, env = "MOAT_ROLE", default_value = "cache")]
    pub role: Role,
    #[clap(long, env = "MOAT_PEER")]
    pub peer: Peer,
    // TODO(MrCroxx): Handle tls configuration.
    #[clap(long, env = "MOAT_TLS", default_value = "false")]
    pub tls: bool,
    #[clap(long, env = "MOAT_BOOTSTRAP_PEERS", num_args = 1.., value_delimiter = ',')]
    pub bootstrap_peers: Vec<Peer>,
    #[clap(long, env = "MOAT_PEER_EVICTION_TIMEOUT", value_parser = humantime::parse_duration, default_value = "10s")]
    pub peer_eviction_timeout: Duration,
    #[clap(long, env = "MOAT_HEALTH_CHECK_TIMEOUT", value_parser = humantime::parse_duration, default_value = "3s")]
    pub health_check_timeout: Duration,
    #[clap(long, env = "MOAT_HEALTH_CHECK_INTERVAL", value_parser = humantime::parse_duration, default_value = "1s")]
    pub health_check_interval: Duration,
    #[clap(long, env = "MOAT_HEALTH_CHECK_PEERS", default_value_t = 3)]
    pub health_check_peers: usize,
    #[clap(long, env = "MOAT_SYNC_TIMEOUT", value_parser = humantime::parse_duration, default_value = "3s")]
    pub sync_timeout: Duration,
    #[clap(long, env = "MOAT_SYNC_INTERVAL", value_parser = humantime::parse_duration, default_value = "1s")]
    pub sync_interval: Duration,
    #[clap(long, env = "MOAT_SYNC_PEERS", default_value_t = 3)]
    pub sync_peers: usize,
    #[clap(long, env = "MOAT_WEIGHT", default_value_t = 1)]
    pub weight: usize,
    #[clap(long, env = "MOAT_CHUNK", default_value = "8MiB")]
    pub chunk: ByteSize,

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
    #[clap(long = "s3-endpoint", env = "MOAT_S3_ENDPOINT")]
    pub endpoint: Url,
    #[clap(long = "s3-access-key-id", env = "MOAT_S3_ACCESS_KEY_ID")]
    pub access_key_id: String,
    #[clap(long = "s3-secret-access-key", env = "MOAT_S3_SECRET_ACCESS_KEY")]
    pub secret_access_key: String,
    #[clap(long = "s3-region", env = "MOAT_S3_REGION")]
    pub region: String,
    #[clap(long = "s3-bucket", env = "MOAT_S3_BUCKET")]
    pub bucket: String,
}

#[derive(Debug, Parser, Serialize, Deserialize)]
pub struct CacheConfig {
    #[clap(long, env = "MOAT_CACHE_MEM", default_value = "64MiB")]
    pub mem: ByteSize,

    #[clap(long, env = "MOAT_CACHE_DIR")]
    pub dir: Option<String>,

    #[clap(long, env = "MOAT_CACHE_DISK", default_value = "1GiB", requires = "dir")]
    pub disk: ByteSize,

    #[clap(long, env = "MOAT_CACHE_FILE_SIZE", default_value = "64MiB")]
    pub file_size: ByteSize,

    #[clap(flatten)]
    pub throttle: Throttle,

    #[clap(long, env = "MOAT_CACHE_FLUSHERS", default_value_t = 4)]
    pub flushers: usize,

    #[clap(long, env = "MOAT_CACHE_RECLAIMERS", default_value_t = 2)]
    pub reclaimers: usize,

    #[clap(long, env = "MOAT_CACHE_BUFFER_POOL_SIZE", default_value = "64MiB")]
    pub buffer_pool_size: ByteSize,

    #[clap(long, env = "MOAT_CACHE_RECOVER_MODE", default_value = "quiet")]
    pub recover_mode: RecoverMode,

    #[clap(long, env = "MOAT_CACHE_RECOVER_CONCURRENCY", default_value_t = 4)]
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
    #[clap(
        long = "telemetry-service-name",
        env = "MOAT_TELEMETRY_SERVICE_NAME",
        default_value = "moat"
    )]
    pub service_name: String,

    /// Endpoint for OpenTelemetry meter collector.
    ///
    /// Example: `http://localhost:4317`.
    #[clap(
        long = "telemetry-meter-endpoint",
        env = "MOAT_TELEMETRY_METER_ENDPOINT",
        default_value = ""
    )]
    pub meter_endpoint: String,
    #[clap(long = "telemetry-meter-report-interval", value_parser = humantime::parse_duration, default_value = "1s")]
    pub meter_report_interval: Duration,

    /// Endpoint for OpenTelemetry logging collector.
    ///
    /// Example: `http://localhost:4317`.
    #[clap(
        long = "telemetry-logging-endpoint",
        env = "MOAT_TELEMETRY_LOGGING_ENDPOINT",
        default_value = ""
    )]
    pub logging_endpoint: String,

    #[clap(
        long = "telemetry-logging-dir",
        env = "MOAT_TELEMETRY_LOGGING_DIR",
        default_value = ".moat/log/"
    )]
    pub logging_dir: String,
    #[clap(
        long = "telemetry-logging-rotation",
        env = "MOAT_TELEMETRY_LOGGING_ROTATION",
        default_value = "never"
    )]
    pub logging_rotation: Rotation,
    #[clap(
        long = "telemetry-logging-color",
        env = "MOAT_TELEMETRY_LOGGING_COLOR",
        default_value_t = false
    )]
    pub logging_color: bool,
}

#[derive(Debug, Clone, Parser, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Prefix for all API endpoints.
    #[clap(long, env = "MOAT_API_PREFIX", default_value = "/api")]
    pub prefix: String,
}
