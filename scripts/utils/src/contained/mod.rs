pub mod discovery;
pub mod state;

use std::path::PathBuf;

use clap::Parser;

/// CLI for the contained game monitor. Preserves the legacy flags and adds the
/// pipeline/cache/memory knobs.
#[derive(Debug, Clone, Parser)]
pub struct ContainedArgs {
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// Main loop polling interval (seconds).
    #[arg(long, default_value = "30")]
    pub poll_interval: u64,
    /// Discover→execute delay (seconds) — avoids the multi-backend 404 race.
    #[arg(long, default_value = "600")]
    pub delay: u64,
    /// Blocks per range — caps SP1 guest memory per execution.
    #[arg(long, default_value = "200")]
    pub batch_size: u64,
    /// Primary-retry budget before a game moves to the background queue.
    #[arg(long, default_value = "1")]
    pub max_retries: u32,
    /// Background-retry max age (seconds). Default 3.5 days.
    #[arg(long, default_value = "302400")]
    pub background_retry_max_age_secs: u64,
    /// Explicit start index (else resume from progress, else latest on-chain).
    #[arg(long)]
    pub start_index: Option<u64>,
    /// Progress file for restart resume.
    #[arg(long)]
    pub progress_file: Option<PathBuf>,
    /// Absolute base dir for the witness/stdin cache.
    #[arg(long, default_value = "/data/op-succinct/cache")]
    pub cache_dir: PathBuf,
    /// Proposer's PROPOSAL_INTERVAL (L2 blocks) — drives window prediction.
    #[arg(long)]
    pub proposal_interval: u64,
    /// Max windows the pipeline may run ahead of the finalized frontier.
    #[arg(long, default_value = "4")]
    pub max_lead_windows: u64,
    /// Grace period (seconds) to keep an stdin blob after its game succeeds.
    #[arg(long, default_value = "3600")]
    pub stdin_grace_secs: u64,
    /// RSS admission safety margin (MiB).
    #[arg(long, default_value = "20480")]
    pub rss_margin_mb: u64,
    /// Per-network-call timeout (seconds).
    #[arg(long, default_value = "120")]
    pub network_call_timeout_secs: u64,
    /// Absolute-duration overrun ceiling (seconds) before admission freezes.
    #[arg(long, default_value = "10800")]
    pub max_process_duration_secs: u64,
}

/// Filled in by Task 21.
pub async fn run(_args: ContainedArgs) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_required_proposal_interval_and_defaults() {
        let args =
            ContainedArgs::parse_from(["game-monitor-contained", "--proposal-interval", "1800"]);
        assert_eq!(args.proposal_interval, 1800);
        assert_eq!(args.delay, 600);
        assert_eq!(args.batch_size, 200);
        assert_eq!(args.max_lead_windows, 4);
        assert_eq!(args.stdin_grace_secs, 3600);
    }
}
