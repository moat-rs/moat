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

use opentelemetry::KeyValue;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
};

use crate::{
    config::TelemetryConfig,
    error::{Error, Result},
    meta::model::Peer,
};

pub fn init(config: &TelemetryConfig, _: &Peer, attributes: &[KeyValue]) -> Result<Box<dyn Send + Sync + 'static>> {
    if config.meter_endpoint.is_empty() {
        return Ok(Box::new(()));
    }

    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_protocol(Protocol::Grpc)
        .with_endpoint(&config.meter_endpoint)
        .build()
        .map_err(Error::other)?;

    let reader = PeriodicReader::builder(exporter)
        .with_interval(config.meter_report_interval)
        .build();
    let resource = Resource::builder().with_attributes(attributes.to_vec()).build();
    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    opentelemetry::global::set_meter_provider(provider.clone());

    let handle = tokio::runtime::Handle::current();
    Ok(Box::new(scopeguard::guard((), move |_| {
        handle.block_on(async {
            provider
                .shutdown()
                .inspect_err(|e| {
                    tracing::error!(?e, "Failed to shutdown OpenTelemetry Meter Provider");
                })
                .ok();
        })
    })))
}
