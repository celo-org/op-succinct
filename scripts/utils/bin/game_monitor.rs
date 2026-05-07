use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fault_proof::contract::{
    DisputeGameFactory::DisputeGameFactoryInstance, OPSuccinctFaultDisputeGame,
};
use log::{debug, error, info, warn};
use op_succinct_common::SequenceTracker;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    signal::unix::{signal, SignalKind},
    time::sleep,
};

const GAME_TYPE: u32 = 42;
/// Kill a process if its log file is this many times larger than the median of peers.
const LOG_VOLUME_KILL_MULTIPLIER: f64 = 10.0;
/// Minimum number of completion history entries required to perform median comparison.
const MEDIAN_THRESHOLD: usize = 3;
/// Kill a process if its time-per-block exceeds this multiplier of the median of completed
/// processes.
const RUNTIME_KILL_MULTIPLIER: f64 = 5.0;

/// Top-level CLI for the game-monitor binary.
///
/// The binary historically exposed a single mode (the long-running daemon). It now multiplexes
/// between subcommands so that one-shot operational tools can ship in the same image. New
/// subcommands should be added to [`CliCommand`] rather than overloading [`RunArgs`].
#[derive(Debug, Clone, Parser)]
#[command(version, about = "Game monitor daemon and operational tools")]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

/// Subcommands exposed by the game-monitor binary.
///
/// Named `CliCommand` rather than `Command` to avoid a clash with [`std::process::Command`],
/// which is used elsewhere in this binary to spawn cost-estimator child processes.
#[derive(Debug, Clone, Subcommand)]
pub enum CliCommand {
    /// Run the game monitor daemon (default behaviour prior to the subcommand split).
    Run(RunArgs),
    /// Read an existing `progress.json`, generate `BackgroundRetry` entries for the given game
    /// indexes, append them, and print the complete `ProgressState` as JSON to stdout.
    ///
    /// The daemon must be stopped while replacing `progress.json`; on next startup it will
    /// load the updated entries and schedule them through the normal background-retry path.
    GenBackgroundRetry(GenBackgroundRetryArgs),
}

/// Arguments for the game monitor daemon (the `run` subcommand).
#[derive(Debug, Clone, Parser)]
pub struct RunArgs {
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

    /// The index of the game to start checking from. If unset the monitor will start with the most
    /// recently created game.
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

    /// Maximum number of entries retained in the completion history used for anomaly
    /// detection (time-per-block and log-size outliers). Older entries are discarded first.
    #[arg(long, default_value = "50")]
    pub max_history_length: usize,

    /// Path to the completion history file. Defaults to `<logs_dir>/completion_history.json`.
    #[arg(long)]
    pub history_file: Option<PathBuf>,

    /// Maximum number of retries for a failed cost estimator process before giving up.
    #[arg(long, default_value = "1")]
    pub cost_estimator_retries: u32,

    /// Batch size passed to the cost estimator, caps the per-execution chunk size to control SP1
    /// guest memory usage. Note smaller games still execute as a single chunk.
    #[arg(long, default_value = "200")]
    pub batch_size: u64,

    /// Path to the progress file. Defaults to `<logs_dir>/progress.json`.
    #[arg(long)]
    pub progress_file: Option<PathBuf>,

    /// Maximum age in seconds (relative to the L1 game creation timestamp) for background
    /// retries. Once a background-queued game exceeds this age, it is evicted and no further
    /// attempts are made. Default is 3.5 days (302400 seconds).
    #[arg(long, default_value = "302400")]
    pub background_retry_max_age_secs: u64,
}

/// Arguments for the `gen-background-retry` subcommand.
///
/// This subcommand is a one-shot tool that reads an existing `progress.json`, appends newly
/// generated [`BackgroundRetry`] entries, and writes the complete [`ProgressState`] to stdout.
/// Operators can inspect the output and, if satisfied, redirect it to replace the original file.
#[derive(Debug, Clone, Parser)]
pub struct GenBackgroundRetryArgs {
    /// Path to the existing `progress.json` file.
    ///
    /// The file is parsed as a [`ProgressState`]. The new background-retry entries are appended
    /// to whatever is already in `background_retries`, and the full state (including
    /// `last_contiguous`) is emitted to stdout. The original file is never modified.
    #[arg(long)]
    pub progress_file: PathBuf,

    /// L1 RPC URL used to look up each game's on-chain creation timestamp.
    ///
    /// Falls back to the `L1_RPC` environment variable if not provided. This makes it easy to
    /// run the tool inside a deployed pod where the daemon's env is already configured.
    #[arg(long, env = "L1_RPC")]
    pub l1_rpc: String,

    /// Address of the dispute game factory proxy on L1.
    ///
    /// Falls back to the `DISPUTE_GAME_FACTORY_ADDRESS` environment variable if not provided.
    #[arg(long, env = "DISPUTE_GAME_FACTORY_ADDRESS")]
    pub dispute_game_factory_address: Address,

    /// `last_wait` value (in seconds) recorded on every emitted entry.
    ///
    /// This is the wait that the daemon will quadruple on the next failure. Picking a sensible
    /// value matters: the daemon's normal exponential schedule starts at
    /// `initial_game_delay * 2 * max_retries * 4` (about 80 minutes with default settings) and
    /// each subsequent failure multiplies it by 4.
    #[arg(long)]
    pub last_wait_secs: u64,

    /// One or more game indexes to emit background-retry entries for.
    ///
    /// Each index is fetched from the dispute game factory to populate `game_created_at`. The
    /// command fails fast if any index can't be fetched or has the wrong game type, so a typo
    /// is surfaced to the operator rather than silently producing a half-correct list.
    #[arg(required = true, num_args = 1..)]
    pub game_indexes: Vec<u64>,
}

/// Distinguishes between primary scheduling (initial attempt + bounded retries on failure) and
/// background scheduling (long-tail retries that run only with spare capacity once primary
/// retries are exhausted).
#[derive(Clone, Copy, Debug)]
enum AttemptKind {
    Primary { retries: u32 },
    Background { attempts: u32 },
}

/// Represents a running cost estimator process for a game.
struct RunningEstimator {
    started_at: Instant,
    process: Child,
    log_file: LogFile,
    block_range: u64,
    kind: AttemptKind,
    /// L1 wall-clock time the game was created. Carried so that on failure we can populate a
    /// `BackgroundRetry` with the correct creation timestamp.
    game_created_at: SystemTime,
}

/// A game index discovered from the factory, once executable_at has passed the game can be
/// executed. The delay before execution helps to reduce infrastructure synchronisation problems,
/// such as what is the latest finalized block, and also provides a mechanism to delay re-execution
/// when there may be some temporary infrastructure outage.
struct PendingGame {
    executable_at: Instant,
    game_index: u64,
    kind: AttemptKind,
}

struct GameData {
    game_index: u64,
    game_address: Address,
    start_block: u64,
    end_block: u64,
    /// L1 wall-clock time at which the game was created on the dispute game factory. Used for
    /// age-based eviction in later commits.
    created_at: SystemTime,
}

impl GameData {
    fn block_range(&self) -> u64 {
        self.end_block.saturating_sub(self.start_block)
    }
}

enum ProcessAction {
    Success { duration: Duration, block_range: u64 },
    Kill { reason: String },
    Retry { reason: String },
}

/// Outcome of a single `spawn_game` invocation. Callers use this to decide whether to keep
/// pulling from their queue or to yield back to the outer poll loop.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
enum SpawnOutcome {
    /// Process started and inserted into `running_processes`. Queue-specific success cleanup
    /// has already been performed inside `spawn_game`.
    Spawned,
    /// The game has the wrong type for this monitor; queue-specific cleanup has been performed.
    WrongGameType,
    /// Fetching game data from L1/L2 failed. Queues are unmodified so the entry will be retried
    /// on the next poll. Callers typically `continue 'outer` to abandon the current iteration.
    FetchFailed,
}

/// Bundles the per-spawn configuration that doesn't change across loop iterations. Built once
/// in `main` and passed by reference to `MonitorState::spawn_game`.
struct SpawnContext<'a, P: alloy_provider::Provider + Clone> {
    factory: &'a DisputeGameFactoryInstance<P>,
    l1_provider: &'a P,
    cost_estimator_binary_path: &'a Path,
    batch_size: u64,
    env_file: &'a Path,
    logs_dir: &'a Path,
}

/// A record of a successfully completed cost estimator process, used for anomaly detection.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CompletionRecord {
    /// Execution duration.
    duration: Duration,
    /// Final log file size in bytes.
    log_size: u64,
    /// Block count
    block_range: u64,
}

/// A long-tail retry for a game whose primary retry budget has been exhausted. Background
/// retries run only with spare capacity and survive process restarts via the persisted
/// `ProgressState`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackgroundRetry {
    game_index: u64,
    /// L1 wall-clock time at which the game was created on the dispute game factory. Used to
    /// enforce the maximum age before eviction.
    game_created_at: SystemTime,
    /// Wall-clock time at which the next attempt becomes eligible. `SystemTime` is used (rather
    /// than `Instant`) so the value survives process restarts.
    next_attempt_at: SystemTime,
    /// Wait used before the most recent attempt; the next wait is `last_wait * 4`.
    last_wait: Duration,
    /// Number of background attempts performed so far for this game.
    attempts: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProgressState {
    last_contiguous: u64,
    #[serde(default)]
    background_retries: Vec<BackgroundRetry>,
}

struct MonitorState {
    running_processes: HashMap<u64, RunningEstimator>,
    pending_games: VecDeque<PendingGame>,
    background_retries: VecDeque<BackgroundRetry>,
    next_game_index: u64,
    initial_game_delay: Duration,
    completion_history: VecDeque<CompletionRecord>,
    max_process_duration_secs: u64,
    max_history_length: usize,
    max_retries: u32,
    history_file: PathBuf,
    sequence_tracker: SequenceTracker,
    progress_file: PathBuf,
    background_retry_max_age: Duration,
}

impl MonitorState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        next_game_index: u64,
        initial_game_delay: Duration,
        max_process_duration_secs: u64,
        max_history_length: usize,
        max_retries: u32,
        history_file: PathBuf,
        progress_file: PathBuf,
        background_retries: VecDeque<BackgroundRetry>,
        background_retry_max_age: Duration,
    ) -> Self {
        let completion_history = Self::load_history(&history_file, max_history_length);
        info!(
            "Loaded {} completion history entries from {}",
            completion_history.len(),
            history_file.display()
        );
        Self {
            running_processes: HashMap::new(),
            pending_games: VecDeque::new(),
            background_retries,
            next_game_index,
            initial_game_delay,
            completion_history,
            max_process_duration_secs,
            max_history_length,
            max_retries,
            history_file,
            sequence_tracker: SequenceTracker::new(next_game_index),
            progress_file,
            background_retry_max_age,
        }
    }

    fn load_progress(path: &Path) -> Option<ProgressState> {
        let data = match fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return None,
        };
        match serde_json::from_str(&data) {
            Ok(state) => Some(state),
            Err(e) => {
                warn!("Failed to parse progress file {}: {}", path.display(), e);
                None
            }
        }
    }

    fn mark_game_completed(&mut self, game_index: u64) {
        let old_end = self.sequence_tracker.end();
        self.sequence_tracker.add(game_index);
        let new_end = self.sequence_tracker.end();
        if new_end != old_end {
            info!("Last contiguous advanced from {} to {}", old_end, new_end);
            self.save_progress();
        }
    }

    fn save_progress(&self) {
        let state = ProgressState {
            last_contiguous: self.sequence_tracker.end(),
            background_retries: self.background_retries.iter().cloned().collect(),
        };
        let data = match serde_json::to_string(&state) {
            Ok(data) => data,
            Err(e) => {
                warn!("Failed to serialize progress state: {}", e);
                return;
            }
        };
        if let Err(e) = atomic_write(&self.progress_file, data.as_bytes()) {
            warn!("Failed to write progress to {}: {}", self.progress_file.display(), e);
        }
    }

    fn load_history(path: &Path, max_length: usize) -> VecDeque<CompletionRecord> {
        let data = match fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return VecDeque::new(),
        };
        let mut records: VecDeque<CompletionRecord> = match serde_json::from_str(&data) {
            Ok(records) => records,
            Err(e) => {
                warn!("Failed to parse completion history from {}: {}", path.display(), e);
                return VecDeque::new();
            }
        };
        while records.len() > max_length {
            records.pop_front();
        }
        records
    }

    fn save_history(&self) {
        match serde_json::to_string(&self.completion_history) {
            Ok(data) => {
                if let Err(e) = fs::write(&self.history_file, data) {
                    warn!(
                        "Failed to write completion history to {}: {}",
                        self.history_file.display(),
                        e
                    );
                }
            }
            Err(e) => {
                warn!("Failed to serialize completion history: {}", e);
            }
        }
    }

    fn push_completion(&mut self, record: CompletionRecord) {
        if self.max_history_length == 0 {
            return;
        }
        if self.completion_history.len() >= self.max_history_length {
            self.completion_history.pop_front();
        }
        self.completion_history.push_back(record);
        self.save_history();
    }

    fn cleanup_finished_processes(&mut self) {
        // Calculate median log size per block
        let lpb_values: Vec<f64> = self
            .completion_history
            .iter()
            .map(|r| r.log_size as f64 / r.block_range as f64)
            .collect();
        let median_lpb: Option<f64> = median(&lpb_values, MEDIAN_THRESHOLD);

        // Calculate median time per block
        let tpb_values: Vec<f64> = self
            .completion_history
            .iter()
            .map(|r| r.duration.as_secs_f64() / r.block_range as f64)
            .collect();
        let median_tpb: Option<f64> = median(&tpb_values, MEDIAN_THRESHOLD);

        let running_log_sizes: HashMap<u64, u64> = self
            .running_processes
            .iter()
            .map(|(id, est)| {
                let size = fs::metadata(&est.log_file.path).map(|m| m.len()).unwrap_or(0);
                (*id, size)
            })
            .collect();

        let mut process_actions: Vec<(u64, ProcessAction)> = Vec::new();

        for (id, estimator) in self.running_processes.iter_mut() {
            let elapsed = estimator.started_at.elapsed();

            match estimator.process.try_wait() {
                Ok(Some(status)) => {
                    let success = status.success();
                    estimator.log_file.mark_complete(success);
                    if success {
                        info!(
                            "Cost estimator {} completed successfully, log file: {}",
                            id,
                            estimator.log_file.path.display(),
                        );
                        process_actions.push((
                            *id,
                            ProcessAction::Success {
                                duration: elapsed,
                                block_range: estimator.block_range,
                            },
                        ));
                    } else {
                        error!(
                            "Cost estimator {} failed with status {:?}, log file: {}",
                            id,
                            status,
                            estimator.log_file.path.display(),
                        );
                        process_actions.push((
                            *id,
                            ProcessAction::Retry { reason: format!("exit status {:?}", status) },
                        ));
                    }
                }
                Ok(None) => {
                    let kill_reason = (|| {
                        if elapsed.as_secs() > self.max_process_duration_secs {
                            return Some(format!(
                                "exceeded maximum duration of {}s",
                                self.max_process_duration_secs
                            ));
                        }
                        if estimator.block_range > 0 {
                            if let Some(med_tpb) = median_tpb {
                                let current_tpb =
                                    elapsed.as_secs_f64() / estimator.block_range as f64;
                                if current_tpb > RUNTIME_KILL_MULTIPLIER * med_tpb {
                                    return Some(format!(
                                        "time per block ({:.1}s) exceeds {:.0}x median ({:.1}s)",
                                        current_tpb, RUNTIME_KILL_MULTIPLIER, med_tpb
                                    ));
                                }
                            }
                            if let Some(med_lpb) = median_lpb {
                                let current_lpb = running_log_sizes.get(id).copied().unwrap_or(0)
                                    as f64 /
                                    estimator.block_range as f64;
                                if current_lpb > LOG_VOLUME_KILL_MULTIPLIER * med_lpb {
                                    return Some(format!(
                                        "log size ({:.1} MB) exceeds {:.0}x median ({:.1} MB)",
                                        current_lpb / (1024.0 * 1024.0),
                                        LOG_VOLUME_KILL_MULTIPLIER,
                                        med_lpb / (1024.0 * 1024.0)
                                    ));
                                }
                            }
                        }
                        None
                    })();

                    if let Some(reason) = kill_reason {
                        estimator.log_file.mark_complete(false);
                        error!(
                            "Cost estimator {} is out of control ({}), log file: {}. Killing it.",
                            id,
                            reason,
                            estimator.log_file.path.display()
                        );
                        process_actions.push((*id, ProcessAction::Kill { reason }));
                    }
                }
                Err(e) => {
                    error!("Error checking process {}: {}", id, e);
                }
            }
        }

        for (id, action) in process_actions {
            match action {
                ProcessAction::Success { duration, block_range } => {
                    if let Some(est) = self.running_processes.remove(&id) {
                        match est.kind {
                            AttemptKind::Primary { .. } => {
                                let log_size =
                                    fs::metadata(&est.log_file.path).map(|m| m.len()).unwrap_or(0);
                                if block_range > 0 {
                                    self.push_completion(CompletionRecord {
                                        duration,
                                        log_size,
                                        block_range,
                                    });
                                }
                                self.mark_game_completed(id);
                            }
                            AttemptKind::Background { attempts } => {
                                // Game already marked completed when it entered the background
                                // queue. Skip mark_game_completed (avoids the SequenceTracker
                                // duplicate-add leak) and skip push_completion (delayed retries
                                // would skew the median-based anomaly detection).
                                info!(
                                    "Background attempt {} for game {} succeeded; removing \
                                     from background queue",
                                    attempts, id
                                );
                                self.background_retries.retain(|bg| bg.game_index != id);
                                self.save_progress();
                            }
                        }
                    }
                }
                ProcessAction::Kill { reason } => {
                    if let Some(mut est) = self.running_processes.remove(&id) {
                        let _ = est.process.kill();
                        self.maybe_requeue(id, est.kind, est.game_created_at, &reason);
                    }
                }
                ProcessAction::Retry { reason } => {
                    if let Some(est) = self.running_processes.remove(&id) {
                        self.maybe_requeue(id, est.kind, est.game_created_at, &reason);
                    }
                }
            }
        }
    }

    fn maybe_requeue(
        &mut self,
        game_index: u64,
        kind: AttemptKind,
        game_created_at: SystemTime,
        reason: &str,
    ) {
        match kind {
            AttemptKind::Primary { retries } if retries < self.max_retries => {
                let new_retries = retries + 1;
                let delay = self.initial_game_delay * 2 * new_retries;
                warn!(
                    "Re-queuing game {} for retry {}/{} ({}) after {:?} delay",
                    game_index, new_retries, self.max_retries, reason, delay
                );
                self.pending_games.push_front(PendingGame {
                    executable_at: Instant::now() + delay,
                    game_index,
                    kind: AttemptKind::Primary { retries: new_retries },
                });
            }
            AttemptKind::Primary { .. } => {
                // Primary retries exhausted: mark the game completed so last_contiguous can
                // advance, and move it onto the background-retry queue for low-priority
                // long-tail attempts. The first background wait is
                // `initial_game_delay * 2 * max_retries * 4`, continuing the primary linear
                // schedule scaled by 4x. `max_retries.max(1)` guards against a 0-length wait
                // when max_retries is configured to 0.
                let last_wait = self.initial_game_delay * 2 * self.max_retries.max(1) * 4;
                let next_attempt_at = SystemTime::now() + last_wait;
                warn!(
                    "Game {} exhausted {} primary retries ({}); moving to background queue with \
                     first attempt in {:?}",
                    game_index, self.max_retries, reason, last_wait
                );
                self.background_retries.push_back(BackgroundRetry {
                    game_index,
                    game_created_at,
                    next_attempt_at,
                    last_wait,
                    attempts: 0,
                });
                self.mark_game_completed(game_index);
                // mark_game_completed only saves when last_contiguous advances; force a save
                // here so the new background entry is persisted regardless.
                self.save_progress();
            }
            AttemptKind::Background { attempts } => {
                // A background attempt failed. Update the existing entry with a 4x-longer wait
                // and bump the attempt counter. The game is already in the sequence tracker
                // from the original primary-retry exhaustion, so we do not call
                // mark_game_completed again.
                let Some(entry) =
                    self.background_retries.iter_mut().find(|bg| bg.game_index == game_index)
                else {
                    warn!(
                        "Background attempt {} for game {} failed ({}) but no background \
                         record was found; dropping",
                        attempts, game_index, reason
                    );
                    return;
                };
                entry.last_wait *= 4;
                entry.next_attempt_at = SystemTime::now() + entry.last_wait;
                entry.attempts = attempts;
                warn!(
                    "Background attempt {} for game {} failed ({}); next attempt in {:?}",
                    attempts, game_index, reason, entry.last_wait
                );
                self.save_progress();
            }
        }
    }

    fn can_spawn_new(&self, max_concurrent: usize) -> bool {
        self.running_processes.len() < max_concurrent
    }

    /// Evict any background-retry entries whose game age (relative to L1 game creation time)
    /// exceeds `background_retry_max_age`. Currently-running entries are left in place; they
    /// will be considered on the next sweep after they finish. Returns the number evicted.
    fn evict_aged_background_retries(&mut self) -> usize {
        let now = SystemTime::now();
        let max_age = self.background_retry_max_age;
        let running = &self.running_processes;
        let before = self.background_retries.len();
        self.background_retries.retain(|bg| {
            // Don't evict an entry whose process is currently running; let it finish.
            if running.contains_key(&bg.game_index) {
                return true;
            }
            let age = now.duration_since(bg.game_created_at).unwrap_or(Duration::ZERO);
            if age > max_age {
                warn!(
                    "Evicting background retry for game {} after {:?} (max age {:?}, \
                     {} attempts)",
                    bg.game_index, age, max_age, bg.attempts
                );
                false
            } else {
                true
            }
        });
        let evicted = before - self.background_retries.len();
        if evicted > 0 {
            self.save_progress();
        }
        evicted
    }

    fn shutdown(&mut self) {
        info!("Shutting down: killing {} running processes", self.running_processes.len());
        for (id, mut est) in self.running_processes.drain() {
            if let Err(e) = est.process.kill() {
                warn!("Failed to kill process for game {}: {}", id, e);
            }
            if let Err(e) = fs::remove_file(&est.log_file.path) {
                warn!("Failed to delete log file {}: {}", est.log_file.path.display(), e);
            }
        }
    }

    /// Fetch game data, spawn a cost estimator, and insert into `running_processes`.
    /// Queue-specific cleanup (on both `WrongGameType` and successful spawn) is performed
    /// internally based on `kind`, so the call sites only have to pick a candidate and react
    /// to the returned outcome.
    async fn spawn_game<P: alloy_provider::Provider + Clone>(
        &mut self,
        game_index: u64,
        kind: AttemptKind,
        ctx: &SpawnContext<'_, P>,
    ) -> Result<SpawnOutcome> {
        let game_data =
            match fetch_game_data(game_index, ctx.factory, ctx.l1_provider.clone()).await {
                Ok(data) => data,
                Err(FetchGameError::WrongGameType { game_index, game_type, expected }) => {
                    debug!(
                        "Skipping game at index {} (type {} != {})",
                        game_index, game_type, expected
                    );
                    self.handle_wrong_game_type(game_index, kind);
                    return Ok(SpawnOutcome::WrongGameType);
                }
                Err(e) => {
                    warn!("Failed to fetch game data for index {}: {:#}. Retrying", game_index, e);
                    return Ok(SpawnOutcome::FetchFailed);
                }
            };

        let log_file =
            LogFile::new(ctx.logs_dir, game_data.game_index, game_data.game_address, kind);
        let child = spawn_cost_estimator(
            ctx.cost_estimator_binary_path,
            ctx.batch_size,
            ctx.env_file,
            &log_file,
            &game_data,
        )?;

        let kind_descr = match kind {
            AttemptKind::Primary { retries: 0 } => "primary".to_string(),
            AttemptKind::Primary { retries } => format!("primary retry {}", retries),
            AttemptKind::Background { attempts } => format!("background attempt {}", attempts),
        };
        info!(
            "Starting cost estimator [{}] for game at index {} address {} (blocks {}-{}, \
             created_at {:?})",
            kind_descr,
            game_data.game_index,
            game_data.game_address,
            game_data.start_block,
            game_data.end_block,
            game_data.created_at,
        );

        self.running_processes.insert(
            game_data.game_index,
            RunningEstimator {
                started_at: Instant::now(),
                process: child,
                log_file,
                block_range: game_data.block_range(),
                kind,
                game_created_at: game_data.created_at,
            },
        );

        // On-success queue-specific cleanup. Background entries stay in `background_retries`
        // while running so `maybe_requeue` can update them on failure.
        if matches!(kind, AttemptKind::Primary { .. }) {
            self.pending_games.retain(|p| p.game_index != game_data.game_index);
        }

        Ok(SpawnOutcome::Spawned)
    }

    /// WrongGameType cleanup, dispatched by `kind`.
    fn handle_wrong_game_type(&mut self, game_index: u64, kind: AttemptKind) {
        match kind {
            AttemptKind::Primary { .. } => {
                self.pending_games.retain(|p| p.game_index != game_index);
                self.mark_game_completed(game_index);
            }
            AttemptKind::Background { .. } => {
                self.background_retries.retain(|bg| bg.game_index != game_index);
                self.save_progress();
            }
        }
    }
}

/// Write `data` to `path` atomically by writing to a temporary sibling file and renaming.
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp_path = match path.file_name() {
        Some(name) => {
            let mut tmp_name = name.to_os_string();
            tmp_name.push(".tmp");
            path.with_file_name(tmp_name)
        }
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no file name",
            ));
        }
    };
    fs::write(&tmp_path, data)?;
    fs::rename(&tmp_path, path)
}

/// Compute the median of a slice of f64 values. Returns if the length of the slice is below the
/// threshold.
fn median(values: &[f64], threshold: usize) -> Option<f64> {
    if values.len() < threshold {
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

#[derive(Debug, thiserror::Error)]
enum FetchGameError {
    #[error("game {game_index} has type {game_type}, expected {expected}")]
    WrongGameType { game_index: u64, game_type: u32, expected: u32 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

async fn fetch_game_data<P: alloy_provider::Provider + Clone>(
    game_index: u64,
    factory: &DisputeGameFactoryInstance<P>,
    l1_provider: P,
) -> Result<GameData, FetchGameError> {
    let game_info = factory
        .gameAtIndex(U256::from(game_index))
        .call()
        .await
        .context("failed to get game at index")?;

    let game_type = game_info.gameType;
    if game_type != GAME_TYPE {
        return Err(FetchGameError::WrongGameType { game_index, game_type, expected: GAME_TYPE });
    }

    let game_address = game_info.proxy;

    let created_at_secs = U256::from(game_info.timestamp).to::<u64>();
    let created_at = UNIX_EPOCH + Duration::from_secs(created_at_secs);

    let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider);

    let l2_block_number =
        game.l2BlockNumber().call().await.context("failed to get L2 block number")?.to::<u64>();

    let start_block = game
        .startingBlockNumber()
        .call()
        .await
        .context("failed to get starting block number")?
        .to::<u64>();

    Ok(GameData { game_index, game_address, start_block, end_block: l2_block_number, created_at })
}

fn spawn_cost_estimator(
    cost_estimator_binary_path: &Path,
    batch_size: u64,
    env_file: &Path,
    log_file: &LogFile,
    game_data: &GameData,
) -> Result<Child> {
    let effective_batch_size = std::cmp::min(batch_size, game_data.block_range()).to_string();
    let args = [
        "--start",
        &game_data.start_block.to_string(),
        "--end",
        &game_data.end_block.to_string(),
        "--batch-size",
        &effective_batch_size,
        "--env-file",
        env_file.to_str().unwrap(),
        "--log-only",
    ];

    let cmd = format!("{} {}", cost_estimator_binary_path.display(), args.join(" "));

    // Write command and env to log file to facilitate easy re-running of the command.
    let mut log_file_handle = File::create(&log_file.path)?;
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
    info!("Logging to: {}", log_file.path.display());

    let child = Command::new(cost_estimator_binary_path)
        .args(args)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .context("Failed to spawn cost estimator process")?;

    Ok(child)
}

struct LogFile {
    path: PathBuf,
}

impl LogFile {
    fn new(logs_dir: &Path, game_index: u64, game_address: Address, kind: AttemptKind) -> Self {
        let path = match kind {
            AttemptKind::Primary { retries: 0 } => {
                logs_dir.join(format!("cost-estimator-{}-{}.log", game_index, game_address))
            }
            AttemptKind::Primary { retries } => logs_dir.join(format!(
                "cost-estimator-{}-{}-retry{}.log",
                game_index, game_address, retries
            )),
            AttemptKind::Background { attempts } => logs_dir.join(format!(
                "cost-estimator-{}-{}-bg-retry{}.log",
                game_index, game_address, attempts
            )),
        };
        Self { path }
    }

    fn extract_game_index(path: &Path) -> Option<u64> {
        let filename = path.file_name()?.to_str()?;
        let stripped = filename.strip_prefix("cost-estimator-")?;
        let dash_pos = stripped.find('-')?;
        stripped[..dash_pos].parse().ok()
    }

    fn mark_complete(&mut self, success: bool) {
        let Some(filename) = self.path.file_name().and_then(|f| f.to_str()) else {
            return;
        };
        let Some(stem) = filename.strip_suffix(".log") else {
            return;
        };
        let suffix = if success { "success" } else { "failure" };
        let new_path = self.path.with_file_name(format!("{}-{}.log", stem, suffix));
        if let Err(e) = fs::rename(&self.path, &new_path) {
            warn!("Failed to rename log {} to {}: {}", self.path.display(), new_path.display(), e);
        } else {
            self.path = new_path;
        }
    }
    fn sizes(logs_dir: &Path) -> Result<Vec<(PathBuf, u64, u64)>> {
        let mut log_files: Vec<(PathBuf, u64, u64)> = Vec::new();
        for entry in fs::read_dir(logs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let size = entry.metadata()?.len();
                // Only consider files matching our naming pattern as deletion
                // candidates.
                if let Some(game_index) = Self::extract_game_index(&path) {
                    log_files.push((path, size, game_index));
                }
            }
        }
        Ok(log_files)
    }
}

fn enforce_log_space_limit(
    max_size_bytes: u64,
    running_game_indices: &HashMap<u64, RunningEstimator>,
    logs_dir: &Path,
) {
    let mut log_files = match LogFile::sizes(logs_dir) {
        Ok(files) => files,
        Err(e) => {
            warn!("Failed to read log sizes for space enforcement: {}", e);
            return;
        }
    };
    let mut total_size: u64 = log_files.iter().map(|t| t.1).sum();

    if total_size <= max_size_bytes {
        return;
    }

    info!(
        "Log directory size ({:.2} MB) exceeds limit ({:.2} MB), cleaning up oldest logs",
        total_size as f64 / (1024.0 * 1024.0),
        max_size_bytes as f64 / (1024.0 * 1024.0),
    );

    // Sort by game index ascending (oldest first).
    log_files.sort_by_key(|(_, _, idx)| *idx);

    for (path, size, game_index) in log_files.iter() {
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
}

/// Entry point. Parses the top-level subcommand and dispatches to the matching handler.
///
/// Logging setup is deferred to each subcommand: the daemon configures `sp1_sdk`'s logger so
/// that operational logs go to stderr, while one-shot tools like `gen-background-retry`
/// deliberately leave logging unconfigured to keep stdout clean for machine-readable output.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Run(args) => run(args).await,
        CliCommand::GenBackgroundRetry(args) => gen_background_retry(args).await,
    }
}

/// Run the long-running game monitor daemon.
///
/// Loads the env file, initialises providers, restores any persisted progress from disk, then
/// enters the poll loop until SIGINT or SIGTERM is received. On shutdown all in-flight
/// cost-estimator processes are killed (their logs are also removed since a partial run is
/// not useful for analysis).
async fn run(args: RunArgs) -> Result<()> {
    // Load environment variables
    dotenv::from_path(&args.env_file).ok();
    sp1_sdk::utils::setup_logger();
    info!("Game monitor args: {:?}", args);

    // Create the logs directory if it doesn't exist
    if !args.logs_dir.exists() {
        fs::create_dir_all(&args.logs_dir).context("Failed to create logs directory")?;
    }

    info!("Starting game monitor for game type {}", GAME_TYPE);

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

    let progress_file =
        args.progress_file.clone().unwrap_or_else(|| args.logs_dir.join("progress.json"));

    let persisted_progress = MonitorState::load_progress(&progress_file);

    let next_game_index = if let Some(index) = args.start_index {
        index
    } else if let Some(progress) = persisted_progress.as_ref() {
        let next = progress.last_contiguous + 1;
        info!("Resuming from persisted last contiguous {}", progress.last_contiguous);
        next
    } else {
        let initial_game_count =
            factory.gameCount().call().block(BlockId::finalized()).await?.to::<u64>();
        match initial_game_count {
            0 => 0,
            n => n - 1,
        }
    };

    let background_retries: VecDeque<BackgroundRetry> =
        persisted_progress.map(|p| p.background_retries.into_iter().collect()).unwrap_or_default();
    info!("Loaded {} persisted background retries", background_retries.len());

    let delay = Duration::from_secs(args.delay);

    let history_file =
        args.history_file.clone().unwrap_or_else(|| args.logs_dir.join("completion_history.json"));
    let mut state = MonitorState::new(
        next_game_index,
        delay,
        args.max_process_duration_secs,
        args.max_history_length,
        args.cost_estimator_retries,
        history_file,
        progress_file,
        background_retries,
        Duration::from_secs(args.background_retry_max_age_secs),
    );

    // Run an eviction sweep up front to discard anything that aged out while the process was
    // down.
    state.evict_aged_background_retries();

    let poll_interval = Duration::from_secs(args.poll_interval);

    let mut sigterm =
        signal(SignalKind::terminate()).context("Failed to register SIGTERM handler")?;
    let mut sigint =
        signal(SignalKind::interrupt()).context("Failed to register SIGINT handler")?;

    let spawn_ctx = SpawnContext {
        factory: &factory,
        l1_provider: &l1_provider,
        cost_estimator_binary_path: args.cost_estimator_binary_path.as_path(),
        batch_size: args.batch_size,
        env_file: args.env_file.as_path(),
        logs_dir: args.logs_dir.as_path(),
    };

    'outer: loop {
        tokio::select! {
            _ = sleep(poll_interval) => {}
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
                break;
            }
        }

        state.cleanup_finished_processes();

        state.evict_aged_background_retries();

        if args.max_logs_size_mb > 0 {
            enforce_log_space_limit(
                args.max_logs_size_mb * 1024 * 1024,
                &state.running_processes,
                &args.logs_dir,
            );
        }

        info!(
            "Running: {}/{}, Pending: {}, Background pending: {}",
            state.running_processes.len(),
            args.max_concurrent,
            state.pending_games.len(),
            state.background_retries.len(),
        );

        let current_time = Instant::now();
        // Primary phase: drain pending_games entries whose delay has elapsed.
        while state.can_spawn_new(args.max_concurrent) {
            let Some((game_index, kind)) = state
                .pending_games
                .iter()
                .find(|p| p.executable_at <= current_time)
                .map(|p| (p.game_index, p.kind))
            else {
                break; // nothing ready in the queue
            };
            if state.spawn_game(game_index, kind, &spawn_ctx).await? == SpawnOutcome::FetchFailed {
                continue 'outer;
            }
        }

        // Background phase. Only consume slots if the primary queue has nothing currently
        // executable, so primary scheduling always wins on contention. Entries stay in
        // background_retries while running so maybe_requeue can update them on failure;
        // cleanup_finished_processes removes them on success.
        let primary_has_ready =
            state.pending_games.iter().any(|p| p.executable_at <= Instant::now());
        if !primary_has_ready {
            while state.can_spawn_new(args.max_concurrent) {
                let now_sys = SystemTime::now();
                let Some((game_index, kind)) = state
                    .background_retries
                    .iter()
                    .find(|bg| {
                        bg.next_attempt_at <= now_sys &&
                            !state.running_processes.contains_key(&bg.game_index)
                    })
                    .map(|bg| {
                        (bg.game_index, AttemptKind::Background { attempts: bg.attempts + 1 })
                    })
                else {
                    break;
                };
                if state.spawn_game(game_index, kind, &spawn_ctx).await? ==
                    SpawnOutcome::FetchFailed
                {
                    continue 'outer;
                }
            }
        }

        // Discover new game indices and queue them for deferred processing.
        let current_game_count = match factory.gameCount().call().block(BlockId::finalized()).await
        {
            Ok(count) => count.to::<u64>(),
            Err(e) => {
                warn!(
                    "Failed to fetch gameCount from factory {}: {}. Retrying",
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
                game_index, state.initial_game_delay
            );
            state.pending_games.push_front(PendingGame {
                executable_at: Instant::now() + state.initial_game_delay,
                game_index,
                kind: AttemptKind::Primary { retries: 0 },
            });
        }
    }

    state.shutdown();
    Ok(())
}

/// Build a [`BackgroundRetry`] for `game_index` by fetching its on-chain creation timestamp.
///
/// Used by the [`gen_background_retry`] subcommand. Errors propagate from
/// [`fetch_game_data`] including the `WrongGameType` case, so that operators see a clear
/// failure if any of the requested indexes does not correspond to an
/// `OPSuccinctFaultDisputeGame` of the expected `GAME_TYPE`.
///
/// `next_attempt_at` is set to `UNIX_EPOCH` so the entry is immediately eligible the next
/// time the daemon starts. `attempts` is initialised to zero so the daemon's scheduling
/// treats the first run as background attempt 1.
async fn make_background_retry<P: alloy_provider::Provider + Clone>(
    game_index: u64,
    last_wait: Duration,
    factory: &DisputeGameFactoryInstance<P>,
    l1_provider: P,
) -> Result<BackgroundRetry> {
    let game = fetch_game_data(game_index, factory, l1_provider)
        .await
        .with_context(|| format!("failed to fetch game data for index {}", game_index))?;
    Ok(BackgroundRetry {
        game_index,
        game_created_at: game.created_at,
        next_attempt_at: UNIX_EPOCH,
        last_wait,
        attempts: 0,
    })
}

/// Load an existing `progress.json`, generate [`BackgroundRetry`] entries for the supplied
/// game indexes, append them to the existing state, and print the complete [`ProgressState`]
/// as pretty-printed JSON on stdout.
///
/// Example usage (daemon must be stopped first):
///
/// ```sh
/// game-monitor gen-background-retry \
///     --progress-file /logs/progress.json \
///     --last-wait-secs 4800 \
///     12345 12346 > /logs/progress.json.new
/// # inspect the output, then replace the original:
/// mv /logs/progress.json.new /logs/progress.json
/// ```
///
/// All log output is suppressed (no logger is initialised) so that stdout contains only the
/// JSON payload. Errors are returned to `main` and printed via `anyhow` on stderr.
async fn gen_background_retry(args: GenBackgroundRetryArgs) -> Result<()> {
    let data = fs::read_to_string(&args.progress_file)
        .with_context(|| format!("failed to read {}", args.progress_file.display()))?;
    let mut state: ProgressState = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", args.progress_file.display()))?;

    let l1_url = args.l1_rpc.parse().context("invalid L1 RPC URL")?;
    let l1_provider = ProviderBuilder::new().connect_http(l1_url);
    let factory =
        DisputeGameFactoryInstance::new(args.dispute_game_factory_address, l1_provider.clone());
    let last_wait = Duration::from_secs(args.last_wait_secs);

    for game_index in args.game_indexes {
        let entry =
            make_background_retry(game_index, last_wait, &factory, l1_provider.clone()).await?;
        state.background_retries.push(entry);
    }

    let json = serde_json::to_string_pretty(&state)
        .context("failed to serialize progress state to JSON")?;
    println!("{}", json);
    Ok(())
}
