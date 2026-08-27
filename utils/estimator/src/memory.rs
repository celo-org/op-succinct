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

/// The kind of work a unit performs. Memory accounting weights witness and prove gas
/// independently (see the admission cost-per-gas model in the embedded monitor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkKind {
    Witness,
    Prove,
}
