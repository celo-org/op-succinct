use crate::{
    host::OPSuccinctHost, network::parse_fulfillment_strategy, witness_generation::WitnessGenerator,
};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use op_succinct_client_utils::{boot::BootInfoStruct, types::AggregationInputs};
use sp1_sdk::{
    network::FulfillmentStrategy, utils, HashableKey, NetworkProver, Prover, ProverClient,
    SP1Proof, SP1ProofMode, SP1ProvingKey, SP1Stdin,
};
/// Get the stdin for the aggregation proof.
pub fn get_agg_proof_stdin(
    proofs: Vec<SP1Proof>,
    boot_infos: Vec<BootInfoStruct>,
    headers: Vec<Header>,
    multi_block_vkey: &sp1_sdk::SP1VerifyingKey,
    latest_checkpoint_head: B256,
    prover_address: Address,
) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::new();
    for proof in proofs {
        let SP1Proof::Compressed(compressed_proof) = proof else {
            return Err(anyhow::anyhow!("Invalid proof passed as compressed proof!"));
        };
        stdin.write_proof(*compressed_proof, multi_block_vkey.vk.clone());
    }

    // Write the aggregation inputs to the stdin.
    stdin.write(&AggregationInputs {
        boot_infos,
        latest_l1_checkpoint_head: latest_checkpoint_head,
        multi_block_vkey: multi_block_vkey.hash_u32(),
        prover_address,
    });
    // The headers have issues serializing with bincode, so use serde_json instead.
    let headers_bytes = serde_cbor::to_vec(&headers).unwrap();
    stdin.write_vec(headers_bytes);

    Ok(stdin)
}

pub async fn get_range_proof_stdin<T: OPSuccinctHost + Send + Sync + 'static>(
    host: &T,
    start_block: u64,
    end_block: u64,
    l1_head_hash: Option<B256>,
    safe_db_fallback: bool,
) -> Result<SP1Stdin> {
    let host_args = host
        .fetch(start_block, end_block, l1_head_hash, safe_db_fallback)
        .await
        .context("Failed to get host CLI args")?;

    let witness_data = host.run(&host_args).await?;

    let sp1_stdin = match host.witness_generator().get_sp1_stdin(witness_data) {
        Ok(stdin) => stdin,
        Err(e) => {
            tracing::error!("Failed to get proof stdin: {}", e);
            return Err(anyhow::anyhow!("Failed to get proof stdin: {}", e));
        }
    };

    Ok(sp1_stdin)
}

pub async fn get_network_range_proof<T: OPSuccinctHost + Send + Sync + 'static>(
    sp1_stdin: SP1Stdin,
    range_pk: &SP1ProvingKey,
    prover: &NetworkProver,
    config: &Config,
) -> Result<SP1Proof> {
    let stdin_bytes = bincode::serialize(&sp1_stdin).unwrap();
    let range_proof = prover
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

    let proof = range_proof.proof.clone();
    let mut public_values = range_proof.public_values.clone();
    let boot_info: BootInfoStruct = public_values.read();

    Ok(proof)
}

pub struct ProvingConfig {
    /// Proof fulfillment strategy for range proofs.
    pub strategy: FulfillmentStrategy,

    /// Proof mode (Core, Compressed, Plonk, Groth16).
    pub mode: SP1ProofMode,

    /// The maximum price per pgu for proving.
    pub max_price_per_pgu: u64,

    /// The minimum auction period (in seconds).
    pub min_auction_period: u64,

    /// The timeout to use for proving (in seconds).
    pub timeout: u64,

    /// The cycle limit to use for proving.
    pub cycle_limit: u64,

    /// The gas limit to use for proving.
    pub gas_limit: u64,

    /// Whether or not to skip simulation.
    pub skip_simulation: bool,

    // Who may bid on this proof request, when strategy is FulfillmentStrategy::Auction.
    pub whitelist: Option<Vec<Address>>,

    /// The auctioneer for this proof request, when strategy is FulfillmentStrategy::Auction.
    pub auctioneer: Option<Address>,

    /// The executor for this proof request, when strategy is FulfillmentStrategy::Auction.
    pub executor: Option<Address>,

    /// The verifier for this proof request, when strategy is FulfillmentStrategy::Auction.
    pub verifier: Option<Address>,

    /// How long to wait before cancelling a proof request that hasn't been assigned.
    pub auction_timeout: u64,
}

use std::env;

impl ProvingConfig {
    /// Create a ProvingConfig from environment variables with a given prefix.
    /// For example, prefix "RANGE" reads "RANGE_STRATEGY", "RANGE_MODE", etc.
    pub fn from_env_with_prefix(prefix: &str) -> Result<Self> {
        let get_var = |name: &str| env::var(format!("{}_{}", prefix, name));
        let get_var_or = |name: &str, default: &str| {
            env::var(format!("{}_{}", prefix, name)).unwrap_or_else(|_| default.to_string())
        };

        Ok(Self {
            strategy: parse_fulfillment_strategy(get_var_or("STRATEGY", "reserved")),

            mode: match get_var_or("MODE", "compressed").to_lowercase().as_str() {
                "core" => SP1ProofMode::Core,
                "groth16" => SP1ProofMode::Groth16,
                "plonk" => SP1ProofMode::Plonk,
                _ => SP1ProofMode::Compressed,
            },

            max_price_per_pgu: get_var_or("MAX_PRICE_PER_PGU", "300000000")
                .parse()
                .context(format!("{}_MAX_PRICE_PER_PGU must be a valid u64", prefix))?,

            min_auction_period: get_var_or("MIN_AUCTION_PERIOD", "1")
                .parse()
                .context(format!("{}_MIN_AUCTION_PERIOD must be a valid u64", prefix))?,

            timeout: get_var_or("TIMEOUT", "14400") // 4 hours
                .parse()
                .context(format!("{}_TIMEOUT must be a valid u64", prefix))?,

            cycle_limit: get_var_or("CYCLE_LIMIT", "1000000000000") // 1 trillion
                .parse()
                .context(format!("{}_CYCLE_LIMIT must be a valid u64", prefix))?,

            gas_limit: get_var_or("GAS_LIMIT", "1000000000000") // 1 trillion
                .parse()
                .context(format!("{}_GAS_LIMIT must be a valid u64", prefix))?,

            skip_simulation: get_var_or("SKIP_SIMULATION", "true")
                .parse()
                .context(format!("{}_SKIP_SIMULATION must be a valid bool", prefix))?,

            whitelist: get_var("WHITELIST").ok().map(|s| {
                s.split(',')
                    .filter(|s| !s.is_empty())
                    .map(|addr| addr.trim().parse().expect("Invalid address in whitelist"))
                    .collect()
            }),

            auctioneer: get_var("AUCTIONEER")
                .ok()
                .map(|s| s.parse().expect("Invalid AUCTIONEER address")),

            executor: get_var("EXECUTOR")
                .ok()
                .map(|s| s.parse().expect("Invalid EXECUTOR address")),

            verifier: get_var("VERIFIER")
                .ok()
                .map(|s| s.parse().expect("Invalid VERIFIER address")),

            auction_timeout: get_var_or("AUCTION_TIMEOUT", "300") // 5 minutes
                .parse()
                .context(format!("{}_AUCTION_TIMEOUT must be a valid u64", prefix))?,
        })
    }

    /// Create a ProvingConfig for range proofs from RANGE_* env vars.
    pub fn range_from_env() -> Result<Self> {
        Self::from_env_with_prefix("RANGE")
    }

    /// Create a ProvingConfig for aggregation proofs from AGG_* env vars.
    pub fn agg_from_env() -> Result<Self> {
        Self::from_env_with_prefix("AGG")
    }
}
