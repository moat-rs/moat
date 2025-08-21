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

mod filler;
mod proxy;

use std::{path::PathBuf, sync::Arc};

use bytes::Bytes;

use foyer::{DirectFsDeviceOptions, Engine, HybridCache, LargeEngineOptions, RuntimeOptions, TokioRuntimeOptions};
use mixtrics::registry::opentelemetry_0_30::OpenTelemetryMetricsRegistry;
use opendal::{Operator, layers::LoggingLayer, services::S3};
use pingora::{
    prelude::*,
    proxy::http_proxy_service_with_name,
    server::{RunArgs, configuration::ServerConf},
};

use crate::{
    api::service::ApiService,
    aws::resigner::{AwsSigV4Resigner, AwsSigV4ResignerConfig},
    config::MoatConfig,
    meta::{
        manager::{Gossip, MetaManager, MetaManagerConfig, Observer},
        model::{MemberList, Role},
    },
    metrics::Metrics,
    runtime::Runtime,
    server::proxy::{Proxy, ProxyConfig},
};

pub struct Moat;

impl Moat {
    pub fn run(config: MoatConfig, runtime: &Runtime) -> anyhow::Result<()> {
        let cache = {
            let config = &config;
            runtime.block_on(async move {
                let registry = Box::new(OpenTelemetryMetricsRegistry::new(opentelemetry::global::meter("foyer")));
                let mut builder = HybridCache::builder()
                    .with_name(config.peer.to_string())
                    .with_metrics_registry(registry)
                    .memory(config.cache.mem.as_u64() as _)
                    // TODO(MrCroxx): Count serialized size?
                    .with_weighter(|path: &Arc<String>, data: &Bytes| path.len() + data.len())
                    .storage(Engine::Large(
                        LargeEngineOptions::new()
                            .with_flushers(config.cache.flushers)
                            .with_reclaimers(config.cache.reclaimers)
                            .with_buffer_pool_size(config.cache.buffer_pool_size.as_u64() as _)
                            .with_recover_concurrency(config.cache.recover_concurrency),
                    ))
                    .with_recover_mode(config.cache.recover_mode)
                    .with_runtime_options(RuntimeOptions::Unified(TokioRuntimeOptions::default()));
                if let Some(dir) = &config.cache.dir {
                    builder = builder.with_device_options(
                        DirectFsDeviceOptions::new(PathBuf::from(dir).join("data"))
                            .with_capacity(config.cache.disk.as_u64() as _)
                            .with_file_size(config.cache.file_size.as_u64() as _)
                            .with_throttle(config.cache.throttle.clone()),
                    );
                }
                builder.build().await
            })
        }?;

        let operator = Operator::new(
            S3::default()
                .endpoint(config.s3_config.endpoint.as_str())
                .region(&config.s3_config.region)
                .bucket(&config.s3_config.bucket)
                .access_key_id(&config.s3_config.access_key_id)
                .secret_access_key(&config.s3_config.secret_access_key)
                .disable_config_load()
                .disable_ec2_metadata(),
        )?
        .layer(LoggingLayer::default())
        .finish();

        let meta_manager = MetaManager::new(MetaManagerConfig {
            role: config.role,
            peer: config.peer.clone(),
            bootstrap_peers: config.bootstrap_peers.clone(),
            peer_eviction_timeout: config.peer_eviction_timeout,
            health_check_timeout: config.health_check_timeout,
            health_check_interval: config.health_check_interval,
            health_check_peers: config.health_check_peers,
            sync_timeout: config.sync_timeout,
            sync_interval: config.sync_interval,
            sync_peers: config.sync_peers,
            weight: config.weight,
            api_prefix: config.api.prefix.clone(),
        });
        match config.role {
            Role::Agent => {
                let observer = Observer::new(runtime.clone(), meta_manager.clone());
                runtime.spawn(async move { observer.run().await });
            }
            Role::Cache => {
                let gossip = Gossip::new(runtime.clone(), meta_manager.clone());
                runtime.spawn(async move { gossip.run().await });
            }
        }

        let api = ApiService::new(&config.api, meta_manager.clone());
        let resigner = AwsSigV4Resigner::new(AwsSigV4ResignerConfig {
            endpoint: config.s3_config.endpoint.clone(),
            region: config.s3_config.region.clone(),
            access_key_id: config.s3_config.access_key_id.clone(),
            secret_access_key: config.s3_config.secret_access_key.clone(),
        });

        let mut conf = ServerConf::new().unwrap();
        conf.grace_period_seconds = Some(0);

        let mut server = Server::new_with_opt_and_conf(None, conf);
        server.bootstrap();

        let s3_host = config.s3_config.endpoint.host_str().unwrap_or("localhost").to_string();
        let s3_port = config
            .s3_config
            .endpoint
            .port()
            .unwrap_or(if config.s3_config.endpoint.scheme() == "https" {
                443
            } else {
                80
            });
        let s3_tls = config.s3_config.endpoint.scheme() == "https";
        let s3_bucket = config.s3_config.bucket.clone();

        let proxy = Proxy::new(ProxyConfig {
            api,
            resigner,
            s3_host,
            s3_port,
            s3_tls,
            s3_bucket,
            role: config.role,
            cache,
            operator,
            meta_manager: meta_manager.clone(),
            peer: config.peer.clone(),
            tls: config.tls,
        });
        let mut service = http_proxy_service_with_name(&server.configuration, proxy, "moat");
        service.add_tcp(&config.listen.to_string());
        server.add_service(service);

        let metrics = Metrics::global();
        metrics.cluster.up.record(1, &[]);
        server.run(RunArgs::default());
        runtime.block_on(async {
            let old = meta_manager.members().await;
            meta_manager.update_cluster_metrics(&old, &MemberList::default());
        });
        metrics.cluster.up.record(0, &[]);

        Ok(())
    }
}
