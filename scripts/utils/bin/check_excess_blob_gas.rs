use alloy_provider::{Provider, ProviderBuilder};
use anyhow::Result;
use clap::Parser;

/// Simple script to fetch blockNumber and excessBlobGas for a range of blocks
#[derive(Parser, Debug)]
#[command(about = "Fetches blockNumber and excessBlobGas for a range of blocks")]
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Create provider
    let provider = ProviderBuilder::new().connect_http(args.rpc_url.parse()?);

    // Print CSV header
    println!("blockNumber,excessBlobGas");

    // Iterate through block range
    for block_num in args.start..=args.end {
        // Fetch block data
        let block = provider.get_block_by_number(block_num.into()).await?;

        if let Some(block) = block {
            // Extract excessBlobGas, defaulting to "null" if not present
            let excess_blob_gas = block
                .header
                .excess_blob_gas
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".to_string());

            println!("{},{}", block_num, excess_blob_gas);
        } else {
            eprintln!("Warning: Block {} not found", block_num);
        }
    }

    Ok(())
}
