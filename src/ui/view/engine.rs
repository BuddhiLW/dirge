//! Adapter: a [`ViewEngine`] that runs any [`Reducer`] on its own
//! thread. The reducer is built on that thread (a cljrs runtime is not
//! `Send`), events arrive over a channel that never blocks the sender,
//! and updates leave on the [`UpdateSink`]. The engine does not know
//! which reducer it runs.
//!
//! The thread supervises its reducer: when a step panics it rebuilds
//! the reducer under a [`RestartPolicy`], folds a fresh
//! [`ViewEvent::Init`] and publishes that model with a notice. The
//! event that panicked is not replayed. Once the policy is spent the
//! thread publishes a model that owns nothing and stops.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use super::domain::{NoticeLevel, ViewEffect, ViewEvent, ViewModel, ViewUpdate};
use super::port::{Reducer, UpdateSink, ViewEngine};
use super::supervise::RestartPolicy;

/// Longest dirge waits at startup for a reducer to boot and answer the
/// initial model.
const BOOT_WAIT: Duration = Duration::from_secs(10);

pub struct ThreadEngine {
    tx: Sender<ViewEvent>,
}

impl ThreadEngine {
    /// [`Self::spawn_with`] under the default [`RestartPolicy`].
    pub fn spawn<R, F>(
        name: &str,
        stack: usize,
        make: F,
        sink: UpdateSink,
    ) -> Result<(Self, ViewModel), String>
    where
        R: Reducer + 'static,
        F: Fn() -> Result<R, String> + Send + 'static,
    {
        Self::spawn_with(name, stack, make, RestartPolicy::default(), sink)
    }

    /// Start thread `name` with `stack` bytes, build the reducer there
    /// with `make`, fold [`ViewEvent::Init`] and answer the initial model.
    /// A reducer that panics is rebuilt with `make` under `policy`.
    pub fn spawn_with<R, F>(
        name: &str,
        stack: usize,
        make: F,
        policy: RestartPolicy,
        sink: UpdateSink,
    ) -> Result<(Self, ViewModel), String>
    where
        R: Reducer + 'static,
        F: Fn() -> Result<R, String> + Send + 'static,
    {
        let (tx, rx) = channel::<ViewEvent>();
        let (ready_tx, ready_rx) = channel::<Result<ViewModel, String>>();
        let thread = name.to_string();
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(stack)
            .spawn(move || {
                let (reducer, last) = match boot(&make) {
                    Ok(booted) => booted,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(last.clone()));
                let supervisor = Supervisor {
                    make,
                    policy,
                    restarts: 0,
                    reducer,
                    last,
                    thread,
                };
                supervisor.run(rx, &sink);
            })
            .map_err(|e| format!("cannot start view thread {name}: {e}"))?;
        let model = ready_rx.recv_timeout(BOOT_WAIT).map_err(|_| {
            format!(
                "view thread {name} did not boot within {}s",
                BOOT_WAIT.as_secs()
            )
        })??;
        Ok((Self { tx }, model))
    }
}

/// Build a reducer with `make` and fold [`ViewEvent::Init`]: the
/// reducer and its initial model.
fn boot<R, F>(make: &F) -> Result<(R, ViewModel), String>
where
    R: Reducer,
    F: Fn() -> Result<R, String>,
{
    make().and_then(|mut reducer| {
        let initial = reducer.step(&ViewEvent::Init)?;
        Ok((reducer, initial.model))
    })
}

/// One event through `reducer`; a failed step keeps the `last` model
/// and reports why.
fn fold<R: Reducer>(reducer: &mut R, event: &ViewEvent, last: &ViewModel) -> ViewUpdate {
    reducer.step(event).unwrap_or_else(|e| ViewUpdate {
        model: last.clone(),
        effects: vec![ViewEffect::notify(NoticeLevel::Error, format!("view: {e}"))],
    })
}

/// `model` with one error notice.
fn with_error(model: ViewModel, text: String) -> ViewUpdate {
    ViewUpdate {
        model,
        effects: vec![ViewEffect::notify(NoticeLevel::Error, text)],
    }
}

/// The engine thread's state: the live reducer, the last model it
/// published, and how to rebuild the reducer.
struct Supervisor<R, F> {
    make: F,
    policy: RestartPolicy,
    restarts: u32,
    reducer: R,
    last: ViewModel,
    thread: String,
}

impl<R, F> Supervisor<R, F>
where
    R: Reducer,
    F: Fn() -> Result<R, String>,
{
    /// Fold every event until the UI hangs up or the policy is spent.
    fn run(mut self, rx: Receiver<ViewEvent>, sink: &UpdateSink) {
        for event in rx {
            let (update, alive) = match self.guarded(&event) {
                Ok(update) => (update, true),
                Err(why) => self.restart(&why),
            };
            self.last = update.model.clone();
            if sink.send(update).is_err() || !alive {
                return;
            }
        }
    }

    /// [`fold`], with a panic caught and described.
    fn guarded(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
        let (reducer, last) = (&mut self.reducer, &self.last);
        catch_unwind(AssertUnwindSafe(|| fold(reducer, event, last)))
            .map_err(|payload| crate::panic_report::payload_text(payload.as_ref()))
    }

    /// Rebuild the reducer after it panicked with `why`, waiting out
    /// the policy's backoff before each try. The update to publish, and
    /// whether the engine is still alive.
    fn restart(&mut self, why: &str) -> (ViewUpdate, bool) {
        let thread = self.thread.clone();
        tracing::error!(target: "dirge::view", %thread, %why, "view reducer panicked");
        while let Some(delay) = self.policy.delay(self.restarts) {
            self.restarts += 1;
            std::thread::sleep(delay);
            let make = &self.make;
            match catch_unwind(AssertUnwindSafe(|| boot(make))) {
                Ok(Ok((reducer, model))) => {
                    self.reducer = reducer;
                    let restarts = self.restarts;
                    tracing::warn!(target: "dirge::view", %thread, restarts, "view engine restarted");
                    let text = format!("view: engine restarted after a panic ({why})");
                    return (with_error(model, text), true);
                }
                Ok(Err(error)) => {
                    tracing::warn!(target: "dirge::view", %thread, %error, "view engine rebuild failed")
                }
                Err(payload) => {
                    let error = crate::panic_report::payload_text(payload.as_ref());
                    tracing::warn!(target: "dirge::view", %thread, %error, "view engine rebuild panicked")
                }
            }
        }
        let restarts = self.restarts;
        tracing::error!(target: "dirge::view", %thread, restarts, "view engine stopped");
        let text = format!(
            "view: engine stopped after {restarts} restart(s) ({why}); view commands are unavailable"
        );
        (with_error(ViewModel::default(), text), false)
    }
}

impl ViewEngine for ThreadEngine {
    fn submit(&self, event: ViewEvent) {
        if self.tx.send(event).is_err() {
            tracing::warn!(target: "dirge::view", "view engine stopped; event dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::native::NativeReducer;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use tokio::sync::mpsc::unbounded_channel;

    struct Failing;

    impl Reducer for Failing {
        fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
            match event {
                ViewEvent::Init => Ok(ViewUpdate::default()),
                _ => Err("boom".into()),
            }
        }
    }

    #[test]
    fn submit_returns_at_once_and_the_update_arrives_on_the_sink() {
        let (sink, mut rx) = unbounded_channel();
        let (engine, model) =
            ThreadEngine::spawn("t-view", 1 << 20, || Ok(NativeReducer::default()), sink).unwrap();
        assert!(!model.swarm_open());
        assert!(model.owns_command("swarm"));
        engine.submit(ViewEvent::command("swarm", &["on"]));
        let update = rx.blocking_recv().unwrap();
        assert!(update.model.swarm_open());
    }

    #[test]
    fn a_failed_step_keeps_the_last_model_and_says_why() {
        let (sink, mut rx) = unbounded_channel();
        let (engine, _) = ThreadEngine::spawn("t-fail", 1 << 20, || Ok(Failing), sink).unwrap();
        engine.submit(ViewEvent::command("swarm", &[]));
        let update = rx.blocking_recv().unwrap();
        assert_eq!(update.model, ViewModel::default());
        assert!(
            matches!(&update.effects[0], ViewEffect::Notify { text, .. } if text.contains("boom"))
        );
    }

    #[test]
    fn a_reducer_that_cannot_boot_fails_the_spawn() {
        let (sink, _rx) = unbounded_channel();
        let err = ThreadEngine::spawn::<NativeReducer, _>(
            "t-dead",
            1 << 20,
            || Err("no runtime".into()),
            sink,
        );
        assert_eq!(err.err().as_deref(), Some("no runtime"));
    }

    /// The native reducer, except that `/boom` panics.
    #[derive(Default)]
    struct Panicky(NativeReducer);

    impl Reducer for Panicky {
        fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
            if matches!(event, ViewEvent::Command { name, .. } if name == "boom") {
                panic!("stub reducer panicked");
            }
            self.0.step(event)
        }
    }

    fn quick(max_restarts: u32) -> RestartPolicy {
        RestartPolicy {
            max_restarts,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        }
    }

    fn error_text(update: &ViewUpdate) -> &str {
        match &update.effects[..] {
            [
                ViewEffect::Notify {
                    level: NoticeLevel::Error,
                    text,
                },
            ] => text,
            other => panic!("expected one error notice, got {other:?}"),
        }
    }

    #[test]
    fn a_panicking_reducer_is_restarted_and_a_later_swarm_still_works() {
        let builds = Arc::new(AtomicUsize::new(0));
        let counted = builds.clone();
        let make = move || {
            counted.fetch_add(1, SeqCst);
            Ok(Panicky::default())
        };
        let (sink, mut rx) = unbounded_channel();
        let (engine, _) =
            ThreadEngine::spawn_with("t-panic", 1 << 20, make, quick(3), sink).unwrap();
        engine.submit(ViewEvent::command("swarm", &["on"]));
        assert!(rx.blocking_recv().unwrap().model.swarm_open());

        engine.submit(ViewEvent::command("boom", &[]));
        let restarted = rx.blocking_recv().unwrap();
        assert!(error_text(&restarted).contains("restarted"));
        assert!(error_text(&restarted).contains("stub reducer panicked"));
        assert!(
            !restarted.model.swarm_open(),
            "a restart folds a fresh Init"
        );

        engine.submit(ViewEvent::command("swarm", &["on"]));
        assert!(rx.blocking_recv().unwrap().model.swarm_open());
        assert_eq!(builds.load(SeqCst), 2);
    }

    #[test]
    fn restarts_are_bounded_and_a_spent_engine_owns_nothing() {
        let (sink, mut rx) = unbounded_channel();
        let make = || Ok(Panicky::default());
        let (engine, _) =
            ThreadEngine::spawn_with("t-spent", 1 << 20, make, quick(1), sink).unwrap();
        engine.submit(ViewEvent::command("boom", &[]));
        assert!(error_text(&rx.blocking_recv().unwrap()).contains("restarted"));

        engine.submit(ViewEvent::command("boom", &[]));
        let farewell = rx.blocking_recv().unwrap();
        assert!(error_text(&farewell).contains("stopped"));
        assert_eq!(farewell.model, ViewModel::default());
        assert!(rx.blocking_recv().is_none(), "the view thread is gone");
        engine.submit(ViewEvent::command("swarm", &[]));
    }

    #[test]
    fn a_failed_rebuild_spends_a_restart() {
        let builds = Arc::new(AtomicUsize::new(0));
        let counted = builds.clone();
        let make = move || match counted.fetch_add(1, SeqCst) {
            0 => Ok(Panicky::default()),
            _ => Err("no runtime".to_string()),
        };
        let (sink, mut rx) = unbounded_channel();
        let (engine, _) =
            ThreadEngine::spawn_with("t-rebuild", 1 << 20, make, quick(2), sink).unwrap();
        engine.submit(ViewEvent::command("boom", &[]));
        assert!(error_text(&rx.blocking_recv().unwrap()).contains("stopped"));
        assert_eq!(builds.load(SeqCst), 3);
    }
}
