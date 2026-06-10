use std::sync::Arc;

use op_succinct_host_utils::{
    block_range::SpanBatchRange, fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;

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
}
