use std::{env, fs, sync::Arc, time::Instant};

use anyhow::{Context, Result};
use clap::Parser;
use op_succinct_host_utils::{
    block_range::get_validated_block_range, fetcher::OPSuccinctDataFetcher, get_range_proof_stdin,
    get_split_range_agg_proof_input, network::parse_fulfillment_strategy, stats::ExecutionStats,
};
use op_succinct_proof_utils::{get_range_elf_embedded, initialize_host};
use op_succinct_prove::{execute_multi, DEFAULT_RANGE};
use op_succinct_scripts::HostExecutorArgs;
use sp1_sdk::{utils, Prover, ProverClient, SP1ProofWithPublicValues};
use tracing::info;

/// Execute the OP Succinct program for multiple blocks.
#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider().install_default().unwrap();

    let args = HostExecutorArgs::parse();

    dotenv::from_path(&args.env_file)
        .context(format!("Environment file not found: {}", args.env_file.display()))?;
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    let host = initialize_host(Arc::new(data_fetcher.clone()));

    // If the end block is provided, check that it is less than the latest finalized block. If the
    // end block is not provided, use the latest finalized block.
    let (l2_start_block, l2_end_block) = get_validated_block_range(
        host.as_ref(),
        &data_fetcher,
        args.start,
        args.end,
        DEFAULT_RANGE,
    )
    .await?;

    let (sp1_stdin, total_instruction_cycles, total_sp1_gas) = get_split_range_agg_proof_input(
        host,
        None,
        args.safe_db_fallback,
        &|sp1_stdin| async move {
            let proof = SP1ProofWithPublicValues::create_mock_proof(
                &self.prover.range_pk,
                public_values,
                SP1ProofMode::Compressed,
                SP1_CIRCUIT_VERSION,
            );
            Ok((proof, 0, 0))
        },
        l2_start_block,
        l2_end_block,
        args.segments,
        range_vk,
        signer_address,
        fetcher,
    )
    .await?;

    let start_time = Instant::now();
    let sp1_stdin = get_range_proof_stdin(
        host.as_ref(),
        l2_start_block,
        l2_end_block,
        None,
        args.safe_db_fallback,
    )
    .await?;
    let witness_generation_duration = start_time.elapsed();

    let stdin_bytes = bincode::serialize(&sp1_stdin).unwrap();
    let stdin_len = stdin_bytes.len();
    info!("Generated SP1 stdin for blocks {l2_start_block} to {l2_end_block}, number: {:?}, size: {stdin_len} bytes", l2_end_block - l2_start_block);

    if args.prove {
        // If the prove flag is set, generate a proof.
        let network_prover = ProverClient::builder().network().build();

        let (pk, _) = network_prover.setup(get_range_elf_embedded());

        // Generate a range proof in compressed mode for aggregation verification.
        let proof = network_prover
            .prove(&pk, &sp1_stdin)
            .compressed()
            .strategy(parse_fulfillment_strategy(env::var("RANGE_PROOF_STRATEGY")?))
            .run()
            .unwrap();

        // Create a proof directory for the chain ID if it doesn't exist.
        let proof_dir = format!("data/{}/proofs", data_fetcher.get_l2_chain_id().await.unwrap());
        if !std::path::Path::new(&proof_dir).exists() {
            fs::create_dir_all(&proof_dir).unwrap();
        }
        // Save the proof to the proof directory corresponding to the chain ID.
        proof
            .save(format!("{proof_dir}/{l2_start_block}-{l2_end_block}.bin"))
            .expect("saving proof failed");
    } else {
        let l2_chain_id = data_fetcher.get_l2_chain_id().await?;
    }

    Ok(())
}
