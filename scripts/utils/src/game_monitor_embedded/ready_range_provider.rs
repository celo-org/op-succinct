use alloy_eips::BlockId;
use op_succinct_host_utils::{
    block_range::{split_range_basic, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
};

use crate::game_monitor_embedded::readiness::range_ready;

/// Provides the next sub-range ready to be processed, ahead of the executor.
///
/// It owns everything needed to answer one question — "what's the next range ready to
/// process?" — so the caller is free to do whatever it wants with each range it receives
/// (build a witness, estimate, prove, …):
///   * predicts game windows from the proposal cadence (`[window_start, window_start +
///     interval]`, which equals the next game's `[start, end]`),
///   * splits each window into fixed-size (`batch_size`) sub-ranges **anchored at the window
///     start**, so the boundaries match the executor's split of the real game (cache keys line up),
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
}

impl ReadyRangeProvider {
    /// `seed` must be a real proposal boundary (the latest on-chain game's `end_block`) so the
    /// predicted windows align with future games — see `latest_game_end_block` in the daemon.
    pub fn new(seed: u64, proposal_interval: u64, batch_size: u64) -> Self {
        assert!(proposal_interval > 0, "proposal_interval must be > 0");
        Self { proposal_interval, batch_size, window_start: seed, cursor: seed }
    }

    /// The next sub-range ready to build, or `None` if nothing is ready right now (the caller
    /// polls again next tick). All window prediction, splitting, and finalization gating
    /// happen here. Transient RPC failures return `None` (retry next tick).
    pub async fn next_range(&mut self, fetcher: &OPSuccinctDataFetcher) -> Option<SpanBatchRange> {
        // Finalized L2 head bounds which sub-ranges are even considered (splitting only up to
        // the finalized prefix keeps boundaries stable); per-range readiness is gated below by
        // the shared `range_ready`.
        let finalized_l2 = match fetcher.get_l2_header(BlockId::finalized()).await {
            Ok(h) => h.number,
            Err(e) => {
                tracing::debug!(error = %e, "ready range provider: finalized-L2 fetch failed; retry next tick");
                return None;
            }
        };

        // Whole window handed out → advance to the next predicted game window. At most one
        // advance per call: afterwards cursor == window_start, so the next window_end is a
        // full interval ahead.
        if self.cursor >= self.window_start + self.proposal_interval {
            self.window_start += self.proposal_interval;
            self.cursor = self.window_start;
        }
        let window_end = self.window_start + self.proposal_interval;

        // Only consider the finalized prefix of the window. Nothing new finalized → done.
        let split_end = finalized_l2.min(window_end);
        if self.cursor >= split_end {
            return None;
        }

        // Fixed-size split (no SafeDB) anchored at the window start so boundaries match the
        // executor's split of the real game `[window_start, window_end]` — both anchor at the
        // same start with the same `batch_size`, so the cache keys line up.
        let sub_ranges = split_range_basic(self.window_start, split_end, self.batch_size);

        // Locate the sub-range starting at the cursor (a real boundary from a prior hand-out,
        // stable because boundaries below the finalized head don't move).
        let idx = sub_ranges.iter().position(|r| r.start == self.cursor)?;

        // The last sub-range of an INCOMPLETE window ends at the finalized cap — an
        // artifact, not a real game boundary — so hold it until finalization reveals its
        // real end. When the window is complete, every boundary is real.
        let window_complete = split_end >= window_end;
        if idx + 1 == sub_ranges.len() && !window_complete {
            return None;
        }

        let range = sub_ranges[idx].clone();

        // Readiness gate (shared with the executor): defer until the range end is finalized on
        // L2 AND L1 has finalized past its `l1_head + buffer`. Ranges are emitted in order and
        // both conditions are monotonic in `range.end`, so if this one isn't ready none after it
        // are either — wait rather than skip. A transient RPC failure is treated the same.
        match range_ready(fetcher, range.end, true).await {
            Ok(true) => {}
            Ok(false) => return None,
            Err(e) => {
                tracing::debug!(start = range.start, end = range.end, error = %e,
                    "ready range provider: readiness check failed; retry next tick");
                return None;
            }
        }

        self.cursor = range.end;
        Some(range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_seeds_cursor_and_window_at_proposal_boundary() {
        let s = ReadyRangeProvider::new(1000, 200, 50);
        assert_eq!(s.window_start, 1000);
        assert_eq!(s.cursor, 1000);
        assert_eq!(s.proposal_interval, 200);
        assert_eq!(s.batch_size, 50);
    }
}
