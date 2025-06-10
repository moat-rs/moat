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

use std::io::Read;

use clap::Parser;
use opendal::{Operator, layers::LoggingLayer, services::S3};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
struct Args {
    #[clap(long)]
    endpoint: String,
    #[clap(long)]
    region: String,
    #[clap(long)]
    bucket: String,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let args = Args::parse();

    let builder = S3::default()
        .endpoint(&args.endpoint)
        .region(&args.region)
        .bucket(&args.bucket)
        .disable_config_load()
        .disable_ec2_metadata()
        .allow_anonymous();

    let op = Operator::new(builder)?.layer(LoggingLayer::default()).finish();

    op.write("obj-1", "Hello, OpenDAL!").await?;

    let mut payload = vec![];
    for i in 0u64..(16 * 1024 * 1024 / 16) {
        payload.append(&mut format!("{i:016x}").into_bytes());
    }
    op.write("obj-2", payload).await?;

    let res = op.list("/").await?;
    tracing::info!(?res, "list");

    let mut s = String::new();
    op.read("obj-1").await?.read_to_string(&mut s)?;
    tracing::info!(data = s, "read obj-1");

    let len = op.stat("obj-2").await?.content_length();
    tracing::info!(len, "stat obj-2");

    op.delete("obj-1").await?;
    op.delete("obj-2").await?;

    let res = op.list("/").await?;
    tracing::info!(?res, "list");

    Ok(())
}
