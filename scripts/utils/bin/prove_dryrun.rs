use std::{env, fs, path::PathBuf, str::FromStr, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use alloy_network::EthereumWallet;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_transport_http::reqwest::Url;
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use fault_proof::contract::{DisputeGameFactory, OPSuccinctFaultDisputeGame, ProposalStatus};
use op_succinct_client_utils::{boot::BootInfoStruct, types::u32_to_u8};
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    get_agg_proof_stdin,
    host::OPSuccinctHost,
    network::{determine_network_mode, get_network_signer, parse_fulfillment_strategy},
    witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::{get_range_elf_embedded, initialize_host};
use sp1_sdk::{
    network::FulfillmentStrategy, utils, HashableKey, Prover, ProverClient, SP1ProofMode,
};
use tracing::info;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The environment file path.
    #[arg(long, default_value = ".env.prove_dryrun")]
    env_file: PathBuf,

    /// Index of the game to prove.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    index: u64,
}

#[derive(Debug, Clone)]
struct Config {
    /// The L1 RPC URL.
    pub l1_rpc: Url,

    /// The address of the factory contract.
    pub factory_address: Address,

    /// Proof fulfillment strategy for range proofs.
    pub range_proof_strategy: FulfillmentStrategy,

    /// Proof fulfillment strategy for aggregation proofs.
    pub agg_proof_strategy: FulfillmentStrategy,

    // Aggregation proof mode (plonk/groth16)
    pub agg_proof_mode: String,

    /// Proposer private key
    pub private_key: String,

    /// Whether to expect NETWORK_PRIVATE_KEY to be an AWS KMS key ARN instead of a
    /// plaintext private key.
    pub use_kms_requester: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            l1_rpc: env::var("L1_RPC")
                .context("L1_RPC must be set")?
                .parse()
                .expect("failed to parse L1_RPC"),
            factory_address: env::var("FACTORY_ADDRESS")
                .context("FACTORY_ADDRESS must be set")?
                .parse()
                .expect("failed to parse FACTORY_ADDRESS"),
            range_proof_strategy: parse_fulfillment_strategy(
                env::var("RANGE_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            agg_proof_strategy: parse_fulfillment_strategy(
                env::var("AGG_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            agg_proof_mode: env::var("AGG_PROOF_MODE")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "plonk".to_string()),
            private_key: env::var("PRIVATE_KEY")
                .context("PRIVATE_KEY must be set")?
                .parse()
                .expect("failed to parse PRIVATE_KEY"),
            use_kms_requester: env::var("USE_KMS_REQUESTER")
                .unwrap_or("false".to_string())
                .parse()?,
        })
    }
}

/// Preflight check for the OP Succinct Fault Dispute Game.
#[tokio::main]
async fn main() -> Result<()> {
    // 1. Set up the environment.
    utils::setup_logger();

    let args = Args::parse();

    dotenv::from_path(&args.env_file)
        .context(format!("Environment file not found: {}", args.env_file.display()))?;

    let config = Config::from_env()?;

    let wallet =
        PrivateKeySigner::from_str(&config.private_key).context("failed to parse private key")?;

    let network_signer = get_network_signer(config.use_kms_requester).await?;
    let network_mode =
        determine_network_mode(config.range_proof_strategy, config.agg_proof_strategy).context(
            "failed to determine network mode from range and agg fulfillment strategies",
        )?;

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    let factory = DisputeGameFactory::new(config.factory_address, data_fetcher.l1_provider.clone());

    let parent_game = OPSuccinctFaultDisputeGame::new(
        factory
            .gameAtIndex(U256::from(args.index - 1))
            .call()
            .await
            .with_context(|| {
                format!("failed to fetch the parent game at index {}", args.index - 1)
            })?
            .proxy,
        data_fetcher.l1_provider.clone(),
    );
    let game = OPSuccinctFaultDisputeGame::new(
        factory
            .gameAtIndex(U256::from(args.index))
            .call()
            .await
            .with_context(|| format!("failed to fetch the game at index {}", args.index))?
            .proxy,
        data_fetcher.l1_provider.clone(),
    );

    info!("Proving for Game #{} (address: {})", args.index, game.address());

    let l1_head_hash: [u8; 32] = game.l1Head().call().await?.0;
    let l2_start_block = parent_game.l2BlockNumber().call().await?.to::<u64>();
    let l2_end_block = game.l2BlockNumber().call().await?.to::<u64>();

    let l1_head = data_fetcher
        .l1_provider
        .get_block_by_hash(l1_head_hash.into())
        .await?
        .expect("failed to fetch L1 head block")
        .header;

    info!(
        l2_start_block,
        l2_end_block,
        l1_head_number = l1_head.number,
        l1_head_hash = %l1_head.hash,
        "Proving L2 block range against L1 head"
    );

    // 2. Generate the range proof.
    let host = initialize_host(Arc::new(data_fetcher.clone()));
    let host_args =
        host.fetch(l2_start_block, l2_end_block, Some(l1_head_hash.into()), false).await?;

    info!("Generating range proof witness data...");
    let witness_data = host.run(&host_args).await?;
    info!("Range proof witness data generated successfully");

    info!("Getting range proof stdin...");
    let range_proof_stdin = host.witness_generator().get_sp1_stdin(witness_data)?;
    info!("Range proof stdin generated successfully");

    // Initialize the network prover.
    let network_prover =
        ProverClient::builder().network_for(network_mode).signer(network_signer.clone()).build();
    info!("Initialized network prover successfully");

    let (range_pk, range_vk) = network_prover.setup(get_range_elf_embedded());
    let mut range_proof = network_prover
        .prove(&range_pk, &range_proof_stdin)
        .compressed()
        .strategy(config.range_proof_strategy)
        .run()
        .unwrap();

    // Save the proof to the proof directory corresponding to the chain ID.
    let range_proof_dir =
        format!("data/{}/proofs/range", data_fetcher.get_l2_chain_id().await.unwrap());
    if !std::path::Path::new(&range_proof_dir).exists() {
        fs::create_dir_all(&range_proof_dir).unwrap();
    }
    range_proof
        .save(format!("{range_proof_dir}/{l2_start_block}-{l2_end_block}.bin"))
        .expect("saving proof failed");
    info!("Range proof saved to {range_proof_dir}/{l2_start_block}-{l2_end_block}.bin");

    // Validation
    let boot_info: BootInfoStruct = range_proof.public_values.read();

    info!("BootInfo L1 head: {:?}", boot_info.l1Head);
    info!("Game L1 head:     {:?}", l1_head.hash);
    assert_eq!(boot_info.l1Head, l1_head.hash, "L1 head hash mismatch");

    let game_root_claim = game.rootClaim().call().await?;
    info!("Boot Info L2PostRoot: {:?}", boot_info.l2PostRoot);
    info!("Game Root Claim:      {:?}", game_root_claim);
    assert_eq!(boot_info.l2PostRoot, game_root_claim, "Root claim mismatch");

    let game_rollup_config_hash = game.rollupConfigHash().call().await?;
    info!("Boot Info Rollup Config Hash: {:?}", boot_info.rollupConfigHash);
    info!("Game Rollup Config Hash:      {:?}", game_rollup_config_hash);
    assert_eq!(boot_info.rollupConfigHash, game_rollup_config_hash, "Rollup config hash mismatch");

    let range_vk_hash = B256::from(u32_to_u8(range_vk.vk.hash_u32()));
    let game_range_v_key_hash = game.rangeVkeyCommitment().call().await?;
    info!("Range Verification Key Hash:      {:?}", range_vk_hash);
    info!("Game Range Verification Key Hash: {:?}", game_range_v_key_hash);
    assert_eq!(range_vk_hash, game_range_v_key_hash, "Range verification key hash mismatch");

    // 3. Generate the aggregation proof.
    let network_prover =
        ProverClient::builder().network_for(network_mode).signer(network_signer).build();
    info!("Initialized network prover successfully");

    let agg_proof_stdin = get_agg_proof_stdin(
        vec![range_proof.proof],
        vec![boot_info.clone()],
        vec![l1_head.clone().into()],
        &range_vk,
        boot_info.l1Head,
        wallet.address(),
    )
    .context("failed to get agg proof stdin")?;

    let agg_proof_mode = match config.agg_proof_mode.to_lowercase().as_str() {
        "groth16" => SP1ProofMode::Groth16,
        "plonk" => SP1ProofMode::Plonk,
        other => {
            return Err(anyhow!(
                "Invalid AGG_PROOF_MODE '{}'. Expected one of: plonk, groth16",
                other
            ))
        }
    };
    info!("Aggregation proof mode: {:?}", agg_proof_mode);

    let (agg_pk, agg_vk) = network_prover.setup(AGGREGATION_ELF);

    let agg_vk_hash = agg_vk.bytes32();
    let game_range_aggregation_v_key = game.aggregationVkey().call().await?.to_string();
    info!("Aggregation Verification Key:      {:?}", range_vk_hash);
    info!("Game Aggregation Verification Key: {}", game_range_aggregation_v_key);
    assert_eq!(
        agg_vk_hash, game_range_aggregation_v_key,
        "Aggregation verification key hash mismatch"
    );

    let agg_proof = network_prover
        .prove(&agg_pk, &agg_proof_stdin)
        .mode(agg_proof_mode)
        .strategy(config.agg_proof_strategy)
        .run()
        .unwrap();

    let agg_proof_dir =
        format!("data/{}/proofs/agg", data_fetcher.get_l2_chain_id().await.unwrap());
    if !std::path::Path::new(&agg_proof_dir).exists() {
        fs::create_dir_all(&agg_proof_dir).unwrap();
    }

    agg_proof.save(format!("{agg_proof_dir}/agg.bin")).expect("saving proof failed");
    info!("Agg proof saved to {agg_proof_dir}/agg.bin");

    // 4. Spin up anvil.
    let fork_number = l1_head.number + 1;

    let anvil =
        Anvil::new().fork(config.l1_rpc).fork_block_number(fork_number).args(["--no-mining"]);
    let anvil_instance = anvil.spawn();
    let endpoint = anvil_instance.endpoint();
    info!("Anvil chain started forked from L1 block number: {} at: {}", fork_number, endpoint);

    // 5. Run the preflight check.
    let provider_with_signer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(wallet))
        .connect_http(Url::parse(&endpoint)?);

    let game = OPSuccinctFaultDisputeGame::new(*game.address(), provider_with_signer.clone());

    let tx = game.prove(agg_proof.bytes().into()).send().await?;

    let client = provider_with_signer.client();
    let _: String = client.request("evm_mine", Vec::<serde_json::Value>::new()).await?;

    let block = provider_with_signer.get_block_by_number(BlockNumberOrTag::Latest).await?;
    info!("Mined block: {}", block.unwrap().header.number);

    let receipt = tx.get_receipt().await?;
    info!("Transaction receipt: {:?}", receipt);

    let claim_data = game.claimData().call().await?;
    assert_eq!(claim_data.status, ProposalStatus::UnchallengedAndValidProofProvided);

    info!("Prove dry-run completed successfully");

    Ok(())
}
