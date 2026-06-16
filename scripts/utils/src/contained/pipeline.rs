use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use op_succinct_estimator::{memory::WorkKind, window::WindowPredictor, Estimator};
use op_succinct_host_utils::{
    block_range::{split_range_based_on_safe_heads_memoized, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
    host::OPSuccinctHost,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Semaphore;

use crate::contained::admission::{current_rss_bytes, Admission};

/// Finality buffer (L1 blocks) the DA host adds when deriving a range's `l1_head`
/// (EigenDA/Ethereum `calculate_safe_l1_head` = `min(get_l1_head(end) + 20, finalized_l1)`).
/// A sub-range is only safe to persist once L1 has finalized past `l1_head + buffer`, so the
/// `min(.., finalized_l1)` cap never binds and the baked-in `l1_head` is deterministic — i.e.
/// equals what the executor recomputes later, keeping the (l1_head-free) cache key sound.
/// Must be >= the host's buffer.
const L1_HEAD_FINALITY_BUFFER: u64 = 20;

/// A sub-range is safe to *persist* only once its end block is L2-finalized AND L1 has
/// finalized past its `l1_head` plus the DA finality buffer (so the witness's capped
/// `l1_head` is deterministic). Keeps the `(chain_id,start,end,da_type)` cache key sound.
pub fn sub_range_ready_to_persist(
    range: &SpanBatchRange,
    finalized_l2: u64,
    range_l1_head: u64,
    finalized_l1: u64,
    l1_head_buffer: u64,
) -> bool {
    range.end <= finalized_l2 && range_l1_head + l1_head_buffer <= finalized_l1
}

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// One iteration of the predictive pipeline.
///
/// Splits the CURRENT window incrementally: anchored at the window start (a proposal
/// boundary, so the sub-range boundaries match the executor's per-game split) but only up to
/// the finalized L2 head. Every sub-range that ends BELOW the finalized frontier has the same
/// boundary it will have in the executor's eventual full-window split, so it can be built as
/// soon as ITS end finalizes — we do NOT wait for the whole window's end to finalize.
///
/// The single sub-range that touches the finalized frontier ends at an artificial cap (not a
/// real game boundary), so it's held back until the finalized head advances past its real
/// end. The window's frontier only advances once every sub-range up to `window.end` is built.
pub async fn pipeline_step<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    predictor: &mut WindowPredictor,
    permits: &Arc<Semaphore>,
    admission: &Admission,
    safe_head_cache: &mut HashMap<u64, u64>,
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
    if !predictor.within_lead(&window, finalized_l2) {
        return Ok(()); // lead cap reached
    }

    // Split anchored at the window start, but only up to the finalized frontier: a sub-range
    // can be built as soon as ITS end finalizes, not the whole window's end.
    let split_end = finalized_l2.min(window.end);
    if split_end <= window.start {
        return Ok(()); // nothing in this window is finalized yet
    }

    // Memoized: each tick queries only the L1 blocks newly finalized since the last tick.
    let sub_ranges = split_range_based_on_safe_heads_memoized(
        fetcher,
        window.start,
        split_end,
        batch_size,
        safe_head_cache,
    )
    .await?;

    // If we have not reached the window end, the LAST sub-range ends at the finalized cap (an
    // artifact, not a real game boundary) — hold it until finalized passes its real end. Every
    // sub-range below it has boundaries identical to the executor's full-window split.
    let window_complete = split_end >= window.end;
    let buildable = if window_complete {
        sub_ranges.as_slice()
    } else {
        &sub_ranges[..sub_ranges.len().saturating_sub(1)]
    };

    // L1-head persist gate (spec §4.4): fetched once and reused for all sub-ranges this tick.
    let finalized_l1 = fetcher.get_l1_header(BlockId::finalized()).await?.number;

    let builds = buildable.iter().map(|range| async move {
        // Already built on a prior tick — nothing to do (and skip the gate's RPC).
        if estimator.cache.has_stdin(range.start, range.end) {
            return true;
        }
        // Per-range L1-head gate: defer until L1 finalizes past l1_head + buffer, so the
        // witness's capped l1_head is deterministic. A failed lookup also defers the range.
        let range_l1_head = match fetcher.get_safe_l1_block_for_l2_block(range.end).await {
            Ok((_, l1)) => l1,
            Err(e) => {
                tracing::debug!(
                    start = range.start,
                    end = range.end,
                    error = %e,
                    "skipping sub-range: safe-head L1 lookup failed; will retry next tick"
                );
                return false;
            }
        };
        if !sub_range_ready_to_persist(
            range,
            finalized_l2,
            range_l1_head,
            finalized_l1,
            L1_HEAD_FINALITY_BUFFER,
        ) {
            tracing::debug!(
                start = range.start,
                end = range.end,
                range_l1_head,
                finalized_l1,
                "skipping sub-range: L1 not finalized past l1_head + buffer (defer persist)"
            );
            return false;
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
                return false;
            }
        };
        // Adaptive admission: block until this build's projected peak fits the budget given
        // live cgroup usage; the semaphore stays as the hard concurrency cap.
        admission.admit(WorkKind::Build, gas).await;
        let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");
        if let Err(e) = estimator.build_range_witness(range).await {
            tracing::warn!(start = range.start, end = range.end, error = %e, "pipeline build failed");
            return false;
        }
        admission.record(WorkKind::Build, gas, current_rss_bytes());
        true
    });
    let results = futures::future::join_all(builds).await;

    // Advance the window frontier only once the whole window is built (reached window.end and
    // every sub-range succeeded). Otherwise revisit it next tick — already-built ranges are
    // skipped via the has_stdin fast-path. Clear the safe-head cache on advance: the next
    // window's L1 range is disjoint, so retained entries are dead weight.
    if window_complete && results.iter().all(|&built| built) {
        predictor.advance_to(window.end);
        safe_head_cache.clear();
    }
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
        assert!(!sub_range_ready_to_persist(&r(200), 199, 50, 100, 0));
        assert!(sub_range_ready_to_persist(&r(200), 200, 50, 100, 0));
    }

    #[test]
    fn not_ready_until_l1_finalized_past_l1_head() {
        assert!(!sub_range_ready_to_persist(&r(200), 200, 150, 100, 0));
        assert!(sub_range_ready_to_persist(&r(200), 200, 100, 100, 0));
    }

    #[test]
    fn l1_head_buffer_must_also_be_finalized() {
        // l1_head=90, finalized_l1=100: the bare head is finalized (90 <= 100) but the +20
        // buffer is not (110 > 100), so the witness's capped l1_head would be non-deterministic
        // → defer. With l1_head=80, the buffer (100) is exactly finalized → ready.
        assert!(!sub_range_ready_to_persist(&r(200), 200, 90, 100, 20));
        assert!(sub_range_ready_to_persist(&r(200), 200, 80, 100, 20));
    }
}
