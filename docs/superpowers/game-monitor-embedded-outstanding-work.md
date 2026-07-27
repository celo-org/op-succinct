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
| Admission cost is an EWMA (α=0.1) of per-episode peak bytes/gas, not a running max (item #6) | `c3c40601` |
| Shared L2+L1 readiness gate extracted to `readiness.rs` (items #3, #10) | `f1a6ba85` |
| `--delay` renamed `--retry-backoff-delay`; discovery no longer delays, only the retry path does (item #13) | `d034b638` |
| Per-kind admission concurrency cap so builds don't starve executes (item #29) | `c1baa130` |
| Concurrent predictive prebuild pipeline, gated by the shared memory admission gate (item #5) | `3e4fd211` |
| Game-task panics caught and requeued as Transient — no more slot leak / wedge (item #23) | `763a641e` |
| Error classification typed: `Oom`→`TooMuchMemory`→`Sp1Execute(ExecutionError)` (deterministic ⇒ non-retryable); `MissingTrieNode` reclassified transient | `43084e7b`, `c19d7a24`, `b28799a4` |
| Stdin pruned immediately on success (grace window + `--stdin-grace-secs` removed); `Fatal` stdin retained for debugging (item #9) | `2c7b2125`, `2fa136ce` |
| Above-watermark completions persisted (`pending`); restart skips completed games (item #27) | `d18f55f8` |
| Readiness buffer and host offset unified into one shared `L1_HEAD_BUFFER` constant (item #10) | `7cd3a581`, `d28c38f7` |
| Post-stdin `drop_witness` made best-effort (no false build failure); short-circuit re-drops leaked blobs (item #12) | this change |
| Admission baseline learned as an EWMA of idle RSS (was a single mis-calibrated startup read), persisted in `CostModel` (item #28) | this change |
| Stale `--delay` module-doc mention corrected — the flag is `--retry-backoff-delay`, discovery is immediate (item #13) | this change |
| Admission `cost` mutex hardened — `cost_guard()` recovers the guard on poison instead of `unwrap()` panicking (item #11) | this change |
| Fast-path test now proves `host.run` is skipped by fault injection (an unbuildable range with pre-seeded stdin) (item #16) | this change |
| Parity test compares `execute_game` to a serial split→execute→aggregate reference field-for-field (`ExecutionStats: PartialEq`) (item #15) | this change |

---

## Outstanding gaps

### Important

#### 23. A panicking game task leaks its concurrency slot → permanent execute deadlock — FIXED (`763a641e`)
Was highest severity — observed wedging chaos-testnet for 8+ hours with zero completions. Each game
runs in a spawned task that reports its outcome exactly once via `tx.send(result)` (`mod.rs:872`),
and the main loop frees the game's slot only when that result arrives (`running_games.remove`,
`mod.rs:622`). If `execute_game` **panics** (`mod.rs:833`) instead of returning `Err` — e.g. a hard
`unwrap` deep in kona during on-demand witness build — the task unwinds before the send, so no
`GameTaskResult` is ever emitted and the slot is never released. There is no `catch_unwind` or
`JoinSet`/`JoinHandle` tracking. Once `max_concurrent_games` panics accumulate, `running_games` is
permanently full, the spawn guard (`mod.rs:759`, `running_games.len() < max_concurrent_games`) is
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
`utils/host/src/fetcher.rs:272` is now `.ok_or_else(|| anyhow!("L2 block {block_number} not
found"))?`, so it surfaces as a retryable `EstimatorError::Transient`.

#### 2. No watchdog: log if a process runs for too long — DEFERRED
Nothing tracks per-execute runtime, so a wedged/frozen execute is never surfaced (spec §7,
§11; plan Task 24). The memory gate is silent and won't catch it, and since SP1 execute
can't be killed (spec §7), a logged alert is what a liveness probe (#4) needs to restart
the pod.
- **Evidence:** `mod.rs:44` docstring — "there is no watchdog or freeze". No
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
- **Splitting:** `executor.rs:47` and `ready_range_provider.rs:73` call `split_range_basic`;
  the safe-head splitters are gone from the daemon (still used by `cost_estimator.rs`).
- **Soundness gate:** extracted into the shared `readiness.rs` (`f1a6ba85`). `range_ready`
  (`readiness.rs:40-53`) calls `get_l1_head(range.end, true)` (`readiness.rs:50`), which uses
  SafeDB when present and otherwise falls back to timestamp-based L1-head estimation — matching
  what the executor's witness bakes in — so the daemon needs no SafeDB; `ready_range_provider.rs:93`
  invokes it.

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
- **Sites:** `mod.rs:561` collects ready ranges (`while let Some(range) =
  provider.next_range(...)`); `mod.rs:568-569` drives the builds via
  `for_each_concurrent(max_concurrent_builds, ...)`; fan-out default at `mod.rs:553-554`; each
  `pipeline_step` still awaits `admission.admit(WorkKind::Build, ..)` (`pipeline.rs:51`).

#### 6. Single global `max_cost_per_gas`, not per-`WorkKind` — FIXED
Build and execute footprints differ ~10×; the old single learned coefficient (folded by a
fixed `alpha`) was learned from memory-heavy builds and then applied undiscounted to
executes, over-projecting them ~10× and starving them (observed on chaos-testnet). Admission
now keeps a `CostModel { cost_per_gas_build, cost_per_gas_execute }`, each an **EWMA (α=0.1) of
per-episode peak** bytes/gas learned only from **pure single-kind** RSS samples (`admission.rs:25-38`,
folded at `:300-311`; `c3c40601` replaced the original running max, which one outlier pinned
forever); the projection charges each kind its own
cost. `alpha` / `--build-gas-weight` is removed (per-kind costs make it unnecessary). Until a
kind's cost is learned, admission stays serial for it. Projection logs now render GiB and log
grants at INFO, waits at DEBUG.

#### 31. Witness build wedges forever on a deterministic hint-fetch error
kona's `OnlineHostBackend::get_preimage` (kona `bin/host/src/backend/online.rs:135-148`, rev
`b4ba5c3`) retries a failed `fetch_hint` in an unconditional busy loop — no backoff, no attempt
cap, no transient-vs-deterministic classification. A hint that fails **deterministically** spins
forever (~500 errors/s), and the witness build never returns, wedging its game slot and the
admission gas it holds indefinitely. Unlike #23 this is not a panic — the task is alive, so
`catch_game_panic` and the two-tier retry never see it.
- **Evidence:** 2026-07-16 Sepolia incident — an `L2StateNode` hint for one missing trie-node
  preimage hit reth's `debug_dbGet` (code-only, 33-byte keys), a deterministic `-32602: Key must
  be 33 bytes, got 32`; >17k `Failed to prefetch hint` errors in minutes, game `29347` stuck in
  witness build for hours (`executing_games=1 active_witness=1`, watermark frozen), pod healthy.
- **Mitigation (this change):** `enable_experimental_witness_endpoint: true` removes that specific
  trigger (complete `debug_executePayload` witness ⇒ no `L2StateNode` fallback), but any future
  deterministic hint failure re-creates the wedge.
- **Preferred fix (upstream kona):** in `OnlineHostBackend::get_preimage`, propagate a
  `fetch_hint` error instead of `continue`. celo-kona's handler already retries transients
  internally with backoff (`hint_retry_policy` / `is_retryable_transport_err`, from `f1c2289`),
  so any error escaping `fetch_hint` is non-transient by construction; propagating it turns the
  spin into a typed `host.run` failure that the monitor's existing requeue/retry policy bounds
  (Transient → retry budget → background → Fatal). Even a mis-classified transient is safe — it
  just burns one bounded retry instead of wedging.
- **Deliberately not done now:** kona lives in the celo-org/optimism fork, and we are moving to a
  new kona version we don't need to fork (celo-org/op-succinct#150) — carrying a fork patch just
  for this would be thrown away. Revisit as an upstream (op-labs kona) contribution, or re-judge
  after #150 lands.
- **Fallback (op-succinct-side, not implemented):** a timeout around `build_range_witness`
  (`host.run`) failing the build with a typed error. Blind to *why* the build hung, and covers the
  silent variant (hint returns `Ok` but the blocked-on key never appears) that error propagation
  does not; decided against for now.
- **Relates to:** #2 (watchdog — same detection need; this is the witness-build case), #23 (slot
  wedge class), #4 (a liveness probe would restart the pod but lose all in-flight work).

### Minor

#### 7. No `WindowPredictor` / lead-distance cap — WON'T DO
Window prediction was folded into `ReadyRangeProvider` with no cap; the planned
`utils/estimator/src/window.rs` and `--max-lead-windows` flag never landed.
- **Sites:** `ready_range_provider.rs:59` advances `window_start += proposal_interval` with
  no lead check; no flag in `EmbeddedArgs` (`mod.rs:127-208`). `window.rs` absent.
- **Decision:** not doing it. The pipeline is already implicitly bounded by L2/L1
  finalization, so it cannot run away unboundedly; an explicit lead-distance cap adds no
  practical benefit.

#### 8. `execute_range` re-fetches block data on every call — FIXED (`1e03045f`)
`execute_range` ran `get_l2_block_data_range` for stats on every call, duplicating the fetch
the executor already does to compute the admission gas key. `execute_range` now takes
`block_data: &[BlockInfo]` and the executor (`executor.rs:68`) threads in the slice it
already fetched, halving L2 RPC round-trips per executed sub-range.

#### 9. Stdin lifecycle on terminal outcomes — RESOLVED (by design)
The stdin lifecycle was reworked around the terminal outcomes:
- **`Success`** prunes its stdin **immediately** (`2c7b2125`); the grace window and
  `--stdin-grace-secs` were removed. A completed game is never re-run (completions are now
  persisted, #27) and games are contiguous (no overlapping-neighbour reuse), so a grace window
  bought nothing.
- **`Fatal`** deliberately **retains** its stdin (`2fa136ce`) so it can be fetched to iterate on
  the execution code locally; the size-cap GC reclaims it under space pressure (oldest-first,
  unprotected). This reversed the short-lived Fatal-prune of `0c9170fc`.
- **`WrongType`** never builds stdin — the type check precedes any build (`discovery.rs:49`) — so
  there is nothing to prune.
- The old "restart drops the in-memory prune list" leak is gone: pruning is immediate and
  completions are persisted, so there is no deferred list to lose.
- **Caveat:** reclaiming retained `Fatal` stdin needs `--max-cache-size > 0` (the chaos overlay
  sets `40GB`); with the default `0` it accumulates until the PVC fills.

#### 10. Readiness buffer duplicated the host's offset — FIXED (`7cd3a581`, `d28c38f7`)
The readiness gate's buffer and the host's `calculate_safe_l1_head` `+20` were separate literals
linked by comment only. They are now a single shared constant `L1_HEAD_BUFFER = 20`
(`utils/host/src/host.rs:17`), imported by both the readiness gate (`readiness.rs`) and the DA
hosts (`utils/{eigenda,ethereum}/host/src/host.rs`), so the gate cannot drift from the offset the
witness bakes in — the compile-time tie this asked for. The old `L1_HEAD_FINALITY_BUFFER` alias
was dropped. A flag was deemed unnecessary (the value must match the host, not be tuned freely).

#### 11. `Mutex::lock().unwrap()` panic sites in admission — FIXED
A poisoned `cost` mutex would crash the calling task, and one such panic cascades: every later
`admit` / `observe` / `persist` `unwrap()`s the `PoisonError` and panics too (silently killing the
sampler and the prebuild pipeline).
- **Fix:** all four production lock sites (`admit`, `observe`, the sampler `rss sample` log, and
  `persist`) now go through a `cost_guard()` helper that recovers the guard on poison
  (`lock().unwrap_or_else(|e| e.into_inner())`). `CostModel` is a learned heuristic with no fragile
  invariant, so proceeding on a possibly half-written value is fine — and the count cap plus margin
  still bound admission. Tests keep `unwrap()` (a poisoned test should fail loudly).
- **Relates to:** #30 (a `tracing` panic hook would surface the *root* panic that poisons it — this
  only stops the cascade).

#### 12. `drop_witness` failure leaked the witness blob and faked a build failure — FIXED
A post-stdin `drop_witness` error made `build_range_witness` return `Transient` even though stdin
was already durably cached — a false "build failed" (spurious retry) — and the leaked witness blob
was never reclaimed, because the retry short-circuits at `has_stdin` and never reached the drop.
- **Fix:** the post-stdin `drop_witness` is now **best-effort** — a failure is logged (`warn`) and
  the build returns `Ok`, since stdin is the durable product (`estimator.rs`, step 3). The
  `has_stdin` short-circuit now also **re-attempts the drop** best-effort, so a re-request of the
  range self-heals a previously-leaked blob; the size-cap GC remains the final backstop. Dropping
  is always safe once stdin exists (the witness is only needed to re-crunch stdin).

#### 13. `--delay` repurposed as retry backoff; discovery/background drains are immediate — FIXED
The `--delay` flag was renamed `--retry-backoff-delay` (`d034b638`) and no longer gates discovery.
Both primary discovery and background-retry drains push with `executable_at: Instant::now()`; the
backoff applies only to same-game requeues on the Primary retry path (`apply_requeue`, `+ delay`).
Background-retry spacing is governed by each entry's `next_attempt_at`. The original "primary
applies delay, background bypasses it" asymmetry no longer exists. The last residual — a stale
`--delay` mention in the module doc comment — is now corrected (this change).

#### 14. Residual `.expect()` panics in `block_range.rs` — WON'T DO (out of scope)
`.expect()` on `get_finalized_l2_block_number` at `block_range.rs:40` (`get_validated_block_range`)
and `:83` (`get_rolling_block_range`) panics on a legitimate `None`. But these functions are **not
called by `game_monitor_embedded`** — only by `cost_estimator.rs`, `gen_sp1_test_artifacts.rs`, and
prove tests. Per the scope rule (fix only what the embedded monitor uses), this is out of scope.

#### 27. Above-watermark completions are not persisted (re-run after restart) — FIXED (`d18f55f8`)
Out-of-order completions above the watermark lived only in `SequenceTracker`'s in-memory `pending`
set; `progress.json` stored only `last_contiguous`, so a restart re-discovered and re-executed
every above-watermark success. With stdin now pruned immediately on success (#9), those reruns
would be full recomputes — so persisting the set became load-bearing, not just an optimisation.
- **Fix:** `ProgressState` gained a serde-default `pending: Vec<u64>`; `SequenceTracker` gained
  `restore` / `contains` / `pending_indices`; `save_progress` writes the pending set; on resume
  (absent an explicit `--start-index`) the tracker is rehydrated from `last_contiguous + pending`
  and discovery skips any already-completed index. `Fatal`/`WrongType` complete via the tracker
  too, so they are persisted and never re-run either.
- **Relates to:** #9 (this is what makes immediate pruning safe), #26 (scheduling).

#### 28. Admission `baseline_bytes` was measured once at startup → mis-calibrated projection — FIXED
`baseline_bytes` used to be read once in `load()` before warm-up (~50 MB) and frozen, while the real
steady-state idle floor is ~2.5–5 GiB. With `baseline ≈ 0` the per-kind `cost_per_gas` folded the
fixed floor into the per-gas slope (a through-origin fit to affine `rss ≈ F + s·gas`), so `project`
under-projected at low concurrency (missed the floor → over-admit) and over-projected at high.
- **Fix:** the baseline is now **learned** like the costs — an EWMA of idle RSS. It moved into
  `CostModel` (`baseline_bytes: f64`, `serde(default)`, persisted and re-seeded across restarts);
  `observe()` folds the current `rss` into it whenever nothing is in flight and no episode is closing
  that tick (the transitional close tick, whose RSS still carries the just-finished work, is skipped).
  `net = rss − baseline` now yields the true marginal `cost_per_gas`, and `project = baseline +
  slope·gas` is correct at all concurrency levels. The startup read remains the initial seed until the
  first idle sample. EWMA (not min/max) means a not-yet-decayed reading can neither crater nor spike
  the floor.
- **Deploy note:** an old persisted model loads (baseline `serde`-defaults to 0 → re-seeded) with its
  `cost_per_gas` still folded-in; those decay to the true marginal as the baseline rises — the
  transition **over**-projects (conservative, safe), and the `--rss-margin-mb` (20 GiB) covers the
  ~3–5 GiB floor during warm-up regardless. Tests: `observe_learns_idle_baseline`, baseline round-trip
  in `persist_round_trips_per_kind_costs`.
- **Relates to:** #6 (per-kind cost model), #29 (sharpens the *secondary* memory bound; the count caps
  stay primary), #26 (scheduling).

### Testing

#### 15. Parity test checks shape only — FIXED
The parity test only asserted shape (`batch_end`, `nb_blocks`, `total_instruction_count > 0`).
- **Fix:** `execute_game_matches_serial_reference` now compares against a **serial reference** —
  it splits the window with `split_range_basic`, `execute_range`s each sub-range independently, and
  aggregates, then asserts `execute_game`'s concurrent split-and-aggregate equals it **field-for-
  field** (`ExecutionStats` gained `PartialEq`/`Eq`). It also asserts the split matches
  `split_range_basic` exactly and `batch_start`/`batch_end`/`nb_blocks` bound the window. Execution
  is deterministic and aggregation is an order-independent sum, so the equality is exact.
- **Not a cross-tool baseline (by design/scope):** `cost_estimator` is not daemon code and is not
  retrofitted to `utils/estimator` (spec §12), and it splits differently (safe-head vs
  `split_range_basic`) so it is not apples-to-apples. Its aggregation is the same one this reuses
  (`stats.rs`), so the serial reference is the faithful in-scope check.

#### 16. Forced-failure recovery test is degraded — FIXED
The test used to only assert a second `build_range_witness` returns `Ok` with cached stdin — it
injected no failure and never proved `host.run` was skipped.
- **Fix:** `second_build_short_circuits_host_run` now proves the skip by fault injection. It builds
  the real range (caching stdin), then takes a range the host *cannot* build (block numbers far
  beyond any chain height): without a cached stdin that build **fails** (host path exercised), but
  with its stdin pre-seeded it returns `Ok` — reachable only via the `has_stdin` short-circuit
  returning before `host.fetch`/`host.run`. Still ENV-gated (needs `OPS_IT_*`), skips cleanly in CI.

#### 17. `estimator.rs` has no unit tests
No `#[cfg(test)] mod tests` in `utils/estimator/src/estimator.rs`. `build_range_witness` /
`execute_range` are covered only by the env-gated integration test. (Plan intended
integration-only coverage — borderline, listed for completeness.)

#### 18. Safe-head splitter has no unit test — WON'T DO (out of scope)
`utils/host/src/block_range.rs` has one test (`basic_split_respects_max_range_and_covers_window`)
covering only `split_range_basic`; the safe-head splitter is untested. But the daemon uses
`split_range_basic`, not the safe-head splitter (removed from the daemon in #3; only `cost_estimator`
still calls it). Per the scope rule (test only what the embedded monitor uses), this is out of scope.

### Deployment

#### 19. Dockerfile builds with default features on
`scripts/utils/Dockerfile.game-monitor-embedded:55` —
`cargo build --release --bin game-monitor-embedded --features eigenda` — pulls in the
default `ethereum` DA alongside `eigenda`. Plan line 17 required
`--no-default-features --features eigenda`.

#### 20. Dockerfile has no `ENTRYPOINT` / `CMD`
`Dockerfile.game-monitor-embedded` ends at the binary `COPY` (line 69). `docker run`
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
`mod.rs:695-706`) and passes the readiness gate. `execute_range`'s `ExecutionStats` is a pure
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
(`admission.rs:186-265`), which has no game/kind ordering and lets the prebuild pipeline
(`max_concurrent_builds` defaults to the whole gate, `mod.rs:553-554`) starve real executes
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

#### 30. Panic hook that logs the root panic via `tracing`
The default panic hook writes only to stderr, so a panic **not** inside a `catch_game_panic` body —
most importantly one in the detached background sampler (`observe`/`persist`) or the prebuild
pipeline task — never reaches the structured `tracing` stream: it prints a raw stderr line (captured
in pod logs but easy to miss), the task dies silently, and any downstream `PoisonError` cascade
(pre-#11) points at the symptom, not the cause.
- **Fix:** install a custom hook (`std::panic::set_hook`) in `init_tracing` that emits
  `tracing::error!` with the panic payload + location, then delegates to the previous hook — so
  every panic (caught or not, any task) lands in the same stream as the rest of the daemon's logs.
- **Relates to:** #11 (which stops the poison cascade but not the root panic's visibility), #23
  (game-task panics already log via `catch_game_panic`; this covers the tasks it does not wrap).
- **Status:** enhancement (observability); not yet planned. Contained to `bin` / `init_tracing`.

---

## Not gaps (by design / verified benign)

- **Cache key** includes `chain_id` (`cache.rs:46`), `start`, `end`, `da_type` — as
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
