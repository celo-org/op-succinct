use op_succinct_host_utils::stats::ExecutionStats;
use std::cmp::{max, min};

/// Aggregate per-range `ExecutionStats` into a single summary stat.
/// Lifted from `scripts/utils/bin/cost_estimator.rs:196-251` (no CSV round-trip).
pub fn aggregate_execution_stats(
    execution_stats: &[ExecutionStats],
    total_execution_time_sec: u64,
    witness_generation_time_sec: u64,
) -> ExecutionStats {
    let mut aggregate = ExecutionStats::default();
    let mut batch_start = u64::MAX;
    let mut batch_end = 0u64;
    for stats in execution_stats {
        batch_start = min(batch_start, stats.batch_start);
        batch_end = max(batch_end, stats.batch_end);

        aggregate.total_instruction_count += stats.total_instruction_count;
        aggregate.oracle_verify_instruction_count += stats.oracle_verify_instruction_count;
        aggregate.derivation_instruction_count += stats.derivation_instruction_count;
        aggregate.block_execution_instruction_count += stats.block_execution_instruction_count;
        aggregate.blob_verification_instruction_count += stats.blob_verification_instruction_count;
        aggregate.total_sp1_gas += stats.total_sp1_gas;
        aggregate.nb_blocks += stats.nb_blocks;
        aggregate.nb_transactions += stats.nb_transactions;
        aggregate.eth_gas_used += stats.eth_gas_used;
        aggregate.l1_fees += stats.l1_fees;
        aggregate.total_tx_fees += stats.total_tx_fees;
        aggregate.bn_pair_cycles += stats.bn_pair_cycles;
        aggregate.bn_add_cycles += stats.bn_add_cycles;
        aggregate.bn_mul_cycles += stats.bn_mul_cycles;
        aggregate.kzg_eval_cycles += stats.kzg_eval_cycles;
        aggregate.ec_recover_cycles += stats.ec_recover_cycles;
        aggregate.p256_verify_cycles += stats.p256_verify_cycles;
    }

    // Safe per-unit averages.
    let nb_blocks = aggregate.nb_blocks.max(1);
    let nb_txs = aggregate.nb_transactions.max(1);
    aggregate.cycles_per_block = aggregate.total_instruction_count / nb_blocks;
    aggregate.cycles_per_transaction = aggregate.total_instruction_count / nb_txs;
    aggregate.transactions_per_block = aggregate.nb_transactions / nb_blocks;
    aggregate.gas_used_per_block = aggregate.eth_gas_used / nb_blocks;
    aggregate.gas_used_per_transaction = aggregate.eth_gas_used / nb_txs;

    aggregate.batch_start = if batch_start == u64::MAX { 0 } else { batch_start };
    aggregate.batch_end = batch_end;
    aggregate.total_execution_time_sec = total_execution_time_sec;
    aggregate.witness_generation_time_sec = witness_generation_time_sec;
    aggregate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(start: u64, end: u64, instrs: u64, blocks: u64, txs: u64) -> ExecutionStats {
        ExecutionStats {
            batch_start: start,
            batch_end: end,
            total_instruction_count: instrs,
            nb_blocks: blocks,
            nb_transactions: txs,
            ..Default::default()
        }
    }

    #[test]
    fn aggregates_range_and_sums() {
        let a = stat(100, 110, 1000, 10, 20);
        let b = stat(110, 120, 2000, 10, 30);
        let agg = aggregate_execution_stats(&[a, b], 42, 7);
        assert_eq!(agg.batch_start, 100);
        assert_eq!(agg.batch_end, 120);
        assert_eq!(agg.total_instruction_count, 3000);
        assert_eq!(agg.nb_blocks, 20);
        assert_eq!(agg.nb_transactions, 50);
        assert_eq!(agg.cycles_per_block, 150);
        assert_eq!(agg.total_execution_time_sec, 42);
        assert_eq!(agg.witness_generation_time_sec, 7);
    }

    #[test]
    fn empty_input_does_not_divide_by_zero() {
        let agg = aggregate_execution_stats(&[], 0, 0);
        assert_eq!(agg.batch_start, 0);
        assert_eq!(agg.cycles_per_block, 0);
    }
}
