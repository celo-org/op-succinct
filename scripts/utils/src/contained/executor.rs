use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

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
    watchdog::Watchdog,
};

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Execute every safe-head sub-range of a game CONCURRENTLY and aggregate the stats.
/// A cache hit (pipeline prebuilt the stdin) skips host.run; a miss builds on demand.
/// Each execute draws an RSS-admission slot and a semaphore permit, so the number of
/// sub-ranges actually running at once is bounded by the shared memory budget — not by
/// how many ranges (or games) are in flight. Returns the aggregate AND the sub-ranges
/// (so the caller can schedule stdin pruning per range after the game succeeds).
///
/// Each running sub-range registers a [`Watchdog`] unit (after acquiring its permit) so
/// the overrun watchdog can observe ALL concurrent units. `admission_frozen` is checked
/// at the top of each sub-range: if the watchdog froze admission, sub-ranges that have
/// not yet started bail out and the game is requeued (its already-built stdins are
/// cached, so the retry re-executes only the unfinished sub-ranges cheaply).
pub async fn execute_game<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    permits: &Arc<Semaphore>,
    admission: &Admission,
    watchdog: &Watchdog,
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

    // One future per sub-range; they run concurrently and are gated by admission + the
    // shared semaphore (the hard concurrency cap).
    let range_futures = sub_ranges.iter().map(|range| {
        let estimator = estimator;
        let fetcher = fetcher;
        let admission = admission;
        let watchdog = watchdog;
        let permits = permits;
        let admission_frozen = admission_frozen;
        async move {
            // Overrun watchdog froze admission: don't start this sub-range. Surfacing a
            // Transient requeues the whole game; its built stdins are cached, so the retry
            // re-executes only the unfinished sub-ranges cheaply.
            if admission_frozen.load(Ordering::SeqCst) {
                return Err(EstimatorError::Transient(anyhow!(
                    "admission frozen mid-game; deferring sub-range {}-{}",
                    range.start,
                    range.end
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
            // Register with the watchdog AFTER acquiring the permit, so the registry tracks
            // work that is actually running. The guard deregisters on drop (incl. the `?`).
            let _unit =
                watchdog.enter(WorkKind::Execute, format!("execute {}-{}", range.start, range.end));
            let stats = estimator.execute_range(range).await?;
            admission.record(WorkKind::Execute, gas, current_rss_bytes());
            Ok::<ExecutionStats, EstimatorError>(stats)
        }
    });

    let results = futures::future::join_all(range_futures).await;

    // Propagate the first error (a successful sub-range's stdin is already cached, so a
    // later retry skips its build). Otherwise aggregate.
    let mut per_range = Vec::with_capacity(results.len());
    for result in results {
        per_range.push(result?);
    }
    Ok((aggregate_execution_stats(&per_range, 0, 0), sub_ranges))
}
