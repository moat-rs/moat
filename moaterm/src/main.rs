use clap::Parser;
use moat::meta::model::Peer;

use crate::app::App;

pub mod app;
pub mod event;
pub mod peer;
pub mod ui;

#[derive(Debug, Parser)]
pub struct Args {
    #[clap(short, long, default_value = "localhost:23456")]
    pub peer: Peer,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let args = Args::parse();

    color_eyre::install()?;
    let terminal = ratatui::init();
    let result = App::new().run(terminal).await;
    ratatui::restore();
    result
}
