//! The view seam: view state (the swarm grid) and the view commands
//! (`/swarm`, `/panel`, `/display`) are folded by a reducer off the UI
//! loop, so changing views neither waits on the agent nor makes the
//! agent wait.
//!
//! Strata, bottom up (each speaks only the language below it):
//! - [`domain`]: the bounded context's values (event, model, effect,
//!   update);
//! - [`wire`]: the JSON codec for engines that speak plain data;
//! - [`port`]: the narrow seams, [`port::ViewEngine`] (submit) and
//!   [`port::Reducer`] (step);
//! - [`promote`]: raw UI input into view events, decided from the
//!   latest model alone (Collect -> Promote);
//! - [`native`]: the reducer in Rust (Pipeline);
//! - [`engine`]: a thread that runs any reducer;
//! - [`boundary`]: carries an update out on the renderer (Boundary).
//!
//! This module is the composition root: it picks an engine from
//! [`engines`] and holds it for [`submit`]. The cljrs engine runs
//! `dirge.view` on its own isolate, apart from the addon isolate whose
//! hooks run during agent turns.

pub(crate) mod boundary;
pub(crate) mod domain;
pub(crate) mod engine;
pub(crate) mod native;
pub(crate) mod port;
pub(crate) mod promote;
/// Only engines that speak plain data use the codec (the cljrs one).
#[cfg(any(feature = "addons", test))]
pub(crate) mod wire;

#[cfg(test)]
pub(crate) mod tests;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) use domain::{ViewEvent, ViewModel, ViewUpdate};
use engine::ThreadEngine;
use native::NativeReducer;
use port::{UpdateSink, ViewEngine};

/// Builds and boots one engine; its initial model on success.
pub type EngineFactory = fn(UpdateSink) -> Result<(ThreadEngine, ViewModel), String>;

/// Stack for the native view thread.
const NATIVE_STACK_BYTES: usize = 2 * 1024 * 1024;

/// Picks the preferred engine by name (`cljrs`, `native`).
const ENGINE_ENV: &str = "DIRGE_VIEW_ENGINE";

/// The engines this build has, in default preference order. A new
/// engine is one more entry.
fn engines() -> Vec<(&'static str, EngineFactory)> {
    vec![
        #[cfg(feature = "addons")]
        (
            "cljrs",
            crate::addons::cljrs::view_isolate::engine as EngineFactory,
        ),
        ("native", native_engine as EngineFactory),
    ]
}

/// `all` with the engine named `wanted` moved to the front (pure).
fn preferred(
    mut all: Vec<(&'static str, EngineFactory)>,
    wanted: Option<&str>,
) -> Vec<(&'static str, EngineFactory)> {
    if let Some(wanted) = wanted {
        all.sort_by_key(|(name, _)| *name != wanted);
    }
    all
}

fn native_engine(sink: UpdateSink) -> Result<(ThreadEngine, ViewModel), String> {
    ThreadEngine::spawn(
        "dirge-view",
        NATIVE_STACK_BYTES,
        || Ok(NativeReducer::default()),
        sink,
    )
}

/// The running engine, set once by [`start`].
static ENGINE: OnceLock<Box<dyn ViewEngine>> = OnceLock::new();

/// Start the first engine that boots (see [`engines`], [`ENGINE_ENV`]);
/// its updates go to `sink`. The initial model; the default model (the
/// view owns nothing) when no engine boots. A second call keeps the
/// first engine.
pub fn start(sink: UpdateSink) -> ViewModel {
    let wanted = std::env::var(ENGINE_ENV).ok();
    for (name, make) in preferred(engines(), wanted.as_deref()) {
        match make(sink.clone()) {
            Ok((engine, model)) => {
                tracing::info!(target: "dirge::view", engine = name, "view engine started");
                let _ = ENGINE.set(Box::new(engine));
                // The reducers hold no producer verbs of their own: tell
                // the engine what the producer accepts (the defaults until
                // a feed advertises).
                submit(crate::extras::panel_feed::producer_event_now());
                // The feed may deliver its first SSE event immediately on
                // subscription; publish ownership before starting that task.
                set_owns_feed(model.owns_feed);
                return model;
            }
            Err(error) => {
                tracing::warn!(target: "dirge::view", engine = name, %error, "view engine unavailable")
            }
        }
    }
    ViewModel::default()
}

/// Queue `event` for the running engine. Never waits; dropped (logged)
/// before [`start`].
pub fn submit(event: ViewEvent) {
    match ENGINE.get() {
        Some(engine) => engine.submit(event),
        None => tracing::debug!(target: "dirge::view", "no view engine; event dropped"),
    }
}

/// Whether the running engine owns the external panels (its latest
/// model said `owns_feed`). Read by the panel feed from its own task,
/// so it is a flag, not the model.
static OWNS_FEED: AtomicBool = AtomicBool::new(false);

/// Set from every applied update (see `boundary::apply`).
pub(crate) fn set_owns_feed(owns: bool) {
    OWNS_FEED.store(owns, Ordering::Relaxed);
}

/// True when panel-feed ops should go to the engine as
/// [`ViewEvent::Feed`] instead of being applied by the UI directly.
pub fn owns_feed() -> bool {
    OWNS_FEED.load(Ordering::Relaxed)
}
