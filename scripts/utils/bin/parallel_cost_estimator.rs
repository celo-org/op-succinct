use alloy_eips::BlockId;
use anyhow::Result;
use clap::Parser;
use log::{info, warn};
use op_succinct_host_utils::{
    block_range::{split_range_basic, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
};
use std::{
    cmp::min,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};
use tokio::{
    process::Command,
    sync::Mutex,
    task::JoinSet,
};

// Number of blocks in 2 weeks (assuming 1 second block time)
// 2 weeks = 14 days * 24 hours * 60 minutes * 60 seconds
const TWO_WEEKS_IN_BLOCKS: u64 = 14 * 24 * 60 * 60;

/// Parallel cost estimator that runs multiple cost_estimator instances concurrently
#[derive(Parser, Debug, Clone)]
#[command(about = "Runs cost estimator for a range of blocks with configurable concurrency")]
pub struct ParallelCostEstimatorArgs {
    /// Starting block number (inclusive). If not provided, uses latest finalized block from L2 RPC.
    #[arg(long)]
    pub from: Option<u64>,
    
    /// Ending block number (exclusive). If not provided, calculates as (from - TWO_WEEKS_IN_BLOCKS).
    #[arg(long)]
    pub to: Option<u64>,
    
    /// Number of blocks in each range to process
    #[arg(long)]
    pub range: u64,
    
    /// Number of concurrent cost_estimator instances to run
    #[arg(long, default_value = "4")]
    pub concurrency: usize,
    
    /// The number of blocks to execute in a single batch (passed to cost_estimator)
    #[arg(long, default_value = "10")]
    pub batch_size: u64,
    
    /// Use cached witness generation (passed to cost_estimator)
    #[arg(long)]
    pub use_cache: bool,
    
    /// Use a fixed recent range (passed to cost_estimator)
    #[arg(long)]
    pub rolling: bool,
    
    /// The number of blocks to use for the default range (passed to cost_estimator)
    #[arg(long, default_value = "5")]
    pub default_range: u64,
    
    /// The environment file to use (passed to cost_estimator)
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    
    /// Whether to generate proofs (passed to cost_estimator)
    #[arg(long)]
    pub prove: bool,
    
    /// Whether to fallback to timestamp-based L1 head estimation (passed to cost_estimator)
    #[arg(long)]
    pub safe_db_fallback: bool,
    
    /// Process ranges and batches in reverse order (from highest to lowest block)
    #[arg(long)]
    pub reverse: bool,
    
    /// Skip writing CSV files and only log execution statistics (passed to cost_estimator)
    #[arg(long, default_value = "true")]
    pub log_only: bool,
}

/// Statistics tracker for parallel execution
#[derive(Debug, Default)]
struct ExecutionTracker {
    completed: usize,
    failed: usize,
    total: usize,
}

impl ExecutionTracker {
    fn new(total: usize) -> Self {
        Self {
            completed: 0,
            failed: 0,
            total,
        }
    }
    
    fn mark_completed(&mut self) {
        self.completed += 1;
        info!(
            "Progress: {}/{} completed, {} failed",
            self.completed, self.total, self.failed
        );
    }
    
    fn mark_failed(&mut self) {
        self.failed += 1;
        warn!(
            "Progress: {}/{} completed, {} failed",
            self.completed, self.total, self.failed
        );
    }
}

/// Run a single cost_estimator instance for the given range
async fn run_cost_estimator(
    range: SpanBatchRange,
    args: &ParallelCostEstimatorArgs,
) -> Result<SpanBatchRange> {
    info!("Starting cost_estimator for blocks {} to {}", range.start, range.end);
    
    let cargo_metadata = cargo_metadata::MetadataCommand::new().exec()?;
    let workspace_root = PathBuf::from(cargo_metadata.workspace_root);
    
    let mut cmd = Command::new("cargo");
    let batch_size = min(args.batch_size, range.end - range.start);
    cmd.current_dir(&workspace_root)
        .arg("run")
        .arg("--release")
        .arg("--bin")
        .arg("cost-estimator")
        .arg("--")
        .arg("--start")
        .arg(range.start.to_string())
        .arg("--end")
        .arg(range.end.to_string())
        .arg("--batch-size")
        .arg(batch_size.to_string())
        .arg("--default-range")
        .arg(args.default_range.to_string())
        .arg("--reverse")
        .arg(args.reverse.to_string())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    
    if args.use_cache {
        cmd.arg("--use-cache");
    }
    
    if args.rolling {
        cmd.arg("--rolling");
    }
    
    if args.prove {
        cmd.arg("--prove");
    }
    
    if args.safe_db_fallback {
        cmd.arg("--safe-db-fallback");
    }
    
    if args.log_only {
        cmd.arg("--log-only");
    }
    
    let status = cmd.status().await?;
    
    if status.success() {
        info!("Completed cost_estimator for blocks {} to {}", range.start, range.end);
        Ok(range)
    } else {
        anyhow::bail!(
            "cost_estimator failed for blocks {} to {} with exit code: {:?}",
            range.start,
            range.end,
            status.code()
        )
    }
}

/// Process all ranges with controlled concurrency
async fn process_ranges(
    ranges: Vec<SpanBatchRange>,
    args: &ParallelCostEstimatorArgs,
) -> Result<()> {
    let total_ranges = ranges.len();
    info!(
        "Processing {} ranges with concurrency of {}",
        total_ranges, args.concurrency
    );
    
    let tracker = Arc::new(Mutex::new(ExecutionTracker::new(total_ranges)));
    let mut handles = JoinSet::new();
    let mut range_iter = ranges.into_iter();
    
    // Spawn initial batch of tasks
    for _ in 0..args.concurrency {
        if let Some(range) = range_iter.next() {
            let args = args.clone();
            let tracker = tracker.clone();
            
            handles.spawn(async move {
                let result = run_cost_estimator(range.clone(), &args).await;
                let mut tracker = tracker.lock().await;
                
                match &result {
                    Ok(_) => tracker.mark_completed(),
                    Err(e) => {
                        warn!("Failed to process range {:?}: {}", range, e);
                        tracker.mark_failed();
                    }
                }
                
                result
            });
        }
    }
    
    // Process results and spawn new tasks as slots become available
    while let Some(result) = handles.join_next().await {
        match result {
            Ok(Ok(_)) => {
                // Task completed successfully, spawn next task if available
                if let Some(range) = range_iter.next() {
                    let args = args.clone();
                    let tracker = tracker.clone();
                    
                    handles.spawn(async move {
                        let result = run_cost_estimator(range.clone(), &args).await;
                        let mut tracker = tracker.lock().await;
                        
                        match &result {
                            Ok(_) => tracker.mark_completed(),
                            Err(e) => {
                                warn!("Failed to process range {:?}: {}", range, e);
                                tracker.mark_failed();
                            }
                        }
                        
                        result
                    });
                }
            }
            Ok(Err(e)) => {
                // Task failed, but continue with remaining tasks
                warn!("Task execution error: {}", e);
                
                // Spawn next task if available
                if let Some(range) = range_iter.next() {
                    let args = args.clone();
                    let tracker = tracker.clone();
                    
                    handles.spawn(async move {
                        let result = run_cost_estimator(range.clone(), &args).await;
                        let mut tracker = tracker.lock().await;
                        
                        match &result {
                            Ok(_) => tracker.mark_completed(),
                            Err(e) => {
                                warn!("Failed to process range {:?}: {}", range, e);
                                tracker.mark_failed();
                            }
                        }
                        
                        result
                    });
                }
            }
            Err(e) => {
                // Task panicked
                warn!("Task panicked: {}", e);
            }
        }
    }
    
    let final_tracker = tracker.lock().await;
    info!(
        "All tasks completed. Success: {}, Failed: {}, Total: {}",
        final_tracker.completed, final_tracker.failed, final_tracker.total
    );
    
    if final_tracker.failed > 0 {
        anyhow::bail!(
            "{} out of {} ranges failed to process",
            final_tracker.failed,
            final_tracker.total
        );
    }
    
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();
    
    let args = ParallelCostEstimatorArgs::parse();
    
    let from_block = if args.from.is_none() {
        // Only to provided, fetch from from L2 RPC
        info!("'from' not provided, fetching latest finalized block from L2 RPC...");
        dotenv::from_path(&args.env_file).ok();
        let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
        let finalized_header = data_fetcher.get_l2_header(BlockId::finalized()).await?;
        let from = finalized_header.number;
        info!("Using latest finalized block: {}", from);
        from
    } else {
        args.from.unwrap()
    };
    let to_block = if args.to.is_none() {
        // Only from provided, calculate to as from - TWO_WEEKS_IN_BLOCKS
        let to = from_block.saturating_sub(TWO_WEEKS_IN_BLOCKS);
        info!("'to' not provided, using from ({}) - TWO_WEEKS_IN_BLOCKS = {}", from_block, to);
        to
    } else {
        args.to.unwrap()
    };

    // Validate arguments
    if from_block <= to_block {
        anyhow::bail!(
            "'from' block ({}) must be > 'to' block ({}). Note: 'to' is the older block when processing backwards.",
            from_block, to_block
        );
    }
    
    if args.range == 0 {
        anyhow::bail!("'range' must be greater than 0");
    }
    
    if args.concurrency == 0 {
        anyhow::bail!("'concurrency' must be greater than 0");
    }
    
    info!(
        "Starting parallel cost estimator for blocks {} to {} with range size {} and concurrency {}",
        from_block, to_block, args.range, args.concurrency
    );
    
    // Split the overall range into sub-ranges
    let mut ranges = split_range_basic(to_block, from_block, args.range);
    if args.reverse {
        ranges.reverse();
    }
    
    info!("Split into {} ranges: {:?}", ranges.len(), ranges);
    
    // Process all ranges with controlled concurrency
    process_ranges(ranges, &args).await?;
    
    info!("Parallel cost estimator completed successfully!");
    
    Ok(())
}

