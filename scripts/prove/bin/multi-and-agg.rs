use alloy_primitives::{Address, Bytes, FixedBytes};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_sol_macro::sol;
use alloy_transport_http::reqwest::Url;
use anyhow::{Context, Result};
use clap::Parser;
use op_succinct_client_utils::boot::BootInfoStruct;
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{
    block_range::get_validated_block_range, fetcher::OPSuccinctDataFetcher, get_agg_proof_stdin,
    host::OPSuccinctHost, network::parse_fulfillment_strategy,
    witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::{get_range_elf_embedded, initialize_host};
use op_succinct_prove::DEFAULT_RANGE;
use sp1_sdk::{
    network::FulfillmentStrategy, utils, HashableKey, Prover, ProverClient, SP1ProofMode,
};
use std::{env, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

/// Execute the OP Succinct program for multiple blocks.
#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider().install_default().unwrap();

    let args = Args::parse();

    dotenv::from_path(&args.env_file)
        .context(format!("Environment file not found: {}", args.env_file.display()))?;
    utils::setup_logger();

    let config = Config::from_env().expect("failed to get config");

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

    let host_args = host.fetch(l2_start_block, l2_end_block, None, config.safe_db_fallback).await?;
    let witness_data = host.run(&host_args).await?;

    // Get the stdin for the block.
    let sp1_stdin = host.witness_generator().get_sp1_stdin(witness_data)?;
    let stdin_bytes = bincode::serialize(&sp1_stdin).unwrap();
    let stdin_len = stdin_bytes.len();
    tracing::info!("Generated SP1 stdin for blocks {l2_start_block} to {l2_end_block}, number: {:?}, size: {stdin_len} bytes", l2_end_block - l2_start_block);

    // If the prove flag is set, generate a proof.
    let network_prover = ProverClient::builder().network().build();

    tracing::info!("Generating Range Proof for blocks {l2_start_block} to {l2_end_block}");
    let (range_pk, range_vk) = network_prover.setup(get_range_elf_embedded());

    let range_proof = network_prover
        .prove(&range_pk, &sp1_stdin)
        .compressed()
        .skip_simulation(true)
        .strategy(config.range_proof_strategy)
        .timeout(Duration::from_secs(config.timeout))
        .min_auction_period(config.min_auction_period)
        .max_price_per_pgu(config.max_price_per_pgu)
        .cycle_limit(config.range_cycle_limit)
        .gas_limit(config.range_gas_limit)
        .run_async()
        .await?;

    tracing::info!("Preparing Stdin for Agg Proof");
    let proof = range_proof.proof.clone();
    let mut public_values = range_proof.public_values.clone();
    let boot_info: BootInfoStruct = public_values.read();
    let (agg_pk, agg_vk) = network_prover.setup(AGGREGATION_ELF);

    let headers = match data_fetcher
        .get_header_preimages(&vec![boot_info.clone()], boot_info.clone().l1Head)
        .await
    {
        Ok(headers) => headers,
        Err(e) => {
            tracing::error!("Failed to get header preimages: {}", e);
            return Err(anyhow::anyhow!("Failed to get header preimages: {}", e));
        }
    };

    let sp1_stdin = match get_agg_proof_stdin(
        vec![proof],
        vec![boot_info.clone()],
        headers,
        &range_vk,
        boot_info.l1Head,
        config.proposer_address,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to get agg proof stdin: {}", e);
            return Err(anyhow::anyhow!("Failed to get agg proof stdin: {}", e));
        }
    };

    tracing::info!("Generating Agg Proof");

    let agg_proof = network_prover
        .prove(&agg_pk, &sp1_stdin)
        .mode(config.agg_proof_mode)
        .strategy(config.agg_proof_strategy)
        .timeout(Duration::from_secs(config.timeout))
        .min_auction_period(config.min_auction_period)
        .max_price_per_pgu(config.max_price_per_pgu)
        .cycle_limit(config.agg_cycle_limit)
        .gas_limit(config.agg_gas_limit)
        .run_async()
        .await?;

    tracing::info!("Aggregation proof generated successfully.");

    if args.verify {
        tracing::info!("Verifying aggregation proof on contract");

        let l1_provider: RootProvider<alloy_network::Ethereum> =
            ProviderBuilder::default().connect_http(config.l1_rpc.clone());
        let l1_chain_id = l1_provider.get_chain_id().await.context("failed to fetch chain ID")?;

        let verifier_address = match (l1_chain_id, config.agg_proof_mode) {
            (1, SP1ProofMode::Groth16) => "0x397A5f7f3dBd538f23DE225B51f532c34448dA9B",
            (1, SP1ProofMode::Plonk) => "0x3B6041173B80E77f038f3F2C0f9744f04837185e",
            (11155111, SP1ProofMode::Groth16) => "0x397A5f7f3dBd538f23DE225B51f532c34448dA9B",
            (11155111, SP1ProofMode::Plonk) => "0x3B6041173B80E77f038f3F2C0f9744f04837185e",
            _ => anyhow::bail!(
                "Unsupported verifier: l1_chain_id={l1_chain_id}, agg_proof_mode={:?}",
                config.agg_proof_mode
            ),
        };

        let verifier = ISP1Verifier::new(
            Address::from_str(verifier_address).context("failed to parse address")?,
            data_fetcher.l1_provider.clone(),
        );

        let public_values: Bytes = Bytes::copy_from_slice(agg_proof.public_values.as_slice());
        let proof_bytes: Bytes = agg_proof.bytes().to_vec().into();

        match verifier
            .verifyProof(FixedBytes(agg_vk.bytes32_raw()), public_values, proof_bytes)
            .gas(30_000_000)
            .call()
            .await
        {
            Ok(_) => tracing::info!("verifyProof call succeeded"),
            Err(e) => tracing::error!("verifyProof call failed: {e}"),
        }
    }

    Ok(())
}

/// The arguments for the host executable.
#[derive(Debug, Clone, Parser)]
pub struct Args {
    /// The start block of the range to execute.
    #[arg(long)]
    pub start: Option<u64>,
    /// The end block of the range to execute.
    #[arg(long)]
    pub end: Option<u64>,
    /// The environment file to use.
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// Whether to verify proofs on chain.
    #[arg(long)]
    pub verify: bool,
}

#[derive(Debug, Clone)]
struct Config {
    pub l1_rpc: Url,

    /// Proposer (Proof creator) address
    pub proposer_address: Address,

    /// Proof fulfillment strategy for range proofs.
    pub range_proof_strategy: FulfillmentStrategy,

    /// Proof fulfillment strategy for aggregation proofs.
    pub agg_proof_strategy: FulfillmentStrategy,

    /// Proof mode for aggregation proofs (Groth16 or Plonk).
    pub agg_proof_mode: SP1ProofMode,

    /// Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not
    /// activated for op-node.
    pub safe_db_fallback: bool,

    /// The maximum price per pgu for proving.
    pub max_price_per_pgu: u64,

    /// The minimum auction period (in seconds).
    pub min_auction_period: u64,

    /// The timeout to use for proving (in seconds).
    pub timeout: u64,

    /// The cycle limit to use for range proofs.
    pub range_cycle_limit: u64,

    /// The gas limit to use for range proofs.
    pub range_gas_limit: u64,

    /// The cycle limit to use for aggregation proofs.
    pub agg_cycle_limit: u64,

    /// The gas limit to use for aggregation proofs.
    pub agg_gas_limit: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            l1_rpc: env::var("L1_RPC")?.parse().expect("L1_RPC not set"),
            proposer_address: env::var("PROPOSER_ADDRESS")?
                .parse()
                .expect("PROPOSER_ADDRESS not set"),
            range_proof_strategy: parse_fulfillment_strategy(
                env::var("RANGE_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            agg_proof_strategy: parse_fulfillment_strategy(
                env::var("AGG_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            agg_proof_mode: if env::var("AGG_PROOF_MODE")
                .unwrap_or("plonk".to_string())
                .to_lowercase() ==
                "groth16"
            {
                SP1ProofMode::Groth16
            } else {
                SP1ProofMode::Plonk
            },
            safe_db_fallback: env::var("SAFE_DB_FALLBACK")
                .unwrap_or("false".to_string())
                .parse()?,
            max_price_per_pgu: env::var("MAX_PRICE_PER_PGU")
                .unwrap_or("300000000".to_string()) // 0.3 PROVE per billion PGU
                .parse()?,
            min_auction_period: env::var("MIN_AUCTION_PERIOD")
                .unwrap_or("1".to_string())
                .parse()?,
            timeout: env::var("TIMEOUT").unwrap_or("14400".to_string()).parse()?, // 4 hours
            range_cycle_limit: env::var("RANGE_CYCLE_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
            range_gas_limit: env::var("RANGE_GAS_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
            agg_cycle_limit: env::var("AGG_CYCLE_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
            agg_gas_limit: env::var("AGG_GAS_LIMIT")
                .unwrap_or("1000000000000".to_string()) // 1 trillion
                .parse()?,
        })
    }
}

sol! {
  #[allow(missing_docs)]
  #[sol(rpc)]
  interface ISP1Verifier {
    function verifyProof(
        bytes32 programVKey,
        bytes calldata publicValues,
        bytes calldata proofBytes
    ) view;
  }
}
