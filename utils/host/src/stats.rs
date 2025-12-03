use std::fmt;

use crate::fetcher::BlockInfo;
use num_format::{Locale, ToFormattedString};
use serde::{Deserialize, Serialize};
use sp1_sdk::ExecutionReport;

/// Statistics for the range execution.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecutionStats {
    pub l1_head: u64,
    pub batch_start: u64,
    pub batch_end: u64,
    /// The wall clock time to generate the witness.
    pub witness_generation_time_sec: u64,
    /// The wall clock time to execute the range on the machine.
    pub total_execution_time_sec: u64,
    pub total_instruction_count: u64,
    pub oracle_verify_instruction_count: u64,
    pub derivation_instruction_count: u64,
    pub block_execution_instruction_count: u64,
    pub blob_verification_instruction_count: u64,
    pub total_sp1_gas: u64,
    pub nb_blocks: u64,
    pub nb_transactions: u64,
    pub eth_gas_used: u64,
    pub l1_fees: u128,
    pub total_tx_fees: u128,
    pub cycles_per_block: u64,
    pub cycles_per_transaction: u64,
    pub transactions_per_block: u64,
    pub gas_used_per_block: u64,
    pub gas_used_per_transaction: u64,
    pub bn_pair_cycles: u64,
    pub bn_add_cycles: u64,
    pub bn_mul_cycles: u64,
    pub kzg_eval_cycles: u64,
    pub ec_recover_cycles: u64,
    pub p256_verify_cycles: u64,
}

/// Write a statistic to the formatter.
fn write_stat(f: &mut fmt::Formatter<'_>, label: &str, value: u64) -> fmt::Result {
    writeln!(f, "| {:<30} | {:>25} |", label, value.to_formatted_string(&Locale::en))
}

impl fmt::Display for ExecutionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "+--------------------------------+---------------------------+")?;
        writeln!(f, "| {:<30} | {:<25} |", "Metric", "Value")?;
        writeln!(f, "+--------------------------------+---------------------------+")?;
        write_stat(f, "Batch Start", self.batch_start)?;
        write_stat(f, "Batch End", self.batch_end)?;
        write_stat(f, "Witness Generation (seconds)", self.witness_generation_time_sec)?;
        write_stat(f, "Execution Duration (seconds)", self.total_execution_time_sec)?;
        write_stat(f, "Total Instruction Count", self.total_instruction_count)?;
        write_stat(f, "Oracle Verify Cycles", self.oracle_verify_instruction_count)?;
        write_stat(f, "Derivation Cycles", self.derivation_instruction_count)?;
        write_stat(f, "Block Execution Cycles", self.block_execution_instruction_count)?;
        write_stat(f, "Blob Verification Cycles", self.blob_verification_instruction_count)?;
        write_stat(f, "Total SP1 Gas", self.total_sp1_gas)?;
        write_stat(f, "Number of Blocks", self.nb_blocks)?;
        write_stat(f, "Number of Transactions", self.nb_transactions)?;
        write_stat(f, "Ethereum Gas Used", self.eth_gas_used)?;
        write_stat(f, "Cycles per Block", self.cycles_per_block)?;
        write_stat(f, "Cycles per Transaction", self.cycles_per_transaction)?;
        write_stat(f, "Transactions per Block", self.transactions_per_block)?;
        write_stat(f, "Gas Used per Block", self.gas_used_per_block)?;
        write_stat(f, "Gas Used per Transaction", self.gas_used_per_transaction)?;
        write_stat(f, "BN Pair Cycles", self.bn_pair_cycles)?;
        write_stat(f, "BN Add Cycles", self.bn_add_cycles)?;
        write_stat(f, "BN Mul Cycles", self.bn_mul_cycles)?;
        write_stat(f, "KZG Eval Cycles", self.kzg_eval_cycles)?;
        write_stat(f, "EC Recover Cycles", self.ec_recover_cycles)?;
        write_stat(f, "P256 Verify Cycles", self.p256_verify_cycles)?;
        writeln!(f, "+--------------------------------+---------------------------+")
    }
}

impl ExecutionStats {
    /// Create a new execution stats.
    pub fn new(
        l1_head: u64,
        block_data: &[BlockInfo],
        report: &ExecutionReport,
        witness_generation_time_sec: u64,
        total_execution_time_sec: u64,
    ) -> Self {
        // Sort the block data by block number.
        let mut block_data = block_data.to_vec();
        block_data.sort_by_key(|b| b.block_number);

        let get_cycles = |key: &str| *report.cycle_tracker.get(key).unwrap_or(&0);

        let nb_blocks = block_data.len() as u64;
        let nb_transactions = block_data.iter().map(|b| b.transaction_count).sum();
        let total_gas_used: u64 = block_data.iter().map(|b| b.gas_used).sum();

        Self {
            l1_head,
            // The "block data" does not include the first block (as it's not executed), so we need
            // to subtract 1 to give the user back the block corresponding to the
            // blockhash they're proving from.
            batch_start: block_data[0].block_number - 1,
            batch_end: block_data[block_data.len() - 1].block_number,
            total_instruction_count: report.total_instruction_count(),
            total_sp1_gas: report.gas.unwrap_or(0),
            block_execution_instruction_count: get_cycles("block-execution"),
            oracle_verify_instruction_count: get_cycles("oracle-verify"),
            derivation_instruction_count: get_cycles("payload-derivation"),
            blob_verification_instruction_count: get_cycles("blob-verification"),
            bn_add_cycles: get_cycles("precompile-bn-add"),
            bn_mul_cycles: get_cycles("precompile-bn-mul"),
            bn_pair_cycles: get_cycles("precompile-bn-pair"),
            kzg_eval_cycles: get_cycles("precompile-kzg-eval"),
            ec_recover_cycles: get_cycles("precompile-ec-recover"),
            p256_verify_cycles: get_cycles("precompile-p256-verify"),
            nb_transactions,
            eth_gas_used: block_data.iter().map(|b| b.gas_used).sum(),
            l1_fees: block_data.iter().map(|b| b.total_l1_fees).sum(),
            total_tx_fees: block_data.iter().map(|b| b.total_tx_fees).sum(),
            nb_blocks,
            cycles_per_block: report.total_instruction_count() / nb_blocks,
            cycles_per_transaction: report.total_instruction_count() / nb_transactions,
            transactions_per_block: nb_transactions / nb_blocks,
            gas_used_per_block: total_gas_used / nb_blocks,
            gas_used_per_transaction: total_gas_used / nb_transactions,
            witness_generation_time_sec,
            total_execution_time_sec,
        }
    }

    /// Merge multiple `ExecutionStats` into one combined stats.
    ///
    /// This is useful when splitting a range into sub-ranges and executing them concurrently.
    /// The merged stats will have:
    /// - `batch_start`: minimum across all stats
    /// - `batch_end`: maximum across all stats
    /// - Additive fields (cycles, gas, transactions, fees): summed
    /// - Derived per-unit fields: recalculated from the merged totals
    /// - Time fields: taken from the provided wall-clock times (not summed from sub-stats)
    pub fn merge(
        stats: &[ExecutionStats],
        witness_generation_time_sec: u64,
        total_execution_time_sec: u64,
    ) -> Self {
        if stats.is_empty() {
            return Self::default();
        }

        if stats.len() == 1 {
            let mut merged = stats[0].clone();
            merged.witness_generation_time_sec = witness_generation_time_sec;
            merged.total_execution_time_sec = total_execution_time_sec;
            return merged;
        }

        // Aggregate additive fields
        let total_instruction_count: u64 = stats.iter().map(|s| s.total_instruction_count).sum();
        let total_sp1_gas: u64 = stats.iter().map(|s| s.total_sp1_gas).sum();
        let oracle_verify_instruction_count: u64 =
            stats.iter().map(|s| s.oracle_verify_instruction_count).sum();
        let derivation_instruction_count: u64 =
            stats.iter().map(|s| s.derivation_instruction_count).sum();
        let block_execution_instruction_count: u64 =
            stats.iter().map(|s| s.block_execution_instruction_count).sum();
        let blob_verification_instruction_count: u64 =
            stats.iter().map(|s| s.blob_verification_instruction_count).sum();
        let nb_blocks: u64 = stats.iter().map(|s| s.nb_blocks).sum();
        let nb_transactions: u64 = stats.iter().map(|s| s.nb_transactions).sum();
        let eth_gas_used: u64 = stats.iter().map(|s| s.eth_gas_used).sum();
        let l1_fees: u128 = stats.iter().map(|s| s.l1_fees).sum();
        let total_tx_fees: u128 = stats.iter().map(|s| s.total_tx_fees).sum();
        let bn_pair_cycles: u64 = stats.iter().map(|s| s.bn_pair_cycles).sum();
        let bn_add_cycles: u64 = stats.iter().map(|s| s.bn_add_cycles).sum();
        let bn_mul_cycles: u64 = stats.iter().map(|s| s.bn_mul_cycles).sum();
        let kzg_eval_cycles: u64 = stats.iter().map(|s| s.kzg_eval_cycles).sum();
        let ec_recover_cycles: u64 = stats.iter().map(|s| s.ec_recover_cycles).sum();
        let p256_verify_cycles: u64 = stats.iter().map(|s| s.p256_verify_cycles).sum();

        // Range fields - take min/max
        let batch_start = stats.iter().map(|s| s.batch_start).min().unwrap_or(0);
        let batch_end = stats.iter().map(|s| s.batch_end).max().unwrap_or(0);
        let l1_head = stats.iter().map(|s| s.l1_head).max().unwrap_or(0);

        // Recalculate derived per-unit fields
        let cycles_per_block = if nb_blocks > 0 { total_instruction_count / nb_blocks } else { 0 };
        let cycles_per_transaction =
            if nb_transactions > 0 { total_instruction_count / nb_transactions } else { 0 };
        let transactions_per_block = if nb_blocks > 0 { nb_transactions / nb_blocks } else { 0 };
        let gas_used_per_block = if nb_blocks > 0 { eth_gas_used / nb_blocks } else { 0 };
        let gas_used_per_transaction =
            if nb_transactions > 0 { eth_gas_used / nb_transactions } else { 0 };

        Self {
            l1_head,
            batch_start,
            batch_end,
            witness_generation_time_sec,
            total_execution_time_sec,
            total_instruction_count,
            oracle_verify_instruction_count,
            derivation_instruction_count,
            block_execution_instruction_count,
            blob_verification_instruction_count,
            total_sp1_gas,
            nb_blocks,
            nb_transactions,
            eth_gas_used,
            l1_fees,
            total_tx_fees,
            cycles_per_block,
            cycles_per_transaction,
            transactions_per_block,
            gas_used_per_block,
            gas_used_per_transaction,
            bn_pair_cycles,
            bn_add_cycles,
            bn_mul_cycles,
            kzg_eval_cycles,
            ec_recover_cycles,
            p256_verify_cycles,
        }
    }
}

/// A [ExecutionStats] that can be displayed as Markdown.
pub struct MarkdownExecutionStats(ExecutionStats);

impl MarkdownExecutionStats {
    /// Creates a [MarkdownExecutionStats].
    pub fn new(inner: ExecutionStats) -> Self {
        Self(inner)
    }
}

impl fmt::Display for MarkdownExecutionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "| {:<30} | {:<25} |", "Metric", "Value")?;
        writeln!(f, "|--------------------------------|---------------------------|")?;
        write_stat(f, "Batch Start", self.0.batch_start)?;
        write_stat(f, "Batch End", self.0.batch_end)?;
        write_stat(f, "Witness Generation (seconds)", self.0.witness_generation_time_sec)?;
        write_stat(f, "Execution Duration (seconds)", self.0.total_execution_time_sec)?;
        write_stat(f, "Total Instruction Count", self.0.total_instruction_count)?;
        write_stat(f, "Oracle Verify Cycles", self.0.oracle_verify_instruction_count)?;
        write_stat(f, "Derivation Cycles", self.0.derivation_instruction_count)?;
        write_stat(f, "Block Execution Cycles", self.0.block_execution_instruction_count)?;
        write_stat(f, "Blob Verification Cycles", self.0.blob_verification_instruction_count)?;
        write_stat(f, "Total SP1 Gas", self.0.total_sp1_gas)?;
        write_stat(f, "Number of Blocks", self.0.nb_blocks)?;
        write_stat(f, "Number of Transactions", self.0.nb_transactions)?;
        write_stat(f, "Ethereum Gas Used", self.0.eth_gas_used)?;
        write_stat(f, "Cycles per Block", self.0.cycles_per_block)?;
        write_stat(f, "Cycles per Transaction", self.0.cycles_per_transaction)?;
        write_stat(f, "Transactions per Block", self.0.transactions_per_block)?;
        write_stat(f, "Gas Used per Block", self.0.gas_used_per_block)?;
        write_stat(f, "Gas Used per Transaction", self.0.gas_used_per_transaction)?;
        write_stat(f, "BN Pair Cycles", self.0.bn_pair_cycles)?;
        write_stat(f, "BN Add Cycles", self.0.bn_add_cycles)?;
        write_stat(f, "BN Mul Cycles", self.0.bn_mul_cycles)?;
        write_stat(f, "KZG Eval Cycles", self.0.kzg_eval_cycles)?;
        write_stat(f, "EC Recover Cycles", self.0.ec_recover_cycles)?;
        write_stat(f, "P256 Verify Cycles", self.0.p256_verify_cycles)
    }
}
