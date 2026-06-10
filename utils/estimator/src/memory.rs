use serde::{Deserialize, Serialize};

/// Parse a cgroup-v2 `memory.max` value. `"max"` means unlimited → `None`.
pub fn parse_cgroup_v2_max(contents: &str) -> Option<u64> {
    let t = contents.trim();
    if t == "max" {
        None
    } else {
        t.parse::<u64>().ok()
    }
}

/// Parse a cgroup-v1 `memory.limit_in_bytes`. A sentinel near i64::MAX means unlimited.
pub fn parse_cgroup_v1_limit(contents: &str) -> Option<u64> {
    let v = contents.trim().parse::<u64>().ok()?;
    // cgroup-v1 reports a huge page-aligned sentinel (~i64::MAX) when unlimited.
    const V1_UNLIMITED_FLOOR: u64 = 0x7FFF_FFFF_FFFF_F000; // 9223372036854771712
    if v >= V1_UNLIMITED_FLOOR {
        None
    } else {
        Some(v)
    }
}

/// Read the effective memory budget in bytes from the cgroup, preferring v2.
/// Returns `None` if no limit is set (unlimited) or the files are absent.
pub fn read_cgroup_budget_bytes() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        return parse_cgroup_v2_max(&s);
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        return parse_cgroup_v1_limit(&s);
    }
    None
}

/// Read live usage in bytes from the cgroup (v2 `memory.current`, v1 `memory.usage_in_bytes`).
pub fn read_cgroup_usage_bytes() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.current") {
        return s.trim().parse::<u64>().ok();
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.usage_in_bytes") {
        return s.trim().parse::<u64>().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_unlimited_is_none() {
        assert_eq!(parse_cgroup_v2_max("max\n"), None);
    }

    #[test]
    fn v2_number_parses() {
        assert_eq!(parse_cgroup_v2_max("536870912000\n"), Some(536_870_912_000));
    }

    #[test]
    fn v1_sentinel_is_unlimited() {
        assert_eq!(parse_cgroup_v1_limit("9223372036854771712\n"), None);
    }

    #[test]
    fn v1_real_limit_parses() {
        assert_eq!(parse_cgroup_v1_limit("536870912000\n"), Some(536_870_912_000));
    }
}

/// One observed completion: peak RSS for a unit of work, keyed by EVM gas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssSample {
    pub gas: u64,
    pub peak_rss_bytes: u64,
    pub kind: WorkKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkKind {
    Build,
    Execute,
}

/// Bounded history of peak-RSS samples used to project the next unit's footprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssHistory {
    pub samples: Vec<RssSample>,
    pub max_len: usize,
}

impl Default for RssHistory {
    // Delegate to `new` so the `max_len >= 1` invariant always holds (a derived
    // `Default` would set `max_len = 0`, silently dropping every recorded sample).
    fn default() -> Self {
        Self::new(64)
    }
}

impl RssHistory {
    pub fn new(max_len: usize) -> Self {
        Self { samples: Vec::new(), max_len: max_len.max(1) }
    }

    pub fn record(&mut self, sample: RssSample) {
        self.samples.push(sample);
        if self.samples.len() > self.max_len {
            let overflow = self.samples.len() - self.max_len;
            self.samples.drain(0..overflow);
        }
    }

    /// Project peak RSS for a unit of `kind` and `gas`. Uses the max peak/gas ratio
    /// observed for that kind (conservative), times gas, with a floor of the largest
    /// recorded peak when there is no usable signal yet.
    pub fn project_peak(&self, kind: WorkKind, gas: u64, default_bytes: u64) -> u64 {
        let mut max_ratio = 0f64;
        let mut max_peak = 0u64;
        for s in self.samples.iter().filter(|s| s.kind == kind && s.gas > 0) {
            max_ratio = max_ratio.max(s.peak_rss_bytes as f64 / s.gas as f64);
            max_peak = max_peak.max(s.peak_rss_bytes);
        }
        if max_ratio > 0.0 {
            ((max_ratio * gas as f64) as u64).max(max_peak)
        } else {
            default_bytes
        }
    }
}

/// Decide whether a unit projected to need `projected_bytes` fits the budget given
/// `current_usage_bytes`, leaving `margin_bytes` headroom.
pub fn admits(
    budget_bytes: Option<u64>,
    current_usage_bytes: u64,
    projected_bytes: u64,
    margin_bytes: u64,
) -> bool {
    match budget_bytes {
        None => true, // no cgroup limit → unlimited
        Some(budget) => current_usage_bytes
            .saturating_add(projected_bytes)
            .saturating_add(margin_bytes)
            <= budget,
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn projects_from_history_ratio() {
        let mut h = RssHistory::new(8);
        h.record(RssSample { gas: 1_000, peak_rss_bytes: 10_000, kind: WorkKind::Execute });
        // ratio 10 bytes/gas → 2000 gas projects 20_000.
        assert_eq!(h.project_peak(WorkKind::Execute, 2_000, 1), 20_000);
    }

    #[test]
    fn falls_back_to_default_without_signal() {
        let h = RssHistory::new(8);
        assert_eq!(h.project_peak(WorkKind::Build, 5_000, 42), 42);
    }

    #[test]
    fn admits_when_it_fits_and_rejects_when_it_does_not() {
        assert!(admits(Some(100), 10, 50, 20)); // 80 <= 100
        assert!(!admits(Some(100), 60, 50, 20)); // 130 > 100
        assert!(admits(None, u64::MAX, u64::MAX, u64::MAX)); // unlimited
    }

    #[test]
    fn history_is_bounded() {
        let mut h = RssHistory::new(2);
        for g in 0..5 {
            h.record(RssSample { gas: g + 1, peak_rss_bytes: 1, kind: WorkKind::Build });
        }
        assert_eq!(h.samples.len(), 2);
    }

    #[test]
    fn default_history_retains_samples() {
        // Default must honor the max_len >= 1 invariant, not silently drop everything.
        let mut h = RssHistory::default();
        assert!(h.max_len >= 1);
        h.record(RssSample { gas: 1, peak_rss_bytes: 1, kind: WorkKind::Build });
        assert_eq!(h.samples.len(), 1);
    }
}
