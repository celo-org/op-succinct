use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::Duration,
};

use op_succinct_estimator::memory::WorkKind;
use serde::{Deserialize, Serialize};

use crate::game_monitor_embedded::{
    registry::{AdmitGuard, WorkloadRegistry},
    rss_source::RssSource,
};

/// Effective gas below which a sample is ignored: with nothing meaningful in flight the
/// cost-per-gas ratio is dominated by noise. Effectively "the registry is non-empty".
const MIN_EFFECTIVE_GAS: f64 = 1.0;

/// Learned state persisted across restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedModel {
    /// Running maximum of `(rss - baseline) / effective_gas` observed by the sampler.
    max_cost_per_gas: f64,
    /// Number of samples folded in so far (diagnostic).
    sample_count: u64,
}

/// Construction parameters for [`Admission`].
pub struct AdmissionConfig {
    /// cgroup memory budget; `None` = unlimited (admission never blocks on memory).
    pub budget_bytes: Option<u64>,
    /// Safety headroom kept below the budget.
    pub margin_bytes: u64,
    /// Build-gas weighting in effective gas. `< 1` discounts builds relative to executes.
    pub alpha: f64,
    /// Hard cap on in-flight units (builds + executes). Bounds non-memory resources —
    /// file descriptors, RPC fan-out, CPU — that the memory model does not constrain, and
    /// is the only cap when the budget is unlimited or the model has no signal.
    pub max_concurrent: usize,
    /// Back-off between failed admit attempts.
    pub admit_poll: Duration,
    /// Sampler tick period.
    pub sample_period: Duration,
    /// Persist the model every N sampler ticks.
    pub persist_every: u32,
    /// Where the learned model is persisted.
    pub persist_path: PathBuf,
}

/// Memory admission via a single learned coefficient.
///
/// A background sampler (see [`Admission::spawn_sampler`]) polls resident memory and folds
/// `(rss - baseline) / effective_gas` into a running maximum `max_cost_per_gas`, where
/// `effective_gas = sum_execute_gas + alpha * sum_build_gas` over the in-flight workload.
/// Admission projects the footprint of one more unit as `baseline + max_cost_per_gas *
/// effective_gas_after` and admits only if that plus a margin fits the budget.
///
/// Cold start: `max_cost_per_gas` is 0 and there is no model, so admission runs strictly
/// serially (admit only when nothing is in flight) until the first game completes
/// ([`Admission::mark_warmed`]). Serial warmup is safe and yields clean single-unit
/// samples to bootstrap the coefficient.
///
/// Independently of memory, a hard `max_concurrent` count caps in-flight units to bound
/// file descriptors, RPC fan-out, and CPU — the resources the memory model ignores.
pub struct Admission {
    budget_bytes: Option<u64>,
    margin_bytes: u64,
    alpha: f64,
    max_concurrent: u64,
    /// Idle resident memory measured at startup; subtracted from every sample.
    baseline_bytes: u64,
    max_cost_per_gas: Mutex<f64>,
    sample_count: AtomicU64,
    /// Once true, admission uses the projection; until then it runs serially.
    warmed: AtomicBool,
    registry: Arc<WorkloadRegistry>,
    rss_source: Box<dyn RssSource>,
    admit_poll: Duration,
    sample_period: Duration,
    persist_every: u32,
    persist_path: PathBuf,
    /// Serialises the admit decision so check + register is atomic across concurrent admits.
    admit_lock: tokio::sync::Mutex<()>,
}

impl Admission {
    /// Load persisted state (if any), measure the idle baseline from `rss_source`, and
    /// build the admission gate. If a credible model was persisted (`max_cost_per_gas >
    /// 0`) the gate starts warmed; otherwise it begins in serial cold-start mode.
    pub fn load(config: AdmissionConfig, rss_source: Box<dyn RssSource>) -> Arc<Self> {
        let persisted: PersistedModel = std::fs::read_to_string(&config.persist_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        // Baseline = resident memory now, before any work is admitted.
        let baseline_bytes = rss_source.read().unwrap_or(0);
        let warmed = persisted.max_cost_per_gas > 0.0;

        tracing::info!(
            baseline_bytes,
            max_cost_per_gas = persisted.max_cost_per_gas,
            warmed,
            "memory admission loaded"
        );

        Arc::new(Self {
            budget_bytes: config.budget_bytes,
            margin_bytes: config.margin_bytes,
            alpha: config.alpha,
            max_concurrent: (config.max_concurrent.max(1)) as u64,
            baseline_bytes,
            max_cost_per_gas: Mutex::new(persisted.max_cost_per_gas),
            sample_count: AtomicU64::new(persisted.sample_count),
            warmed: AtomicBool::new(warmed),
            registry: Arc::new(WorkloadRegistry::default()),
            rss_source,
            admit_poll: config.admit_poll,
            sample_period: config.sample_period,
            persist_every: config.persist_every.max(1),
            persist_path: config.persist_path,
            admit_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Block until a unit of `kind`/`gas` is admitted, then return a guard that holds the
    /// reservation until dropped. Two predicates, both required: the in-flight count must
    /// be below `max_concurrent` (hard cap), and memory must fit — by projection once
    /// warmed, or by running serially (registry empty) during cold start.
    pub async fn admit(&self, kind: WorkKind, gas: u64) -> AdmitGuard {
        loop {
            {
                let _decision = self.admit_lock.lock().await;
                let (sb, se) = self.registry.snapshot();
                let within_count = self.registry.in_flight() < self.max_concurrent;
                let memory_ok = if self.warmed.load(Relaxed) {
                    let cpg = *self.max_cost_per_gas.lock().unwrap();
                    self.fits(self.project(cpg, sb, se, kind, gas))
                } else {
                    // Cold start: no model. Serial only.
                    sb == 0 && se == 0
                };
                if within_count && memory_ok {
                    return AdmitGuard::new(self.registry.clone(), kind, gas);
                }
            }
            tracing::debug!(?kind, gas, "admission waiting for headroom");
            tokio::time::sleep(self.admit_poll).await;
        }
    }

    /// Project total resident memory if a `kind`/`gas` unit is admitted on top of the
    /// currently in-flight `(sum_build_gas, sum_execute_gas)`.
    fn project(&self, cpg: f64, sum_build_gas: u64, sum_execute_gas: u64, kind: WorkKind, gas: u64) -> u64 {
        let delta = match kind {
            WorkKind::Build => self.alpha * gas as f64,
            WorkKind::Execute => gas as f64,
        };
        let eff = sum_execute_gas as f64 + self.alpha * sum_build_gas as f64 + delta;
        (self.baseline_bytes as f64 + cpg * eff) as u64
    }

    /// Whether `projected` plus the margin fits the budget. Unlimited budget always fits.
    fn fits(&self, projected: u64) -> bool {
        match self.budget_bytes {
            None => true,
            Some(budget) => projected.saturating_add(self.margin_bytes) <= budget,
        }
    }

    /// Fold one resident-memory reading into the model. Called by the sampler. Skips
    /// readings taken while the registry is effectively empty (baseline-dominated).
    pub fn observe(&self, rss: u64) {
        let (sb, se) = self.registry.snapshot();
        let eff = se as f64 + self.alpha * sb as f64;
        if eff < MIN_EFFECTIVE_GAS {
            return;
        }
        let net = rss.saturating_sub(self.baseline_bytes) as f64;
        let cpg = net / eff;
        {
            let mut max = self.max_cost_per_gas.lock().unwrap();
            *max = max.max(cpg);
        }
        self.sample_count.fetch_add(1, Relaxed);
    }

    /// Leave cold-start serial mode and begin using the projection. Idempotent.
    pub fn mark_warmed(&self) {
        self.warmed.store(true, Relaxed);
    }

    /// Spawn the background RSS sampler. Ticks every `sample_period`, folding each reading
    /// into `max_cost_per_gas`, and persists every `persist_every` ticks. Detached: runs
    /// for the process lifetime (periodic persistence means a kill loses at most one
    /// interval of learning).
    pub fn spawn_sampler(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.sample_period);
            let mut since_persist: u32 = 0;
            loop {
                ticker.tick().await;
                if let Some(rss) = self.rss_source.read() {
                    self.observe(rss);
                }
                since_persist += 1;
                if since_persist >= self.persist_every {
                    self.persist();
                    since_persist = 0;
                }
            }
        });
    }

    /// Persist the learned model. Best-effort, atomic-rename; never holds the lock across
    /// the file write. Creates the parent directory if it does not yet exist — the cache
    /// dir is otherwise created lazily on the first witness save, so without this the very
    /// first persist (before any game completes) would fail.
    pub fn persist(&self) {
        let model = PersistedModel {
            max_cost_per_gas: *self.max_cost_per_gas.lock().unwrap(),
            sample_count: self.sample_count.load(Relaxed),
        };
        let json = match serde_json::to_string(&model) {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(%error, "failed to serialize memory model");
                return;
            }
        };
        let tmp = self.persist_path.with_extension("json.tmp");
        let result = self
            .persist_path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&tmp, &json))
            .and_then(|_| std::fs::rename(&tmp, &self.persist_path));
        if let Err(error) = result {
            tracing::warn!(
                %error,
                path = %self.persist_path.display(),
                "failed to persist memory model"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_monitor_embedded::rss_source::Unsupported;

    fn test_admission(
        budget: Option<u64>,
        max_concurrent: usize,
        persist_path: PathBuf,
    ) -> Arc<Admission> {
        Admission::load(
            AdmissionConfig {
                budget_bytes: budget,
                margin_bytes: 0,
                alpha: 0.1,
                max_concurrent,
                admit_poll: Duration::from_millis(5),
                sample_period: Duration::from_millis(5),
                persist_every: 1,
                persist_path,
            },
            Box::new(Unsupported),
        )
    }

    #[tokio::test]
    async fn cold_start_is_serial_then_warmed_uses_projection() {
        let dir = tempfile::tempdir().unwrap();
        let adm = test_admission(None, 64, dir.path().join("model.json"));

        // Cold start: first admit succeeds (registry empty).
        let g1 = adm.admit(WorkKind::Build, 1_000).await;
        assert_eq!(adm.registry.snapshot(), (1_000, 0));

        // A second concurrent admit must block while g1 is held (serial cold start).
        let blocked = adm.admit(WorkKind::Execute, 5_000);
        tokio::pin!(blocked);
        tokio::select! {
            _ = &mut blocked => panic!("second admit should block during serial cold start"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }

        // Release the first; now the blocked admit proceeds.
        drop(g1);
        let g2 = tokio::time::timeout(Duration::from_millis(200), &mut blocked)
            .await
            .expect("admit should proceed once the registry drains");
        assert_eq!(adm.registry.snapshot(), (0, 5_000));
        drop(g2);
    }

    #[test]
    fn observe_folds_running_max_skipping_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let adm = test_admission(None, 64, dir.path().join("model.json"));

        // Empty registry → skipped, no change.
        adm.observe(10_000);
        assert_eq!(*adm.max_cost_per_gas.lock().unwrap(), 0.0);

        // With work in flight, the ratio is recorded; baseline is 0 (Unsupported source).
        let _g = AdmitGuard::new(adm.registry.clone(), WorkKind::Execute, 1_000);
        adm.observe(10_000); // 10_000 / 1_000 = 10
        assert_eq!(*adm.max_cost_per_gas.lock().unwrap(), 10.0);

        // A lower ratio does not lower the max.
        adm.observe(5_000); // ratio 5 < 10
        assert_eq!(*adm.max_cost_per_gas.lock().unwrap(), 10.0);

        // A higher ratio raises it.
        adm.observe(20_000); // ratio 20 > 10
        assert_eq!(*adm.max_cost_per_gas.lock().unwrap(), 20.0);
    }

    #[test]
    fn projection_weights_build_gas_by_alpha() {
        let dir = tempfile::tempdir().unwrap();
        let adm = test_admission(Some(1_000_000), 64, dir.path().join("model.json"));
        // cpg = 1 byte per effective gas; baseline 0; alpha 0.1.
        // Execute of 1000 gas → eff 1000 → projected 1000.
        assert_eq!(adm.project(1.0, 0, 0, WorkKind::Execute, 1_000), 1_000);
        // Build of 1000 gas → eff 100 → projected 100 (discounted by alpha).
        assert_eq!(adm.project(1.0, 0, 0, WorkKind::Build, 1_000), 100);
    }

    #[test]
    fn persist_round_trips_max_cost_per_gas_and_warms_on_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.json");

        let adm = test_admission(None, 64, path.clone());
        let _g = AdmitGuard::new(adm.registry.clone(), WorkKind::Execute, 1_000);
        adm.observe(7_000); // cpg = 7
        adm.persist();

        // Reload: model restored and gate starts warmed.
        let reloaded = test_admission(None, 64, path);
        assert_eq!(*reloaded.max_cost_per_gas.lock().unwrap(), 7.0);
        assert!(reloaded.warmed.load(Relaxed));
    }

    #[test]
    fn persist_creates_missing_parent_dir() {
        // Regression: the cache dir is created lazily on the first witness save, so the
        // first persist (before any game completes) must create its own parent or it
        // silently fails forever.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent").join("nested").join("model.json");
        assert!(!path.parent().unwrap().exists());

        let adm = test_admission(None, 64, path.clone());
        let _g = AdmitGuard::new(adm.registry.clone(), WorkKind::Execute, 1_000);
        adm.observe(7_000); // cpg = 7
        adm.persist();

        assert!(path.exists(), "persist must create the file under a missing parent dir");
        let reloaded = test_admission(None, 64, path);
        assert_eq!(*reloaded.max_cost_per_gas.lock().unwrap(), 7.0);
        assert!(reloaded.warmed.load(Relaxed));
    }

    #[tokio::test]
    async fn count_cap_bounds_concurrency_even_when_memory_fits() {
        let dir = tempfile::tempdir().unwrap();
        // Unlimited budget → memory always fits; warmed → no serial gate. Only the count
        // cap (2) can bound concurrency here.
        let adm = test_admission(None, 2, dir.path().join("model.json"));
        adm.mark_warmed();

        let g1 = adm.admit(WorkKind::Build, 1).await;
        let g2 = adm.admit(WorkKind::Execute, 1).await;
        assert_eq!(adm.registry.in_flight(), 2);

        // Third admit must block at the cap.
        let blocked = adm.admit(WorkKind::Build, 1);
        tokio::pin!(blocked);
        tokio::select! {
            _ = &mut blocked => panic!("third admit should block at the concurrency cap"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }

        // Free a slot; the blocked admit proceeds.
        drop(g1);
        let g3 = tokio::time::timeout(Duration::from_millis(200), &mut blocked)
            .await
            .expect("admit should proceed once a slot frees");
        assert_eq!(adm.registry.in_flight(), 2);
        drop(g2);
        drop(g3);
    }
}
