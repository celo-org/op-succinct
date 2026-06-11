use std::sync::Arc;

use op_succinct_estimator::{aggregate_execution_stats, Estimator, EstimatorError};
use op_succinct_host_utils::{
    block_range::{split_range_based_on_safe_heads_with_fetcher, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
    host::OPSuccinctHost,
    stats::ExecutionStats,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Semaphore;

use crate::contained::discovery::GameData;

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Execute every safe-head sub-range of a game and aggregate the stats. A cache hit
/// (pipeline prebuilt the stdin) skips host.run; a miss builds on demand. Each execute
/// holds an RSS-admission permit. Returns the aggregate AND the sub-ranges (so the
/// caller can schedule stdin pruning per range after the game succeeds).
pub async fn execute_game<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    permits: &Arc<Semaphore>,
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
        let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");
        let stats = estimator.execute_range(range).await?;
        per_range.push(stats);
    }
    Ok((aggregate_execution_stats(&per_range, 0, 0), sub_ranges))
}
