# Contained Game Monitor — Design Spec

- **Date:** 2026-06-10
- **Status:** Approved for planning
- **Author:** piers (with Claude)
- **Topic:** Replace the subprocess-based game monitor with a single contained process that runs cost-estimation internally — driven by a predictive, finalization-triggered witness pipeline — resiliently, with on-disk caching of witness data and SP1 stdin.

---

## 1. Context & motivation

Today the game monitor (`scripts/utils/bin/game_monitor.rs`, ~1640 lines) is a daemon that, for each on-chain dispute game, **spawns the `cost-estimator` binary as a subprocess** and infers success/failure by **scraping the child's log tail**. This works but has structural problems:

1. **Flakiness wastes whole runs.** A cost-estimator run takes ~1 hour and ~50 GB RAM. A single transient failure — typically an L1 or beacon fetch **right at the end** of a run — propagates as an unhandled error and kills the entire process, discarding an hour of work. The fetch layer has an explicit `FIXME: Add retries for all requests` (`utils/host/src/fetcher.rs:99`) that was never implemented; every RPC call propagates with `?`/`expect`.
2. **Failure handling is fragile.** Results return only as an exit code plus a 1 MiB log-tail scrape (`game_monitor.rs:1048-1075`) classified into a `FailureType` enum that **does not even drive retry** — every non-zero exit retries identically.
3. **Wasted re-fetching, no lookahead.** Each run fetches its witness data from cold when the game is already due (`cost_estimator.rs:297-305`); a retry re-fetches everything. Nothing is built ahead of time, even though the L2 blocks for a range are produced long before the game must be cost-estimated.
4. **Operational pain.** Per-game logs live in files inside the pod; debugging means `kubectl exec` in and reading files. Logs are deleted on shutdown (`game_monitor.rs:822-832`), losing in-flight diagnostics.

The cost-estimator is essentially a **thin CLI over already-public library code** (`cost_estimator.rs:253-348`): build fetcher + host → resolve range → split into `SpanBatchRange`s → per range `host.fetch → host.run → get_sp1_stdin` → `CpuProver.execute(range_elf, stdin)` → `ExecutionStats`. Only two functions and a CSV round-trip are non-library glue, which makes an in-process rewrite tractable.

## 2. Goals & non-goals

### Goals
- **Single contained process.** Run cost-estimation internally — no subprocess, no log-scraping. Results are structured `Result` values.
- **Predictive pre-building.** Don't wait for a game to be due. Predict each range from the proposal cadence and **build its SP1 stdin as soon as its L2 blocks finalize**, so a game's witness is ready before it's executed.
- **Fetch/crunch once, cache both.** Cache the `WitnessData` (output of `host.run`) and the `SP1Stdin` (output of `get_sp1_stdin`) on disk, so a rerun never re-fetches/re-derives/re-proves and an execute never re-crunches.
- **Resilience.** Catch transient failures at the smallest sensible boundary so a blip redoes a small chunk, not an hour.
- **Unified logging** to stdout via `tracing`, with a per-run span so concurrent work is attributable.
- **Behavioural parity** with the cost-estimator's measurement, and preservation of the current monitor's control-plane semantics.

### Non-goals (this iteration)
- Touching `cost_estimator.rs` (left as-is until the new shape stabilises) or the live validity proposer.
- True per-channel/per-span-batch range alignment (safe-head granularity is sufficient).
- Cross-DA support beyond **EigenDA** (the production target). The code stays DA-generic via the existing `OPSuccinctHost` trait, but only EigenDA is exercised/hardened.
- A metrics/DB backend (structured logs only; metrics are future work).
- A content-addressed per-preimage store / rocksdb (see §4.4 "Not included").

## 3. Key decisions (locked)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | **DA layer = EigenDA** | Production target (Celo). The canoe recursion proof is generated at the **end of `host.run`** (`utils/eigenda/host/src/witness_generator.rs:161-192`), so caching `WitnessData` skips it on rerun. |
| D2 | **Fully in-process** (one OS process) | User requirement. Consequence: no SIGKILL isolation, and memory must budget **both** witness-gen and execute — see §7. |
| D3 | **Predictive, finalization-triggered witness pipeline** (single design, no phases) | Predict ranges from `PROPOSAL_INTERVAL`; build `WitnessData`+`SP1Stdin` as each range's end finalizes, ahead of execution — see §4.2. |
| D4 | **Cache both `WitnessData` and `SP1Stdin`** on disk (plain blobs, no DB), pruned on separate clocks | `WitnessData` is the fetch-expensive durable artifact; `SP1Stdin` is what execute consumes — see §4.4. |
| D5 | **Safe-head sub-range splitting** within a window | Reuse `split_range_based_on_safe_heads`; no net-new host-side channel decoding. Sub-range boundaries are **computed** from chain data, not predicted. |
| D6 | **`tracing` to stdout (JSON)** | Concurrent in-process work makes plain `log` lines unattributable; spans fix this and bridge `log`. |
| D7 | **Scope = new monitor + new lib; leave `cost_estimator.rs` + proposer untouched** | Small blast radius; edits to `utils/*` are additive. |

## 4. Architecture

Two new artifacts in the op-succinct repo. The daemon has two cooperating concerns — a **proactive witness pipeline** (producer) and a **reactive game executor** (consumer) — that hand off through the on-disk cache (§4.4).

### 4.1 `scripts/utils/bin/game_monitor_contained.rs` — the daemon

**Reactive control plane (the game executor).** Keeps today's proven semantics:
- **Discovery** — finalized-block index polling of `factory.gameCount()`/`gameAtIndex` (`game_monitor.rs:1541-1566`), `GAME_TYPE == 42` filter (`:42`), real `start/end` from `startingBlockNumber`/`l2BlockNumber`, `created_at` from the factory timestamp (`fetch_game_data`, `:1101-1140`).
- **Discover→execute `--delay`** — the window that avoids the multi-backend RPC 404 race (`:130-136`). Preserved.
- **Execution** — for each of a game's safe-head sub-ranges, **load the prebuilt `SP1Stdin` from the cache** (a hit, because the pipeline built it ahead of time; on a miss, build it on demand) and run the SP1 execute (the ~50 GB step), gated by RSS admission (§7). Aggregate `ExecutionStats`.
- **Scheduling & retry** — two-tier Primary (linear backoff, bounded) → Background (4× exponential, age-bounded) from `maybe_requeue` (`:708-781`)/`evict_aged_background_retries` (`:791-818`); `last_contiguous` frontier (`utils/common/src/sequence_tracker.rs`) so a permanently-failed game never stalls the daemon; `progress.json` for restart resume.

**Replaces** `spawn_cost_estimator` (`:1147-1208`) with in-process calls into `utils/estimator` and a cache lookup.

### 4.2 The predictive witness pipeline (proactive producer) — the core of the rewrite

A background pipeline that builds SP1 stdin **ahead** of execution, so a game's witness is ready before it's due:

1. **Predict the window.** Game boundaries step by `PROPOSAL_INTERVAL` (`celo/create_new_game_branch.sh:83`: `l2 = parent + PROPOSAL_INTERVAL`). The pipeline tracks the frontier (last built end) and the next window is `[frontier, frontier + PROPOSAL_INTERVAL]`. Configure the monitor with the proposer's interval. (Best-effort/speculative: the on-chain game is the source of truth, so a wrong interval only wastes work and the executor rebuilds on demand.)
2. **Compute sub-ranges.** Once the window's L2 blocks exist, **compute** its safe-head sub-ranges from chain data (`split_range_based_on_safe_heads`) — these can't be predicted from the interval; they come from `optimism_safeHeadAtL1Block`.
3. **Trigger on finalization.** As soon as a sub-range's **end block is finalized on L2** (and L1 has finalized past its `l1_head`, §4.4), run the **two processes sequentially for that sub-range**: `host.run` (fetch `WitnessData`) → `get_sp1_stdin` (crunch to `SP1Stdin`), caching **both** (§4.4).
4. **Bounded concurrency.** Multiple sub-range builds run concurrently, gated by the same RSS budget as execution (witness-gen has its own multi-GB footprint — §7).

When the executor later processes a game, every sub-range's stdin is already cached → it just executes. Because the pipeline (over a predicted window) and the executor (over the real game `[start, end]`) run the **same deterministic safe-head split**, the keys match whenever the prediction matched the real game boundary.

### 4.3 `utils/estimator/` — new workspace crate ("the new cost estimator" as a library)

A **new crate** (not a module in `utils/host`) because the core needs `sp1-sdk` + `utils/proof` (the range ELF) deps that `utils/host` should not carry. It exposes the lifted core of `cost_estimator.rs`, split into the pipeline's two concerns:

```rust
// proactive: fetch + crunch + cache both. Called by the pipeline (§4.2).
pub async fn build_range_witness(
    host, fetcher, cache, range: SpanBatchRange, opts,
) -> Result<(), EstimatorError>;     // host.run -> cache WitnessData -> get_sp1_stdin -> cache SP1Stdin

// reactive: load stdin (build on miss) + SP1 execute. Called by the executor (§4.1).
pub async fn execute_range(
    cache, range: SpanBatchRange, opts,
) -> Result<ExecutionStats, EstimatorError>;
```

- `build_range_witness` runs `host.run` then `get_sp1_stdin` **sequentially** (stdin depends on `WitnessData`), caching each.
- `execute_range` loads the cached stdin and runs SP1 inside its own `tokio::task::spawn_blocking` (`CpuProver` spins its own tokio runtime and must not run on the async runtime, `cost_estimator.rs:132-135`). **Concurrency across ranges is governed by the daemon's RSS admission gate (§7), not an uncontrolled rayon `par_iter` over all ranges** (`:136`) — that would bypass admission.
- `ExecutionStats::new` maps the SP1 `ExecutionReport` to stats (`utils/host/src/stats.rs:81-139`); the daemon aggregates a game's `Vec<ExecutionStats>` via the lifted `aggregate_execution_stats` (`cost_estimator.rs:196-251`) — no CSV write/reread.
- `EstimatorError` — a structured enum that **replaces log-tail scraping**, seeded from today's five `FAILURE_PATTERNS` (`game_monitor.rs:1031-1037`): `NoHealthyBackend` (-32011/503), `NoStateAvailable` (-32002), `ExceedsProofWindow` (-32602), `MissingTrieNode` (-32000), `DnsLookupFailure`, plus `Oom`, `Transient(source)`, `Fatal(source)`. Each variant is tagged transient (retry) vs fatal (don't retry; never retry `Oom`).

### 4.4 Witness cache — on-disk `WitnessData` and `SP1Stdin` blobs

Both artifacts are cached as plain serialized blobs on disk — the same mechanism as today's `witness_cache` (`utils/host/src/witness_cache.rs:17-67`), which already bincodes to `data/<chain_id>/witness-cache/<start>-<end>.bin`. **No database, no rocksdb, no content-addressed store** — one file per range per artifact.

- **What/where:** per sub-range, keyed `chain_id/start/end/da_type`, under a configurable absolute base dir. Adapt `witness_cache` to hold **both** `WitnessData` and `SP1Stdin` (it stores only the final stdin today), and fix its key — add a DA-type discriminator (EigenDA's `EigenDAWitnessData` is incompatible with `DefaultWitnessData`) — and its base dir (cwd-relative today).
- **Why cache both:** for EigenDA the canoe recursion proof is generated at the **end of `host.run`** (`utils/eigenda/host/src/witness_generator.rs:161-192`) and stored inside `eigenda_data`; `get_sp1_stdin` (`:70-98`) is then pure CPU serialization that moves the proof into the stdin proof-stream. Caching `WitnessData` skips fetch + derivation **and the expensive canoe proving** on any rerun; caching `SP1Stdin` lets the executor run immediately without re-crunching.
- **Pruning (separate clocks):** `SP1Stdin` is regenerable from `WitnessData`, and `WitnessData` is the bigger, fetch-expensive artifact. So: drop a range's `WitnessData` once its `SP1Stdin` is built (it's only needed for a re-crunch); keep its `SP1Stdin` until the owning game **succeeds** + a grace window (~1 h, configurable) for near-term reruns. Games are disjoint/non-overlapping, so stdin pruning is time-based per game.
- **Key soundness (`l1_head`):** the witness also depends on `l1_head` (from `calculate_safe_l1_head(end)`, capped at the finalized L1 header). For a finalized range this is deterministic, so the `(chain_id, start, end, da_type)` key is sound — but the pipeline must only **persist** a range's artifacts once L1 has finalized past its `l1_head`. Otherwise fold `l1_head` into the key.
- **Not included:** a fine-grained content-addressed per-preimage store (kona's `data_dir` kv pointed at disk) was considered and dropped. It would **not** make fetching boundary-free — `host.run` is always per-range (it fetches *and* derives a range together, and the majority of the witness is range-specific L2 execution-state trie nodes producible only by running that range; only the minority "derivation-input" L1/blob data is range-agnostic). It would only dedupe that minority across adjacent ranges, which `PROPOSAL_INTERVAL` prediction + the per-range cache make unnecessary. Revisit only if measured cross-range re-fetch proves costly.

### 4.5 Resilience layer

- **Per-RPC retry:** add `alloy` `RetryBackoffLayer` (already a dependency, currently unused, `Cargo.toml:~160`) to the L1/L2 providers; wrap raw JSON-RPC (`fetch_rpc_data`, fresh client per call today, `fetcher.rs:569`) with a shared `network_call_with_timeout` (lift the reusable helper from `fault-proof/src/prover.rs:203-256`). Idempotent and runs **before** expensive witness-gen — highest leverage, lowest cost.
- **Per-sub-range build retry:** wrap `host.run → get_sp1_stdin`; on a *successful* `host.run` the `WitnessData` is cached (§4.4), so a later retry skips it. Once stdin is cached, a later **execute** failure costs only a re-execute — the key resilience win (no hour of re-fetch/re-derive/re-prove).
- **EigenDA retry unit = the whole `host.run`.** The canoe proof is generated *inside* `host.run` (`:161-192`, via external `hokulea_witgen`), so it can't be cleanly checkpointed *before* canoe without restructuring shared code. A successful `host.run` is cached as `WitnessData`; a failure *within* it retries the method (the inline `RetryBackoffLayer` absorbs most transient blips, so outright `host.run` failure is rare). Memoizing the canoe proof by input-hash is a possible future refinement.
- **Harden panics:** convert the KZG `.unwrap()`s and `Mutex` locks in `OnlineBlobStore` (`utils/host/src/witness_generation/online_blob_store.rs:32,45-54`) to errors so a blob/KZG fault is a retryable `EstimatorError`, not a crash (matters more now that one process holds all work).
- **Outer per-game retry:** the existing `maybe_requeue` remains as the last resort; with inner retries it should rarely fire.

### 4.6 Logging / observability

`tracing` subscriber emitting **JSON to stdout** (collected by infra). One span per unit of work carrying `game` + `range` + `attempt`; `host.run`/`get_sp1_stdin`/`execute` as child spans. Enable the `log` bridge so existing `log::`-based library code is captured without a big-bang migration. Replaces fs-`metadata().len()` health polling and the 1 MiB log-tail scrape.

## 5. Data flow

**Producer — the witness pipeline (proactive, continuous):**
```
track frontier = last built end
predict next window [frontier, frontier + PROPOSAL_INTERVAL]
  → once window blocks exist: compute safe-head sub-ranges (from chain)
  → for each sub-range, when its end block is L2-finalized (and L1 finalized past l1_head):
        host.run(range)            → WitnessData   → cache it          (RSS-admitted)
        get_sp1_stdin(WitnessData) → SP1Stdin      → cache it
        drop cached WitnessData (stdin now built)
  → advance frontier
```

**Consumer — the game executor (reactive):**
```
discover index (finalized) → wait --delay → fetch game data (type-42; real start/end)
  → safe-head split into sub-ranges
  → for each sub-range:
        SP1Stdin cached?  ── hit ──► execute (RSS-admitted) → ExecutionStats
                          ── miss ─► build_range_witness (on demand) → execute
  → aggregate ExecutionStats → mark game success → schedule stdin prune (after grace)
```

**Failure cascade (innermost first):** per-call retry (transient RPC) → per-sub-range build retry (a successful `host.run` is cached, so a later execute failure skips it) → per-game requeue (Primary → Background; `last_contiguous` still advances so the daemon never stalls).

## 6. Code-sharing seams (what to lift / change)

- **Lift** `execute_blocks_and_write_stats_csv` + `aggregate_execution_stats` (`cost_estimator.rs:35-251`) into `utils/estimator`, split into `build_range_witness` (fetch+crunch+cache) and `execute_range` (load+execute); return `ExecutionStats` (drop CSV write/reread + `cargo_metadata` workspace_root lookup).
- **Call directly:** `OPSuccinctHost::fetch/run` + `witness_generator().get_sp1_stdin` (`utils/host/src/host.rs:81,92-107`; EigenDA generator `utils/eigenda/host/src/witness_generator.rs`).
- **Build once at startup:** `OPSuccinctDataFetcher::new_with_rollup_config` + `initialize_host` (`utils/proof/src/lib.rs:46-78`), threaded everywhere (the estimator builds the fetcher twice today).
- **Additive change:** add a `split_range_based_on_safe_heads_with_fetcher(&fetcher, …)` variant so the splitter reuses the shared fetcher instead of building its own `default()` (`block_range.rs:124,146`); leave the existing signature (used by `cost_estimator.rs`) intact.
- **Reuse** `split_range_based_on_safe_heads` (`block_range.rs:119-180`) as the splitter; `split_range_basic` as fallback when `is_safe_db_activated()` is false.

## 7. Memory model (consequence of D2)

- **Budget source:** read the **cgroup** limit — `/sys/fs/cgroup/memory.max` (v2) / `memory.limit_in_bytes` (v1) — not host `/proc/meminfo`; live usage from `memory.current`. Mainnet pods ≈ 500 GiB; testnets less.
- **Two heavy workloads share the budget.** Witness-gen (`host.run`, multi-GB of preimages held in memory) now runs **concurrently** with SP1 execute (~50 GB). RSS admission must gate **both** kinds of work — pipeline builds and executor executes draw from the same budget.
- **Per-execution bound:** `--batch-size` caps L2 blocks per range → caps SP1 guest memory per execution. Default **conservatively** so any single execution is a fraction of the pod.
- **Adaptive concurrency:** extend the completion-history (`completion_history.json`) to record **peak RSS** for both build and execute, keyed by EVM gas (tracks memory better than block count). Project the next unit's peak from history; admit only if it fits remaining budget with a margin.
- **No force-kill (accepted tradeoff):** a running SP1 execution cannot be cancelled (dropping a `spawn_blocking` task does not stop the closure; only a subprocess could). So we rely on batch-size + admission rather than SIGKILL. The old absolute-duration kill-guard becomes **warn + stop admitting + alert**. Wedged network calls (the more likely "stuck" case) are bounded by per-call timeouts/retry.

## 8. Transient-failure map → retry boundaries

| Category | Where | Today | Recommended |
|----------|-------|-------|-------------|
| L1/L2/op-node RPC blip (idempotent) | `fetcher.rs` headers, `debug_getRawBlock`, `optimism_outputAtBlock`, `safeHeadAtL1Block` binary search, `get_host_args` proofs | none (`FIXME:99`) | `RetryBackoffLayer` + `network_call_with_timeout`; runs before witness-gen |
| Late mid-pipeline L1/beacon fetch | `host.run` via kona `OnlineHostBackend`; `OnlineBlobStore` | none; aborts after ~1 h | inline `RetryBackoffLayer`; on `host.run` success cache `WitnessData` so a later (execute) failure skips it |
| EigenDA canoe proof (end of `host.run`) | `eigenda/.../witness_generator.rs:161-192` (fresh L1 client + recursion proof) | none | cache `WitnessData` (incl. proof) so a success-then-late-failure rerun skips it; a `host.run` failure retries with inline RPC retry; (opt.) memoize canoe by input-hash |
| SP1 execution OOM | `cost_estimator.rs:152` rayon par_iter | subprocess SIGKILL isolation | **do not retry**; bound by batch-size + admission (§7) |
| RPC backend desync (gameCount/gameAtIndex) | discovery | mitigated by finalized reads + `--delay` | preserve verbatim |

**Retry boundaries (smallest re-doable unit first):** per-RPC-call → per-sub-range `host.run` (on success cached as `WitnessData`) → per-range execute (don't retry OOM) → per-game (`maybe_requeue`, rarely fires).

## 9. Preserved vs changed

- **Preserved:** finalized index polling, type-42 filter, discover→execute `--delay`, `last_contiguous` semantics, `progress.json` resume, Primary/Background retry policy.
- **Changed:** subprocess + log-scrape → in-process structured `Result`; **cold, on-demand witness-gen → predictive pipeline that prebuilds stdin as blocks finalize**; SIGKILL supervision → cooperative cancellation + RSS admission covering witness-gen *and* execute; `log` → `tracing` spans to stdout; CSV/log IPC removed.

## 10. Testing strategy

- **Unit:** safe-head splitting (boundary alignment + `max_range_size`); witness-cache hit/miss for both artifacts + separate-clock pruning; `EstimatorError` classification; RSS projection from synthetic history; cgroup budget parsing (v1 + v2 fixtures); window prediction from `PROPOSAL_INTERVAL`.
- **Integration:** run a small real L2 range in-process and assert aggregated stats match a `cost_estimator` baseline; assert the pipeline prebuilds a range's stdin once its end finalizes and the executor then **skips `host.run`** (cache hit); inject a forced mid-build RPC failure and assert recovery without redoing a cached step; assert stdin prune evicts after grace and `WitnessData` is dropped once stdin is built.

## 11. Risks & mitigations

- **Dual memory pressure (D2):** witness-gen and execute now share one address space with no SIGKILL. → conservative batch-size, RSS admission covering both, harden the `Mutex`/KZG `.unwrap()`s in `online_blob_store.rs`.
- **No cooperative cancellation of execute:** a runaway pure-CPU execution can't be stopped. → batch-size bounds runtime; network hangs handled by timeouts; absolute-duration becomes warn+alert.
- **Prediction accuracy:** a wrong `PROPOSAL_INTERVAL` (config drift, off-cadence game) prebuilds ranges that never hit. → best-effort only (on-chain game is source of truth); executor rebuilds on miss; cap pipeline lead distance; prune speculative misses by age.
- **Witness-cache correctness:** stale/cross-DA hits produce wrong witness. → key on `chain_id/start/end/da_type`; absolute base dir; persist only when `l1_head` is finalized-stable; invalidate on witness-schema change.
- **kona external retry unknown:** the kona `OnlineHostBackend` retry/timeout behaviour lives in the external dependency; layering our retries could double-retry. → confirm against the pinned kona-host version during implementation; prefer wrapping at our boundaries.
- **Behavioural compatibility:** must preserve finalized reads, type-42, `--delay`, frontier semantics or reintroduce the 404 race / frontier stall.

## 12. Open questions / future work

- Retrofit `cost_estimator.rs` to call `utils/estimator` once the shape stabilises (deferred per D7).
- Wire the `WitnessData`/`SP1Stdin` cache + retry into the live validity proposer (out of scope now).
- Optionally execute speculatively (not just build) for predicted games, if lookahead on the ~50 GB step is worth the speculative memory.
- True per-channel/span-batch alignment if safe-head granularity proves insufficient.
- Metrics/DB backend for run history and health.
- Confirm whether operators depend on the replayable log-header / `rerun-cost-estimator.sh` before dropping it.

## 13. Key references (file:line anchors)

- Monitor control plane: `game_monitor.rs` — `run` 1361-1571, `spawn_cost_estimator` 1147-1208, `fetch_game_data` 1101-1140, `maybe_requeue` 708-781, supervision 597-634, `FAILURE_PATTERNS` 1031-1037.
- Estimator core: `cost_estimator.rs` 35-251 (lift), 253-348 (main), SP1 execute 132-143, witness_cache gating 90-117, split selection 286-293.
- Host/witness: `utils/host/src/host.rs:66-143` (`fetch`/`run`); `witness_generation/{traits.rs:39-89, preimage_witness_collector.rs:11-64, online_blob_store.rs:13-54}`; `utils/client/src/witness/{mod.rs:44-88, preimage_store.rs:46-58}`.
- EigenDA witness gen: `utils/eigenda/host/src/witness_generator.rs` — `run`/canoe gen 100-204 (canoe 161-192), `get_sp1_stdin` 70-98; EigenDA host `fetch` `utils/eigenda/host/src/host.rs:27-46`.
- Boot info / range encoding: `utils/host/src/fetcher.rs:874-963` (`get_host_args` → `SingleChainHost`).
- Splitting: `utils/host/src/block_range.rs:88-180`.
- Cache: `utils/host/src/witness_cache.rs:17-67`.
- Fetcher: `utils/host/src/fetcher.rs` — FIXME 99, providers 100-107/182-198, safe-head 723-813.
- Retry helper: `fault-proof/src/prover.rs:203-256`. RetryBackoffLayer dep: `Cargo.toml:~160`.
- Proof/host init + ELF: `utils/proof/src/lib.rs:46-78`.
- Proposal cadence: `celo/create_new_game_branch.sh:83` (`l2 = parent + PROPOSAL_INTERVAL`).
