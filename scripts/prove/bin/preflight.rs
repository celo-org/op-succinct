use std::{env, fs, path::PathBuf, str::FromStr, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_transport_http::reqwest::Url;
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use fault_proof::contract::{DisputeGameFactory, OPSuccinctFaultDisputeGame, ProposalStatus};
use op_succinct_client_utils::boot::BootInfoStruct;
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    get_agg_proof_stdin,
    host::OPSuccinctHost,
    network::{determine_network_mode, get_network_signer, parse_fulfillment_strategy},
    witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::{get_range_elf_embedded, initialize_host};
use sp1_sdk::{network::FulfillmentStrategy, utils, Prover, ProverClient};
use tracing::info;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The environment file path.
    #[arg(long, default_value = ".env.preflight")]
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

    /// Proposer private key
    pub private_key: String,

    /// Whether to expect NETWORK_PRIVATE_KEY to be an AWS KMS key ARN instead of a
    /// plaintext private key.
    pub use_kms_requester: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            l1_rpc: env::var("L1_RPC")?.parse().expect("L1_RPC not set"),
            factory_address: env::var("FACTORY_ADDRESS")?.parse().expect("FACTORY_ADDRESS not set"),
            range_proof_strategy: parse_fulfillment_strategy(
                env::var("RANGE_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            agg_proof_strategy: parse_fulfillment_strategy(
                env::var("AGG_PROOF_STRATEGY").unwrap_or("reserved".to_string()),
            ),
            private_key: env::var("PRIVATE_KEY")?.parse().expect("PRIVATE_KEY not set"),
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
    let proposer_signer =
        PrivateKeySigner::from_str(&config.private_key).context("Failed to parse private key")?;
    let network_signer = get_network_signer(config.use_kms_requester).await?;
    let network_mode =
        determine_network_mode(config.range_proof_strategy, config.agg_proof_strategy).context(
            "failed to determine network mode from range and agg fulfillment strategies",
        )?;

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    let factory = DisputeGameFactory::new(config.factory_address, data_fetcher.l1_provider.clone());

    let parent_game_address = factory.gameAtIndex(U256::from(args.index - 1)).call().await?.proxy;
    let game_address = factory.gameAtIndex(U256::from(args.index)).call().await?.proxy;

    let parent_game =
        OPSuccinctFaultDisputeGame::new(parent_game_address, data_fetcher.l1_provider.clone());
    let game = OPSuccinctFaultDisputeGame::new(game_address, data_fetcher.l1_provider.clone());

    let l1_head_hash = game.l1Head().call().await?.0;
    let l2_start_block = parent_game.l2BlockNumber().call().await?.to::<u64>();
    let l2_end_block = game.l2BlockNumber().call().await?.to::<u64>();

    let l1_head_block = data_fetcher
        .l1_provider
        .get_block_by_hash(l1_head_hash.into())
        .await?
        .expect("failed to fetch L1 head block");
    let l1_head = l1_head_block.header;

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
        ProverClient::builder().network_for(network_mode).signer(network_signer).build();
    info!("Initialized network prover successfully");

    let (range_pk, _range_vk) = network_prover.setup(get_range_elf_embedded());
    let mut range_proof =
        network_prover.prove(&range_pk, &range_proof_stdin).compressed().run().unwrap();

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

    // 3. Generate the aggregation proof.
    let boot_info: BootInfoStruct = range_proof.public_values.read();
    assert_eq!(boot_info.l1Head, l1_head_hash, "L1 head hash mismatch");

    let (_, range_vk) = network_prover.setup(get_range_elf_embedded());

    let agg_proof_stdin = get_agg_proof_stdin(
        vec![range_proof.proof],
        vec![boot_info.clone()],
        vec![l1_head.clone().into()],
        &range_vk,
        boot_info.l1Head,
        proposer_signer.address(),
    )
    .expect("failed to get agg proof stdin");

    let (agg_pk, _) = network_prover.setup(AGGREGATION_ELF);
    let agg_proof = network_prover.prove(&agg_pk, &agg_proof_stdin).plonk().run().unwrap();

    let agg_proof_dir =
        format!("data/{}/proofs/agg", data_fetcher.get_l2_chain_id().await.unwrap());
    if !std::path::Path::new(&agg_proof_dir).exists() {
        fs::create_dir_all(&agg_proof_dir).unwrap();
    }

    agg_proof.save(format!("{agg_proof_dir}/agg.bin")).expect("saving proof failed");
    info!("Agg proof saved to {agg_proof_dir}/agg.bin");

    // 4. Spin up anvil.
    let l1_head_number =
        data_fetcher.l1_provider.get_block_by_hash(boot_info.l1Head).await?.unwrap().header.number;

    let anvil =
        Anvil::new().fork(config.l1_rpc).fork_block_number(l1_head_number).args(["--no-mining"]);
    let anvil_instance = anvil.spawn();
    let endpoint = anvil_instance.endpoint();
    info!("Anvil chain started forked from L1 block number: {} at: {}", l1_head_number, endpoint);

    // 5. Run the preflight check.
    let provider_with_signer =
        ProviderBuilder::new().wallet(proposer_signer).connect_http(Url::parse(&endpoint)?);
    let client = provider_with_signer.client();

    let game = OPSuccinctFaultDisputeGame::new(game_address, provider_with_signer.clone());

    let game_l1_head = game.l1Head().call().await?;
    info!("Game's L1 head: {:?}", game_l1_head);
    info!("Proof's L1 head (boot_info.l1Head): {:?}", boot_info.l1Head);

    if game_l1_head != boot_info.l1Head {
        return Err(anyhow!(
            "L1 head mismatch! Game expects {:?} but proof contains {:?}",
            game_l1_head,
            boot_info.l1Head
        ));
    }

    let tx = game.prove(agg_proof.bytes().into()).send().await?;

    let _: String = client.request("evm_mine", Vec::<serde_json::Value>::new()).await?;

    let block = provider_with_signer.get_block_by_number(BlockNumberOrTag::Latest).await?;
    info!("Mined block: {}", block.unwrap().header.number);

    let receipt = tx.get_receipt().await?;
    info!("Transaction receipt: {:?}", receipt);

    let claim_data = game.claimData().call().await?;
    assert_eq!(claim_data.status, ProposalStatus::UnchallengedAndValidProofProvided);

    info!("Successfully completed preflight check");

    Ok(())
}
