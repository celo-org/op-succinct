use clap::Parser;
use op_succinct_host_utils::witness_cache::DEFAULT_CACHE_BASE_DIR;
use std::path::PathBuf;

pub mod config_common;

/// The arguments for the host executable.
#[derive(Debug, Clone, Parser)]
pub struct HostExecutorArgs {
    /// The start block of the range to execute.
    #[arg(long)]
    pub start: Option<u64>,
    /// The end block of the range to execute.
    #[arg(long)]
    pub end: Option<u64>,
    /// The number of blocks to execute in a single batch.
    #[arg(long, default_value = "10")]
    pub batch_size: u64,
    /// Enable caching: load from cache if available, save to cache if not.
    #[arg(long)]
    pub cache: bool,
    /// Base directory under which witness caches are stored. Cache files are written to
    /// `<cache_dir>/<chain_id>/witness-cache/`.
    #[arg(long, default_value = DEFAULT_CACHE_BASE_DIR)]
    pub cache_dir: PathBuf,
    /// Use a fixed recent range.
    #[arg(long)]
    pub rolling: bool,
    /// The number of blocks to use for the default range.
    #[arg(long, default_value = "5")]
    pub default_range: u64,
    /// The environment file to use.
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// Whether to generate proofs.
    #[arg(long)]
    pub prove: bool,
    /// Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not
    /// activated for op-node.
    #[clap(long)]
    pub safe_db_fallback: bool,
    /// Skip writing CSV files and only log execution statistics.
    #[arg(long)]
    pub log_only: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct ConfigArgs {
    /// The environment file to use.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
}
