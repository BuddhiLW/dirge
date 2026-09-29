//! L0 domain: the values command hooks are made of.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{LazyLock, Mutex};

use serde::Deserialize;
use serde_json::{Map, Value};

/// Default per-command timeout, in seconds, when a hook declares none.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// The lifecycle moments a hook can be registered for. The named variants
/// are the moments dirge fires itself, under Claude Code's names. Any other
/// name is `Other`: a seam fires it by name ([`HookEvent::named`]) with no
/// change here, and addons hear it through an open hook key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    SubagentStart,
    UserPromptSubmit,
    Stop,
    SubagentStop,
    /// An event dirge has no variant for. Build it with [`HookEvent::named`],
    /// so a known name still lands on its variant.
    Other(&'static str),
}

impl HookEvent {
    /// The events dirge fires itself.
    pub const ALL: [HookEvent; 7] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::SessionStart,
        HookEvent::SubagentStart,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
        HookEvent::SubagentStop,
    ];

    /// The event called `name`: its variant when dirge fires it, `Other`
    /// otherwise.
    pub fn named(name: &str) -> HookEvent {
        HookEvent::ALL
            .into_iter()
            .find(|e| e.as_str() == name)
            .unwrap_or_else(|| HookEvent::Other(intern(name)))
    }

    /// True for an event outside [`HookEvent::ALL`].
    pub fn is_open(self) -> bool {
        matches!(self, HookEvent::Other(_))
    }

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
            HookEvent::Other(name) => name,
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

/// `name` as a `&'static str`, kept for the life of the process. Event
/// names come from config and from the seams that fire them, so the set
/// stays small; interning keeps [`HookEvent`] `Copy`.
fn intern(name: &str) -> &'static str {
    static NAMES: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Default::default);
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    match names.get(name) {
        Some(interned) => interned,
        None => {
            let interned: &'static str = Box::leak(name.into());
            names.insert(interned);
            interned
        }
    }
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `{ "type": "command", "command": ..., "timeout": ... }` entry, or
/// `{ "type": "addon", "addon": ..., "handler": ..., "timeout": ... }`,
/// answered by a handler an addon registered instead of a process. The
/// `type` is open: any other type goes to the runner installed for it
/// ([`super::boundary::install_runner`]), which reads its own fields from
/// `extra`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HookCommand {
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub command: String,
    /// Seconds.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// `type: "addon"`: the id of the addon that answers.
    #[serde(default)]
    pub addon: Option<String>,
    /// `type: "addon"`: the handler name within that addon.
    #[serde(default)]
    pub handler: Option<String>,
    /// Every other field, for the runner of any other `type`.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_kind() -> String {
    "command".to_string()
}

impl HookCommand {
    /// `type: "command"` entries with a non-blank command run, and
    /// `type: "addon"` entries naming both an addon and a handler. An entry
    /// of any other non-blank type is its runner's to judge.
    pub fn is_runnable(&self) -> bool {
        match self.kind.as_str() {
            "command" => !self.command.trim().is_empty(),
            "addon" => self.addon_target().is_some(),
            kind => !kind.trim().is_empty(),
        }
    }

    /// `(addon, handler)` of an addon entry, both non-blank.
    pub fn addon_target(&self) -> Option<(&str, &str)> {
        if self.kind != "addon" {
            return None;
        }
        let addon = self
            .addon
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        let handler = self
            .handler
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        Some((addon, handler))
    }

    /// What names this entry in a log line: its command, `addon:<id>/<handler>`,
    /// or `type:<type>`.
    pub fn label(&self) -> String {
        match (self.kind.as_str(), self.addon_target()) {
            (_, Some((addon, handler))) => format!("addon:{addon}/{handler}"),
            ("command", None) => self.command.clone(),
            (kind, None) => format!("type:{kind}"),
        }
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
    NonZeroExit {
        code: Option<i32>,
        stderr: String,
    },
    Unreadable {
        path: String,
        detail: String,
    },
    /// No runner is installed for the entry's `type`.
    NoRunner(String),
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
            HookError::NoRunner(kind) => write!(f, "no runner answers `type: {kind}` entries"),
        }
    }
}

/// The folded answer of every command that ran for one event. Combines
/// as a monoid: the first block wins, then the first ask, contexts
/// concatenate, the last input rewrite wins.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutcome {
    /// Blocking reason (exit 2, `permissionDecision: "deny"`, or
    /// `decision: "block"`).
    pub block: Option<String>,
    /// `permissionDecision: "ask"`: the action waits for the user to
    /// confirm it, with this reason shown at the prompt. A block
    /// outranks it.
    pub ask: Option<String>,
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

    pub fn asked(reason: impl Into<String>) -> Self {
        Self {
            ask: Some(reason.into()),
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
        if self.ask.is_none() {
            self.ask = other.ask;
        }
        self.context.extend(other.context);
        if other.updated_input.is_some() {
            self.updated_input = other.updated_input;
        }
        self
    }

    /// The ask still pending: `None` once a block decides the action.
    pub fn pending_ask(&self) -> Option<&str> {
        match self.block {
            Some(_) => None,
            None => self.ask.as_deref(),
        }
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

/// What `UserPromptSubmit` made of a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Send this text to the model: the prompt, with any hook context
    /// prepended.
    Proceed(String),
    /// Do not call the model. Carries the message shown to the user.
    Blocked(String),
}

/// Wraps hook context for injection into a model-visible message.
pub fn system_reminder(event: HookEvent, text: &str) -> String {
    format!("<system-reminder>\n{event} hook additional context: {text}\n</system-reminder>")
}
