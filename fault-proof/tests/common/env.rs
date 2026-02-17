//! Common test environment setup utilities.
use alloy_primitives::{hex, Address, B256};
use alloy_transport_http::reqwest::Url;
use anyhow::{Context, Result};
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{
    fetcher::{get_rpcs_from_env, OPSuccinctDataFetcher, RPCConfig},
    OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH,
};
use op_succinct_proof_utils::get_range_elf_embedded;
use sp1_sdk::{HashableKey, Prover, ProverClient};
use tracing::info;

use fault_proof::config::FaultDisputeGameConfig;

use crate::common::{constants::*, ANVIL};

use super::{
    anvil::{setup_anvil_chain, AnvilFork},
    contracts::{deploy_test_contracts, DeployedContracts},
};

/// Common test environment setup
pub struct TestEnvironment {
    /// RPC configuration
    pub rpc_config: RPCConfig,
    /// Anvil fork
    pub anvil: AnvilFork,
    /// Deployed contracts
    pub deployed: DeployedContracts,
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        let mut anvil_lock = ANVIL.lock().unwrap();
        if let Some(anvil_instance) = anvil_lock.take() {
            info!("Stopping Anvil instance");
            drop(anvil_instance);
        }
    }
}

/// Compute vkeys from ELF programs.
/// Returns (aggregation_vkey, range_vkey_commitment) as B256 values.
/// Uses the same computation as `proposer.rs` and `config_common.rs`.
pub fn compute_vkeys() -> (B256, B256) {
    let prover = ProverClient::builder().cpu().build();

    let (_, agg_vk) = prover.setup(AGGREGATION_ELF);
    let aggregation_vkey = {
        let hex_str = agg_vk.bytes32();
        B256::from_slice(&hex::decode(hex_str.trim_start_matches("0x")).unwrap())
    };

    let (_, range_vk) = prover.setup(get_range_elf_embedded());
    let range_vkey_commitment = B256::from(u32_to_u8(range_vk.vk.hash_u32()));

    (aggregation_vkey, range_vkey_commitment)
}

/// The test configuration, used for integration tests.
pub fn test_config(
    starting_l2_block_number: u64,
    starting_root: String,
    aggregation_vkey: B256,
    range_vkey_commitment: B256,
    rollup_config_hash: B256,
) -> FaultDisputeGameConfig {
    FaultDisputeGameConfig {
        activate_contracts: false,
        aggregation_vkey: aggregation_vkey.to_string(),
        anchor_state_registry_address: Address::ZERO.to_string(),
        celo_superchain_config_address: Address::ZERO.to_string(),
        challenger_addresses: vec![CHALLENGER_ADDRESS.to_string()],
        challenger_bond_wei: CHALLENGER_BOND.to::<u64>(),
        dispute_game_finality_delay_seconds: DISPUTE_GAME_FINALITY_DELAY_SECONDS,
        fallback_timeout_fp_secs: FALLBACK_TIMEOUT.to::<u64>(),
        game_type: TEST_GAME_TYPE,
        initial_bond_wei: INIT_BOND.to::<u64>(),
        configure_contracts: true,
        dispute_game_factory_address: Address::ZERO.to_string(),
        max_challenge_duration: MAX_CHALLENGE_DURATION,
        max_prove_duration: MAX_PROVE_DURATION,
        optimism_portal2_address: Address::ZERO.to_string(),
        permissionless_mode: false,
        proposer_addresses: vec![PROPOSER_ADDRESS.to_string()],
        range_vkey_commitment: range_vkey_commitment.to_string(),
        rollup_config_hash: rollup_config_hash.to_string(),
        starting_l2_block_number,
        starting_root,
        use_sp1_mock_verifier: true,
        verifier_address: Address::ZERO.to_string(),
    }
}

impl TestEnvironment {
    /// Initialize logging for tests
    pub fn init_logging() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();
    }

    /// Create a new test environment with common setup
    pub async fn setup() -> Result<Self> {
        // Compute vkeys from ELFs - these will match the proposer's computed vkeys
        let (aggregation_vkey, range_vkey_commitment) = compute_vkeys();

        // Get environment variables
        let mut rpc_config = get_rpcs_from_env();

        let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

        // Compute rollup_config_hash from fetcher's chain config - matches proposer's computation
        let rollup_config_hash = hash_rollup_config(
            fetcher.rollup_config.as_ref().context("rollup_config required for test setup")?,
        );

        // Setup fresh Anvil chain
        let anvil = setup_anvil_chain().await?;

        // Put the test config into ../contracts/opsuccinctfdgconfig.json
        let test_config: FaultDisputeGameConfig = test_config(
            anvil.starting_l2_block_number,
            anvil.starting_root.clone(),
            aggregation_vkey,
            range_vkey_commitment,
            rollup_config_hash,
        );
        let json = serde_json::to_string_pretty(&test_config)?;
        std::fs::write(OP_SUCCINCT_FAULT_DISPUTE_GAME_CONFIG_PATH.clone(), json)?;

        // Update RPC config with Anvil endpoint
        rpc_config.l1_rpc = Url::parse(&anvil.endpoint.clone())?;

        // Deploy contracts
        info!("=== Deploying Contracts ===");
        let deployed = deploy_test_contracts(&anvil.endpoint, DEPLOYER_PRIVATE_KEY).await?;

        Ok(Self { rpc_config, anvil, deployed })
    }
}
