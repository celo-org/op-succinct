# Game Monitor (embedded) — outstanding work

Status of the `game-monitor-embedded` daemon against its spec
(`docs/superpowers/specs/2026-06-10-game-monitor-contained-design.md`) and plan
(`docs/superpowers/plans/2026-06-10-contained-game-monitor.md`).

Every item below was verified against the source on the `piersy/game-monitor-rebuild`
branch; line numbers are current as of that branch. Module path:
`scripts/utils/src/game_monitor_embedded/`.

> Note on the original audit: it was a single pass and contained errors. Two claims were
> struck as **false** (discovery is not finalized-pinned in the legacy monitor either, so
> there is no regression; and the cache key *does* include `chain_id`). Several were
> downgraded. This document only lists items confirmed still-present by a second pass.

---

## Already fixed (this branch)

| Fix | Commit |
|-----|--------|
| Memory-model persistence created its parent dir (was failing silently every tick) | `053d94b3` |
| Startup `on_chain_count - 1` underflow + fatal seed-fetch on a fresh chain | `240836aa` |
| Per-game / per-range / per-attempt tracing spans; `host.run`/`get_sp1_stdin`/`execute` child spans; explicit `tracing-log` bridge | `58564bd8` |
| Log permanent abandonment of aged-out background retries | `d34eb5b2` |
| `get_l2_block_data_range` no longer panics on a missing block (now a retryable error) | `20d92420` |
| `execute_range` no longer re-fetches block data — executor threads it in (item #8) | `1e03045f` |
| Daemon split with `split_range_basic`; SafeDB dependency removed (item #3) | `7702f04c` |
| Admission liveness floor: a poisoned `max_cost_per_gas` can no longer wedge admission | `6863f30d` |
| SP1 executor logs attributed to the `execute` span (entered inside `spawn_blocking`) | `09d4de7a` |
| Per-kind admission cost model (Build vs Execute), `alpha` removed; readable GiB logs (item #6) | `c7da269d` |

---

## Outstanding gaps

### Important

#### 1. `get_l2_block_data_range` panics on a missing block — FIXED (`20d92420`)
A transient L2 RPC returning `Ok(None)` panicked the whole daemon. The `.unwrap()` at
`utils/host/src/fetcher.rs:269` is now `.ok_or_else(|| anyhow!("L2 block {block_number} not
found"))?`, so it surfaces as a retryable `EstimatorError::Transient`.

#### 2. No watchdog: log if a process runs for too long
Nothing tracks per-execute runtime, so a wedged/frozen execute is never surfaced (spec §7,
§11; plan Task 24). The memory gate is silent and won't catch it, and since SP1 execute
can't be killed (line 204), a logged alert is what a liveness probe (#4) needs to restart
the pod.
- **Evidence:** `mod.rs:43` docstring — "there is no watchdog or freeze". No
  `max_process_duration` anywhere.
- **Fix:** track each in-flight execute's start time; on overrun emit `tracing::error!`.
  (Don't gate admission — the count cap and memory projection already throttle a stuck
  unit, which keeps holding its slot and gas.)

#### 3. SafeDB dependency removed from the daemon — FIXED
The daemon no longer uses safe-head splitting at all. Both the executor and the predictive
pipeline now split with `split_range_basic` (a pure `start + k*batch_size` chop, anchored at
the game/window start), so the two splits are identical and the cache keys line up with no
SafeDB RPCs.
- **Splitting:** `executor.rs:47` and `ready_range_provider.rs:97` call `split_range_basic`;
  the safe-head splitters are gone from the daemon (still used by `cost_estimator.rs`).
- **Soundness gate:** `ready_range_provider.rs` now calls `get_l1_head(range.end, true)`,
  which uses SafeDB when present and otherwise falls back to timestamp-based L1-head
  estimation — matching what the executor's witness bakes in — so the daemon needs no SafeDB.

#### 4. No Kubernetes liveness / readiness probes
A wedged process is never restarted by k8s.
- **Evidence:** `infrastructure/helm-charts/succinct-game-monitor-embedded/templates/statefulset.yaml`
  — container block has no `livenessProbe`/`readinessProbe`.
- **Fix:** add probes (the daemon has no HTTP port today, so an `exec`/process-liveness
  probe, or add a tiny health endpoint).

#### 5. Predictive pipeline builds sub-ranges serially
Spec §4.2 step 4 ("Bounded concurrency. Multiple sub-range builds run concurrently") is
unimplemented — the prefetch half cannot keep ahead of the executor under load.
- **Site:** `mod.rs:474` serial `while let Some(range) = provider.next_range(...)`, each
  `pipeline_step` awaited before the next (comment at `mod.rs:473` "Serial for now").
- **Fix:** drive N concurrent `pipeline_step`s bounded by the same RSS admission gate.

#### 6. Single global `max_cost_per_gas`, not per-`WorkKind` — FIXED
Build and execute footprints differ ~10×; the old single learned coefficient (folded by a
fixed `alpha`) was learned from memory-heavy builds and then applied undiscounted to
executes, over-projecting them ~10× and starving them (observed on chaos-testnet). Admission
now keeps a `CostModel { cost_per_gas_build, cost_per_gas_execute }`, each a running max
learned only from **pure single-kind** RSS samples; the projection charges each kind its own
cost. `alpha` / `--build-gas-weight` is removed (per-kind costs make it unnecessary). Until a
kind's cost is learned, admission stays serial for it. Projection logs now render GiB and log
grants at INFO, waits at DEBUG.

### Minor

#### 7. No `WindowPredictor` / lead-distance cap — WON'T DO
Window prediction was folded into `ReadyRangeProvider` with no cap; the planned
`utils/estimator/src/window.rs` and `--max-lead-windows` flag never landed.
- **Sites:** `ready_range_provider.rs:84` advances `window_start += proposal_interval` with
  no lead check; no flag in `EmbeddedArgs` (`mod.rs:121-184`). `window.rs` absent.
- **Decision:** not doing it. The pipeline is already implicitly bounded by L2/L1
  finalization, so it cannot run away unboundedly; an explicit lead-distance cap adds no
  practical benefit.

#### 8. `execute_range` re-fetches block data on every call — FIXED (`1e03045f`)
`execute_range` ran `get_l2_block_data_range` for stats on every call, duplicating the fetch
the executor already does to compute the admission gas key. `execute_range` now takes
`block_data: &[BlockInfo]` and the executor (`executor.rs:71`) threads in the slice it
already fetched, halving L2 RPC round-trips per executed sub-range.

#### 9. Orphan stdin not pruned on `WrongType` / `Fatal`
`prunables.push` runs only on `Success`; a `Fatal`-after-partial-execution leaves cached
stdin on disk indefinitely. No age-based sweep reclaims orphans.
- **Sites:** push at `mod.rs:278` (Success only); `mod.rs:283` (WrongType) and `mod.rs:309`
  (Fatal) call `complete_game` without scheduling a prune.
- **Caveat:** a `WrongType` (non-type-42) game usually never built stdin, so its leak is
  typically vacuous.

#### 10. `L1_HEAD_FINALITY_BUFFER` hard-coded
- **Site:** `ready_range_provider.rs:14` — `const L1_HEAD_FINALITY_BUFFER: u64 = 20;`. Linked
  to the host's `calculate_safe_l1_head` buffer by comment only; no flag, no compile-time tie.
- **Fix:** derive from / assert against the host constant, or expose a flag.

#### 11. `Mutex::lock().unwrap()` panic sites in admission
A poisoned mutex would crash the calling task.
- **Sites:** `admission.rs:140` (`admit`), `:225` (`observe`), `:264` (`persist`).
- **Fix:** handle the `PoisonError` (recover the guard) instead of `unwrap()`.

#### 12. `drop_witness` failure leaks the witness blob
On a `drop_witness` error after stdin is saved, the build returns `Transient`; the retry
short-circuits at `has_stdin` and never re-drops, leaking the `.bin` and logging a false
"build failed".
- **Sites:** `estimator.rs:89` (drop → Transient), short-circuit at `estimator.rs:45-47`.
- **Fix:** treat a post-stdin `drop_witness` failure as non-fatal (log + continue), and/or
  best-effort drop on the next sweep.

#### 13. Background retries bypass the `--delay` 404-race window
Primary discovery applies `--delay`; background drains set `executable_at: Instant::now()`.
- **Sites:** primary push `mod.rs:576` (`+ delay`); background drain push `mod.rs:597`
  (`Instant::now()`).
- **Fix:** apply the same delay when draining background retries.

#### 14. Residual `.expect()` panics in `block_range.rs`
- **Sites:** `block_range.rs:40` (`get_validated_block_range`), `:83`
  (`get_rolling_block_range`).
- **Caveat:** **not on the embedded daemon's hot path** — these functions are only called by
  `cost_estimator.rs`, `gen_sp1_test_artifacts.rs`, and prove tests, never by
  `game_monitor_embedded`. Low priority; listed for completeness.

### Testing

#### 15. Parity test checks shape only
`execute_game_aggregates_over_real_range`
(`scripts/utils/tests/game_monitor_embedded_integration.rs:99-133`) asserts `batch_end`,
`nb_blocks`, `total_instruction_count > 0`, `!ranges.is_empty()` — never compares against a
real `cost-estimator` baseline (plan Task 22 Step 1 recommended it as optional).

#### 16. Forced-failure recovery test is degraded
`second_build_is_noop_fast_path`
(`integration.rs:165-186`) only asserts a second `build_range_witness` returns `Ok` with
cached stdin; it injects no failure and never proves `host.run` was skipped. (The plan had
already softened this from true fault injection.)

#### 17. `estimator.rs` has no unit tests
No `#[cfg(test)] mod tests` in `utils/estimator/src/estimator.rs`. `build_range_witness` /
`execute_range` are covered only by the env-gated integration test. (Plan intended
integration-only coverage — borderline, listed for completeness.)

#### 18. Safe-head splitter has no unit test
`utils/host/src/block_range.rs` has one test
(`basic_split_respects_max_range_and_covers_window`, line 228) covering only
`split_range_basic`; the safe-head splitter's boundary/`max_range_size` logic is untested.

### Deployment

#### 19. Dockerfile builds with default features on
`scripts/utils/Dockerfile.game-monitor-embedded:49` —
`cargo build --release --bin game-monitor-embedded --features eigenda` — pulls in the
default `ethereum` DA alongside `eigenda`. Plan line 17 required
`--no-default-features --features eigenda`.

#### 20. Dockerfile has no `ENTRYPOINT` / `CMD`
`Dockerfile.game-monitor-embedded` ends at the binary `COPY` (line 63). `docker run`
produces nothing; the run command comes from Helm `command:`. Harmless in k8s, surprising
elsewhere.

#### 21. No `terminationGracePeriodSeconds`
Absent from the chart. With `replicas: 1` and an SP1 execute that cannot be cancelled
(spec §7), a rolling restart SIGKILLs in-flight work after the default 30 s.

#### 22. No container `securityContext`
No `runAsNonRoot` / `podSecurityContext` / `securityContext` in the chart — the container
runs as image default (root).

---

## Not gaps (by design / verified benign)

- **Cache key** includes `chain_id` (`cache.rs:44`), `start`, `end`, `da_type` — as
  specified. Only `l1_head` is omitted, deliberately.
- **`log` → `tracing` bridge** is active (`tracing-subscriber`'s `tracing-log` feature +
  `.init()` install a `LogTracer`); now pinned explicitly.
- **Discovery reads** (`gameAtIndex`/`l2BlockNumber`/`startingBlockNumber`) are not
  finalized-pinned — matching the legacy monitor exactly. Only `gameCount` is pinned, in
  both. No regression.
- **Frontier seeded once, never resynced** — spec §4.2 describes best-effort prediction and
  does not mandate resync; the executor rebuilds on a miss.
- **`memory_model.json` vs `completion_history.json`** — different filename from the spec's
  prose, but persistence now works; cosmetic.
- **`resources: {}`** in `values.yaml:55` — intentional; memory limit is set per overlay.
- **`join_all` fan-out** in `executor.rs` — bounded by the admission gate; acceptable.
- **No migration/cutover doc** — neither spec nor plan required one (the plan only requires
  leaving the legacy monitor untouched).

## Deliberate future work (spec §12 — intentionally deferred)

- Retrofit `cost_estimator.rs` to call the `utils/estimator` library.
- Wire the cache + retry into the live validity proposer.
- Speculative execute for predicted games.
- True per-channel / span-batch range alignment (safe-head granularity is the chosen scope).
- Metrics / DB backend for run history and health.
- Canoe proof memoization by input-hash.
- Force-kill of in-flight SP1 execute (accepted tradeoff).
- Cross-DA support beyond EigenDA.
