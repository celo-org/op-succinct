use anyhow::Result;
use clap::Parser;
use op_succinct_scripts::game_monitor_embedded::EmbeddedArgs;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn init_tracing() {
    // JSON to stdout; capture library `log::` lines via the tracing-log bridge.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer().json().with_current_span(true).with_span_list(true))
        .with(filter)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = EmbeddedArgs::parse();
    dotenv::from_path(&args.env_file).ok();
    init_tracing();
    tracing::info!(?args, "starting embedded game monitor");
    op_succinct_scripts::game_monitor_embedded::run(args).await
}
