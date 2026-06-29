use clap::ValueEnum;
use op_succinct_estimator::memory::read_cgroup_usage_bytes;

/// A source of a resident-memory reading in bytes. The sampler polls this on its tick.
pub trait RssSource: Send + Sync {
    /// Current resident memory in bytes, or `None` if it can't be read on this platform.
    fn read(&self) -> Option<u64>;
}

/// Per-process RSS from Linux `/proc/self/status` (`VmRSS`). Preferred: it ignores other
/// processes sharing the cgroup, so a noisy sidecar doesn't pollute the model.
pub struct ProcSelfStatus;

impl RssSource for ProcSelfStatus {
    fn read(&self) -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        parse_vmrss_bytes(&status)
    }
}

/// Whole-cgroup live usage (`memory.current` / `memory.usage_in_bytes`). Correct when the
/// monitor is the only process in its cgroup (typical in production containers).
pub struct CgroupCurrent;

impl RssSource for CgroupCurrent {
    fn read(&self) -> Option<u64> {
        read_cgroup_usage_bytes()
    }
}

/// Fallback for platforms without `/proc` or cgroups (e.g. macOS dev). Always `None`, so
/// the sampler simply records nothing and the model never warms from samples.
pub struct Unsupported;

impl RssSource for Unsupported {
    fn read(&self) -> Option<u64> {
        None
    }
}

/// Selects which `RssSource` to sample. `Auto` picks per-process on Linux, unsupported
/// elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RssSourceKind {
    Auto,
    Proc,
    Cgroup,
}

/// Build the configured RSS source.
pub fn make_source(kind: RssSourceKind) -> Box<dyn RssSource> {
    match kind {
        RssSourceKind::Proc => Box::new(ProcSelfStatus),
        RssSourceKind::Cgroup => Box::new(CgroupCurrent),
        RssSourceKind::Auto => {
            #[cfg(target_os = "linux")]
            {
                Box::new(ProcSelfStatus)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Box::new(Unsupported)
            }
        }
    }
}

/// Parse the `VmRSS:` line of `/proc/self/status` (value is in kB) into bytes.
pub fn parse_vmrss_bytes(status: &str) -> Option<u64> {
    let rest = status.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vmrss_kb_to_bytes() {
        let status = "Name:\tfoo\nVmPeak:\t  900 kB\nVmRSS:\t  2048 kB\nThreads:\t8\n";
        assert_eq!(parse_vmrss_bytes(status), Some(2048 * 1024));
    }

    #[test]
    fn missing_vmrss_is_none() {
        assert_eq!(parse_vmrss_bytes("Name:\tfoo\nThreads:\t8\n"), None);
    }
}
