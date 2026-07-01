use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use op_succinct_estimator::memory::WorkKind;
use serde::{Deserialize, Serialize};

use crate::game_monitor_embedded::{
    registry::{AdmitGuard, WorkloadRegistry},
    rss_source::RssSource,
};

/// Minimum in-flight gas (of the kind being attributed) for a sample to be folded in: below
/// this the cost-per-gas ratio is dominated by baseline noise. Effectively "that kind is in
/// flight".
const MIN_SAMPLE_GAS: f64 = 1.0;

const BYTES_PER_GIB: f64 = (1u64 << 30) as f64;

/// Bytes rendered as GiB, for human-readable logs.
fn gib(bytes: u64) -> f64 {
    bytes as f64 / BYTES_PER_GIB
}

/// Learned peak memory cost in bytes of RSS per unit of EVM gas, tracked SEPARATELY per work
/// kind. A build's footprint per gas differs from an execute's by ~10x, so a single shared
/// coefficient mis-projects whichever kind it was not learned from (it over-projects executes
/// off build samples, starving them). Each kind keeps its own running max, learned only from
/// samples taken while just that kind was in flight. Persisted verbatim across restarts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct CostModel {
    /// Running max bytes/gas observed while only builds were in flight.
    cost_per_gas_build: f64,
    /// Running max bytes/gas observed while only executes were in flight.
    cost_per_gas_execute: f64,
    /// Pure-sample counts folded per kind (diagnostic).
    samples_build: u64,
    samples_execute: u64,
}

impl CostModel {
    /// The learned cost for `kind` (bytes of RSS per gas).
    fn cost(&self, kind: WorkKind) -> f64 {
        match kind {
            WorkKind::Build => self.cost_per_gas_build,
            WorkKind::Execute => self.cost_per_gas_execute,
        }
    }

    /// Whether both kinds present in `(build_gas, execute_gas)` have a learned (non-zero)
    /// cost. A zero (unknown) cost would under-project, so admission stays serial for a kind
    /// until at least one pure sample of it has been observed.
    fn known_for(&self, build_gas: u64, execute_gas: u64) -> bool {
        (build_gas == 0 || self.cost_per_gas_build > 0.0)
            && (execute_gas == 0 || self.cost_per_gas_execute > 0.0)
    }
}

/// Construction parameters for [`Admission`].
pub struct AdmissionConfig {
    /// cgroup memory budget; `None` = unlimited (admission never blocks on memory).
    pub budget_bytes: Option<u64>,
    /// Safety headroom kept below the budget.
    pub margin_bytes: u64,
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

/// Memory admission via per-kind learned coefficients.
///
/// A background sampler (see [`Admission::spawn_sampler`]) polls resident memory. When only
/// one kind is in flight it folds `(rss - baseline) / gas_of_that_kind` into that kind's
/// running max (mixed-kind readings can't be attributed and are skipped). Admission projects
/// the footprint of the in-flight set plus one more unit as `baseline + cost_build *
/// build_gas + cost_execute * execute_gas` and admits only if that plus a margin fits.
///
/// Cold start / liveness floor: with nothing in flight a unit is always admitted, so a
/// pessimistic model can never wedge the daemon; that single unit's memory is bounded by
/// reality and yields a clean pure sample. Until a kind's cost is learned, admission stays
/// serial for it (an unknown cost is not trusted to project concurrency).
///
/// Independently of memory, a hard `max_concurrent` count caps in-flight units to bound
/// file descriptors, RPC fan-out, and CPU — the resources the memory model ignores.
pub struct Admission {
    budget_bytes: Option<u64>,
    margin_bytes: u64,
    max_concurrent: u64,
    /// Idle resident memory measured at startup; subtracted from every sample.
    baseline_bytes: u64,
    /// Per-kind learned costs; shared with the sampler.
    cost: Mutex<CostModel>,
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
    /// Load the persisted per-kind model (if any), measure the idle baseline from
    /// `rss_source`, and build the admission gate.
    pub fn load(config: AdmissionConfig, rss_source: Box<dyn RssSource>) -> Arc<Self> {
        let model: CostModel = std::fs::read_to_string(&config.persist_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        // Baseline = resident memory now, before any work is admitted.
        let baseline_bytes = rss_source.read().unwrap_or(0);

        tracing::info!(
            baseline_gib = %format!("{:.1}", gib(baseline_bytes)),
            cost_per_gas_build = %format!("{:.0}", model.cost_per_gas_build),
            cost_per_gas_execute = %format!("{:.0}", model.cost_per_gas_execute),
            samples_build = model.samples_build,
            samples_execute = model.samples_execute,
            "memory admission loaded"
        );

        Arc::new(Self {
            budget_bytes: config.budget_bytes,
            margin_bytes: config.margin_bytes,
            max_concurrent: (config.max_concurrent.max(1)) as u64,
            baseline_bytes,
            cost: Mutex::new(model),
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
    /// reservation until dropped. Two predicates, both required: the in-flight count must be
    /// below `max_concurrent` (hard cap), and memory must be OK — the projection fits, or the
    /// registry is empty (liveness floor), or the budget is unlimited.
    pub async fn admit(&self, kind: WorkKind, gas: u64) -> AdmitGuard {
        loop {
            {
                let _decision = self.admit_lock.lock().await;
                let (sb, se) = self.registry.snapshot();
                let registry_empty = sb == 0 && se == 0;
                let within_count = self.registry.in_flight() < self.max_concurrent;

                let model = *self.cost.lock().unwrap();
                // Gas of each kind in flight AFTER admitting this unit.
                let (build_gas, execute_gas) = match kind {
                    WorkKind::Build => (sb + gas, se),
                    WorkKind::Execute => (sb, se + gas),
                };
                let projected = self.project(&model, build_gas, execute_gas);
                let fits = self.fits(projected);
                let costs_known = model.known_for(build_gas, execute_gas);

                let memory_ok = match self.budget_bytes {
                    None => true, // unlimited budget: never gate on memory
                    // Floor: always admit one when nothing is in flight. Otherwise the
                    // projection must fit AND every kind's cost must be known.
                    Some(_) => registry_empty || (costs_known && fits),
                };
                let admitted = within_count && memory_ok;

                let reason = if !within_count {
                    "concurrency cap"
                } else if registry_empty {
                    "serial floor"
                } else if !costs_known {
                    "cost unlearned"
                } else if fits {
                    "fits"
                } else {
                    "over budget"
                };
                let projected_gib = format!("{:.1}", gib(projected));
                let limit_gib = match self.budget_bytes {
                    Some(b) => format!("{:.1}GiB", gib(b.saturating_sub(self.margin_bytes))),
                    None => "unlimited".to_string(),
                };
                let cost_per_gas = format!("{:.0}", model.cost(kind));

                // Grants log at INFO; waits at DEBUG. A wait repeats every poll for every
                // queued unit, so keeping waits off the default INFO stream avoids drowning it.
                if admitted {
                    tracing::info!(
                        kind = ?kind,
                        gas,
                        cost_per_gas = %cost_per_gas,
                        projected_gib = %projected_gib,
                        limit_gib = %limit_gib,
                        in_flight_build_gas = sb,
                        in_flight_execute_gas = se,
                        "admission admit ({reason})"
                    );
                    return AdmitGuard::new(self.registry.clone(), kind, gas);
                }
                tracing::debug!(
                    kind = ?kind,
                    gas,
                    cost_per_gas = %cost_per_gas,
                    projected_gib = %projected_gib,
                    limit_gib = %limit_gib,
                    in_flight_build_gas = sb,
                    in_flight_execute_gas = se,
                    "admission wait ({reason})"
                );
            }
            tokio::time::sleep(self.admit_poll).await;
        }
    }

    /// Project total resident memory for the in-flight set `(build_gas, execute_gas)` (which
    /// already includes the unit under consideration), charging each kind its own cost.
    fn project(&self, model: &CostModel, build_gas: u64, execute_gas: u64) -> u64 {
        (self.baseline_bytes as f64
            + model.cost_per_gas_build * build_gas as f64
            + model.cost_per_gas_execute * execute_gas as f64) as u64
    }

    /// Whether `projected` plus the margin fits the budget. Unlimited budget always fits.
    fn fits(&self, projected: u64) -> bool {
        match self.budget_bytes {
            None => true,
            Some(budget) => projected.saturating_add(self.margin_bytes) <= budget,
        }
    }

    /// Fold one resident-memory reading into the model. Called by the sampler. Only pure
    /// single-kind readings are attributed: with both kinds in flight a single RSS scalar
    /// can't be split between them, so mixed (and empty) readings are skipped.
    pub fn observe(&self, rss: u64) {
        let (sb, se) = self.registry.snapshot();
        let net = rss.saturating_sub(self.baseline_bytes) as f64;
        let mut model = self.cost.lock().unwrap();
        if se == 0 && sb as f64 >= MIN_SAMPLE_GAS {
            model.cost_per_gas_build = model.cost_per_gas_build.max(net / sb as f64);
            model.samples_build += 1;
        } else if sb == 0 && se as f64 >= MIN_SAMPLE_GAS {
            model.cost_per_gas_execute = model.cost_per_gas_execute.max(net / se as f64);
            model.samples_execute += 1;
        }
    }

    /// Spawn the background RSS sampler. Ticks every `sample_period`, folding each reading
    /// into the per-kind costs, and persists every `persist_every` ticks. Detached: runs for
    /// the process lifetime (periodic persistence means a kill loses at most one interval of
    /// learning).
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

    /// Persist the learned model. Best-effort, atomic-rename; never holds the lock across the
    /// file write. Creates the parent directory if it does not yet exist — the cache dir is
    /// otherwise created lazily on the first witness save, so without this the very first
    /// persist (before any game completes) would fail.
    pub fn persist(&self) {
        let model = *self.cost.lock().unwrap();
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
    async fn cold_start_is_serial_until_cost_learned() {
        let dir = tempfile::tempdir().unwrap();
        // Finite budget + fresh model (both costs 0). Until a kind's cost is learned its
        // projection isn't trusted, so admission stays serial via the floor.
        let adm = test_admission(Some(1_000_000_000), 64, dir.path().join("model.json"));

        // Registry empty → floor admits the first unit.
        let g1 = adm.admit(WorkKind::Build, 1_000).await;
        assert_eq!(adm.registry.snapshot(), (1_000, 0));

        // A second concurrent admit must block: build cost is still unlearned, so the
        // projection can't be trusted and the floor doesn't apply (registry non-empty).
        let blocked = adm.admit(WorkKind::Execute, 5_000);
        tokio::pin!(blocked);
        tokio::select! {
            _ = &mut blocked => panic!("second admit should block until a cost is learned"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }

        // Release the first; now the blocked admit proceeds (registry drained → floor).
        drop(g1);
        let g2 = tokio::time::timeout(Duration::from_millis(200), &mut blocked)
            .await
            .expect("admit should proceed once the registry drains");
        assert_eq!(adm.registry.snapshot(), (0, 5_000));
        drop(g2);
    }

    #[test]
    fn observe_attributes_pure_samples_per_kind() {
        let dir = tempfile::tempdir().unwrap();
        let adm = test_admission(None, 64, dir.path().join("model.json"));

        // Empty registry → skipped, no change.
        adm.observe(10_000);
        assert_eq!(*adm.cost.lock().unwrap(), CostModel::default());

        // Only a build in flight → attributed to build; baseline is 0 (Unsupported source).
        {
            let _b = AdmitGuard::new(adm.registry.clone(), WorkKind::Build, 1_000);
            adm.observe(10_000); // 10_000 / 1_000 = 10
            assert_eq!(adm.cost.lock().unwrap().cost_per_gas_build, 10.0);
            adm.observe(5_000); // lower ratio does not lower the max
            assert_eq!(adm.cost.lock().unwrap().cost_per_gas_build, 10.0);
        }
        // Execute cost is untouched by build samples.
        assert_eq!(adm.cost.lock().unwrap().cost_per_gas_execute, 0.0);

        // Only an execute in flight → attributed to execute.
        {
            let _e = AdmitGuard::new(adm.registry.clone(), WorkKind::Execute, 2_000);
            adm.observe(10_000); // 10_000 / 2_000 = 5
            assert_eq!(adm.cost.lock().unwrap().cost_per_gas_execute, 5.0);
        }

        // Mixed kinds in flight → not attributable, skipped (no change to either).
        let before = *adm.cost.lock().unwrap();
        let _b = AdmitGuard::new(adm.registry.clone(), WorkKind::Build, 1_000);
        let _e = AdmitGuard::new(adm.registry.clone(), WorkKind::Execute, 1_000);
        adm.observe(999_999);
        assert_eq!(adm.cost.lock().unwrap().cost_per_gas_build, before.cost_per_gas_build);
        assert_eq!(adm.cost.lock().unwrap().cost_per_gas_execute, before.cost_per_gas_execute);
    }

    #[test]
    fn project_sums_each_kind_cost() {
        let dir = tempfile::tempdir().unwrap();
        let adm = test_admission(Some(1_000_000), 64, dir.path().join("model.json"));
        // baseline 0 (Unsupported); build 2 B/gas, execute 3 B/gas.
        let model = CostModel {
            cost_per_gas_build: 2.0,
            cost_per_gas_execute: 3.0,
            ..CostModel::default()
        };
        assert_eq!(adm.project(&model, 1_000, 0), 2_000);
        assert_eq!(adm.project(&model, 0, 1_000), 3_000);
        assert_eq!(adm.project(&model, 1_000, 1_000), 5_000);
    }

    #[test]
    fn persist_round_trips_per_kind_costs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.json");

        let adm = test_admission(None, 64, path.clone());
        {
            let _b = AdmitGuard::new(adm.registry.clone(), WorkKind::Build, 1_000);
            adm.observe(7_000); // build cost = 7
        }
        {
            let _e = AdmitGuard::new(adm.registry.clone(), WorkKind::Execute, 1_000);
            adm.observe(3_000); // execute cost = 3
        }
        adm.persist();

        let reloaded = test_admission(None, 64, path);
        let model = *reloaded.cost.lock().unwrap();
        assert_eq!(model.cost_per_gas_build, 7.0);
        assert_eq!(model.cost_per_gas_execute, 3.0);
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
        let _b = AdmitGuard::new(adm.registry.clone(), WorkKind::Build, 1_000);
        adm.observe(7_000); // build cost = 7
        adm.persist();

        assert!(path.exists(), "persist must create the file under a missing parent dir");
        let reloaded = test_admission(None, 64, path);
        assert_eq!(reloaded.cost.lock().unwrap().cost_per_gas_build, 7.0);
    }

    #[tokio::test]
    async fn count_cap_bounds_concurrency_even_when_memory_fits() {
        let dir = tempfile::tempdir().unwrap();
        // Unlimited budget → memory never gates. Only the count cap (2) can bound concurrency.
        let adm = test_admission(None, 2, dir.path().join("model.json"));

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

    #[tokio::test]
    async fn liveness_floor_admits_one_unit_when_model_is_poisoned() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny budget + an absurd learned cost for both kinds: the projection can never fit,
        // even for a single unit. The floor must still admit one unit when the registry is
        // empty so a poisoned model can't wedge the daemon — but it must NOT admit a second.
        let adm = test_admission(Some(1_000_000), 64, dir.path().join("model.json"));
        {
            let mut m = adm.cost.lock().unwrap();
            m.cost_per_gas_build = 1.0e9; // ~1GB/gas: nothing fits a 1MB budget
            m.cost_per_gas_execute = 1.0e9;
        }

        // Registry empty → liveness floor admits the first unit despite the projection.
        let g1 =
            tokio::time::timeout(Duration::from_millis(200), adm.admit(WorkKind::Execute, 1_000))
                .await
                .expect("floor must admit one unit when nothing is in flight");
        assert_eq!(adm.registry.in_flight(), 1);

        // A second admit must block: the floor only covers an empty registry; the projection
        // (which never fits) gates everything beyond the first.
        let blocked = adm.admit(WorkKind::Build, 1_000);
        tokio::pin!(blocked);
        tokio::select! {
            _ = &mut blocked => panic!("second admit must block while one unit is in flight"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }

        // Free the first; the floor applies again and the blocked admit proceeds.
        drop(g1);
        let g2 = tokio::time::timeout(Duration::from_millis(200), &mut blocked)
            .await
            .expect("admit should proceed once the registry drains");
        assert_eq!(adm.registry.in_flight(), 1);
        drop(g2);
    }
}
