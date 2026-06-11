pub mod discovery;
pub mod executor;
pub mod pipeline;
pub mod state;

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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
    DaType, Estimator, WindowPredictor, WitnessCache,
};
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use op_succinct_proof_utils::initialize_host;
use tokio::sync::Semaphore;

use crate::contained::{
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
        Some(b) => ((b.saturating_sub(usage).saturating_sub(margin) / unit_bytes.max(1)) as usize)
            .max(1),
    }
}

/// A game can only be executed once its end block is derivable from the finalized L2
/// head. If the finalized head hasn't reached `end_block`, defer (re-queue) without
/// consuming retry budget.
fn game_is_executable(finalized_l2: u64, end_block: u64) -> bool {
    finalized_l2 >= end_block
}

/// True if an in-flight unit started at `started` has exceeded the duration ceiling.
fn is_overrun(started: Instant, now: Instant, ceiling_secs: u64) -> bool {
    now.duration_since(started).as_secs() > ceiling_secs
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

    // ── Overrun-guard admission state ──────────────────────────────────────
    // When an in-flight execute runs past the duration ceiling, the watchdog FREEZES
    // admission: the pipeline and executor stop starting NEW work (spec §7 — a running
    // SP1 execute cannot be force-killed in-process). The running unit is left to finish.
    let admission_frozen = Arc::new(AtomicBool::new(false));
    // Records when the current inline execute started (None = idle). The watchdog reads it.
    let execute_started: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

    // ── Predictive pipeline task ───────────────────────────────────────────
    let poll = Duration::from_secs(args.poll_interval);
    {
        let estimator = estimator.clone();
        let fetcher = fetcher.clone();
        let permits = permits.clone();
        let admission_frozen = admission_frozen.clone();
        let proposal_interval = args.proposal_interval;
        let max_lead_windows = args.max_lead_windows;
        let batch_size = args.batch_size;
        tokio::spawn(async move {
            let start_frontier = fetcher
                .get_l2_header(BlockId::finalized())
                .await
                .map(|h| h.number)
                .unwrap_or(0);
            let mut predictor =
                WindowPredictor::new(start_frontier, proposal_interval, max_lead_windows);
            loop {
                if let Err(e) = pipeline::pipeline_step(
                    &estimator,
                    &fetcher,
                    &mut predictor,
                    &permits,
                    &admission_frozen,
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

    // ── Overrun watchdog task ──────────────────────────────────────────────
    {
        let frozen = admission_frozen.clone();
        let started = execute_started.clone();
        let ceiling = args.max_process_duration_secs;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(poll).await;
                let overrun = {
                    let guard = started.lock().unwrap();
                    guard.map(|s| is_overrun(s, Instant::now(), ceiling)).unwrap_or(false)
                };
                if overrun && !frozen.swap(true, Ordering::SeqCst) {
                    tracing::error!(
                        ceiling_secs = ceiling,
                        "ALERT: execute exceeded duration ceiling; freezing admission \
                         (no new work will start; running unit cannot be force-killed)"
                    );
                }
            }
        });
    }

    // ── L1 provider + dispute game factory (from env, matching the legacy) ──
    let l1_rpc = std::env::var("L1_RPC").context("L1_RPC not set")?;
    let factory_address = std::env::var("DISPUTE_GAME_FACTORY_ADDRESS")
        .context("DISPUTE_GAME_FACTORY_ADDRESS not set")?
        .parse::<Address>()
        .context("Invalid DISPUTE_GAME_FACTORY_ADDRESS")?;
    let l1_provider = ProviderBuilder::new().connect_http(l1_rpc.parse()?);
    let factory = DisputeGameFactoryInstance::new(factory_address, l1_provider.clone());

    // ── Restart resume ─────────────────────────────────────────────────────
    let progress_path =
        args.progress_file.clone().unwrap_or_else(|| args.cache_dir.join("progress.json"));
    let persisted = load_progress(&progress_path);
    let on_chain_count =
        factory.gameCount().call().block(BlockId::finalized()).await?.to::<u64>();
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

    // ── Main loop ──────────────────────────────────────────────────────────
    loop {
        // Sweep eligible stdin blobs before sleeping so a long-running iteration
        // doesn't delay pruning by an extra poll period.
        let now = std::time::SystemTime::now();
        prunables.retain(|p| {
            if is_prune_eligible(now, p.eligible_at) {
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
        let count = match factory.gameCount().call().block(BlockId::finalized()).await {
            Ok(c) => c.to::<u64>(),
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

        // (b) Drain due background retries into the pending queue as Background attempts.
        let now_sys = SystemTime::now();
        let due: Vec<u64> = background_retries
            .iter()
            .filter(|bg| bg.next_attempt_at <= now_sys)
            .map(|bg| bg.game_index)
            .collect();
        for game_index in due {
            if let Some(bg) = background_retries.iter().find(|b| b.game_index == game_index) {
                pending_games.push_back(PendingGame {
                    executable_at: Instant::now(),
                    game_index,
                    kind: AttemptKind::Background { attempts: bg.attempts + 1 },
                });
            }
        }

        // (c) Evict aged-out background retries (this loop executes synchronously per game,
        // so no entry is "running" here).
        let now_sys = SystemTime::now();
        background_retries.retain(|bg| !is_background_retry_aged_out(bg, now_sys, max_age, false));

        // (d) Execute due pending games.
        let current_time = Instant::now();
        let due: Vec<PendingGame> = pending_games
            .iter()
            .filter(|p| p.executable_at <= current_time)
            .cloned()
            .collect();
        // Drop the executed entries from the pending queue up front; failures re-queue them.
        pending_games.retain(|p| p.executable_at > current_time);

        for pg in due {
            match fetch_game_data(pg.game_index, &factory, l1_provider.clone()).await {
                Ok(game) => {
                    // L2-behind defer: a game's end block must be derivable from the
                    // finalized L2 head before it can be executed. If the finalized head
                    // hasn't reached it, re-queue the same attempt (no retry budget spent)
                    // with a short defer instead of burning a retry in the splitter.
                    let finalized_l2 =
                        match fetcher.get_l2_header(BlockId::finalized()).await.map(|h| h.number) {
                            Ok(n) => n,
                            Err(e) => {
                                tracing::warn!(
                                    game_index = pg.game_index,
                                    error = %e,
                                    "failed to fetch finalized L2 head; will retry"
                                );
                                pending_games.push_back(PendingGame {
                                    executable_at: Instant::now()
                                        + Duration::from_secs(args.poll_interval),
                                    game_index: pg.game_index,
                                    kind: pg.kind,
                                });
                                continue;
                            }
                        };
                    if !game_is_executable(finalized_l2, game.end_block) {
                        tracing::debug!(
                            game_index = pg.game_index,
                            finalized_l2,
                            end_block = game.end_block,
                            "deferring game {}: L2 behind",
                            pg.game_index
                        );
                        pending_games.push_back(PendingGame {
                            executable_at: Instant::now()
                                + Duration::from_secs(args.poll_interval),
                            game_index: pg.game_index,
                            kind: pg.kind,
                        });
                        continue;
                    }
                    // Admission frozen by the overrun watchdog: don't start new work.
                    // Re-queue this attempt unchanged (no retry budget spent), like the
                    // L2-behind defer path above.
                    if admission_frozen.load(Ordering::SeqCst) {
                        tracing::debug!(
                            game_index = pg.game_index,
                            "admission frozen; deferring game {} (no new work started)",
                            pg.game_index
                        );
                        pending_games.push_back(PendingGame {
                            executable_at: Instant::now()
                                + Duration::from_secs(args.poll_interval),
                            game_index: pg.game_index,
                            kind: pg.kind,
                        });
                        continue;
                    }
                    // Record the start so the watchdog can detect an overrun; clear it on
                    // BOTH success and error so a failed execute leaves no stale start time.
                    *execute_started.lock().unwrap() = Some(Instant::now());
                    let result =
                        execute_game(&estimator, &fetcher, &permits, &game, args.batch_size).await;
                    *execute_started.lock().unwrap() = None;
                    match result {
                        Ok((stats, ranges)) => {
                            tracing::info!(
                                game_index = pg.game_index,
                                start = game.start_block,
                                end = game.end_block,
                                sp1_gas = stats.total_sp1_gas,
                                blocks = stats.nb_blocks,
                                ranges = ranges.len(),
                                "game executed"
                            );
                            tracker.add(pg.game_index);
                            // A successful background game is now complete: drop its entry.
                            background_retries.retain(|b| b.game_index != pg.game_index);
                            if let Err(e) = save_progress(&progress_path, &tracker, &background_retries)
                            {
                                tracing::warn!(error = %e, "failed to save progress");
                            }
                            let eligible_at = std::time::SystemTime::now()
                                + std::time::Duration::from_secs(args.stdin_grace_secs);
                            for range in &ranges {
                                prunables.push(PrunableStdin {
                                    start: range.start,
                                    end: range.end,
                                    eligible_at,
                                });
                            }
                        }
                        Err(e) if e.is_transient() => {
                            tracing::warn!(
                                game_index = pg.game_index,
                                error = %e,
                                "game execution failed (transient); requeuing"
                            );
                            if let Err(err) = apply_requeue(
                                &mut pending_games,
                                &mut background_retries,
                                &mut tracker,
                                pg.clone(),
                                game.created_at,
                                &args,
                                &progress_path,
                            ) {
                                tracing::warn!(error = %err, "failed to apply requeue");
                            }
                        }
                        Err(e) => {
                            // Fatal: advance the frontier so the loop never stalls on it.
                            tracing::error!(
                                game_index = pg.game_index,
                                error = %e,
                                "game execution failed (fatal); skipping"
                            );
                            tracker.add(pg.game_index);
                            background_retries.retain(|b| b.game_index != pg.game_index);
                            if let Err(e) = save_progress(&progress_path, &tracker, &background_retries)
                            {
                                tracing::warn!(error = %e, "failed to save progress");
                            }
                        }
                    }
                }
                Err(FetchGameError::WrongGameType { game_index, game_type, .. }) => {
                    tracing::info!(game_index, game_type, "skipping non-type-42 game");
                    tracker.add(pg.game_index);
                    background_retries.retain(|b| b.game_index != pg.game_index);
                    if let Err(e) = save_progress(&progress_path, &tracker, &background_retries) {
                        tracing::warn!(error = %e, "failed to save progress");
                    }
                }
                Err(FetchGameError::Other(e)) => {
                    // Transient fetch error: re-queue this attempt unchanged to retry next tick.
                    tracing::warn!(
                        game_index = pg.game_index,
                        error = %e,
                        "failed to fetch game data; will retry"
                    );
                    pending_games.push_back(PendingGame {
                        executable_at: Instant::now() + poll,
                        game_index: pg.game_index,
                        kind: pg.kind,
                    });
                }
            }
        }
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
    fn overrun_detected_past_ceiling() {
        use std::time::{Duration, Instant};
        let started = Instant::now();
        assert!(!super::is_overrun(started, started + Duration::from_secs(10), 60));
        assert!(super::is_overrun(started, started + Duration::from_secs(61), 60));
    }

    #[test]
    fn prune_eligible_only_after_grace() {
        use std::time::{Duration, SystemTime};
        let eligible_at = SystemTime::now() + Duration::from_secs(3600);
        assert!(!super::is_prune_eligible(SystemTime::now(), eligible_at));
        assert!(super::is_prune_eligible(
            eligible_at + Duration::from_secs(1),
            eligible_at
        ));
    }
}
