use std::{env, fs, sync::Arc, time::Instant};

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{self, StreamExt, TryStreamExt};
use op_succinct_host_utils::{
    block_range::{get_validated_block_range, RangeSplitCount},
    fetcher::{BlockInfo, OPSuccinctDataFetcher},
    host::OPSuccinctHost,
    network::parse_fulfillment_strategy,
    stats::ExecutionStats,
    witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::{get_range_elf_embedded, initialize_host};
use op_succinct_prove::DEFAULT_RANGE;
use op_succinct_scripts::HostExecutorArgs;
use sp1_sdk::{utils, ExecutionReport, Prover, ProverClient, SP1Stdin};
use tracing::info;

/// Data needed for executing/proving a sub-range.
struct SubRangeData {
    start: u64,
    end: u64,
    sp1_stdin: SP1Stdin,
}

/// Result of executing a sub-range.
struct SubRangeResult {
    block_data: Vec<BlockInfo>,
    report: ExecutionReport,
    execution_duration_secs: u64,
}

/// Execute the OP Succinct program for multiple blocks.
#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider().install_default().unwrap();

    let args = HostExecutorArgs::parse();

    dotenv::from_path(&args.env_file)
        .context(format!("Environment file not found: {}", args.env_file.display()))?;
    utils::setup_logger();

    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    let host = initialize_host(Arc::new(data_fetcher.clone()));

    // If the end block is provided, check that it is less than the latest finalized block. If the
    // end block is not provided, use the latest finalized block.
    let (l2_start_block, l2_end_block) = get_validated_block_range(
        host.as_ref(),
        &data_fetcher,
        args.start,
        args.end,
        DEFAULT_RANGE,
    )
    .await?;

    // Create the split count from the CLI argument.
    let split_count = RangeSplitCount::new(args.split_count).context("Invalid split count")?;

    // Split the range into sub-ranges.
    let ranges =
        split_count.split(l2_start_block, l2_end_block).context("Failed to split range")?;
    let num_ranges = ranges.len();

    // Determine max concurrency.
    let max_concurrent = args.max_concurrent.unwrap_or(num_ranges).min(num_ranges);

    info!(
        "Processing blocks {} to {} split into {} sub-ranges with max {} concurrent operations",
        l2_start_block, l2_end_block, num_ranges, max_concurrent
    );

    // Phase 1: Generate witness data for all sub-ranges concurrently.
    let witness_start_time = Instant::now();

    let witness_tasks = ranges.iter().map(|(start, end)| {
        let host = host.clone();
        let start = *start;
        let end = *end;
        async move {
            info!("Generating witness for sub-range {} to {}", start, end);
            let host_args = host.fetch(start, end, None, args.safe_db_fallback).await?;
            info!("host_args: {:?}", host_args);
            let witness_data = host.run(&host_args).await?;
            info!("got ma witness data");
            let sp1_stdin = host.witness_generator().get_sp1_stdin(witness_data)?;

            let stdin_bytes = bincode::serialize(&sp1_stdin).unwrap();
            info!(
                "Generated SP1 stdin for blocks {} to {}, size: {} bytes",
                start,
                end,
                stdin_bytes.len()
            );

            Ok::<_, anyhow::Error>(SubRangeData { start, end, sp1_stdin })
        }
    });

    let witness_stream = stream::iter(witness_tasks);
    let sub_range_data: Vec<SubRangeData> =
        witness_stream.buffer_unordered(max_concurrent).try_collect().await?;

    let witness_generation_duration = witness_start_time.elapsed();
    info!(
        "Witness generation complete for all {} sub-ranges in {:?}",
        num_ranges, witness_generation_duration
    );

    if args.prove {
        // Phase 2 (Prove Mode): Generate proofs concurrently for all sub-ranges.
        let network_prover = Arc::new(ProverClient::builder().network().build());
        let (pk, _) = network_prover.setup(get_range_elf_embedded());
        let strategy = parse_fulfillment_strategy(env::var("RANGE_PROOF_STRATEGY")?);

        // Create a proof directory for the chain ID if it doesn't exist.
        let proof_dir = format!("data/{}/proofs", data_fetcher.get_l2_chain_id().await.unwrap());
        if !std::path::Path::new(&proof_dir).exists() {
            fs::create_dir_all(&proof_dir).unwrap();
        }

        // Generate proofs concurrently using run_async().
        let prove_tasks = sub_range_data.into_iter().map(|data| {
            let network_prover = network_prover.clone();
            let pk = pk.clone();
            let strategy = strategy.clone();
            let proof_dir = proof_dir.clone();
            async move {
                info!("Generating proof for sub-range {} to {}", data.start, data.end);
                let proof = network_prover
                    .prove(&pk, &data.sp1_stdin)
                    .compressed()
                    .strategy(strategy)
                    .run_async()
                    .await
                    .map_err(|e| anyhow::anyhow!("Proof generation failed: {}", e))?;

                proof
                    .save(format!("{proof_dir}/{}-{}.bin", data.start, data.end))
                    .expect("saving proof failed");
                info!("Saved proof for sub-range {} to {}", data.start, data.end);
                Ok::<_, anyhow::Error>(())
            }
        });

        let prove_stream = stream::iter(prove_tasks);
        prove_stream.buffer_unordered(max_concurrent).try_collect::<Vec<_>>().await?;

        info!("All {} proofs generated and saved successfully", num_ranges);
    } else {
        // Phase 2 (Execute Mode): Execute concurrently for all sub-ranges.
        let execution_start_time = Instant::now();

        let execute_tasks = sub_range_data.into_iter().map(|data| {
            let data_fetcher = data_fetcher.clone();
            async move {
                info!("Executing sub-range {} to {}", data.start, data.end);
                let start_time = Instant::now();
                let prover = ProverClient::builder().mock().build();

                let (_, report) = prover
                    .execute(get_range_elf_embedded(), &data.sp1_stdin)
                    .calculate_gas(true)
                    .deferred_proof_verification(false)
                    .run()
                    .map_err(|e| anyhow::anyhow!("Execution failed: {}", e))?;

                let execution_duration = start_time.elapsed();
                let block_data = data_fetcher.get_l2_block_data_range(data.start, data.end).await?;

                info!(
                    "Execution complete for sub-range {} to {} in {:?}",
                    data.start, data.end, execution_duration
                );

                Ok::<_, anyhow::Error>(SubRangeResult {
                    block_data,
                    report,
                    execution_duration_secs: execution_duration.as_secs(),
                })
            }
        });

        let execute_stream = stream::iter(execute_tasks);
        let results: Vec<SubRangeResult> =
            execute_stream.buffer_unordered(max_concurrent).try_collect().await?;

        let total_execution_duration = execution_start_time.elapsed();

        // Build individual stats for each sub-range.
        let sub_stats: Vec<ExecutionStats> = results
            .iter()
            .map(|r| {
                ExecutionStats::new(
                    0,
                    &r.block_data,
                    &r.report,
                    0, // We'll set this in the merged stats
                    r.execution_duration_secs,
                )
            })
            .collect();

        // Print individual sub-range stats.
        for (i, stats) in sub_stats.iter().enumerate() {
            info!("Sub-range {} stats:\n{}", i + 1, stats);
        }

        // Merge all stats into one.
        let merged_stats = ExecutionStats::merge(
            &sub_stats,
            witness_generation_duration.as_secs(),
            total_execution_duration.as_secs(),
        );

        println!("Merged Execution Stats ({} sub-ranges): \n{}", num_ranges, merged_stats);

        let l2_chain_id = data_fetcher.get_l2_chain_id().await?;

        // Create the report directory if it doesn't exist.
        let report_dir = format!("execution-reports/multi/{l2_chain_id}");
        if !std::path::Path::new(&report_dir).exists() {
            fs::create_dir_all(&report_dir)?;
        }

        let report_path =
            format!("execution-reports/multi/{l2_chain_id}/{l2_start_block}-{l2_end_block}.csv");

        // Write merged stats to CSV.
        let mut csv_writer = csv::Writer::from_path(&report_path)?;
        csv_writer.serialize(&merged_stats)?;
        csv_writer.flush()?;

        info!("Saved merged execution report to {}", report_path);
    }

    Ok(())
}
