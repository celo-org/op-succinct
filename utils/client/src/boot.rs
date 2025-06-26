//! This module contains the prologue phase of the client program, pulling in the boot
//! information, which is passed to the zkVM a public inputs to be verified on chain.

use alloy_primitives::B256;
use alloy_sol_types::sol;
use celo_genesis::{CeloHardForkConfig, CeloRollupConfig};
use kona_proof::BootInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ABI encoding of AggregationOutputs is 6 * 32 bytes.
pub const AGGREGATION_OUTPUTS_SIZE: usize = 6 * 32;

/// Hash the serialized rollup config using SHA256. Note: The rollup config is never unrolled
/// on-chain, so switching to a different hash function is not a concern, as long as the config hash
/// is consistent with the one on the contract.
pub fn hash_rollup_config(config: &CeloRollupConfig) -> B256 {
    let serialized_config = {
        // Manually construct the JSON to match the RPC response format
        let full_config = serde_json::json!({
            "genesis": {
                "l1": {
                    "hash": format!("0x{:x}", config.op_rollup_config.genesis.l1.hash),
                    "number": config.op_rollup_config.genesis.l1.number,
                },
                "l2": {
                    "hash": format!("0x{:x}", config.op_rollup_config.genesis.l2.hash),
                    "number": config.op_rollup_config.genesis.l2.number,
                },
                "l2_time": config.op_rollup_config.genesis.l2_time,
                "system_config": config.op_rollup_config.genesis.system_config.as_ref().map(|sc| {
                    serde_json::json!({
                        "batcherAddr": format!("0x{:x}", sc.batcher_address),
                        "overhead": format!("0x{:064x}", sc.overhead),
                        "scalar": format!("0x{:064x}", sc.scalar),
                        "gasLimit": sc.gas_limit,
                        "eip1559Params": format!("0x{:016x}",
                            (sc.eip1559_denominator.unwrap_or(0) as u64) |
                            ((sc.eip1559_elasticity.unwrap_or(0) as u64) << 8)
                        ),
                        "operatorFeeParams": format!("0x{:064x}",
                            (sc.operator_fee_scalar.unwrap_or(0) as u128) |
                            ((sc.operator_fee_constant.unwrap_or(0) as u128) << 64)
                        ),
                    })
                }),
            },
            "block_time": config.op_rollup_config.block_time,
            "max_sequencer_drift": config.op_rollup_config.max_sequencer_drift,
            "seq_window_size": config.op_rollup_config.seq_window_size,
            "channel_timeout": config.op_rollup_config.channel_timeout,
            "l1_chain_id": config.op_rollup_config.l1_chain_id,
            "l2_chain_id": config.op_rollup_config.l2_chain_id,
            "regolith_time": config.op_rollup_config.hardforks.regolith_time.unwrap_or(0),
            // "cel2_time": config.hardforks.cel2_time.unwrap_or(0),
            "canyon_time": config.op_rollup_config.hardforks.canyon_time.unwrap_or(0),
            "delta_time": config.op_rollup_config.hardforks.delta_time.unwrap_or(0),
            "ecotone_time": config.op_rollup_config.hardforks.ecotone_time.unwrap_or(0),
            "fjord_time": config.op_rollup_config.hardforks.fjord_time.unwrap_or(0),
            "granite_time": config.op_rollup_config.hardforks.granite_time.unwrap_or(0),
            "holocene_time": config.op_rollup_config.hardforks.holocene_time.unwrap_or(0),
            "isthmus_time": config.op_rollup_config.hardforks.isthmus_time.unwrap_or(0),
            "batch_inbox_address": format!("0x{:x}", config.op_rollup_config.batch_inbox_address),
            "deposit_contract_address": format!("0x{:x}", config.op_rollup_config.deposit_contract_address),
            "l1_system_config_address": format!("0x{:x}", config.op_rollup_config.l1_system_config_address),
            "protocol_versions_address": format!("0x{:x}", config.op_rollup_config.protocol_versions_address),
            "chain_op_config": {
                "eip1559Elasticity": config.op_rollup_config.chain_op_config.eip1559_elasticity,
                "eip1559Denominator": config.op_rollup_config.chain_op_config.eip1559_denominator,
                "eip1559DenominatorCanyon": config.op_rollup_config.chain_op_config.eip1559_denominator_canyon,
            },
            "alt_da": config.op_rollup_config.alt_da_config.as_ref().map(|alt_da| {
                serde_json::json!({
                    "da_challenge_contract_address": alt_da.da_challenge_address.map(|addr| format!("0x{addr:x}")),
                    "da_commitment_type": alt_da.da_commitment_type.as_deref(),
                    "da_challenge_window": alt_da.da_challenge_window,
                    "da_resolve_window": alt_da.da_resolve_window,
                })
            }),
        });
        serde_json::to_string_pretty(&full_config).unwrap()
    };
    // let serialized_config = serde_json::to_string_pretty(config).unwrap();

    // Create a SHA256 hasher
    let mut hasher = Sha256::new();

    // Hash the serialized config
    hasher.update(serialized_config.as_bytes());

    // Finalize and convert to B256
    let hash = hasher.finalize();
    B256::from_slice(hash.as_slice())
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
        let celo_rollup_config = CeloRollupConfig {
            op_rollup_config: boot_info.rollup_config.clone(),
            hardforks: CeloHardForkConfig {
                op_hardfork_config: boot_info.rollup_config.hardforks,
                cel2_time: Some(0),
            },
        };
        BootInfoStruct {
            l1Head: boot_info.l1_head,
            l2PreRoot: boot_info.agreed_l2_output_root,
            l2PostRoot: boot_info.claimed_l2_output_root,
            l2BlockNumber: boot_info.claimed_l2_block_number,
            rollupConfigHash: hash_rollup_config(&celo_rollup_config),
        }
    }
}
