//! This module contains the prologue phase of the client program, pulling in the boot
//! information, which is passed to the zkVM a public inputs to be verified on chain.

use alloy_primitives::B256;
use alloy_sol_types::sol;
use celo_genesis::CeloRollupConfig;
use kona_proof::BootInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ABI encoding of AggregationOutputs is 6 * 32 bytes.
pub const AGGREGATION_OUTPUTS_SIZE: usize = 6 * 32;

/// Hash the serialized rollup config using SHA256. Note: The rollup config is never unrolled
/// on-chain, so switching to a different hash function is not a concern, as long as the config hash
/// is consistent with the one on the contract.
pub fn hash_rollup_config(config: &CeloRollupConfig) -> B256 {
    let serialized_config = serde_json::to_string_pretty(&config.op_rollup_config).unwrap();

    // Create a SHA256 hasher
    let mut hasher = Sha256::new();

    // Hash the serialized config
    hasher.update(serialized_config.as_bytes());

    // Finalize and convert to B256
    let hash = hasher.finalize();
    B256::from_slice(&hash)
}

sol! {
    #[derive(Debug, Serialize, Deserialize)]
    struct BootInfoStruct {
        bytes32 l1Head;
        bytes32 l2PreRoot;
        bytes32 l2PostRoot;
        uint64 l2BlockNumber;
        bytes32 rollupConfigHash;
    }
}

impl From<BootInfo> for BootInfoStruct {
    fn from(boot_info: BootInfo) -> Self {
        // Wrap RollupConfig with CeloRollupConfig
        let celo_rollup_config = CeloRollupConfig::new(boot_info.rollup_config.clone());
        BootInfoStruct {
            l1Head: boot_info.l1_head,
            l2PreRoot: boot_info.agreed_l2_output_root,
            l2PostRoot: boot_info.claimed_l2_output_root,
            l2BlockNumber: boot_info.claimed_l2_block_number,
            rollupConfigHash: hash_rollup_config(&celo_rollup_config),
        }
    }
}

#[cfg(test)]
mod espresso_pin_tests {
    //! Pins the consensus-critical Espresso batch-authentication schedule that celo-kona's
    //! [`CeloBootInfo::load`] resolves for the known Celo chains — the exact `espresso_time` and
    //! `batch_authenticator_address` the espresso-wiring commit stitches onto the rollup config
    //! and carries into derivation.
    //!
    //! celo-kona is currently pinned to a PR head (rev `e715051`, celo-org/celo-kona#242) that must
    //! be re-pinned to a release tag before this repo merges. These tests drive
    //! `CeloBootInfo::load` with a tiny in-memory preimage oracle and assert the Chaos and
    //! Mainnet Espresso values. If a celo-kona re-pin silently changes the Chaos schedule (or
    //! flips Mainnet on), they fail loudly rather than letting a proof derive against a
    //! different, unreviewed schedule.

    use super::*;
    use alloy_primitives::address;
    use async_trait::async_trait;
    use celo_proof::CeloBootInfo;
    use kona_preimage::{
        errors::{PreimageOracleError, PreimageOracleResult},
        PreimageKey, PreimageOracleClient,
    };
    use kona_proof::boot::{
        L1_HEAD_KEY, L2_CHAIN_ID_KEY, L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY, L2_OUTPUT_ROOT_KEY,
    };
    use std::collections::HashMap;

    // Registry-known Celo L2 chain ids.
    const CELO_CHAOS_CHAIN_ID: u64 = 11162320;
    const CELO_MAINNET_CHAIN_ID: u64 = 42220;
    // A claim block comfortably past each chain's genesis L2 block number, so `load`'s BPO
    // timestamp computation (`l2_time + (claim_block - genesis.l2.number) * block_time`) does not
    // underflow (Celo Mainnet genesis L2 number is 31_056_500).
    const SAFE_CLAIM_BLOCK: u64 = 50_000_000;

    /// A minimal in-memory [`PreimageOracleClient`] backed by a map of local preimage keys to their
    /// bytes. It serves only the local keys `CeloBootInfo::load` reads; any other key is a
    /// `KeyNotFound` error, so a re-pin that introduced a new preimage requirement would surface
    /// loudly here rather than silently.
    struct InMemOracle {
        preimages: HashMap<PreimageKey, Vec<u8>>,
    }

    #[async_trait]
    impl PreimageOracleClient for InMemOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            self.preimages.get(&key).cloned().ok_or(PreimageOracleError::KeyNotFound)
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            let value = self.preimages.get(&key).ok_or(PreimageOracleError::KeyNotFound)?;
            if value.len() != buf.len() {
                return Err(PreimageOracleError::BufferLengthMismatch(buf.len(), value.len()));
            }
            buf.copy_from_slice(value);
            Ok(())
        }
    }

    /// Builds an oracle carrying the five local keys `CeloBootInfo::load` reads: the three 32-byte
    /// roots (arbitrary distinct values — they don't affect Espresso resolution) plus the claim
    /// block number and L2 chain id as big-endian u64s.
    fn oracle_for(chain_id: u64, claim_block: u64) -> InMemOracle {
        let mut preimages = HashMap::new();
        preimages.insert(PreimageKey::new_local(L1_HEAD_KEY.to()), vec![0x11u8; 32]);
        preimages.insert(PreimageKey::new_local(L2_OUTPUT_ROOT_KEY.to()), vec![0x22u8; 32]);
        preimages.insert(PreimageKey::new_local(L2_CLAIM_KEY.to()), vec![0x33u8; 32]);
        preimages.insert(
            PreimageKey::new_local(L2_CLAIM_BLOCK_NUMBER_KEY.to()),
            claim_block.to_be_bytes().to_vec(),
        );
        preimages
            .insert(PreimageKey::new_local(L2_CHAIN_ID_KEY.to()), chain_id.to_be_bytes().to_vec());
        InMemOracle { preimages }
    }

    #[tokio::test]
    async fn chaos_boot_enables_espresso_batch_auth() {
        let oracle = oracle_for(CELO_CHAOS_CHAIN_ID, SAFE_CLAIM_BLOCK);
        let boot = CeloBootInfo::load(&oracle).await.expect("Chaos boot info must load");

        let expected_authenticator = address!("b4B5343d9635b05cA4FbdB09BB4929E21A1A8B37");
        assert_eq!(
            boot.espresso.espresso_time,
            Some(1782910800),
            "Chaos Espresso fork timestamp changed — review the celo-kona re-pin",
        );
        assert_eq!(
            boot.espresso.batch_authenticator_address,
            Some(expected_authenticator),
            "Chaos BatchAuthenticator address changed — review the celo-kona re-pin",
        );

        // Mirror the pipeline wiring: the resolved Espresso config is stitched onto the rollup
        // config the derivation pipeline runs with.
        let mut cfg = CeloRollupConfig::new(boot.op_boot_info.rollup_config.clone());
        cfg.espresso = boot.espresso;
        assert!(cfg.is_batch_auth_enabled(), "Chaos must run event-based batch authorization");
        assert_eq!(cfg.batch_auth_params().unwrap(), Some((expected_authenticator, 1782910800)),);
    }

    #[tokio::test]
    async fn mainnet_boot_disables_espresso_batch_auth() {
        let oracle = oracle_for(CELO_MAINNET_CHAIN_ID, SAFE_CLAIM_BLOCK);
        let boot = CeloBootInfo::load(&oracle).await.expect("Mainnet boot info must load");

        assert_eq!(
            boot.espresso.espresso_time, None,
            "Mainnet Espresso must stay unscheduled — review the celo-kona re-pin",
        );
        assert_eq!(
            boot.espresso.batch_authenticator_address, None,
            "Mainnet must carry no BatchAuthenticator — review the celo-kona re-pin",
        );

        let mut cfg = CeloRollupConfig::new(boot.op_boot_info.rollup_config.clone());
        cfg.espresso = boot.espresso;
        assert!(!cfg.is_batch_auth_enabled(), "Mainnet must keep sender-based batch authorization");
    }
}
