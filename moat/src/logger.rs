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

use clap::Args;
use serde::{Deserialize, Serialize};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, prelude::*};

use crate::server::MoatConfig;

#[derive(Debug, Args, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[clap(long = "log_dir", default_value = ".moat/log/")]
    log_dir: String,
}

pub fn init_logger(config: &MoatConfig) {
    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    create_dir_all(&config.logging.log_dir).expect("Failed to create log directory");
    let prefix = format!("{peer}.log", peer = config.peer);
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &config.logging.log_dir, prefix);
    let file_layer = tracing_subscriber::fmt::layer().with_writer(file_appender);

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(stdout_layer)
        .with(file_layer)
        .init();
}
