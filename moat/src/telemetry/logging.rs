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

use crate::{
    config::TelemetryConfig,
    error::{Error, Result},
    meta::model::Peer,
};
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, Protocol, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    logs::{BatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider},
};
use tracing_appender::rolling::RollingFileAppender;
use tracing_subscriber::{EnvFilter, prelude::*};

pub fn init(config: &TelemetryConfig, peer: &Peer, attributes: &[KeyValue]) -> Result<Box<dyn Send + Sync + 'static>> {
    let exporter = LogExporter::builder()
        .with_tonic()
        .with_protocol(Protocol::Grpc)
        .with_endpoint(&config.logging_endpoint)
        .build()
        .map_err(Error::other)?;
    let processor = BatchLogProcessor::builder(exporter)
        .with_batch_config(BatchConfigBuilder::default().build())
        .build();
    let resource = Resource::builder().with_attributes(attributes.to_vec()).build();
    let provider = SdkLoggerProvider::builder()
        .with_log_processor(processor)
        .with_resource(resource)
        .build();
    let otel_layer = OpenTelemetryTracingBridge::new(&provider);

    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    create_dir_all(&config.logging_dir).expect("Failed to create log directory");
    let file_appender = RollingFileAppender::builder()
        .rotation(config.logging_rotation.into())
        .filename_prefix(peer.to_string())
        .filename_suffix("log")
        .build(&config.logging_dir)
        .unwrap();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(config.logging_color);

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(stdout_layer)
        .with(file_layer)
        .with(otel_layer)
        .init();

    Ok(Box::new(scopeguard::guard((), move |_| {
        provider
            .shutdown()
            .inspect_err(|e| {
                tracing::error!(?e, "Failed to shutdown OpenTelemetry Tracer Provider");
            })
            .ok();
    })))
}
