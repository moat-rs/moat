mod api;
mod aws;
mod server;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::server::{Moat, MoatConfig};

#[derive(Debug, Parser)]
struct Args {
    #[clap(long, default_value = "127.0.0.1:23456")]
    endpoint: String,

    #[clap(long)]
    s3_endpoint: String,
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
        s3_endpoint: args.s3_endpoint,
        s3_access_key_id: args.s3_access_key_id,
        s3_secret_access_key: args.s3_secret_access_key,
        s3_region: args.s3_region,
        endpoint: args.endpoint,
    });
}
