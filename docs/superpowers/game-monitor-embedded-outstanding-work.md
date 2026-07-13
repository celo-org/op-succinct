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
| Per-kind admission cost model (Build vs Execute), `alpha` removed; readable GiB logs (item #6) | `cec85b11` |
| Concurrent predictive prebuild pipeline, gated by the shared memory admission gate (item #5) | `3e4fd211` |
| Game-task panics caught and requeued as Transient — no more slot leak / wedge (item #23) | `763a641e` |

---

## Outstanding gaps

### Important

#### 23. A panicking game task leaks its concurrency slot → permanent execute deadlock — FIXED (`763a641e`)
Was highest severity — observed wedging chaos-testnet for 8+ hours with zero completions. Each game
runs in a spawned task that reports its outcome exactly once via `tx.send(result)` (`mod.rs:807`),
and the main loop frees the game's slot only when that result arrives (`running_games.remove`,
`mod.rs:574`). If `execute_game` **panics** (`mod.rs:774`) instead of returning `Err` — e.g. a hard
`unwrap` deep in kona during on-demand witness build — the task unwinds before the send, so no
`GameTaskResult` is ever emitted and the slot is never released. There is no `catch_unwind` or
`JoinSet`/`JoinHandle` tracking. Once `max_concurrent_games` panics accumulate, `running_games` is
permanently full, the spawn guard (`mod.rs:711`, `running_games.len() < max_concurrent_games`) is
always false, and no game is scheduled again: `executing=N, active_executes=0`, watermark frozen,
`pending` unbounded — while the main loop and the *separate* prebuild pipeline keep running, so the
pod still looks alive.
- **Evidence:** 2026-07-09 incident — 5× `thread 'tokio-rt-worker' panicked at
  kona/.../providers-alloy/src/blobs.rs:61: Failed to load genesis time from beacon client:
  Backend("HTTP request failed: error decoding response body")` between 00:23–00:29 UTC leaked all
  5 slots; last `game executed` 00:28:54 UTC, then wedged 8.7 h with the loop still polling.
- **Trigger vs bug:** the beacon-client HTTP flake is transient; the monitor converts it into a
  permanent deadlock via the slot leak. (kona's `unwrap` is upstream under `~/.cargo` — not ours.)
- **Fix (Option A, `763a641e`):** the spawned body is wrapped in `catch_game_panic`
  (`AssertUnwindSafe(..).catch_unwind()`); a caught panic is logged at `error!` and mapped to a
  `Transient` `GameTaskResult`, so the existing slot-release and two-tier retry still run. Transient
  self-heals a flaky-dependency panic and is bounded for a deterministic one (retry budget →
  background → age-out). SP1-execute panics are unaffected — they already surface as a `JoinError`
  via `spawn_blocking`. Unit-tested (`panicking_game_body_is_caught_as_transient`).
- **Root cause also fixed upstream:** celo-kona now loads the beacon genesis/slot config under
  backoff instead of panicking (celo-kona #239, cherry-picked onto `game-monitor-improvements`), so
  the specific trigger no longer fires. Option B (`JoinSet` slot tracking) and the watchdog (#2)
  remain as deferred hardening that would also cover this class.

#### 1. `get_l2_block_data_range` panics on a missing block — FIXED (`20d92420`)
A transient L2 RPC returning `Ok(None)` panicked the whole daemon. The `.unwrap()` at
`utils/host/src/fetcher.rs:269` is now `.ok_or_else(|| anyhow!("L2 block {block_number} not
found"))?`, so it surfaces as a retryable `EstimatorError::Transient`.

#### 2. No watchdog: log if a process runs for too long — DEFERRED
Nothing tracks per-execute runtime, so a wedged/frozen execute is never surfaced (spec §7,
§11; plan Task 24). The memory gate is silent and won't catch it, and since SP1 execute
can't be killed (line 204), a logged alert is what a liveness probe (#4) needs to restart
the pod.
- **Evidence:** `mod.rs:43` docstring — "there is no watchdog or freeze". No
  `max_process_duration` anywhere.
- **Fix:** track each in-flight execute's start time; on overrun emit `tracing::error!`.
  (Don't gate admission — the count cap and memory projection already throttle a stuck
  unit, which keeps holding its slot and gas.)
- **Deferred:** with the panic slot-leak (#23) and its beacon trigger fixed, the acute wedge this
  guarded against is gone; revisit if a genuinely frozen/slow execute becomes a problem.

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

#### 4. No Kubernetes liveness / readiness probes — DEFERRED
A wedged process is never restarted by k8s.
- **Evidence:** `infrastructure/helm-charts/succinct-game-monitor-embedded/templates/statefulset.yaml`
  — container block has no `livenessProbe`/`readinessProbe`.
- **Fix:** add probes (the daemon has no HTTP port today, so an `exec`/process-liveness
  probe, or add a tiny health endpoint).
- **Deferred:** paired with #2 (auto-restart hardening); deferred for the same reason.

#### 5. Predictive pipeline builds sub-ranges serially — FIXED (`3e4fd211`)
Spec §4.2 step 4 ("Bounded concurrency. Multiple sub-range builds run concurrently") is now
implemented, so the prefetch half can keep ahead of the executor under load. The pipeline
task collects the ready sub-ranges with a sequential cursor walk, then builds them
concurrently, capped at `max_concurrent_builds` (default `--max-concurrent-units`), with the
shared RSS admission gate as the real governor of how many run at once.
- **Sites:** `mod.rs:513` collects ready ranges (`while let Some(range) =
  provider.next_range(...)`); `mod.rs:520-521` drives the builds via
  `for_each_concurrent(max_concurrent_builds, ...)`; fan-out default at `mod.rs:505-506`; each
  `pipeline_step` still awaits `admission.admit(WorkKind::Build, ..)` (`pipeline.rs:51`).

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

#### 27. Above-watermark completions are not persisted (re-run after restart)
Successful executions ahead of the contiguous watermark live only in `SequenceTracker`'s in-memory
`pending: HashSet<u64>` (`sequence_tracker.rs:9`, advanced in `add()` at `:24-34`). `ProgressState`
persists only `last_contiguous` + `background_retries` (`state.rs:38-43`); `save_progress` writes
`tracker.end()` (`state.rs:124`), never the `pending` set. On restart `resume_index` returns
`last_contiguous + 1` (`state.rs:106`) with an empty tracker, so every out-of-order success from the
watermark up is re-discovered and re-executed. Idempotent and usually cache-cheap (witness/stdin
cache survives on the PVC), but wasteful — and a genuine re-compute if that range's stdin was
pruned/evicted or the PVC was wiped. Amplified by newest-first scheduling, which keeps the watermark
trailing and the above-watermark set large.
- **Fix:** serialize the above-watermark set — add a `pending: Vec<u64>` to `ProgressState` and an
  accessor on `SequenceTracker`, rehydrate it on resume so already-executed games are skipped (still
  re-scan from `last_contiguous + 1`, but short-circuit any index already in the restored set).
- **Relates to:** #26 (scheduling) and the watermark trade-off. Low risk, contained to
  `sequence_tracker.rs` + `state.rs`.

#### 28. Admission `baseline_bytes` is measured once at startup (too early) → mis-calibrated projection
`baseline_bytes` is read once in `load()` before any work runs (`admission.rs:152-153`) and then
frozen (`:130`, `:168`). At that instant the process hasn't warmed up (heap/allocator high-water,
resident buffers), so it captures ~50 MB — while the RSS debug data shows a real steady-state idle
floor of ~2.5–5 GiB that accumulates afterwards and is never re-measured. With `baseline ≈ 0`,
`observe()` learns each kind's `cost_per_gas` from `(rss − ~0)/gas` (`:284`), folding the fixed floor
into the per-gas slope — a through-origin fit to affine data (`rss ≈ F + s·gas`). Net effect:
`project()` (`:261-263`) under-projects at low concurrency (misses the floor → over-admits) and
over-projects at high (error ∝ `F·(gas/g_ep − 1)`).
- **Fix (learn it, don't hardcode):** treat the floor like the costs — an EWMA of *idle* RSS. In
  `observe()`, when nothing is in flight (`sb == 0 && se == 0`), fold the current `rss` into the
  baseline via `fold_ewma`/`EWMA_ALPHA`. Idle ticks never fold a cost, so it slots in cleanly beside
  the per-kind episode logic. Move `baseline_bytes` into `CostModel` (already behind the `self.cost`
  mutex `observe` holds, and `project` already takes `&CostModel`) so it is mutable, persisted and
  re-seeded across restarts; keep the startup read as the initial seed. Then `net = rss − baseline`
  yields the true marginal `cost_per_gas` and `project` becomes a correct `floor + slope·gas` at all
  concurrency levels.
- **Edge cases:** never-idle → hold last estimate (the RSS data shows ~12–16% of ticks fully idle, so
  there is plenty to learn from; avoid a min-over-window fallback that conflates idle with low-work);
  when the baseline rises, the previously-inflated `cost_per_gas` must re-converge (the EWMA handles
  it — reset/ignore the persisted model once on first deploy); use EWMA (not min/max) so a transient
  dip cannot crater the floor.
- **Evidence:** 20k `admission rss sample` points (09:00–15:30 BST, `sha-dc929b7`) — fixed floor
  ~2.5–5 GiB vs model baseline ~50 MB (`net_gib ≈ rss_gib − 0.05`).
- **Relates to:** #6 (per-kind cost model), #26 (scheduling). Contained to `admission.rs`.

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

### Enhancements

#### 24. Speculative execute for predicted games
The prebuild pipeline already builds witnesses ahead of a game appearing, but the expensive zk
`execute` only runs once the game is discovered on-chain (`pending_games` fed from `gameCount`,
`mod.rs:635-658`) and passes the readiness gate. `execute_range`'s `ExecutionStats` is a pure
function of the range, not the game index (`estimator.rs:101-138`, `executor.rs:47`/`68`), so a
predicted range can be executed ahead of time and reused when its game appears — hiding execute
latency the way the pipeline hides build latency.
- **Sketch:** after the pipeline builds a ready range, also `execute_range` it and cache the
  `ExecutionStats` keyed by `(chain_id, start, end, da)` (mirror `WitnessCache`, persisted). At
  discovery, map the game's range → cached result → attribute and advance the `SequenceTracker`,
  else fall back to on-demand (same best-effort pattern as the witness cache).
- **Must-haves:** SP1 execute is uncancellable, so speculation must not head-of-line-block real
  games — reserve execute capacity for discovered games and add a lead cap (item #7's "won't do"
  reasoning flips here: over-executing is expensive). A misprediction wastes the dominant execute
  cost, not just a build (bounded by prediction accuracy — the same bet the prebuild makes). Sound
  for estimation; reusing a speculative result as a real proof would need the game's committed L1
  anchor to match the witness's baked-in `l1_head`.
- **Status:** promoted from spec §12 (deferred) to an active todo.

#### 25. Canoe proof memoization by input-hash
Witness build produces canoe proofs per range (seen in `host.run`: "canoe witness provider:
producing N canoe proof(s) for M DA certs"); identical inputs are re-proved across overlapping or
retried builds. Memoize canoe proofs keyed by a hash of their input so a repeat input reuses the
cached proof instead of recomputing.
- **Fix:** locate where the canoe proof is produced in the witness/DA path and wrap it in an
  input-hash-keyed cache (persisted alongside the witness cache); consult before proving.
- **Status:** promoted from spec §12 (deferred) to an active todo.

#### 26. Cache-fronted scheduling: LIFO games + per-type FIFO-priority / LIFO-speculative queues
Reshapes prioritization around two result caches — a **witness cache** and a **proof cache** —
both fillable ahead of a game landing on-chain (the proposer's ranges and their splits are
predictable from chain progression). Replaces today's unordered admission poll-race
(`admission.rs:184-255`), which has no game/kind ordering and lets the prebuild pipeline
(`max_concurrent_builds` defaults to the whole gate, `mod.rs:505-506`) starve real executes
(observed `active_witness=6` vs `active_prove=2` at startup).

Model:
- **Per work type (witness build, proof execute)** a worker pool drains two feeders in strict
  order: (1) an **on-demand FIFO priority queue** — ranges a currently-executing game needs that
  are not cached and not already in flight; (2) a **speculative LIFO queue** — fed only by chain
  progression, newest available range pushed to the front. Workers pull from the FIFO priority
  queue first; only when it is empty do they pull from the LIFO speculative queue.
- **Games** sit on their own **LIFO queue** (newest first). Pop a game → look up its components in
  the caches: all present → it completes almost instantly (assemble cached results); anything
  missing (and not already in flight) → enqueue those pieces onto the FIFO priority queue for the
  relevant type (witness and/or proof).

Why: games LIFO ⇒ newest games start first; FIFO-priority-before-LIFO-speculative ⇒ once a game is
underway its dependencies leap ahead of all speculative work; the priority queue is **FIFO, not
LIFO**, so components are served in the order games demanded them — an in-flight game's needs are
satisfied before a later-started game's, so newer games / new speculative work cannot starve a game
already executing. Speculative pre-compute only ever consumes spare capacity.

Subsumes/relates to: the prove-priority + depth-first concern (raised against the current gate);
generalises the prebuild pipeline (#5) and speculative execute (#24) into the two LIFO-speculative
feeders; the proof cache is where #24's `ExecutionStats` would live.

Open questions for the plan:
- **Capacity:** queues are per-type, but do witness-build and proof-execute share one
  memory/concurrency budget (today's single admission gate) or become independent pools? They still
  contend for RAM either way.
- **Witness→proof dependency:** proving a range needs its witness first — does a proof demand for an
  un-built range promote a witness demand that feeds it, or does the proof worker build inline on a
  miss (as `execute_range` does today)?
- **Watermark:** newest-first keeps the contiguous completion watermark trailing (oldest games
  finish last). Accept per-game latency as the goal, or pair with oldest-first selection for
  watermark progress?
- **Status:** design captured; not yet planned/implemented.

#### 29. Kind-aware admission gate: separate per-kind count caps — first step done (`c1baa130`)
The shared `max_concurrent` count cap was kind-blind: a build unit (~2 GiB, ~250 B/gas from the RSS
data) and an execute unit (~8–12 GiB, ~2000 B/gas envelope) each counted as one slot, so the cheap
prebuild pipeline could hold slots expensive executes needed (`active_witness=6, active_prove=2`).
The count caps stay the **primary** concurrency control — memory is too hard to estimate reliably to
lean on — but they become per-kind and separately sized.
- **Why the count cap stays primary (not memory-gated):** the memory projection is only a scattered
  EWMA (R²≈0.63) with a mis-calibrated baseline (#28), so leaning on it would over-admit and OOM when
  it under-projects — and an OOM SIGKILLs an uncancellable execute + restarts the pod. The count cap
  is also the CPU/fd/RPC bound the memory model ignores, and cold start has no projection to gate on
  at all. So memory stays a *secondary* safety bound (a unit must pass both the count cap and
  `fits`), never the primary knob.
- **Plan:** give build and execute **separate, independently tunable caps** (distinct config knobs)
  instead of both reusing `max_concurrent`. Size each to its real footprint — executes to the
  memory/CPU ceiling, builds looser (cheap in RAM, already bounded by the pipeline's
  `max_concurrent_builds` fan-out). The memory projection (`fits`) is retained as-is, as the
  secondary safety bound.
- **Done (`c1baa130`):** the count cap is now per-kind — build and execute units are each capped
  independently, so builds no longer consume execute slots; memory still bounds the two jointly.
  Interim: both kinds still reuse `max_concurrent`. Tested (`builds_do_not_consume_execute_slots`).
- **Follow-up:** add distinct config for the build vs execute caps (e.g. a gate-side build cap,
  separate from the pipeline fan-out, plus an execute cap) so each is sized on purpose rather than
  sharing `max_concurrent`.
- **Relates to:** #6 (per-kind cost), #26 (scheduling sits on this gate), #28 (baseline — sharpens
  the *secondary* memory bound, but the caps stay primary).

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
- True per-channel / span-batch range alignment (safe-head granularity is the chosen scope).
- Metrics / DB backend for run history and health.
- Force-kill of in-flight SP1 execute (accepted tradeoff).
- Cross-DA support beyond EigenDA.
