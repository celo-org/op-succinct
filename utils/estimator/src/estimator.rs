use std::sync::Arc;

use op_succinct_host_utils::{
    block_range::SpanBatchRange,
    fetcher::{BlockInfo, OPSuccinctDataFetcher},
    host::OPSuccinctHost,
    stats::ExecutionStats,
    witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::get_range_elf_embedded;
use rkyv::rancor::Error as RkyvError;
use sp1_sdk::{
    blocking::{CpuProver, Prover},
    Elf,
};
use tracing::Instrument;

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
    // Mirror cache.rs's save_witness/load_witness rkyv bounds so
    // this impl can call them.
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive,
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    /// Producer: fetch `WitnessData` (host.run) then crunch to `SP1Stdin`, caching both,
    /// then drop the witness blob. A cached witness skips host.run; a cached stdin is a no-op.
    pub async fn witness_range(&self, range: &SpanBatchRange) -> Result<(), EstimatorError> {
        if self.cache.has_stdin(range.start, range.end) {
            // Already generated. Best-effort reclaim of a witness blob a prior run may have leaked if
            // its post-stdin `drop_witness` (step 3) failed — that retry path lands here and can
            // never reach the drop below. Normally the blob is already gone, so this is one cheap
            // `exists()` check.
            if let Err(e) = self.cache.drop_witness(range.start, range.end) {
                tracing::warn!(
                    start = range.start,
                    end = range.end,
                    error = %e,
                    "drop_witness (retry sweep) failed; leaving witness blob for the size-cap GC"
                );
            }
            return Ok(()); // witness already generated
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
                // `host.run` as a child span (spec §4.6): the heaviest, most failure-prone
                // step, attributable under the enclosing range/game span.
                let w = self
                    .host
                    .run(&args)
                    .instrument(tracing::info_span!("host.run"))
                    .await
                    .map_err(EstimatorError::classify)?;
                self.cache
                    .save_witness(range.start, range.end, &w)
                    .map_err(EstimatorError::Transient)?;
                w
            }
        };

        // 2. SP1Stdin: pure CPU serialization of the witness, then cache it. Synchronous, so
        // an entered span guard (no await held across) is correct here.
        let stdin = {
            let _span = tracing::info_span!("get_sp1_stdin").entered();
            self.host
                .witness_generator()
                .get_sp1_stdin(witness)
                .map_err(EstimatorError::classify)?
        };
        self.cache.save_stdin(range.start, range.end, &stdin).map_err(EstimatorError::Transient)?;

        // 3. Best-effort drop of the (large) witness blob — it is only needed to re-crunch stdin,
        // which is now durably cached, so a failure here is a cleanup problem, not a witness failure.
        // Returning an error would both falsely report witness generation as failed AND leak the blob: the
        // retry would short-circuit at `has_stdin` above (which now re-attempts this drop). Leave a
        // failed drop for that sweep or the size-cap GC.
        if let Err(e) = self.cache.drop_witness(range.start, range.end) {
            tracing::warn!(
                start = range.start,
                end = range.end,
                error = %e,
                "drop_witness failed; stdin is cached, leaving witness blob for the size-cap GC"
            );
        }
        Ok(())
    }

    /// Consumer: load the cached stdin (generate the witness on miss), run the SP1 prove, and
    /// produce `ExecutionStats`. `block_data` for the range is supplied by the caller — the
    /// prover already fetched it to compute the admission gas key — so this avoids a
    /// second `get_l2_block_data_range` round-trip per prove. The caller must hold an
    /// RSS-admission slot.
    pub async fn prove_range(
        &self,
        range: &SpanBatchRange,
        block_data: &[BlockInfo],
    ) -> Result<ExecutionStats, EstimatorError> {
        // Ensure stdin exists (cache hit is the common path; miss generates the witness on demand).
        if !self.cache.has_stdin(range.start, range.end) {
            self.witness_range(range).await?;
        }
        let stdin = self
            .cache
            .load_stdin(range.start, range.end)
            .map_err(EstimatorError::Transient)?
            .ok_or_else(|| {
                EstimatorError::Fatal(anyhow::anyhow!("stdin missing after witness generation"))
            })?;

        // SP1 prove must run off the async runtime: CpuProver spins its own tokio runtime.
        // `prove` as a child span (spec §4.6). The span is entered INSIDE the blocking
        // closure — instrumenting the JoinHandle future only covers the await, so the SP1
        // executor's own logs (`sp1_core_executor::*`), which run on the blocking thread,
        // would otherwise escape the span. Created here so it parents to the current
        // range/game span, then moved onto the blocking thread.
        let prove_span = tracing::info_span!("prove");
        let exec = tokio::task::spawn_blocking(move || {
            let _entered = prove_span.enter();
            let prover = CpuProver::new();
            prover
                .execute(Elf::Static(get_range_elf_embedded()), stdin)
                .deferred_proof_verification(false)
                .run()
        })
        .await
        .map_err(|e| EstimatorError::Fatal(anyhow::anyhow!("prove task join error: {e}")))?;

        // SP1 prove is deterministic over fixed stdin, so every `ExecutionError` reproduces
        // on retry and is non-retryable. Carry the concrete error through unchanged.
        let (_public_values, report) = exec.map_err(EstimatorError::Sp1Execute)?;

        Ok(ExecutionStats::new(0, block_data, &report, 0, 0))
    }
}
