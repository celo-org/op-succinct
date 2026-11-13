use std::collections::HashSet;

/// Tracks the end of a contiguous sequence of indices.
///
/// As indices are added (potentially out of order), the tracker eagerly advances
/// its `end` value through any contiguous run starting from `end + 1`.
pub struct SequenceTracker {
    end: Option<u64>,
    pending: HashSet<u64>,
}

impl SequenceTracker {
    /// Creates a tracker from the last completed index, or `None` to start at index zero.
    pub fn new(end: Option<u64>) -> Self {
        Self { end, pending: HashSet::new() }
    }

    /// Returns the current end of the contiguous sequence.
    pub fn end(&self) -> Option<u64> {
        self.end
    }

    /// Adds an index and eagerly advances `end` through any contiguous run.
    pub fn add(&mut self, index: u64) {
        if self.end.is_some_and(|end| index <= end) {
            return;
        }
        self.pending.insert(index);
        while let Some(next) = self.end.map_or(Some(0), |end| end.checked_add(1)) {
            if !self.pending.remove(&next) {
                break;
            }
            self.end = Some(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_additions() {
        let tracker = SequenceTracker::new(Some(1));
        assert_eq!(tracker.end(), Some(1));
    }

    #[test]
    fn test_single_next() {
        let mut tracker = SequenceTracker::new(Some(0));
        tracker.add(1);
        assert_eq!(tracker.end(), Some(1));
    }

    #[test]
    fn test_single_not_next() {
        let mut tracker = SequenceTracker::new(Some(1));
        tracker.add(3);
        assert_eq!(tracker.end(), Some(1));
    }

    #[test]
    fn test_consecutive() {
        let mut tracker = SequenceTracker::new(Some(0));
        tracker.add(1);
        tracker.add(2);
        tracker.add(3);
        tracker.add(4);
        assert_eq!(tracker.end(), Some(4));
    }

    #[test]
    fn test_out_of_order() {
        let mut tracker = SequenceTracker::new(Some(0));
        tracker.add(3);
        tracker.add(1);
        tracker.add(2);
        tracker.add(5);
        assert_eq!(tracker.end(), Some(3));

        // Adding the missing index advances through the gap.
        tracker.add(4);
        assert_eq!(tracker.end(), Some(5));
    }

    #[test]
    fn test_duplicates() {
        let mut tracker = SequenceTracker::new(Some(0));
        tracker.add(1);
        tracker.add(1);
        tracker.add(2);
        tracker.add(2);
        assert_eq!(tracker.end(), Some(2));
    }

    #[test]
    fn test_pending_cleared() {
        let mut tracker = SequenceTracker::new(Some(0));
        tracker.add(1);
        tracker.add(2);
        tracker.add(4);
        tracker.add(5);
        assert_eq!(tracker.end(), Some(2));
        // 4 and 5 are still pending
        assert!(tracker.pending.contains(&4));
        assert!(tracker.pending.contains(&5));

        // Fill the gap
        tracker.add(3);
        assert_eq!(tracker.end(), Some(5));
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn test_add_at_or_below_end() {
        let mut tracker = SequenceTracker::new(Some(5));
        tracker.add(3);
        tracker.add(5);
        assert_eq!(tracker.end(), Some(5));
        // Indices at or below `end` are dropped, not stashed in `pending`.
        assert!(tracker.pending.is_empty());
        tracker.add(6);
        assert_eq!(tracker.end(), Some(6));
        assert!(tracker.pending.is_empty());
    }
}
