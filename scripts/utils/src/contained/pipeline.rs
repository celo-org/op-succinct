use std::sync::Arc;

use op_succinct_estimator::{memory::WorkKind, Estimator};
use op_succinct_host_utils::{
    block_range::SpanBatchRange, fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Semaphore;

use crate::contained::admission::{current_rss_bytes, Admission};

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Build the witness for ONE sub-range — a single step of the pipeline. Projects the unit's
/// memory from its gas, waits for admission headroom, takes a semaphore permit, runs
/// `host.run` + `get_sp1_stdin` (cached by `build_range_witness`), and records the observed
/// footprint. The caller pulls ranges from the `ReadyRangeProvider` and drives one of these
/// per range; concurrency, if any, is the caller's concern.
pub async fn pipeline_step<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    permits: &Arc<Semaphore>,
    admission: &Admission,
    range: &SpanBatchRange,
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
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    // Gas-weighted RSS projection key: sum the sub-range's L2 block gas.
    let block_data = fetcher.get_l2_block_data_range(range.start, range.end).await?;
    let gas: u64 = block_data.iter().map(|b| b.gas_used).sum();

    // Adaptive admission: block until this build's projected peak fits the budget given live
    // cgroup usage; the semaphore stays as the hard concurrency cap.
    admission.admit(WorkKind::Build, gas).await;
    let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");

    estimator.build_range_witness(range).await?;
    admission.record(WorkKind::Build, gas, current_rss_bytes());
    Ok(())
}
