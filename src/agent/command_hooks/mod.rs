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

pub use boundary::ShellRunner;
pub use domain::{HookEvent, HooksConfig};
pub use registry::CommandHooks;

/// Hooks attached to one loop: the registry plus the event that fires
/// when that loop is about to finish (`Stop` for the main agent,
/// `SubagentStop` for a forked child).
#[derive(Clone, Debug)]
pub struct HookBinding {
    pub hooks: Arc<CommandHooks>,
    pub stop_event: HookEvent,
}

impl HookBinding {
    pub fn main(hooks: Arc<CommandHooks>) -> Self {
        Self {
            hooks,
            stop_event: HookEvent::Stop,
        }
    }

    pub fn subagent(hooks: Arc<CommandHooks>) -> Self {
        Self {
            hooks,
            stop_event: HookEvent::SubagentStop,
        }
    }
}

static GLOBAL: OnceLock<Arc<CommandHooks>> = OnceLock::new();

/// Install the process-wide registry from the loaded config. No-op when
/// no hook is configured or a registry is already installed.
pub fn install_from_config(cfg: &crate::config::Config) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = dirs::home_dir();
    let hooks = CommandHooks::from_sources(
        cfg.hooks.as_ref(),
        cfg.claude_hooks.unwrap_or(false),
        crate::extras::dirge_paths::project_root(&cwd),
        home.as_deref(),
        Arc::new(ShellRunner),
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
