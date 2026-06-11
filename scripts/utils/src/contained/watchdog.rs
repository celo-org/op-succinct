//! In-flight work registry for the overrun watchdog.
//!
//! Replaces the single `Arc<Mutex<Option<Instant>>>` start-slot, which could only
//! describe ONE running unit and therefore silently assumed serial execution. Now
//! that builds and executes run concurrently (multiple games, multiple sub-ranges),
//! the watchdog must see EVERY in-flight heavy unit at once. This is the in-process
//! analogue of the legacy monitor's `running_processes` map (game_monitor.rs:357),
//! holding just what the watchdog needs: a start time, the kind, and a label.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use op_succinct_estimator::memory::WorkKind;

/// One in-flight heavy unit (a pipeline build or an executor execute).
#[derive(Clone, Debug)]
pub struct InflightUnit {
    pub started: Instant,
    pub kind: WorkKind,
    pub label: String,
}

/// Registry of currently-running heavy units. Cheaply cloneable handle is the inner
/// `Arc<Mutex<..>>`; the `Watchdog` owns the id counter.
#[derive(Default)]
pub struct Watchdog {
    inflight: Arc<Mutex<HashMap<u64, InflightUnit>>>,
    next_id: AtomicU64,
}

/// RAII handle: removes its registry entry on drop, so a unit is deregistered on
/// success, on an early `?`, and on unwind — no manual clearing, no stale slot.
pub struct UnitGuard {
    inflight: Arc<Mutex<HashMap<u64, InflightUnit>>>,
    id: u64,
}

impl Drop for UnitGuard {
    fn drop(&mut self) {
        // Tolerate a poisoned lock: the registry is best-effort telemetry, never a
        // correctness gate, so a poisoned mutex must not panic on the drop path.
        if let Ok(mut map) = self.inflight.lock() {
            map.remove(&self.id);
        }
    }
}

impl Watchdog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a unit as in-flight; the returned guard deregisters it on drop.
    /// Call this AFTER acquiring the admission permit so the registry reflects work
    /// that is actually running (holding a permit), not work queued behind admission.
    pub fn enter(&self, kind: WorkKind, label: impl Into<String>) -> UnitGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let unit = InflightUnit { started: Instant::now(), kind, label: label.into() };
        self.inflight.lock().unwrap().insert(id, unit);
        UnitGuard { inflight: self.inflight.clone(), id }
    }

    /// Every in-flight unit whose runtime exceeds `ceiling_secs`, as
    /// `(kind, label, elapsed_secs)`. Empty when nothing is overrunning — which is
    /// what lets the watchdog THAW admission once a runaway finishes.
    pub fn overrunning(&self, now: Instant, ceiling_secs: u64) -> Vec<(WorkKind, String, u64)> {
        self.inflight
            .lock()
            .unwrap()
            .values()
            .filter_map(|u| {
                let elapsed = now.duration_since(u.started).as_secs();
                (elapsed > ceiling_secs).then(|| (u.kind, u.label.clone(), elapsed))
            })
            .collect()
    }

    /// Number of in-flight units (test/observability helper).
    pub fn inflight_len(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn guard_registers_and_deregisters() {
        let wd = Watchdog::new();
        assert_eq!(wd.inflight_len(), 0);
        let g1 = wd.enter(WorkKind::Execute, "execute 1-2");
        let g2 = wd.enter(WorkKind::Build, "build 2-3");
        assert_eq!(wd.inflight_len(), 2);
        drop(g1);
        assert_eq!(wd.inflight_len(), 1);
        drop(g2);
        assert_eq!(wd.inflight_len(), 0);
    }

    #[test]
    fn overrunning_reports_only_units_past_ceiling_across_all_inflight() {
        let wd = Watchdog::new();
        let _slow = wd.enter(WorkKind::Execute, "execute 10-20");
        let _fast = wd.enter(WorkKind::Build, "build 20-30");
        // Nothing has overrun a 60s ceiling yet.
        assert!(wd.overrunning(Instant::now(), 60).is_empty());
        // Simulate the slow unit having started 100s ago by checking against a future now.
        let later = Instant::now() + Duration::from_secs(100);
        let over = wd.overrunning(later, 60);
        // BOTH are past a 60s ceiling at +100s; the watchdog sees every in-flight unit,
        // not just one slot.
        assert_eq!(over.len(), 2);
    }

    #[test]
    fn empty_registry_never_overruns_so_admission_can_thaw() {
        let wd = Watchdog::new();
        assert!(wd.overrunning(Instant::now() + Duration::from_secs(10_000), 1).is_empty());
    }
}
