use anyhow::Result;
use clap::Parser;
use op_succinct_scripts::game_monitor_embedded::EmbeddedArgs;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn init_tracing() {
    // Output format is chosen by `LOG_FORMAT`: the default is a human-readable text format
    // (span context like `game{index=..}:range{start=..}:` is printed inline before each
    // message); `LOG_FORMAT=json` opts back into structured JSON for log aggregation.
    //
    // Either way `.init()` installs the `log` -> `tracing` bridge (a `LogTracer`) because the
    // `tracing-subscriber` `tracing-log` feature is enabled (see scripts/utils/Cargo.toml),
    // so `log::`-based library code (kona, alloy, …) is captured without a manual
    // `LogTracer::init()` — calling that here as well would double-install and panic.
    let filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"));

    if json {
        tracing_subscriber::registry()
            .with(fmt::layer().json().with_current_span(true).with_span_list(true))
            .with(filter())
            .init();
    } else {
        // ANSI off: container stdout is not a TTY, so colour codes would just be noise.
        tracing_subscriber::registry()
            .with(fmt::layer().with_target(true).with_ansi(false))
            .with(filter())
            .init();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = EmbeddedArgs::parse();
    dotenv::from_path(&args.env_file).ok();
    init_tracing();
    tracing::info!(?args, "starting embedded game monitor");
    op_succinct_scripts::game_monitor_embedded::run(args).await
}
