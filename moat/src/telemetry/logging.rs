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

use std::{borrow::Cow, fs::create_dir_all};

use opentelemetry::{KeyValue, trace::TracerProvider};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use tracing_appender::rolling::RollingFileAppender;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{EnvFilter, prelude::*};

use crate::{
    config::MoatConfig,
    error::{Error, Result},
};

pub fn init(config: &MoatConfig) -> Result<Box<dyn Send + Sync + 'static>> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_protocol(Protocol::Grpc)
        .with_endpoint(&config.telemetry.logging_endpoint)
        .build()
        .map_err(Error::other);
    let resource = opentelemetry_sdk::Resource::builder()
        .with_attributes([KeyValue::new("service.name", config.telemetry.service_name.to_string())])
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter.unwrap())
        .with_resource(resource)
        .build();
    let tracer = provider.tracer(Cow::Owned(config.telemetry.service_name.clone()));
    let otel_layer = OpenTelemetryLayer::new(tracer);

    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    create_dir_all(&config.telemetry.logging_dir).expect("Failed to create log directory");
    let file_appender = RollingFileAppender::builder()
        .rotation(config.telemetry.logging_rotation.into())
        .filename_prefix(config.peer.to_string())
        .filename_suffix("log")
        .build(&config.telemetry.logging_dir)
        .unwrap();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(config.telemetry.logging_color);

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
                tracing::error!(?e, "Failed to shutdown OpenTelemetry Meter Provider");
            })
            .ok();
    })))
}
