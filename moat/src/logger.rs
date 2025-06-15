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

use std::fs::create_dir_all;

use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, prelude::*};

use crate::server::MoatConfig;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum)]
pub enum LogRotation {
    Minutely,
    Hourly,
    Daily,
    Never,
}

impl From<LogRotation> for Rotation {
    fn from(value: LogRotation) -> Self {
        match value {
            LogRotation::Minutely => Rotation::MINUTELY,
            LogRotation::Hourly => Self::HOURLY,
            LogRotation::Daily => Self::DAILY,
            LogRotation::Never => Self::NEVER,
        }
    }
}

#[derive(Debug, Args, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[clap(long = "log_dir", default_value = ".moat/log/")]
    log_dir: String,
    #[clap(long = "log_rotation", default_value = "never")]
    rotation: LogRotation,
    #[clap(long = "log_color", default_value_t = false)]
    color: bool,
}

pub fn init_logger(config: &MoatConfig) {
    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    create_dir_all(&config.logging.log_dir).expect("Failed to create log directory");
    let file_appender = RollingFileAppender::builder()
        .rotation(config.logging.rotation.into())
        .filename_prefix(config.peer.to_string())
        .filename_suffix("log")
        .build(&config.logging.log_dir)
        .unwrap();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(config.logging.color);

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(stdout_layer)
        .with(file_layer)
        .init();
}
