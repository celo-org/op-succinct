use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use anyhow::{Context, Result};
use clap::Parser;
use fault_proof::contract::{
    DisputeGameFactory::DisputeGameFactoryInstance, OPSuccinctFaultDisputeGame,
};
use log::{error, info, warn};
use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio::time::sleep;

const GAME_TYPE: u32 = 42;
const MAX_RETRIES: u32 = 3;
/// Kill a process if its log file is this many times larger than the median of peers.
const LOG_VOLUME_KILL_MULTIPLIER: f64 = 10.0;
/// Minimum number of running processes required to perform log volume comparison.
const LOG_VOLUME_MIN_PEERS: usize = 3;
/// Kill a process if its time-per-block exceeds this multiplier of the median of completed
/// processes.
const RUNTIME_ANOMALY_MULTIPLIER: f64 = 5.0;

/// Arguments for the game monitor.
#[derive(Debug, Clone, Parser)]
pub struct GameMonitorArgs {
    /// The environment file to use. This file should contain the following environment variables:
    ///
    /// - DISPUTE_GAME_FACTORY_ADDRESS: The address of the dispute game factory contract.
    ///
    /// - L1_RPC: The URL of the L1 RPC endpoint.
    ///
    /// - L1_BEACON_RPC: The URL of the L1 beacon RPC endpoint.
    ///
    /// - L2_RPC: The URL of the L2 RPC endpoint.
    ///
    /// - L2_NODE_RPC: The URL of the L2 node RPC endpoint.
    ///
    /// - EIGENDA_PROXY_ADDRESS: The address of the eigenda proxy service.
    ///
    /// - OP_SUCCINCT_MOCK: Must be 'true'
    ///
    /// - SP1_PROVER: Must be 'mock'
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,

    /// The polling interval in seconds.
    #[arg(long, default_value = "30")]
    pub poll_interval: u64,

    /// Maximum number of concurrent cost estimator processes.
    #[arg(long, default_value = "5")]
    pub max_concurrent: usize,

    /// The path to the cost estimator binary.
    #[arg(long, default_value = "cost-estimator")]
    pub cost_estimator_binary_path: PathBuf,

    /// The directory under which to store the logs.
    #[arg(long, default_value = "logs")]
    pub logs_dir: PathBuf,

    /// Maximum total size in megabytes for the logs directory. When this limit
    /// is exceeded, the oldest log files (by game index) are deleted until the
    /// total size is within the limit. Log files for currently running processes
    /// are never deleted. A value of 0 disables the limit.
    #[arg(long, default_value = "0")]
    pub max_logs_size_mb: u64,

    // The index of the game to start checking from. If unset the monitor will
    #[arg(long, default_value = None)]
    pub start_index: Option<u64>,

    /// The time in seconds to wait between discovering a game index and fetching its details
    /// from L1/L2. This delay mitigates node-desync issues that occur when accessing L1 or L2
    /// via a proxy with multiple backends (e.g. gameCount sees a game on one backend but
    /// gameAtIndex fails on another). The default value of 10 minutes should be safe given the
    /// default values used when running op stack nodes.
    #[arg(long, default_value = "600")]
    pub delay: u64,

    /// Maximum duration in seconds before a cost estimator process is killed.
    /// When completion history is available, a relative check (based on time-per-block
    /// vs completed processes) may kill sooner. This value acts as the absolute ceiling.
    #[arg(long, default_value = "10800")]
    pub max_process_duration_secs: u64,
}

/// Represents a running cost estimator process for a game.
struct RunningEstimator {
    started_at: Instant,
    process: Child,
    log_file: PathBuf,
    block_range: u64,
    retries: u32,
    game_address: Address,
    start_block: u64,
    end_block: u64,
}

/// Pre-fetched game details for retrying a killed process.
/// When present, the pending game processing loop skips RPC calls
/// and directly spawns the cost estimator.
struct RetryInfo {
    game_address: Address,
    start_block: u64,
    end_block: u64,
}

/// A game index discovered from the factory, waiting for its delay to elapse before
/// fetching game details and spawning the cost estimator.
struct PendingGame {
    discovered_at: Instant,
    game_index: u64,
    retries: u32,
    retry_info: Option<RetryInfo>,
}

struct MonitorState {
    running_processes: HashMap<u64, RunningEstimator>,
    pending_games: VecDeque<PendingGame>,
    next_game_index: u64,
    completion_history: Vec<f64>,
    max_process_duration_secs: u64,
}

impl MonitorState {
    fn new(next_game_index: u64, max_process_duration_secs: u64) -> Self {
        Self {
            running_processes: HashMap::new(),
            pending_games: VecDeque::new(),
            next_game_index,
            completion_history: Vec::new(),
            max_process_duration_secs,
        }
    }

    /// Clean up finished processes and return their results.
    fn cleanup_finished_processes(&mut self) {
        // Pre-compute log sizes (needed for log volume anomaly check).
        // We do this before iter_mut to avoid borrow issues.
        let log_sizes: Vec<(u64, f64)> = self
            .running_processes
            .iter()
            .map(|(id, est)| {
                let size = fs::metadata(&est.log_file).map(|m| m.len()).unwrap_or(0);
                (*id, size as f64)
            })
            .collect();

        // Compute median log size (only meaningful with enough peers).
        let median_log_size: Option<f64> = if log_sizes.len() >= LOG_VOLUME_MIN_PEERS {
            let sizes: Vec<f64> = log_sizes.iter().map(|(_, s)| *s).collect();
            median(&sizes)
        } else {
            None
        };

        // Compute median time-per-block from historical completions.
        let median_tpb: Option<f64> = median(&self.completion_history);

        // Classify each running process. We use an enum to avoid nested borrows.
        enum ProcessAction {
            Success { tpb: Option<f64> },
            Kill { reason: String },
            Retry { reason: String },
        }

        let mut process_actions: Vec<(u64, ProcessAction)> = Vec::new();

        for (id, estimator) in self.running_processes.iter_mut() {
            let elapsed = estimator.started_at.elapsed();
            let elapsed_secs = elapsed.as_secs_f64();

            match estimator.process.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        let tpb = if estimator.block_range > 0 {
                            Some(elapsed_secs / estimator.block_range as f64)
                        } else {
                            None
                        };
                        info!(
                            "Cost estimator {} completed successfully, log file: {}",
                            id,
                            estimator.log_file.display(),
                        );
                        process_actions.push((*id, ProcessAction::Success { tpb }));
                    } else {
                        error!(
                            "Cost estimator {} failed with status {:?}, log file: {}",
                            id,
                            status,
                            estimator.log_file.display(),
                        );
                        process_actions.push((
                            *id,
                            ProcessAction::Retry { reason: format!("exit status {:?}", status) },
                        ));
                    }
                }
                Ok(None) => {
                    // Process still running — check watchdog heuristics.
                    let mut kill_reason: Option<String> = None;

                    // Heuristic 1: Absolute timeout.
                    if elapsed.as_secs() > self.max_process_duration_secs {
                        kill_reason = Some(format!(
                            "exceeded maximum duration of {}s",
                            self.max_process_duration_secs
                        ));
                    }

                    // Heuristic 2: Runtime anomaly (time-per-block vs history).
                    if kill_reason.is_none() && estimator.block_range > 0 {
                        if let Some(med_tpb) = median_tpb {
                            let current_tpb = elapsed_secs / estimator.block_range as f64;
                            if current_tpb > RUNTIME_ANOMALY_MULTIPLIER * med_tpb {
                                kill_reason = Some(format!(
                                    "time per block ({:.1}s) exceeds {:.0}x median ({:.1}s)",
                                    current_tpb, RUNTIME_ANOMALY_MULTIPLIER, med_tpb
                                ));
                            }
                        }
                    }

                    // Heuristic 3: Log volume anomaly.
                    if kill_reason.is_none() {
                        if let Some(med_log) = median_log_size {
                            let this_size = log_sizes
                                .iter()
                                .find(|(lid, _)| *lid == *id)
                                .map(|(_, s)| *s)
                                .unwrap_or(0.0);
                            if this_size > LOG_VOLUME_KILL_MULTIPLIER * med_log {
                                kill_reason = Some(format!(
                                    "log size ({:.1} MB) exceeds {:.0}x median ({:.1} MB)",
                                    this_size / (1024.0 * 1024.0),
                                    LOG_VOLUME_KILL_MULTIPLIER,
                                    med_log / (1024.0 * 1024.0)
                                ));
                            }
                        }
                    }

                    if let Some(reason) = kill_reason {
                        error!(
                            "Cost estimator {} is out of control ({}), log file: {}. Killing it.",
                            id,
                            reason,
                            estimator.log_file.display()
                        );
                        process_actions.push((*id, ProcessAction::Kill { reason }));
                    }
                }
                Err(e) => {
                    error!("Error checking process {}: {}", id, e);
                    process_actions.push((
                        *id,
                        ProcessAction::Retry { reason: format!("process check error: {}", e) },
                    ));
                }
            }
        }

        // Process actions (iter_mut borrow is now released).
        let mut history_updates: Vec<f64> = Vec::new();

        for (id, action) in process_actions {
            match action {
                ProcessAction::Success { tpb } => {
                    if let Some(t) = tpb {
                        history_updates.push(t);
                    }
                    self.running_processes.remove(&id);
                }
                ProcessAction::Kill { reason } => {
                    if let Some(mut est) = self.running_processes.remove(&id) {
                        let _ = est.process.kill();
                        if est.retries < MAX_RETRIES {
                            let new_retries = est.retries + 1;
                            warn!(
                                "Re-queuing game {} for retry {}/{} after kill ({})",
                                id, new_retries, MAX_RETRIES, reason
                            );
                            self.pending_games.push_back(PendingGame {
                                discovered_at: Instant::now(),
                                game_index: id,
                                retries: new_retries,
                                retry_info: Some(RetryInfo {
                                    game_address: est.game_address,
                                    start_block: est.start_block,
                                    end_block: est.end_block,
                                }),
                            });
                        } else {
                            error!(
                                "Game {} failed after {} retries ({}), giving up.",
                                id, MAX_RETRIES, reason
                            );
                        }
                    }
                }
                ProcessAction::Retry { reason } => {
                    if let Some(est) = self.running_processes.remove(&id) {
                        if est.retries < MAX_RETRIES {
                            let new_retries = est.retries + 1;
                            warn!(
                                "Re-queuing game {} for retry {}/{} ({})",
                                id, new_retries, MAX_RETRIES, reason
                            );
                            self.pending_games.push_back(PendingGame {
                                discovered_at: Instant::now(),
                                game_index: id,
                                retries: new_retries,
                                retry_info: Some(RetryInfo {
                                    game_address: est.game_address,
                                    start_block: est.start_block,
                                    end_block: est.end_block,
                                }),
                            });
                        } else {
                            error!(
                                "Game {} failed after {} retries ({}), giving up.",
                                id, MAX_RETRIES, reason
                            );
                        }
                    }
                }
            }
        }

        self.completion_history.extend(history_updates);
    }

    /// Check if we can spawn a new process.
    fn can_spawn_new(&self, max_concurrent: usize) -> bool {
        self.running_processes.len() < max_concurrent
    }
}

/// Compute the median of a slice of f64 values. Returns None if the slice is empty.
fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    } else {
        Some(sorted[mid])
    }
}

/// Spawns a cost estimator process for the given block range.
fn spawn_cost_estimator(
    cost_estimator_binary_path: &PathBuf,
    env_file: &Path,
    log_file: &PathBuf,
    start_block: u64,
    end_block: u64,
    batch_size: u64,
) -> Result<Child> {
    let args = [
        "--start",
        &start_block.to_string(),
        "--end",
        &end_block.to_string(),
        "--batch-size",
        &batch_size.to_string(),
        "--env-file",
        env_file.to_str().unwrap(),
    ];

    let cmd = format!("{} {}", cost_estimator_binary_path.display(), args.join(" "));

    // Write command and env to log file to facilitate easy re-running of the command.
    let mut log_file_handle = File::create(log_file)?;
    writeln!(log_file_handle, "=== Cost Estimator Command ===")?;
    writeln!(log_file_handle, "{}", cmd)?;
    writeln!(log_file_handle, "=== Cost Estimator ENV ===")?;
    let relevant_vars = [
        "DISPUTE_GAME_FACTORY_ADDRESS",
        "L1_RPC",
        "L1_BEACON_RPC",
        "L2_RPC",
        "L2_NODE_RPC",
        "EIGENDA_PROXY_ADDRESS",
        "OP_SUCCINCT_MOCK",
        "SP1_PROVER",
    ];
    for var in relevant_vars {
        if let Ok(value) = env::var(var) {
            writeln!(log_file_handle, "{}={}", var, value)?;
        }
    }
    writeln!(log_file_handle, "=== Output ===")?;
    writeln!(log_file_handle)?;

    // Create log file for this specific run
    let stdout_file = log_file_handle.try_clone()?;
    let stderr_file = log_file_handle.try_clone()?;

    info!("Running cost estimator: {}", cmd);
    info!("Logging to: {}", log_file.display());

    let child = Command::new(cost_estimator_binary_path)
        .args(args)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .context("Failed to spawn cost estimator process")?;

    Ok(child)
}

fn extract_game_index(path: &Path) -> Option<u64> {
    let filename = path.file_name()?.to_str()?;
    let stripped = filename.strip_prefix("cost-estimator-")?;
    let dash_pos = stripped.find('-')?;
    stripped[..dash_pos].parse().ok()
}

fn enforce_log_space_limit(
    logs_dir: &Path,
    max_size_bytes: u64,
    running_game_indices: &HashMap<u64, RunningEstimator>,
) -> Result<()> {
    let mut log_files: Vec<(PathBuf, u64, u64)> = Vec::new();
    let mut total_size: u64 = 0;

    for entry in fs::read_dir(logs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let size = entry.metadata()?.len();
            total_size += size;
            // Only consider files matching our naming pattern as deletion
            // candidates.
            if let Some(game_index) = extract_game_index(&path) {
                log_files.push((path, size, game_index));
            }
        }
    }

    if total_size <= max_size_bytes {
        return Ok(());
    }

    info!(
        "Log directory size ({:.2} MB) exceeds limit ({:.2} MB), cleaning up oldest logs",
        total_size as f64 / (1024.0 * 1024.0),
        max_size_bytes as f64 / (1024.0 * 1024.0),
    );

    // Sort by game index ascending (oldest first).
    log_files.sort_by_key(|(_, _, idx)| *idx);

    for (path, size, game_index) in &log_files {
        if total_size <= max_size_bytes {
            break;
        }

        if running_game_indices.contains_key(game_index) {
            continue;
        }

        match fs::remove_file(path) {
            Ok(()) => {
                info!("Deleted log file: {}", path.display());
                total_size -= size;
            }
            Err(e) => {
                warn!("Failed to delete log file {}: {}", path.display(), e);
            }
        }
    }

    if total_size > max_size_bytes {
        warn!(
            "Log directory still exceeds limit after cleanup ({:.2} MB remaining), some files may belong to running processes",
            total_size as f64 / (1024.0 * 1024.0),
        );
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = GameMonitorArgs::parse();

    // Load environment variables
    dotenv::from_path(&args.env_file).ok();
    sp1_sdk::utils::setup_logger();
    info!("Game monitor args: {:?}", args);

    // Create the logs directory if it doesn't exist
    if !args.logs_dir.exists() {
        fs::create_dir_all(&args.logs_dir).context("Failed to create logs directory")?;
    }

    info!("Starting game monitor for game type {}", GAME_TYPE);
    info!("Environment file: {}", args.env_file.display());
    info!("Polling interval: {}s", args.poll_interval);
    info!("Max concurrent processes: {}", args.max_concurrent);
    info!("Cost estimator binary path: {}", args.cost_estimator_binary_path.display());
    if args.max_logs_size_mb > 0 {
        info!("Max logs size: {} MB", args.max_logs_size_mb);
    } else {
        info!("Max logs size: unlimited");
    }

    // Get required environment variables
    let l1_rpc = env::var("L1_RPC").context("L1_RPC not set")?;
    let dispute_game_factory_address = env::var("DISPUTE_GAME_FACTORY_ADDRESS")
        .context("DISPUTE_GAME_FACTORY_ADDRESS not set")?
        .parse::<Address>()
        .context("Invalid DISPUTE_GAME_FACTORY_ADDRESS")?;

    info!("L1 RPC: {}", l1_rpc);
    info!("Dispute Game Factory: {}", dispute_game_factory_address);

    // Set up L1 provider and factory contract
    let l1_provider = ProviderBuilder::new().connect_http(l1_rpc.parse()?);
    let factory =
        DisputeGameFactoryInstance::new(dispute_game_factory_address, l1_provider.clone());

    // If start_index is unset start from the most recent game, or game at index 0 if there are no
    // games. Otherwise use the start_index.
    let next_game_index = match args.start_index {
        Some(index) => index,
        None => {
            let initial_game_count = factory.gameCount().call().await?.to::<u64>();
            match initial_game_count {
                0 => 0,
                n => n - 1,
            }
        }
    };
    let mut state = MonitorState::new(next_game_index, args.max_process_duration_secs);

    let poll_interval = Duration::from_secs(args.poll_interval);
    let delay = Duration::from_secs(args.delay);

    // Main monitoring loop
    'outer: loop {
        sleep(poll_interval).await;
        state.cleanup_finished_processes();

        if args.max_logs_size_mb > 0 {
            if let Err(e) = enforce_log_space_limit(
                &args.logs_dir,
                args.max_logs_size_mb * 1024 * 1024,
                &state.running_processes,
            ) {
                error!("Failed to enforce log space limit: {}", e);
            }
        }

        info!(
            "Running: {}/{}, Pending: {}",
            state.running_processes.len(),
            args.max_concurrent,
            state.pending_games.len()
        );

        // Process pending games whose delay has elapsed: fetch game info and spawn.
        while let Some(pending) = state.pending_games.front() {
            if !state.can_spawn_new(args.max_concurrent) {
                break;
            }
            // Retries have pre-fetched game info and skip the discovery delay.
            if pending.retry_info.is_none() && pending.discovered_at.elapsed() < delay {
                break;
            }

            let game_index = pending.game_index;

            let game_info = match factory.gameAtIndex(U256::from(game_index)).call().await {
                Ok(info) => info,
                Err(e) => {
                    warn!("Failed to get game at index {}: {}. Retrying", game_index, e,);
                    continue 'outer;
                }
            };

            let game_type = game_info.gameType;
            let game_address = game_info.proxy;

            if game_type != GAME_TYPE {
                info!(
                    "Skipping game at index {} (type {} != {})",
                    game_index, game_type, GAME_TYPE
                );
                // Drop the game
                state.pending_games.pop_front();
                continue;
            }

            info!("Processing game {} at index {}", game_address, game_index);

            let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider.clone());

            let l2_block_number = match game.l2BlockNumber().call().await {
                Ok(block) => block.to::<u64>(),
                Err(e) => {
                    warn!(
                        "Failed to get L2 block number for game {} at index {}: {}. Retrying",
                        game_address, game_index, e
                    );
                    continue 'outer;
                }
            };

            let start_block = match game.startingBlockNumber().call().await {
                Ok(block) => block.to::<u64>(),
                Err(e) => {
                    error!(
                        "Failed to get staring block number for game {} at index {}: {}. Retrying.",
                        game_address, game_index, e
                    );
                    continue 'outer;
                }
            };
            let end_block = l2_block_number;

            // Drop the game
            let pending = state.pending_games.pop_front().unwrap();

            info!("Game {} covers L2 blocks {} to {}", game_address, start_block, end_block);

            let log_file = if pending.retries > 0 {
                args.logs_dir.join(format!(
                    "cost-estimator-{}-{}-retry{}.log",
                    game_index, game_address, pending.retries
                ))
            } else {
                args.logs_dir.join(format!("cost-estimator-{}-{}.log", game_index, game_address))
            };

            let child = spawn_cost_estimator(
                &args.cost_estimator_binary_path,
                &args.env_file,
                &log_file,
                start_block,
                end_block,
                end_block - start_block,
            )?;
            info!(
                "Started cost estimator for game {} at index {} (blocks {}-{})",
                game_address, game_index, start_block, end_block
            );
            state.running_processes.insert(
                game_index,
                RunningEstimator {
                    started_at: Instant::now(),
                    process: child,
                    log_file,
                    block_range: end_block.saturating_sub(start_block),
                    retries: pending.retries,
                    game_address,
                    start_block,
                    end_block,
                },
            );
        }

        // Discover new game indices and queue them for deferred processing.
        let current_game_count = match factory.gameCount().call().await {
            Ok(count) => count.to::<u64>(),
            Err(e) => {
                error!(
                    "Failed to Fetch gameCount from factory {}: {}. Retrying",
                    dispute_game_factory_address, e
                );
                continue;
            }
        };
        while state.next_game_index < current_game_count {
            let game_index = state.next_game_index;
            state.next_game_index += 1;

            info!(
                "Discovered new game at index {}, queuing for processing after {:?} delay",
                game_index, delay
            );
            state.pending_games.push_back(PendingGame {
                discovered_at: Instant::now(),
                game_index,
                retries: 0,
                retry_info: None,
            });
        }
    }
}
