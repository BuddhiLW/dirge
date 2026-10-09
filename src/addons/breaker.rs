//! A circuit breaker in front of the addon hooks.
//!
//! [`blocking_within`] stops waiting on a hook that outlasts its budget, but
//! it cannot stop the hook: the work keeps its thread, and every addon hook
//! runs on the one isolate thread. Once one hook hangs, every later hook
//! queues behind it and waits out its own full budget too (10s per turn
//! hook, 60s per fold, 30s per ACP `_meta`). So after a timeout the breaker
//! opens. Hooks are skipped until the abandoned work has returned (the
//! isolate drained) or [`COOLDOWN`] has passed since the trip, whichever
//! comes first. After the cooldown the next hook is a probe: if it also
//! times out, the breaker trips again.
//!
//! [`Breaker`] is the pure state, driven with explicit instants.
//! [`within`] is the boundary every addon hook calls in place of
//! [`blocking_within`].

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::{NoAnswer, blocking_within};

/// How long hooks stay skipped after a timeout when the abandoned work has
/// not returned.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// Whether addon hooks run, given the work abandoned at a timeout that is
/// still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breaker {
    /// Abandoned hook calls that have not returned yet.
    stuck: usize,
    /// When the breaker last tripped.
    tripped_at: Option<Instant>,
    cooldown: Duration,
}

impl Breaker {
    pub const fn new(cooldown: Duration) -> Self {
        Self {
            stuck: 0,
            tripped_at: None,
            cooldown,
        }
    }

    /// Whether a hook may run at `now`: nothing is stuck, or the cooldown
    /// since the last trip has passed.
    pub fn admits(&self, now: Instant) -> bool {
        match (self.stuck, self.tripped_at) {
            (0, _) | (_, None) => true,
            (_, Some(at)) => now.saturating_duration_since(at) >= self.cooldown,
        }
    }

    /// A hook ran past its budget at `now`, and its work is still running.
    /// Answers whether this trip opened a breaker that was closed, which is
    /// when the notice is due.
    pub fn trip(&mut self, now: Instant) -> bool {
        let was_open = self.stuck > 0;
        self.stuck += 1;
        self.tripped_at = Some(now);
        !was_open
    }

    /// Abandoned work returned. Answers whether the isolate has now drained,
    /// which closes the breaker.
    pub fn drained_one(&mut self) -> bool {
        self.stuck = self.stuck.saturating_sub(1);
        if self.stuck == 0 {
            self.tripped_at = None;
            true
        } else {
            false
        }
    }
}

static GLOBAL: Mutex<Breaker> = Mutex::new(Breaker::new(COOLDOWN));

/// [`blocking_within`] for an addon hook, behind the process-wide breaker.
/// While the breaker is open the hook is not run and the answer is
/// [`NoAnswer::Skipped`].
pub(crate) async fn within<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, NoAnswer> {
    guarded_within(&GLOBAL, budget, work).await
}

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const ABANDONED: u8 = 2;

/// [`within`] against `breaker`, so tests can bring their own.
pub(crate) async fn guarded_within<T: Send + 'static>(
    breaker: &'static Mutex<Breaker>,
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, NoAnswer> {
    if !lock(breaker).admits(Instant::now()) {
        return Err(NoAnswer::Skipped);
    }
    // One state per call, so the work and the waiter agree on which of them
    // saw the end: work finishing after the waiter gave up drains the
    // breaker, work finishing in time leaves it alone.
    let state = Arc::new(AtomicU8::new(RUNNING));
    let seen = state.clone();
    let answer = blocking_within(budget, move || {
        let out = work();
        if seen
            .compare_exchange(RUNNING, DONE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
            && lock(breaker).drained_one()
        {
            tracing::info!(target: "dirge::addon", "addon hooks resumed: the stuck hook returned");
        }
        out
    })
    .await;
    if let Err(NoAnswer::TimedOut(_)) = &answer {
        // Abandon and trip under the breaker lock: work that returns right
        // after the abandon sees ABANDONED, then waits on this lock, so its
        // `drained_one` always follows the `trip` it undoes.
        let mut open = lock(breaker);
        if state
            .compare_exchange(RUNNING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && open.trip(Instant::now())
        {
            drop(open);
            tracing::warn!(
                target: "dirge::addon",
                cooldown = ?COOLDOWN,
                "an addon hook timed out; addon hooks are skipped until it returns or the cooldown passes"
            );
        }
    }
    answer
}

fn lock(breaker: &Mutex<Breaker>) -> std::sync::MutexGuard<'_, Breaker> {
    breaker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn a_closed_breaker_admits() {
        assert!(Breaker::new(SECOND).admits(Instant::now()));
    }

    #[test]
    fn a_trip_opens_until_the_cooldown_passes() {
        let t0 = Instant::now();
        let mut b = Breaker::new(SECOND);
        assert!(b.trip(t0), "the first trip is the one that notifies");
        assert!(!b.admits(t0));
        assert!(!b.admits(t0 + SECOND / 2));
        assert!(b.admits(t0 + SECOND), "after the cooldown a probe runs");
    }

    #[test]
    fn a_second_trip_restarts_the_cooldown_without_a_new_notice() {
        let t0 = Instant::now();
        let mut b = Breaker::new(SECOND);
        b.trip(t0);
        assert!(!b.trip(t0 + SECOND), "already open: no second notice");
        assert!(!b.admits(t0 + SECOND + SECOND / 2));
    }

    #[test]
    fn the_breaker_closes_once_every_stuck_call_returned() {
        let t0 = Instant::now();
        let mut b = Breaker::new(SECOND);
        b.trip(t0);
        b.trip(t0);
        assert!(!b.drained_one());
        assert!(!b.admits(t0));
        assert!(b.drained_one());
        assert!(b.admits(t0));
        assert_eq!(b, Breaker::new(SECOND));
    }

    static TIMEOUT_BREAKER: Mutex<Breaker> = Mutex::new(Breaker::new(Duration::from_secs(60)));

    #[tokio::test]
    async fn a_timeout_skips_later_hooks_until_the_stuck_one_returns() {
        let (release, parked) = std::sync::mpsc::channel::<()>();
        let budget = Duration::from_millis(50);
        let first = guarded_within(&TIMEOUT_BREAKER, budget, move || {
            let _ = parked.recv();
        })
        .await;
        assert_eq!(first, Err(NoAnswer::TimedOut(budget)));

        let skipped = guarded_within(&TIMEOUT_BREAKER, budget, || 7).await;
        assert_eq!(skipped, Err(NoAnswer::Skipped));

        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock(&TIMEOUT_BREAKER).admits(Instant::now()) {
            assert!(Instant::now() < deadline, "the breaker never closed");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(guarded_within(&TIMEOUT_BREAKER, budget, || 7).await, Ok(7));
    }

    static IN_TIME_BREAKER: Mutex<Breaker> = Mutex::new(Breaker::new(Duration::from_secs(60)));

    #[tokio::test]
    async fn hooks_that_answer_in_time_never_trip() {
        for n in 0..5 {
            let out = guarded_within(&IN_TIME_BREAKER, Duration::from_secs(5), move || n).await;
            assert_eq!(out, Ok(n));
        }
        assert_eq!(
            *lock(&IN_TIME_BREAKER),
            Breaker::new(Duration::from_secs(60))
        );
    }
}
