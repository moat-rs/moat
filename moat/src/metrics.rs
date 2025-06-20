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

use std::{borrow::Cow, sync::LazyLock, time::Instant};

use opentelemetry::{
    KeyValue,
    metrics::{Counter, Gauge, Histogram, Meter},
};

use crate::meta::model::{Peer, Role};

#[derive(Debug)]
pub struct Metrics {
    pub api: ApiMetrics,
    pub cluster: ClusterMetrics,
    pub s3: S3Metrics,
}

impl Metrics {
    pub fn global() -> &'static Self {
        static GLOBAL_METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);
        &GLOBAL_METRICS
    }

    fn new() -> Self {
        let meter = opentelemetry::global::meter("moat");

        let api = ApiMetrics::new(&meter);
        let cluster = ClusterMetrics::new(&meter);
        let s3 = S3Metrics::new(&meter);

        Self { api, cluster, s3 }
    }
}

#[derive(Debug)]
pub struct ApiMetrics {
    pub count: Counter<u64>,
    #[expect(unused)]
    pub duration: Histogram<f64>,
}

impl ApiMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            count: meter
                .u64_counter("moat.api.count")
                .with_description("Moat API call count")
                .build(),
            duration: meter
                .f64_histogram("moat.api.duration")
                .with_description("Moat API call duration in seconds")
                .with_unit("second")
                .with_boundaries(vec![
                    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 5.0, 10.0,
                ])
                .build(),
        }
    }

    pub fn labels(api: &str, method: &str, status: &str) -> [KeyValue; 3] {
        [
            KeyValue::new("api", api.to_string()),
            KeyValue::new("method", method.to_string()),
            KeyValue::new("status", status.to_string()),
        ]
    }
}

#[derive(Debug)]
pub struct ClusterMetrics {
    pub peer: Gauge<i64>,
}

impl ClusterMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            peer: meter
                .i64_gauge("moat.peer.up")
                .with_description("Moat peer state")
                .build(),
        }
    }

    pub fn peer_labels(target: &Peer, role: Role) -> [KeyValue; 2] {
        [
            KeyValue::new("target", target.to_string()),
            KeyValue::new("role", role.to_string()),
        ]
    }
}

#[derive(Debug)]
pub struct S3Metrics {
    pub count: Counter<u64>,
    pub bytes: Counter<u64>,
    pub duration: Histogram<f64>,
}

impl S3Metrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            count: meter
                .u64_counter("moat.s3.count")
                .with_description("Moat API call count")
                .build(),
            bytes: meter
                .u64_counter("moat.s3.bytes")
                .with_description("Moat S3 API call bytes transferred")
                .build(),
            duration: meter
                .f64_histogram("moat.s3.duration")
                .with_description("Moat API call duration in seconds")
                .with_unit("second")
                .with_boundaries(vec![
                    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 5.0, 10.0,
                ])
                .build(),
        }
    }

    pub fn labels(
        operation: impl Into<Cow<'static, str>>,
        status: impl Into<Cow<'static, str>>,
        ctx: impl Into<Cow<'static, str>>,
    ) -> [KeyValue; 3] {
        [
            KeyValue::new("operation", operation.into()),
            KeyValue::new("status", status.into()),
            KeyValue::new("ctx", ctx.into()),
        ]
    }

    pub const OPERATION_GET_OBJECT: &'static str = "GetObject";

    pub const STATUS_OK: &'static str = "ok";

    pub const CTX_NONE: &'static str = "none";
    pub const CTX_CACHED: &'static str = "cached";
    pub const CTX_FETCHED: &'static str = "fetched";
    pub const CTX_PROXIED: &'static str = "proxied";
    pub const CTX_S3: &'static str = "s3";
}

#[derive(Debug)]
pub struct S3MetricsGuard {
    operation: Cow<'static, str>,
    status: Cow<'static, str>,
    ctx: Cow<'static, str>,
    count: u64,
    bytes: u64,
    start: Instant,
}

impl Drop for S3MetricsGuard {
    fn drop(&mut self) {
        let metrics = Metrics::global();
        metrics.s3.count.add(
            self.count,
            &S3Metrics::labels(self.operation.clone(), self.status.clone(), self.ctx.clone()),
        );
        metrics.s3.bytes.add(
            self.bytes,
            &S3Metrics::labels(self.operation.clone(), self.status.clone(), self.ctx.clone()),
        );
        metrics.s3.duration.record(
            self.start.elapsed().as_secs_f64(),
            &S3Metrics::labels(self.operation.clone(), self.status.clone(), self.ctx.clone()),
        );
    }
}

impl S3MetricsGuard {
    pub fn new(
        operation: impl Into<Cow<'static, str>>,
        status: impl Into<Cow<'static, str>>,
        ctx: impl Into<Cow<'static, str>>,
        count: u64,
        bytes: u64,
    ) -> Self {
        let operation = operation.into();
        let status = status.into();
        let ctx = ctx.into();
        let start = Instant::now();
        S3MetricsGuard {
            operation,
            status,
            ctx,
            count,
            bytes,
            start,
        }
    }

    #[expect(unused)]
    pub fn status(&mut self, status: impl Into<Cow<'static, str>>) -> &mut Self {
        self.status = status.into();
        self
    }

    pub fn ctx(&mut self, ctx: impl Into<Cow<'static, str>>) -> &mut Self {
        self.ctx = ctx.into();
        self
    }

    pub fn add_bytes(&mut self, bytes: u64) -> &mut Self {
        self.bytes += bytes;
        self
    }
}
