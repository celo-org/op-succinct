use std::sync::Arc;

use op_succinct_host_utils::{
    block_range::SpanBatchRange, fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost,
    stats::ExecutionStats, witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::get_range_elf_embedded;
use rkyv::rancor::Error as RkyvError;
use sp1_sdk::{
    blocking::{CpuProver, Prover},
    Elf,
};

use crate::{cache::WitnessCache, error::EstimatorError};

/// In-process estimator over a DA-specific host `H`. Producer + consumer share one.
pub struct Estimator<H: OPSuccinctHost> {
    pub host: Arc<H>,
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    pub cache: WitnessCache,
    pub chain_id: u64,
    pub safe_db_fallback: bool,
}

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

impl<H: OPSuccinctHost> Estimator<H>
where
    // Mirror cache.rs's save_witness/load_witness rkyv bounds so this impl can call them.
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive,
    <WitnessOf<H> as rkyv::Archive>::Archived:
        rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
            + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    /// Producer: fetch `WitnessData` (host.run) then crunch to `SP1Stdin`, caching both,
    /// then drop the witness blob. A cached witness skips host.run; a cached stdin is a no-op.
    pub async fn build_range_witness(&self, range: &SpanBatchRange) -> Result<(), EstimatorError> {
        if self.cache.has_stdin(range.start, range.end) {
            return Ok(()); // already built
        }

        // 1. WitnessData: load from cache, else host.fetch + host.run, then cache it.
        let witness: WitnessOf<H> = match self
            .cache
            .load_witness::<WitnessOf<H>>(range.start, range.end)
            .map_err(EstimatorError::Transient)?
        {
            Some(w) => w,
            None => {
                let args = self
                    .host
                    .fetch(range.start, range.end, None, self.safe_db_fallback)
                    .await
                    .map_err(EstimatorError::classify)?;
                let w = self.host.run(&args).await.map_err(EstimatorError::classify)?;
                self.cache
                    .save_witness(range.start, range.end, &w)
                    .map_err(EstimatorError::Transient)?;
                w
            }
        };

        // 2. SP1Stdin: pure CPU serialization of the witness, then cache it.
        let stdin = self
            .host
            .witness_generator()
            .get_sp1_stdin(witness)
            .map_err(EstimatorError::classify)?;
        self.cache
            .save_stdin(range.start, range.end, &stdin)
            .map_err(EstimatorError::Transient)?;

        // 3. Drop the (large) witness blob — only needed for a re-crunch.
        self.cache
            .drop_witness(range.start, range.end)
            .map_err(EstimatorError::Transient)?;
        Ok(())
    }

    /// Consumer: load the cached stdin (build on miss), run the SP1 execute, and
    /// produce `ExecutionStats`. The caller must hold an RSS-admission slot.
    pub async fn execute_range(
        &self,
        range: &SpanBatchRange,
    ) -> Result<ExecutionStats, EstimatorError> {
        // Ensure stdin exists (cache hit is the common path; miss builds on demand).
        if !self.cache.has_stdin(range.start, range.end) {
            self.build_range_witness(range).await?;
        }
        let stdin = self
            .cache
            .load_stdin(range.start, range.end)
            .map_err(EstimatorError::Transient)?
            .ok_or_else(|| EstimatorError::Fatal(anyhow::anyhow!("stdin missing after build")))?;

        // Block data for stats (cheap relative to witness-gen). Parity: l1_head passed as 0.
        let block_data = self
            .fetcher
            .get_l2_block_data_range(range.start, range.end)
            .await
            .map_err(EstimatorError::classify)?;

        // SP1 execute must run off the async runtime: CpuProver spins its own tokio runtime.
        let exec = tokio::task::spawn_blocking(move || {
            let prover = CpuProver::new();
            prover
                .execute(Elf::Static(get_range_elf_embedded()), stdin)
                .deferred_proof_verification(false)
                .run()
        })
        .await
        .map_err(|e| EstimatorError::Fatal(anyhow::anyhow!("execute task join error: {e}")))?;

        let (_public_values, report) = exec
            .map_err(|e| EstimatorError::classify(anyhow::anyhow!("SP1 execute failed: {e:?}")))?;

        Ok(ExecutionStats::new(0, &block_data, &report, 0, 0))
    }
}
