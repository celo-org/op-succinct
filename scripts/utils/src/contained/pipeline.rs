use std::sync::Arc;

use op_succinct_estimator::{memory::WorkKind, Estimator};
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost, witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Semaphore;

use crate::contained::{
    admission::{current_rss_bytes, Admission},
    source::WitnessSource,
};

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Drain every range the source reports ready this tick and build it. The source decides
/// *what* is ready (window prediction + splitting + finalization gating); this just builds.
/// Builds run concurrently, gated by RSS admission + the shared semaphore. Best-effort: a
/// failed build is left for the executor to rebuild on a cache miss, so we don't track or
/// retry it here — the source's forward cursor has already moved past it.
pub async fn pipeline_step<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    source: &mut WitnessSource,
    permits: &Arc<Semaphore>,
    admission: &Admission,
) -> anyhow::Result<()>
where
    // Mirror Estimator<H>'s impl rkyv bounds so this can call build_range_witness.
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
    // Pull all ranges ready right now (the source stops at the first not-yet-ready one).
    let mut ready = Vec::new();
    while let Some(range) = source.next_range(fetcher).await {
        ready.push(range);
    }

    // Build them concurrently; admission + the semaphore bound how many run at once.
    let builds = ready.into_iter().map(|range| async move {
        let gas: u64 = match fetcher.get_l2_block_data_range(range.start, range.end).await {
            Ok(bd) => bd.iter().map(|b| b.gas_used).sum(),
            Err(e) => {
                tracing::debug!(start = range.start, end = range.end, error = %e,
                    "skipping build: block-data fetch failed; will retry next tick");
                return;
            }
        };
        admission.admit(WorkKind::Build, gas).await;
        let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");
        if let Err(e) = estimator.build_range_witness(&range).await {
            tracing::warn!(start = range.start, end = range.end, error = %e, "pipeline build failed");
            return;
        }
        admission.record(WorkKind::Build, gas, current_rss_bytes());
    });
    futures::future::join_all(builds).await;

    Ok(())
}
