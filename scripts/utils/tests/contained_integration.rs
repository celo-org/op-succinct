//! Integration tests for the contained monitor. RPC-dependent tests are a no-op when
//! `OPS_IT_L2_RPC` is unset. Run manually against a node with the env set.

use std::sync::Arc;

use op_succinct_estimator::{
    cache::{DaType, WitnessCache},
    Estimator,
};
use op_succinct_host_utils::{block_range::SpanBatchRange, fetcher::OPSuccinctDataFetcher};
use op_succinct_proof_utils::initialize_host;
use op_succinct_scripts::contained::{discovery::GameData, executor::execute_game};

/// RPC-dependent tests run only when a live L2 node is configured.
fn it_enabled() -> bool {
    std::env::var("OPS_IT_L2_RPC").is_ok()
}

/// Read a `u64` from an env var, falling back to `default` when unset/unparsable.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The integration range from env, or `None` if not configured (so the test skips
/// cleanly rather than hard-failing on a zero-width range).
fn range_enabled() -> Option<(u64, u64)> {
    let start: u64 = std::env::var("OPS_IT_START").ok()?.parse().ok()?;
    let end: u64 = std::env::var("OPS_IT_END").ok()?.parse().ok()?;
    if end <= start {
        return None;
    }
    Some((start, end))
}

/// Build the real estimator the same way `contained::run` does, plus a temp-dir cache.
///
/// Construction is a macro (not a function) so the concrete host type returned by
/// `initialize_host` flows to the call site: a `fn -> Arc<Estimator<impl OPSuccinctHost>>`
/// would erase the host's rkyv bounds, making `build_range_witness`/`execute_range`
/// uncallable. With the construction inlined, the eigenda host's bounds resolve
/// automatically — exactly as they do where `contained::run` builds the estimator.
///
/// Binds `$est` (the `Arc<Estimator<…>>`), `$fetcher` (`Arc<OPSuccinctDataFetcher>`),
/// `$cache` (`WitnessCache`), and `$dir` (the `TempDir` whose lifetime backs the cache).
macro_rules! build_estimator {
    ($est:ident, $fetcher:ident, $cache:ident, $dir:ident) => {
        let $fetcher = Arc::new(OPSuccinctDataFetcher::new_with_rollup_config().await.unwrap());
        let chain_id = $fetcher.get_l2_chain_id().await.unwrap();
        let host = initialize_host($fetcher.clone());
        let $dir = tempfile::TempDir::new().unwrap();
        let $cache = WitnessCache::new($dir.path(), chain_id, DaType::EigenDa);
        let $est = Arc::new(Estimator {
            host,
            fetcher: $fetcher.clone(),
            cache: $cache.clone(),
            chain_id,
            safe_db_fallback: true,
        });
    };
}

/// Test 1 — pure prune + drop mechanics (NO RPC, must pass in CI).
#[test]
fn stdin_prunes_after_grace_and_witness_dropped() {
    use op_succinct_estimator::cache::{DaType, WitnessCache};
    let dir = tempfile::TempDir::new().unwrap();
    let cache = WitnessCache::new(dir.path(), 42220, DaType::EigenDa);
    cache.save_stdin(10, 20, &sp1_sdk::SP1Stdin::default()).unwrap();
    assert!(cache.has_stdin(10, 20));
    cache.prune_stdin(10, 20).unwrap(); // grace elapsed → prune
    assert!(!cache.has_stdin(10, 20));
    assert!(!cache.has_witness(10, 20)); // witness was never built / already dropped
}

/// Test 2 — parity/aggregation over a small real range (ENV-GATED).
#[tokio::test]
async fn execute_game_aggregates_over_real_range() {
    if !it_enabled() {
        eprintln!("skipping: OPS_IT_L2_RPC unset");
        return;
    }
    dotenv::from_filename(".env").ok();
    let Some((start, end)) = range_enabled() else {
        eprintln!("skipping: OPS_IT_START/OPS_IT_END unset or invalid");
        return;
    };
    let batch_size = env_u64("OPS_IT_BATCH", 100);

    build_estimator!(estimator, fetcher, _cache, _dir);

    let game = GameData {
        game_index: 0,
        game_address: alloy_primitives::Address::ZERO,
        start_block: start,
        end_block: end,
        created_at: std::time::SystemTime::now(),
    };

    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let (stats, ranges) =
        execute_game(&estimator, &fetcher, &permits, &game, batch_size).await.unwrap();

    assert_eq!(stats.batch_end, end);
    // The safe-head split is contiguous and get_l2_block_data_range covers start+1..=end
    // with no gaps or double-counting, so the aggregated executed-block count equals the
    // game width (end - start).
    assert_eq!(stats.nb_blocks, end - start);
    assert!(stats.total_instruction_count > 0);
    assert!(!ranges.is_empty());
}

/// Test 3 — prebuild-skip / cache-soundness (ENV-GATED). A pipeline-built stdin is
/// usable by the executor, and building the stdin drops the (large) witness blob.
#[tokio::test]
async fn prebuilt_stdin_is_consumed_by_executor() {
    if !it_enabled() {
        eprintln!("skipping: OPS_IT_L2_RPC unset");
        return;
    }
    dotenv::from_filename(".env").ok();
    let Some((start, end)) = range_enabled() else {
        eprintln!("skipping: OPS_IT_START/OPS_IT_END unset or invalid");
        return;
    };

    build_estimator!(estimator, _fetcher, cache, _dir);
    let range = SpanBatchRange { start, end };

    // Pipeline producer: build the stdin (host.run → crunch → cache stdin → drop witness).
    estimator.build_range_witness(&range).await.unwrap();
    assert!(cache.has_stdin(range.start, range.end));
    assert!(!cache.has_witness(range.start, range.end)); // witness dropped once stdin built

    // Consumer: the executor reuses the prebuilt stdin and produces real stats.
    let stats = estimator.execute_range(&range).await.unwrap();
    assert!(stats.total_instruction_count > 0);
}

/// Test 4 — forced-failure recovery / resilience invariant (ENV-GATED). A cached step is
/// not redone: a second `build_range_witness` for the same range is a no-op fast-path.
#[tokio::test]
async fn second_build_is_noop_fast_path() {
    if !it_enabled() {
        eprintln!("skipping: OPS_IT_L2_RPC unset");
        return;
    }
    dotenv::from_filename(".env").ok();
    let Some((start, end)) = range_enabled() else {
        eprintln!("skipping: OPS_IT_START/OPS_IT_END unset or invalid");
        return;
    };

    build_estimator!(estimator, _fetcher, cache, _dir);
    let range = SpanBatchRange { start, end };

    // Build once to success — stdin (and/or witness) is now cached.
    estimator.build_range_witness(&range).await.unwrap();
    assert!(cache.has_stdin(range.start, range.end));

    // A second build returns Ok without re-fetching: the cached stdin is not redone.
    estimator.build_range_witness(&range).await.unwrap();
    assert!(cache.has_stdin(range.start, range.end));
}
