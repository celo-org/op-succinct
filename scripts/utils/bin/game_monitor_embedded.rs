use anyhow::Result;
use clap::Parser;
use op_succinct_host_utils::build_env_filter;
use op_succinct_scripts::game_monitor_embedded::EmbeddedArgs;
use tikv_jemallocator::Jemalloc;
use tracing_subscriber::{fmt, prelude::*};

// jemalloc (over the default glibc allocator) so freed memory is returned to the OS promptly
// instead of being stranded in per-thread arenas. This keeps process RSS tracking live usage,
// which the memory-admission sampler depends on: with glibc, RSS stayed at its high-water mark
// between units and corrupted the learned per-gas cost. The aggressive decay config
// (`background_thread:true,dirty_decay_ms:0,muzzy_decay_ms:0`) is baked in at build time via
// `JEMALLOC_SYS_WITH_MALLOC_CONF` in Dockerfile.game-monitor-embedded.
#[global_allocator]
static ALLOCATOR: Jemalloc = Jemalloc;

fn init_tracing() {
    // Output format is chosen by `LOG_FORMAT`: the default is a human-readable text format
    // (span context like `game{index=..}:range{start=..}:` is printed inline before each
    // message); `LOG_FORMAT=json` opts back into structured JSON for log aggregation.
    //
    // The filter is the shared `build_env_filter` (utils/host/src/logger.rs): an `info`
    // default with the noisy kona/sp1 internal modules turned down, `RUST_LOG` layered on
    // top. Reused rather than duplicated so suppression lives in one place, matching the
    // other host binaries (e.g. cost_estimator).
    //
    // Either way `.init()` installs the `log` -> `tracing` bridge (a `LogTracer`) because the
    // `tracing-subscriber` `tracing-log` feature is enabled (see scripts/utils/Cargo.toml),
    // so `log::`-based library code (kona, alloy, …) is captured without a manual
    // `LogTracer::init()` — calling that here as well would double-install and panic.
    let filter = build_env_filter;
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
