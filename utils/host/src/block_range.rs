use std::{
    cmp::{max, min},
    collections::HashMap,
};

use crate::rpc_types::{OutputResponse, SafeHeadResponse};
use alloy_eips::BlockId;
use anyhow::{bail, Result};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::{
    fetcher::{OPSuccinctDataFetcher, RPCMode},
    host::OPSuccinctHost,
};

const TWO_HOURS_IN_BLOCKS: u64 = 3600;

/// Get the start and end block numbers for a range, with validation.
pub async fn get_validated_block_range<H: OPSuccinctHost>(
    host: &H,
    data_fetcher: &OPSuccinctDataFetcher,
    start: Option<u64>,
    end: Option<u64>,
    default_range: u64,
) -> Result<(u64, u64)> {
    // Get the latest finalized block number when end block is not provided.
    // Even though the safeDB is activated, we use the finalized block number as the
    // end block by default to ensure the program doesn't run into L2 Block Validation
    // Failure error.
    // L2 Block Validation Failure error might still occur. See
    // [Troubleshooting](../troubleshooting.md#l2-block-validation-failure) for more details.
    let l2_finalized_block_number = data_fetcher.get_l2_header(BlockId::finalized()).await?.number;
    let end_number = host
        .get_finalized_l2_block_number(
            data_fetcher,
            l2_finalized_block_number - TWO_HOURS_IN_BLOCKS,
        )
        .await?
        .expect("Failed to get finalized L2 block number");

    // If end block not provided, use latest finalized block
    let l2_end_block = match end {
        Some(end) => {
            if end > end_number {
                bail!(
                    "The end block ({}) is greater than the latest finalized block ({})",
                    end,
                    end_number
                );
            }
            end
        }
        None => end_number,
    };

    // If start block not provided, use end block - default_range
    let l2_start_block = match start {
        Some(start) => start,
        None => max(1, l2_end_block.saturating_sub(default_range)),
    };

    if l2_start_block >= l2_end_block {
        bail!("Start block ({}) must be less than end block ({})", l2_start_block, l2_end_block);
    }

    Ok((l2_start_block, l2_end_block))
}

/// Get a rolling block range whose end aligns with the host's finalized L2 block.
///
/// The returned tuple represents the last `range` blocks that the host considers finalized
/// according to its DA-specific logic, making the range safe to use for proof generation.
pub async fn get_rolling_block_range<H: OPSuccinctHost>(
    host: &H,
    data_fetcher: &OPSuccinctDataFetcher,
    range: u64,
) -> Result<(u64, u64)> {
    let header = data_fetcher.get_l2_header(BlockId::finalized()).await?;
    let l2_end_block = host
        .get_finalized_l2_block_number(data_fetcher, header.number - TWO_HOURS_IN_BLOCKS)
        .await?
        .expect("Failed to get finalized L2 block number");

    Ok((l2_end_block - range, l2_end_block))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanBatchRange {
    pub start: u64,
    pub end: u64,
}

/// Split a range of blocks into a list of span batch ranges.
///
/// This is a simple implementation used when the safeDB is not activated on the L2 Node.
pub fn split_range_basic(start: u64, end: u64, max_range_size: u64) -> Vec<SpanBatchRange> {
    let mut ranges = Vec::new();
    let mut current_start = start;

    while current_start < end {
        let current_end = min(current_start + max_range_size, end);
        ranges.push(SpanBatchRange { start: current_start, end: current_end });
        current_start = current_end;
    }

    ranges
}

/// Split a range of blocks into a list of span batch ranges based on L2 safeHeads.
///
/// 1. Get the L1 block range [L1 origin of l2_start, L1Head] where L1Head is the block from which
///    l2_end can be derived
/// 2. Loop over L1 blocks to get safeHead increases (batch posts) which form a step function
/// 3. Split ranges based on safeHead increases and max batch size
///
/// Example: If safeHeads are [27,49,90] and max_size=30, ranges will be [(0,27), (27,49), (49,69),
/// (69,90)]
pub async fn split_range_based_on_safe_heads(
    l2_start: u64,
    l2_end: u64,
    max_range_size: u64,
) -> Result<Vec<SpanBatchRange>> {
    let data_fetcher = OPSuccinctDataFetcher::default();
    split_range_based_on_safe_heads_with_fetcher(&data_fetcher, l2_start, l2_end, max_range_size)
        .await
}

/// Additive variant: reuse a caller-owned fetcher (no per-call `default()` builds).
pub async fn split_range_based_on_safe_heads_with_fetcher(
    data_fetcher: &OPSuccinctDataFetcher,
    l2_start: u64,
    l2_end: u64,
    max_range_size: u64,
) -> Result<Vec<SpanBatchRange>> {
    let mut safe_head_cache = HashMap::new();
    split_range_based_on_safe_heads_memoized(
        data_fetcher,
        l2_start,
        l2_end,
        max_range_size,
        &mut safe_head_cache,
    )
    .await
}

/// Like [`split_range_based_on_safe_heads_with_fetcher`], but memoizes
/// `optimism_safeHeadAtL1Block` lookups in `safe_head_cache` (keyed by L1 block number).
///
/// The predictive pipeline re-splits a growing `[window.start, finalized]` prefix every tick
/// as the finalized head advances; without memoization each tick re-queries every L1 block in
/// the window. Safe-head values are immutable for finalized L1 blocks, so the cache is sound
/// as long as the caller only splits up to the finalized head (the pipeline does). Each tick
/// then queries only the L1 blocks newly finalized since the previous call.
pub async fn split_range_based_on_safe_heads_memoized(
    data_fetcher: &OPSuccinctDataFetcher,
    l2_start: u64,
    l2_end: u64,
    max_range_size: u64,
    safe_head_cache: &mut HashMap<u64, u64>,
) -> Result<Vec<SpanBatchRange>> {
    // Get the L1 origin of l2_start
    let l2_start_hex = format!("0x{l2_start:x}");
    let start_output: OutputResponse = data_fetcher
        .fetch_rpc_data_with_mode(
            RPCMode::L2Node,
            "optimism_outputAtBlock",
            vec![l2_start_hex.into()],
        )
        .await?;
    let l1_start = start_output.block_ref.l1_origin.number;

    // Get the L1Head from which l2_end can be derived
    let (_, l1_head_number) = data_fetcher.get_safe_l1_block_for_l2_block(l2_end).await?;

    // Query only the L1 blocks not already cached (safe heads are immutable for finalized
    // blocks). Propagate a transient safe-head RPC failure as an error instead of panicking —
    // this runs inline in the embedded daemon, where a panic would crash the whole process.
    let missing: Vec<u64> =
        (l1_start..=l1_head_number).filter(|b| !safe_head_cache.contains_key(b)).collect();
    let fetched: Vec<(u64, u64)> = futures::stream::iter(missing)
        .map(|block| async move {
            let l1_block_hex = format!("0x{block:x}");
            let result: SafeHeadResponse = data_fetcher
                .fetch_rpc_data_with_mode(
                    RPCMode::L2Node,
                    "optimism_safeHeadAtL1Block",
                    vec![l1_block_hex.into()],
                )
                .await?;
            Ok::<(u64, u64), anyhow::Error>((block, result.safe_head.number))
        })
        .buffered(15)
        .try_collect()
        .await?;
    safe_head_cache.extend(fetched);

    // Collect and sort the unique safe heads across [l1_start, l1_head_number] from the cache.
    let mut safe_heads: Vec<u64> =
        (l1_start..=l1_head_number).filter_map(|b| safe_head_cache.get(&b).copied()).collect();
    safe_heads.sort();
    safe_heads.dedup();

    // Loop over all of the safe heads and create ranges.
    let mut ranges = Vec::new();
    let mut current_l2_start = l2_start;
    for safe_head in safe_heads {
        if safe_head > current_l2_start && current_l2_start < l2_end {
            let mut range_start = current_l2_start;
            while range_start + max_range_size < min(l2_end, safe_head) {
                ranges
                    .push(SpanBatchRange { start: range_start, end: range_start + max_range_size });
                range_start += max_range_size;
            }
            ranges.push(SpanBatchRange { start: range_start, end: min(l2_end, safe_head) });
            current_l2_start = safe_head;
        }
    }

    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_split_respects_max_range_and_covers_window() {
        let ranges = split_range_basic(100, 250, 60);
        assert_eq!(ranges.first().unwrap().start, 100);
        assert_eq!(ranges.last().unwrap().end, 250);
        for w in &ranges {
            assert!(w.end - w.start <= 60);
        }
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }
}
