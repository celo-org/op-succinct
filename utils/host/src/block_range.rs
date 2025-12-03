use std::{
    cmp::{max, min},
    collections::HashSet,
    num::NonZeroU8,
    str::FromStr,
};

use alloy_eips::BlockId;
use anyhow::{bail, Result};
use futures::StreamExt;
use kona_rpc::{OutputResponse, SafeHeadResponse};
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

    // Get all the unique safeHeads between l1_start and l1_head
    let mut ranges = Vec::new();
    let mut current_l2_start = l2_start;
    let safe_heads = futures::stream::iter(l1_start..=l1_head_number)
        .map(|block| async move {
            let l1_block_hex = format!("0x{block:x}");
            let data_fetcher = OPSuccinctDataFetcher::default();
            let result: SafeHeadResponse = data_fetcher
                .fetch_rpc_data_with_mode(
                    RPCMode::L2Node,
                    "optimism_safeHeadAtL1Block",
                    vec![l1_block_hex.into()],
                )
                .await
                .expect("Failed to fetch safe head");
            result.safe_head.number
        })
        .buffered(15)
        .collect::<HashSet<_>>()
        .await;

    // Collect and sort the safe heads.
    let mut safe_heads: Vec<_> = safe_heads.into_iter().collect();
    safe_heads.sort();

    // Loop over all of the safe heads and create ranges.
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

/// How many chunks the range proof input is partitioned into (1-16 inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RangeSplitCount(NonZeroU8);

impl RangeSplitCount {
    pub const MAX: u8 = 16;

    /// Create a new `RangeSplitCount`.
    pub fn new(count: u8) -> Result<Self> {
        if count == 0 || count > Self::MAX {
            bail!("range splits must be between 1 and 16, got {count}");
        }

        let count = NonZeroU8::new(count)
            .ok_or_else(|| anyhow::anyhow!("range splits must be non zero"))?;

        Ok(Self(count))
    }

    /// Returns a `RangeSplitCount` of one.
    pub fn one() -> Self {
        Self(NonZeroU8::new(1).expect("1 is non-zero"))
    }

    /// Convert to `usize`.
    pub fn to_usize(self) -> usize {
        self.0.get() as usize
    }

    /// Split `[start, end)` into up to `count` contiguous, non-empty subranges.
    ///
    /// Behavior:
    /// - Errors if `start > end` or the range is empty.
    /// - Caps the number of produced segments to the number of blocks in the range.
    /// - Uses ceil division to keep segments as even as possible; the final segment takes any
    ///   remainder.
    /// - Always returns ranges that exactly cover `[start, end)` with no gaps or overlaps.
    ///
    /// NOTE: Ceiling division may yield fewer segments than requested when step sizes exhaust the
    /// range early. Example: 9 blocks ÷ 4 → step=3 → 3 segments: [0,3), [3,6), [6,9).
    pub fn split(&self, start: u64, end: u64) -> Result<Vec<(u64, u64)>> {
        let total = end.checked_sub(start).ok_or_else(|| {
            anyhow::anyhow!("end block {end} is not greater than start block {start}")
        })?;
        if total == 0 {
            bail!("start block equals end block ({start}); nothing to prove");
        }

        let splits = self.to_usize();

        if splits == 1 {
            return Ok(vec![(start, end)]);
        }

        // Never split into more parts than there are blocks.
        let segments = splits.min(total as usize);
        let mut ranges = Vec::with_capacity(segments);

        let step = total.div_ceil(segments as u64);

        let mut cur = start;
        for _ in 0..segments {
            if cur >= end {
                break;
            }
            let next = cur.saturating_add(step).min(end);
            ranges.push((cur, next));
            cur = next;
        }

        Ok(ranges)
    }
}

impl TryFrom<u8> for RangeSplitCount {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<u64> for RangeSplitCount {
    type Error = anyhow::Error;

    fn try_from(value: u64) -> Result<Self> {
        let count: u8 = value
            .try_into()
            .map_err(|_| anyhow::anyhow!("range splits must be between 1 and 16, got {value}"))?;
        Self::new(count)
    }
}

impl FromStr for RangeSplitCount {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let value: u64 = s.parse()?;
        Self::try_from(value)
    }
}

#[cfg(test)]
mod split_range_tests {
    use crate::block_range::RangeSplitCount;
    use rstest::rstest;

    fn range_split_count(count: u8) -> RangeSplitCount {
        RangeSplitCount::new(count).expect("valid range split count")
    }

    /// Assert that ranges are non-empty, contiguous, and exactly cover [start, end).
    fn assert_contiguous_cover(ranges: &[(u64, u64)], start: u64, end: u64) {
        assert!(!ranges.is_empty(), "expected at least one range");
        assert_eq!(ranges.first().unwrap().0, start, "first range should start at {start}");
        assert_eq!(ranges.last().unwrap().1, end, "last range should end at {end}");
        for window in ranges.windows(2) {
            let a = window[0];
            let b = window[1];
            assert!(a.0 < a.1, "range must be non-empty: {}-{}", a.0, a.1);
            assert_eq!(a.1, b.0, "ranges must be contiguous: {} != {}", a.1, b.0);
            assert!(b.0 < b.1, "range must be non-empty: {}-{}", b.0, b.1);
        }
    }

    #[rstest]
    #[case::single_split(range_split_count(1), 10, 20, &[(10, 20)])]
    #[case::single_block(range_split_count(2), 0, 1, &[(0, 1)])]
    #[case::no_empty_tail(range_split_count(4), 0, 5, &[(0, 2), (2, 4), (4, 5)])]
    #[case::offset_uneven(range_split_count(4), 5, 14, &[(5, 8), (8, 11), (11, 14)])]
    #[case::even_split(range_split_count(4), 0, 10, &[(0, 3), (3, 6), (6, 9), (9, 10)])]
    #[case::large_splits_small_range(range_split_count(15), 0, 1, &[(0, 1)])]
    #[case::max_splits_exact(
        range_split_count(RangeSplitCount::MAX),
        0,
        16,
        &[
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 4),
            (4, 5),
            (5, 6),
            (6, 7),
            (7, 8),
            (8, 9),
            (9, 10),
            (10, 11),
            (11, 12),
            (12, 13),
            (13, 14),
            (14, 15),
            (15, 16)
        ]
    )]
    #[case::max_splits_caps(range_split_count(RangeSplitCount::MAX), 0, 3, &[(0, 1), (1, 2), (2, 3)])]
    #[case::stop_when_done(
        RangeSplitCount::new(15).unwrap(),
        0,
        16,
        &[(0, 2), (2, 4), (4, 6), (6, 8), (8, 10), (10, 12), (12, 14), (14, 16)]
    )]
    fn test_splits_expected_paths(
        #[case] splits: RangeSplitCount,
        #[case] start: u64,
        #[case] end: u64,
        #[case] expected: &[(u64, u64)],
    ) {
        let ranges = splits.split(start, end).expect("split should succeed");
        assert_eq!(ranges, expected);
        assert_contiguous_cover(&ranges, start, end);
    }

    #[test]
    fn test_splits_extreme() {
        let start = u64::MAX - 100;
        let end = u64::MAX;
        let ranges =
            RangeSplitCount::new(15).unwrap().split(start, end).expect("split should succeed");
        assert_contiguous_cover(&ranges, start, end);
    }

    #[test]
    fn test_errors_on_reversed_bounds() {
        let err = RangeSplitCount::new(2).unwrap().split(8, 3).unwrap_err();
        assert!(
            err.to_string().contains("not greater than start block"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_errors_on_empty_range() {
        let err = RangeSplitCount::new(2).unwrap().split(5, 5).unwrap_err();
        assert!(err.to_string().contains("equals end block"), "unexpected error: {err}");
    }

    #[test]
    fn test_rejects_zero() {
        let err = RangeSplitCount::new(0).unwrap_err();
        assert!(err.to_string().contains("between 1 and 16"), "unexpected error: {err}");
    }

    #[test]
    fn test_rejects_above_max_from_str() {
        let err = "17".parse::<RangeSplitCount>().unwrap_err();
        assert!(err.to_string().contains("between 1 and 16"), "unexpected error: {err}");
    }
}
