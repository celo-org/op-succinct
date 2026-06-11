use op_succinct_host_utils::block_range::SpanBatchRange;

/// Predicts proposal windows from the proposer's `PROPOSAL_INTERVAL`.
#[derive(Debug, Clone)]
pub struct WindowPredictor {
    proposal_interval: u64,
    /// Last built window end (the frontier).
    frontier: u64,
    /// Max windows the pipeline may run ahead of the frontier.
    max_lead_windows: u64,
}

impl WindowPredictor {
    pub fn new(start_frontier: u64, proposal_interval: u64, max_lead_windows: u64) -> Self {
        assert!(proposal_interval > 0, "PROPOSAL_INTERVAL must be > 0");
        Self { proposal_interval, frontier: start_frontier, max_lead_windows: max_lead_windows.max(1) }
    }

    pub fn frontier(&self) -> u64 {
        self.frontier
    }

    /// The next predicted window after the frontier.
    pub fn next_window(&self) -> SpanBatchRange {
        SpanBatchRange { start: self.frontier, end: self.frontier + self.proposal_interval }
    }

    /// Whether predicting `window.end` stays within the lead cap relative to a known
    /// finalized L2 head — prevents speculating arbitrarily far ahead.
    pub fn within_lead(&self, window: &SpanBatchRange, finalized_l2: u64) -> bool {
        let lead = window.end.saturating_sub(finalized_l2);
        lead <= self.proposal_interval * self.max_lead_windows
    }

    /// Advance the frontier once a window has been built.
    pub fn advance_to(&mut self, built_end: u64) {
        if built_end > self.frontier {
            self.frontier = built_end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicts_next_window_from_interval() {
        let p = WindowPredictor::new(1000, 200, 3);
        let w = p.next_window();
        assert_eq!(w.start, 1000);
        assert_eq!(w.end, 1200);
    }

    #[test]
    fn advances_frontier_monotonically() {
        let mut p = WindowPredictor::new(1000, 200, 3);
        p.advance_to(1200);
        assert_eq!(p.frontier(), 1200);
        p.advance_to(900); // never goes backwards
        assert_eq!(p.frontier(), 1200);
        assert_eq!(p.next_window().start, 1200);
    }

    #[test]
    fn lead_cap_bounds_speculation() {
        let p = WindowPredictor::new(1000, 200, 2); // cap = 400 ahead of finalized
        let near = SpanBatchRange { start: 1000, end: 1200 };
        let far = SpanBatchRange { start: 2000, end: 2200 };
        assert!(p.within_lead(&near, 1000));
        assert!(!p.within_lead(&far, 1000));
    }
}
