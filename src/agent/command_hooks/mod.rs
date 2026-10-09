//! Claude-Code-compatible command hooks.
//!
//! A `hooks` block maps a lifecycle event to matcher groups of shell
//! commands. Each command receives the event's JSON payload on stdin and
//! answers through its exit code and, optionally, a JSON object on
//! stdout: Claude Code's hook contract, so a hook written for Claude Code
//! runs unchanged.
//!
//! Strata:
//! - [`domain`]: the values (events, commands, outcomes, errors);
//! - [`policy`], [`dialect`]: pure calculations over them;
//! - [`boundary`]: the process and file effects, behind the
//!   [`HookRunner`] port;
//! - [`registry`]: the per-event pipeline;
//! - [`loop_hooks`]: adapters onto the agent loop's hook slots.

pub mod boundary;
pub mod dialect;
pub mod domain;
pub mod loop_hooks;
pub mod policy;
pub mod registry;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub use domain::{CompactTrigger, HookEvent, HookOutcome, HooksConfig, PreCompact};
pub use registry::CommandHooks;

use crate::permission::ask::AskSender;

/// Hooks attached to one loop: the registry, the event that fires
/// when that loop is about to finish (`Stop` for the main agent,
/// `SubagentStop` for a forked child), and the permission prompt a
/// `PreToolUse` "ask" is routed to (`None` denies it: nobody to ask).
#[derive(Clone, Debug)]
pub struct HookBinding {
    pub hooks: Arc<CommandHooks>,
    pub stop_event: HookEvent,
    pub ask: Option<AskSender>,
}

impl HookBinding {
    pub fn main(hooks: Arc<CommandHooks>) -> Self {
        Self {
            hooks,
            stop_event: HookEvent::Stop,
            ask: None,
        }
    }

    pub fn subagent(hooks: Arc<CommandHooks>) -> Self {
        Self {
            hooks,
            stop_event: HookEvent::SubagentStop,
            ask: None,
        }
    }

    pub fn with_ask(mut self, ask: Option<AskSender>) -> Self {
        self.ask = ask;
        self
    }
}

static GLOBAL: OnceLock<Arc<CommandHooks>> = OnceLock::new();
static LISTENING: OnceLock<Arc<CommandHooks>> = OnceLock::new();

fn project_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::extras::dirge_paths::project_root(&cwd)
}

/// Install the process-wide registry from the loaded config. No-op when
/// no hook is configured or a registry is already installed.
pub fn install_from_config(cfg: &crate::config::Config) {
    let home = dirs::home_dir();
    let hooks = CommandHooks::from_sources(
        cfg.hooks.as_ref(),
        cfg.claude_hooks.unwrap_or(false),
        project_dir(),
        home.as_deref(),
        Arc::new(boundary::DispatchRunner::live()),
    );
    if hooks.is_empty() {
        return;
    }
    tracing::info!(
        target: "dirge::hooks",
        events = ?hooks.configured_events(),
        "command hooks installed",
    );
    let _ = GLOBAL.set(Arc::new(hooks));
}

/// The installed registry, `None` when no hook is configured.
pub fn global() -> Option<Arc<CommandHooks>> {
    GLOBAL.get().cloned()
}

/// The registry an open event ([`HookEvent::named`]) is fired on: the
/// installed one, else, once a listener is registered (the addon host
/// registers one), a registry with no entries, so the listeners hear it
/// with no hook configured. `None` when neither exists. The loops keep
/// [`global`]: this one has no entries for the events they fire.
pub fn for_open_events() -> Option<Arc<CommandHooks>> {
    open_registry(global(), &boundary::listeners(), || {
        LISTENING
            .get_or_init(|| {
                Arc::new(CommandHooks::new(
                    HooksConfig::new(),
                    project_dir(),
                    Arc::new(boundary::DispatchRunner::live()),
                ))
            })
            .clone()
    })
}

/// `configured` when hooks are, else `empty()` when some listener could
/// hear an open event, else `None`. Listeners are checked at each call:
/// the addon host registers its listener after the config is read.
fn open_registry(
    configured: Option<Arc<CommandHooks>>,
    listeners: &boundary::Listeners,
    empty: impl FnOnce() -> Arc<CommandHooks>,
) -> Option<Arc<CommandHooks>> {
    configured.or_else(|| (!listeners.is_empty()).then(empty))
}

/// Fire `PreCompact` on `hooks` for a compaction about to run and wait
/// for its answers. A block is logged, never obeyed.
pub async fn pre_compact(
    hooks: Arc<CommandHooks>,
    subject: PreCompact,
    session_id: Option<String>,
) {
    let event = PreCompact::event();
    let payload = hooks.payload(event, session_id.as_deref(), subject.fields());
    let targets = vec![subject.target().to_string()];
    ignore_block(&hooks.run_async(event, targets, payload).await);
}

/// [`pre_compact`] on the open-event registry, off the calling executor: a
/// slow hook or addon listener holds a blocking-pool thread, never the
/// single-threaded UI loop that awaits this. No-op when nothing listens.
pub async fn pre_compact_open(subject: PreCompact, session_id: Option<String>) {
    if let Some(hooks) = for_open_events() {
        pre_compact(hooks, subject, session_id).await;
    }
}

/// `PreCompact` for `subject` on `hooks`, every answer folded. Blocking.
/// Production fires through [`pre_compact_open`]; the tests read the
/// folded outcome here.
#[cfg(test)]
pub fn pre_compact_on(
    hooks: &CommandHooks,
    subject: &PreCompact,
    session_id: Option<&str>,
) -> HookOutcome {
    let event = PreCompact::event();
    let payload = hooks.payload(event, session_id, subject.fields());
    hooks.run_blocking(event, &[subject.target()], &payload)
}

fn ignore_block(outcome: &HookOutcome) {
    if let Some(reason) = &outcome.block {
        tracing::warn!(
            target: "dirge::hooks",
            %reason,
            "PreCompact block ignored: a compaction cannot be refused",
        );
    }
}
