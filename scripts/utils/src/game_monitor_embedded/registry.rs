use std::sync::{
    atomic::{AtomicU64, Ordering::Relaxed},
    Arc,
};

use op_succinct_estimator::memory::WorkKind;

/// Live tally of in-flight work, keyed by kind, as summed EVM gas plus a unit count. The
/// sampler reads the gas sums to label each RSS sample with the workload that produced it;
/// the admission path reads them to project the footprint of admitting one more unit, and
/// reads the count to enforce the hard concurrency cap. Three atomics — no per-unit
/// bookkeeping, no map.
#[derive(Debug, Default)]
pub struct WorkloadRegistry {
    sum_witness_gas: AtomicU64,
    sum_prove_gas: AtomicU64,
    witness_units: AtomicU64,
    prove_units: AtomicU64,
}

impl WorkloadRegistry {
    /// `(sum_witness_gas, sum_prove_gas)` currently in flight.
    pub fn snapshot(&self) -> (u64, u64) {
        (self.sum_witness_gas.load(Relaxed), self.sum_prove_gas.load(Relaxed))
    }

    /// `(witness_units, prove_units)` currently in flight.
    pub fn units(&self) -> (u64, u64) {
        (self.witness_units.load(Relaxed), self.prove_units.load(Relaxed))
    }

    /// Number of units (witness + prove) currently in flight.
    pub fn in_flight(&self) -> u64 {
        self.witness_units.load(Relaxed) + self.prove_units.load(Relaxed)
    }

    fn add(&self, kind: WorkKind, gas: u64) {
        match kind {
            WorkKind::Witness => {
                self.sum_witness_gas.fetch_add(gas, Relaxed);
                self.witness_units.fetch_add(1, Relaxed);
            }
            WorkKind::Prove => {
                self.sum_prove_gas.fetch_add(gas, Relaxed);
                self.prove_units.fetch_add(1, Relaxed);
            }
        };
    }

    fn sub(&self, kind: WorkKind, gas: u64) {
        match kind {
            WorkKind::Witness => {
                self.sum_witness_gas.fetch_sub(gas, Relaxed);
                self.witness_units.fetch_sub(1, Relaxed);
            }
            WorkKind::Prove => {
                self.sum_prove_gas.fetch_sub(gas, Relaxed);
                self.prove_units.fetch_sub(1, Relaxed);
            }
        };
    }
}

/// RAII handle returned by `Admission::admit`. Registers the unit's gas on creation and
/// deregisters it on drop — so success, error, panic, and cancellation all release the
/// reservation without explicit bookkeeping at the call site.
pub struct AdmitGuard {
    registry: Arc<WorkloadRegistry>,
    kind: WorkKind,
    gas: u64,
}

impl AdmitGuard {
    /// Register the unit and return the guard. Called inside the admission decision lock so
    /// registration is atomic with the fit check.
    pub(crate) fn new(registry: Arc<WorkloadRegistry>, kind: WorkKind, gas: u64) -> Self {
        registry.add(kind, gas);
        Self { registry, kind, gas }
    }
}

impl Drop for AdmitGuard {
    fn drop(&mut self) {
        self.registry.sub(self.kind, self.gas);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_registers_and_deregisters() {
        let reg = Arc::new(WorkloadRegistry::default());
        assert_eq!(reg.snapshot(), (0, 0));
        assert_eq!(reg.in_flight(), 0);
        {
            let _b = AdmitGuard::new(reg.clone(), WorkKind::Witness, 100);
            let _e = AdmitGuard::new(reg.clone(), WorkKind::Prove, 250);
            assert_eq!(reg.snapshot(), (100, 250));
            assert_eq!(reg.in_flight(), 2);
            {
                let _b2 = AdmitGuard::new(reg.clone(), WorkKind::Witness, 50);
                assert_eq!(reg.snapshot(), (150, 250));
                assert_eq!(reg.in_flight(), 3);
            }
            // inner witness guard dropped
            assert_eq!(reg.snapshot(), (100, 250));
            assert_eq!(reg.in_flight(), 2);
        }
        // all dropped
        assert_eq!(reg.snapshot(), (0, 0));
        assert_eq!(reg.in_flight(), 0);
    }
}
