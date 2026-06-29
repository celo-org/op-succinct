use op_succinct_estimator::{memory::WorkKind, Estimator};
use op_succinct_host_utils::{
    block_range::SpanBatchRange, fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;

use crate::game_monitor_embedded::admission::Admission;

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Build the witness for ONE sub-range — a single step of the pipeline. Waits for
/// admission, which registers this build's gas (and a concurrency slot) for the duration
/// via the returned guard, then runs `host.run` + `get_sp1_stdin` (cached by
/// `build_range_witness`). The sampler observes the resulting footprint out of band. The
/// caller pulls ranges from the `ReadyRangeProvider` and drives one of these per range;
/// concurrency, if any, is the caller's concern.
///
/// Runs in a per-range span (spec §4.6) — the pipeline has no `game`/`attempt` context, so
/// builds are attributed to `range` alone, with the host.run/get_sp1_stdin child spans nested
/// underneath.
#[tracing::instrument(
    name = "range",
    skip_all,
    fields(start = range.start, end = range.end, work = "build")
)]
pub async fn pipeline_step<H: OPSuccinctHost>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
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

    // Adaptive admission: block until this build fits the memory budget and the concurrency
    // cap. The guard keeps the build's gas and slot registered until it drops.
    let _admit = admission.admit(WorkKind::Build, gas).await;

    estimator.build_range_witness(range).await?;
    Ok(())
}
