use anyhow::Result;
use bt_audio_bridge_daemon::{config, controller};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Foreground controller for BT Audio Bridge")]
struct Arguments {
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    controller::run(config::config_path(arguments.config)?).await
}
