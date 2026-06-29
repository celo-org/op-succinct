//! Game monitor daemon and operational tools.
//!
//! Watches for new fault-proof dispute games on L1 and runs a `cost-estimator` child process
//! for each in mock-proving mode. Implements a two-tier retry system:
//!
//! 1. **Primary**: fast retries with linear backoff (bounded by `--cost-estimator-retries`).
//! 2. **Background**: long-tail retries with exponential 4x backoff, only using spare execution
//!    slots, evicted after `--background-retry-max-age-secs`.
//!
//! Running processes are monitored for anomalies (excessive runtime or log volume relative to
//! the median of recent completions) and killed if they exceed configurable multipliers.
//!
//! Progress (`last_contiguous` game index + background retry queue) is persisted to disk so
//! the daemon can resume after restarts.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
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
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    signal::unix::{signal, SignalKind},
    time::sleep,
};

/// The dispute game type we monitor. Games with a different type are skipped.
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

/// On-chain metadata for a single dispute game, fetched from L1 via [`fetch_game_data`].
struct GameData {
    game_index: u64,
    game_address: Address,
    /// L2 block at which the game's execution range starts.
    start_block: u64,
    /// L2 block at which the game's execution range ends.
    end_block: u64,
    /// L1 head the game is anchored to on-chain. Forwarded to the cost estimator so it matches the
    /// proposer instead of looking the L1 head up via the op-node safeDB.
    l1_head: B256,
    /// L1 wall-clock time at which the game was created on the dispute game factory. Used for
    /// age-based eviction of background retries.
    created_at: SystemTime,
}

impl GameData {
    fn block_range(&self) -> u64 {
        self.end_block.saturating_sub(self.start_block)
    }
}

/// Deferred action determined during the process-scan phase of
/// [`MonitorState::cleanup_finished_processes`]. Actions are collected first and applied
/// afterwards to avoid mutating `running_processes` while iterating over it.
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
    /// The L2 finalized block is behind the game's end block, so the cost estimator would have
    /// nothing to execute against. The entry is left in its queue; the caller should skip this
    /// candidate and try the next one (older games in the queue may have lower end blocks the
    /// L2 has already finalised). The next poll will re-check skipped entries.
    L2Behind,
}

/// Bundles the per-spawn configuration that doesn't change across loop iterations. Built once
/// in `main` and passed by reference to `MonitorState::spawn_game`.
struct SpawnContext<'a, P: alloy_provider::Provider + Clone> {
    factory: &'a DisputeGameFactoryInstance<P>,
    l1_provider: &'a P,
    /// L2 (op-geth) provider used to read the finalized head before spawning. Games whose end
    /// block is ahead of the L2 finalized block can't be executed yet and are deferred.
    l2_provider: &'a P,
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

/// The subset of daemon state that survives restarts, serialised to `progress.json`.
///
/// `last_contiguous` is the highest game index such that every index up to and including it
/// has been processed (successfully or with retries exhausted). On startup the daemon resumes
/// from `last_contiguous + 1`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProgressState {
    last_contiguous: u64,
    #[serde(default)]
    background_retries: Vec<BackgroundRetry>,
}

/// The full mutable state of the running daemon. Not serialised directly; the persistable
/// subset is extracted into [`ProgressState`] by [`save_progress`](Self::save_progress).
struct MonitorState {
    /// Currently executing cost-estimator child processes, keyed by game index.
    running_processes: HashMap<u64, RunningEstimator>,
    /// Games waiting for their `executable_at` delay to elapse before spawning.
    pending_games: VecDeque<PendingGame>,
    /// Games whose primary retries are exhausted, awaiting low-priority background attempts.
    background_retries: VecDeque<BackgroundRetry>,
    /// The next game index to discover from the factory (monotonically increasing).
    next_game_index: u64,
    /// Delay applied to newly discovered games and used as the base for retry backoff.
    initial_game_delay: Duration,
    /// Sliding window of recent successful completions for anomaly detection.
    completion_history: VecDeque<CompletionRecord>,
    /// Absolute ceiling on cost-estimator runtime before it is killed.
    max_process_duration_secs: u64,
    /// Max entries in `completion_history`; oldest are evicted when full.
    max_history_length: usize,
    /// Number of primary retries before a game moves to background.
    max_retries: u32,
    history_file: PathBuf,
    /// Tracks out-of-order game completions to compute `last_contiguous`.
    sequence_tracker: SequenceTracker,
    progress_file: PathBuf,
    /// Games older than this (from L1 creation time) are evicted from background retries.
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

    /// Load persisted progress from disk. Returns `None` if the file doesn't exist or can't
    /// be parsed (a warning is logged in the latter case).
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

    /// Record a game as completed. If this causes `last_contiguous` to advance (i.e. there
    /// are no more gaps below this index), progress is persisted to disk.
    fn mark_game_completed(&mut self, game_index: u64) {
        let old_end = self.sequence_tracker.end();
        self.sequence_tracker.add(game_index);
        let new_end = self.sequence_tracker.end();
        if new_end != old_end {
            info!("Last contiguous advanced from {} to {}", old_end, new_end);
            self.save_progress();
        }
    }

    /// Atomically write the current `last_contiguous` and `background_retries` to disk.
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

    /// Load completion history from disk, truncating to `max_length` if it has grown.
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

    /// Atomically write the completion history to disk.
    fn save_history(&self) {
        match serde_json::to_string(&self.completion_history) {
            Ok(data) => {
                if let Err(e) = atomic_write(&self.history_file, data.as_bytes()) {
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

    /// Append a completion record (evicting the oldest if at capacity) and persist to disk.
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

    /// Poll all running processes and handle completions, failures, and anomalies.
    ///
    /// This is the core housekeeping method called at the top of each poll iteration. It:
    /// 1. Computes median time-per-block and log-size-per-block from completion history.
    /// 2. Scans each running process via `try_wait()`:
    ///    - Exited successfully -> `ProcessAction::Success`
    ///    - Exited with error   -> `ProcessAction::Retry`
    ///    - Still running but exceeds duration/time/log anomaly thresholds -> `ProcessAction::Kill`
    /// 3. Applies deferred actions: records completions, kills runaways, re-queues failures.
    ///
    /// Actions are collected into a `Vec` first because we can't mutate `running_processes`
    /// (to remove entries or call `maybe_requeue`) while iterating over it.
    fn cleanup_finished_processes(&mut self) {
        // Calculate median log size per block from completion history for anomaly comparison.
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

        // Snapshot current log file sizes for all running processes so we can compare
        // against the median without re-reading metadata during the scan loop.
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
                        // Classify the failure log before logging so the failure type is
                        // included inline. Grep for `failure type unknown` to find failures
                        // not covered by an existing pattern.
                        let failure_type = detect_failure_type(&estimator.log_file.path);
                        error!(
                            "Cost estimator {} failed with status {:?}, failure type {}, \
                             log file: {}",
                            id,
                            status,
                            failure_type,
                            estimator.log_file.path.display(),
                        );
                        process_actions.push((
                            *id,
                            ProcessAction::Retry { reason: format!("exit status {:?}", status) },
                        ));
                    }
                }
                Ok(None) => {
                    // Process still running. Check kill conditions using a closure that
                    // returns Some(reason) on the first triggered condition.
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
                                let current_log_bytes =
                                    running_log_sizes.get(id).copied().unwrap_or(0) as f64;
                                let current_lpb = current_log_bytes / estimator.block_range as f64;
                                if current_lpb > LOG_VOLUME_KILL_MULTIPLIER * med_lpb {
                                    let median_log_bytes = med_lpb * estimator.block_range as f64;
                                    return Some(format!(
                                        "log size ({:.1} MB, {:.0} bytes/block) exceeds \
                                         {:.0}x median ({:.1} MB, {:.0} bytes/block)",
                                        current_log_bytes / (1024.0 * 1024.0),
                                        current_lpb,
                                        LOG_VOLUME_KILL_MULTIPLIER,
                                        median_log_bytes / (1024.0 * 1024.0),
                                        med_lpb,
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

        // Apply deferred actions now that we're no longer borrowing running_processes.
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

    /// Decide what to do with a failed game based on its `AttemptKind`:
    ///
    /// - **Primary with retries remaining**: re-queue to `pending_games` with linear backoff.
    /// - **Primary with retries exhausted**: mark completed (so `last_contiguous` can advance),
    ///   move to `background_retries` with the first exponential wait.
    /// - **Background**: quadruple `last_wait` on the existing entry for the next attempt.
    fn maybe_requeue(
        &mut self,
        game_index: u64,
        kind: AttemptKind,
        game_created_at: SystemTime,
        reason: &str,
    ) {
        match kind {
            // Primary retry: re-queue with linear backoff (delay * 2 * retry_number).
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

    /// Returns true if there are spare execution slots for new processes.
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

    /// Graceful shutdown: kill all running cost-estimator processes and delete their log files
    /// (incomplete logs are not useful for analysis).
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

        // A game can be posted on L1 before the L2 has actually produced (or finalised) all of
        // the blocks it covers. Running the cost estimator in that state would either fail or
        // produce a misleading result, so we defer until the L2 catches up. The next poll will
        // re-check.
        let l2_finalized_block = match ctx.l2_provider.get_block(BlockId::finalized()).await {
            Ok(Some(block)) => block.header.number,
            Ok(None) => {
                warn!(
                    "L2 finalized block not returned by provider; deferring spawn of game {}",
                    game_data.game_index,
                );
                return Ok(SpawnOutcome::FetchFailed);
            }
            Err(e) => {
                warn!(
                    "Failed to fetch L2 finalized block for game {}: {:#}. Retrying",
                    game_data.game_index, e,
                );
                return Ok(SpawnOutcome::FetchFailed);
            }
        };
        if l2_finalized_block < game_data.end_block {
            warn!(
                "L2 finalized block {} is behind game {} end block {}; deferring spawn until \
                 the L2 catches up",
                l2_finalized_block, game_data.game_index, game_data.end_block,
            );
            return Ok(SpawnOutcome::L2Behind);
        }

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

/// Classification of the cause of a cost-estimator failure, derived from the tail of its
/// failure log. The non-`Unknown` variants all represent transient environmental issues (RPC
/// backend health, missing state, DNS resolution) rather than programmatic faults, so flagging
/// them lets the daemon distinguish "infra was sick" from "the estimator is broken" when
/// primary retries are exhausted.
#[derive(Debug, Clone, Copy)]
enum FailureType {
    /// `HTTP error 503 ... no backend is currently healthy to serve traffic` (JSON-RPC code
    /// `-32011`). The proxy in front of the L1/L2 nodes returned 503 because none of its
    /// backends were healthy.
    NoHealthyBackend,
    /// `No state available for block ...` (JSON-RPC code `-32002`). The RPC node pruned or
    /// never had state for the requested historical block.
    NoStateAvailable,
    /// `distance to target block exceeds maximum proof window` (JSON-RPC code `-32602`). The
    /// requested block is too far from the node's head for an `eth_getProof` call.
    ExceedsProofWindow,
    /// `missing trie node ... is not available` (JSON-RPC code `-32000`). The RPC node is
    /// missing a trie node for the requested state.
    MissingTrieNode,
    /// `Failed to fetch safe head` with a `dns error` cause. The op-node hostname could not be
    /// resolved (typically a transient cluster-DNS hiccup).
    DnsLookupFailure,
    /// `Failed to load genesis time from beacon client` with a `HTTP request failed: error
    /// decoding response body`, typically a temporary failure of the beacon client.
    BeaconClientFailure,
    /// `error sending request for url ...`. A request to an upstream service (L1/L2 RPC, beacon
    /// client, prover, or proxy) failed at the transport layer — connection refused, reset,
    /// timed out, or DNS resolution failed before a higher-level pattern could match. This is a
    /// catch-all for transient connectivity issues that aren't pinned to a specific endpoint by
    /// an earlier, more specific pattern, so it is matched last.
    GenericRequestFailure,
    /// No known pattern matched. Either the log could not be read, or the failure mode is
    /// new/programmatic; callers should treat this as "not a known infrastructure failure"
    /// rather than as a positive signal of a code bug.
    Unknown,
}

/// Short human-readable description suitable for inclusion in a log line. The string is
/// stable and grep-friendly; in particular, [`FailureType::Unknown`] renders as the literal
/// `"unknown"` so an operator can scan logs for failures lacking a classification.
impl std::fmt::Display for FailureType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::NoHealthyBackend => "no healthy RPC backend (HTTP 503)",
            Self::NoStateAvailable => "RPC node has no state for requested block",
            Self::ExceedsProofWindow => "distance to target block exceeds maximum proof window",
            Self::MissingTrieNode => "RPC node missing trie node",
            Self::DnsLookupFailure => "DNS lookup failure",
            Self::BeaconClientFailure => "beacon client failure",
            Self::GenericRequestFailure => "generic upstream request failure",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// Maximum number of trailing bytes scanned by [`detect_failure_type`]. Analysis of historical
/// failure logs shows the canonical error line is within ~100 lines (and ~500 KiB worst-case,
/// when several worker threads panicked before the main thread) of EOF; 1 MiB covers every
/// observed case with margin.
const FAILURE_SCAN_TAIL_BYTES: u64 = 1024 * 1024;

/// Patterns matched against each line of the failure log, in descending order of observed
/// frequency. Each entry is the set of substrings that must all appear in a single line for
/// the classification to apply; matching is intentionally loose (substring rather than full
/// regex) because the chosen anchors are invariant across thread ids, request ids, and
/// addresses. Scoping each match to a single line — rather than the whole tail — prevents
/// false positives where two unrelated log entries happen to contain the constituent
/// substrings of a multi-needle pattern (notably [`FailureType::DnsLookupFailure`]).
const FAILURE_PATTERNS: &[(&[&str], FailureType)] = &[
    (&["no backend is currently healthy to serve traffic"], FailureType::NoHealthyBackend),
    (&["No state available for block"], FailureType::NoStateAvailable),
    (&["distance to target block exceeds maximum proof window"], FailureType::ExceedsProofWindow),
    (&["missing trie node"], FailureType::MissingTrieNode),
    (&["Failed to fetch safe head", "dns error"], FailureType::DnsLookupFailure),
    (&["Temporary failure in name resolution"], FailureType::DnsLookupFailure),
    (
        &[
            "Failed to load genesis time from beacon client",
            "HTTP request failed: error decoding response body",
        ],
        FailureType::BeaconClientFailure,
    ),
    (&["error sending request for url"], FailureType::GenericRequestFailure),
];

/// Classify the tail of a failure log against the known cost-estimator failure patterns.
///
/// The function reads at most [`FAILURE_SCAN_TAIL_BYTES`] from the end of the file and matches
/// each line against [`FAILURE_PATTERNS`], returning the first matching classification.
/// Patterns are derived from a survey of historical failure logs (see the
/// `forno-eu-game-monitor-logs/` corpus).
///
/// Returns [`FailureType::Unknown`] for I/O errors or for any failure mode not covered by the
/// patterns above.
fn detect_failure_type(log_path: &Path) -> FailureType {
    let Ok(mut file) = File::open(log_path) else {
        return FailureType::Unknown;
    };
    let Ok(metadata) = file.metadata() else {
        return FailureType::Unknown;
    };
    let len = metadata.len();
    let start = len.saturating_sub(FAILURE_SCAN_TAIL_BYTES);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return FailureType::Unknown;
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return FailureType::Unknown;
    }
    let tail = String::from_utf8_lossy(&bytes);

    tail.lines().find_map(classify_line).unwrap_or(FailureType::Unknown)
}

/// Match a single log line against [`FAILURE_PATTERNS`] in priority order.
fn classify_line(line: &str) -> Option<FailureType> {
    FAILURE_PATTERNS
        .iter()
        .find(|(patterns, _)| patterns.iter().all(|n| line.contains(n)))
        .map(|(_, ft)| *ft)
}

/// Compute the median of `values`. Returns `None` if fewer than `threshold` entries are
/// available, preventing anomaly detection from triggering on insufficient data.
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

/// Fetch on-chain metadata for a single game: type, address, L2 block range, creation time.
///
/// Returns [`FetchGameError::WrongGameType`] if the game's type doesn't match [`GAME_TYPE`],
/// allowing callers to skip non-matching games without treating it as a transient failure.
async fn fetch_game_data<P: alloy_provider::Provider + Clone>(
    game_index: u64,
    factory: &DisputeGameFactoryInstance<P>,
    l1_provider: P,
) -> Result<GameData, FetchGameError> {
    // Look up game metadata from the factory contract.
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

    let l1_head = game.l1Head().call().await.context("failed to get L1 head")?;

    Ok(GameData {
        game_index,
        game_address,
        start_block,
        end_block: l2_block_number,
        l1_head,
        created_at,
    })
}

/// Spawn a `cost-estimator` child process for the given game.
///
/// The log file is pre-seeded with a header block containing the exact command and environment
/// variables, so that a failed run can be replayed manually via `rerun-cost-estimator.sh`.
/// Both stdout and stderr of the child are redirected into the log file.
fn spawn_cost_estimator(
    cost_estimator_binary_path: &Path,
    batch_size: u64,
    env_file: &Path,
    log_file: &LogFile,
    game_data: &GameData,
) -> Result<Child> {
    // Cap batch size to the game's block range so small games run as a single chunk.
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
        "--no-safe-head-split",
        "--l1-head",
        &game_data.l1_head.to_string(),
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

/// Manages the lifecycle of a per-game log file.
///
/// Log files follow the naming convention:
/// `cost-estimator-<index>-<address>[-retry<n>][-bg-retry<n>].log`
///
/// On completion, [`mark_complete`](Self::mark_complete) appends `-success` or `-failure`
/// before the `.log` extension.
struct LogFile {
    path: PathBuf,
}

impl LogFile {
    /// Build the log file path from the game index, address, and attempt kind.
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

    /// Parse the game index from a log filename. Used by log-space enforcement to identify
    /// which game a log belongs to (so logs for running games are not deleted).
    fn extract_game_index(path: &Path) -> Option<u64> {
        let filename = path.file_name()?.to_str()?;
        let stripped = filename.strip_prefix("cost-estimator-")?;
        let dash_pos = stripped.find('-')?;
        stripped[..dash_pos].parse().ok()
    }

    /// Rename the log file to include a `-success` or `-failure` suffix before `.log`.
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
    /// List all log files with their sizes and game indexes, for log-space enforcement.
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

/// Delete the oldest log files (by game index) until the total log directory size is within
/// `max_size_bytes`. Logs for currently running games are never deleted.
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
    let l2_rpc = env::var("L2_RPC").context("L2_RPC not set")?;
    let dispute_game_factory_address = env::var("DISPUTE_GAME_FACTORY_ADDRESS")
        .context("DISPUTE_GAME_FACTORY_ADDRESS not set")?
        .parse::<Address>()
        .context("Invalid DISPUTE_GAME_FACTORY_ADDRESS")?;

    info!("L1 RPC: {}", l1_rpc);
    info!("L2 RPC: {}", l2_rpc);
    info!("Dispute Game Factory: {}", dispute_game_factory_address);

    // Set up L1 and L2 providers, and the factory contract. The L2 provider is used during
    // spawn scheduling to verify the L2 has finalised the game's end block; see `spawn_game`.
    let l1_provider = ProviderBuilder::new().connect_http(l1_rpc.parse()?);
    let l2_provider = ProviderBuilder::new().connect_http(l2_rpc.parse()?);
    let factory =
        DisputeGameFactoryInstance::new(dispute_game_factory_address, l1_provider.clone());

    let progress_file =
        args.progress_file.clone().unwrap_or_else(|| args.logs_dir.join("progress.json"));

    let persisted_progress = MonitorState::load_progress(&progress_file);

    // Determine starting game index: explicit flag > persisted progress > latest on-chain.
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
        l2_provider: &l2_provider,
        cost_estimator_binary_path: args.cost_estimator_binary_path.as_path(),
        batch_size: args.batch_size,
        env_file: args.env_file.as_path(),
        logs_dir: args.logs_dir.as_path(),
    };

    // ── Main poll loop ──────────────────────────────────────────────────────
    // Each iteration: cleanup -> evict -> enforce log limits -> spawn primary -> spawn
    // background -> discover new games. The loop breaks on SIGTERM/SIGINT for graceful
    // shutdown.
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

        // Phase 1: handle completed/failed/runaway processes.
        state.cleanup_finished_processes();

        // Phase 2: drop background retries that have exceeded their max age.
        state.evict_aged_background_retries();

        // Phase 3: delete oldest log files if the directory exceeds the size limit.
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

        // Phase 4: spawn pending primary games whose delay has elapsed, up to max_concurrent.
        // A snapshot of the eligible candidates is taken once so we can iterate without
        // holding a borrow on `state.pending_games`; `spawn_game` only mutates the entry it
        // operates on, so other snapshot entries remain valid as we walk through them.
        let current_time = Instant::now();
        let primary_candidates: Vec<(u64, AttemptKind)> = state
            .pending_games
            .iter()
            .filter(|p| p.executable_at <= current_time)
            .map(|p| (p.game_index, p.kind))
            .collect();
        for (game_index, kind) in primary_candidates {
            if !state.can_spawn_new(args.max_concurrent) {
                break;
            }
            match state.spawn_game(game_index, kind, &spawn_ctx).await? {
                SpawnOutcome::FetchFailed => continue 'outer,
                SpawnOutcome::L2Behind => continue,
                SpawnOutcome::Spawned | SpawnOutcome::WrongGameType => {}
            }
        }

        // Phase 5: spawn background retries using spare slots. Entries stay in background_retries
        // while running so maybe_requeue can update them on failure; cleanup_finished_processes
        // removes them on success.
        let now_sys = SystemTime::now();
        let background_candidates: Vec<(u64, AttemptKind)> = state
            .background_retries
            .iter()
            .filter(|bg| {
                bg.next_attempt_at <= now_sys &&
                    !state.running_processes.contains_key(&bg.game_index)
            })
            .map(|bg| (bg.game_index, AttemptKind::Background { attempts: bg.attempts + 1 }))
            .collect();
        for (game_index, kind) in background_candidates {
            if !state.can_spawn_new(args.max_concurrent) {
                break;
            }
            match state.spawn_game(game_index, kind, &spawn_ctx).await? {
                SpawnOutcome::FetchFailed => continue 'outer,
                SpawnOutcome::L2Behind => continue,
                SpawnOutcome::Spawned | SpawnOutcome::WrongGameType => {}
            }
        }

        // Phase 6: discover new game indexes from the factory and queue them with a delay.
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
