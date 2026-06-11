use std::sync::Arc;

use alloy_eips::BlockId;
use op_succinct_estimator::{memory::WorkKind, window::WindowPredictor, Estimator};
use op_succinct_host_utils::{
    block_range::{split_range_based_on_safe_heads_with_fetcher, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
    host::OPSuccinctHost,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Semaphore;

use crate::contained::admission::{current_rss_bytes, Admission};

/// A sub-range is safe to *persist* only once its end block is L2-finalized AND L1 has
/// finalized past its `l1_head` (spec key-soundness). Keeps the
/// `(chain_id,start,end,da_type)` cache key deterministic.
pub fn sub_range_ready_to_persist(
    range: &SpanBatchRange,
    finalized_l2: u64,
    range_l1_head: u64,
    finalized_l1: u64,
) -> bool {
    range.end <= finalized_l2 && range_l1_head <= finalized_l1
}

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// One iteration: predict the next window, compute its sub-ranges if its blocks exist,
/// and build each ready sub-range (RSS-admitted via `permits`). Advances the frontier
/// past the built window.
pub async fn pipeline_step<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    predictor: &mut WindowPredictor,
    permits: &Arc<Semaphore>,
    admission: &Admission,
    batch_size: u64,
) -> anyhow::Result<()>
where
    // Mirror Estimator<H>'s impl rkyv bounds so this can call build_range_witness.
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
    // Real finalized-L2 head; on any RPC error do nothing this tick.
    let finalized_l2 = fetcher
        .get_l2_header(BlockId::finalized())
        .await
        .map(|h| h.number)
        .unwrap_or(predictor.frontier());

    let window = predictor.next_window();
    if window.end > finalized_l2 || !predictor.within_lead(&window, finalized_l2) {
        return Ok(()); // window's blocks don't exist yet, or lead cap reached
    }

    let sub_ranges = split_range_based_on_safe_heads_with_fetcher(
        fetcher,
        window.start,
        window.end,
        batch_size,
    )
    .await?;

    // L1-head persist gate (spec §4.4): a sub-range's stdin is only safe to persist once
    // L1 has finalized past the range's `l1_head`. Fetched once and reused for all
    // sub-ranges this tick.
    let finalized_l1 = fetcher.get_l1_header(BlockId::finalized()).await?.number;

    let builds = sub_ranges.iter().map(|range| async move {
        // Per-range L1-head gate: skip (don't persist) until L1 finalizes past this
        // range's l1_head. A failed safe-head lookup also defers the range to a later tick.
        let range_l1_head = match fetcher.get_safe_l1_block_for_l2_block(range.end).await {
            Ok((_, l1)) => l1,
            Err(e) => {
                tracing::debug!(
                    start = range.start,
                    end = range.end,
                    error = %e,
                    "skipping sub-range: safe-head L1 lookup failed; will retry next tick"
                );
                return;
            }
        };
        if !sub_range_ready_to_persist(range, finalized_l2, range_l1_head, finalized_l1) {
            tracing::debug!(
                start = range.start,
                end = range.end,
                finalized_l2,
                range_l1_head,
                finalized_l1,
                "skipping sub-range: L1 not finalized past l1_head (defer persist)"
            );
            return;
        }

        // Gas-weighted RSS projection key: sum the sub-range's L2 block gas.
        let gas: u64 = match fetcher.get_l2_block_data_range(range.start, range.end).await {
            Ok(bd) => bd.iter().map(|b| b.gas_used).sum(),
            Err(e) => {
                tracing::debug!(
                    start = range.start,
                    end = range.end,
                    error = %e,
                    "skipping sub-range: block-data fetch failed; will retry next tick"
                );
                return;
            }
        };
        // Adaptive admission: block until this build is projected to fit the budget given
        // live cgroup usage; the semaphore stays as the hard concurrency cap.
        admission.admit(WorkKind::Build, gas).await;
        let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");
        if let Err(e) = estimator.build_range_witness(range).await {
            tracing::warn!(start = range.start, end = range.end, error = %e, "pipeline build failed");
        }
        admission.record(WorkKind::Build, gas, current_rss_bytes());
    });
    futures::future::join_all(builds).await;

    predictor.advance_to(window.end);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(end: u64) -> SpanBatchRange {
        SpanBatchRange { start: end - 10, end }
    }

    #[test]
    fn not_ready_until_l2_finalized() {
        assert!(!sub_range_ready_to_persist(&r(200), 199, 50, 100));
        assert!(sub_range_ready_to_persist(&r(200), 200, 50, 100));
    }

    #[test]
    fn not_ready_until_l1_finalized_past_l1_head() {
        assert!(!sub_range_ready_to_persist(&r(200), 200, 150, 100));
        assert!(sub_range_ready_to_persist(&r(200), 200, 100, 100));
    }
}
