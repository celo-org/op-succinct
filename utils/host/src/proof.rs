use crate::{host::OPSuccinctHost, witness_generation::WitnessGenerator};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use op_succinct_client_utils::{boot::BootInfoStruct, types::AggregationInputs};
use sp1_sdk::{
    network::FulfillmentStrategy, HashableKey, NetworkProver, SP1Proof, SP1ProofMode,
    SP1ProofWithPublicValues, SP1ProvingKey, SP1Stdin,
};
use std::env;
use tokio::time::Duration;

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

macro_rules! maybe_set {
    ($builder:expr, $opt:expr, $method:ident) => {
        match $opt {
            Some(val) => $builder.$method(val),
            None => $builder,
        }
    };
}

pub async fn get_network_proof(
    sp1_stdin: SP1Stdin,
    range_pk: &SP1ProvingKey,
    prover: &NetworkProver,
    config: &ProvingConfig,
) -> Result<SP1ProofWithPublicValues> {
    let builder = prover.prove(range_pk, &sp1_stdin);

    let builder = maybe_set!(builder, config.strategy, strategy);
    let builder = maybe_set!(builder, config.mode, mode);
    let builder = maybe_set!(builder, config.cycle_limit, cycle_limit);
    let builder = maybe_set!(builder, config.gas_limit, gas_limit);
    let builder = maybe_set!(builder, config.max_price_per_pgu, max_price_per_pgu);
    let builder = builder.skip_simulation(config.skip_simulation);
    let builder = maybe_set!(builder, config.proving_timeout, timeout);
    let builder = maybe_set!(builder, config.min_auction_period, min_auction_period);
    let builder = maybe_set!(builder, config.auction_timeout, auction_timeout);
    let builder = builder.whitelist(config.whitelist.clone());
    let builder = maybe_set!(builder, config.auctioneer, auctioneer);
    let builder = maybe_set!(builder, config.executor, executor);
    let builder = maybe_set!(builder, config.verifier, verifier);

    let proof = builder.run_async().await?;

    Ok(proof.clone())
}

// let agg_proof = network_prover
// .prove(&agg_pk, &sp1_stdin)
// .mode(config.agg_proof_mode)
// .strategy(config.agg_proof_strategy)
// .timeout(Duration::from_secs(config.timeout))
// .min_auction_period(config.min_auction_period)
// .max_price_per_pgu(config.max_price_per_pgu)
// .cycle_limit(config.agg_cycle_limit)
// .gas_limit(config.agg_gas_limit)
// .run_async()
// .await?;

// impl ProvingConfig {
//     /// Create a ProvingConfig from environment variables with a given prefix.
//     /// For example, prefix "RANGE" reads "RANGE_STRATEGY", "RANGE_MODE", etc.
//     pub fn from_env_with_prefix(prefix: &str) -> Result<Self> {
//         let get_var = |name: &str| env::var(format!("{}_{}", prefix, name));
//         let get_var_or = |name: &str, default: &str| {
//             env::var(format!("{}_{}", prefix, name)).unwrap_or_else(|_| default.to_string())
//         };

//         Ok(Self {
//             strategy: parse_fulfillment_strategy(get_var_or("STRATEGY", "reserved")),

//             mode: match get_var_or("MODE", "compressed").to_lowercase().as_str() {
//                 "core" => SP1ProofMode::Core,
//                 "groth16" => SP1ProofMode::Groth16,
//                 "plonk" => SP1ProofMode::Plonk,
//                 _ => SP1ProofMode::Compressed,
//             },

//             max_price_per_pgu: get_var_or("MAX_PRICE_PER_PGU", "300000000")
//                 .parse()
//                 .context(format!("{}_MAX_PRICE_PER_PGU must be a valid u64", prefix))?,

//             min_auction_period: get_var_or("MIN_AUCTION_PERIOD", "1")
//                 .parse()
//                 .context(format!("{}_MIN_AUCTION_PERIOD must be a valid u64", prefix))?,

//             proving_timeout: get_var_or("TIMEOUT", "14400") // 4 hours
//                 .parse()
//                 .context(format!("{}_TIMEOUT must be a valid u64", prefix))?,

//             cycle_limit: get_var_or("CYCLE_LIMIT", "1000000000000") // 1 trillion
//                 .parse()
//                 .context(format!("{}_CYCLE_LIMIT must be a valid u64", prefix))?,

//             gas_limit: get_var_or("GAS_LIMIT", "1000000000000") // 1 trillion
//                 .parse()
//                 .context(format!("{}_GAS_LIMIT must be a valid u64", prefix))?,

//             skip_simulation: get_var_or("SKIP_SIMULATION", "true")
//                 .parse()
//                 .context(format!("{}_SKIP_SIMULATION must be a valid bool", prefix))?,

//             whitelist: get_var("WHITELIST").ok().map(|s| {
//                 s.split(',')
//                     .filter(|s| !s.is_empty())
//                     .map(|addr| addr.trim().parse().expect("Invalid address in whitelist"))
//                     .collect()
//             }),

//             auctioneer: get_var("AUCTIONEER")
//                 .ok()
//                 .map(|s| s.parse().expect("Invalid AUCTIONEER address")),

//             executor: get_var("EXECUTOR")
//                 .ok()
//                 .map(|s| s.parse().expect("Invalid EXECUTOR address")),

//             verifier: get_var("VERIFIER")
//                 .ok()
//                 .map(|s| s.parse().expect("Invalid VERIFIER address")),

//             auction_timeout: get_var_or("AUCTION_TIMEOUT", "300") // 5 minutes
//                 .parse()
//                 .context(format!("{}_AUCTION_TIMEOUT must be a valid u64", prefix))?,
//         })
//     }

//     /// Create a ProvingConfig for range proofs from RANGE_* env vars.
//     pub fn range_from_env() -> Result<Self> {
//         Self::from_env_with_prefix("RANGE")
//     }

//     /// Create a ProvingConfig for aggregation proofs from AGG_* env vars.
//     pub fn agg_from_env() -> Result<Self> {
//         Self::from_env_with_prefix("AGG")
//     }
// }

#[derive(Debug, Clone)]
pub struct ProvingConfig {
    pub strategy: Option<FulfillmentStrategy>,
    pub mode: Option<SP1ProofMode>,
    pub cycle_limit: Option<u64>,
    pub gas_limit: Option<u64>,
    pub max_price_per_pgu: Option<u64>,
    pub skip_simulation: bool,
    pub proving_timeout: Option<Duration>,
    pub min_auction_period: Option<u64>,
    pub auction_timeout: Option<Duration>,
    pub whitelist: Option<Vec<Address>>,
    pub auctioneer: Option<Address>,
    pub executor: Option<Address>,
    pub verifier: Option<Address>,
}

impl ProvingConfig {
    /// Load a ProvingConfig from environment variables with the given prefix.
    ///
    /// For example, with prefix "RANGE", reads:
    /// - RANGE_STRATEGY, RANGE_MODE, RANGE_CYCLE_LIMIT, etc.
    ///
    /// All fields are optional - missing env vars result in None.
    pub fn from_env_with_prefix(prefix: &str) -> Result<Self> {
        let get = |suffix: &str| env::var(format!("{}_{}", prefix, suffix)).ok();

        let parse_duration = |suffix: &str| -> Option<Duration> {
            get(suffix).and_then(|s| humantime::parse_duration(&s).ok())
        };

        let parse_u64 = |suffix: &str| -> Option<u64> { get(suffix).and_then(|s| s.parse().ok()) };

        let parse_address =
            |suffix: &str| -> Option<Address> { get(suffix).and_then(|s| s.parse().ok()) };

        let parse_addresses = |suffix: &str| -> Option<Vec<Address>> {
            get(suffix).map(|s| {
                s.split(',')
                    .filter(|s| !s.trim().is_empty())
                    .filter_map(|addr| addr.trim().parse().ok())
                    .collect()
            })
        };

        Ok(Self {
            strategy: get("STRATEGY").and_then(|s| match s.to_lowercase().as_str() {
                "reserved" => Some(FulfillmentStrategy::Reserved),
                "hosted" => Some(FulfillmentStrategy::Hosted),
                "auction" => Some(FulfillmentStrategy::Auction),
                _ => None,
            }),
            mode: get("MODE").and_then(|s| match s.to_lowercase().as_str() {
                "core" => Some(SP1ProofMode::Core),
                "compressed" => Some(SP1ProofMode::Compressed),
                "plonk" => Some(SP1ProofMode::Plonk),
                "groth16" => Some(SP1ProofMode::Groth16),
                _ => None,
            }),
            cycle_limit: parse_u64("CYCLE_LIMIT"),
            gas_limit: parse_u64("GAS_LIMIT"),
            max_price_per_pgu: parse_u64("MAX_PRICE_PER_PGU"),
            skip_simulation: get("SKIP_SIMULATION").and_then(|s| s.parse().ok()).unwrap_or(false),
            proving_timeout: parse_duration("PROVING_TIMEOUT"),
            min_auction_period: parse_u64("MIN_AUCTION_PERIOD"),
            auction_timeout: parse_duration("AUCTION_TIMEOUT"),
            whitelist: parse_addresses("WHITELIST"),
            auctioneer: parse_address("AUCTIONEER"),
            executor: parse_address("EXECUTOR"),
            verifier: parse_address("VERIFIER"),
        })
    }

    /// Load range proof config from RANGE_* env vars.
    pub fn range_from_env() -> Result<Self> {
        Self::from_env_with_prefix("RANGE")
    }

    /// Load aggregation proof config from AGG_* env vars.
    pub fn agg_from_env() -> Result<Self> {
        Self::from_env_with_prefix("AGG")
    }
}
