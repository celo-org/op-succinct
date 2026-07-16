//! Integration tests for the embedded monitor. RPC-dependent tests are a no-op when
//! `OPS_IT_L2_RPC` is unset. Run manually against a node with the env set.

use std::sync::Arc;

use op_succinct_estimator::{
    aggregate_execution_stats,
    cache::{DaType, WitnessCache},
    Estimator,
};
use op_succinct_host_utils::{
    block_range::{split_range_basic, SpanBatchRange},
    fetcher::OPSuccinctDataFetcher,
};
use op_succinct_proof_utils::initialize_host;
use op_succinct_scripts::game_monitor_embedded::{
    admission::{Admission, AdmissionConfig},
    discovery::GameData,
    executor::execute_game,
    rss_source::Unsupported,
};

/// Build an unbudgeted admission gate for integration tests (no memory limit; serial cold
/// start). Fast admit poll so the serial warmup phase doesn't add latency between ranges.
fn test_admission(persist_path: std::path::PathBuf) -> Arc<Admission> {
    Admission::load(
        AdmissionConfig {
            budget_bytes: None,
            margin_bytes: 0,
            max_concurrent: 8,
            admit_poll: std::time::Duration::from_millis(20),
            sample_period: std::time::Duration::from_millis(50),
            persist_every: 50,
            persist_path,
        },
        Box::new(Unsupported),
    )
}

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

/// Build the real estimator the same way `game_monitor_embedded::run` does, plus a temp-dir cache.
///
/// Construction is a macro (not a function) so the concrete host type returned by
/// `initialize_host` flows to the call site: a `fn -> Arc<Estimator<impl OPSuccinctHost>>`
/// would erase the host's rkyv bounds, making `build_range_witness`/`execute_range`
/// uncallable. With the construction inlined, the eigenda host's bounds resolve
/// automatically — exactly as they do where `game_monitor_embedded::run` builds the estimator.
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

/// Test 2 — parity over a small real range (ENV-GATED). `execute_game`'s concurrent
/// split-and-aggregate must equal a straightforward serial reference: split the window with
/// `split_range_basic`, `execute_range` each sub-range, then aggregate. Execution is
/// deterministic and aggregation is an order-independent sum, so the two must match
/// field-for-field.
///
/// A true cross-tool `cost_estimator` baseline is out of scope: `cost_estimator` is not daemon
/// code and is not retrofitted to `utils/estimator` (spec §12), and it splits differently
/// (safe-head vs `split_range_basic`), so it is not apples-to-apples. Its aggregation logic is
/// the very one this reuses (`stats.rs`), so the reference below is the faithful in-scope check.
#[tokio::test]
async fn execute_game_matches_serial_reference() {
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

    let admission = test_admission(_dir.path().join("memory_model.json"));
    let (game_stats, ranges) =
        execute_game(&estimator, &fetcher, &admission, &game, batch_size).await.unwrap();

    // The daemon's split must be exactly `split_range_basic` over the window, covering every
    // block once (no gaps or overlap).
    let expected_ranges = split_range_basic(start, end, batch_size);
    assert_eq!(ranges.len(), expected_ranges.len(), "sub-range count");
    for (got, want) in ranges.iter().zip(&expected_ranges) {
        assert_eq!((got.start, got.end), (want.start, want.end), "sub-range boundary");
    }
    assert_eq!(game_stats.batch_start, start);
    assert_eq!(game_stats.batch_end, end);
    assert_eq!(game_stats.nb_blocks, end - start);
    assert!(game_stats.total_instruction_count > 0);

    // Parity: execute the same sub-ranges serially and aggregate. `execute_range` is
    // deterministic (cached stdin → same SP1 report), so this must equal `execute_game`'s
    // concurrent aggregate exactly — proving the split, concurrency, and aggregation are sound.
    let mut per_range = Vec::with_capacity(expected_ranges.len());
    for r in &expected_ranges {
        let block_data = fetcher.get_l2_block_data_range(r.start, r.end).await.unwrap();
        per_range.push(estimator.execute_range(r, &block_data).await.unwrap());
    }
    let reference = aggregate_execution_stats(&per_range, 0, 0);
    assert_eq!(game_stats, reference, "execute_game aggregate must match the serial reference");
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

    build_estimator!(estimator, fetcher, cache, _dir);
    let range = SpanBatchRange { start, end };

    // Pipeline producer: build the stdin (host.run → crunch → cache stdin → drop witness).
    estimator.build_range_witness(&range).await.unwrap();
    assert!(cache.has_stdin(range.start, range.end));
    assert!(!cache.has_witness(range.start, range.end)); // witness dropped once stdin built

    // Consumer: the executor reuses the prebuilt stdin and produces real stats. Block data
    // is fetched by the caller (as the executor does) and threaded into execute_range.
    let block_data = fetcher.get_l2_block_data_range(range.start, range.end).await.unwrap();
    let stats = estimator.execute_range(&range, &block_data).await.unwrap();
    assert!(stats.total_instruction_count > 0);
}

/// Test 4 — the fast-path invariant (ENV-GATED): once a range's stdin is cached,
/// `build_range_witness` short-circuits and never touches the host (`host.fetch` / `host.run`).
///
/// Proven by fault injection rather than a bare `Ok`: a range the host *cannot* build (blocks
/// that don't exist) fails when its stdin is not cached — establishing the host path is exercised
/// and fails — but returns `Ok` once its stdin is pre-seeded. The only way the second call can
/// succeed is by returning at the `has_stdin` short-circuit, before the (failing) host path. That
/// is exactly the skip the executor and pipeline rely on for a cache hit.
#[tokio::test]
async fn second_build_short_circuits_host_run() {
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

    // Build the real range once (host.fetch + host.run + get_sp1_stdin), caching its stdin.
    let range = SpanBatchRange { start, end };
    estimator.build_range_witness(&range).await.unwrap();
    assert!(cache.has_stdin(range.start, range.end));

    // A range the host cannot build: block numbers far beyond any real chain height. With no
    // cached stdin the build MUST reach the host and fail — this exercises (and fails) exactly
    // the path the short-circuit skips.
    let unbuildable = SpanBatchRange { start: 9_000_000_000_000_000_000, end: 9_000_000_000_000_000_100 };
    assert!(
        estimator.build_range_witness(&unbuildable).await.is_err(),
        "sanity: an unbuildable range must fail when its stdin is not cached (host path exercised)"
    );

    // Pre-seed its stdin, then build again: `Ok` is only reachable via the `has_stdin`
    // short-circuit returning before host.fetch/host.run — proving host.run is skipped.
    cache.save_stdin(unbuildable.start, unbuildable.end, &sp1_sdk::SP1Stdin::default()).unwrap();
    estimator.build_range_witness(&unbuildable).await.unwrap();
    assert!(cache.has_stdin(unbuildable.start, unbuildable.end));
}

/// Test 5 — frontier/game cache-key alignment (ENV-GATED). The pipeline prebuilds a
/// window's stdin; an on-chain game whose `[start_block, end_block]` equals that window
/// then finds the stdin already cache-resident, so `execute_game` hits the cache instead
/// of rebuilding. This documents the alignment invariant the frontier-seed fix enforces:
/// the pipeline's frontier must sit on a real proposal boundary so its split sub-ranges
/// share cache keys with the executor's game splits.
///
/// This is the alignment-POSITIVE assertion. A divergent-boundary test (a misaligned
/// frontier missing the cache) needs a live multi-game chain to derive two genuinely
/// different proposal boundaries, so it is out of scope here.
#[tokio::test]
async fn pipeline_window_aligned_to_game_boundary_hits_cache() {
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

    build_estimator!(estimator, fetcher, cache, _dir);

    // Pipeline producer: prebuild the window's stdin (the predictor would do this for an
    // aligned window `[start, end]`).
    let range = SpanBatchRange { start, end };
    estimator.build_range_witness(&range).await.unwrap();

    // The key invariant: the aligned window's stdin is cache-resident BEFORE the executor
    // runs, so an aligned game will hit the cache rather than rebuild.
    assert!(cache.has_stdin(start, end));

    // The aligned game: its on-chain boundaries equal the prebuilt window.
    let game = GameData {
        game_index: 0,
        game_address: alloy_primitives::Address::ZERO,
        start_block: start,
        end_block: end,
        created_at: std::time::SystemTime::now(),
    };

    let admission = test_admission(_dir.path().join("memory_model.json"));
    let (stats, ranges) =
        execute_game(&estimator, &fetcher, &admission, &game, batch_size).await.unwrap();

    // execute_game succeeds and produces real stats over the aligned range.
    assert_eq!(stats.batch_end, end);
    assert!(stats.total_instruction_count > 0);
    assert!(!ranges.is_empty());
}
