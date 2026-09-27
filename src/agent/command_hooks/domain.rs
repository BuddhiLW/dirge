//! L0 domain: the values command hooks are made of.

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

/// Default per-command timeout, in seconds, when a hook declares none.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// The lifecycle moments a hook can be registered for. Closed: an event
/// name outside this set is carried in config but never fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    SubagentStart,
    UserPromptSubmit,
    Stop,
    SubagentStop,
}

impl HookEvent {
    pub const ALL: [HookEvent; 7] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::SessionStart,
        HookEvent::SubagentStart,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
        HookEvent::SubagentStop,
    ];

    /// The event's name in a `hooks` block and in `hook_event_name`.
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStop => "SubagentStop",
        }
    }

    /// Whether plain (non-JSON) stdout on exit 0 is model context.
    pub fn stdout_is_context(self) -> bool {
        matches!(
            self,
            HookEvent::SessionStart | HookEvent::UserPromptSubmit | HookEvent::SubagentStart
        )
    }
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `{ "type": "command", "command": ..., "timeout": ... }` entry.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HookCommand {
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub command: String,
    /// Seconds.
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn default_kind() -> String {
    "command".to_string()
}

impl HookCommand {
    /// Only `type: "command"` entries with a non-blank command run.
    pub fn is_runnable(&self) -> bool {
        self.kind == "command" && !self.command.trim().is_empty()
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1)
    }
}

/// One `{ "matcher": ..., "hooks": [...] }` group.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HookMatcher {
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookCommand>,
}

/// Event name to matcher groups: the shape of Claude Code's `hooks` key.
pub type HooksConfig = HashMap<String, Vec<HookMatcher>>;

/// A command that ran to completion.
#[derive(Debug, Clone, PartialEq)]
pub struct Exited {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Why a command produced no verdict. Every variant allows the action.
#[derive(Debug, Clone, PartialEq)]
pub enum HookError {
    SpawnFailed(String),
    TimedOut(u64),
    NonZeroExit { code: Option<i32>, stderr: String },
    Unreadable { path: String, detail: String },
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookError::SpawnFailed(e) => write!(f, "did not start: {e}"),
            HookError::TimedOut(secs) => write!(f, "timed out after {secs}s"),
            HookError::NonZeroExit { code, stderr } => {
                write!(f, "exited {code:?}: {}", stderr.trim())
            }
            HookError::Unreadable { path, detail } => write!(f, "{path}: {detail}"),
        }
    }
}

/// The folded answer of every command that ran for one event. Combines
/// as a monoid: the first block wins, contexts concatenate, the last
/// input rewrite wins.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutcome {
    /// Blocking reason (exit 2, `permissionDecision: "deny"`, or
    /// `decision: "block"`).
    pub block: Option<String>,
    /// `additionalContext` strings, plus plain stdout for the events whose
    /// stdout is context.
    pub context: Vec<String>,
    /// `hookSpecificOutput.updatedInput`, in Claude's dialect.
    pub updated_input: Option<Value>,
}

impl HookOutcome {
    pub fn blocked(reason: impl Into<String>) -> Self {
        Self {
            block: Some(reason.into()),
            ..Self::default()
        }
    }

    pub fn with_context(text: impl Into<String>) -> Self {
        Self {
            context: vec![text.into()],
            ..Self::default()
        }
    }

    pub fn combine(mut self, other: HookOutcome) -> HookOutcome {
        if self.block.is_none() {
            self.block = other.block;
        }
        self.context.extend(other.context);
        if other.updated_input.is_some() {
            self.updated_input = other.updated_input;
        }
        self
    }

    /// Context joined into one block, `None` when there is none.
    pub fn context_text(&self) -> Option<String> {
        let joined = self
            .context
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        (!joined.is_empty()).then_some(joined)
    }
}

/// Wraps hook context for injection into a model-visible message.
pub fn system_reminder(event: HookEvent, text: &str) -> String {
    format!("<system-reminder>\n{event} hook additional context: {text}\n</system-reminder>")
}
