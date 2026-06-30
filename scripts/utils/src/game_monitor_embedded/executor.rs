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
use tracing::Instrument;

use crate::game_monitor_embedded::{admission::Admission, discovery::GameData};

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Execute every safe-head sub-range of a game CONCURRENTLY and aggregate the stats.
/// A cache hit (pipeline prebuilt the stdin) skips host.run; a miss builds on demand.
/// Each execute passes through admission, which gates on both the projected memory budget
/// and the hard concurrency cap, so the number of sub-ranges actually running at once is
/// bounded regardless of how many ranges (or games) are in flight. Returns the aggregate
/// AND the sub-ranges (so the caller can schedule stdin pruning per range after the game
/// succeeds).
pub async fn execute_game<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    admission: &Admission,
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
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
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

    // One future per sub-range; they run concurrently and are gated by admission, which
    // bounds both projected memory and the in-flight count. Each runs in a per-range child
    // span (spec §4.6) so its logs — and the host.run/get_sp1_stdin/execute child spans
    // inside the estimator — are attributable to `range` under the enclosing `game` span.
    let range_futures = sub_ranges.iter().map(|range| {
        let range_span = tracing::info_span!("range", start = range.start, end = range.end);
        async move {
            // Gas-weighted RSS projection key: sum the sub-range's L2 block gas. Fetched
            // once here and threaded into `execute_range` below, which reuses it for stats
            // instead of re-fetching the same range.
            let block_data = fetcher
                .get_l2_block_data_range(range.start, range.end)
                .await
                .map_err(EstimatorError::classify)?;
            let gas: u64 = block_data.iter().map(|b| b.gas_used).sum();
            // Adaptive admission: block until this unit fits the memory budget and the
            // concurrency cap. The guard keeps the execute's gas and slot registered until
            // it drops.
            let _admit = admission.admit(WorkKind::Execute, gas).await;
            let stats = estimator.execute_range(range, &block_data).await?;
            Ok::<ExecutionStats, EstimatorError>(stats)
        }
        .instrument(range_span)
    });

    let results = futures::future::join_all(range_futures).await;

    // Propagate the first error (a successful sub-range's stdin is already cached, so a
    // later retry skips its build). Otherwise aggregate.
    let per_range = results.into_iter().collect::<Result<Vec<_>, _>>()?;
    Ok((aggregate_execution_stats(&per_range, 0, 0), sub_ranges))
}
