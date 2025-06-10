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
mod meta;
mod server;

use std::net::SocketAddr;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use url::Url;

use crate::{
    meta::model::Identity,
    server::{Moat, MoatConfig},
};

#[derive(Debug, Parser)]
struct Args {
    #[clap(long, default_value = "127.0.0.1:23456")]
    listen: SocketAddr,
    #[clap(long, default_value = "provider")]
    identity: Identity,
    #[clap(long, required = false)]
    bootstrap_peers: Vec<SocketAddr>,

    #[clap(long)]
    s3_endpoint: Url,
    #[clap(long)]
    s3_access_key_id: String,
    #[clap(long)]
    s3_secret_access_key: String,
    #[clap(long)]
    s3_region: String,
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let args = Args::parse();
    tracing::info!(?args, "Start Moat");

    Moat::run(MoatConfig {
        listen: args.listen,
        identity: args.identity,
        bootstrap_peers: args.bootstrap_peers,
        s3_endpoint: args.s3_endpoint,
        s3_access_key_id: args.s3_access_key_id,
        s3_secret_access_key: args.s3_secret_access_key,
        s3_region: args.s3_region,
    });
}
