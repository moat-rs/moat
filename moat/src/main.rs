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

mod api;
mod aws;
mod config;
mod error;
mod meta;
mod metrics;
mod runtime;
mod server;
mod telemetry;

use clap::Parser;

use crate::{config::MoatConfig, runtime::Runtime, server::Moat};

fn main() {
    let config = MoatConfig::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");
    let runtime = Runtime::new(runtime);

    let guards = runtime
        .block_on(async { telemetry::init(&config) })
        .expect("Failed to initialize telemetry");

    tracing::info!(?config, "Start Moat");

    if let Err(e) = Moat::run(config, &runtime) {
        tracing::error!(?e, "Failed to run Moat");
    }

    drop(guards);
    drop(runtime);
}
