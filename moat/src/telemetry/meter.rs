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

#[cfg(not(feature = "prometheus"))]
mod otel {
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
}

#[cfg(not(feature = "prometheus"))]
pub use otel::init;

#[cfg(feature = "prometheus")]
mod prometheus {

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
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider};
    use prometheus::{Encoder, Registry, TextEncoder};
    use tokio::net::TcpListener;

    use crate::{
        config::TelemetryConfig,
        error::{Error, Result},
        meta::model::Peer,
    };

    pub fn init(config: &TelemetryConfig, _: &Peer, attributes: &[KeyValue]) -> Result<Box<dyn Send + Sync + 'static>> {
        let Some(addr) = config.meter_prometheus_listen.clone() else {
            return Ok(Box::new(()));
        };

        let registry = Registry::new();

        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .map_err(Error::other)?;
        let resource = Resource::builder().with_attributes(attributes.to_vec()).build();
        let provider = SdkMeterProvider::builder()
            .with_reader(exporter)
            .with_resource(resource)
            .build();

        opentelemetry::global::set_meter_provider(provider.clone());

        let handle = tokio::runtime::Handle::current();

        handle.spawn(async move {
            let listener = TcpListener::bind(addr).await.unwrap();
            while let Ok((stream, _)) = listener.accept().await {
                if let Err(err) = Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service_fn(|req| serve_req(req, registry.clone())))
                    .await
                {
                    eprintln!("{err}");
                }
            }
        });

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
}

#[cfg(feature = "prometheus")]
pub use prometheus::init;
