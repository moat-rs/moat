use clap::Parser;
use moat::meta::model::Peer;
use serde::{Deserialize, Serialize};

use crate::app::App;

pub mod app;
pub mod event;
pub mod op;
pub mod ui;

#[derive(Debug, Parser, Serialize, Deserialize)]
struct Args {
    #[clap(short, long)]
    peer: Peer,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let args = Args::parse();

    color_eyre::install()?;
    let terminal = ratatui::init();
    let result = App::new(args.peer).run(terminal).await;
    ratatui::restore();
    result
}
