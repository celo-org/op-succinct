use std::{
    collections::VecDeque,
    time::{Duration, Instant, SystemTime},
};

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
pub fn resume_index(
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

/// Loads progress JSON, tolerating a missing/corrupt file.
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
        assert_eq!(resume_index(Some(5), Some(&p), 100), 5);
        assert_eq!(resume_index(None, Some(&p), 100), 42);
        assert_eq!(resume_index(None, None, 100), 99);
        assert_eq!(resume_index(None, None, 0), 0);
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
