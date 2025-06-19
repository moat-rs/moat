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

use std::sync::LazyLock;

use opentelemetry::{
    KeyValue,
    metrics::{Counter, Gauge, Histogram, Meter},
};

use crate::meta::model::{Peer, Role};

#[derive(Debug)]
pub struct Metrics {
    pub api: ApiMetrics,
    pub cluster: ClusterMetrics,
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

        Self { api, cluster }
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

    pub fn labels(api: &str, operation: &str, status: &str) -> [KeyValue; 3] {
        [
            KeyValue::new("api", api.to_string()),
            KeyValue::new("operation", operation.to_string()),
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
