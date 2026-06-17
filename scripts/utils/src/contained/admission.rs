use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use op_succinct_estimator::memory::{
    admits, read_cgroup_usage_bytes, RssHistory, RssSample, WorkKind,
};

/// Adaptive memory admission (spec §7): before starting a build or execute, project the
/// unit's peak RSS(resident set size) from history (keyed by EVM gas) and admit only if it fits the
/// budget given LIVE cgroup usage + some margin. Records observed footprints to grow the history.
pub struct Admission {
    budget_bytes: Option<u64>,
    margin_bytes: u64,
    default_unit_bytes: u64,
    history: Mutex<RssHistory>,
    history_path: PathBuf,
    poll: Duration,
}

impl Admission {
    pub fn load(
        budget_bytes: Option<u64>,
        margin_bytes: u64,
        default_unit_bytes: u64,
        history_path: PathBuf,
        poll: Duration,
    ) -> Self {
        let history = std::fs::read_to_string(&history_path)
            .ok()
            .and_then(|s| serde_json::from_str::<RssHistory>(&s).ok())
            .unwrap_or_else(|| RssHistory::new(200));
        Self {
            budget_bytes,
            margin_bytes,
            default_unit_bytes,
            history: Mutex::new(history),
            history_path,
            poll,
        }
    }

    /// Block until a unit of `kind`/`gas` is projected to fit the budget. Polls live usage.
    pub async fn admit(&self, kind: WorkKind, gas: u64) {
        loop {
            let projected = {
                let h = self.history.lock().unwrap();
                h.project_peak(kind, gas, self.default_unit_bytes)
            };
            let usage = read_cgroup_usage_bytes().unwrap_or(0);
            if admits(self.budget_bytes, usage, projected, self.margin_bytes) {
                return;
            }
            tracing::debug!(?kind, gas, projected, usage, "admission waiting for memory headroom");
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Record an observed peak (approximate under concurrency — see note) keyed by gas.
    pub fn record(&self, kind: WorkKind, gas: u64, peak_bytes: u64) {
        {
            let mut h = self.history.lock().unwrap();
            h.record(RssSample { gas, peak_rss_bytes: peak_bytes, kind });
        }
        // Best-effort persistence (a crash mid-write just loses the latest sample).
        if let Ok(json) = {
            let h = self.history.lock().unwrap();
            serde_json::to_string(&*h)
        } {
            let tmp = self.history_path.with_extension("json.tmp");
            if std::fs::write(&tmp, &json)
                .and_then(|_| std::fs::rename(&tmp, &self.history_path))
                .is_err()
            {
                tracing::warn!("failed to persist completion history");
            }
        }
    }
}

/// Approximate post-unit resident footprint (cgroup memory.current). NOTE: under
/// concurrent build+execute this over-attributes shared memory to the unit, which makes
/// admission CONSERVATIVE (over-estimate → safer). Documented limitation per §7.
pub fn current_rss_bytes() -> u64 {
    read_cgroup_usage_bytes().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admit_returns_immediately_with_no_budget_and_record_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        // budget None → admits always true regardless of live usage.
        let adm = Admission::load(None, 0, 1, path.clone(), Duration::from_millis(1));

        // Should return immediately (no waiting) since budget is unlimited.
        adm.admit(WorkKind::Build, 0).await;

        // Recording a sample must persist a readable RssHistory JSON file.
        adm.record(WorkKind::Build, 1_000, 4_096);
        let json = std::fs::read_to_string(&path).unwrap();
        let parsed: RssHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.samples.len(), 1);
        assert_eq!(parsed.samples[0].gas, 1_000);
        assert_eq!(parsed.samples[0].peak_rss_bytes, 4_096);
        assert_eq!(parsed.samples[0].kind, WorkKind::Build);
    }
}
