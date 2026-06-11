use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use anyhow::anyhow;
use op_succinct_estimator::{
    aggregate_execution_stats, memory::WorkKind, Estimator, EstimatorError,
};
use op_succinct_host_utils::{
    block_range::{split_range_based_on_safe_heads_with_fetcher, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
    host::OPSuccinctHost,
    stats::ExecutionStats,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Semaphore;

use crate::contained::{
    admission::{current_rss_bytes, Admission},
    discovery::GameData,
};

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Execute every safe-head sub-range of a game and aggregate the stats. A cache hit
/// (pipeline prebuilt the stdin) skips host.run; a miss builds on demand. Each execute
/// holds an RSS-admission permit. Returns the aggregate AND the sub-ranges (so the
/// caller can schedule stdin pruning per range after the game succeeds).
///
/// `execute_started` is set to `Some(now)` around EACH sub-range's `execute_range` and
/// cleared afterwards, so the overrun watchdog measures ONE sub-range (matching the
/// singular ALERT text). `admission_frozen` is checked at the top of each sub-range: if
/// the watchdog has frozen admission mid-game, the remaining sub-ranges are deferred and
/// the game is requeued (its already-built stdins are cached, so the retry is cheap).
pub async fn execute_game<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    permits: &Arc<Semaphore>,
    admission: &Admission,
    execute_started: &Arc<Mutex<Option<Instant>>>,
    admission_frozen: &Arc<AtomicBool>,
    game: &GameData,
    batch_size: u64,
) -> Result<(ExecutionStats, Vec<SpanBatchRange>), EstimatorError>
where
    // Mirror Estimator<H>'s impl rkyv bounds so this can call execute_range/build_range_witness.
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive,
    <WitnessOf<H> as rkyv::Archive>::Archived:
        rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
            + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    let sub_ranges = split_range_based_on_safe_heads_with_fetcher(
        fetcher,
        game.start_block,
        game.end_block,
        batch_size,
    )
    .await
    .map_err(EstimatorError::classify)?;

    let mut per_range = Vec::with_capacity(sub_ranges.len());
    for range in &sub_ranges {
        // Overrun watchdog froze admission mid-game: stop draining the remaining
        // sub-ranges. Requeue the whole game (its built stdins are cached, so the retry
        // re-executes only the unfinished sub-ranges cheaply).
        if admission_frozen.load(Ordering::SeqCst) {
            return Err(EstimatorError::Transient(anyhow!(
                "admission frozen mid-game; deferring remaining sub-ranges"
            )));
        }
        // Gas-weighted RSS projection key: sum the sub-range's L2 block gas. One extra
        // (cheap) fetch versus the SP1 execute that follows.
        let block_data = fetcher
            .get_l2_block_data_range(range.start, range.end)
            .await
            .map_err(EstimatorError::classify)?;
        let gas: u64 = block_data.iter().map(|b| b.gas_used).sum();
        // Adaptive admission: block until this unit is projected to fit the budget given
        // live cgroup usage; the semaphore stays as the hard concurrency cap.
        admission.admit(WorkKind::Execute, gas).await;
        let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");
        // Mark the start so the watchdog measures THIS single sub-range execute, and clear
        // it right after the await (before `?`) so a failed execute leaves no stale start
        // and we never hold the std Mutex across the await.
        *execute_started.lock().unwrap() = Some(Instant::now());
        let result = estimator.execute_range(range).await;
        *execute_started.lock().unwrap() = None;
        let stats = result?;
        admission.record(WorkKind::Execute, gas, current_rss_bytes());
        per_range.push(stats);
    }
    Ok((aggregate_execution_stats(&per_range, 0, 0), sub_ranges))
}
