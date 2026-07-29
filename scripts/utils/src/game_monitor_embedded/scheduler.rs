//! Cache-fronted scheduling.
//!
//! Two worker pools — **witness** and **prove** — each drain two queues in
//! strict order: first a FIFO **priority** queue of on-demand work (ranges a discovered game
//! needs), then a LIFO **speculative** queue filled by chain progression (newest range
//! first). Because the priority queue is FIFO, an in-flight game's demands are served in the
//! order games issued them, so newer games or speculative work can never starve a game
//! already proving; speculative pre-compute only ever uses spare capacity.
//!
//! Games assemble their results from two on-disk caches: the stdin (witness) cache and the
//! proof cache (`ExecutionStats` keyed by range). A game whose ranges are all cached
//! completes without proving anything. A prove demand for a range with no witness does not
//! generate it inline — it **promotes a witness demand** onto the witness priority queue and
//! waits until the witness is generated (`prove_waiting`).
//!
//! Speculative proving is bounded by a **lead cap**: at most
//! `--max-speculative-lead-windows` predicted windows past the newest discovered game's end
//! block. Window prediction is operator config, not protocol law (a proposal-interval change
//! or a re-anchored game shifts every boundary), and a wasted speculative prove is the
//! dominant cost (~10 min, ~12 GiB, uncancellable) — the cap bounds that waste. Speculative
//! *witness tasks* have no lead cap (they are cheap and bounded by finalization anyway).
//!
//! Workers still pass every unit through the shared memory-admission gate; the pools decide
//! only *ordering*. Failures of demanded units are recorded per range and consumed by the
//! demanding game (`take_error`), feeding the normal game-level retry policy; speculative
//! failures are dropped (the range's witness/proof is regenerated on demand if a game needs it).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use op_succinct_estimator::Estimator;
use op_succinct_host_utils::{
    block_range::SpanBatchRange, fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost,
    witness_generation::WitnessGenerator,
};
use rkyv::rancor::Error as RkyvError;
use tokio::sync::Notify;

use crate::game_monitor_embedded::{admission::Admission, executor, pipeline};

/// A sub-range's identity in the queues and caches: `(start_block, end_block)`.
pub type RangeKey = (u64, u64);

/// Why a unit is being run: demanded by a discovered game, or predicted ahead of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Needed by a currently-proving game; served before all speculative work.
    Demand,
    /// Predicted from chain progression; served only when no demand is queued.
    Speculative,
}

/// Terminal failure of a demanded unit, recorded for the demanding game to consume.
#[derive(Debug, Clone)]
pub struct DemandError {
    pub transient: bool,
    pub message: String,
}

/// Queue depths for the `monitor status` line.
#[derive(Debug, Clone, Copy)]
pub struct Depths {
    pub queued_witness: usize,
    pub queued_prove: usize,
    pub waiting_witness: usize,
}

#[derive(Default)]
struct Queues {
    witness_priority: VecDeque<RangeKey>, // FIFO: push_back / pop_front
    witness_spec: VecDeque<RangeKey>,     // LIFO: push_front / pop_front
    prove_priority: VecDeque<RangeKey>,   // FIFO
    prove_spec: VecDeque<RangeKey>,       // LIFO
    /// Prove demands waiting until their witness (stdin) is generated.
    prove_waiting: HashSet<RangeKey>,
    /// Every witness task queued or in flight (dedup).
    witness_tracked: HashSet<RangeKey>,
    /// Every prove queued, waiting, or in flight (dedup).
    prove_tracked: HashSet<RangeKey>,
    /// Failures of demanded units, keyed by range, awaiting `take_error`.
    errors: HashMap<RangeKey, DemandError>,
}

pub struct Scheduler {
    q: Mutex<Queues>,
    witness_wake: Notify,
    prove_wake: Notify,
    /// Notified when a unit finishes (success or recorded failure); game tasks register on
    /// this (`completed_notified`) and re-check the proof cache / error map.
    completed: Notify,
    /// End block of the newest discovered game. Speculative proves may run at most
    /// `lead_blocks` past this.
    latest_game_end: AtomicU64,
    /// Speculative proves may run at most this many blocks past `latest_game_end`
    /// (`--max-speculative-lead-windows * --proposal-interval`; 0 disables them).
    lead_blocks: u64,
}

impl Scheduler {
    /// `latest_game_end_seed` provides the initial `latest_game_end` before discovery has
    /// observed a game (the latest on-chain game's end block, or 0 — which keeps
    /// speculative proving off until the first discovered game raises it).
    pub fn new(latest_game_end_seed: u64, lead_blocks: u64) -> Self {
        Self {
            q: Mutex::new(Queues::default()),
            witness_wake: Notify::new(),
            prove_wake: Notify::new(),
            completed: Notify::new(),
            latest_game_end: AtomicU64::new(latest_game_end_seed),
            lead_blocks,
        }
    }

    /// Recover the queues even if a worker panicked while holding the lock.
    fn queues(&self) -> std::sync::MutexGuard<'_, Queues> {
        self.q.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record the newest discovered game's end block, raising the speculative prove limit.
    pub fn note_game_end(&self, end_block: u64) {
        self.latest_game_end.fetch_max(end_block, Ordering::Relaxed);
        // Speculative proves previously past the limit may now be eligible.
        self.prove_wake.notify_waiters();
    }

    /// A game demands a range's proof. Dedups against queued/waiting/in-flight work and
    /// promotes an already-queued speculative prove to the priority queue.
    pub fn demand_prove(&self, key: RangeKey) {
        let mut q = self.queues();
        if q.prove_tracked.contains(&key) {
            if let Some(pos) = q.prove_spec.iter().position(|k| *k == key) {
                q.prove_spec.remove(pos);
                q.prove_priority.push_back(key);
                drop(q);
                self.prove_wake.notify_waiters();
            }
            return; // already queued (priority/waiting) or in flight
        }
        q.prove_tracked.insert(key);
        q.errors.remove(&key); // stale error from an older attempt
        q.prove_priority.push_back(key);
        drop(q);
        self.prove_wake.notify_waiters();
    }

    /// The speculative witness task offers a range for speculative witness generation.
    pub fn push_speculative_witness(&self, key: RangeKey) {
        let mut q = self.queues();
        if q.witness_tracked.contains(&key) {
            return;
        }
        q.witness_tracked.insert(key);
        q.witness_spec.push_front(key); // LIFO: newest range first
        drop(q);
        self.witness_wake.notify_waiters();
    }

    /// Next witness job: priority FIFO first, else speculative LIFO. The key stays tracked
    /// while in flight (dedup); `witness_done`/`witness_failed` releases it.
    pub fn next_witness(&self) -> Option<(RangeKey, Class)> {
        let mut q = self.queues();
        if let Some(k) = q.witness_priority.pop_front() {
            return Some((k, Class::Demand));
        }
        q.witness_spec.pop_front().map(|k| (k, Class::Speculative))
    }

    /// Next prove job: priority FIFO first, else the newest speculative range within the
    /// lead cap (entries past the limit stay queued until `note_game_end` raises it).
    pub fn next_prove(&self) -> Option<(RangeKey, Class)> {
        let mut q = self.queues();
        if let Some(k) = q.prove_priority.pop_front() {
            return Some((k, Class::Demand));
        }
        let limit = self.latest_game_end.load(Ordering::Relaxed).saturating_add(self.lead_blocks);
        let pos = q.prove_spec.iter().position(|k| k.1 <= limit)?;
        Some((q.prove_spec.remove(pos).expect("position exists"), Class::Speculative))
    }

    /// A prove demand found no witness: promote a witness demand onto the witness priority
    /// queue (or promote a queued speculative witness task) and hold the prove demand in
    /// `prove_waiting` until the witness completes.
    pub fn wait_for_witness(&self, key: RangeKey) {
        let mut q = self.queues();
        q.prove_waiting.insert(key); // stays prove_tracked
        if let Some(pos) = q.witness_spec.iter().position(|k| *k == key) {
            q.witness_spec.remove(pos);
            q.witness_priority.push_back(key);
        } else if !q.witness_tracked.contains(&key) {
            q.witness_tracked.insert(key);
            q.witness_priority.push_back(key);
        }
        drop(q);
        self.witness_wake.notify_waiters();
    }

    /// A witness task finished: release any waiting prove demand for the range; a speculative
    /// witness is additionally queued for speculative proving.
    pub fn witness_done(&self, key: RangeKey, class: Class) {
        let mut q = self.queues();
        q.witness_tracked.remove(&key);
        let wake = if q.prove_waiting.remove(&key) {
            q.prove_priority.push_back(key); // still prove_tracked
            true
        } else if class == Class::Speculative && !q.prove_tracked.contains(&key) {
            q.prove_tracked.insert(key);
            q.prove_spec.push_front(key);
            true
        } else {
            false
        };
        drop(q);
        if wake {
            self.prove_wake.notify_waiters();
        }
    }

    /// A witness task failed. A failure that blocks a waiting prove demand is recorded for the
    /// demanding game; a purely speculative failure is dropped (regenerated on demand later).
    pub fn witness_failed(&self, key: RangeKey, transient: bool, message: String) {
        let mut q = self.queues();
        q.witness_tracked.remove(&key);
        let had_waiter = q.prove_waiting.remove(&key);
        if had_waiter {
            q.prove_tracked.remove(&key);
            q.errors.insert(key, DemandError { transient, message });
        }
        drop(q);
        if had_waiter {
            self.completed.notify_waiters();
        }
    }

    /// A prove finished; its stats are in the proof cache. Wakes waiting games.
    pub fn prove_done(&self, key: RangeKey) {
        self.queues().prove_tracked.remove(&key);
        self.completed.notify_waiters();
    }

    /// A prove failed. Demanded failures are recorded for the demanding game;
    /// speculative failures are dropped.
    pub fn prove_failed(&self, key: RangeKey, class: Class, transient: bool, message: String) {
        let mut q = self.queues();
        q.prove_tracked.remove(&key);
        if class == Class::Demand {
            q.errors.insert(key, DemandError { transient, message });
        }
        drop(q);
        self.completed.notify_waiters();
    }

    /// A speculative prove was dropped without running (e.g. its stdin was evicted
    /// between witness generation and pop); untrack so a later demand re-queues it.
    pub fn prove_dropped(&self, key: RangeKey) {
        self.queues().prove_tracked.remove(&key);
    }

    /// Take (consume) the first recorded error among `keys`, if any.
    pub fn take_error(&self, keys: &[RangeKey]) -> Option<DemandError> {
        let mut q = self.queues();
        let k = keys.iter().find(|k| q.errors.contains_key(k)).copied()?;
        q.errors.remove(&k)
    }

    /// Register interest in unit completions. Callers must poll/`enable` the returned
    /// future BEFORE re-checking caches, then await it, to avoid a lost wakeup.
    pub fn completed_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.completed.notified()
    }

    /// Queue depths for the status line.
    pub fn depths(&self) -> Depths {
        let q = self.queues();
        Depths {
            queued_witness: q.witness_priority.len() + q.witness_spec.len(),
            queued_prove: q.prove_priority.len() + q.prove_spec.len(),
            waiting_witness: q.prove_waiting.len(),
        }
    }
}

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Fallback wake period for idle workers/waiters — guards against a missed notification
/// leaving a queue permanently stalled (cheap: one queue re-check per period).
pub(crate) const IDLE_RECHECK: Duration = Duration::from_secs(60);

/// Spawn the witness and prove worker pools. Workers run for the process lifetime; every
/// unit passes through the shared admission gate inside `pipeline_step`/`prove_step`.
pub fn spawn_workers<H: OPSuccinctHost + 'static>(
    scheduler: Arc<Scheduler>,
    estimator: Arc<Estimator<H>>,
    fetcher: Arc<OPSuccinctDataFetcher>,
    admission: Arc<Admission>,
    witness_workers: usize,
    prove_workers: usize,
) where
    // Mirror Estimator<H>'s impl rkyv bounds so the workers can call witness/prove; `Send`
    // because the worker futures (which hold the witness across awaits) are spawned.
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive
        + Send,
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    for _ in 0..witness_workers.max(1) {
        let (s, e, f, a) =
            (scheduler.clone(), estimator.clone(), fetcher.clone(), admission.clone());
        tokio::spawn(async move { witness_worker(s, e, f, a).await });
    }
    for _ in 0..prove_workers.max(1) {
        let (s, e, f, a) =
            (scheduler.clone(), estimator.clone(), fetcher.clone(), admission.clone());
        tokio::spawn(async move { prove_worker(s, e, f, a).await });
    }
}

async fn witness_worker<H: OPSuccinctHost>(
    scheduler: Arc<Scheduler>,
    estimator: Arc<Estimator<H>>,
    fetcher: Arc<OPSuccinctDataFetcher>,
    admission: Arc<Admission>,
) where
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive,
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    loop {
        // Register interest BEFORE the pop so a push between "empty" and "await" still wakes.
        let wake = scheduler.witness_wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let Some((key, class)) = scheduler.next_witness() else {
            let _ = tokio::time::timeout(IDLE_RECHECK, wake).await;
            continue;
        };
        let range = SpanBatchRange { start: key.0, end: key.1 };
        match pipeline::pipeline_step(&estimator, &fetcher, &admission, &range).await {
            Ok(()) => scheduler.witness_done(key, class),
            Err(e) => {
                tracing::warn!(
                    start = key.0,
                    end = key.1,
                    ?class,
                    error = %e,
                    "witness generation failed"
                );
                scheduler.witness_failed(key, e.is_transient(), format!("{e}"));
            }
        }
    }
}

async fn prove_worker<H: OPSuccinctHost>(
    scheduler: Arc<Scheduler>,
    estimator: Arc<Estimator<H>>,
    fetcher: Arc<OPSuccinctDataFetcher>,
    admission: Arc<Admission>,
) where
    WitnessOf<H>: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        > + rkyv::Archive,
    <WitnessOf<H> as rkyv::Archive>::Archived: rkyv::Deserialize<WitnessOf<H>, rkyv::api::high::HighDeserializer<RkyvError>>
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>,
{
    loop {
        let wake = scheduler.prove_wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let Some((key, class)) = scheduler.next_prove() else {
            let _ = tokio::time::timeout(IDLE_RECHECK, wake).await;
            continue;
        };
        // Promoted-demand decision point: a demand with no witness yet is held waiting behind
        // a witness demand instead of generating it inline; a speculative range whose stdin
        // vanished (evicted) is dropped — a game that needs it will re-demand.
        if !estimator.cache.has_stdin(key.0, key.1) &&
            !estimator.cache.has_stats(key.0, key.1)
        {
            match class {
                Class::Demand => scheduler.wait_for_witness(key),
                Class::Speculative => scheduler.prove_dropped(key),
            }
            continue;
        }
        let range = SpanBatchRange { start: key.0, end: key.1 };
        match executor::prove_step(&estimator, &fetcher, &admission, &range).await {
            Ok(_) => scheduler.prove_done(key),
            Err(e) => {
                tracing::warn!(
                    start = key.0,
                    end = key.1,
                    ?class,
                    error = %e,
                    "range prove failed"
                );
                scheduler.prove_failed(key, class, e.is_transient(), format!("{e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demands_are_fifo_and_served_before_speculative_lifo() {
        let s = Scheduler::new(0, 0);
        s.push_speculative_witness((10, 20));
        s.push_speculative_witness((20, 30)); // newest — LIFO front
        s.demand_prove((0, 10));
        s.wait_for_witness((0, 10)); // prove demand promotes a witness demand
        assert_eq!(s.next_witness(), Some(((0, 10), Class::Demand)));
        assert_eq!(s.next_witness(), Some(((20, 30), Class::Speculative)));
        assert_eq!(s.next_witness(), Some(((10, 20), Class::Speculative)));
        assert_eq!(s.next_witness(), None);
    }

    #[test]
    fn wait_for_witness_promotes_a_queued_speculative_witness_to_priority() {
        let s = Scheduler::new(0, 0);
        s.push_speculative_witness((10, 20));
        s.push_speculative_witness((20, 30));
        s.demand_prove((10, 20));
        s.wait_for_witness((10, 20));
        // (10,20) is moved ahead of the speculative queue; no duplicate entry remains.
        assert_eq!(s.next_witness(), Some(((10, 20), Class::Demand)));
        assert_eq!(s.next_witness(), Some(((20, 30), Class::Speculative)));
        assert_eq!(s.next_witness(), None);
    }

    #[test]
    fn witness_done_releases_waiting_prove_demand() {
        let s = Scheduler::new(0, 0);
        s.demand_prove((10, 20));
        assert_eq!(s.next_prove(), Some(((10, 20), Class::Demand)));
        s.wait_for_witness((10, 20));
        assert_eq!(s.next_prove(), None); // waiting, not queued
        assert_eq!(s.next_witness(), Some(((10, 20), Class::Demand)));
        s.witness_done((10, 20), Class::Demand);
        assert_eq!(s.next_prove(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn speculative_witness_feeds_prove_within_lead_cap_only() {
        // Speculative prove limit: latest game end 1000 + lead 100 => 1100.
        let s = Scheduler::new(1000, 100);
        s.push_speculative_witness((1000, 1100));
        s.push_speculative_witness((1100, 1200));
        let (k1, _) = s.next_witness().unwrap(); // (1100,1200) — LIFO newest first
        let (k2, _) = s.next_witness().unwrap();
        s.witness_done(k1, Class::Speculative);
        s.witness_done(k2, Class::Speculative);
        // Only (1000,1100) is within the limit; (1100,1200) waits for it to rise.
        assert_eq!(s.next_prove(), Some(((1000, 1100), Class::Speculative)));
        assert_eq!(s.next_prove(), None);
        s.note_game_end(1100); // limit now 1200
        assert_eq!(s.next_prove(), Some(((1100, 1200), Class::Speculative)));
    }

    #[test]
    fn zero_lead_disables_speculative_prove() {
        let s = Scheduler::new(1000, 0);
        s.push_speculative_witness((1000, 1100));
        let (k, _) = s.next_witness().unwrap();
        s.witness_done(k, Class::Speculative);
        assert_eq!(s.next_prove(), None); // 1100 > limit 1000, forever
    }

    #[test]
    fn demand_promotes_queued_speculative_prove_past_the_limit() {
        let s = Scheduler::new(1000, 0); // speculation disabled
        s.push_speculative_witness((1100, 1200));
        let (k, _) = s.next_witness().unwrap();
        s.witness_done(k, Class::Speculative);
        assert_eq!(s.next_prove(), None);
        s.demand_prove((1100, 1200)); // a real game needs it: the limit no longer applies
        assert_eq!(s.next_prove(), Some(((1100, 1200), Class::Demand)));
    }

    #[test]
    fn demanded_failures_are_recorded_and_consumed_once() {
        let s = Scheduler::new(0, 0);
        s.demand_prove((10, 20));
        let (k, class) = s.next_prove().unwrap();
        s.prove_failed(k, class, true, "rpc blip".into());
        let err = s.take_error(&[(10, 20)]).expect("error recorded");
        assert!(err.transient);
        assert!(err.message.contains("rpc blip"));
        assert!(s.take_error(&[(10, 20)]).is_none()); // consumed
        s.demand_prove((10, 20)); // re-demand works after failure
        assert_eq!(s.next_prove(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn witness_failure_reaches_the_waiting_demand() {
        let s = Scheduler::new(0, 0);
        s.demand_prove((10, 20));
        s.next_prove();
        s.wait_for_witness((10, 20));
        s.next_witness();
        s.witness_failed((10, 20), false, "witness unavailable".into());
        let err = s.take_error(&[(10, 20)]).expect("witness failure recorded");
        assert!(!err.transient);
        // The waiting demand was cleared; the range can be demanded again.
        s.demand_prove((10, 20));
        assert_eq!(s.next_prove(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn speculative_failures_are_dropped_silently() {
        let s = Scheduler::new(1000, 1000);
        s.push_speculative_witness((1000, 1100));
        let (k, _) = s.next_witness().unwrap();
        s.witness_failed(k, true, "blip".into());
        assert!(s.take_error(&[(1000, 1100)]).is_none());
        s.push_speculative_witness((1000, 1100)); // re-offer works (untracked again)
        assert_eq!(s.next_witness(), Some(((1000, 1100), Class::Speculative)));
    }

    #[test]
    fn demand_dedups_against_queued_waiting_and_inflight() {
        let s = Scheduler::new(0, 0);
        s.demand_prove((10, 20));
        s.demand_prove((10, 20)); // queued dup
        assert_eq!(s.next_prove(), Some(((10, 20), Class::Demand)));
        s.demand_prove((10, 20)); // in-flight dup
        assert_eq!(s.next_prove(), None);
        s.prove_done((10, 20));
        s.demand_prove((10, 20)); // fresh demand after completion
        assert_eq!(s.next_prove(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn depths_reflect_queues() {
        let s = Scheduler::new(0, 0);
        s.push_speculative_witness((10, 20));
        s.demand_prove((0, 10));
        assert_eq!(s.next_prove(), Some(((0, 10), Class::Demand))); // worker pops...
        s.wait_for_witness((0, 10)); // ...finds no stdin, waits behind a witness demand
        let d = s.depths();
        assert_eq!(d.queued_witness, 2); // spec + promoted demand
        assert_eq!(d.queued_prove, 0);
        assert_eq!(d.waiting_witness, 1);
    }
}
