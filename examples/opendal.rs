use clap::Parser;
use opendal::{Operator, layers::LoggingLayer, services::S3};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
struct Args {
    #[clap(long)]
    endpoint: String,
    #[clap(long)]
    access_key_id: String,
    #[clap(long)]
    secret_access_key: String,
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
        .bucket(&args.bucket)
        .region(&args.region)
        .access_key_id(&args.access_key_id)
        .secret_access_key(&args.secret_access_key);

    let op = Operator::new(builder)?.layer(LoggingLayer::default()).finish();

    op.write("obj-1", "Hello, OpenDAL!").await?;

    let mut payload = vec![];
    for i in 0u64..(16 * 1024 * 1024 / 16) {
        payload.append(&mut format!("{i:016x}").into_bytes());
    }
    op.write("obj-2", payload).await?;

    let res = op.list("/").await?;
    tracing::info!(?res, "list");

    Ok(())
}
