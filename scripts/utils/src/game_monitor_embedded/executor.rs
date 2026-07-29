use op_succinct_estimator::{
    aggregate_execution_stats, memory::WorkKind, Estimator, EstimatorError,
};
use op_succinct_host_utils::{
    block_range::{split_range_basic, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
    host::OPSuccinctHost,
    stats::ExecutionStats,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;

use crate::game_monitor_embedded::{
    admission::Admission,
    discovery::GameData,
    scheduler::{RangeKey, Scheduler},
};

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Execute ONE sub-range: proof-cache hit returns the cached stats; a miss runs the SP1
/// execute under an admission slot and persists the result. Run by the execute workers —
/// the caller (worker) has already ensured the range's stdin exists (or put the demand
/// into the waiting set).
///
/// Runs in a per-range span (spec §4.6) — workers have no `game` context, so executes are
/// attributed to `range` alone, with the `execute` child span nested underneath.
#[tracing::instrument(
    name = "range",
    skip_all,
    fields(start = range.start, end = range.end, work = "execute")
)]
pub async fn execute_step<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    admission: &Admission,
    range: &SpanBatchRange,
) -> Result<ExecutionStats, EstimatorError>
where
    // Mirror Estimator<H>'s impl rkyv bounds so this can call execute_range.
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive,
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    // Proof-cache hit: the execute already ran (a prior attempt, a speculative execute, or a
    // pre-restart run) — reuse it, no admission slot needed.
    if let Some(stats) = estimator
        .cache
        .load_stats(range.start, range.end)
        .map_err(EstimatorError::Transient)?
    {
        tracing::debug!("proof cache hit; skipping execute");
        return Ok(stats);
    }

    // Gas-weighted RSS projection key: sum the sub-range's L2 block gas. Threaded into
    // `execute_range`, which reuses it for stats instead of re-fetching.
    let block_data = fetcher
        .get_l2_block_data_range(range.start, range.end)
        .await
        .map_err(EstimatorError::classify)?;
    let gas: u64 = block_data.iter().map(|b| b.gas_used).sum();
    // Adaptive admission: block until this unit fits the memory budget and the concurrency
    // cap. The guard keeps the execute's gas and slot registered until it drops.
    let _admit = admission.admit(WorkKind::Execute, gas).await;
    let stats = estimator.execute_range(range, &block_data).await?;

    // Persist to the proof cache. Best-effort: the result is still returned on a write
    // failure, the range just re-executes if needed again.
    if let Err(e) = estimator.cache.save_stats(range.start, range.end, &stats) {
        tracing::warn!(error = %e, "failed to persist execution stats");
    }
    Ok(stats)
}

/// Execute a game by demanding its sub-ranges from the scheduler and assembling the results
/// from the proof cache.
///
/// The game itself does no heavy work: it splits `[start_block, end_block]` into fixed
/// `batch_size` sub-ranges (anchored at the game start, matching the speculative build
/// task so cache keys line up), demands whatever the proof cache is missing, and waits.
/// Demands go
/// onto the execute pool's FIFO priority queue — served before all speculative work, in the
/// order games issued them — and an un-built range promotes a witness demand rather than
/// building inline. A game whose ranges were all pre-computed completes without executing
/// anything.
///
/// A recorded failure of any demanded range fails the whole game (first error wins), feeding
/// the normal two-tier retry; completed ranges stay in the proof cache, so a retry only
/// re-runs what actually failed. On success returns the aggregate AND the sub-ranges so the
/// caller can schedule stdin pruning per range.
pub async fn execute_game<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    scheduler: &Scheduler,
    game: &GameData,
    batch_size: u64,
) -> Result<(ExecutionStats, Vec<SpanBatchRange>), EstimatorError>
where
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive,
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    let sub_ranges = split_range_basic(game.start_block, game.end_block, batch_size);
    let keys: Vec<RangeKey> = sub_ranges.iter().map(|r| (r.start, r.end)).collect();
    // This game is now the newest known — raise the speculative execute limit.
    scheduler.note_game_end(game.end_block);

    loop {
        // Register interest in completions BEFORE checking the cache, so a unit finishing
        // between the check and the await still wakes us (no lost wakeup).
        let completed = scheduler.completed_notified();
        tokio::pin!(completed);
        completed.as_mut().enable();

        let mut per_range = Vec::with_capacity(sub_ranges.len());
        let mut missing: Vec<RangeKey> = Vec::new();
        for r in &sub_ranges {
            match estimator
                .cache
                .load_stats(r.start, r.end)
                .map_err(EstimatorError::Transient)?
            {
                Some(stats) => per_range.push(stats),
                None => missing.push((r.start, r.end)),
            }
        }
        if missing.is_empty() {
            return Ok((aggregate_execution_stats(&per_range, 0, 0), sub_ranges));
        }

        // A recorded failure of any demanded range fails the game (first error wins). Units
        // still in flight for the other ranges keep running; their results land in the proof
        // cache and a retry reuses them.
        if let Some(err) = scheduler.take_error(&keys) {
            let e = anyhow::anyhow!(err.message);
            return Err(if err.transient {
                EstimatorError::Transient(e)
            } else {
                EstimatorError::Fatal(e)
            });
        }

        // (Re-)demand the missing ranges — dedup'd inside the scheduler, so re-demanding an
        // in-flight range is a no-op.
        for key in missing {
            scheduler.demand_execute(key);
        }
        // Fallback tick guards against any missed notification wedging the game silently.
        let _ = tokio::time::timeout(crate::game_monitor_embedded::scheduler::IDLE_RECHECK, completed)
            .await;
    }
}
