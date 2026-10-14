use std::collections::HashSet;

/// Tracks the end of a contiguous sequence of indices.
///
/// As indices are added (potentially out of order), the tracker eagerly advances
/// its `end` value through any contiguous run starting from `end + 1`.
pub struct SequenceTracker {
    end: u64,
    pending: HashSet<u64>,
}

impl SequenceTracker {
    /// Creates a new tracker with `end` set to the given value.
    pub fn new(end: u64) -> Self {
        Self { end, pending: HashSet::new() }
    }

    /// Returns the current end of the contiguous sequence.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Adds an index and eagerly advances `end` through any contiguous run.
    pub fn add(&mut self, index: u64) {
        if index <= self.end {
            return;
        }
        self.pending.insert(index);
        let mut check_from = self.end + 1;
        while self.pending.remove(&check_from) {
            self.end = check_from;
            check_from += 1;
        }
    }

    /// Rebuilds a tracker from persisted state: the contiguous `end` plus the out-of-order
    /// completions above it. Re-adding through `add` preserves the invariants — anything at or
    /// below `end` is dropped, and `end` advances across any now-contiguous run.
    pub fn restore(end: u64, pending: impl IntoIterator<Item = u64>) -> Self {
        let mut tracker = Self::new(end);
        for index in pending {
            tracker.add(index);
        }
        tracker
    }

    /// True if `index` is already completed — at or below the contiguous `end`, or recorded as
    /// an out-of-order completion above it.
    pub fn contains(&self, index: u64) -> bool {
        index <= self.end || self.pending.contains(&index)
    }

    /// The out-of-order completions above `end`, ascending. Persisted alongside `end` so a
    /// restart neither loses them nor re-runs them.
    pub fn pending_indices(&self) -> Vec<u64> {
        let mut indices: Vec<u64> = self.pending.iter().copied().collect();
        indices.sort_unstable();
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_additions() {
        let tracker = SequenceTracker::new(1);
        assert_eq!(tracker.end(), 1);
    }

    #[test]
    fn test_single_next() {
        let mut tracker = SequenceTracker::new(0);
        tracker.add(1);
        assert_eq!(tracker.end(), 1);
    }

    #[test]
    fn test_single_not_next() {
        let mut tracker = SequenceTracker::new(1);
        tracker.add(3);
        assert_eq!(tracker.end(), 1);
    }

    #[test]
    fn test_consecutive() {
        let mut tracker = SequenceTracker::new(0);
        tracker.add(1);
        tracker.add(2);
        tracker.add(3);
        tracker.add(4);
        assert_eq!(tracker.end(), 4);
    }

    #[test]
    fn test_out_of_order() {
        let mut tracker = SequenceTracker::new(0);
        tracker.add(3);
        tracker.add(1);
        tracker.add(2);
        tracker.add(5);
        assert_eq!(tracker.end(), 3);

        // Adding the missing index advances through the gap.
        tracker.add(4);
        assert_eq!(tracker.end(), 5);
    }

    #[test]
    fn test_duplicates() {
        let mut tracker = SequenceTracker::new(0);
        tracker.add(1);
        tracker.add(1);
        tracker.add(2);
        tracker.add(2);
        assert_eq!(tracker.end(), 2);
    }

    #[test]
    fn test_pending_cleared() {
        let mut tracker = SequenceTracker::new(0);
        tracker.add(1);
        tracker.add(2);
        tracker.add(4);
        tracker.add(5);
        assert_eq!(tracker.end(), 2);
        // 4 and 5 are still pending
        assert!(tracker.pending.contains(&4));
        assert!(tracker.pending.contains(&5));

        // Fill the gap
        tracker.add(3);
        assert_eq!(tracker.end(), 5);
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn test_add_at_or_below_end() {
        let mut tracker = SequenceTracker::new(5);
        tracker.add(3);
        tracker.add(5);
        assert_eq!(tracker.end(), 5);
        // Indices at or below `end` are dropped, not stashed in `pending`.
        assert!(tracker.pending.is_empty());
        tracker.add(6);
        assert_eq!(tracker.end(), 6);
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn contains_reports_completed_indices() {
        let mut tracker = SequenceTracker::new(2);
        tracker.add(4);
        assert!(tracker.contains(1)); // below end
        assert!(tracker.contains(2)); // == end
        assert!(!tracker.contains(3)); // a gap, not completed
        assert!(tracker.contains(4)); // out-of-order completion
        assert!(!tracker.contains(5));
    }

    #[test]
    fn restore_round_trips_end_and_pending() {
        let mut tracker = SequenceTracker::new(0);
        tracker.add(1);
        tracker.add(2);
        tracker.add(4);
        tracker.add(6);
        assert_eq!(tracker.end(), 2);
        assert_eq!(tracker.pending_indices(), vec![4, 6]);

        let restored = SequenceTracker::restore(tracker.end(), tracker.pending_indices());
        assert_eq!(restored.end(), 2);
        assert_eq!(restored.pending_indices(), vec![4, 6]);
        assert!(restored.contains(4));
    }

    #[test]
    fn restore_advances_end_through_contiguous_pending() {
        // A restored pending set that happens to abut `end` heals the watermark.
        let restored = SequenceTracker::restore(2, [3, 4, 6]);
        assert_eq!(restored.end(), 4);
        assert_eq!(restored.pending_indices(), vec![6]);
    }
}
