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
    executor::prove_game,
    rss_source::Unsupported,
    scheduler::{spawn_workers, Scheduler},
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
/// would erase the host's rkyv bounds, making `witness_range`/`prove_range`
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
    assert!(!cache.has_witness(10, 20)); // witness was never generated / already dropped
}

/// Test 2 — parity over a small real range (ENV-GATED). `prove_game`'s concurrent
/// split-and-aggregate must equal a straightforward serial reference: split the window with
/// `split_range_basic`, `prove_range` each sub-range, then aggregate. Proving is
/// deterministic and aggregation is an order-independent sum, so the two must match
/// field-for-field.
///
/// A true cross-tool `cost_estimator` baseline is out of scope: `cost_estimator` is not daemon
/// code and is not retrofitted to `utils/estimator` (spec §12), and it splits differently
/// (safe-head vs `split_range_basic`), so it is not apples-to-apples. Its aggregation logic is
/// the very one this reuses (`stats.rs`), so the reference below is the faithful in-scope check.
#[tokio::test(flavor = "multi_thread")]
async fn prove_game_matches_serial_reference() {
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

    // Games assemble from the proof cache via the scheduler's worker pools (#26): spawn a
    // small pool (speculation disabled — lead 0) and let `prove_game` demand its ranges.
    let admission = test_admission(_dir.path().join("memory_model.json"));
    let sched = Arc::new(Scheduler::new(0, 0));
    spawn_workers(sched.clone(), estimator.clone(), fetcher.clone(), admission, 2, 4);
    let (game_stats, ranges) = prove_game(&estimator, &sched, &game, batch_size).await.unwrap();

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

    // Parity: prove the same sub-ranges serially and aggregate. `prove_range` is
    // deterministic (cached stdin → same SP1 report), so this must equal `prove_game`'s
    // concurrent aggregate exactly — proving the split, concurrency, and aggregation are sound.
    let mut per_range = Vec::with_capacity(expected_ranges.len());
    for r in &expected_ranges {
        let block_data = fetcher.get_l2_block_data_range(r.start, r.end).await.unwrap();
        per_range.push(estimator.prove_range(r, &block_data).await.unwrap());
    }
    let reference = aggregate_execution_stats(&per_range, 0, 0);
    assert_eq!(game_stats, reference, "prove_game aggregate must match the serial reference");
}

/// Test 3 — pre-generation-skip / cache-soundness (ENV-GATED). A pipeline-generated stdin is
/// usable by the executor, and generating the stdin drops the (large) witness blob.
#[tokio::test(flavor = "multi_thread")]
async fn pregenerated_stdin_is_consumed_by_prover() {
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

    // Pipeline producer: generate the stdin (host.run → crunch → cache stdin → drop witness).
    estimator.witness_range(&range).await.unwrap();
    assert!(cache.has_stdin(range.start, range.end));
    assert!(!cache.has_witness(range.start, range.end)); // witness dropped once stdin generated

    // Consumer: the prover reuses the pre-generated stdin and produces real stats. Block data
    // is fetched by the caller (as the prover does) and threaded into prove_range.
    let block_data = fetcher.get_l2_block_data_range(range.start, range.end).await.unwrap();
    let stats = estimator.prove_range(&range, &block_data).await.unwrap();
    assert!(stats.total_instruction_count > 0);
}

/// Test 4 — the fast-path invariant (ENV-GATED): once a range's stdin is cached,
/// `witness_range` short-circuits and never touches the host (`host.fetch` / `host.run`).
///
/// Proven by fault injection rather than a bare `Ok`: a range the host *cannot* witness (blocks
/// that don't exist) fails when its stdin is not cached — establishing the host path is exercised
/// and fails — but returns `Ok` once its stdin is pre-seeded. The only way the second call can
/// succeed is by returning at the `has_stdin` short-circuit, before the (failing) host path. That
/// is exactly the skip the prover and pipeline rely on for a cache hit.
#[tokio::test(flavor = "multi_thread")]
async fn second_witness_short_circuits_host_run() {
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

    // Generate the real range's witness once (host.fetch + host.run + get_sp1_stdin), caching
    // its stdin.
    let range = SpanBatchRange { start, end };
    estimator.witness_range(&range).await.unwrap();
    assert!(cache.has_stdin(range.start, range.end));

    // A range the host cannot witness: block numbers far beyond any real chain height. With no
    // cached stdin the witness task MUST reach the host and fail — this exercises (and fails)
    // exactly the path the short-circuit skips.
    let unwitnessable = SpanBatchRange { start: 9_000_000_000_000_000_000, end: 9_000_000_000_000_000_100 };
    assert!(
        estimator.witness_range(&unwitnessable).await.is_err(),
        "sanity: an unwitnessable range must fail when its stdin is not cached (host path exercised)"
    );

    // Pre-seed its stdin, then witness again: `Ok` is only reachable via the `has_stdin`
    // short-circuit returning before host.fetch/host.run — proving host.run is skipped.
    cache.save_stdin(unwitnessable.start, unwitnessable.end, &sp1_sdk::SP1Stdin::default()).unwrap();
    estimator.witness_range(&unwitnessable).await.unwrap();
    assert!(cache.has_stdin(unwitnessable.start, unwitnessable.end));
}

/// Test 5 — frontier/game cache-key alignment (ENV-GATED). The pipeline pre-generates a
/// window's stdin; an on-chain game whose `[start_block, end_block]` equals that window
/// then finds the stdin already cache-resident, so `prove_game` hits the cache instead
/// of regenerating. This documents the alignment invariant the frontier-seed fix enforces:
/// the pipeline's frontier must sit on a real proposal boundary so its split sub-ranges
/// share cache keys with the executor's game splits.
///
/// This is the alignment-POSITIVE assertion. A divergent-boundary test (a misaligned
/// frontier missing the cache) needs a live multi-game chain to derive two genuinely
/// different proposal boundaries, so it is out of scope here.
#[tokio::test(flavor = "multi_thread")]
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

    // Pipeline producer: pre-generate the window's stdin (the predictor would do this for an
    // aligned window `[start, end]`).
    let range = SpanBatchRange { start, end };
    estimator.witness_range(&range).await.unwrap();

    // The key invariant: the aligned window's stdin is cache-resident BEFORE the prover
    // runs, so an aligned game will hit the cache rather than regenerate.
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
    let sched = Arc::new(Scheduler::new(0, 0));
    spawn_workers(sched.clone(), estimator.clone(), fetcher.clone(), admission, 2, 4);
    let (stats, ranges) = prove_game(&estimator, &sched, &game, batch_size).await.unwrap();

    // prove_game succeeds and produces real stats over the aligned range.
    assert_eq!(stats.batch_end, end);
    assert!(stats.total_instruction_count > 0);
    assert!(!ranges.is_empty());
}
