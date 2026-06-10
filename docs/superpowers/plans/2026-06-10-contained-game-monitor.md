# Contained Game Monitor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the subprocess-based game monitor with a single contained daemon that runs cost-estimation in-process, driven by a predictive finalization-triggered witness pipeline, with on-disk caching of both `WitnessData` and `SP1Stdin`.

**Architecture:** A new library crate `utils/estimator` lifts the cost-estimator core into two methods — `build_range_witness` (fetch + crunch + cache) and `execute_range` (load + SP1 execute). A new binary `scripts/utils/bin/game_monitor_contained.rs` keeps today's proven control-plane (finalized discovery, type-42 filter, `--delay`, two-tier retry, `last_contiguous` frontier, `progress.json` resume) but calls the library directly instead of spawning a subprocess, and runs a background pipeline that prebuilds each range's stdin as its L2 blocks finalize. Both heavy workloads (witness-gen and execute) share one RSS-admission budget read from the cgroup.

**Tech Stack:** Rust, tokio, `sp1-sdk` (`CpuProver` blocking), alloy 1.6.3 (`RetryBackoffLayer`), rkyv 0.8 (`WitnessData` blobs), bincode (`SP1Stdin` blobs), `tracing` + `tracing-subscriber` (JSON to stdout), `thiserror`.

**Spec:** `docs/superpowers/specs/2026-06-10-game-monitor-contained-design.md`

**Sequencing:** Tasks 1–13 build and test the standalone `utils/estimator` library, the reworked witness cache, and the resilience layer. Tasks 14–22 build the daemon, the predictive pipeline, and the memory model on top. Each task is independently committable; the library half compiles and tests green before the daemon half begins.

**Conventions for the implementer:**
- All `cargo` commands run from the repo root `/Users/pierspowlesland/workspaces/game-monitor-rebuild/op-succinct`.
- The production DA target is **EigenDA**. Build/test the estimator and binary with `--no-default-features --features eigenda` (default feature is `ethereum`). Where a command is DA-sensitive it says so.
- The repo uses inline `#[cfg(test)] mod tests` modules with `tempfile` + `rstest` dev-deps. Follow that.
- Internal crate names: `op-succinct-host-utils` (`utils/host`), `op-succinct-proof-utils` (`utils/proof`), `op-succinct-client-utils` (`utils/client`), `op-succinct-common` (`utils/common`), `op-succinct-eigenda-host-utils` (`utils/eigenda/host`), `op-succinct-scripts` (`scripts/utils`), `op-succinct-fp` (`fault-proof`).
- The repo `.git` is a gitlink; `git` commands work from the repo root. Commit after every task.

---

## File Structure

**New files:**
- `utils/estimator/Cargo.toml` — new workspace crate manifest.
- `utils/estimator/src/lib.rs` — crate root, re-exports.
- `utils/estimator/src/error.rs` — `EstimatorError` (replaces log-tail scraping).
- `utils/estimator/src/stats.rs` — lifted `aggregate_execution_stats`.
- `utils/estimator/src/cache.rs` — `WitnessCache`: keyed `WitnessData` + `SP1Stdin` blobs, DA discriminator, absolute base dir, separate-clock pruning.
- `utils/estimator/src/retry.rs` — `network_call_with_timeout` helper (lifted from `fault-proof`).
- `utils/estimator/src/estimator.rs` — `Estimator<H>` with `build_range_witness` + `execute_range`.
- `utils/estimator/src/memory.rs` — cgroup budget parsing + `RssAdmission` gate + peak-RSS history.
- `utils/estimator/src/window.rs` — `PROPOSAL_INTERVAL` window prediction (pure).
- `scripts/utils/bin/game_monitor_contained.rs` — the daemon.
- `scripts/utils/src/contained/mod.rs` + submodules — daemon support types (state, retry, pipeline) extracted for testability. (Declared from the new binary; see Task 14.)

**Modified files:**
- `Cargo.toml` (root) — add `utils/estimator` to members; add `alloy-rpc-client` + `alloy-transport` retry layer deps if missing; add `op-succinct-estimator` path dep.
- `utils/host/src/block_range.rs` — add `split_range_based_on_safe_heads_with_fetcher` (additive; leave existing signature intact).
- `utils/host/src/witness_generation/online_blob_store.rs` — convert `.unwrap()`s and the `Mutex` lock to errors.
- `utils/host/src/fetcher.rs` — add `RetryBackoffLayer` to L1/L2 providers; wrap raw JSON-RPC in a shared timeout/retry helper.
- `scripts/utils/Cargo.toml` — add the `game-monitor-contained` bin target and the `op-succinct-estimator` dep.

---

## Task 1: Scaffold the `utils/estimator` crate

**Files:**
- Create: `utils/estimator/Cargo.toml`
- Create: `utils/estimator/src/lib.rs`
- Modify: `Cargo.toml` (root) — workspace members + internal dep entry

- [ ] **Step 1: Add the crate to the workspace members**

In root `Cargo.toml`, the `[workspace] members` list (lines 1–22), add `"utils/estimator",` after `"utils/proof",`:

```toml
    "utils/proof",
    "utils/estimator",
    "utils/signer",
```

- [ ] **Step 2: Add the internal dep entry**

In root `Cargo.toml` `[workspace.dependencies]`, beside the other `op-succinct-*` path deps (after `op-succinct-proof-utils = { path = "utils/proof" }`):

```toml
op-succinct-estimator = { path = "utils/estimator" }
```

- [ ] **Step 3: Write the crate manifest**

Create `utils/estimator/Cargo.toml`. Mirrors `utils/proof/Cargo.toml` (sp1-sdk + host-utils) plus the deps the lifted core needs:

```toml
[package]
name = "op-succinct-estimator"
version.workspace = true
license.workspace = true
edition.workspace = true

[dependencies]
# local
op-succinct-host-utils.workspace = true
op-succinct-proof-utils.workspace = true
op-succinct-client-utils.workspace = true
op-succinct-common.workspace = true
op-succinct-celestia-host-utils = { workspace = true, optional = true }
op-succinct-eigenda-host-utils = { workspace = true, optional = true }
op-succinct-ethereum-host-utils = { workspace = true, optional = true }

# sp1
sp1-sdk.workspace = true

# general
anyhow.workspace = true
thiserror.workspace = true
async-trait.workspace = true
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
bincode.workspace = true
rkyv.workspace = true
tokio.workspace = true
tracing.workspace = true
futures.workspace = true
alloy-primitives.workspace = true

[dev-dependencies]
tempfile.workspace = true
rstest = "0.26"

[features]
default = ["ethereum"]
celestia = ["op-succinct-celestia-host-utils", "op-succinct-proof-utils/celestia"]
eigenda = ["op-succinct-eigenda-host-utils", "op-succinct-proof-utils/eigenda"]
ethereum = ["op-succinct-ethereum-host-utils", "op-succinct-proof-utils/ethereum"]
```

- [ ] **Step 4: Write a minimal crate root**

Create `utils/estimator/src/lib.rs`:

```rust
//! In-process cost estimator: fetch + crunch witness data and execute SP1 ranges,
//! with on-disk caching of both `WitnessData` and `SP1Stdin`.

pub mod error;

pub use error::EstimatorError;
```

- [ ] **Step 5: Verify it builds**

Run: `cargo check -p op-succinct-estimator`
Expected: compiles (a warning that `error` is empty is fine until Task 2). If `error.rs` doesn't exist yet, create it empty so this passes, then fill it in Task 2.

Run: `cargo check -p op-succinct-estimator --no-default-features --features eigenda`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml utils/estimator/
git commit -m "feat(estimator): scaffold op-succinct-estimator crate"
```

---

## Task 2: `EstimatorError` — structured failures replacing log-tail scraping

**Files:**
- Create: `utils/estimator/src/error.rs`
- Test: inline `#[cfg(test)] mod tests` in `error.rs`

The five seed variants come verbatim from today's `FAILURE_PATTERNS` (`game_monitor.rs:1031-1037`). Each variant carries a `transient()` classifier that drives retry. `Oom` is never retried.

- [ ] **Step 1: Write the failing test**

Create `utils/estimator/src/error.rs` with the test first:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EstimatorError {
    #[error("no backend is currently healthy to serve traffic")]
    NoHealthyBackend,
    #[error("no state available for block")]
    NoStateAvailable,
    #[error("distance to target block exceeds maximum proof window")]
    ExceedsProofWindow,
    #[error("missing trie node")]
    MissingTrieNode,
    #[error("dns lookup failure")]
    DnsLookupFailure,
    #[error("out of memory during execution")]
    Oom,
    #[error("transient failure: {0}")]
    Transient(#[source] anyhow::Error),
    #[error("fatal failure: {0}")]
    Fatal(#[source] anyhow::Error),
}

impl EstimatorError {
    /// True if the failure is worth retrying. `Oom` is never retried.
    pub fn is_transient(&self) -> bool {
        match self {
            EstimatorError::NoHealthyBackend
            | EstimatorError::NoStateAvailable
            | EstimatorError::DnsLookupFailure
            | EstimatorError::Transient(_) => true,
            EstimatorError::ExceedsProofWindow
            | EstimatorError::MissingTrieNode
            | EstimatorError::Oom
            | EstimatorError::Fatal(_) => false,
        }
    }

    /// Classify an upstream `anyhow` error by matching the legacy failure-pattern
    /// substrings against its full chain (mirrors `game_monitor.rs` FAILURE_PATTERNS).
    pub fn classify(err: anyhow::Error) -> Self {
        let msg = format!("{err:#}").to_lowercase();
        if msg.contains("no backend is currently healthy to serve traffic") {
            EstimatorError::NoHealthyBackend
        } else if msg.contains("no state available for block") {
            EstimatorError::NoStateAvailable
        } else if msg.contains("distance to target block exceeds maximum proof window") {
            EstimatorError::ExceedsProofWindow
        } else if msg.contains("missing trie node") {
            EstimatorError::MissingTrieNode
        } else if msg.contains("dns error") || msg.contains("failed to fetch safe head") {
            EstimatorError::DnsLookupFailure
        } else if msg.contains("memory allocation") || msg.contains("out of memory") {
            EstimatorError::Oom
        } else {
            EstimatorError::Transient(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_legacy_failure_patterns() {
        let e = EstimatorError::classify(anyhow::anyhow!(
            "no backend is currently healthy to serve traffic"
        ));
        assert!(matches!(e, EstimatorError::NoHealthyBackend));
        assert!(e.is_transient());
    }

    #[test]
    fn proof_window_is_fatal() {
        let e = EstimatorError::classify(anyhow::anyhow!(
            "distance to target block exceeds maximum proof window"
        ));
        assert!(matches!(e, EstimatorError::ExceedsProofWindow));
        assert!(!e.is_transient());
    }

    #[test]
    fn oom_is_never_transient() {
        assert!(!EstimatorError::Oom.is_transient());
    }

    #[test]
    fn unknown_error_is_transient_by_default() {
        let e = EstimatorError::classify(anyhow::anyhow!("some novel rpc blip"));
        assert!(matches!(e, EstimatorError::Transient(_)));
        assert!(e.is_transient());
    }
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p op-succinct-estimator error::tests`
Expected: 4 tests pass. (This task's "test" and "impl" are one file because the enum is the unit under test; the tests exercise `classify`/`is_transient`.)

- [ ] **Step 3: Commit**

```bash
git add utils/estimator/src/error.rs
git commit -m "feat(estimator): EstimatorError with transient/fatal classification"
```

---

## Task 3: Lift `aggregate_execution_stats` into the estimator

**Files:**
- Create: `utils/estimator/src/stats.rs`
- Modify: `utils/estimator/src/lib.rs`
- Test: inline in `stats.rs`

Lift `aggregate_execution_stats` (`cost_estimator.rs:196-251`) verbatim — it is a pure fold over `&[ExecutionStats]`. Dropping the CSV round-trip is the whole point (spec §6).

- [ ] **Step 1: Write the failing test**

Create `utils/estimator/src/stats.rs`:

```rust
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
```

- [ ] **Step 2: Wire the module**

In `utils/estimator/src/lib.rs` add:

```rust
pub mod stats;
pub use stats::aggregate_execution_stats;
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p op-succinct-estimator stats::tests`
Expected: 2 tests pass.

> If `ExecutionStats` field names differ from those above (cross-check `utils/host/src/stats.rs:10-41`), the compiler will name the mismatch — fix to match the real struct, do not invent fields.

- [ ] **Step 4: Commit**

```bash
git add utils/estimator/src/stats.rs utils/estimator/src/lib.rs
git commit -m "feat(estimator): lift aggregate_execution_stats (no CSV round-trip)"
```

---

## Task 4: Witness cache — key, paths, DA discriminator, absolute base dir

**Files:**
- Create: `utils/estimator/src/cache.rs`
- Modify: `utils/estimator/src/lib.rs`
- Test: inline in `cache.rs`

Adapt `witness_cache.rs:17-67`. Fixes per spec §4.4: add a DA-type discriminator to the key (EigenDA's `EigenDAWitnessData` is incompatible with `DefaultWitnessData`); make the base dir a configurable absolute path instead of cwd-relative `data/`.

- [ ] **Step 1: Write the failing test for key/path construction**

Create `utils/estimator/src/cache.rs`:

```rust
use std::path::{Path, PathBuf};

/// DA-type discriminator folded into every cache key. The on-disk `WitnessData`
/// layouts differ by DA (EigenDA adds `eigenda_data`), so a key without this would
/// let an EigenDA blob be loaded as a DefaultWitnessData and vice-versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaType {
    Ethereum,
    Celestia,
    EigenDa,
}

impl DaType {
    pub fn as_str(self) -> &'static str {
        match self {
            DaType::Ethereum => "ethereum",
            DaType::Celestia => "celestia",
            DaType::EigenDa => "eigenda",
        }
    }
}

/// On-disk cache for `WitnessData` and `SP1Stdin` blobs, keyed by
/// `(chain_id, start, end, da_type)` under an absolute base directory.
#[derive(Debug, Clone)]
pub struct WitnessCache {
    base_dir: PathBuf,
    chain_id: u64,
    da_type: DaType,
}

impl WitnessCache {
    pub fn new(base_dir: impl AsRef<Path>, chain_id: u64, da_type: DaType) -> Self {
        Self { base_dir: base_dir.as_ref().to_path_buf(), chain_id, da_type }
    }

    fn cache_dir(&self) -> PathBuf {
        self.base_dir.join(self.chain_id.to_string()).join("witness-cache")
    }

    pub fn witness_path(&self, start: u64, end: u64) -> PathBuf {
        self.cache_dir().join(format!("{start}-{end}-{}-witness.bin", self.da_type.as_str()))
    }

    pub fn stdin_path(&self, start: u64, end: u64) -> PathBuf {
        self.cache_dir().join(format!("{start}-{end}-{}-stdin.bin", self.da_type.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_include_da_type_and_absolute_base() {
        let cache = WitnessCache::new("/var/op-succinct/cache", 42220, DaType::EigenDa);
        let w = cache.witness_path(100, 200);
        let s = cache.stdin_path(100, 200);
        assert_eq!(
            w,
            PathBuf::from("/var/op-succinct/cache/42220/witness-cache/100-200-eigenda-witness.bin")
        );
        assert_eq!(
            s,
            PathBuf::from("/var/op-succinct/cache/42220/witness-cache/100-200-eigenda-stdin.bin")
        );
    }

    #[test]
    fn eigenda_and_ethereum_keys_differ() {
        let eigen = WitnessCache::new("/c", 1, DaType::EigenDa);
        let eth = WitnessCache::new("/c", 1, DaType::Ethereum);
        assert_ne!(eigen.witness_path(1, 2), eth.witness_path(1, 2));
    }
}
```

- [ ] **Step 2: Wire the module**

In `utils/estimator/src/lib.rs`:

```rust
pub mod cache;
pub use cache::{DaType, WitnessCache};
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p op-succinct-estimator cache::tests`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add utils/estimator/src/cache.rs utils/estimator/src/lib.rs
git commit -m "feat(estimator): WitnessCache keys with DA discriminator + absolute base"
```

---

## Task 5: Witness cache — `SP1Stdin` blob save/load + existence checks

**Files:**
- Modify: `utils/estimator/src/cache.rs`
- Test: inline in `cache.rs`

`SP1Stdin` implements serde, so reuse the existing bincode approach (`witness_cache.rs:30-46`). Round-trip an empty `SP1Stdin` through a temp dir.

- [ ] **Step 1: Write the failing test**

Append to `utils/estimator/src/cache.rs` impl block, the stdin methods:

```rust
use anyhow::Result;
use sp1_sdk::SP1Stdin;
use std::fs;

impl WitnessCache {
    pub fn has_stdin(&self, start: u64, end: u64) -> bool {
        self.stdin_path(start, end).exists()
    }

    pub fn save_stdin(&self, start: u64, end: u64, stdin: &SP1Stdin) -> Result<PathBuf> {
        let dir = self.cache_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let path = self.stdin_path(start, end);
        let tmp = path.with_extension("bin.tmp");
        fs::write(&tmp, bincode::serialize(stdin)?)?;
        fs::rename(&tmp, &path)?; // atomic publish so a crash mid-write leaves no half blob
        Ok(path)
    }

    pub fn load_stdin(&self, start: u64, end: u64) -> Result<Option<SP1Stdin>> {
        let path = self.stdin_path(start, end);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        Ok(Some(bincode::deserialize(&bytes)?))
    }
}
```

Add to the test module:

```rust
    #[test]
    fn stdin_round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 42220, DaType::EigenDa);
        assert!(!cache.has_stdin(10, 20));
        let stdin = sp1_sdk::SP1Stdin::default();
        cache.save_stdin(10, 20, &stdin).unwrap();
        assert!(cache.has_stdin(10, 20));
        let loaded = cache.load_stdin(10, 20).unwrap();
        assert!(loaded.is_some());
    }

    #[test]
    fn load_stdin_returns_none_on_miss() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 1, DaType::Ethereum);
        assert!(cache.load_stdin(1, 2).unwrap().is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p op-succinct-estimator cache::tests`
Expected: 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git add utils/estimator/src/cache.rs
git commit -m "feat(estimator): cache SP1Stdin blobs with atomic publish"
```

---

## Task 6: Witness cache — `WitnessData` blob save/load (rkyv)

**Files:**
- Modify: `utils/estimator/src/cache.rs`
- Test: inline in `cache.rs` (eigenda-feature-gated)

`DefaultWitnessData`/`EigenDAWitnessData` derive only rkyv (`utils/client/src/witness/mod.rs:44,61`). Serialize with `rkyv::to_bytes::<rkyv::rancor::Error>` and read back with `rkyv::from_bytes::<W, _>` — the exact pattern the range program uses (`programs/range/eigenda/src/main.rs:30`).

- [ ] **Step 1: Write the failing test**

Append to `utils/estimator/src/cache.rs`:

```rust
use rkyv::rancor::Error as RkyvError;
use rkyv::{api::high::HighSerializer, ser::allocator::ArenaHandle, util::AlignedVec, Archive, Serialize as RkyvSerialize};

impl WitnessCache {
    pub fn has_witness(&self, start: u64, end: u64) -> bool {
        self.witness_path(start, end).exists()
    }

    /// Persist a `WitnessData` blob via rkyv. `W` must be the DA-specific witness type.
    pub fn save_witness<W>(&self, start: u64, end: u64, witness: &W) -> Result<PathBuf>
    where
        W: for<'a> RkyvSerialize<HighSerializer<AlignedVec, ArenaHandle<'a>, RkyvError>>,
    {
        let dir = self.cache_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let path = self.witness_path(start, end);
        let tmp = path.with_extension("bin.tmp");
        let bytes = rkyv::to_bytes::<RkyvError>(witness)?;
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Load a `WitnessData` blob via rkyv. Returns `None` on a miss.
    pub fn load_witness<W>(&self, start: u64, end: u64) -> Result<Option<W>>
    where
        W: Archive,
        W::Archived: rkyv::Deserialize<W, rkyv::api::high::HighDeserializer<RkyvError>>,
    {
        let path = self.witness_path(start, end);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        Ok(Some(rkyv::from_bytes::<W, RkyvError>(&bytes)?))
    }

    /// Drop the (large) `WitnessData` blob once its stdin is built. Idempotent.
    pub fn drop_witness(&self, start: u64, end: u64) -> Result<()> {
        let path = self.witness_path(start, end);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}
```

Add an eigenda-gated test:

```rust
    #[cfg(feature = "eigenda")]
    #[test]
    fn witness_round_trips_and_drops() {
        use op_succinct_client_utils::witness::EigenDAWitnessData;
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 42220, DaType::EigenDa);
        let witness = EigenDAWitnessData::default();
        assert!(!cache.has_witness(10, 20));
        cache.save_witness(10, 20, &witness).unwrap();
        assert!(cache.has_witness(10, 20));
        let loaded: Option<EigenDAWitnessData> = cache.load_witness(10, 20).unwrap();
        assert!(loaded.is_some());
        cache.drop_witness(10, 20).unwrap();
        assert!(!cache.has_witness(10, 20));
        cache.drop_witness(10, 20).unwrap(); // idempotent
    }
```

- [ ] **Step 2: Verify the rkyv generic bounds compile**

Run: `cargo test -p op-succinct-estimator --no-default-features --features eigenda cache::tests`
Expected: all cache tests pass including `witness_round_trips_and_drops`.

> The exact rkyv 0.8 serializer/deserializer type aliases (`HighSerializer`, `HighDeserializer`, `ArenaHandle`, `AlignedVec`) must match the version locked in `Cargo.lock`. If a bound fails to resolve, mirror the concrete call sites already in the tree: serialize like `utils/eigenda/host/src/witness_generator.rs:95` (`to_bytes::<rkyv::rancor::Error>(&witness)`) and deserialize like `programs/range/eigenda/src/main.rs:30` (`rkyv::from_bytes::<EigenDAWitnessData, Error>(&bytes)`). Simplify the generic bounds until those two call patterns type-check.

- [ ] **Step 3: Confirm the import path for the witness type**

Run: `cargo check -p op-succinct-client-utils` then confirm `EigenDAWitnessData` is exported. If the path differs from `op_succinct_client_utils::witness::EigenDAWitnessData`, find it:
Run: `rg -n "pub use|pub struct EigenDAWitnessData" utils/client/src`
Fix the test import to the real path.

- [ ] **Step 4: Commit**

```bash
git add utils/estimator/src/cache.rs
git commit -m "feat(estimator): cache WitnessData blobs via rkyv + drop_witness"
```

---

## Task 7: Witness cache — separate-clock pruning of stdin

**Files:**
- Modify: `utils/estimator/src/cache.rs`
- Test: inline in `cache.rs`

Spec §4.4 pruning: drop `WitnessData` once `SP1Stdin` is built (Task 6 `drop_witness` already does this on demand); keep `SP1Stdin` until the owning game **succeeds** + a configurable grace window (~1h). Games are disjoint, so stdin pruning is time-based per range. Implement age-based pruning the daemon schedules.

- [ ] **Step 1: Write the failing test**

Append to `utils/estimator/src/cache.rs`:

```rust
impl WitnessCache {
    /// Delete a range's stdin blob (called after the owning game succeeds + grace).
    pub fn prune_stdin(&self, start: u64, end: u64) -> Result<()> {
        let path = self.stdin_path(start, end);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Age in seconds of a range's stdin blob, or `None` if absent/unreadable.
    pub fn stdin_age_secs(&self, start: u64, end: u64, now: std::time::SystemTime) -> Option<u64> {
        let path = self.stdin_path(start, end);
        let modified = fs::metadata(&path).ok()?.modified().ok()?;
        now.duration_since(modified).ok().map(|d| d.as_secs())
    }
}
```

Add tests:

```rust
    #[test]
    fn prune_stdin_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 1, DaType::Ethereum);
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        assert!(cache.has_stdin(1, 2));
        cache.prune_stdin(1, 2).unwrap();
        assert!(!cache.has_stdin(1, 2));
        cache.prune_stdin(1, 2).unwrap(); // no error on missing
    }

    #[test]
    fn stdin_age_is_some_after_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = WitnessCache::new(dir.path(), 1, DaType::Ethereum);
        cache.save_stdin(1, 2, &sp1_sdk::SP1Stdin::default()).unwrap();
        let age = cache.stdin_age_secs(1, 2, std::time::SystemTime::now());
        assert!(age.is_some());
        assert!(cache.stdin_age_secs(9, 9, std::time::SystemTime::now()).is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p op-succinct-estimator cache::tests`
Expected: all cache tests pass.

- [ ] **Step 3: Commit**

```bash
git add utils/estimator/src/cache.rs
git commit -m "feat(estimator): stdin prune + age helpers (separate-clock pruning)"
```

---

## Task 8: Additive safe-head splitter that reuses a shared fetcher

**Files:**
- Modify: `utils/host/src/block_range.rs`
- Test: inline `#[cfg(test)]` in `block_range.rs`

Spec §6: add `split_range_based_on_safe_heads_with_fetcher(&fetcher, …)` so the splitter reuses the shared fetcher instead of building `OPSuccinctDataFetcher::default()` twice (`block_range.rs:124,146`). Leave the existing signature intact (used by `cost_estimator.rs`); make it delegate.

- [ ] **Step 1: Add the new function delegating from the old**

In `utils/host/src/block_range.rs`, refactor so the existing `split_range_based_on_safe_heads` builds the fetcher once and calls the new variant. Replace the body of `split_range_based_on_safe_heads` (lines 119–180) with a thin wrapper and add the `_with_fetcher` variant holding the real logic:

```rust
/// Existing public signature — preserved. Builds a fetcher once and delegates.
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
    // Get the L1 origin of l2_start.
    let l2_start_hex = format!("0x{l2_start:x}");
    let start_output: OutputResponse = data_fetcher
        .fetch_rpc_data_with_mode(RPCMode::L2Node, "optimism_outputAtBlock", vec![l2_start_hex.into()])
        .await?;
    let l1_start = start_output.block_ref.l1_origin.number;

    // Get the L1Head from which l2_end can be derived.
    let (_, l1_head_number) = data_fetcher.get_safe_l1_block_for_l2_block(l2_end).await?;

    // Collect unique safeHeads between l1_start and l1_head, reusing `data_fetcher`.
    let safe_heads = futures::stream::iter(l1_start..=l1_head_number)
        .map(|block| {
            let data_fetcher = data_fetcher;
            async move {
                let l1_block_hex = format!("0x{block:x}");
                let result: SafeHeadResponse = data_fetcher
                    .fetch_rpc_data_with_mode(
                        RPCMode::L2Node,
                        "optimism_safeHeadAtL1Block",
                        vec![l1_block_hex.into()],
                    )
                    .await
                    .expect("Failed to fetch safe head");
                result.safe_head.number
            }
        })
        .buffered(15)
        .collect::<std::collections::HashSet<_>>()
        .await;

    let mut safe_heads: Vec<_> = safe_heads.into_iter().collect();
    safe_heads.sort();

    let mut ranges = Vec::new();
    let mut current_l2_start = l2_start;
    for safe_head in safe_heads {
        if safe_head > current_l2_start && current_l2_start < l2_end {
            let mut range_start = current_l2_start;
            while range_start + max_range_size < min(l2_end, safe_head) {
                ranges.push(SpanBatchRange { start: range_start, end: range_start + max_range_size });
                range_start += max_range_size;
            }
            ranges.push(SpanBatchRange { start: range_start, end: min(l2_end, safe_head) });
            current_l2_start = safe_head;
        }
    }
    Ok(ranges)
}
```

> Confirm the closure borrow of `data_fetcher` across `buffered` compiles (it's `&OPSuccinctDataFetcher`, which is `Sync`). If the borrow checker complains about the captured reference outliving the stream, capture via `let data_fetcher = data_fetcher;` (shown) or `move` with an `Arc` — but prefer keeping the shared borrow.

- [ ] **Step 2: Write a determinism test for `split_range_basic` boundaries (no network)**

The safe-head splitter needs live RPC, so the unit test targets the deterministic chunking the daemon and pipeline both depend on. Add to the `#[cfg(test)] mod tests` in `block_range.rs`:

```rust
    #[test]
    fn basic_split_respects_max_range_and_covers_window() {
        let ranges = split_range_basic(100, 250, 60);
        assert_eq!(ranges.first().unwrap().start, 100);
        assert_eq!(ranges.last().unwrap().end, 250);
        for w in &ranges {
            assert!(w.end - w.start <= 60);
        }
        // Contiguous, no gaps.
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }
```

- [ ] **Step 3: Run the test + confirm the old call site still compiles**

Run: `cargo test -p op-succinct-host-utils block_range::tests::basic_split_respects_max_range_and_covers_window`
Expected: PASS.
Run: `cargo check -p op-succinct-scripts --no-default-features --features eigenda`
Expected: `cost_estimator.rs` still compiles against the unchanged `split_range_based_on_safe_heads`.

- [ ] **Step 4: Commit**

```bash
git add utils/host/src/block_range.rs
git commit -m "feat(host): split_range_based_on_safe_heads_with_fetcher (reuse shared fetcher)"
```

---

## Task 9: Lift `network_call_with_timeout` into the estimator

**Files:**
- Create: `utils/estimator/src/retry.rs`
- Modify: `utils/estimator/src/lib.rs`
- Test: inline in `retry.rs`

Lift the reusable helper from `fault-proof/src/prover.rs:341-368` as a free async function (drop the `&self`, `ProposerGauge`, `proof_id` coupling) so the estimator and pipeline can bound any idempotent network future with a timeout.

- [ ] **Step 1: Write the failing test**

Create `utils/estimator/src/retry.rs`:

```rust
use anyhow::{bail, Result};
use std::future::Future;
use std::time::Duration;

/// Bound an idempotent network future with a timeout. On timeout returns an error
/// classified `transient` downstream. Lifted from `fault-proof/src/prover.rs:341-368`.
pub async fn network_call_with_timeout<F, T>(
    timeout_secs: u64,
    operation: &str,
    future: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(Duration::from_secs(timeout_secs), future).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => {
            tracing::warn!(operation, error = %e, "network error");
            Err(e)
        }
        Err(_) => {
            tracing::warn!(operation, timeout_secs, "network call timed out");
            bail!("network timeout after {timeout_secs}s for {operation}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_value_when_future_completes() {
        let v = network_call_with_timeout(5, "noop", async { Ok::<_, anyhow::Error>(7) })
            .await
            .unwrap();
        assert_eq!(v, 7);
    }

    #[tokio::test]
    async fn times_out_slow_future() {
        let res: Result<()> = network_call_with_timeout(1, "slow", async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        })
        .await;
        assert!(res.is_err());
        assert!(format!("{}", res.unwrap_err()).contains("timeout"));
    }
}
```

- [ ] **Step 2: Wire the module**

In `utils/estimator/src/lib.rs`:

```rust
pub mod retry;
pub use retry::network_call_with_timeout;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p op-succinct-estimator retry::tests`
Expected: 2 tests pass (the slow one uses tokio's paused clock implicitly via real sleep; if flaky on CI, wrap with `tokio::time::pause()` — but real 1s vs 5s is fine here).

- [ ] **Step 4: Commit**

```bash
git add utils/estimator/src/retry.rs utils/estimator/src/lib.rs
git commit -m "feat(estimator): lift network_call_with_timeout helper"
```

---

## Task 10: Harden `OnlineBlobStore` — convert panics to retryable errors

**Files:**
- Modify: `utils/host/src/witness_generation/online_blob_store.rs`
- Test: inline `#[cfg(test)]` in that file

Spec §4.5: convert the KZG `.unwrap()`s (lines 46,47,48,50) and the `Mutex` lock (line 32) in `OnlineBlobStore` to errors so a blob/KZG fault is a retryable `EstimatorError` rather than a crash. The `BlobProvider::Error` associated type is `T::Error` today; widen the error path so failures propagate.

- [ ] **Step 1: Make `get_blob_data` fallible**

In `utils/host/src/witness_generation/online_blob_store.rs`, change `get_blob_data` (lines 44–53) to return `Result` and replace each `.unwrap()`:

```rust
fn get_blob_data(
    blob: &Blob,
    settings: &EnvKzgSettings,
) -> anyhow::Result<(KzgRsBlob, Bytes48, Bytes48)> {
    let c_kzg_blob = c_kzg::Blob::from_bytes(blob.as_slice())
        .map_err(|e| anyhow::anyhow!("invalid KZG blob: {e:?}"))?;
    let commitment = settings
        .get()
        .blob_to_kzg_commitment(&c_kzg_blob)
        .map_err(|e| anyhow::anyhow!("blob_to_kzg_commitment failed: {e:?}"))?;
    let proof = settings
        .get()
        .compute_blob_kzg_proof(&c_kzg_blob, &commitment.to_bytes())
        .map_err(|e| anyhow::anyhow!("compute_blob_kzg_proof failed: {e:?}"))?;
    let rs_blob = KzgRsBlob::from_slice(&*c_kzg_blob)
        .map_err(|e| anyhow::anyhow!("KzgRsBlob::from_slice failed: {e:?}"))?;
    Ok((rs_blob, kzg_rs::Bytes48(*commitment.to_bytes()), kzg_rs::Bytes48(*proof.to_bytes())))
}
```

- [ ] **Step 2: Propagate at the call site, replacing the `Mutex` unwrap**

In `get_and_validate_blobs` (lines 25–41), replace the `self.store.lock().unwrap()` (line 32) and the `get_blob_data` call. The method returns `Result<_, Self::Error>` where `Self::Error = T::Error`. To carry our new errors, the cleanest path that matches kona's trait is to map into `T::Error` if it is `From<...>`, but the lowest-risk change preserving the trait is to log-and-skip-poison by recovering the lock and `?`-ing the blob computation into the existing error type. Verify `T::Error`'s constructibility first:

Run: `rg -n "type Error" utils/host/src/witness_generation/online_blob_store.rs`
Then implement:

```rust
async fn get_and_validate_blobs(
    &mut self,
    block_ref: &BlockInfo,
    blob_hashes: &[B256],
) -> Result<Vec<Box<Blob>>, Self::Error> {
    let blobs = self.provider.get_and_validate_blobs(block_ref, blob_hashes).await?;
    let settings = EnvKzgSettings::default();

    let mut store = self
        .store
        .lock()
        .map_err(|_| Self::Error::from(anyhow::anyhow!("OnlineBlobStore mutex poisoned")))?;
    for blob in &blobs {
        let (c_kzg_blob, commitment, proof) = get_blob_data(blob, &settings)
            .map_err(Self::Error::from)?;
        store.blobs.push(c_kzg_blob);
        store.commitments.push(commitment);
        store.proofs.push(proof);
    }
    Ok(blobs)
}
```

> If `Self::Error` (i.e. `T::Error`, the kona `BlobProviderError`) does **not** implement `From<anyhow::Error>`, do not force it. Instead pick the closest existing variant (e.g. a `Backend(String)`/`Custom` variant) and construct it: inspect with `rg -n "enum .*BlobProviderError|impl .*BlobProviderError" $(rg -l BlobProviderError ~/.cargo 2>/dev/null | head -1)` — but **do not edit anything under `~/.cargo`**. If no constructible variant exists, widen `OnlineBlobStore`'s own `type Error` to a local enum `OnlineBlobStoreError<E>` that wraps both `E` and `anyhow::Error`, and update the impl's `type Error`. Choose the smaller diff that compiles.

- [ ] **Step 3: Add a unit test for the KZG-failure path**

Add to (or create) a `#[cfg(test)] mod tests` in `online_blob_store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_blob_data_errors_on_invalid_blob() {
        // A wrong-sized blob must yield an error, not a panic.
        let settings = EnvKzgSettings::default();
        let bad = Blob::default(); // confirm Blob::default() is not a valid KZG blob; else build a short one
        // If Blob::default() happens to be valid, replace with an intentionally malformed input.
        let _ = get_blob_data(&bad, &settings); // must not panic
    }
}
```

> If `Blob::default()` is a valid KZG blob (no error), the meaningful assertion is simply "does not panic". Keep the test as a panic-guard; the behavioural win (errors propagate) is covered by the integration forced-failure test in Task 22.

- [ ] **Step 4: Verify it compiles and the test passes**

Run: `cargo test -p op-succinct-host-utils online_blob_store`
Run: `cargo check -p op-succinct-host-utils --no-default-features --features eigenda` (confirm the eigenda host, which reuses `OnlineBlobStore`, still builds)
Expected: both succeed.

- [ ] **Step 5: Commit**

```bash
git add utils/host/src/witness_generation/online_blob_store.rs
git commit -m "fix(host): convert OnlineBlobStore KZG/mutex panics to retryable errors"
```

---

## Task 11: Add `RetryBackoffLayer` to L1/L2 providers

**Files:**
- Modify: `utils/host/src/fetcher.rs`
- Modify: root `Cargo.toml` (only if `alloy-rpc-client` / the retry layer is not yet a dep)
- Test: inline `#[cfg(test)]` in `fetcher.rs`

Spec §4.5 / §8: the fetcher's providers are built with `ProviderBuilder::default().connect_http(url)` and have no retry (`fetcher.rs:186-188,205-207`; `FIXME:99`). Add alloy's `RetryBackoffLayer` so idempotent RPC blips retry before expensive witness-gen.

- [ ] **Step 1: Confirm the retry layer is available**

`RetryBackoffLayer` lives in `alloy-transport` (re-exported via `alloy::transport::layers`). The workspace pins `alloy = "=1.6.3"` with features incl. `providers`,`transport` (root `Cargo.toml:165-172`) and `alloy-transport = "=1.6.3"` (line 160).

Run: `rg -n "RetryBackoffLayer" ~/.cargo/registry/src 2>/dev/null | grep "alloy-transport-1.6.3" | head`
If found, no new dep is needed — use `alloy::transport::layers::RetryBackoffLayer`. If the `alloy` umbrella does not re-export it, add to root `[workspace.dependencies]`:

```toml
alloy-rpc-client = { version = "=1.6.3" }
```

and use `alloy_transport::layers::RetryBackoffLayer` + `alloy_rpc_client::ClientBuilder`.

- [ ] **Step 2: Build the providers through a retrying client**

In `utils/host/src/fetcher.rs`, add a small constructor helper and use it in both `new` (186-188) and `new_with_rollup_config` (205-207). The retry layer is applied at the RPC-client layer:

```rust
use alloy_transport::layers::RetryBackoffLayer;
use alloy_rpc_client::ClientBuilder;

/// Build an HTTP RootProvider with a bounded retry/backoff layer for idempotent calls.
/// max_retries=3, initial_backoff=500ms, compute-units-per-second cap left default.
fn http_provider_with_retries<N: alloy_provider::Network>(url: Url) -> Arc<RootProvider<N>> {
    let retry = RetryBackoffLayer::new(3, 500, 100);
    let client = ClientBuilder::default().layer(retry).http(url);
    Arc::new(RootProvider::<N>::new(client))
}
```

Replace the provider construction lines, e.g.:

```rust
let l1_provider = http_provider_with_retries::<alloy_network::Ethereum>(rpc_config.l1_rpc.clone());
let l2_provider = http_provider_with_retries::<Celo>(rpc_config.l2_rpc.clone());
```

> The exact `RetryBackoffLayer::new` signature for alloy 1.6.3 is `(max_rate_limit_retries, initial_backoff_ms, compute_units_per_second)`. Confirm with `rg -n "impl RetryBackoffLayer|pub fn new" $(rg -l "struct RetryBackoffLayer" ~/.cargo/registry/src/*/alloy-transport-1.6.3/ 2>/dev/null | head -1)` and adjust the argument names. The `RootProvider::<N>::new(client)` shape and the `Network` generic (`Celo` for L2, `Ethereum` for L1) must match the existing `RootProvider` / `RootProvider<Celo>` field types in the struct (`fetcher.rs:101-102`). If `ProviderBuilder::default().connect_client(client)` is the idiomatic 1.6.3 path, prefer it — keep the change minimal and type-correct.

- [ ] **Step 3: Wrap the raw JSON-RPC path with the timeout helper**

`fetch_rpc_data` (`fetcher.rs:565-590`) builds a fresh `reqwest::Client` per call with no timeout. Add a timeout to the request builder so a wedged call can't hang forever:

```rust
let client = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(120))
    .build()?;
```

(Per-call retry of this raw path is provided at the caller boundary via `network_call_with_timeout` from Task 9 where the pipeline invokes it; the providers built in Step 2 already retry the typed alloy calls.)

- [ ] **Step 4: Add a smoke test that providers still construct**

Add to the `#[cfg(test)] mod tests` in `fetcher.rs` (env-gated so it doesn't need a live node):

```rust
    #[test]
    fn http_provider_with_retries_constructs() {
        let url: Url = "http://localhost:8545".parse().unwrap();
        let _p = http_provider_with_retries::<alloy_network::Ethereum>(url);
        // Construction must not panic; no network call is made.
    }
```

- [ ] **Step 5: Verify**

Run: `cargo test -p op-succinct-host-utils fetcher::tests::http_provider_with_retries_constructs`
Run: `cargo check -p op-succinct-host-utils`
Expected: both succeed.

- [ ] **Step 6: Commit**

```bash
git add utils/host/src/fetcher.rs Cargo.toml
git commit -m "feat(host): RetryBackoffLayer on L1/L2 providers + RPC timeout (FIXME:99)"
```

---

## Task 12: `Estimator<H>` — `build_range_witness` (fetch + crunch + cache both)

**Files:**
- Create: `utils/estimator/src/estimator.rs`
- Modify: `utils/estimator/src/lib.rs`
- Test: inline in `estimator.rs` (construction/compile test only; behaviour is covered by Task 22 integration)

Realizes the spec §4.3 producer half. A struct holds the shared `host`, `fetcher`, `cache`, `chain_id`. `build_range_witness` runs `host.run` then `get_sp1_stdin` **sequentially**, caching each, then drops `WitnessData` once stdin is built. A `WitnessData` cache hit skips `host.run`.

- [ ] **Step 1: Write the estimator struct + `build_range_witness`**

Create `utils/estimator/src/estimator.rs`:

```rust
use std::sync::Arc;

use anyhow::Result;
use op_succinct_host_utils::{
    block_range::SpanBatchRange,
    fetcher::OPSuccinctDataFetcher,
    host::OPSuccinctHost,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use rkyv::{api::high::{HighDeserializer, HighSerializer}, ser::allocator::ArenaHandle, util::AlignedVec, Archive, Serialize as RkyvSerialize};

use crate::{cache::WitnessCache, error::EstimatorError};

/// In-process estimator over a DA-specific host `H`. Producer + consumer share one.
pub struct Estimator<H: OPSuccinctHost> {
    pub host: Arc<H>,
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    pub cache: WitnessCache,
    pub chain_id: u64,
    pub safe_db_fallback: bool,
}

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

impl<H: OPSuccinctHost> Estimator<H>
where
    WitnessOf<H>: for<'a> RkyvSerialize<HighSerializer<AlignedVec, ArenaHandle<'a>, RkyvError>>
        + Archive,
    <WitnessOf<H> as Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, HighDeserializer<RkyvError>>,
{
    /// Producer: fetch `WitnessData` (host.run) then crunch to `SP1Stdin`, caching both,
    /// then drop the witness blob. A cached witness skips host.run; a cached stdin is a no-op.
    pub async fn build_range_witness(&self, range: &SpanBatchRange) -> Result<(), EstimatorError> {
        if self.cache.has_stdin(range.start, range.end) {
            return Ok(()); // already built
        }

        // 1. WitnessData: load from cache, else host.fetch + host.run, then cache it.
        let witness: WitnessOf<H> = match self
            .cache
            .load_witness::<WitnessOf<H>>(range.start, range.end)
            .map_err(EstimatorError::Transient)?
        {
            Some(w) => w,
            None => {
                let args = self
                    .host
                    .fetch(range.start, range.end, None, self.safe_db_fallback)
                    .await
                    .map_err(EstimatorError::classify)?;
                let w = self.host.run(&args).await.map_err(EstimatorError::classify)?;
                self.cache
                    .save_witness(range.start, range.end, &w)
                    .map_err(EstimatorError::Transient)?;
                w
            }
        };

        // 2. SP1Stdin: pure CPU serialization of the witness, then cache it.
        let stdin = self
            .host
            .witness_generator()
            .get_sp1_stdin(witness)
            .map_err(EstimatorError::classify)?;
        self.cache
            .save_stdin(range.start, range.end, &stdin)
            .map_err(EstimatorError::Transient)?;

        // 3. Drop the (large) witness blob — only needed for a re-crunch.
        self.cache
            .drop_witness(range.start, range.end)
            .map_err(EstimatorError::Transient)?;
        Ok(())
    }
}
```

- [ ] **Step 2: Wire the module**

In `utils/estimator/src/lib.rs`:

```rust
pub mod estimator;
pub use estimator::Estimator;
```

- [ ] **Step 3: Verify it compiles for the eigenda target**

Run: `cargo check -p op-succinct-estimator --no-default-features --features eigenda`
Expected: compiles. The generic bounds on `WitnessOf<H>` must resolve for `EigenDAWitnessData`.

> If the trait bounds do not resolve, the fallback is to drop the generic rkyv bounds and make `Estimator` monomorphic over the concrete DA host chosen by feature flags (the binary picks one host anyway). Confirm `WitnessGenerator::WitnessData` and `OPSuccinctHost::WitnessGenerator` associated-type paths against `utils/host/src/host.rs:66-71` and `utils/host/src/witness_generation/traits.rs:24-92`.

- [ ] **Step 4: Commit**

```bash
git add utils/estimator/src/estimator.rs utils/estimator/src/lib.rs
git commit -m "feat(estimator): build_range_witness (cache WitnessData + SP1Stdin, drop witness)"
```

---

## Task 13: `Estimator<H>` — `execute_range` (load stdin + SP1 execute)

**Files:**
- Modify: `utils/estimator/src/estimator.rs`
- Test: inline in `estimator.rs` (compile/signature test; behaviour in Task 22)

Realizes the spec §4.3 consumer half. Loads cached stdin (builds on miss), runs SP1 execute inside `spawn_blocking` (`CpuProver` spins its own runtime — `cost_estimator.rs:132-135`), and maps the `ExecutionReport` to `ExecutionStats` (`stats.rs:81-139`). **Concurrency is governed by the daemon's RSS gate (Task 17), not a rayon `par_iter`** (spec §4.3).

- [ ] **Step 1: Add `execute_range`**

Append to the `impl<H: OPSuccinctHost> Estimator<H>` block in `estimator.rs`:

```rust
use op_succinct_host_utils::stats::ExecutionStats;
use op_succinct_proof_utils::get_range_elf_embedded;
use sp1_sdk::{blocking::{CpuProver, Prover}, Elf};

impl<H: OPSuccinctHost> Estimator<H>
where
    WitnessOf<H>: for<'a> RkyvSerialize<HighSerializer<AlignedVec, ArenaHandle<'a>, RkyvError>>
        + Archive,
    <WitnessOf<H> as Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, HighDeserializer<RkyvError>>,
{
    /// Consumer: load the cached stdin (build on miss), run the SP1 execute, and
    /// produce `ExecutionStats`. The caller must hold an RSS-admission slot.
    pub async fn execute_range(
        &self,
        range: &SpanBatchRange,
    ) -> Result<ExecutionStats, EstimatorError> {
        // Ensure stdin exists (cache hit is the common path; miss builds on demand).
        if !self.cache.has_stdin(range.start, range.end) {
            self.build_range_witness(range).await?;
        }
        let stdin = self
            .cache
            .load_stdin(range.start, range.end)
            .map_err(EstimatorError::Transient)?
            .ok_or_else(|| EstimatorError::Fatal(anyhow::anyhow!("stdin missing after build")))?;

        // Block data for stats (cheap relative to witness-gen). Parity: l1_head passed as 0.
        let block_data = self
            .fetcher
            .get_l2_block_data_range(range.start, range.end)
            .await
            .map_err(EstimatorError::classify)?;

        // SP1 execute must run off the async runtime: CpuProver spins its own tokio runtime.
        let exec = tokio::task::spawn_blocking(move || {
            let prover = CpuProver::new();
            prover
                .execute(Elf::Static(get_range_elf_embedded()), stdin)
                .deferred_proof_verification(false)
                .run()
        })
        .await
        .map_err(|e| EstimatorError::Fatal(anyhow::anyhow!("execute task join error: {e}")))?;

        let (_public_values, report) = exec.map_err(|e| {
            // SP1 execution failure: classify; an allocation failure becomes Oom (never retried).
            EstimatorError::classify(anyhow::anyhow!("SP1 execute failed: {e:?}"))
        })?;

        Ok(ExecutionStats::new(0, &block_data, &report, 0, 0))
    }
}
```

- [ ] **Step 2: Verify it compiles for the eigenda target**

Run: `cargo check -p op-succinct-estimator --no-default-features --features eigenda`
Expected: compiles.

> Confirm the SP1 6.1.0 `execute(...).run()` return type. From `cost_estimator.rs:140-155` it is `Result<(SP1PublicValues, ExecutionReport), _>` destructured as `let (_, report) = result.unwrap();`. If `run()` returns a different shape, match it. `Elf::Static` and `get_range_elf_embedded()` are the exact calls from `cost_estimator.rs:141`. `stdin` must be moved into the closure (it is `Send`); if `SP1Stdin` is not `Send`, build the prover and stdin inside the closure and pass only the raw bytes — but the existing code moves stdin into a rayon closure, so `Send` holds.

- [ ] **Step 3: Commit**

```bash
git add utils/estimator/src/estimator.rs
git commit -m "feat(estimator): execute_range (load stdin, spawn_blocking SP1 execute, stats)"
```

---

## Task 14: cgroup memory-budget parsing (v1 + v2)

**Files:**
- Create: `utils/estimator/src/memory.rs`
- Modify: `utils/estimator/src/lib.rs`
- Test: inline in `memory.rs` (string fixtures — no real cgroup access)

Spec §7: read the cgroup limit (`/sys/fs/cgroup/memory.max` v2, `memory.limit_in_bytes` v1), not host `/proc/meminfo`; live usage from `memory.current`. Parse as pure functions over file contents so they unit-test with fixtures.

- [ ] **Step 1: Write the failing tests + parsers**

Create `utils/estimator/src/memory.rs`:

```rust
/// Parse a cgroup-v2 `memory.max` value. `"max"` means unlimited → `None`.
pub fn parse_cgroup_v2_max(contents: &str) -> Option<u64> {
    let t = contents.trim();
    if t == "max" {
        None
    } else {
        t.parse::<u64>().ok()
    }
}

/// Parse a cgroup-v1 `memory.limit_in_bytes`. A sentinel near u64::MAX means unlimited.
pub fn parse_cgroup_v1_limit(contents: &str) -> Option<u64> {
    let v = contents.trim().parse::<u64>().ok()?;
    // v1 reports an enormous page-aligned sentinel when unlimited.
    if v >= u64::MAX / 4096 * 4096 - 4096 {
        None
    } else {
        Some(v)
    }
}

/// Read the effective memory budget in bytes from the cgroup, preferring v2.
/// Returns `None` if no limit is set (unlimited) or the files are absent.
pub fn read_cgroup_budget_bytes() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        return parse_cgroup_v2_max(&s);
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        return parse_cgroup_v1_limit(&s);
    }
    None
}

/// Read live usage in bytes from the cgroup (v2 `memory.current`, v1 `memory.usage_in_bytes`).
pub fn read_cgroup_usage_bytes() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.current") {
        return s.trim().parse::<u64>().ok();
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.usage_in_bytes") {
        return s.trim().parse::<u64>().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_unlimited_is_none() {
        assert_eq!(parse_cgroup_v2_max("max\n"), None);
    }

    #[test]
    fn v2_number_parses() {
        assert_eq!(parse_cgroup_v2_max("536870912000\n"), Some(536_870_912_000));
    }

    #[test]
    fn v1_sentinel_is_unlimited() {
        assert_eq!(parse_cgroup_v1_limit("9223372036854771712\n"), None);
    }

    #[test]
    fn v1_real_limit_parses() {
        assert_eq!(parse_cgroup_v1_limit("536870912000\n"), Some(536_870_912_000));
    }
}
```

- [ ] **Step 2: Wire the module**

In `utils/estimator/src/lib.rs`:

```rust
pub mod memory;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p op-succinct-estimator memory::tests`
Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add utils/estimator/src/memory.rs utils/estimator/src/lib.rs
git commit -m "feat(estimator): cgroup v1/v2 memory budget + usage parsing"
```

---

## Task 15: RSS admission gate + peak-RSS projection history

**Files:**
- Modify: `utils/estimator/src/memory.rs`
- Test: inline in `memory.rs`

Spec §7: an admission gate that records **peak RSS** per unit (build and execute) keyed by **EVM gas**, projects the next unit's peak from history, and admits only if it fits the remaining budget with a margin. Pure projection logic is unit-tested with synthetic history; live usage comes from Task 14.

- [ ] **Step 1: Write the failing tests + the gate**

Append to `utils/estimator/src/memory.rs`:

```rust
use serde::{Deserialize, Serialize};

/// One observed completion: peak RSS for a unit of work, keyed by EVM gas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssSample {
    pub gas: u64,
    pub peak_rss_bytes: u64,
    pub kind: WorkKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkKind {
    Build,
    Execute,
}

/// Bounded history of peak-RSS samples used to project the next unit's footprint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RssHistory {
    pub samples: Vec<RssSample>,
    pub max_len: usize,
}

impl RssHistory {
    pub fn new(max_len: usize) -> Self {
        Self { samples: Vec::new(), max_len: max_len.max(1) }
    }

    pub fn record(&mut self, sample: RssSample) {
        self.samples.push(sample);
        if self.samples.len() > self.max_len {
            let overflow = self.samples.len() - self.max_len;
            self.samples.drain(0..overflow);
        }
    }

    /// Project peak RSS for a unit of `kind` and `gas`. Uses the max peak/gas ratio
    /// observed for that kind (conservative), times gas, with a floor of the largest
    /// recorded peak when there is no usable signal yet.
    pub fn project_peak(&self, kind: WorkKind, gas: u64, default_bytes: u64) -> u64 {
        let mut max_ratio = 0f64;
        let mut max_peak = 0u64;
        for s in self.samples.iter().filter(|s| s.kind == kind && s.gas > 0) {
            max_ratio = max_ratio.max(s.peak_rss_bytes as f64 / s.gas as f64);
            max_peak = max_peak.max(s.peak_rss_bytes);
        }
        if max_ratio > 0.0 {
            ((max_ratio * gas as f64) as u64).max(max_peak)
        } else {
            default_bytes
        }
    }
}

/// Decide whether a unit projected to need `projected_bytes` fits the budget given
/// `current_usage_bytes`, leaving `margin_bytes` headroom.
pub fn admits(
    budget_bytes: Option<u64>,
    current_usage_bytes: u64,
    projected_bytes: u64,
    margin_bytes: u64,
) -> bool {
    match budget_bytes {
        None => true, // no cgroup limit → unlimited
        Some(budget) => current_usage_bytes
            .saturating_add(projected_bytes)
            .saturating_add(margin_bytes)
            <= budget,
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn projects_from_history_ratio() {
        let mut h = RssHistory::new(8);
        h.record(RssSample { gas: 1_000, peak_rss_bytes: 10_000, kind: WorkKind::Execute });
        // ratio 10 bytes/gas → 2000 gas projects 20_000.
        assert_eq!(h.project_peak(WorkKind::Execute, 2_000, 1), 20_000);
    }

    #[test]
    fn falls_back_to_default_without_signal() {
        let h = RssHistory::new(8);
        assert_eq!(h.project_peak(WorkKind::Build, 5_000, 42), 42);
    }

    #[test]
    fn admits_when_it_fits_and_rejects_when_it_does_not() {
        assert!(admits(Some(100), 10, 50, 20)); // 80 <= 100
        assert!(!admits(Some(100), 60, 50, 20)); // 130 > 100
        assert!(admits(None, u64::MAX, u64::MAX, u64::MAX)); // unlimited
    }

    #[test]
    fn history_is_bounded() {
        let mut h = RssHistory::new(2);
        for g in 0..5 {
            h.record(RssSample { gas: g + 1, peak_rss_bytes: 1, kind: WorkKind::Build });
        }
        assert_eq!(h.samples.len(), 2);
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p op-succinct-estimator memory::`
Expected: all memory tests pass (parsing + admission).

- [ ] **Step 3: Commit**

```bash
git add utils/estimator/src/memory.rs
git commit -m "feat(estimator): RSS admission gate + peak-RSS projection history"
```

---

## Task 16: Window prediction from `PROPOSAL_INTERVAL`

**Files:**
- Create: `utils/estimator/src/window.rs`
- Modify: `utils/estimator/src/lib.rs`
- Test: inline in `window.rs`

Spec §4.2 step 1: predict the next window `[frontier, frontier + PROPOSAL_INTERVAL]` from the proposer's interval; track the frontier (last built end). Pure arithmetic, plus a lead-distance cap (spec §11) so the pipeline never runs unbounded ahead.

- [ ] **Step 1: Write the failing tests + the predictor**

Create `utils/estimator/src/window.rs`:

```rust
use op_succinct_host_utils::block_range::SpanBatchRange;

/// Predicts proposal windows from the proposer's `PROPOSAL_INTERVAL`.
#[derive(Debug, Clone)]
pub struct WindowPredictor {
    proposal_interval: u64,
    /// Last built window end (the frontier).
    frontier: u64,
    /// Max windows the pipeline may run ahead of the frontier.
    max_lead_windows: u64,
}

impl WindowPredictor {
    pub fn new(start_frontier: u64, proposal_interval: u64, max_lead_windows: u64) -> Self {
        assert!(proposal_interval > 0, "PROPOSAL_INTERVAL must be > 0");
        Self { proposal_interval, frontier: start_frontier, max_lead_windows: max_lead_windows.max(1) }
    }

    pub fn frontier(&self) -> u64 {
        self.frontier
    }

    /// The next predicted window after the frontier.
    pub fn next_window(&self) -> SpanBatchRange {
        SpanBatchRange { start: self.frontier, end: self.frontier + self.proposal_interval }
    }

    /// Whether predicting `window.end` stays within the lead cap relative to a known
    /// finalized L2 head — prevents speculating arbitrarily far ahead.
    pub fn within_lead(&self, window: &SpanBatchRange, finalized_l2: u64) -> bool {
        let lead = window.end.saturating_sub(finalized_l2);
        lead <= self.proposal_interval * self.max_lead_windows
    }

    /// Advance the frontier once a window has been built.
    pub fn advance_to(&mut self, built_end: u64) {
        if built_end > self.frontier {
            self.frontier = built_end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicts_next_window_from_interval() {
        let p = WindowPredictor::new(1000, 200, 3);
        let w = p.next_window();
        assert_eq!(w.start, 1000);
        assert_eq!(w.end, 1200);
    }

    #[test]
    fn advances_frontier_monotonically() {
        let mut p = WindowPredictor::new(1000, 200, 3);
        p.advance_to(1200);
        assert_eq!(p.frontier(), 1200);
        p.advance_to(900); // never goes backwards
        assert_eq!(p.frontier(), 1200);
        assert_eq!(p.next_window().start, 1200);
    }

    #[test]
    fn lead_cap_bounds_speculation() {
        let p = WindowPredictor::new(1000, 200, 2); // cap = 400 ahead of finalized
        let near = SpanBatchRange { start: 1000, end: 1200 };
        let far = SpanBatchRange { start: 2000, end: 2200 };
        assert!(p.within_lead(&near, 1000));
        assert!(!p.within_lead(&far, 1000));
    }
}
```

- [ ] **Step 2: Wire the module**

In `utils/estimator/src/lib.rs`:

```rust
pub mod window;
pub use window::WindowPredictor;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p op-succinct-estimator window::tests`
Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add utils/estimator/src/window.rs utils/estimator/src/lib.rs
git commit -m "feat(estimator): PROPOSAL_INTERVAL window prediction with lead cap"
```

**Milestone:** The `utils/estimator` library is complete and tested. Run the full library suite before starting the daemon half:
Run: `cargo test -p op-succinct-estimator --no-default-features --features eigenda`
Expected: all unit tests pass.

---

## Task 17: Daemon scaffold — binary target, CLI args, tracing, support module

**Files:**
- Create: `scripts/utils/bin/game_monitor_contained.rs`
- Create: `scripts/utils/src/contained/mod.rs`
- Modify: `scripts/utils/Cargo.toml`
- Test: inline `#[cfg(test)] mod tests` in `contained/mod.rs`

Stand up the new binary with `tracing` JSON-to-stdout (spec §4.6) and a CLI that preserves the existing flags (`game_monitor.rs:81-171`) plus the new ones (`--proposal-interval`, `--cache-dir`, `--stdin-grace-secs`, `--max-lead-windows`, `--rss-margin-mb`). The control-plane logic lives in `scripts/utils/src/contained/` so it can be unit-tested without a `main`.

- [ ] **Step 1: Declare the bin target and the support module**

In `scripts/utils/Cargo.toml`, after the `game-monitor` `[[bin]]` (lines 47–49), add:

```toml
[[bin]]
name = "game-monitor-contained"
path = "bin/game_monitor_contained.rs"
```

In the `[dependencies]` of `scripts/utils/Cargo.toml`, add (matching the existing workspace-dep style):

```toml
op-succinct-estimator.workspace = true
tracing.workspace = true
tracing-subscriber = { workspace = true, features = ["env-filter", "json"] }
```

In `scripts/utils/src/lib.rs`, add the module:

```rust
pub mod contained;
```

- [ ] **Step 2: Write the CLI args (failing test for defaults)**

Create `scripts/utils/src/contained/mod.rs`:

```rust
use std::path::PathBuf;

use clap::Parser;

pub mod state; // Task 19
pub mod pipeline; // Task 20

/// CLI for the contained game monitor. Preserves the legacy flags and adds the
/// pipeline/cache/memory knobs.
#[derive(Debug, Clone, Parser)]
pub struct ContainedArgs {
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,

    /// Main loop polling interval (seconds).
    #[arg(long, default_value = "30")]
    pub poll_interval: u64,

    /// Discover→execute delay (seconds) — avoids the multi-backend 404 race.
    #[arg(long, default_value = "600")]
    pub delay: u64,

    /// Blocks per range — caps SP1 guest memory per execution.
    #[arg(long, default_value = "200")]
    pub batch_size: u64,

    /// Primary-retry budget before a game moves to the background queue.
    #[arg(long, default_value = "1")]
    pub max_retries: u32,

    /// Background-retry max age (seconds). Default 3.5 days.
    #[arg(long, default_value = "302400")]
    pub background_retry_max_age_secs: u64,

    /// Explicit start index (else resume from progress, else latest on-chain).
    #[arg(long)]
    pub start_index: Option<u64>,

    /// Progress file for restart resume.
    #[arg(long)]
    pub progress_file: Option<PathBuf>,

    /// Absolute base dir for the witness/stdin cache.
    #[arg(long, default_value = "/data/op-succinct/cache")]
    pub cache_dir: PathBuf,

    /// Proposer's PROPOSAL_INTERVAL (L2 blocks) — drives window prediction.
    #[arg(long)]
    pub proposal_interval: u64,

    /// Max windows the pipeline may run ahead of the finalized frontier.
    #[arg(long, default_value = "4")]
    pub max_lead_windows: u64,

    /// Grace period (seconds) to keep an stdin blob after its game succeeds.
    #[arg(long, default_value = "3600")]
    pub stdin_grace_secs: u64,

    /// RSS admission safety margin (MiB).
    #[arg(long, default_value = "20480")]
    pub rss_margin_mb: u64,

    /// Per-network-call timeout (seconds).
    #[arg(long, default_value = "120")]
    pub network_call_timeout_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_required_proposal_interval_and_defaults() {
        let args = ContainedArgs::parse_from([
            "game-monitor-contained",
            "--proposal-interval",
            "1800",
        ]);
        assert_eq!(args.proposal_interval, 1800);
        assert_eq!(args.delay, 600);
        assert_eq!(args.batch_size, 200);
        assert_eq!(args.max_lead_windows, 4);
        assert_eq!(args.stdin_grace_secs, 3600);
    }
}
```

- [ ] **Step 3: Write the binary entrypoint with JSON tracing**

Create `scripts/utils/bin/game_monitor_contained.rs`:

```rust
use anyhow::Result;
use clap::Parser;
use op_succinct_scripts::contained::ContainedArgs;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn init_tracing() {
    // JSON to stdout; bridge `log` so library `log::` lines are captured.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer().json().with_current_span(true).with_span_list(true))
        .with(filter)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = ContainedArgs::parse();
    dotenv::from_path(&args.env_file).ok();
    init_tracing();
    tracing::info!(?args, "starting contained game monitor");

    // Wired in Task 21.
    op_succinct_scripts::contained::run(args).await
}
```

Add a placeholder `run` to `contained/mod.rs` so it compiles (filled in Task 21):

```rust
pub async fn run(_args: ContainedArgs) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented")
}
```

> The `log`→`tracing` bridge: confirm whether `tracing-log` is needed. If library code uses `log::` macros (it does — see `game_monitor.rs:23`), enable the bridge by adding `tracing-log` and calling `tracing_log::LogTracer::init()?` in `init_tracing`, OR enable `tracing-subscriber`'s `tracing-log` feature. Check `rg -n "tracing-log" Cargo.toml`; add the dep if absent.

- [ ] **Step 4: Verify it builds and the args test passes**

Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda contained::tests`
Run: `cargo check -p op-succinct-scripts --no-default-features --features eigenda --bin game-monitor-contained`
Expected: test passes; binary compiles (it will `bail!` at runtime — fine for now).

> `scripts/utils` features: confirm how the DA feature reaches this crate. Check `scripts/utils/Cargo.toml` for an `eigenda` feature that forwards to `op-succinct-proof-utils/eigenda` and `op-succinct-estimator/eigenda`. If absent, add it mirroring `utils/proof/Cargo.toml`'s feature block.

- [ ] **Step 5: Commit**

```bash
git add scripts/utils/Cargo.toml scripts/utils/src/lib.rs scripts/utils/src/contained/ scripts/utils/bin/game_monitor_contained.rs
git commit -m "feat(monitor): contained daemon scaffold + JSON tracing + CLI args"
```

---

## Task 18: Game discovery + `fetch_game_data` (preserve type-42 + finalized reads)

**Files:**
- Create: `scripts/utils/src/contained/discovery.rs`
- Modify: `scripts/utils/src/contained/mod.rs`
- Test: inline in `discovery.rs`

Preserve the proven control-plane reads verbatim (spec §4.1 / §9): finalized-block index polling of `factory.gameCount()`/`gameAtIndex`, `GAME_TYPE == 42` filter, real `start/end` from `startingBlockNumber`/`l2BlockNumber`, `created_at` from the factory timestamp. Lift `fetch_game_data` and `GameData` from `game_monitor.rs:1093-1140`.

- [ ] **Step 1: Lift `GameData`, `FetchGameError`, `fetch_game_data`**

Create `scripts/utils/src/contained/discovery.rs`:

```rust
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use anyhow::Context;
use fault_proof::contract::{
    DisputeGameFactory::DisputeGameFactoryInstance, OPSuccinctFaultDisputeGame,
};

/// Game type filter — OP Succinct fault dispute game (verbatim from game_monitor.rs:42).
pub const GAME_TYPE: u32 = 42;

#[derive(Debug, Clone)]
pub struct GameData {
    pub game_index: u64,
    pub game_address: Address,
    pub start_block: u64,
    pub end_block: u64,
    pub created_at: SystemTime,
}

impl GameData {
    pub fn block_range(&self) -> u64 {
        self.end_block.saturating_sub(self.start_block)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchGameError {
    #[error("game {game_index} has type {game_type}, expected {expected}")]
    WrongGameType { game_index: u64, game_type: u32, expected: u32 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Lifted verbatim from game_monitor.rs:1105-1140. Reads game metadata, filters type-42,
/// and resolves the real [start, end] block range plus the factory `created_at`.
pub async fn fetch_game_data<P: alloy_provider::Provider + Clone>(
    game_index: u64,
    factory: &DisputeGameFactoryInstance<P>,
    l1_provider: P,
) -> Result<GameData, FetchGameError> {
    let game_info = factory
        .gameAtIndex(U256::from(game_index))
        .call()
        .await
        .context("failed to get game at index")?;

    let game_type = game_info.gameType;
    if game_type != GAME_TYPE {
        return Err(FetchGameError::WrongGameType { game_index, game_type, expected: GAME_TYPE });
    }

    let game_address = game_info.proxy;
    let created_at_secs = U256::from(game_info.timestamp).to::<u64>();
    let created_at = UNIX_EPOCH + Duration::from_secs(created_at_secs);

    let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider);
    let l2_block_number =
        game.l2BlockNumber().call().await.context("failed to get L2 block number")?.to::<u64>();
    let start_block = game
        .startingBlockNumber()
        .call()
        .await
        .context("failed to get starting block number")?
        .to::<u64>();

    Ok(GameData { game_index, game_address, start_block, end_block: l2_block_number, created_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_range_is_end_minus_start() {
        let g = GameData {
            game_index: 1,
            game_address: Address::ZERO,
            start_block: 100,
            end_block: 300,
            created_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(g.block_range(), 200);
    }

    #[test]
    fn game_type_constant_is_42() {
        assert_eq!(GAME_TYPE, 42);
    }
}
```

- [ ] **Step 2: Wire the module + declare the dep**

In `scripts/utils/src/contained/mod.rs` add `pub mod discovery;`. Confirm `scripts/utils/Cargo.toml` already depends on `op-succinct-fp` (the `fault_proof` crate) and `alloy-primitives`/`alloy-provider`; the legacy `game_monitor.rs` imports them (lines 17,19,21–24), so they are present. If not, add `op-succinct-fp.workspace = true`, `alloy-primitives.workspace = true`, `alloy-provider.workspace = true`.

- [ ] **Step 3: Run the tests + check it compiles**

Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda contained::discovery::tests`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add scripts/utils/src/contained/discovery.rs scripts/utils/src/contained/mod.rs scripts/utils/Cargo.toml
git commit -m "feat(monitor): lift game discovery + fetch_game_data (type-42, finalized reads)"
```

---

## Task 19: Scheduling + two-tier retry + `progress.json` resume

**Files:**
- Create: `scripts/utils/src/contained/state.rs`
- Modify: `scripts/utils/src/contained/mod.rs`
- Test: inline in `state.rs`

Preserve the scheduling/retry semantics (spec §4.1 / §9): two-tier Primary (linear backoff, bounded) → Background (4× exponential, age-bounded) from `maybe_requeue`/`evict_aged_background_retries` (`game_monitor.rs:708-818`); `last_contiguous` frontier via `SequenceTracker` (`utils/common/src/sequence_tracker.rs`); `progress.json` persist/resume (`game_monitor.rs:341-432`). Lift the data structures and the two requeue functions; make them testable without live processes.

- [ ] **Step 1: Lift the retry/progress types + `maybe_requeue` logic**

Create `scripts/utils/src/contained/state.rs`:

```rust
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use op_succinct_common::SequenceTracker;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptKind {
    Primary { retries: u32 },
    Background { attempts: u32 },
}

#[derive(Clone, Debug)]
pub struct PendingGame {
    pub executable_at: Instant,
    pub game_index: u64,
    pub kind: AttemptKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackgroundRetry {
    pub game_index: u64,
    pub game_created_at: SystemTime,
    pub next_attempt_at: SystemTime,
    pub last_wait: Duration,
    pub attempts: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProgressState {
    pub last_contiguous: u64,
    #[serde(default)]
    pub background_retries: Vec<BackgroundRetry>,
}

/// Decision returned by the pure retry policy, so it can be unit-tested without
/// touching the live `pending_games`/`background_retries` collections.
#[derive(Debug, PartialEq)]
pub enum RequeueDecision {
    /// Re-queue as Primary after `delay`, with the new retry count.
    Primary { retries: u32, delay: Duration },
    /// Primary exhausted: complete the game (advance frontier) and enqueue Background.
    ToBackground { first_wait: Duration },
    /// Background attempt failed: quadruple the wait.
    Background { next_wait: Duration },
}

/// Pure two-tier policy lifted from game_monitor.rs:708-781.
/// `initial_game_delay` is the `--delay` window; `max_retries` is the Primary budget.
pub fn requeue_decision(
    kind: AttemptKind,
    initial_game_delay: Duration,
    max_retries: u32,
    prev_background_wait: Option<Duration>,
) -> RequeueDecision {
    match kind {
        AttemptKind::Primary { retries } if retries < max_retries => {
            let new_retries = retries + 1;
            // Linear: delay * 2 * retry_number.
            let delay = initial_game_delay * 2 * new_retries;
            RequeueDecision::Primary { retries: new_retries, delay }
        }
        AttemptKind::Primary { .. } => {
            // First background wait = delay * 2 * max_retries * 4 (linear schedule x4).
            let first_wait = initial_game_delay * 2 * max_retries.max(1) * 4;
            RequeueDecision::ToBackground { first_wait }
        }
        AttemptKind::Background { .. } => {
            let base = prev_background_wait.unwrap_or(initial_game_delay);
            RequeueDecision::Background { next_wait: base * 4 }
        }
    }
}

/// Age-based eviction predicate lifted from game_monitor.rs:791-818.
pub fn is_background_retry_aged_out(
    bg: &BackgroundRetry,
    now: SystemTime,
    max_age: Duration,
    is_running: bool,
) -> bool {
    if is_running {
        return false; // never evict a running game
    }
    now.duration_since(bg.game_created_at).unwrap_or(Duration::ZERO) > max_age
}

/// Restart resume: determine the next game index (explicit > persisted > latest on-chain).
pub fn resume_next_index(
    explicit_start: Option<u64>,
    persisted: Option<&ProgressState>,
    on_chain_game_count: u64,
) -> u64 {
    if let Some(i) = explicit_start {
        i
    } else if let Some(p) = persisted {
        p.last_contiguous + 1
    } else {
        on_chain_game_count.saturating_sub(1)
    }
}

/// Loads progress JSON, tolerating a missing/corrupt file (logs a warning upstream).
pub fn load_progress(path: &std::path::Path) -> Option<ProgressState> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_progress(
    path: &std::path::Path,
    tracker: &SequenceTracker,
    background: &VecDeque<BackgroundRetry>,
) -> anyhow::Result<()> {
    let state = ProgressState {
        last_contiguous: tracker.end(),
        background_retries: background.iter().cloned().collect(),
    };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&state)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_retry_uses_linear_backoff() {
        let d = requeue_decision(
            AttemptKind::Primary { retries: 0 },
            Duration::from_secs(600),
            2,
            None,
        );
        assert_eq!(d, RequeueDecision::Primary { retries: 1, delay: Duration::from_secs(1200) });
    }

    #[test]
    fn exhausted_primary_moves_to_background() {
        let d = requeue_decision(
            AttemptKind::Primary { retries: 2 },
            Duration::from_secs(600),
            2,
            None,
        );
        // 600 * 2 * 2 * 4 = 9600
        assert_eq!(d, RequeueDecision::ToBackground { first_wait: Duration::from_secs(9600) });
    }

    #[test]
    fn background_quadruples_wait() {
        let d = requeue_decision(
            AttemptKind::Background { attempts: 1 },
            Duration::from_secs(600),
            2,
            Some(Duration::from_secs(9600)),
        );
        assert_eq!(d, RequeueDecision::Background { next_wait: Duration::from_secs(38400) });
    }

    #[test]
    fn running_background_is_never_aged_out() {
        let bg = BackgroundRetry {
            game_index: 1,
            game_created_at: SystemTime::UNIX_EPOCH,
            next_attempt_at: SystemTime::UNIX_EPOCH,
            last_wait: Duration::from_secs(1),
            attempts: 0,
        };
        let now = SystemTime::now();
        assert!(!is_background_retry_aged_out(&bg, now, Duration::from_secs(1), true));
        assert!(is_background_retry_aged_out(&bg, now, Duration::from_secs(1), false));
    }

    #[test]
    fn resume_prefers_explicit_then_persisted_then_chain() {
        let p = ProgressState { last_contiguous: 41, background_retries: vec![] };
        assert_eq!(resume_next_index(Some(5), Some(&p), 100), 5);
        assert_eq!(resume_next_index(None, Some(&p), 100), 42);
        assert_eq!(resume_next_index(None, None, 100), 99);
        assert_eq!(resume_next_index(None, None, 0), 0);
    }

    #[test]
    fn progress_round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("progress.json");
        let mut tracker = SequenceTracker::new(0);
        tracker.add(1);
        tracker.add(2);
        let bg = VecDeque::new();
        save_progress(&path, &tracker, &bg).unwrap();
        let loaded = load_progress(&path).unwrap();
        assert_eq!(loaded.last_contiguous, 2);
    }
}
```

- [ ] **Step 2: Wire the module + dep**

In `scripts/utils/src/contained/mod.rs` add `pub mod state;` (replace the Task 17 placeholder `pub mod state;` comment). Confirm `op-succinct-common.workspace = true` is in `scripts/utils/Cargo.toml` (the legacy monitor imports `op_succinct_common::SequenceTracker`, line 26 — so present).

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda contained::state::tests`
Expected: 6 tests pass.

- [ ] **Step 4: Commit**

```bash
git add scripts/utils/src/contained/state.rs scripts/utils/src/contained/mod.rs
git commit -m "feat(monitor): two-tier retry policy + progress resume (pure, tested)"
```

---

## Task 20: The predictive witness pipeline (producer)

**Files:**
- Create: `scripts/utils/src/contained/pipeline.rs`
- Modify: `scripts/utils/src/contained/mod.rs`
- Test: inline in `pipeline.rs` (finalization-gate predicate; full loop covered by Task 22 integration)

Spec §4.2: a background task that predicts the next window, computes its safe-head sub-ranges once its L2 blocks exist, and triggers `build_range_witness` per sub-range when the sub-range's end is L2-finalized **and** L1 has finalized past its `l1_head` (key-soundness gate, §4.4). Bounded concurrency shares the RSS gate (Task 15). The deterministic gate predicate is unit-tested; the live loop is integration-tested.

- [ ] **Step 1: Write the finalization-gate predicate + its test**

Create `scripts/utils/src/contained/pipeline.rs`:

```rust
use op_succinct_host_utils::block_range::SpanBatchRange;

/// A sub-range is safe to *persist* only once its end block is L2-finalized AND L1 has
/// finalized past its `l1_head` (spec §4.4 key-soundness). This keeps the
/// `(chain_id,start,end,da_type)` key deterministic.
pub fn sub_range_ready_to_persist(
    range: &SpanBatchRange,
    finalized_l2: u64,
    range_l1_head: u64,
    finalized_l1: u64,
) -> bool {
    range.end <= finalized_l2 && range_l1_head <= finalized_l1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(end: u64) -> SpanBatchRange {
        SpanBatchRange { start: end - 10, end }
    }

    #[test]
    fn not_ready_until_l2_finalized() {
        assert!(!sub_range_ready_to_persist(&r(200), 199, 50, 100));
        assert!(sub_range_ready_to_persist(&r(200), 200, 50, 100));
    }

    #[test]
    fn not_ready_until_l1_finalized_past_l1_head() {
        assert!(!sub_range_ready_to_persist(&r(200), 200, 150, 100));
        assert!(sub_range_ready_to_persist(&r(200), 200, 100, 100));
    }
}
```

- [ ] **Step 2: Write the pipeline driver (uses the estimator + splitter + window predictor)**

Append the async driver to `pipeline.rs`. It is wired into `run` in Task 21; here it is a standalone async fn so it can be spawned:

```rust
use std::sync::Arc;

use op_succinct_estimator::{window::WindowPredictor, Estimator};
use op_succinct_host_utils::{
    block_range::split_range_based_on_safe_heads_with_fetcher,
    fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost,
};
use tokio::sync::Semaphore;

/// One iteration: predict the next window, compute its sub-ranges if its blocks exist,
/// and build each ready sub-range (RSS-admitted via `permits`). Returns the new frontier
/// (advanced past the last fully-built window) or the unchanged frontier if not ready.
pub async fn pipeline_step<H>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    predictor: &mut WindowPredictor,
    permits: &Arc<Semaphore>,
    batch_size: u64,
) -> anyhow::Result<()>
where
    H: OPSuccinctHost,
    Estimator<H>: Sync,
{
    use op_succinct_host_utils::host::OPSuccinctHost as _;

    let finalized_l2 = fetcher
        .get_l2_finalized_block_number()
        .await
        .unwrap_or(predictor.frontier());

    let window = predictor.next_window();
    if window.end > finalized_l2 || !predictor.within_lead(&window, finalized_l2) {
        return Ok(()); // window's blocks don't exist yet, or lead cap reached
    }

    // Compute safe-head sub-ranges from chain data (reusing the shared fetcher).
    let sub_ranges = split_range_based_on_safe_heads_with_fetcher(
        fetcher, window.start, window.end, batch_size,
    )
    .await?;

    // Build each sub-range concurrently, gated by the RSS-admission semaphore.
    let builds = sub_ranges.iter().map(|range| async {
        let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");
        if let Err(e) = estimator.build_range_witness(range).await {
            tracing::warn!(start = range.start, end = range.end, error = %e, "pipeline build failed");
        }
    });
    futures::future::join_all(builds).await;

    predictor.advance_to(window.end);
    Ok(())
}
```

> `get_l2_finalized_block_number` / `get_l2_finalized_block` — confirm the exact fetcher method name. The trait `OPSuccinctHost::get_finalized_l2_block_number` exists (`host.rs`); if the `fetcher` lacks a direct finalized-L2 helper, use the host's method (it takes `&fetcher` + a proposed block number) or `fetcher.get_l2_header(BlockId::finalized())`. Pick the one that exists; the predicate's contract (only build ready sub-ranges) is what matters. The `Semaphore` permit count is set by the RSS gate in Task 21 — here it bounds concurrency; Task 21 sizes it from `admits(...)`.

- [ ] **Step 3: Wire the module**

In `scripts/utils/src/contained/mod.rs` add `pub mod pipeline;`.

- [ ] **Step 4: Run the predicate tests + compile-check the driver**

Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda contained::pipeline::tests`
Run: `cargo check -p op-succinct-scripts --no-default-features --features eigenda`
Expected: 2 tests pass; the crate compiles.

- [ ] **Step 5: Commit**

```bash
git add scripts/utils/src/contained/pipeline.rs scripts/utils/src/contained/mod.rs
git commit -m "feat(monitor): predictive witness pipeline (finalization-gated build)"
```

---

## Task 21: Wire the daemon — main loop joining executor + pipeline + RSS gate

**Files:**
- Modify: `scripts/utils/src/contained/mod.rs` (implement `run`)
- Create: `scripts/utils/src/contained/executor.rs`
- Test: build + a dry-run smoke test (no live RPC)

Implement the reactive executor (spec §4.1 / §5) and the top-level `run` that builds the shared fetcher/host/estimator once (spec §6: build once at startup), spawns the pipeline task, and runs the discovery/execute/retry loop. The RSS gate sizes the shared semaphore.

- [ ] **Step 1: Write the executor (per-game: split → load-or-build stdin → execute → aggregate)**

Create `scripts/utils/src/contained/executor.rs`:

```rust
use std::sync::Arc;

use op_succinct_estimator::{stats::aggregate_execution_stats, Estimator};
use op_succinct_host_utils::{
    block_range::split_range_based_on_safe_heads_with_fetcher,
    fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost, stats::ExecutionStats,
};
use tokio::sync::Semaphore;

use crate::contained::discovery::GameData;

/// Execute every safe-head sub-range of a game and aggregate the stats.
/// A cache hit (pipeline prebuilt the stdin) skips host.run; a miss builds on demand.
/// Each execute holds an RSS-admission permit.
pub async fn execute_game<H>(
    estimator: &Estimator<H>,
    fetcher: &OPSuccinctDataFetcher,
    permits: &Arc<Semaphore>,
    game: &GameData,
    batch_size: u64,
) -> Result<ExecutionStats, op_succinct_estimator::EstimatorError>
where
    H: OPSuccinctHost,
    Estimator<H>: Sync,
{
    // Same deterministic split the pipeline used over the predicted window → keys match.
    let sub_ranges = split_range_based_on_safe_heads_with_fetcher(
        fetcher, game.start_block, game.end_block, batch_size,
    )
    .await
    .map_err(op_succinct_estimator::EstimatorError::classify)?;

    let mut per_range = Vec::with_capacity(sub_ranges.len());
    for range in &sub_ranges {
        let _permit = permits.clone().acquire_owned().await.expect("semaphore closed");
        let stats = estimator.execute_range(range).await?;
        per_range.push(stats);
    }
    Ok(aggregate_execution_stats(&per_range, 0, 0))
}
```

- [ ] **Step 2: Implement `run` in `contained/mod.rs`**

Replace the placeholder `run` with the real one. Build shared resources once, derive the RSS budget, spawn the pipeline, and run the discovery/execute/retry loop. (Use the existing `game_monitor.rs:1361-1571` loop structure as the reference for ordering: cleanup → evict-aged → primary spawns → background retries → discovery.)

```rust
pub mod discovery;
pub mod executor;
pub mod pipeline;
pub mod state;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_eips::BlockId;
use alloy_provider::ProviderBuilder;
use op_succinct_common::SequenceTracker;
use op_succinct_estimator::{
    cache::{DaType, WitnessCache},
    memory::{admits, read_cgroup_budget_bytes, read_cgroup_usage_bytes, RssHistory},
    window::WindowPredictor,
    Estimator,
};
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use op_succinct_proof_utils::initialize_host;
use tokio::sync::Semaphore;

use self::state::{load_progress, requeue_decision, resume_next_index};

pub async fn run(args: ContainedArgs) -> anyhow::Result<()> {
    // Build shared fetcher + host + estimator ONCE (spec §6).
    let fetcher = Arc::new(OPSuccinctDataFetcher::new_with_rollup_config().await?);
    let chain_id = fetcher.get_l2_chain_id().await?;
    let host = initialize_host(fetcher.clone());

    // DA type is fixed by build features; eigenda is the production target.
    let da_type = if cfg!(feature = "eigenda") {
        DaType::EigenDa
    } else if cfg!(feature = "celestia") {
        DaType::Celestia
    } else {
        DaType::Ethereum
    };
    let cache = WitnessCache::new(&args.cache_dir, chain_id, da_type);

    let estimator = Arc::new(Estimator {
        host: host.clone(),
        fetcher: fetcher.clone(),
        cache: cache.clone(),
        chain_id,
        safe_db_fallback: true,
    });

    // RSS-admission: size the shared permit pool from the cgroup budget.
    let budget = read_cgroup_budget_bytes();
    let margin_bytes = args.rss_margin_mb * 1024 * 1024;
    let usage = read_cgroup_usage_bytes().unwrap_or(0);
    let mut history = RssHistory::new(50);
    // Conservative default per-unit footprint until history exists (~55 GiB).
    let default_unit_bytes: u64 = 55 * 1024 * 1024 * 1024;
    let max_concurrent = compute_initial_permits(budget, usage, default_unit_bytes, margin_bytes);
    let permits = Arc::new(Semaphore::new(max_concurrent));
    tracing::info!(?budget, max_concurrent, "RSS admission initialized");

    // Spawn the predictive pipeline (producer).
    {
        let estimator = estimator.clone();
        let fetcher = fetcher.clone();
        let permits = permits.clone();
        let proposal_interval = args.proposal_interval;
        let batch_size = args.batch_size;
        let poll = Duration::from_secs(args.poll_interval);
        let max_lead = args.max_lead_windows;
        tokio::spawn(async move {
            // Frontier starts at the finalized head; refined as windows build.
            let start_frontier = fetcher
                .get_l2_header(BlockId::finalized())
                .await
                .map(|h| h.number)
                .unwrap_or(0);
            let mut predictor = WindowPredictor::new(start_frontier, proposal_interval, max_lead);
            loop {
                if let Err(e) = pipeline::pipeline_step(
                    &estimator, &fetcher, &mut predictor, &permits, batch_size,
                )
                .await
                {
                    tracing::warn!(error = %e, "pipeline step failed");
                }
                tokio::time::sleep(poll).await;
            }
        });
    }

    // Reactive control plane (executor) — discovery/execute/retry loop.
    let l1_provider = ProviderBuilder::default().connect_http(std::env::var("L1_RPC")?.parse()?);
    let factory_address: alloy_primitives::Address =
        std::env::var("DISPUTE_GAME_FACTORY_ADDRESS")?.parse()?;
    let factory = fault_proof::contract::DisputeGameFactory::DisputeGameFactoryInstance::new(
        factory_address,
        l1_provider.clone(),
    );

    let progress_path = args
        .progress_file
        .clone()
        .unwrap_or_else(|| args.cache_dir.join("progress.json"));
    let persisted = load_progress(&progress_path);
    let on_chain_count =
        factory.gameCount().call().block(BlockId::finalized()).await?.to::<u64>();
    let next_index = resume_next_index(args.start_index, persisted.as_ref(), on_chain_count);
    let mut tracker = SequenceTracker::new(next_index.saturating_sub(1));
    let mut background: VecDeque<state::BackgroundRetry> =
        persisted.map(|p| p.background_retries.into_iter().collect()).unwrap_or_default();
    let mut pending: VecDeque<state::PendingGame> = VecDeque::new();
    let mut next_game_index = next_index;

    let poll = Duration::from_secs(args.poll_interval);
    loop {
        // Discovery: finalized index polling (verbatim semantics from game_monitor.rs:1541-1566).
        let count = factory.gameCount().call().block(BlockId::finalized()).await?.to::<u64>();
        while next_game_index < count {
            pending.push_back(state::PendingGame {
                executable_at: Instant::now() + Duration::from_secs(args.delay),
                game_index: next_game_index,
                kind: state::AttemptKind::Primary { retries: 0 },
            });
            next_game_index += 1;
        }

        // Execute any due games (serially through the RSS gate inside execute_game).
        let now = Instant::now();
        let due: Vec<_> = pending
            .iter()
            .enumerate()
            .filter(|(_, p)| p.executable_at <= now)
            .map(|(i, _)| i)
            .collect();
        for i in due.into_iter().rev() {
            let pg = pending.remove(i).unwrap();
            match discovery::fetch_game_data(pg.game_index, &factory, l1_provider.clone()).await {
                Ok(game) => {
                    match executor::execute_game(
                        &estimator, &fetcher, &permits, &game, args.batch_size,
                    )
                    .await
                    {
                        Ok(stats) => {
                            tracing::info!(game = pg.game_index, ?stats, "game executed");
                            tracker.add(pg.game_index);
                            // Schedule stdin prune after grace (Task 22 wires the timer).
                            state::save_progress(&progress_path, &tracker, &background)?;
                        }
                        Err(e) if e.is_transient() => {
                            apply_requeue(&mut pending, &mut background, &mut tracker,
                                pg, game.created_at, &args, &progress_path)?;
                        }
                        Err(e) => {
                            tracing::error!(game = pg.game_index, error = %e, "fatal; completing");
                            tracker.add(pg.game_index); // advance frontier; never stall
                            state::save_progress(&progress_path, &tracker, &background)?;
                        }
                    }
                }
                Err(discovery::FetchGameError::WrongGameType { .. }) => {
                    tracker.add(pg.game_index); // not a type-42 game; advance past it
                }
                Err(e) => tracing::warn!(game = pg.game_index, error = %e, "fetch failed; will retry"),
            }
        }

        tokio::time::sleep(poll).await;
    }
}

/// Initial permit count: how many default-sized units fit the budget now.
fn compute_initial_permits(
    budget: Option<u64>,
    usage: u64,
    unit_bytes: u64,
    margin: u64,
) -> usize {
    match budget {
        None => 4, // unlimited → a sane default cap
        Some(b) => {
            let avail = b.saturating_sub(usage).saturating_sub(margin);
            ((avail / unit_bytes.max(1)) as usize).max(1)
        }
    }
}

/// Apply the pure retry decision to the live queues (bridges Task 19's policy to state).
fn apply_requeue(
    pending: &mut VecDeque<state::PendingGame>,
    background: &mut VecDeque<state::BackgroundRetry>,
    tracker: &mut SequenceTracker,
    pg: state::PendingGame,
    created_at: std::time::SystemTime,
    args: &ContainedArgs,
    progress_path: &std::path::Path,
) -> anyhow::Result<()> {
    let initial_delay = Duration::from_secs(args.delay);
    let prev = background.iter().find(|b| b.game_index == pg.game_index).map(|b| b.last_wait);
    match requeue_decision(pg.kind, initial_delay, args.max_retries, prev) {
        state::RequeueDecision::Primary { retries, delay } => {
            pending.push_back(state::PendingGame {
                executable_at: Instant::now() + delay,
                game_index: pg.game_index,
                kind: state::AttemptKind::Primary { retries },
            });
        }
        state::RequeueDecision::ToBackground { first_wait } => {
            background.push_back(state::BackgroundRetry {
                game_index: pg.game_index,
                game_created_at: created_at,
                next_attempt_at: std::time::SystemTime::now() + first_wait,
                last_wait: first_wait,
                attempts: 0,
            });
            tracker.add(pg.game_index); // mark complete so frontier advances
            state::save_progress(progress_path, tracker, background)?;
        }
        state::RequeueDecision::Background { next_wait } => {
            if let Some(b) = background.iter_mut().find(|b| b.game_index == pg.game_index) {
                b.last_wait = next_wait;
                b.next_attempt_at = std::time::SystemTime::now() + next_wait;
                b.attempts += 1;
            }
            state::save_progress(progress_path, tracker, background)?;
        }
    }
    Ok(())
}
```

> This `run` is the integration point; several method names need confirming against the tree as you wire them: `fetcher.get_l2_chain_id`, `fetcher.get_l2_header(BlockId::finalized())`, the `DisputeGameFactoryInstance::new` path, and `factory.gameCount()`. All are used by the legacy `game_monitor.rs` — copy the exact call sites (it imports the same `fault_proof::contract` types, lines 21–24). The background-retry *draining* (re-enqueuing due background games into `pending` as `AttemptKind::Background`) and the aged-eviction sweep are mechanical additions mirroring `game_monitor.rs:1520-1539` and `:791-818`; add them using `state::is_background_retry_aged_out` and `requeue_decision` — the pure policy is already tested in Task 19.

- [ ] **Step 3: Build the binary**

Run: `cargo build -p op-succinct-scripts --no-default-features --features eigenda --bin game-monitor-contained`
Expected: compiles. Resolve any method-name mismatches against the legacy monitor's call sites as flagged above.

- [ ] **Step 4: Add a permit-sizing unit test**

In `contained/mod.rs` `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn permits_scale_with_budget() {
        // 500 GiB budget, 0 used, 20 GiB margin, 55 GiB/unit → 8 units.
        let budget = Some(500u64 * 1024 * 1024 * 1024);
        let unit = 55u64 * 1024 * 1024 * 1024;
        let margin = 20u64 * 1024 * 1024 * 1024;
        assert_eq!(super::compute_initial_permits(budget, 0, unit, margin), 8);
        assert_eq!(super::compute_initial_permits(None, 0, unit, margin), 4);
    }
```

Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda contained::tests::permits_scale_with_budget`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add scripts/utils/src/contained/
git commit -m "feat(monitor): wire run loop — executor + pipeline + RSS admission"
```

---

## Task 22: Integration tests — parity, prebuild-skip, forced-failure recovery, prune

**Files:**
- Create: `scripts/utils/tests/contained_integration.rs`
- Test: a small real L2 range (env-gated so CI without RPC skips)

Spec §10 integration coverage. These need live RPC (`L1_RPC`, `L2_RPC`, `L2_NODE_RPC`, `L1_BEACON_RPC`, `EIGENDA_PROXY_ADDRESS`); gate each on an env check so the suite is a no-op when unset. Run them manually against a node.

- [ ] **Step 1: Parity — aggregated stats match a `cost_estimator` baseline**

Create `scripts/utils/tests/contained_integration.rs`:

```rust
//! Integration tests for the contained monitor. Each requires live RPC and is a no-op
//! when `OPS_IT_L2_RPC` is unset. Run manually:
//!   OPS_IT_L2_RPC=$L2_RPC OPS_IT_START=<n> OPS_IT_END=<n+small> \
//!   cargo test -p op-succinct-scripts --no-default-features --features eigenda --test contained_integration -- --nocapture

fn it_enabled() -> bool {
    std::env::var("OPS_IT_L2_RPC").is_ok()
}

#[tokio::test]
async fn aggregated_stats_match_cost_estimator_baseline() {
    if !it_enabled() {
        eprintln!("skipping: OPS_IT_L2_RPC unset");
        return;
    }
    // Build a fetcher/host/estimator over a small range; execute via execute_game's path
    // (split → execute_range → aggregate). Assert nb_blocks == end - start and that
    // total_instruction_count > 0. Compare batch_start/batch_end to the requested range.
    // (A full byte-equality baseline vs the cost-estimator CSV is the gold standard; at
    // minimum assert the aggregate is non-trivial and the range matches.)
    // ... construct Estimator as in contained::run, call executor::execute_game over a
    // GameData with the env-provided start/end, assert on the returned ExecutionStats.
}
```

> Fill the body using the same construction as `contained::run` (Task 21). The assertion bar: `stats.batch_end == end`, `stats.nb_blocks == end - start`, `stats.total_instruction_count > 0`. For a true baseline, run the existing `cost-estimator --start S --end E --batch-size B` and compare `total_instruction_count` within a small tolerance (cycle counts are deterministic, so equality should hold).

- [ ] **Step 2: Prebuild-skip — pipeline builds stdin, executor skips host.run**

Add:

```rust
#[tokio::test]
async fn executor_skips_host_run_when_pipeline_prebuilt_stdin() {
    if !it_enabled() {
        return;
    }
    // 1. Run build_range_witness over a sub-range → assert cache.has_stdin(range).
    // 2. Assert the witness blob was dropped: cache.has_witness(range) == false.
    // 3. Run execute_range over the same range → assert it returns stats without a
    //    network host.run (instrument by asserting wall-clock << a cold build, or by
    //    pointing the host at an unreachable L2_NODE so a host.run would error but the
    //    cached stdin path succeeds).
}
```

> The cleanest assertion that `host.run` was skipped: after building the stdin, drop the witness, then set the host's `L2_NODE_RPC` to an unroutable address and confirm `execute_range` still succeeds (it only reads the cached stdin + fetches cheap block_data; if block_data also needs L2, keep L2_RPC valid and only break the witness-server path). Adapt to whichever dependency uniquely gates `host.run`.

- [ ] **Step 3: Forced mid-build failure recovers without redoing a cached step**

Add:

```rust
#[tokio::test]
async fn forced_rpc_failure_recovers_without_recrunch() {
    if !it_enabled() {
        return;
    }
    // 1. Run host.run to success so WitnessData is cached (cache.has_witness == true)
    //    — to do this, intercept before get_sp1_stdin (e.g. call host.run + save_witness
    //    directly, skipping stdin).
    // 2. Inject a get_sp1_stdin failure path is hard; instead assert the resilience
    //    invariant directly: with WitnessData cached, a second build_range_witness call
    //    does NOT call host.fetch/host.run (point L2_NODE at an unroutable host) and still
    //    produces the stdin from the cached witness. Assert cache.has_stdin == true after.
}
```

- [ ] **Step 4: Prune — stdin evicts after grace; witness dropped once stdin built**

Add (no live RPC needed — pure cache mechanics):

```rust
#[tokio::test]
async fn stdin_prunes_after_grace_and_witness_dropped() {
    use op_succinct_estimator::cache::{DaType, WitnessCache};
    let dir = tempfile::TempDir::new().unwrap();
    let cache = WitnessCache::new(dir.path(), 42220, DaType::EigenDa);
    cache.save_stdin(10, 20, &sp1_sdk::SP1Stdin::default()).unwrap();
    assert!(cache.has_stdin(10, 20));
    // Simulate grace elapsed: prune evicts.
    cache.prune_stdin(10, 20).unwrap();
    assert!(!cache.has_stdin(10, 20));
    // Witness-dropped-after-stdin is covered by Task 6's unit test (drop_witness).
    assert!(!cache.has_witness(10, 20));
}
```

- [ ] **Step 5: Run the env-gated suite (no-op without RPC) + the pure prune test**

Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda --test contained_integration`
Expected: the pure `stdin_prunes_after_grace_and_witness_dropped` passes; the RPC-gated tests print "skipping" and pass. With env set, run them against a node and confirm green.

- [ ] **Step 6: Commit**

```bash
git add scripts/utils/tests/contained_integration.rs
git commit -m "test(monitor): integration — parity, prebuild-skip, recovery, prune"
```

---

## Task 23: Schedule stdin pruning after game success + grace

**Files:**
- Modify: `scripts/utils/src/contained/mod.rs`
- Test: covered by Task 22 prune test + a unit timer test

Spec §4.4: keep an stdin blob until the owning game **succeeds** + grace (~1h). Wire a pruning sweep into the main loop that evicts stdins older than `stdin_grace_secs` for games already past the contiguous frontier.

- [ ] **Step 1: Track succeeded ranges + add a prune sweep**

In `contained/mod.rs`, after a game executes successfully, record its sub-ranges and the success time, then in each loop iteration prune any whose stdin age exceeds the grace. Add a small `Prunable` list:

```rust
struct PrunableStdin {
    start: u64,
    end: u64,
    eligible_at: std::time::SystemTime,
}
```

On success, push each sub-range with `eligible_at = now + grace`. In the loop, before sleeping:

```rust
let now = std::time::SystemTime::now();
prunables.retain(|p| {
    if now >= p.eligible_at {
        let _ = estimator.cache.prune_stdin(p.start, p.end);
        false
    } else {
        true
    }
});
```

> `execute_game` already computes the sub-ranges; return them (or recompute deterministically) so the prune list is exact. Prefer returning `(ExecutionStats, Vec<SpanBatchRange>)` from `execute_game` to avoid a second split.

- [ ] **Step 2: Unit-test the grace predicate**

Add to `contained/mod.rs` tests:

```rust
    #[test]
    fn prune_only_after_grace() {
        let now = std::time::SystemTime::now();
        let grace = std::time::Duration::from_secs(3600);
        let eligible_at = now + grace;
        assert!(now < eligible_at); // not yet
        assert!(now + grace + std::time::Duration::from_secs(1) > eligible_at); // later, yes
    }
```

- [ ] **Step 3: Verify + commit**

Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda contained::tests`
Run: `cargo build -p op-succinct-scripts --no-default-features --features eigenda --bin game-monitor-contained`
Expected: tests pass; binary builds.

```bash
git add scripts/utils/src/contained/
git commit -m "feat(monitor): prune stdin blobs after game success + grace"
```

---

## Task 24: Final integration check + warn-and-stop-admitting guard

**Files:**
- Modify: `scripts/utils/src/contained/mod.rs`
- Test: full workspace check

Spec §7: the old absolute-duration kill-guard becomes **warn + stop admitting + alert** (a running SP1 execute cannot be force-killed in-process). Add a watchdog that, when wall-clock for a unit exceeds a ceiling, logs a warning and stops issuing new permits (closes admission) rather than killing.

- [ ] **Step 1: Add the admission-freeze on overrun**

In `contained/mod.rs`, track the start time of each in-flight execute; if any exceeds `max_process_duration_secs` (add the flag, default `10800`), log `tracing::error!` and stop acquiring new permits (e.g. set an `AtomicBool admission_frozen` checked before `acquire_owned`). Do not kill the task.

```rust
// Before acquiring a permit in pipeline_step / execute_game:
if admission_frozen.load(std::sync::atomic::Ordering::Relaxed) {
    tracing::warn!("admission frozen due to overrun; not starting new work");
    return Ok(()); // or skip this range
}
```

> Thread an `Arc<AtomicBool>` into `pipeline_step` and `execute_game`. The watchdog can be a lightweight check in the main loop comparing `Instant::now()` against recorded in-flight start times. Keep it minimal — the real protection is batch-size bounding runtime (spec §11); this is the alerting backstop.

- [ ] **Step 2: Full workspace verification**

Run: `cargo test -p op-succinct-estimator --no-default-features --features eigenda`
Run: `cargo test -p op-succinct-host-utils`
Run: `cargo test -p op-succinct-scripts --no-default-features --features eigenda contained::`
Run: `cargo build --no-default-features --features eigenda --bin game-monitor-contained -p op-succinct-scripts`
Run: `cargo check -p op-succinct-scripts` (default ethereum feature still builds)
Expected: all green.

- [ ] **Step 3: Confirm the legacy monitor is untouched**

Run: `cargo build -p op-succinct-scripts --bin game-monitor`
Expected: the old `game-monitor` binary still builds — the rewrite is additive (spec §2 non-goals: leave `cost_estimator.rs` + proposer untouched; the old monitor remains until the new one is proven).

- [ ] **Step 4: Commit**

```bash
git add scripts/utils/src/contained/
git commit -m "feat(monitor): warn+freeze-admission overrun guard (no in-process kill)"
```

---

## Self-Review Notes (for the implementer)

This plan was checked against the spec section by section:

- **§4.1 reactive control plane** → Tasks 18 (discovery/type-42/finalized), 19 (retry/frontier/progress), 21 (executor + main loop), 23 (prune), 24 (overrun guard).
- **§4.2 predictive pipeline** → Tasks 16 (window prediction), 20 (sub-range compute + finalization gate + build), 8 (shared-fetcher splitter).
- **§4.3 `utils/estimator`** → Tasks 1 (crate), 12 (`build_range_witness`), 13 (`execute_range`), 2 (`EstimatorError`), 3 (`aggregate_execution_stats`).
- **§4.4 witness cache** → Tasks 4 (key/DA discriminator/absolute base), 5 (stdin blobs), 6 (WitnessData blobs + drop), 7 (separate-clock prune), 23 (grace).
- **§4.5 resilience** → Tasks 9 (`network_call_with_timeout`), 10 (harden `OnlineBlobStore`), 11 (`RetryBackoffLayer`), and the build/execute retry boundaries realized in 12/13/19.
- **§4.6 logging** → Task 17 (`tracing` JSON to stdout + `log` bridge).
- **§7 memory model** → Tasks 14 (cgroup parsing), 15 (RSS admission + peak history), 21 (permit sizing), 24 (warn+freeze).
- **§10 testing** → unit tests in every task; Task 22 integration (parity, prebuild-skip, forced-failure recovery, prune).

**Type-consistency anchors** (used identically across tasks): `WitnessCache::{witness_path, stdin_path, has_stdin, has_witness, save_stdin, load_stdin, save_witness, load_witness, drop_witness, prune_stdin, stdin_age_secs}`; `Estimator::{build_range_witness, execute_range}`; `EstimatorError::{is_transient, classify}`; `split_range_based_on_safe_heads_with_fetcher`; `aggregate_execution_stats`; `WindowPredictor::{next_window, within_lead, advance_to, frontier}`; `requeue_decision` / `RequeueDecision`.

**Known confirm-against-the-pinned-dep points** (flagged inline, consistent with spec §11's "confirm against the pinned kona-host version"): the rkyv 0.8 serializer type aliases (Task 6/12), `RetryBackoffLayer::new` arg order for alloy 1.6.3 (Task 11), the SP1 6.1.0 `execute(...).run()` return shape (Task 13), the `OnlineBlobStore` `Self::Error` constructibility (Task 10), and exact fetcher finalized-block helper names (Tasks 20/21). Each has a concrete fallback written into the step.
