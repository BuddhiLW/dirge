//! Addon host value types.

use std::path::PathBuf;

use serde_json::Value;

use crate::command_class::CommandClass;

/// A dirge hook point an addon may contribute to through its IAddon `hooks`
/// map. The set is closed: an addon keyed on anything else is ignored by
/// dirge, which is what lets one addon carry hooks for several hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// `(fn [ctx] -> string|nil)`, text appended to the main session's
    /// system prompt. `ctx` = `{:cwd :session-id}`.
    SystemPrompt,
    /// `(fn [ctx] -> string|nil)`, text prepended to each submitted prompt as
    /// a system reminder. `ctx` = `{:prompt :session-id :first-prompt?
    /// :tokens :ctx-max :pressure}`.
    OnPrompt,
    /// `(fn [ctx] -> nil|{:block reason}|{:context text}|{:args map})` before
    /// every tool call. `ctx` = `{:tool :args :tool-call-id}`.
    BeforeToolCall,
    /// `(fn [ctx] -> nil|{:context text})` after every tool call.
    /// `ctx` = `{:tool :args :result :error? :tool-use-id :tokens :ctx-max
    /// :pressure}`.
    AfterToolCall,
    /// `(fn [ctx] -> string|{:context text}|nil)` once per session, before its
    /// first turn in this process, once the MCP servers have connected. The
    /// text reaches that turn as a system reminder.
    /// `ctx` = `{:session-id :cwd :first-prompt? :mcp-servers}`.
    SessionStart,
    /// `(fn [ctx] -> any)` when a session ends, before the MCP servers close;
    /// bounded by a timeout, answer ignored.
    /// `ctx` = `{:session-id :cwd :reason}`, `:reason` is `exit` or `swap`.
    SessionEnd,
    // The compaction hooks `:dirge/compact` and `:dirge/before-compact` are
    // open hook keys, not variants: see `super::compaction::COMPACT` and
    // `super::compaction::BEFORE_COMPACT`.
}

impl HookPoint {
    pub const ALL: [HookPoint; 6] = [
        HookPoint::SystemPrompt,
        HookPoint::OnPrompt,
        HookPoint::BeforeToolCall,
        HookPoint::AfterToolCall,
        HookPoint::SessionStart,
        HookPoint::SessionEnd,
    ];

    /// The keyword (without the colon) an addon uses as its `hooks` key.
    pub fn key(self) -> &'static str {
        match self {
            HookPoint::SystemPrompt => "dirge/system-prompt",
            HookPoint::OnPrompt => "dirge/on-prompt",
            HookPoint::BeforeToolCall => "dirge/before-tool-call",
            HookPoint::AfterToolCall => "dirge/after-tool-call",
            HookPoint::SessionStart => "dirge/session-start",
            HookPoint::SessionEnd => "dirge/session-end",
        }
    }

    pub fn from_key(key: &str) -> Option<HookPoint> {
        let key = key.strip_prefix(':').unwrap_or(key);
        HookPoint::ALL.into_iter().find(|p| p.key() == key)
    }
}

/// What to load: manifests in load order, and the source roots `require`
/// resolves their namespaces from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddonPlan {
    pub manifests: Vec<PathBuf>,
    pub source_roots: Vec<PathBuf>,
}

impl AddonPlan {
    pub fn is_empty(&self) -> bool {
        self.manifests.is_empty()
    }
}

/// One tool an addon contributes, as its `tools` entry described it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub addon_id: String,
    /// The name the addon gave the tool.
    pub name: String,
    /// The name the model sees: `name` made safe for provider tool-name rules.
    pub exposed_name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolSpec {
    /// The name the model calls the tool by.
    pub fn model_name(&self) -> &str {
        &self.exposed_name
    }
}

/// One slash command an addon contributes, listed under the `:dirge/commands`
/// key of its `hooks` map:
/// `{:dirge/commands {"name" {:description "..." :class :read-only :handler (fn [ctx] ...)}}}`.
/// The handler gets `{:args "rest of the line" :argv [...] :cwd ...}` and
/// answers nil, a string, or a map read as a [`CommandOutput`].
#[derive(Debug, Clone, PartialEq)]
pub struct CommandSpec {
    pub addon_id: String,
    /// The name typed after `/`.
    pub name: String,
    pub description: String,
    /// Whether it may run while a turn is in flight; mutating when the
    /// addon declares none.
    pub class: CommandClass,
}

/// What an addon command asked dirge to do with its answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandOutput {
    /// Shown in the chat area.
    pub text: Option<String>,
    /// Submitted as the next user prompt, starting a turn.
    pub prompt: Option<String>,
}

/// What loading one manifest produced.
#[derive(Debug, Clone, PartialEq)]
pub struct AddonSummary {
    pub id: String,
    pub manifest: PathBuf,
    pub tools: Vec<ToolSpec>,
    pub hooks: Vec<HookPoint>,
    pub commands: Vec<CommandSpec>,
    pub health: Value,
}

/// What `/addons reload` changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReloadReport {
    /// Ids loaded, in load order.
    pub loaded: Vec<String>,
    pub failures: Vec<LoadFailure>,
    /// Source files that did not evaluate; the addons using them may run
    /// stale code.
    pub source_errors: Vec<LoadFailure>,
    /// Source files left alone (a namespace nothing has loaded, the
    /// protocol namespace), with why: editing them changes nothing.
    pub source_skips: Vec<LoadFailure>,
    /// Exposed tool names that appeared or disappeared.
    pub tools_added: Vec<String>,
    pub tools_removed: Vec<String>,
}

/// What evaluating the addons' sources again did, file by file: the ones
/// that failed and the ones left alone, each with why.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceOutcome {
    pub errors: Vec<(PathBuf, String)>,
    pub skipped: Vec<(PathBuf, String)>,
}

/// One change an addon asks of dirge's side panel.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelRequest {
    /// Create or replace panel `id` with `lines` of `(text, face)`.
    Show {
        id: String,
        title: String,
        lines: Vec<(String, String)>,
    },
    /// Create or replace panel `id` with rendered markdown.
    Markdown {
        id: String,
        title: String,
        markdown: String,
    },
    /// Add one line to log-style panel `id`.
    Append {
        id: String,
        text: String,
        face: String,
    },
    /// Make `id` a log-style panel painted first.
    Focus {
        id: String,
        title: String,
    },
    Close {
        id: String,
    },
}

/// A manifest that failed to load, kept so `/addons` can say why.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadFailure {
    pub manifest: PathBuf,
    pub error: String,
}

/// One addon's answer to a hook call.
#[derive(Debug, Clone, PartialEq)]
pub struct HookReply {
    pub addon_id: String,
    pub result: Result<Value, String>,
}

/// The ctx key under which an emit site names the fields its hook reads as
/// keywords. The host converts them and drops the key before any hook runs;
/// the fields the host already converts for a hook key apply as well.
pub const KEYWORD_FIELDS: &str = "dirge/keyword-fields";

/// `ctx` with `fields` declared as keywords for the hook it is emitted to.
/// A ctx that is not a map is returned as is.
pub fn with_keyword_fields(mut ctx: Value, fields: &[&str]) -> Value {
    if let Value::Object(map) = &mut ctx {
        map.insert(KEYWORD_FIELDS.into(), fields.into());
    }
    ctx
}

/// The folded answer of every addon to `BeforeToolCall`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeforeOutcome {
    /// `(addon-id, reason)` of the first addon that blocked.
    pub block: Option<(String, String)>,
    pub context: Vec<String>,
    /// The last replacement args any addon returned.
    pub args: Option<Value>,
}
