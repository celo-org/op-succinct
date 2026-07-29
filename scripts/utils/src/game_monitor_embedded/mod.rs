//! Embedded game monitor: a single in-process daemon that discovers OP Succinct
//! fault-dispute games on-chain and runs cost estimation (SP1 execution) for each,
//! replacing the legacy monitor that shelled out to the `cost-estimator` binary and
//! scraped its logs.
//!
//! # Cache-fronted scheduling (outstanding-work #26)
//!
//! Everything starts in [`run`], which builds the shared resources once and then drives the
//! control plane (discovery/retry, the main loop) and the data plane (the
//! [`scheduler::Scheduler`]'s build and execute worker pools). They meet at two on-disk
//! caches keyed `chain_id/start/end/da_type`: the **witness (stdin) cache** and the **proof
//! cache** (`ExecutionStats` per range).
//!
//! * **Speculative feeders** (proactive). A [`ReadyRangeProvider`] predicts the next game
//!   window from the proposal cadence, splits it into fixed sub-ranges anchored at the
//!   window start, and offers each soundly-finalized sub-range to the build pool's
//!   speculative LIFO queue (newest first). A completed speculative build feeds the execute
//!   pool's speculative LIFO queue, bounded by the lead cap
//!   (`--max-speculative-lead-windows` past the newest discovered game) — so both the
//!   witness *and* the execute are usually already cached when the game arrives.
//!
//! * **Games** (reactive, the main loop). The factory poll enqueues new games newest-first;
//!   each due game runs as a light task that splits into the same sub-ranges, demands
//!   whatever the proof cache is missing (FIFO priority queues, served before all
//!   speculative work), and assembles the aggregate from the cache. An un-built range
//!   promotes a witness demand rather than building inline.
//!
//! Because the feeder anchors its splits at the proposal boundary, its sub-range boundaries
//! match the games', the cache keys line up, and the happy path is pure cache assembly.
//!
//! # Concurrency and state
//!
//! Games run as concurrent tokio tasks, capped by `--max-concurrent-games`; the heavy units
//! run on the worker pools (`--max-concurrent-builds` build workers,
//! `--max-concurrent-units` execute workers). Each game task reports its outcome over an
//! mpsc channel as a `GameTaskResult`; the main loop is the *only* writer of the scheduling
//! state ([`PendingGame`] queue, [`BackgroundRetry`] queue, [`SequenceTracker`] frontier,
//! prune list), so none of it needs locking. The outcome is applied single-threaded by
//! `apply_game_result`.
//!
//! # Memory admission
//!
//! Both halves pass every build/execute unit through one shared [`Admission`] gate. A
//! background sampler learns a cost-per-(effective-)gas coefficient from resident memory;
//! admission projects whether one more unit fits the cgroup budget (less a margin) and also
//! enforces a hard in-flight count cap. Until the first game completes it runs strictly
//! serially (cold start). This is the only backpressure — there is no watchdog or freeze;
//! a stuck RPC is bounded by per-call timeouts instead.
//!
//! # Retry policy
//!
//! Failures follow a two-tier policy ([`state::requeue_decision`]): a small **Primary**
//! budget with linear backoff, then a long **Background** queue that quadruples the wait
//! each attempt, up to `--background-retry-max-age-secs` (~3.5 days). Conditions that are
//! nobody's fault — the finalized L2 head not yet reaching a game's end block, a
//! control-plane RPC blip — are *deferred* (re-queued) without spending budget. Moving a
//! game to the background queue advances the [`SequenceTracker`] frontier, so one stuck
//! game never stalls the contiguous frontier or the daemon.
//!
//! # Restart
//!
//! Progress is persisted to `progress.json` (the contiguous-completed frontier, the
//! out-of-order completions above it, and the background queue) and restored on startup via
//! [`state::resume_index`]; an explicit `--start-index` overrides it, otherwise it falls back
//! to the latest on-chain game. Restoring the above-frontier completions means a restart skips
//! games already done rather than re-running them.
//!
//! # Submodules
//!
//! * [`admission`] — the memory-admission gate and its learned model.
//! * [`registry`] — the in-flight gas/count tally and the RAII [`registry::AdmitGuard`].
//! * [`rss_source`] — resident-memory sampling sources (proc / cgroup / unsupported).
//! * [`ready_range_provider`] — window prediction, splitting, finalization gating.
//! * [`scheduler`] — the FIFO-priority/LIFO-speculative queues and the worker pools.
//! * [`pipeline`] — builds one witness per sub-range (the build workers' unit of work).
//! * [`executor`] — executes one sub-range ([`executor::execute_step`]) and assembles games
//!   from the proof cache ([`executor::execute_game`]).
//! * [`discovery`] — reads game metadata from the factory and filters to type-42.
//! * [`state`] — the queue/attempt types, the pure retry policy, and progress persistence.

pub mod admission;
pub mod discovery;
pub mod executor;
pub mod pipeline;
pub mod readiness;
pub mod ready_range_provider;
pub mod registry;
pub mod scheduler;
pub mod rss_source;
pub mod state;

use std::{
    collections::{HashSet, VecDeque},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_provider::ProviderBuilder;
use anyhow::Context;
use clap::Parser;
use fault_proof::contract::DisputeGameFactory::DisputeGameFactoryInstance;
use futures::FutureExt;
use op_succinct_common::SequenceTracker;
use op_succinct_estimator::{
    memory::read_cgroup_budget_bytes, network_call_with_timeout, DaType, Estimator, WitnessCache,
};
use op_succinct_host_utils::{
    block_range::{split_range_basic, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
    stats::ExecutionStats,
};
use op_succinct_proof_utils::initialize_host;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::game_monitor_embedded::{
    admission::{Admission, AdmissionConfig},
    discovery::{fetch_game_data, FetchGameError},
    executor::execute_game,
    ready_range_provider::ReadyRangeProvider,
    rss_source::{make_source, RssSourceKind},
    scheduler::Scheduler,
    state::{
        is_background_retry_aged_out, load_progress, requeue_decision, resume_index, save_progress,
        AttemptKind, BackgroundRetry, PendingGame, RequeueDecision,
    },
};

/// CLI for the embedded game monitor. Preserves the legacy flags and adds the
/// pipeline/cache/memory knobs.
#[derive(Debug, Clone, Parser)]
pub struct EmbeddedArgs {
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// Main loop polling interval (seconds).
    #[arg(long, default_value = "30")]
    pub poll_interval: u64,
    /// Base delay for the two-tier retry backoff, as a human duration (`10s`, `3m`, `5h`).
    ///
    /// Applied ONLY to retries of *failed* games — first execution is not delayed (readiness is
    /// gated by L2/L1 finalization instead). With base `d` and `--max-retries` `m`, a game that
    /// keeps failing is re-queued, in order:
    ///
    /// - Primary (in-loop) retry `n` in `1..=m`: after `d * 2 * n` (linear — `2d`, `4d`, ...).
    /// - First Background retry (Primary budget spent): after `d * 2 * m * 4`.
    /// - Each subsequent Background retry: after the previous wait `* 4`, up to
    ///   `--background-retry-max-age-secs`.
    #[arg(long, value_parser = parse_duration, default_value = "10m")]
    pub retry_backoff_delay: Duration,
    /// Blocks per range. Scales the memory footprint of each unit: host RSS per
    /// build/execute grows roughly linearly with the range's gas (what the admission model
    /// projects), as does the SP1 guest's touched memory — the guest is 64-bit and bounded
    /// only by SP1's soft `MEMORY_LIMIT` budget (default 24 GiB, env-overridable), not by
    /// address space. Larger ranges also coarsen retry/cache/speculation granularity: the
    /// proof cache, requeues, and the prebuild pipeline all work per range.
    #[arg(long, default_value = "200")]
    pub batch_size: u64,
    /// Maximum number of games executed concurrently (replaces the legacy
    /// `--max-concurrent`). Per-range memory is bounded separately by RSS admission;
    /// this caps game-task fan-out and concurrent control-plane fetches.
    #[arg(long, default_value = "5")]
    pub max_concurrent_games: usize,
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
    /// RSS admission safety margin (MiB).
    #[arg(long, default_value = "20480")]
    pub rss_margin_mb: u64,
    /// Hard cap on concurrently in-flight build/execute units. Bounds file descriptors,
    /// RPC fan-out, and CPU — the resources the memory model does not constrain — and is
    /// the only cap when the budget is unlimited or the model has no signal yet.
    #[arg(long, default_value = "8")]
    pub max_concurrent_units: usize,
    /// Number of build workers draining the build queues; the shared memory admission gate
    /// remains the real governor of how many builds run at once. Defaults to
    /// `--max-concurrent-units`; set lower to reserve admission capacity for executes.
    #[arg(long)]
    pub max_concurrent_builds: Option<usize>,
    /// Lead cap for SPECULATIVE executes: how many predicted windows past the newest
    /// discovered game's end block they may run. Window prediction is operator config, not
    /// protocol law, and a wasted speculative execute is the dominant cost (uncancellable),
    /// so this bounds the waste when an assumption breaks (interval change, re-anchor,
    /// stalled proposer). `0` disables speculative execution (witness prebuild is unaffected;
    /// it stays finalization-bounded).
    #[arg(long, default_value = "1")]
    pub max_speculative_lead_windows: u64,
    /// Resident-memory sampling source: `auto` (per-process on Linux), `proc`
    /// (`/proc/self/status`), or `cgroup` (`memory.current`).
    #[arg(long, value_enum, default_value_t = RssSourceKind::Auto)]
    pub rss_source: RssSourceKind,
    /// Memory sampler tick period (milliseconds).
    #[arg(long, default_value = "100")]
    pub sample_period_ms: u64,
    /// Persist the learned memory model every N sampler ticks.
    #[arg(long, default_value = "300")]
    pub persist_every_ticks: u32,
    /// Per-network-call timeout (seconds).
    #[arg(long, default_value = "120")]
    pub network_call_timeout_secs: u64,
    /// Max total size for the on-disk witness/stdin cache, as a human-readable size
    /// (e.g. `40GB`, `40GiB`). When the cache exceeds this after a poll, blobs are evicted
    /// oldest-first until it fits, skipping the ranges of games still awaiting a background
    /// retry. `0` disables the cap (unbounded). Set below the PVC size to leave headroom for
    /// one poll's worth of in-flight writes.
    #[arg(long, value_parser = parse_cache_size, default_value = "0")]
    pub max_cache_size: u64,
}

/// Parse a human-readable cache size (`40GB`, `40GiB`, `0`, ...) into bytes. Decimal units
/// (`GB`) are 1000-based, binary units (`GiB`) 1024-based.
fn parse_cache_size(s: &str) -> Result<u64, String> {
    parse_size::parse_size(s).map_err(|e| format!("invalid --max-cache-size '{s}': {e}"))
}

/// Parse a human-readable duration (`10s`, `3m`, `5h`, `1h30m`, ...) into a [`Duration`].
fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| format!("invalid duration '{s}': {e}"))
}

/// DA type for the cache key, selected by the build feature.
fn da_type() -> DaType {
    #[cfg(feature = "celestia")]
    {
        DaType::Celestia
    }
    #[cfg(all(feature = "eigenda", not(feature = "celestia")))]
    {
        DaType::EigenDa
    }
    #[cfg(not(any(feature = "eigenda", feature = "celestia")))]
    {
        DaType::Ethereum
    }
}

/// A range whose stdin blob should be pruned. A finished game (Success or Fatal) is done
/// with its ranges, so they are reclaimed on the next main-loop sweep.
struct PrunableStdin {
    start: u64,
    end: u64,
}

/// Outcome of one spawned game-execution task, sent back to the main loop which owns the
/// scheduling state and applies the mutation single-threaded (no locks on the queues).
enum GameTaskResult {
    /// Game executed: advance the frontier, drop any background entry, schedule prunes.
    /// `stats` is boxed to keep this large variant from bloating every `GameTaskResult`.
    Success { pg: PendingGame, stats: Box<ExecutionStats>, ranges: Vec<SpanBatchRange> },
    /// Non-type-42 game: advance the frontier, drop any background entry.
    WrongType { pg: PendingGame, game_type: u32 },
    /// Transient execution failure: apply the two-tier requeue policy. Carries the game's
    /// block range so a move to the background queue records it (for cache protection).
    Transient {
        pg: PendingGame,
        created_at: SystemTime,
        start_block: u64,
        end_block: u64,
        error: String,
    },
    /// Fatal execution failure: advance the frontier (never stall), drop background entry.
    /// The failed game's stdin is deliberately NOT pruned — it is kept for debugging and only
    /// reclaimed by the size-cap GC — so no ranges are carried.
    Fatal { pg: PendingGame, error: String },
    /// Pre-execution transient condition (L2 behind, control-plane blip): re-queue the
    /// same attempt without spending retry budget.
    Defer { pg: PendingGame },
}

impl GameTaskResult {
    fn game_index(&self) -> u64 {
        match self {
            GameTaskResult::Success { pg, .. } |
            GameTaskResult::WrongType { pg, .. } |
            GameTaskResult::Transient { pg, .. } |
            GameTaskResult::Fatal { pg, .. } |
            GameTaskResult::Defer { pg } => pg.game_index,
        }
    }
}

/// Apply a finished game task's outcome to the scheduling state. Runs only on the main
/// loop, so the queues/tracker/prunables are mutated without locking.
#[allow(clippy::too_many_arguments)]
fn apply_game_result(
    result: GameTaskResult,
    pending_games: &mut VecDeque<PendingGame>,
    background_retries: &mut VecDeque<BackgroundRetry>,
    tracker: &mut SequenceTracker,
    prunables: &mut Vec<PrunableStdin>,
    args: &EmbeddedArgs,
    progress_path: &Path,
    now_instant: Instant,
) {
    match result {
        GameTaskResult::Success { pg, stats, ranges } => {
            tracing::info!(
                game_index = pg.game_index,
                sp1_gas = stats.total_sp1_gas,
                blocks = stats.nb_blocks,
                ranges = ranges.len(),
                "game executed"
            );
            // Completes the game and purges any stale duplicate queue entry.
            complete_game(pg.game_index, tracker, background_retries, pending_games, progress_path);
            for range in &ranges {
                prunables.push(PrunableStdin { start: range.start, end: range.end });
            }
        }
        GameTaskResult::WrongType { pg, game_type } => {
            tracing::info!(game_index = pg.game_index, game_type, "skipping non-type-42 game");
            complete_game(pg.game_index, tracker, background_retries, pending_games, progress_path);
        }
        GameTaskResult::Transient { pg, created_at, start_block, end_block, error } => {
            tracing::warn!(
                game_index = pg.game_index,
                error,
                "game execution failed (transient); requeuing"
            );
            if let Err(e) = apply_requeue(
                pending_games,
                background_retries,
                tracker,
                pg,
                created_at,
                start_block,
                end_block,
                args,
                progress_path,
            ) {
                tracing::warn!(error = %e, "failed to apply requeue");
            }
        }
        GameTaskResult::Fatal { pg, error } => {
            tracing::error!(
                game_index = pg.game_index,
                error,
                "game execution failed (fatal); skipping, retaining stdin for debugging"
            );
            // Deliberately do NOT prune: a fatal game's cached stdin is kept on disk so it can
            // be fetched to iterate on the execution code locally. The size-cap GC reclaims it
            // under space pressure (a fatal range is unprotected, so evicted oldest-first).
            complete_game(pg.game_index, tracker, background_retries, pending_games, progress_path);
        }
        GameTaskResult::Defer { pg } => {
            // Re-queue the same attempt, immediately eligible (no retry budget spent). The loop's
            // own `sleep(poll)` sits between here and the spawn step, so it already paces the
            // re-check by one poll — adding a delay here would just be absorbed by that sleep.
            pending_games.push_back(PendingGame {
                executable_at: now_instant,
                game_index: pg.game_index,
                kind: pg.kind,
            });
        }
    }
}

/// Mark a game complete: advance the frontier, drop any queued/background entries
/// for it, and persist progress (warn-only on failure).
fn complete_game(
    game_index: u64,
    tracker: &mut SequenceTracker,
    background_retries: &mut VecDeque<BackgroundRetry>,
    pending_games: &mut VecDeque<PendingGame>,
    progress_path: &Path,
) {
    tracker.add(game_index);
    background_retries.retain(|b| b.game_index != game_index);
    pending_games.retain(|p| p.game_index != game_index);
    if let Err(e) = save_progress(progress_path, tracker, background_retries) {
        tracing::warn!(error = %e, "failed to save progress");
    }
}

/// Apply the two-tier retry policy to a failed game, mutating the queues + persisting.
#[allow(clippy::too_many_arguments)]
fn apply_requeue(
    pending: &mut VecDeque<PendingGame>,
    background: &mut VecDeque<BackgroundRetry>,
    tracker: &mut SequenceTracker,
    pg: PendingGame,
    created_at: SystemTime,
    start_block: u64,
    end_block: u64,
    args: &EmbeddedArgs,
    progress_path: &Path,
) -> anyhow::Result<()> {
    let initial_delay = args.retry_backoff_delay;
    let prev_wait = background.iter().find(|b| b.game_index == pg.game_index).map(|b| b.last_wait);
    match requeue_decision(pg.kind, initial_delay, args.max_retries, prev_wait) {
        RequeueDecision::Primary { retries, delay } => {
            pending.push_front(PendingGame {
                executable_at: Instant::now() + delay,
                game_index: pg.game_index,
                kind: AttemptKind::Primary { retries },
            });
        }
        RequeueDecision::ToBackground { first_wait } => {
            // Primary budget exhausted: advance the frontier (never stall) and enqueue
            // a background retry.
            tracker.add(pg.game_index);
            background.push_back(BackgroundRetry {
                game_index: pg.game_index,
                game_created_at: created_at,
                next_attempt_at: SystemTime::now() + first_wait,
                last_wait: first_wait,
                attempts: 0,
                start_block,
                end_block,
            });
            save_progress(progress_path, tracker, background)?;
        }
        RequeueDecision::Background { next_wait } => {
            // Update the matching background entry in place with the new schedule.
            if let Some(entry) = background.iter_mut().find(|b| b.game_index == pg.game_index) {
                entry.last_wait = next_wait;
                entry.next_attempt_at = SystemTime::now() + next_wait;
                entry.attempts += 1;
            }
            save_progress(progress_path, tracker, background)?;
        }
    }
    Ok(())
}

/// Run a spawned game task's body, converting a panic into a `Transient` result instead of
/// letting it unwind the task. The main loop frees a game's concurrency slot only when the task
/// reports a result (`running_games.remove`), so an uncaught panic would send nothing and leak
/// the slot forever — enough of them wedge the monitor (item #23). A panic is treated as
/// transient: the two-tier retry recovers a flaky-dependency panic and bounds a deterministic one
/// (retry budget -> background -> age-out). `recovery_pg` is a clone kept outside `body`, since
/// `body` moves its own `pg` and it is gone once the task unwinds; the game's block range is
/// unknown here, so it defaults (an un-ranged background entry is simply unprotected).
async fn catch_game_panic(
    recovery_pg: PendingGame,
    body: impl std::future::Future<Output = GameTaskResult>,
) -> GameTaskResult {
    match AssertUnwindSafe(body).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            let msg = panic_message(&*payload);
            tracing::error!(
                game_index = recovery_pg.game_index,
                panic = %msg,
                "game task panicked; requeuing as transient"
            );
            GameTaskResult::Transient {
                pg: recovery_pg,
                created_at: SystemTime::now(),
                start_block: 0,
                end_block: 0,
                error: format!("panic: {msg}"),
            }
        }
    }
}

/// Best-effort message from a caught panic payload; `&str` / `String` cover virtually all panics.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// Run the embedded game monitor: build shared resources once, spawn the predictive
/// pipeline, and run the discovery/execute/retry loop.
pub async fn run(args: EmbeddedArgs) -> anyhow::Result<()> {
    // ── Shared resources (built once) ──────────────────────────────────────
    let fetcher = Arc::new(OPSuccinctDataFetcher::new_with_rollup_config().await?);
    let chain_id = fetcher.get_l2_chain_id().await?;
    let host = initialize_host(fetcher.clone());
    let cache = WitnessCache::new(&args.cache_dir, chain_id, da_type());
    let estimator = Arc::new(Estimator {
        host: host.clone(),
        fetcher: fetcher.clone(),
        cache: cache.clone(),
        chain_id,
        safe_db_fallback: true,
    });

    // ── Adaptive memory admission ──────────────────────────────────────────
    // A background sampler folds resident memory into a learned cost-per-(effective-)gas
    // coefficient; admission projects whether one more unit fits the budget AND enforces a
    // hard in-flight count cap (bounding fds/RPC/CPU when memory isn't the binding
    // constraint). Shared by the pipeline and the executor. Cold start runs serially per
    // kind until that kind's per-gas cost has been learned from a pure sample.
    let budget = read_cgroup_budget_bytes();
    let margin = args.rss_margin_mb * 1024 * 1024;
    tracing::info!(?budget, max_concurrent = args.max_concurrent_units, "memory admission sized");
    let admission = Admission::load(
        AdmissionConfig {
            budget_bytes: budget,
            margin_bytes: margin,
            max_concurrent: args.max_concurrent_units,
            admit_poll: Duration::from_secs(args.poll_interval),
            sample_period: Duration::from_millis(args.sample_period_ms),
            persist_every: args.persist_every_ticks,
            persist_path: args.cache_dir.join("memory_model.json"),
        },
        make_source(args.rss_source),
    );
    // Start sampling before any work is admitted; detached, self-persists periodically.
    admission.clone().spawn_sampler();

    // ── L1 provider + dispute game factory (from env, matching the legacy) ──
    // Built before the pipeline spawn so we can seed the predictor's frontier from the
    // latest on-chain game's proposal boundary (see `latest_game_end_block`).
    let l1_rpc = std::env::var("L1_RPC").context("L1_RPC not set")?;
    let factory_address = std::env::var("DISPUTE_GAME_FACTORY_ADDRESS")
        .context("DISPUTE_GAME_FACTORY_ADDRESS not set")?
        .parse::<Address>()
        .context("Invalid DISPUTE_GAME_FACTORY_ADDRESS")?;
    let l1_provider = ProviderBuilder::new().connect_http(l1_rpc.parse()?);
    let factory = DisputeGameFactoryInstance::new(factory_address, l1_provider.clone());

    let on_chain_count = factory.gameCount().call().block(BlockId::finalized()).await?.to::<u64>();

    // Seed the predictive pipeline from the latest finalized game's proposal boundary. Three
    // cases must not crash the daemon at startup: a fresh chain with no finalized games yet
    // (`on_chain_count == 0`, which would otherwise underflow `on_chain_count - 1`), a transient
    // RPC failure fetching the latest game, and a non-type-42 latest game. In all of them the
    // pipeline is simply left unseeded — the executor still builds witnesses on demand as games
    // are discovered.
    let pipeline_seed = match on_chain_count.checked_sub(1) {
        None => {
            tracing::info!("no finalized games yet; predictive pipeline not seeded");
            None
        }
        Some(latest_index) => {
            match fetch_game_data(latest_index, &factory, l1_provider.clone()).await {
                Ok(game_data) => Some(game_data.end_block),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to fetch latest game; predictive pipeline not seeded");
                    None
                }
            }
        }
    };

    // ── Cache-fronted scheduler + worker pools (outstanding-work #26) ──────
    // The scheduler owns the FIFO-priority / LIFO-speculative queues; the worker pools
    // drain them, passing every unit through the shared admission gate. The speculative
    // execute horizon is seeded from the latest on-chain game (0 keeps speculation off
    // until discovery observes one).
    let poll = Duration::from_secs(args.poll_interval);
    let lead_blocks = args.proposal_interval.saturating_mul(args.max_speculative_lead_windows);
    let sched = Arc::new(Scheduler::new(pipeline_seed.unwrap_or(0), lead_blocks));
    let build_workers = args.max_concurrent_builds.unwrap_or(args.max_concurrent_units).max(1);
    scheduler::spawn_workers(
        sched.clone(),
        estimator.clone(),
        fetcher.clone(),
        admission.clone(),
        build_workers,
        args.max_concurrent_units.max(1),
    );

    // ── Speculative build feeder ───────────────────────────────────────────
    // Offers each soundly-finalized predicted sub-range to the build pool's speculative
    // queue. Builds themselves run on the workers; a completed speculative build then feeds
    // the (lead-capped) speculative execute queue inside the scheduler.
    if let Some(seed) = pipeline_seed {
        let estimator = estimator.clone();
        let fetcher = fetcher.clone();
        let sched = sched.clone();
        let proposal_interval = args.proposal_interval;
        let batch_size = args.batch_size;
        tokio::spawn(async move {
            let mut provider = ReadyRangeProvider::new(seed, proposal_interval, batch_size);
            loop {
                while let Some(range) = provider.next_range(&fetcher).await {
                    // Already built (e.g. a restart re-walk): skip the queue — the worker
                    // would only short-circuit at `has_stdin` after burning an admission
                    // admit.
                    if estimator.cache.has_stdin(range.start, range.end) {
                        continue;
                    }
                    sched.push_speculative_build((range.start, range.end));
                }
                tokio::time::sleep(poll).await;
            }
        });
    }

    // ── Restart resume ─────────────────────────────────────────────────────
    let progress_path =
        args.progress_file.clone().unwrap_or_else(|| args.cache_dir.join("progress.json"));
    let persisted = load_progress(&progress_path);
    let mut next_game_index = resume_index(args.start_index, persisted.as_ref(), on_chain_count);
    tracing::info!(next_game_index, on_chain_count, "resume point determined");

    // Restore the completion set from persisted progress: the contiguous watermark plus the
    // out-of-order completions above it, so a restart neither loses the watermark nor re-runs an
    // already-completed game. An explicit --start-index means the operator is taking manual
    // control, so start from a clean tracker at the requested point.
    let mut tracker = match (args.start_index, persisted.as_ref()) {
        (None, Some(p)) => SequenceTracker::restore(p.last_contiguous, p.pending.iter().copied()),
        _ => SequenceTracker::new(next_game_index.saturating_sub(1)),
    };
    let mut background_retries: VecDeque<BackgroundRetry> =
        persisted.map(|p| p.background_retries.into_iter().collect()).unwrap_or_default();
    let mut pending_games: VecDeque<PendingGame> = VecDeque::new();

    let max_age = Duration::from_secs(args.background_retry_max_age_secs);

    let mut prunables: Vec<PrunableStdin> = Vec::new();

    // Finished game tasks report their outcome here; the main loop applies the state
    // mutation (single-threaded), so the tracker/queues/prunables need no locking.
    let (results_tx, mut results_rx) = mpsc::unbounded_channel::<GameTaskResult>();
    // Game indices whose task is currently running — bounds concurrency and prevents
    // double-spawning a game (e.g. a background drain while it is already executing).
    let mut running_games: HashSet<u64> = HashSet::new();

    // ── Main loop ──────────────────────────────────────────────────────────
    loop {
        // (0) Apply outcomes from finished game tasks. State mutation happens only here.
        let now_instant = Instant::now();
        while let Ok(result) = results_rx.try_recv() {
            running_games.remove(&result.game_index());
            apply_game_result(
                result,
                &mut pending_games,
                &mut background_retries,
                &mut tracker,
                &mut prunables,
                &args,
                &progress_path,
                now_instant,
            );
        }

        // A finished game is done with its ranges, so prune their stdin blobs now. Drains the
        // whole list each iteration (populated by the Success/Fatal handlers above).
        for p in prunables.drain(..) {
            if let Err(e) = estimator.cache.prune_stdin(p.start, p.end) {
                tracing::warn!(start = p.start, end = p.end, error = %e, "stdin prune failed");
            }
        }

        // Backstop GC: bound the cache to `--max-cache-bytes`, evicting oldest blobs first
        // but never a range still owed to a background retry (whose stdin we keep cached for
        // the retry). Reclaims the orphans the per-game prune misses — Fatal/WrongType
        // outcomes, un-executed prebuilds, and everything leaked when a restart drops the
        // in-memory prune list. Also reaps stray temp files from failed atomic writes.
        if args.max_cache_size > 0 {
            let protected: HashSet<(u64, u64)> = background_retries
                .iter()
                .flat_map(|bg| {
                    split_range_basic(bg.start_block, bg.end_block, args.batch_size)
                        .into_iter()
                        .map(|r| (r.start, r.end))
                })
                .collect();
            match estimator.cache.enforce_size_cap(args.max_cache_size, &protected) {
                Ok(o) if o.files_deleted > 0 || o.tmp_deleted > 0 => tracing::info!(
                    bytes_before = o.bytes_before,
                    bytes_freed = o.bytes_freed,
                    files_deleted = o.files_deleted,
                    tmp_deleted = o.tmp_deleted,
                    protected_ranges = protected.len(),
                    "cache size-cap sweep"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "cache size-cap sweep failed"),
            }
        }

        tokio::time::sleep(poll).await;

        // (a) Discovery: queue newly-created games, immediately eligible — readiness (L2/L1
        // finalization) gates when each actually runs, not a fixed delay. Bound the control-plane
        // read so a wedged RPC can't stall the loop; a timeout yields an Err handled like any
        // other fetch failure (warn + retry next tick).
        let count =
            match network_call_with_timeout(args.network_call_timeout_secs, "gameCount", async {
                Ok(factory.gameCount().call().block(BlockId::finalized()).await?.to::<u64>())
            })
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to fetch gameCount; retrying");
                    continue;
                }
            };
        while next_game_index < count {
            let game_index = next_game_index;
            next_game_index += 1;
            // Skip games already completed in a prior run (restored above-watermark set) — a
            // rerun is wasteful and, since stdin is pruned on completion, a full recompute.
            // During normal running this never fires; indices are discovered exactly once.
            if tracker.contains(game_index) {
                continue;
            }
            tracing::info!(game_index, "discovered new game");
            // Immediately eligible; the readiness gate (L2/L1 finalization) and Defer handle
            // "not ready yet" without a fixed delay.
            pending_games.push_front(PendingGame {
                executable_at: Instant::now(),
                game_index,
                kind: AttemptKind::Primary { retries: 0 },
            });
        }

        // (b) Drain due background retries into the pending queue as Background attempts,
        // skipping any game already running or already queued so no duplicate is enqueued.
        let now_sys = SystemTime::now();
        let due: Vec<(u64, u32)> = background_retries
            .iter()
            .filter(|bg| bg.next_attempt_at <= now_sys)
            .map(|bg| (bg.game_index, bg.attempts))
            .collect();
        for (game_index, attempts) in due {
            if running_games.contains(&game_index) ||
                pending_games.iter().any(|p| p.game_index == game_index)
            {
                continue;
            }
            pending_games.push_back(PendingGame {
                executable_at: Instant::now(),
                game_index,
                kind: AttemptKind::Background { attempts: attempts + 1 },
            });
        }

        // (c) Evict aged-out background retries — but never one whose task is running.
        // Eviction is terminal: the game will never be estimated again, so log it (the only
        // game outcome that is otherwise invisible — successes and failures all log).
        let now_sys = SystemTime::now();
        background_retries.retain(|bg| {
            let aged_out = is_background_retry_aged_out(
                bg,
                now_sys,
                max_age,
                running_games.contains(&bg.game_index),
            );
            if aged_out {
                tracing::warn!(
                    game_index = bg.game_index,
                    attempts = bg.attempts,
                    max_age_secs = max_age.as_secs(),
                    "background retry aged out; abandoning game permanently"
                );
            }
            !aged_out
        });

        // (d) Spawn due pending games as concurrent tasks, up to the concurrency cap.
        // Memory is bounded per-range by RSS admission inside execute_game; this caps the
        // number of in-flight games. Each spawned task reports back via `results_tx`.
        let current_time = Instant::now();
        let mut remaining: VecDeque<PendingGame> = VecDeque::with_capacity(pending_games.len());
        while let Some(pg) = pending_games.pop_front() {
            let startable = pg.executable_at <= current_time &&
                !running_games.contains(&pg.game_index) &&
                running_games.len() < args.max_concurrent_games;
            if !startable {
                remaining.push_back(pg);
                continue;
            }
            running_games.insert(pg.game_index);
            let tx = results_tx.clone();
            let estimator = estimator.clone();
            let fetcher = fetcher.clone();
            let sched = sched.clone();
            let factory = factory.clone();
            let l1_provider = l1_provider.clone();
            let timeout_secs = args.network_call_timeout_secs;
            let batch_size = args.batch_size;
            // One span per unit of work (spec §4.6): every log line emitted while this game
            // runs — including bridged `log::` lines from host.run/execute deep in the
            // libraries — carries `game` + `attempt`, and the per-range child spans add
            // `range`. Created before `pg` is moved into the task.
            let game_span = tracing::info_span!("game", index = pg.game_index, attempt = ?pg.kind);
            tokio::spawn(
                async move {
                    // Recover a panic anywhere in the task body (fetch / readiness / execute /
                    // on-demand build, incl. panics deep in kona/sp1) into a `Transient` result so
                    // the main loop's slot release still runs; without this a
                    // panicking task sends no result and permanently leaks its
                    // concurrency slot (item #23).
                    let recovery_pg = pg.clone();
                    let result = catch_game_panic(recovery_pg, async move {
                        // Bound the control-plane game-data read; a timeout is a transient defer.
                        let fetched =
                            network_call_with_timeout(timeout_secs, "fetch_game_data", async {
                                Ok(fetch_game_data(pg.game_index, &factory, l1_provider.clone())
                                    .await)
                            })
                            .await;
                        match fetched {
                            Err(e) => {
                                tracing::warn!(
                                    game_index = pg.game_index,
                                    error = %e,
                                    "fetch_game_data timed out; will retry"
                                );
                                GameTaskResult::Defer { pg }
                            }
                            Ok(Ok(game)) => {
                                // Readiness gate: defer until the game's end block is finalized on
                                // L2 AND L1 has finalized past its `l1_head + buffer` (so the
                                // host's finality cap can't shrink the `+ 20` derivation
                                // read-ahead slack). Not-ready or a control-plane blip both
                                // re-queue without spending retry budget.
                                match readiness::range_ready(
                                    &fetcher,
                                    game.end_block,
                                    estimator.safe_db_fallback,
                                )
                                .await
                                {
                                    Err(e) => {
                                        tracing::warn!(
                                            game_index = pg.game_index,
                                            error = %e,
                                            "readiness check failed; will retry"
                                        );
                                        GameTaskResult::Defer { pg }
                                    }
                                    Ok(false) => {
                                        tracing::debug!(
                                            game_index = pg.game_index,
                                            end_block = game.end_block,
                                            "deferring game: end block not finalized on L2/L1 yet"
                                        );
                                        GameTaskResult::Defer { pg }
                                    }
                                    Ok(true) => match execute_game(
                                        &estimator, &sched, &game, batch_size,
                                    )
                                    .await
                                    {
                                        Ok((stats, ranges)) => GameTaskResult::Success {
                                            pg,
                                            stats: Box::new(stats),
                                            ranges,
                                        },
                                        Err(e) if e.is_transient() => GameTaskResult::Transient {
                                            pg,
                                            created_at: game.created_at,
                                            start_block: game.start_block,
                                            end_block: game.end_block,
                                            error: format!("{e}"),
                                        },
                                        Err(e) => {
                                            GameTaskResult::Fatal { pg, error: format!("{e}") }
                                        }
                                    },
                                }
                            }
                            Ok(Err(FetchGameError::WrongGameType { game_type, .. })) => {
                                GameTaskResult::WrongType { pg, game_type }
                            }
                            Ok(Err(FetchGameError::Other(e))) => {
                                tracing::warn!(
                                    game_index = pg.game_index,
                                    error = %e,
                                    "failed to fetch game data; will retry"
                                );
                                GameTaskResult::Defer { pg }
                            }
                        }
                    })
                    .await;
                    // The receiver lives for the whole process; a send error only means
                    // shutdown, in which case dropping the result is fine.
                    let _ = tx.send(result);
                }
                .instrument(game_span),
            );
        }
        pending_games = remaining;

        // Orchestrator status snapshot, once per poll — the whole control plane at a glance:
        // the completion watermark, the depth of each queue, the scheduler's queued work
        // (builds/executes awaiting a worker, executes parked behind a witness demand), plus
        // the heavy sub-range units the admission gate has in flight.
        let (active_witness, active_prove) = admission.in_flight_units();
        let depths = sched.depths();
        tracing::info!(
            watermark = tracker.end(),
            pending_games = pending_games.len(),
            executing_games = running_games.len(),
            background_games = background_retries.len(),
            queued_witness = depths.queued_witness,
            queued_prove = depths.queued_prove,
            waiting_witness = depths.waiting_witness,
            active_witness,
            active_prove,
            "monitor status"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_required_proposal_interval_and_defaults() {
        let args =
            EmbeddedArgs::parse_from(["game-monitor-embedded", "--proposal-interval", "1800"]);
        assert_eq!(args.proposal_interval, 1800);
        assert_eq!(args.retry_backoff_delay, Duration::from_secs(600)); // default 10m
        assert_eq!(args.batch_size, 200);
        assert_eq!(args.max_concurrent_games, 5);
        assert_eq!(args.max_concurrent_units, 8);
        assert_eq!(args.max_speculative_lead_windows, 1); // one window of speculative execute
        assert_eq!(args.max_cache_size, 0); // cap disabled by default
    }

    #[test]
    fn max_cache_size_parses_human_units() {
        let args = EmbeddedArgs::parse_from([
            "game-monitor-embedded",
            "--proposal-interval",
            "900",
            "--max-cache-size",
            "40GB",
        ]);
        assert_eq!(args.max_cache_size, 40_000_000_000); // decimal GB = 1000^3
        let gib = EmbeddedArgs::parse_from([
            "game-monitor-embedded",
            "--proposal-interval",
            "900",
            "--max-cache-size",
            "40GiB",
        ]);
        assert_eq!(gib.max_cache_size, 40 * 1024 * 1024 * 1024); // binary GiB = 1024^3
    }

    #[test]
    fn retry_backoff_delay_parses_human_durations() {
        let args = EmbeddedArgs::parse_from([
            "game-monitor-embedded",
            "--proposal-interval",
            "900",
            "--retry-backoff-delay",
            "90s",
        ]);
        assert_eq!(args.retry_backoff_delay, Duration::from_secs(90));
        let h = EmbeddedArgs::parse_from([
            "game-monitor-embedded",
            "--proposal-interval",
            "900",
            "--retry-backoff-delay",
            "2m",
        ]);
        assert_eq!(h.retry_backoff_delay, Duration::from_secs(120));
    }

    #[tokio::test]
    async fn panicking_game_body_is_caught_as_transient() {
        let pg = PendingGame {
            executable_at: Instant::now(),
            game_index: 42,
            kind: AttemptKind::Primary { retries: 0 },
        };
        let result = catch_game_panic(pg, async { panic!("boom in kona") }).await;
        match result {
            GameTaskResult::Transient { pg, error, .. } => {
                assert_eq!(pg.game_index, 42);
                assert!(error.contains("boom in kona"), "unexpected error: {error}");
            }
            _ => panic!("expected a Transient result from a panicking body"),
        }
    }

    #[tokio::test]
    async fn non_panicking_game_body_passes_through() {
        let pg = PendingGame {
            executable_at: Instant::now(),
            game_index: 7,
            kind: AttemptKind::Primary { retries: 0 },
        };
        let body_pg = pg.clone();
        let result =
            catch_game_panic(pg, async move { GameTaskResult::Defer { pg: body_pg } }).await;
        assert!(matches!(result, GameTaskResult::Defer { .. }));
    }
}
