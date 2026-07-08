//! Embedded game monitor: a single in-process daemon that discovers OP Succinct
//! fault-dispute games on-chain and runs cost estimation (SP1 execution) for each,
//! replacing the legacy monitor that shelled out to the `cost-estimator` binary and
//! scraped its logs.
//!
//! # Two cooperating halves, joined by the cache
//!
//! Everything starts in [`run`], which builds the shared resources once and then drives
//! two loops that never call each other directly — they meet only at the on-disk witness
//! cache (keyed `chain_id/start/end/da_type`; the DA discriminator comes from `da_type`):
//!
//! * **Predictive pipeline** (proactive, a spawned task). A [`ReadyRangeProvider`] predicts
//!   the next game window from the proposal cadence, splits it into safe-head sub-ranges
//!   anchored at the window start, and hands out each sub-range once it is soundly
//!   finalized (L2 past its end, L1 past its `l1_head + buffer`). [`pipeline::pipeline_step`]
//!   builds the witness and SP1 stdin for each and caches both — running *ahead* of the
//!   executor so the stdin is usually already on disk when the game arrives. Builds run
//!   concurrently (bounded by `--max-concurrent-builds`, default the admission unit cap),
//!   with the shared memory admission gate governing how many actually run at once.
//!
//! * **Reactive executor** (the main loop). Polls the dispute-game factory for newly
//!   finalized games, applies the discover→execute `--delay`, then spawns each due game as
//!   a task. [`executor::execute_game`] splits the real game into the same safe-head
//!   sub-ranges, loads each prebuilt stdin from the cache (building on a miss), runs the
//!   SP1 execute over the sub-ranges concurrently, and aggregates the stats.
//!
//! Because the pipeline anchors its splits at the proposal boundary, its sub-range
//! boundaries match the executor's, the cache keys line up, and the happy path is a hit.
//!
//! # Concurrency and state
//!
//! Games run as concurrent tokio tasks, capped by `--max-concurrent-games`. Each task
//! reports its outcome over an mpsc channel as a `GameTaskResult`; the main loop is the
//! *only* writer of the scheduling state ([`PendingGame`] queue, [`BackgroundRetry`] queue,
//! [`SequenceTracker`] frontier, prune list), so none of it needs locking. The outcome is
//! applied single-threaded by `apply_game_result`.
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
//! Progress is persisted to `progress.json` (the contiguous-completed frontier plus the
//! background queue) and restored on startup via [`state::resume_index`]; an explicit
//! `--start-index` overrides it, otherwise it falls back to the latest on-chain game.
//!
//! # Submodules
//!
//! * [`admission`] — the memory-admission gate and its learned model.
//! * [`registry`] — the in-flight gas/count tally and the RAII [`registry::AdmitGuard`].
//! * [`rss_source`] — resident-memory sampling sources (proc / cgroup / unsupported).
//! * [`ready_range_provider`] — window prediction, safe-head splitting, finalization gating.
//! * [`pipeline`] — builds one witness per ready sub-range.
//! * [`executor`] — runs a discovered game's sub-ranges and aggregates the stats.
//! * [`discovery`] — reads game metadata from the factory and filters to type-42.
//! * [`state`] — the queue/attempt types, the pure retry policy, and progress persistence.

pub mod admission;
pub mod discovery;
pub mod executor;
pub mod pipeline;
pub mod ready_range_provider;
pub mod readiness;
pub mod registry;
pub mod rss_source;
pub mod state;

use std::{
    collections::{HashSet, VecDeque},
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
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::game_monitor_embedded::{
    admission::{Admission, AdmissionConfig},
    discovery::{fetch_game_data, FetchGameError},
    executor::execute_game,
    ready_range_provider::ReadyRangeProvider,
    rss_source::{make_source, RssSourceKind},
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
    /// Blocks per range — caps SP1 guest memory per execution.
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
    /// Grace period (seconds) to keep an stdin blob after its game succeeds.
    #[arg(long, default_value = "3600")]
    pub stdin_grace_secs: u64,
    /// RSS admission safety margin (MiB).
    #[arg(long, default_value = "20480")]
    pub rss_margin_mb: u64,
    /// Hard cap on concurrently in-flight build/execute units. Bounds file descriptors,
    /// RPC fan-out, and CPU — the resources the memory model does not constrain — and is
    /// the only cap when the budget is unlimited or the model has no signal yet.
    #[arg(long, default_value = "8")]
    pub max_concurrent_units: usize,
    /// Max concurrent prebuild-pipeline builds. Bounds how far ahead the pipeline reads and
    /// how many build futures are in flight; the shared memory admission gate remains the
    /// real governor of how many run at once. Defaults to `--max-concurrent-units`; set lower
    /// to reserve admission capacity for executes.
    #[arg(long)]
    pub max_concurrent_builds: Option<usize>,
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

struct PrunableStdin {
    start: u64,
    end: u64,
    eligible_at: std::time::SystemTime,
}

/// An stdin blob may be pruned once its game has succeeded AND the grace window has
/// elapsed (so a near-term rerun can still hit the cache).
fn is_prune_eligible(now: std::time::SystemTime, eligible_at: std::time::SystemTime) -> bool {
    now >= eligible_at
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
    Transient { pg: PendingGame, created_at: SystemTime, start_block: u64, end_block: u64, error: String },
    /// Fatal execution failure: advance the frontier (never stall), drop background entry.
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
    now_sys: SystemTime,
    now_instant: Instant,
    poll: Duration,
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
            let eligible_at = now_sys + Duration::from_secs(args.stdin_grace_secs);
            for range in &ranges {
                prunables.push(PrunableStdin { start: range.start, end: range.end, eligible_at });
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
                "game execution failed (fatal); skipping"
            );
            complete_game(pg.game_index, tracker, background_retries, pending_games, progress_path);
        }
        GameTaskResult::Defer { pg } => {
            // Re-queue the same attempt shortly (no retry budget spent).
            pending_games.push_back(PendingGame {
                executable_at: now_instant + poll,
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
        Some(latest_index) => match fetch_game_data(latest_index, &factory, l1_provider.clone())
            .await
        {
            Ok(game_data) => Some(game_data.end_block),
            Err(e) => {
                tracing::warn!(error = %e, "failed to fetch latest game; predictive pipeline not seeded");
                None
            }
        },
    };

    // ── Predictive pipeline task ───────────────────────────────────────────
    let poll = Duration::from_secs(args.poll_interval);
    if let Some(seed) = pipeline_seed {
        let estimator = estimator.clone();
        let fetcher = fetcher.clone();
        let admission = admission.clone();
        let proposal_interval = args.proposal_interval;
        let batch_size = args.batch_size;
        // Default the pipeline's build fan-out to the admission unit cap, so by default the
        // shared memory admission gate is the effective governor of build concurrency.
        let max_concurrent_builds =
            args.max_concurrent_builds.unwrap_or(args.max_concurrent_units).max(1);
        tokio::spawn(async move {
            let mut provider = ReadyRangeProvider::new(seed, proposal_interval, batch_size);
            loop {
                // Collect every sub-range the provider reports ready right now — cheap
                // finality checks plus a forward cursor advance, so this stays sequential.
                let mut ready = Vec::new();
                while let Some(range) = provider.next_range(&fetcher).await {
                    ready.push(range);
                }
                // Build them concurrently, capped at `max_concurrent_builds`. Each step still
                // passes through the shared memory admission gate, which is what actually
                // bounds how many run at once (memory budget + the in-flight unit cap); this
                // limit just bounds how many build futures we hold in flight.
                futures::stream::iter(ready)
                    .for_each_concurrent(max_concurrent_builds, |range| {
                        let estimator = estimator.clone();
                        let fetcher = fetcher.clone();
                        let admission = admission.clone();
                        async move {
                            if let Err(e) =
                                pipeline::pipeline_step(&estimator, &fetcher, &admission, &range)
                                    .await
                            {
                                tracing::warn!(
                                    start = range.start,
                                    end = range.end,
                                    error = %e,
                                    "pipeline build failed"
                                );
                            }
                        }
                    })
                    .await;
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

    let mut tracker = SequenceTracker::new(next_game_index.saturating_sub(1));
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
        let now_sys = SystemTime::now();
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
                now_sys,
                now_instant,
                poll,
            );
        }

        // Sweep eligible stdin blobs before sleeping so a long-running iteration
        // doesn't delay pruning by an extra poll period.
        prunables.retain(|p| {
            if is_prune_eligible(now_sys, p.eligible_at) {
                if let Err(e) = estimator.cache.prune_stdin(p.start, p.end) {
                    tracing::warn!(start = p.start, end = p.end, error = %e, "stdin prune failed");
                }
                false
            } else {
                true
            }
        });

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

        // (a) Discovery: queue newly-created games with the discover→execute delay.
        // Bound the control-plane read so a wedged RPC can't stall the loop; a timeout
        // yields an Err handled like any other fetch failure (warn + retry next tick).
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
            let admission = admission.clone();
            let factory = factory.clone();
            let l1_provider = l1_provider.clone();
            let timeout_secs = args.network_call_timeout_secs;
            let batch_size = args.batch_size;
            // One span per unit of work (spec §4.6): every log line emitted while this game
            // runs — including bridged `log::` lines from host.run/execute deep in the
            // libraries — carries `game` + `attempt`, and the per-range child spans add
            // `range`. Created before `pg` is moved into the task.
            let game_span =
                tracing::info_span!("game", index = pg.game_index, attempt = ?pg.kind);
            tokio::spawn(async move {
                // Bound the control-plane game-data read; a timeout is a transient defer.
                let fetched = network_call_with_timeout(timeout_secs, "fetch_game_data", async {
                    Ok(fetch_game_data(pg.game_index, &factory, l1_provider.clone()).await)
                })
                .await;
                let result = match fetched {
                    Err(e) => {
                        tracing::warn!(
                            game_index = pg.game_index,
                            error = %e,
                            "fetch_game_data timed out; will retry"
                        );
                        GameTaskResult::Defer { pg }
                    }
                    Ok(Ok(game)) => {
                        // Soundness gate: defer until the game's end block is finalized on L2
                        // AND L1 has finalized past its `l1_head + buffer` (so the witness's
                        // baked-in l1_head is deterministic). Not-ready or a control-plane blip
                        // both re-queue without spending retry budget.
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
                                &estimator, &fetcher, &admission, &game, batch_size,
                            )
                            .await
                            {
                                Ok((stats, ranges)) => {
                                    GameTaskResult::Success { pg, stats: Box::new(stats), ranges }
                                }
                                Err(e) if e.is_transient() => GameTaskResult::Transient {
                                    pg,
                                    created_at: game.created_at,
                                    start_block: game.start_block,
                                    end_block: game.end_block,
                                    error: format!("{e}"),
                                },
                                Err(e) => GameTaskResult::Fatal { pg, error: format!("{e}") },
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
                };
                // The receiver lives for the whole process; a send error only means
                // shutdown, in which case dropping the result is fine.
                let _ = tx.send(result);
            }
            .instrument(game_span));
        }
        pending_games = remaining;

        // Orchestrator status snapshot, once per poll — the whole control plane at a glance:
        // the completion watermark, the depth of each queue, plus the heavy sub-range units
        // the admission gate has in flight (witness builds and SP1 proves/executes).
        let (active_witness, active_prove) = admission.in_flight_units();
        tracing::info!(
            watermark = tracker.end(),
            pending_games = pending_games.len(),
            executing_games = running_games.len(),
            background_games = background_retries.len(),
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
        assert_eq!(args.stdin_grace_secs, 3600);
        assert_eq!(args.max_concurrent_games, 5);
        assert_eq!(args.max_concurrent_units, 8);
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

    #[test]
    fn prune_eligible_only_after_grace() {
        use std::time::{Duration, SystemTime};
        let eligible_at = SystemTime::now() + Duration::from_secs(3600);
        assert!(!super::is_prune_eligible(SystemTime::now(), eligible_at));
        assert!(super::is_prune_eligible(eligible_at + Duration::from_secs(1), eligible_at));
    }
}
