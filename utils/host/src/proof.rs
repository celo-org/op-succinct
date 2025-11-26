use crate::{host::OPSuccinctHost, witness_generation::traits::WitnessGenerator};
use alloy_consensus::Header;
use alloy_primitives::{Address, FixedBytes, B256};
use anyhow::{Context, Result};
use op_succinct_client_utils::{boot::BootInfoStruct, types::AggregationInputs};
use sp1_sdk::{HashableKey, SP1Proof, SP1Stdin};

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

/// Fetches witness data and generates SP1 stdin for a given block range.
///
/// # Arguments
/// * `host` - The host instance to use for fetching and generating witness data
/// * `start_block` - Starting block number
/// * `end_block` - Ending block number
/// * `l1_head_hash` - L1 head hash for context
/// * `safe_db_fallback` - Whether to use safe DB fallback
///
/// # Returns
/// SP1Stdin containing the witness data for the range proof
pub async fn get_range_proof_stdin<T: OPSuccinctHost>(
    host: &T,
    start_block: u64,
    end_block: u64,
    l1_head_hash: Option<FixedBytes<32>>,
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
