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
