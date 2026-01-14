use alloy_provider::{Provider, ProviderBuilder};
use anyhow::Result;
use clap::Parser;

const RANGE_SIZE: u64 = 1800;

/// Finds the most active 1800-block range (by transaction count) within a given block range
#[derive(Parser, Debug)]
#[command(about = "Finds the most active 1800-block range by transaction count")]
struct Args {
    /// Starting block number (inclusive)
    #[arg(long)]
    start: u64,

    /// Ending block number (inclusive)
    #[arg(long)]
    end: u64,

    /// RPC endpoint URL
    #[arg(long)]
    rpc_url: String,
}

struct RangeStats {
    start: u64,
    end: u64,
    tx_count: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.end < args.start {
        anyhow::bail!("end block must be >= start block");
    }

    if args.end - args.start < RANGE_SIZE {
        anyhow::bail!(
            "block range must be at least {} blocks, got {}",
            RANGE_SIZE,
            args.end - args.start
        );
    }

    let provider = ProviderBuilder::new().connect_http(args.rpc_url.parse()?);

    let mut all_ranges: Vec<RangeStats> = Vec::new();
    let mut current_start = args.start;

    // Process non-overlapping 1800-block ranges
    while current_start + RANGE_SIZE <= args.end {
        let range_end = current_start + RANGE_SIZE;
        let mut tx_count: u64 = 0;

        // Count transactions in this range
        for block_num in current_start..range_end {
            let block = provider.get_block_by_number(block_num.into()).await?;

            if let Some(block) = block {
                tx_count += block.transactions.len() as u64;
            } else {
                eprintln!("Warning: Block {} not found", block_num);
            }
        }

        eprintln!(
            "Range {}-{}: {} transactions",
            current_start, range_end, tx_count
        );

        all_ranges.push(RangeStats {
            start: current_start,
            end: range_end,
            tx_count,
        });

        current_start = range_end;
    }

    if all_ranges.is_empty() {
        anyhow::bail!("No complete ranges found");
    }

    // Find min and max
    let max_range = all_ranges.iter().max_by_key(|r| r.tx_count).unwrap();
    let min_range = all_ranges.iter().min_by_key(|r| r.tx_count).unwrap();

    println!("\n=== Results ===");
    println!("Total ranges analyzed: {}", all_ranges.len());
    println!(
        "Most active range: {} - {} ({} transactions)",
        max_range.start, max_range.end, max_range.tx_count
    );
    println!(
        "Least active range: {} - {} ({} transactions)",
        min_range.start, min_range.end, min_range.tx_count
    );

    Ok(())
}
