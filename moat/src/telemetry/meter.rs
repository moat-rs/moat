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

use crate::{
    config::TelemetryConfig,
    error::{Error, Result},
};

pub fn init(config: &TelemetryConfig) -> Result<Box<dyn Send + Sync + 'static>> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(&config.meter_endpoint)
        .build()
        .map_err(Error::other)?;

    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter)
        .with_interval(config.meter_report_interval)
        .build();
    let resource = opentelemetry_sdk::Resource::builder()
        .with_attributes([KeyValue::new("service.name", config.service_name.to_string())])
        .build();
    let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    opentelemetry::global::set_meter_provider(provider.clone());

    Ok(Box::new(scopeguard::guard((), move |_| {
        provider
            .shutdown()
            .inspect_err(|e| {
                tracing::error!(?e, "Failed to shutdown OpenTelemetry Meter Provider");
            })
            .ok();
    })))
}
