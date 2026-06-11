pub mod admission;
pub mod discovery;
pub mod executor;
pub mod pipeline;
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
    memory::{read_cgroup_budget_bytes, read_cgroup_usage_bytes},
    network_call_with_timeout, DaType, Estimator, WindowPredictor, WitnessCache,
};
use op_succinct_host_utils::{
    block_range::SpanBatchRange, fetcher::OPSuccinctDataFetcher, stats::ExecutionStats,
};
use op_succinct_proof_utils::initialize_host;
use tokio::sync::{mpsc, Semaphore};

use crate::contained::{
    admission::Admission,
    discovery::{fetch_game_data, FetchGameError},
    executor::execute_game,
    state::{
        is_background_retry_aged_out, load_progress, requeue_decision, resume_next_index,
        save_progress, AttemptKind, BackgroundRetry, PendingGame, RequeueDecision,
    },
};

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

/// Size the RSS-admission semaphore from the cgroup budget. `None` budget (no cgroup
/// limit) defaults to 4. Otherwise: how many `unit_bytes` units fit after reserving
/// current usage + margin, floored at 1.
fn compute_initial_permits(budget: Option<u64>, usage: u64, unit_bytes: u64, margin: u64) -> usize {
    match budget {
        None => 4,
        Some(b) => {
            ((b.saturating_sub(usage).saturating_sub(margin) / unit_bytes.max(1)) as usize).max(1)
        }
    }
}

/// A game can only be executed once its end block is derivable from the finalized L2
/// head. If the finalized head hasn't reached `end_block`, defer (re-queue) without
/// consuming retry budget.
fn game_is_executable(finalized_l2: u64, end_block: u64) -> bool {
    finalized_l2 >= end_block
}

/// Compute the pipeline's frontier seed: the latest on-chain game's `end_block`.
///
/// WHY a proposal boundary, not just any finalized L2 head: the pipeline prebuilds
/// witness stdins by splitting its predicted windows, and the executor consumes them by
/// splitting the on-chain game's `[start_block, end_block]`. `split_range_based_on_safe_heads`
/// anchors its sub-range stepping (`range_start += max_range_size`) on the split's own
/// `l2_start`, so the two paths only produce the SAME sub-range boundaries — and thus the
/// SAME `(chain_id, start, end, da_type)` cache keys — when the pipeline's frontier sits
/// on a real proposal boundary. Proposal boundaries step by `PROPOSAL_INTERVAL`
/// (`l2 = parent + PROPOSAL_INTERVAL`); game N's `end_block` IS a proposal boundary and
/// game N+1's `start_block == game N's end_block`. Seeding the frontier at the latest
/// game's `end_block` and stepping by `proposal_interval` makes the predicted windows
/// `[latest_end, latest_end + interval], …` coincide with the FUTURE games' `[start, end]`
/// ranges, so the executor hits the pipeline's prebuilt stdins.
///
/// Best-effort (spec §4.2): a wrong/stale interval just wastes work and the executor
/// rebuilds on demand. Returns `None` when there are no games or the latest game can't be
/// fetched (wrong type / RPC error); the caller then falls back to the finalized L2 head.
async fn latest_game_end_block<P: alloy_provider::Provider + Clone>(
    on_chain_count: u64,
    factory: &DisputeGameFactoryInstance<P>,
    l1_provider: P,
) -> Option<u64> {
    if on_chain_count == 0 {
        return None;
    }
    match fetch_game_data(on_chain_count - 1, factory, l1_provider).await {
        Ok(game) => Some(game.end_block),
        Err(e) => {
            tracing::warn!(
                game_index = on_chain_count - 1,
                error = %e,
                "failed to fetch latest game for frontier seed; falling back to finalized L2 head"
            );
            None
        }
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
    Success { pg: PendingGame, stats: ExecutionStats, ranges: Vec<SpanBatchRange> },
    /// Non-type-42 game: advance the frontier, drop any background entry.
    WrongType { pg: PendingGame, game_type: u32 },
    /// Transient execution failure: apply the two-tier requeue policy.
    Transient { pg: PendingGame, created_at: SystemTime, error: String },
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
    args: &ContainedArgs,
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
            tracker.add(pg.game_index);
            background_retries.retain(|b| b.game_index != pg.game_index);
            // Purge any stale duplicate queue entry for this now-completed game.
            pending_games.retain(|p| p.game_index != pg.game_index);
            if let Err(e) = save_progress(progress_path, tracker, background_retries) {
                tracing::warn!(error = %e, "failed to save progress");
            }
            let eligible_at = now_sys + Duration::from_secs(args.stdin_grace_secs);
            for range in &ranges {
                prunables.push(PrunableStdin { start: range.start, end: range.end, eligible_at });
            }
        }
        GameTaskResult::WrongType { pg, game_type } => {
            tracing::info!(game_index = pg.game_index, game_type, "skipping non-type-42 game");
            tracker.add(pg.game_index);
            background_retries.retain(|b| b.game_index != pg.game_index);
            pending_games.retain(|p| p.game_index != pg.game_index);
            if let Err(e) = save_progress(progress_path, tracker, background_retries) {
                tracing::warn!(error = %e, "failed to save progress");
            }
        }
        GameTaskResult::Transient { pg, created_at, error } => {
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
            tracker.add(pg.game_index);
            background_retries.retain(|b| b.game_index != pg.game_index);
            pending_games.retain(|p| p.game_index != pg.game_index);
            if let Err(e) = save_progress(progress_path, tracker, background_retries) {
                tracing::warn!(error = %e, "failed to save progress");
            }
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

/// Apply the two-tier retry policy to a failed game, mutating the queues + persisting.
fn apply_requeue(
    pending: &mut VecDeque<PendingGame>,
    background: &mut VecDeque<BackgroundRetry>,
    tracker: &mut SequenceTracker,
    pg: PendingGame,
    created_at: SystemTime,
    args: &ContainedArgs,
    progress_path: &Path,
) -> anyhow::Result<()> {
    let initial_delay = Duration::from_secs(args.delay);
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

/// Run the contained game monitor: build shared resources once, spawn the predictive
/// pipeline, and run the discovery/execute/retry loop.
pub async fn run(args: ContainedArgs) -> anyhow::Result<()> {
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

    // ── RSS-admission semaphore ────────────────────────────────────────────
    let budget = read_cgroup_budget_bytes();
    let margin = args.rss_margin_mb * 1024 * 1024;
    let usage = read_cgroup_usage_bytes().unwrap_or(0);
    let default_unit = 55u64 * 1024 * 1024 * 1024; // ~55 GiB per concurrent execution
    let max_conc = compute_initial_permits(budget, usage, default_unit, margin);
    tracing::info!(max_conc, ?budget, usage, "RSS admission sized");
    let permits = Arc::new(Semaphore::new(max_conc));

    // Adaptive RSS admission (spec §7): gas-weighted projection from observed history,
    // gating each build/execute on LIVE cgroup usage + margin. Shared by the pipeline and
    // the executor; the semaphore above remains the hard concurrency cap.
    let admission = Arc::new(Admission::load(
        budget,
        margin,
        default_unit,
        args.cache_dir.join("completion_history.json"),
        Duration::from_secs(args.poll_interval),
    ));

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

    // ── Pipeline frontier seed ─────────────────────────────────────────────
    // The frontier MUST land on a real proposal boundary so the pipeline's prebuilt
    // window stdins share cache keys with the executor's game splits. Prefer the latest
    // on-chain game's `end_block` (a true proposal boundary); the closure below falls back
    // to the finalized L2 head (then 0) only when there are no games / the fetch failed.
    let frontier_seed = latest_game_end_block(on_chain_count, &factory, l1_provider.clone()).await;

    // ── Predictive pipeline task ───────────────────────────────────────────
    let poll = Duration::from_secs(args.poll_interval);
    {
        let estimator = estimator.clone();
        let fetcher = fetcher.clone();
        let permits = permits.clone();
        let admission = admission.clone();
        let proposal_interval = args.proposal_interval;
        let max_lead_windows = args.max_lead_windows;
        let batch_size = args.batch_size;
        tokio::spawn(async move {
            // Use the precomputed proposal-boundary seed; only fall back to the finalized
            // L2 head (then 0) when no aligned boundary is available.
            let start_frontier = match frontier_seed {
                Some(end_block) => end_block,
                None => {
                    fetcher.get_l2_header(BlockId::finalized()).await.map(|h| h.number).unwrap_or(0)
                }
            };
            let mut predictor =
                WindowPredictor::new(start_frontier, proposal_interval, max_lead_windows);
            loop {
                if let Err(e) = pipeline::pipeline_step(
                    &estimator,
                    &fetcher,
                    &mut predictor,
                    &permits,
                    &admission,
                    batch_size,
                )
                .await
                {
                    tracing::warn!(error = %e, "pipeline step failed");
                }
                tokio::time::sleep(poll).await;
            }
        });
    }

    // ── Restart resume ─────────────────────────────────────────────────────
    let progress_path =
        args.progress_file.clone().unwrap_or_else(|| args.cache_dir.join("progress.json"));
    let persisted = load_progress(&progress_path);
    let mut next_game_index =
        resume_next_index(args.start_index, persisted.as_ref(), on_chain_count);
    tracing::info!(next_game_index, on_chain_count, "resume point determined");

    let mut tracker = SequenceTracker::new(next_game_index.saturating_sub(1));
    let mut background_retries: VecDeque<BackgroundRetry> =
        persisted.map(|p| p.background_retries.into_iter().collect()).unwrap_or_default();
    let mut pending_games: VecDeque<PendingGame> = VecDeque::new();

    let delay = Duration::from_secs(args.delay);
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
            tracing::info!(game_index, ?delay, "discovered new game");
            pending_games.push_front(PendingGame {
                executable_at: Instant::now() + delay,
                game_index,
                kind: AttemptKind::Primary { retries: 0 },
            });
        }

        // (b) Drain due background retries into the pending queue as Background attempts,
        // skipping any game already running or already queued so no duplicate is enqueued.
        let now_sys = SystemTime::now();
        let due: Vec<u64> = background_retries
            .iter()
            .filter(|bg| bg.next_attempt_at <= now_sys)
            .map(|bg| bg.game_index)
            .collect();
        for game_index in due {
            if running_games.contains(&game_index) ||
                pending_games.iter().any(|p| p.game_index == game_index)
            {
                continue;
            }
            if let Some(bg) = background_retries.iter().find(|b| b.game_index == game_index) {
                pending_games.push_back(PendingGame {
                    executable_at: Instant::now(),
                    game_index,
                    kind: AttemptKind::Background { attempts: bg.attempts + 1 },
                });
            }
        }

        // (c) Evict aged-out background retries — but never one whose task is running.
        let now_sys = SystemTime::now();
        background_retries.retain(|bg| {
            !is_background_retry_aged_out(
                bg,
                now_sys,
                max_age,
                running_games.contains(&bg.game_index),
            )
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
            let permits = permits.clone();
            let admission = admission.clone();
            let factory = factory.clone();
            let l1_provider = l1_provider.clone();
            let timeout_secs = args.network_call_timeout_secs;
            let batch_size = args.batch_size;
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
                        // L2-behind defer: the end block must be derivable from the finalized
                        // L2 head before execution; if not, re-queue (no retry budget spent).
                        match fetcher.get_l2_header(BlockId::finalized()).await.map(|h| h.number) {
                            Err(e) => {
                                tracing::warn!(
                                    game_index = pg.game_index,
                                    error = %e,
                                    "failed to fetch finalized L2 head; will retry"
                                );
                                GameTaskResult::Defer { pg }
                            }
                            Ok(finalized_l2)
                                if !game_is_executable(finalized_l2, game.end_block) =>
                            {
                                tracing::debug!(
                                    game_index = pg.game_index,
                                    finalized_l2,
                                    end_block = game.end_block,
                                    "deferring game: L2 behind"
                                );
                                GameTaskResult::Defer { pg }
                            }
                            Ok(_) => match execute_game(
                                &estimator,
                                &fetcher,
                                &permits,
                                &admission,
                                &game,
                                batch_size,
                            )
                            .await
                            {
                                Ok((stats, ranges)) => {
                                    GameTaskResult::Success { pg, stats, ranges }
                                }
                                Err(e) if e.is_transient() => GameTaskResult::Transient {
                                    pg,
                                    created_at: game.created_at,
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
            });
        }
        pending_games = remaining;
    }
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
        assert_eq!(args.max_concurrent_games, 5);
    }

    #[test]
    fn permits_scale_with_budget() {
        let budget = Some(500u64 * 1024 * 1024 * 1024);
        let unit = 55u64 * 1024 * 1024 * 1024;
        let margin = 20u64 * 1024 * 1024 * 1024;
        // 500GiB - 20GiB margin = 480GiB; /55GiB = 8.7 → 8.
        assert_eq!(super::compute_initial_permits(budget, 0, unit, margin), 8);
        assert_eq!(super::compute_initial_permits(None, 0, unit, margin), 4);
    }

    #[test]
    fn game_deferred_until_l2_reaches_end_block() {
        assert!(!super::game_is_executable(99, 100));
        assert!(super::game_is_executable(100, 100));
        assert!(super::game_is_executable(150, 100));
    }

    #[test]
    fn prune_eligible_only_after_grace() {
        use std::time::{Duration, SystemTime};
        let eligible_at = SystemTime::now() + Duration::from_secs(3600);
        assert!(!super::is_prune_eligible(SystemTime::now(), eligible_at));
        assert!(super::is_prune_eligible(eligible_at + Duration::from_secs(1), eligible_at));
    }
}
