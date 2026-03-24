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
        self.pending.insert(index);
        let mut check_from = self.end + 1;
        while self.pending.remove(&check_from) {
            self.end = check_from;
            check_from += 1;
        }
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
        // Old indices just sit in pending harmlessly (or were never contiguous).
        tracker.add(6);
        assert_eq!(tracker.end(), 6);
    }
}
