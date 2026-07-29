//! Cache-fronted scheduling (outstanding-work #26).
//!
//! Two worker pools — witness **build** and proof **execute** — each drain two feeders in
//! strict order: first a FIFO **priority** queue of on-demand work (ranges a discovered game
//! needs), then a LIFO **speculative** queue fed by chain progression (newest range first).
//! Because the priority queue is FIFO, an in-flight game's demands are served in the order
//! games issued them, so newer games or speculative work can never starve a game already
//! executing; speculative pre-compute only ever consumes spare capacity.
//!
//! Games assemble their results from two on-disk caches: the stdin (witness) cache and the
//! proof cache (`ExecutionStats` keyed by range). A game whose ranges are all cached
//! completes without executing anything. An execute demand for an un-built range does not
//! build inline — it **promotes a witness demand** onto the build priority queue and parks
//! until the witness lands (`exec_waiting`).
//!
//! The speculative execute feeder is bounded by a **lead cap**: at most
//! `--max-speculative-lead-windows` predicted windows past the newest discovered game's end
//! block. Window prediction is operator config, not protocol law (a proposal-interval change
//! or a re-anchored game shifts every boundary), and a wasted speculative execute is the
//! dominant cost (~10 min, ~12 GiB, uncancellable) — the cap bounds that waste. Speculative
//! *builds* stay uncapped (cheap; finalization-bounded), as item #7 decided.
//!
//! Workers still pass every unit through the shared memory-admission gate; the pools decide
//! only *ordering*. Failures of demanded units are recorded per range and consumed by the
//! demanding game (`take_error`), feeding the normal game-level retry policy; speculative
//! failures are dropped (the range is rebuilt/re-executed on demand if a game needs it).

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
    /// Needed by a currently-executing game; served before all speculative work.
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
    build_priority: VecDeque<RangeKey>, // FIFO: push_back / pop_front
    build_spec: VecDeque<RangeKey>,     // LIFO: push_front / pop_front
    exec_priority: VecDeque<RangeKey>,  // FIFO
    exec_spec: VecDeque<RangeKey>,      // LIFO
    /// Execute demands parked until their witness (stdin) is built.
    exec_waiting: HashSet<RangeKey>,
    /// Every build queued or in flight (dedup).
    build_tracked: HashSet<RangeKey>,
    /// Every execute queued, parked, or in flight (dedup).
    exec_tracked: HashSet<RangeKey>,
    /// Failures of demanded units, keyed by range, awaiting `take_error`.
    errors: HashMap<RangeKey, DemandError>,
}

pub struct Scheduler {
    q: Mutex<Queues>,
    build_wake: Notify,
    exec_wake: Notify,
    /// Notified when a unit finishes (success or recorded failure); game tasks register on
    /// this (`completed_notified`) and re-check the proof cache / error map.
    completed: Notify,
    /// End block of the newest discovered game — the speculative execute horizon anchor.
    latest_game_end: AtomicU64,
    /// Speculative executes may run at most this many blocks past `latest_game_end`
    /// (`--max-speculative-lead-windows * --proposal-interval`; 0 disables them).
    lead_blocks: u64,
}

impl Scheduler {
    /// `latest_game_end_seed` anchors the speculative-execute horizon until discovery
    /// observes a game (the pipeline seed — the latest on-chain game's end block — or 0,
    /// which keeps speculation off until the first game raises it).
    pub fn new(latest_game_end_seed: u64, lead_blocks: u64) -> Self {
        Self {
            q: Mutex::new(Queues::default()),
            build_wake: Notify::new(),
            exec_wake: Notify::new(),
            completed: Notify::new(),
            latest_game_end: AtomicU64::new(latest_game_end_seed),
            lead_blocks,
        }
    }

    /// Recover the queues even if a worker panicked while holding the lock.
    fn queues(&self) -> std::sync::MutexGuard<'_, Queues> {
        self.q.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Raise the speculative-execute horizon to the newest discovered game's end block.
    pub fn note_game_end(&self, end_block: u64) {
        self.latest_game_end.fetch_max(end_block, Ordering::Relaxed);
        // Previously over-horizon speculative executes may have become eligible.
        self.exec_wake.notify_waiters();
    }

    /// A game demands a range's execution. Dedups against queued/parked/in-flight work and
    /// promotes an already-queued speculative execute to the priority queue.
    pub fn demand_execute(&self, key: RangeKey) {
        let mut q = self.queues();
        if q.exec_tracked.contains(&key) {
            if let Some(pos) = q.exec_spec.iter().position(|k| *k == key) {
                q.exec_spec.remove(pos);
                q.exec_priority.push_back(key);
                drop(q);
                self.exec_wake.notify_waiters();
            }
            return; // already queued (priority/parked) or in flight
        }
        q.exec_tracked.insert(key);
        q.errors.remove(&key); // stale error from an older attempt
        q.exec_priority.push_back(key);
        drop(q);
        self.exec_wake.notify_waiters();
    }

    /// The predictive feeder offers a range for speculative witness build.
    pub fn push_speculative_build(&self, key: RangeKey) {
        let mut q = self.queues();
        if q.build_tracked.contains(&key) {
            return;
        }
        q.build_tracked.insert(key);
        q.build_spec.push_front(key); // LIFO: newest range first
        drop(q);
        self.build_wake.notify_waiters();
    }

    /// Next build job: priority FIFO first, else speculative LIFO. The key stays tracked
    /// while in flight (dedup); `build_done`/`build_failed` releases it.
    pub fn next_build(&self) -> Option<(RangeKey, Class)> {
        let mut q = self.queues();
        if let Some(k) = q.build_priority.pop_front() {
            return Some((k, Class::Demand));
        }
        q.build_spec.pop_front().map(|k| (k, Class::Speculative))
    }

    /// Next execute job: priority FIFO first, else the newest **lead-eligible** speculative
    /// range (entries past the horizon stay queued until `note_game_end` raises it).
    pub fn next_execute(&self) -> Option<(RangeKey, Class)> {
        let mut q = self.queues();
        if let Some(k) = q.exec_priority.pop_front() {
            return Some((k, Class::Demand));
        }
        let horizon = self.latest_game_end.load(Ordering::Relaxed).saturating_add(self.lead_blocks);
        let pos = q.exec_spec.iter().position(|k| k.1 <= horizon)?;
        Some((q.exec_spec.remove(pos).expect("position exists"), Class::Speculative))
    }

    /// An execute demand found no witness: promote a witness demand onto the build priority
    /// queue (or promote a queued speculative build) and park the execute until it lands.
    pub fn park_for_witness(&self, key: RangeKey) {
        let mut q = self.queues();
        q.exec_waiting.insert(key); // stays exec_tracked
        if let Some(pos) = q.build_spec.iter().position(|k| *k == key) {
            q.build_spec.remove(pos);
            q.build_priority.push_back(key);
        } else if !q.build_tracked.contains(&key) {
            q.build_tracked.insert(key);
            q.build_priority.push_back(key);
        }
        drop(q);
        self.build_wake.notify_waiters();
    }

    /// A build finished: release any parked execute demand for the range; a speculative
    /// build additionally feeds the speculative execute queue.
    pub fn build_done(&self, key: RangeKey, class: Class) {
        let mut q = self.queues();
        q.build_tracked.remove(&key);
        let wake = if q.exec_waiting.remove(&key) {
            q.exec_priority.push_back(key); // still exec_tracked
            true
        } else if class == Class::Speculative && !q.exec_tracked.contains(&key) {
            q.exec_tracked.insert(key);
            q.exec_spec.push_front(key);
            true
        } else {
            false
        };
        drop(q);
        if wake {
            self.exec_wake.notify_waiters();
        }
    }

    /// A build failed. A failure that blocks a parked execute demand is recorded for the
    /// demanding game; a purely speculative failure is dropped (rebuilt on demand later).
    pub fn build_failed(&self, key: RangeKey, transient: bool, message: String) {
        let mut q = self.queues();
        q.build_tracked.remove(&key);
        let had_waiter = q.exec_waiting.remove(&key);
        if had_waiter {
            q.exec_tracked.remove(&key);
            q.errors.insert(key, DemandError { transient, message });
        }
        drop(q);
        if had_waiter {
            self.completed.notify_waiters();
        }
    }

    /// An execute finished; its stats are in the proof cache. Wakes waiting games.
    pub fn execute_done(&self, key: RangeKey) {
        self.queues().exec_tracked.remove(&key);
        self.completed.notify_waiters();
    }

    /// An execute failed. Demanded failures are recorded for the demanding game;
    /// speculative failures are dropped.
    pub fn execute_failed(&self, key: RangeKey, class: Class, transient: bool, message: String) {
        let mut q = self.queues();
        q.exec_tracked.remove(&key);
        if class == Class::Demand {
            q.errors.insert(key, DemandError { transient, message });
        }
        drop(q);
        self.completed.notify_waiters();
    }

    /// A speculative execute was dropped without running (e.g. its stdin was evicted
    /// between build and pop); untrack so a later demand re-queues it.
    pub fn execute_dropped(&self, key: RangeKey) {
        self.queues().exec_tracked.remove(&key);
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
            queued_witness: q.build_priority.len() + q.build_spec.len(),
            queued_prove: q.exec_priority.len() + q.exec_spec.len(),
            waiting_witness: q.exec_waiting.len(),
        }
    }
}

type WitnessOf<H> = <<H as OPSuccinctHost>::WitnessGenerator as WitnessGenerator>::WitnessData;

/// Fallback wake period for idle workers/waiters — belt-and-braces against a missed
/// notification wedging a queue (cheap: one queue re-check per period).
pub(crate) const IDLE_RECHECK: Duration = Duration::from_secs(60);

/// Spawn the build and execute worker pools. Workers run for the process lifetime; every
/// unit passes through the shared admission gate inside `pipeline_step`/`execute_step`.
pub fn spawn_workers<H: OPSuccinctHost + 'static>(
    scheduler: Arc<Scheduler>,
    estimator: Arc<Estimator<H>>,
    fetcher: Arc<OPSuccinctDataFetcher>,
    admission: Arc<Admission>,
    build_workers: usize,
    execute_workers: usize,
) where
    // Mirror Estimator<H>'s impl rkyv bounds so the workers can call build/execute; `Send`
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
    for _ in 0..build_workers.max(1) {
        let (s, e, f, a) =
            (scheduler.clone(), estimator.clone(), fetcher.clone(), admission.clone());
        tokio::spawn(async move { build_worker(s, e, f, a).await });
    }
    for _ in 0..execute_workers.max(1) {
        let (s, e, f, a) =
            (scheduler.clone(), estimator.clone(), fetcher.clone(), admission.clone());
        tokio::spawn(async move { execute_worker(s, e, f, a).await });
    }
}

async fn build_worker<H: OPSuccinctHost>(
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
        let wake = scheduler.build_wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let Some((key, class)) = scheduler.next_build() else {
            let _ = tokio::time::timeout(IDLE_RECHECK, wake).await;
            continue;
        };
        let range = SpanBatchRange { start: key.0, end: key.1 };
        match pipeline::pipeline_step(&estimator, &fetcher, &admission, &range).await {
            Ok(()) => scheduler.build_done(key, class),
            Err(e) => {
                tracing::warn!(
                    start = key.0,
                    end = key.1,
                    ?class,
                    error = %e,
                    "witness build failed"
                );
                scheduler.build_failed(key, e.is_transient(), format!("{e}"));
            }
        }
    }
}

async fn execute_worker<H: OPSuccinctHost>(
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
        let wake = scheduler.exec_wake.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let Some((key, class)) = scheduler.next_execute() else {
            let _ = tokio::time::timeout(IDLE_RECHECK, wake).await;
            continue;
        };
        // Promoted-demand decision point: an un-built demand parks behind a witness demand
        // instead of building inline; a speculative range whose stdin vanished (evicted) is
        // dropped — a game that needs it will re-demand.
        if !estimator.cache.has_stdin(key.0, key.1) &&
            !estimator.cache.has_stats(key.0, key.1)
        {
            match class {
                Class::Demand => scheduler.park_for_witness(key),
                Class::Speculative => scheduler.execute_dropped(key),
            }
            continue;
        }
        let range = SpanBatchRange { start: key.0, end: key.1 };
        match executor::execute_step(&estimator, &fetcher, &admission, &range).await {
            Ok(_) => scheduler.execute_done(key),
            Err(e) => {
                tracing::warn!(
                    start = key.0,
                    end = key.1,
                    ?class,
                    error = %e,
                    "range execute failed"
                );
                scheduler.execute_failed(key, class, e.is_transient(), format!("{e}"));
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
        s.push_speculative_build((10, 20));
        s.push_speculative_build((20, 30)); // newest — LIFO front
        s.demand_execute((0, 10));
        s.park_for_witness((0, 10)); // execute demand promotes a build demand
        assert_eq!(s.next_build(), Some(((0, 10), Class::Demand)));
        assert_eq!(s.next_build(), Some(((20, 30), Class::Speculative)));
        assert_eq!(s.next_build(), Some(((10, 20), Class::Speculative)));
        assert_eq!(s.next_build(), None);
    }

    #[test]
    fn park_promotes_a_queued_speculative_build_to_priority() {
        let s = Scheduler::new(0, 0);
        s.push_speculative_build((10, 20));
        s.push_speculative_build((20, 30));
        s.demand_execute((10, 20));
        s.park_for_witness((10, 20));
        // (10,20) jumps the speculative queue; no duplicate entry remains.
        assert_eq!(s.next_build(), Some(((10, 20), Class::Demand)));
        assert_eq!(s.next_build(), Some(((20, 30), Class::Speculative)));
        assert_eq!(s.next_build(), None);
    }

    #[test]
    fn build_done_releases_parked_execute_demand() {
        let s = Scheduler::new(0, 0);
        s.demand_execute((10, 20));
        assert_eq!(s.next_execute(), Some(((10, 20), Class::Demand)));
        s.park_for_witness((10, 20));
        assert_eq!(s.next_execute(), None); // parked, not queued
        assert_eq!(s.next_build(), Some(((10, 20), Class::Demand)));
        s.build_done((10, 20), Class::Demand);
        assert_eq!(s.next_execute(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn speculative_build_feeds_execute_within_lead_cap_only() {
        // Horizon: latest game end 1000 + lead 100 => 1100.
        let s = Scheduler::new(1000, 100);
        s.push_speculative_build((1000, 1100));
        s.push_speculative_build((1100, 1200));
        let (k1, _) = s.next_build().unwrap(); // (1100,1200) — LIFO newest first
        let (k2, _) = s.next_build().unwrap();
        s.build_done(k1, Class::Speculative);
        s.build_done(k2, Class::Speculative);
        // Only (1000,1100) is lead-eligible; (1100,1200) waits for the horizon.
        assert_eq!(s.next_execute(), Some(((1000, 1100), Class::Speculative)));
        assert_eq!(s.next_execute(), None);
        s.note_game_end(1100); // horizon now 1200
        assert_eq!(s.next_execute(), Some(((1100, 1200), Class::Speculative)));
    }

    #[test]
    fn zero_lead_disables_speculative_execute() {
        let s = Scheduler::new(1000, 0);
        s.push_speculative_build((1000, 1100));
        let (k, _) = s.next_build().unwrap();
        s.build_done(k, Class::Speculative);
        assert_eq!(s.next_execute(), None); // 1100 > horizon 1000, forever
    }

    #[test]
    fn demand_promotes_queued_speculative_execute_past_the_horizon() {
        let s = Scheduler::new(1000, 0); // speculation disabled
        s.push_speculative_build((1100, 1200));
        let (k, _) = s.next_build().unwrap();
        s.build_done(k, Class::Speculative);
        assert_eq!(s.next_execute(), None);
        s.demand_execute((1100, 1200)); // a real game needs it: horizon no longer applies
        assert_eq!(s.next_execute(), Some(((1100, 1200), Class::Demand)));
    }

    #[test]
    fn demanded_failures_are_recorded_and_consumed_once() {
        let s = Scheduler::new(0, 0);
        s.demand_execute((10, 20));
        let (k, class) = s.next_execute().unwrap();
        s.execute_failed(k, class, true, "rpc blip".into());
        let err = s.take_error(&[(10, 20)]).expect("error recorded");
        assert!(err.transient);
        assert!(err.message.contains("rpc blip"));
        assert!(s.take_error(&[(10, 20)]).is_none()); // consumed
        s.demand_execute((10, 20)); // re-demand works after failure
        assert_eq!(s.next_execute(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn build_failure_reaches_the_parked_demand() {
        let s = Scheduler::new(0, 0);
        s.demand_execute((10, 20));
        s.next_execute();
        s.park_for_witness((10, 20));
        s.next_build();
        s.build_failed((10, 20), false, "unbuildable".into());
        let err = s.take_error(&[(10, 20)]).expect("build failure recorded");
        assert!(!err.transient);
        // The parked demand was cleared; the range can be demanded again.
        s.demand_execute((10, 20));
        assert_eq!(s.next_execute(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn speculative_failures_are_dropped_silently() {
        let s = Scheduler::new(1000, 1000);
        s.push_speculative_build((1000, 1100));
        let (k, _) = s.next_build().unwrap();
        s.build_failed(k, true, "blip".into());
        assert!(s.take_error(&[(1000, 1100)]).is_none());
        s.push_speculative_build((1000, 1100)); // re-offer works (untracked again)
        assert_eq!(s.next_build(), Some(((1000, 1100), Class::Speculative)));
    }

    #[test]
    fn demand_dedups_against_queued_parked_and_inflight() {
        let s = Scheduler::new(0, 0);
        s.demand_execute((10, 20));
        s.demand_execute((10, 20)); // queued dup
        assert_eq!(s.next_execute(), Some(((10, 20), Class::Demand)));
        s.demand_execute((10, 20)); // in-flight dup
        assert_eq!(s.next_execute(), None);
        s.execute_done((10, 20));
        s.demand_execute((10, 20)); // fresh demand after completion
        assert_eq!(s.next_execute(), Some(((10, 20), Class::Demand)));
    }

    #[test]
    fn depths_reflect_queues() {
        let s = Scheduler::new(0, 0);
        s.push_speculative_build((10, 20));
        s.demand_execute((0, 10));
        assert_eq!(s.next_execute(), Some(((0, 10), Class::Demand))); // worker pops...
        s.park_for_witness((0, 10)); // ...finds no stdin, parks behind a build demand
        let d = s.depths();
        assert_eq!(d.queued_witness, 2); // spec + promoted demand
        assert_eq!(d.queued_prove, 0);
        assert_eq!(d.waiting_witness, 1);
    }
}
