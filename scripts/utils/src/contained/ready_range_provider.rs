use std::collections::HashMap;

use alloy_eips::BlockId;
use op_succinct_host_utils::{
    block_range::{split_range_based_on_safe_heads_memoized, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
};

/// L1 finality buffer (blocks) matching the DA host's `calculate_safe_l1_head`
/// (`min(get_l1_head(end) + 20, finalized_l1)`). A range is only handed out once L1 has
/// finalized past its `l1_head + buffer`, so the `min(.., finalized_l1)` cap never binds and
/// the witness's baked-in `l1_head` is deterministic — i.e. equals what the executor
/// recomputes later, keeping the (l1_head-free) cache key sound. Must be >= the host's buffer.
const L1_HEAD_FINALITY_BUFFER: u64 = 20;

/// True once L1 has finalized past a range's `l1_head` plus the finality buffer.
fn l1_head_finalized(range_l1_head: u64, finalized_l1: u64, buffer: u64) -> bool {
    range_l1_head + buffer <= finalized_l1
}

/// Provides the next sub-range ready to be processed, ahead of the executor.
///
/// It owns everything needed to answer one question — "what's the next range ready to
/// process?" — so the caller is free to do whatever it wants with each range it receives
/// (build a witness, estimate, prove, …):
///   * predicts game windows from the proposal cadence (`[frontier, frontier + interval]`, which
///     equals the next game's `[start, end]`),
///   * splits each window into safe-head sub-ranges **anchored at the window start**, so the
///     boundaries match the executor's split of the real game (cache keys line up),
///   * hands them out one at a time as each becomes soundly ready: its end is L2-finalized AND L1
///     has finalized past its `l1_head + buffer`.
///
/// State is just a forward-only block cursor. Downstream failures are the caller's problem;
/// the provider never needs completion feedback.
pub struct ReadyRangeProvider {
    proposal_interval: u64,
    batch_size: u64,
    /// Start of the current game window (a proposal boundary).
    window_start: u64,
    /// Next sub-range start to hand out (`window_start <= cursor <= window_start + interval`).
    cursor: u64,
    /// Memoized `safeHeadAtL1Block` lookups (immutable for finalized L1 blocks); the split
    /// re-queries only newly-finalized blocks. Cleared when the window advances.
    safe_head_cache: HashMap<u64, u64>,
}

impl ReadyRangeProvider {
    /// `seed` must be a real proposal boundary (the latest on-chain game's `end_block`) so the
    /// predicted windows align with future games — see `latest_game_end_block` in the daemon.
    pub fn new(seed: u64, proposal_interval: u64, batch_size: u64) -> Self {
        assert!(proposal_interval > 0, "proposal_interval must be > 0");
        Self {
            proposal_interval,
            batch_size,
            window_start: seed,
            cursor: seed,
            safe_head_cache: HashMap::new(),
        }
    }

    /// The next sub-range ready to build, or `None` if nothing is ready right now (the caller
    /// polls again next tick). All window prediction, splitting, and finalization gating
    /// happen here. Transient RPC failures return `None` (retry next tick).
    pub async fn next_range(&mut self, fetcher: &OPSuccinctDataFetcher) -> Option<SpanBatchRange> {
        let finalized_l2 = match fetcher.get_l2_header(BlockId::finalized()).await {
            Ok(h) => h.number,
            Err(e) => {
                tracing::debug!(error = %e, "ready range provider: finalized-L2 fetch failed; retry next tick");
                return None;
            }
        };
        let finalized_l1 = match fetcher.get_l1_header(BlockId::finalized()).await {
            Ok(h) => h.number,
            Err(e) => {
                tracing::debug!(error = %e, "ready range provider: finalized-L1 fetch failed; retry next tick");
                return None;
            }
        };

        // Whole window handed out → advance to the next predicted game window. At most one
        // advance per call: afterwards cursor == window_start, so the next window_end is a
        // full interval ahead.
        if self.cursor >= self.window_start + self.proposal_interval {
            self.window_start += self.proposal_interval;
            self.cursor = self.window_start;
            self.safe_head_cache.clear();
        }
        let window_end = self.window_start + self.proposal_interval;

        // Only consider the finalized prefix of the window. Nothing new finalized → done.
        let split_end = finalized_l2.min(window_end);
        if self.cursor >= split_end {
            return None;
        }

        // Split anchored at the window start so boundaries match the executor's split of
        // the real game `[window_start, window_end]`.
        let sub_ranges = match split_range_based_on_safe_heads_memoized(
            fetcher,
            self.window_start,
            split_end,
            self.batch_size,
            &mut self.safe_head_cache,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "ready range provider: split failed; retry next tick");
                return None;
            }
        };

        // Locate the sub-range starting at the cursor (a real boundary from a prior hand-out,
        // stable because boundaries below the finalized frontier don't move).
        let idx = sub_ranges.iter().position(|r| r.start == self.cursor)?;

        // The last sub-range of an INCOMPLETE window ends at the finalized cap — an
        // artifact, not a real game boundary — so hold it until finalization reveals its
        // real end. When the window is complete, every boundary is real.
        let window_complete = split_end >= window_end;
        if idx + 1 == sub_ranges.len() && !window_complete {
            return None;
        }

        let range = sub_ranges[idx].clone();

        // Soundness gate: defer until L1 has finalized past `l1_head + buffer`. Ranges are
        // emitted in order and `l1_head` is monotonic in `range.end`, so if this one isn't
        // ready none after it are either — wait rather than skip.
        let range_l1_head = match fetcher.get_safe_l1_block_for_l2_block(range.end).await {
            Ok((_, l1)) => l1,
            Err(e) => {
                tracing::debug!(start = range.start, end = range.end, error = %e,
                    "ready range provider: safe-head L1 lookup failed; retry next tick");
                return None;
            }
        };
        if !l1_head_finalized(range_l1_head, finalized_l1, L1_HEAD_FINALITY_BUFFER) {
            return None;
        }

        self.cursor = range.end;
        Some(range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l1_head_finalized_requires_buffer_past_finalized() {
        // bare head finalized but +20 buffer not → not ready (cap would bind).
        assert!(!l1_head_finalized(90, 100, 20));
        // buffer exactly finalized → ready.
        assert!(l1_head_finalized(80, 100, 20));
        // zero buffer reduces to a plain finalized check.
        assert!(l1_head_finalized(100, 100, 0));
        assert!(!l1_head_finalized(101, 100, 0));
    }

    #[test]
    fn new_seeds_cursor_and_window_at_proposal_boundary() {
        let s = ReadyRangeProvider::new(1000, 200, 50);
        assert_eq!(s.window_start, 1000);
        assert_eq!(s.cursor, 1000);
        assert_eq!(s.proposal_interval, 200);
        assert_eq!(s.batch_size, 50);
    }
}
