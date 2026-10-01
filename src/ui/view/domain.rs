//! The view bounded context's values (L0). Pure data: no renderer, no
//! channel, no engine. Every other stratum of `ui::view` speaks these.
//!
//! Ubiquitous language:
//! - a **cell** is one tile of the swarm grid: an external panel or an
//!   in-process subagent;
//! - a **view event** is something the user did to the view (a view
//!   command, a grid key);
//! - the **view model** is what the view engine publishes after folding
//!   an event: the swarm grid's state and what the view owns;
//! - a **view effect** is something the UI must do because of an event
//!   (a notice, a producer reply, a side-panel mode, opening or
//!   messaging a subagent);
//! - a **view update** is one model plus its effects.

use serde::{Deserialize, Serialize};

/// One swarm-grid cell: an external panel (by panel id) or an
/// in-process subagent (by full task id). Selection is kept by cell, so
/// a producer refocus or a finishing sibling does not move it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum GridCell {
    Panel(String),
    Agent(String),
}

/// Something the user did to the view. Closed set: the engines and the
/// UI loop agree on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ViewEvent {
    /// First event; answers the initial model.
    Init,
    /// `/name args..` for a command the view owns.
    Command { name: String, args: Vec<String> },
    /// A grid key (see `promote::key_name`), with the cells the grid
    /// shows in paint order and its current column count.
    Grid {
        key: String,
        cells: Vec<GridCell>,
        columns: usize,
    },
    /// One panel op from outside the agent loop (a panel feed), passed
    /// through undecoded; only an engine whose model `owns_feed` gets
    /// these. `feed/ended` is sent when the feed's stream ends.
    Feed { op: serde_json::Value },
    /// Key consumed by a focused external panel.
    Key { key: String, panel: String },
    /// What the panel producer accepts: its reply verbs, the grid keys
    /// bound to them (by grid key name), and `/panel`'s usage line. Sent
    /// at start and whenever the producer's advertisement changes; the
    /// reducers own no producer verbs of their own.
    Producer {
        replies: Vec<ProducerVerb>,
        keys: Vec<ProducerKey>,
        usage: String,
    },
}

/// Whether a producer reply verb names an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplyTarget {
    /// It takes none.
    None,
    /// It needs one.
    Required,
    /// It may carry one; the producer decides what it means.
    Optional,
}

/// A reply verb the producer accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProducerVerb {
    pub name: String,
    pub target: ReplyTarget,
}

/// A grid key the producer binds: to a reply verb, or (`invoke`) to a
/// verb invoked on the selected panel cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerKey {
    pub key: String,
    pub verb: String,
    pub invoke: bool,
}

impl ViewEvent {
    pub fn command(name: &str, args: &[&str]) -> Self {
        Self::Command {
            name: name.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

/// The swarm grid while it is open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SwarmModel {
    /// Selected cell; `None` paints the first cell.
    #[serde(default)]
    pub selected: Option<GridCell>,
}

/// A slash command the view owns, as `/help` and completion show it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "ViewCommandWire")]
pub struct ViewCommand {
    /// Command name, no slash.
    pub name: String,
    /// One line for `/help`.
    pub summary: String,
    /// First-argument words completion offers.
    pub args: Vec<String>,
}

impl From<&str> for ViewCommand {
    /// A command known by name only.
    fn from(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Self::default()
        }
    }
}

/// A view command as an engine writes it: a bare name, or a map.
#[derive(Deserialize)]
#[serde(untagged)]
enum ViewCommandWire {
    Name(String),
    Spec {
        name: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl From<ViewCommandWire> for ViewCommand {
    fn from(wire: ViewCommandWire) -> Self {
        match wire {
            ViewCommandWire::Name(name) => name.as_str().into(),
            ViewCommandWire::Spec {
                name,
                summary,
                args,
            } => Self {
                name,
                summary,
                args,
            },
        }
    }
}

/// What the engine publishes after every event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ViewModel {
    /// `Some` while the swarm grid is open.
    #[serde(default)]
    pub swarm: Option<SwarmModel>,
    /// Key names the open grid consumes, sorted.
    #[serde(default)]
    pub grid_keys: Vec<String>,
    /// Keys claimed by the focused panel (published by the reducer).
    #[serde(default)]
    pub panel_keys: Vec<String>,
    /// Grid keys the panel producer binds, as it advertised them.
    #[serde(default)]
    pub producer_keys: Vec<ProducerKey>,
    /// The slash commands the view owns, sorted by name. They run
    /// whether or not the agent is busy.
    #[serde(default)]
    pub view_commands: Vec<ViewCommand>,
    /// The engine owns the external panels: panel-feed ops come to it
    /// as [`ViewEvent::Feed`] and it answers with `paint`/`unpaint`.
    /// When false the UI applies feed ops itself.
    #[serde(default)]
    pub owns_feed: bool,
}

impl ViewModel {
    pub fn swarm_open(&self) -> bool {
        self.swarm.is_some()
    }

    pub fn owns_command(&self, name: &str) -> bool {
        self.view_commands.iter().any(|c| c.name == name)
    }

    /// The names of the view commands, in model order.
    #[cfg(test)]
    pub fn command_names(&self) -> Vec<&str> {
        self.view_commands.iter().map(|c| c.name.as_str()).collect()
    }

    pub fn panel_consumes(&self, key: &str) -> bool {
        self.panel_keys.iter().any(|k| k == key)
    }

    pub fn grid_consumes(&self, key: &str) -> bool {
        self.grid_keys.iter().any(|k| k == key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// Which side panels a panel-mode effect sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelScope {
    Both,
    Right,
}

/// Something the UI does after an update. Closed set: each variant has
/// one interpreter in `boundary`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ViewEffect {
    /// A line in the chat area.
    Notify { level: NoticeLevel, text: String },
    /// A reply to the external panel producer, by wire action name.
    Reply {
        action: String,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    /// Set side-panel mode(s): `on`, `off`, `auto` or `debug`.
    PanelMode { scope: PanelScope, mode: String },
    /// Force each side panel on or off.
    Panes { left: bool, right: bool },
    /// Print the side panels' state (it depends on the terminal width,
    /// which only the renderer knows).
    PanelStatus,
    /// Print which panes are shown.
    DisplayStatus,
    /// Show this subagent's chat tab (full task id).
    OpenAgent { id: String },
    /// Start a `/msg` to this subagent in the editor (full task id).
    MessageAgent { id: String },
    /// Set external panel `id` wholesale: the engine owns panel policy
    /// (what a feed op means, focus, scroll, bounds on history); the
    /// UI only sanitises and paints. `offset` counts rows away from
    /// the anchor: the top, or the bottom when `tail`.
    Paint {
        id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        rows: Vec<Vec<PaintSpan>>,
        #[serde(default)]
        tail: bool,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        focus: bool,
    },
    /// Remove external panel `id`.
    Unpaint { id: String },
    /// Ask the UI to open a project-local file (or preview a supplied diff).
    OpenFile {
        path: String,
        #[serde(default)]
        line: Option<usize>,
        #[serde(default)]
        diff: Option<String>,
    },
}

/// One styled run of a painted row; `face` is a wire face name
/// (`added`, `warn`, `dim`, ...), unknown names paint plain.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PaintSpan {
    pub text: String,
    #[serde(default)]
    pub face: String,
}

impl ViewEffect {
    pub fn notify(level: NoticeLevel, text: impl Into<String>) -> Self {
        Self::Notify {
            level,
            text: text.into(),
        }
    }
}

/// One answer from the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewUpdate {
    pub model: ViewModel,
    pub effects: Vec<ViewEffect>,
}
