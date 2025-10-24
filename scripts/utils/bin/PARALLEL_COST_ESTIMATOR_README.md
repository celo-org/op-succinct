# Parallel Cost Estimator

A utility to run multiple `cost-estimator` instances in parallel for processing large block ranges efficiently.

## Overview

The `parallel-cost-estimator` divides a large block range into smaller chunks and processes them concurrently using multiple instances of the `cost-estimator` binary. This approach significantly speeds up the cost estimation process for large block ranges.

## Features

- **Configurable Concurrency**: Control how many `cost-estimator` instances run simultaneously
- **Automatic Range Splitting**: Automatically divides the block range into manageable chunks
- **Progress Tracking**: Real-time feedback on completed and failed ranges
- **Fault Tolerance**: Continues processing remaining ranges even if some fail
- **Resource Management**: Uses a worker pool pattern to avoid overwhelming system resources

## Usage

### Basic Command

```bash
cargo run --release --bin parallel-cost-estimator -- \
  --from <START_BLOCK> \
  --to <END_BLOCK> \
  --range <BLOCKS_PER_RANGE> \
  --concurrency <NUM_PARALLEL_WORKERS>
```

### Parameters

#### Required Parameters

- `--from <BLOCK_NUMBER>`: Starting block number (inclusive)
- `--to <BLOCK_NUMBER>`: Ending block number (inclusive)
- `--range <SIZE>`: Number of blocks in each processing range

#### Optional Parameters

- `--concurrency <NUM>`: Number of concurrent cost_estimator instances (default: 4)
- `--batch-size <SIZE>`: Blocks per batch within each range (default: 10, passed to cost_estimator)
- `--default-range <SIZE>`: Default range size (default: 5, passed to cost_estimator)
- `--env-file <PATH>`: Environment file path (default: .env)
- `--use-cache`: Enable cached witness generation
- `--rolling`: Use rolling block range
- `--prove`: Generate proofs
- `--safe-db-fallback`: Fallback to timestamp-based L1 head estimation
- `--reverse`: Process ranges in reverse order (highest blocks first)

### Examples

#### Example 1: Process 1000 blocks with default settings

```bash
cargo run --release --bin parallel-cost-estimator -- \
  --from 1000000 \
  --to 1001000 \
  --range 100 \
  --concurrency 4
```

This will:
- Split blocks 1,000,000-1,001,000 into 10 ranges of 100 blocks each
- Process 4 ranges concurrently
- Each range will use batch-size of 10 (default)

#### Example 2: High concurrency with custom batch size

```bash
cargo run --release --bin parallel-cost-estimator -- \
  --from 1000000 \
  --to 1010000 \
  --range 500 \
  --concurrency 8 \
  --batch-size 50
```

This will:
- Split blocks 1,000,000-1,010,000 into 20 ranges of 500 blocks each
- Process 8 ranges concurrently
- Each range processes 50 blocks per batch

#### Example 3: With caching and proof generation

```bash
cargo run --release --bin parallel-cost-estimator -- \
  --from 2000000 \
  --to 2005000 \
  --range 250 \
  --concurrency 6 \
  --use-cache \
  --prove
```

#### Example 4: Process in reverse order (newest blocks first)

```bash
cargo run --release --bin parallel-cost-estimator -- \
  --from 100 \
  --to 300 \
  --range 50 \
  --concurrency 2 \
  --reverse
```

This will:
- Process ranges in reverse: [250-300, 200-249, 150-199, 100-149]
- First 2 concurrent processes: range 250-300 and range 200-249
- Useful for prioritizing recent block data

## How It Works

1. **Range Splitting**: The script divides the `from-to` block range into chunks of `range` size
2. **Worker Pool**: Spawns initial batch of `concurrency` number of cost_estimator processes
3. **Dynamic Scheduling**: As each process completes, a new one is spawned with the next range
4. **Progress Tracking**: Logs progress showing completed vs failed ranges
5. **Completion**: Continues until all ranges are processed or an error occurs

### Reverse Order Processing

When using `--reverse`, ranges are processed from highest to lowest block numbers:

```bash
# Example: --from 100 --to 300 --range 50 --concurrency 2 --reverse

Normal order:  [100-149, 150-199, 200-249, 250-300]
Reverse order: [250-300, 200-249, 150-199, 100-149]

Timeline:
  t0: Process 1: 250-300 (started)
      Process 2: 200-249 (started)
  t1: Process 1: done → Process 1: 150-199 (started)
  t2: Process 2: done → Process 2: 100-149 (started)
  t3: All complete
```

**Note on batch ordering within ranges**: Each `cost_estimator` process internally splits its range into batches and processes them in parallel using thread-level parallelism (rayon). The batches within a single range are executed concurrently, not sequentially, so there's no guaranteed order for batch processing within a range. The `--reverse` flag only affects the order in which ranges are assigned to worker processes.

## Architecture

The parallel cost estimator follows the same pattern as the `execution-verifier` in the celo-kona repository:

```
User Input (from, to, range, concurrency)
    ↓
Range Splitting (e.g., 1000-5000 with range=500 → [1000-1500, 1500-2000, ...])
    ↓
Worker Pool (spawns 'concurrency' number of workers)
    ↓
Dynamic Task Assignment (as workers finish, new tasks are assigned)
    ↓
Aggregated Results
```

## Performance Considerations

### Choosing the Right Parameters

- **concurrency**: 
  - Set based on your system's CPU cores and memory
  - Higher values = faster processing but more resource usage
  - Recommended: 2-8 for most systems

- **range**: 
  - Larger ranges = fewer overhead but longer per-task execution
  - Smaller ranges = better load balancing but more startup overhead
  - Recommended: 100-1000 blocks depending on chain activity

- **batch-size**: 
  - Controls memory usage within each cost_estimator instance
  - Smaller values = lower memory but more iterations
  - Recommended: 10-50 blocks

### Example Configurations

**For Fast, Light Chains:**
```bash
--range 1000 --concurrency 8 --batch-size 100
```

**For Heavy, Complex Chains:**
```bash
--range 100 --concurrency 4 --batch-size 10
```

**For Memory-Constrained Systems:**
```bash
--range 50 --concurrency 2 --batch-size 5
```

## Output

The script will:
1. Log the overall plan (number of ranges, configuration)
2. Show progress as each range completes
3. Display final statistics (completed, failed, total)
4. Generate individual CSV reports in `execution-reports/<chain-id>/` for each range

Each cost_estimator instance produces its own report file:
```
execution-reports/<chain-id>/<start>-<end>-report.csv
```

## Error Handling

- If a range fails, the error is logged but processing continues
- Final summary shows how many ranges succeeded vs failed
- Exit code is non-zero if any ranges failed

## Building

From the op-succinct root directory:

```bash
cargo build --release --bin parallel-cost-estimator
```

## Testing

To verify the script works with a small range:

```bash
cargo run --release --bin parallel-cost-estimator -- \
  --from 100000 \
  --to 100020 \
  --range 5 \
  --concurrency 2
```

This small test will process 4 ranges (100000-100004, 100005-100009, 100010-100014, 100015-100020) with 2 concurrent workers.

## Troubleshooting

### Issue: "cargo: command not found"
**Solution**: Ensure Rust and Cargo are installed and in your PATH

### Issue: Cost estimator processes failing
**Solution**: 
- Check your .env file is properly configured
- Verify RPC endpoints are accessible
- Try reducing concurrency or range size
- Check individual error logs for specific issues

### Issue: Out of memory errors
**Solution**: 
- Reduce `concurrency`
- Reduce `batch-size`
- Reduce `range`

### Issue: Slow processing
**Solution**: 
- Increase `concurrency` (if system has resources)
- Increase `batch-size`
- Increase `range`
- Enable `--use-cache` if running multiple times

## See Also

- Original `cost-estimator`: `scripts/utils/bin/cost_estimator.rs`
- Similar pattern in celo-kona: `bin/execution-verifier/src/main.rs`

