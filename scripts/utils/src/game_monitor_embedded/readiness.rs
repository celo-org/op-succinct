//! Shared readiness gate for building/executing a range.
//!
//! A range (a pipeline sub-range, or a game's `[start, end]`) may only be built or executed
//! once it is *finalized on both sides*:
//!   * its **end block is finalized on L2**, so the batch covering it is already in finalized
//!     L1 and the data needed to derive it is available; and
//!   * **L1 has finalized past its `l1_head + buffer`**, so the host's `calculate_safe_l1_head`
//!     finality cap (`min(l1_head + 20, finalized_l1)`) never binds and the witness gets the
//!     host's full `+ 20` read-ahead slack. Derivation can need L1 blocks beyond the
//!     batch-posting block (the reason for the `+ 20` is unexplained; see the FIXME in
//!     `utils/ethereum/host/src/host.rs`) and cannot walk past the baked-in `l1_head`, so a
//!     capped head risks a failed `host.run` near the finality frontier. A minor bonus: the
//!     baked-in head no longer depends on when the witness is built, so an evicted blob
//!     rebuilds identically.
//!
//! Both the predictive pipeline (`ready_range_provider`) and the reactive executor gate on this
//! single function, so the readiness rule lives in exactly one place.

use alloy_eips::BlockId;
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;

/// L1 finality buffer (blocks). MUST match the host's `calculate_safe_l1_head` offset
/// (`+ 20` in `utils/ethereum/host/src/host.rs`): a range is only ready once L1 has finalized
/// past `l1_head + L1_HEAD_FINALITY_BUFFER`, which is exactly what keeps that host's
/// `min(l1_head + 20, finalized_l1)` cap from binding.
pub const L1_HEAD_FINALITY_BUFFER: u64 = 20;

/// True once L2 has finalized the range's end block.
fn l2_finalized(finalized_l2: u64, end_block: u64) -> bool {
    finalized_l2 >= end_block
}

/// True once L1 has finalized past the range's `l1_head` plus the finality buffer.
fn l1_head_finalized(l1_head: u64, finalized_l1: u64, buffer: u64) -> bool {
    l1_head + buffer <= finalized_l1
}

/// Whether the range/game ending at `end_block` is ready to build or execute: its end
/// block is finalized on L2 AND L1 is finalized past its `l1_head + [`L1_HEAD_FINALITY_BUFFER`]`.
///
/// A transient RPC failure surfaces as `Err`; callers treat that the same as "not ready"
/// (re-queue / retry next tick) rather than a hard failure. `get_l1_head(.., true)` uses SafeDB
/// when present and otherwise falls back to timestamp-based estimation, matching what the
/// executor's witness bakes in — so the daemon needs no SafeDB.
pub async fn range_ready(
    fetcher: &OPSuccinctDataFetcher,
    end_block: u64,
    safe_db_fallback: bool,
) -> anyhow::Result<bool> {
    let finalized_l2 = fetcher.get_l2_header(BlockId::finalized()).await?.number;
    if !l2_finalized(finalized_l2, end_block) {
        return Ok(false);
    }

    let (_, l1_head) = fetcher.get_l1_head(end_block, safe_db_fallback).await?;
    let finalized_l1 = fetcher.get_l1_header(BlockId::finalized()).await?.number;
    Ok(l1_head_finalized(l1_head, finalized_l1, L1_HEAD_FINALITY_BUFFER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_finalized_requires_end_block_at_or_below_finalized() {
        assert!(!l2_finalized(99, 100));
        assert!(l2_finalized(100, 100));
        assert!(l2_finalized(150, 100));
    }

    #[test]
    fn l1_head_finalized_requires_buffer_past_finalized() {
        // bare head finalized but +20 buffer not → not ready (the host's cap would bind).
        assert!(!l1_head_finalized(90, 100, 20));
        // buffer exactly finalized → ready.
        assert!(l1_head_finalized(80, 100, 20));
        // zero buffer reduces to a plain finalized check.
        assert!(l1_head_finalized(100, 100, 0));
        assert!(!l1_head_finalized(101, 100, 0));
    }
}
