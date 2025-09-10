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

use crate::{
    config::TelemetryConfig,
    error::{Error, Result},
    meta::model::Peer,
};

use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
};

pub fn init(config: &TelemetryConfig, _: &Peer, attributes: &[KeyValue]) -> Result<Box<dyn Send + Sync + 'static>> {
    let handle = tokio::runtime::Handle::current();

    let resource = Resource::builder().with_attributes(attributes.to_vec()).build();
    let mut provider_builder = SdkMeterProvider::builder().with_resource(resource);

    if !config.meter_endpoint.is_empty() {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_protocol(Protocol::Grpc)
            .with_endpoint(&config.meter_endpoint)
            .build()
            .map_err(Error::other)?;

        let reader = PeriodicReader::builder(exporter)
            .with_interval(config.meter_report_interval)
            .build();

        provider_builder = provider_builder.with_reader(reader);
    }

    #[cfg(feature = "prometheus")]
    if let Some(addr) = config.meter_prometheus_listen {
        use http_body_util::Full;

        use hyper::{
            Method, Request, Response,
            body::{Bytes, Incoming},
            header::CONTENT_TYPE,
            service::service_fn,
        };
        use hyper_util::{
            rt::{TokioExecutor, TokioIo},
            server::conn::auto::Builder,
        };

        use prometheus::{Encoder, Registry, TextEncoder};
        use tokio::net::TcpListener;

        use crate::error::Error;

        async fn serve_req(
            req: Request<Incoming>,
            registry: Registry,
        ) -> std::result::Result<Response<Full<Bytes>>, hyper::Error> {
            let response = match (req.method(), req.uri().path()) {
                (&Method::GET, "/metrics") => {
                    let mut buffer = vec![];
                    let encoder = TextEncoder::new();
                    let metric_families = registry.gather();
                    encoder.encode(&metric_families, &mut buffer).unwrap();

                    Response::builder()
                        .status(200)
                        .header(CONTENT_TYPE, encoder.format_type())
                        .body(Full::new(Bytes::from(buffer)))
                        .unwrap()
                }
                (&Method::GET, "/") => Response::builder()
                    .status(200)
                    .body(Full::new("Hello World".into()))
                    .unwrap(),
                _ => Response::builder()
                    .status(404)
                    .body(Full::new("Missing Page".into()))
                    .unwrap(),
            };

            Ok(response)
        }

        let registry = Registry::new();

        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .map_err(Error::other)?;

        provider_builder = provider_builder.with_reader(exporter);

        handle.spawn(async move {
            tracing::info!(?addr, "[prometheus exporter]: Starting Prometheus metrics server.",);
            let listener = TcpListener::bind(addr).await.unwrap();
            while let Ok((stream, peer)) = listener.accept().await {
                tracing::debug!(?peer, "[prometheus exporter]: Accepted connection.");
                if let Err(e) = Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service_fn(|req| serve_req(req, registry.clone())))
                    .await
                {
                    tracing::error!(?e, "[prometheus exporter]: Failed to serve connection.");
                }
            }
        });
    }

    let provider = provider_builder.build();

    opentelemetry::global::set_meter_provider(provider.clone());

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
