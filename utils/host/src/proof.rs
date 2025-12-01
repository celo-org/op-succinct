use crate::{
    fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost,
    witness_generation::traits::WitnessGenerator,
};
use alloy_consensus::Header;
use alloy_primitives::{Address, FixedBytes, B256};
use anyhow::{Context, Result};
use futures::Future;
use op_succinct_client_utils::{boot::BootInfoStruct, types::AggregationInputs};
use sp1_sdk::{HashableKey, SP1Proof, SP1ProofWithPublicValues, SP1Stdin, SP1VerifyingKey};
use std::sync::Arc;

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

pub async fn get_split_range_agg_proof_input<T, F, Fut>(
    host: Arc<T>,
    l1_head_hash: Option<FixedBytes<32>>,
    safe_db_fallback: bool,
    prove_range: &F,
    start_block: u64,
    end_block: u64,
    segments: usize,
    range_vk: &SP1VerifyingKey,
    signer_address: Address,
    fetcher: Arc<OPSuccinctDataFetcher>,
) -> Result<(SP1Stdin, u64, u64)>
where
    T: OPSuccinctHost + Clone + Send + Sync,
    F: Fn(SP1Stdin) -> Fut + Clone + Send + Sync,
    Fut: Future<Output = Result<(SP1ProofWithPublicValues, u64, u64)>> + Send,
{
    let ranges = split_range(segments, start_block, end_block);
    let num_ranges = ranges.len();
    // Pre-allocate and initialize so index writes are safe.
    let mut proofs = vec![None; num_ranges];
    let mut boot_infos = vec![None; num_ranges];

    let mut total_instruction_cycles: u64 = 0;
    let mut total_sp1_gas: u64 = 0;
    use futures::stream::{FuturesUnordered, StreamExt};
    let mut tasks: FuturesUnordered<_> = ranges
        .into_iter()
        .enumerate()
        .map(|(idx, (start, end))| {
            let host = host.clone();
            async move {
                // Propagate errors instead of unwrap().
                let sp1_stdin = get_range_proof_stdin(
                    host.as_ref(),
                    start,
                    end,
                    l1_head_hash,
                    safe_db_fallback,
                )
                .await?;

                let (range_proof, inst_cycles, sp1_gas) = prove_range(sp1_stdin).await?;

                tracing::info!("Preparing Stdin for Agg Proof");

                let proof = range_proof.proof.clone();
                let mut public_values = range_proof.public_values.clone();
                let boot_info: BootInfoStruct = public_values.read();

                Ok::<_, anyhow::Error>((idx, proof, boot_info, inst_cycles, sp1_gas))
            }
        })
        .collect();

    while let Some(result) = tasks.next().await {
        let (idx, proof, boot_info, inst_cycles, sp1_gas) = result?;

        total_instruction_cycles = total_instruction_cycles
            .checked_add(inst_cycles)
            .ok_or_else(|| anyhow::anyhow!("Instruction cycles overflow"))?;

        total_sp1_gas = total_sp1_gas
            .checked_add(sp1_gas)
            .ok_or_else(|| anyhow::anyhow!("SP1 gas overflow"))?;

        proofs[idx] = Some(proof);
        boot_infos[idx] = Some(boot_info);
    }

    let proofs: Vec<_> = proofs.into_iter().map(|p| p.expect("missing proof for range")).collect();

    let boot_infos: Vec<_> =
        boot_infos.into_iter().map(|b| b.expect("missing boot info for range")).collect();

    let latest_l1_head = boot_infos.last().context("No boot infos generated")?.l1Head;

    let headers = match fetcher.get_header_preimages(&boot_infos, latest_l1_head).await {
        Ok(headers) => headers,
        Err(e) => {
            tracing::error!("Failed to get header preimages: {}", e);
            return Err(anyhow::anyhow!("Failed to get header preimages: {}", e));
        }
    };

    let agg_proof_stdin = match get_agg_proof_stdin(
        proofs,
        boot_infos,
        headers,
        range_vk,
        latest_l1_head,
        signer_address,
    ) {
        Ok(stdin) => stdin,
        Err(e) => {
            tracing::error!("Failed to get agg proof stdin: {}", e);
            return Err(anyhow::anyhow!("Failed to get agg proof stdin: {}", e));
        }
    };

    Ok((agg_proof_stdin, total_instruction_cycles, total_sp1_gas))

    // proofs
    //     .into_iter()
    //     .map(|p| p.ok_or_else(|| anyhow::anyhow!("Missing proof for range")))
    //     .collect::<Result<Vec<_>>>()
}

fn split_range(segments: usize, start: u64, end: u64) -> Vec<(u64, u64)> {
    let total = end.saturating_sub(start);
    if segments == 0 || total == 0 {
        return vec![(start, end)];
    }

    let mut ranges = Vec::with_capacity(segments);
    let step = total.div_ceil(segments as u64);

    let mut cur = start;
    for _ in 0..segments {
        if cur >= end {
            break;
        }
        let next = (cur + step).min(end);
        ranges.push((cur, next));
        cur = next;
    }
    ranges
}
