//! Pipeline: the view reducer in Rust, the fallback for the cljrs
//! `dirge.view` engine (the parity tests hold both to the same answers).
//!
//! What the view owns is registered in two tables, [`COMMANDS`] and
//! [`GRID_KEYMAP`]; the model's `view_commands` and `grid_keys` are
//! derived from them. Adding a view command or a grid key is a table
//! row, never an edit to a match.

use super::domain::{
    GridCell, NoticeLevel, PanelScope, ProducerKey, ProducerVerb, ReplyTarget, SwarmModel,
    ViewCommand, ViewEffect, ViewEvent, ViewModel, ViewUpdate,
};
use super::port::Reducer;
use crate::extras::panel_feed::{ReplyAction, ReplyVerb};
use crate::ui::renderer::parse_display_spec;
use crate::ui::swarm::SwarmCmd;

/// A view command: folds its arguments into the view state and names
/// the effects.
type Command = fn(&mut NativeReducer, &[&str]) -> Vec<ViewEffect>;

/// A view command's first-argument words, from the view state.
type Args = fn(&NativeReducer) -> Vec<String>;

/// One registered view command: what `/help` says, what completion
/// offers, and what runs.
struct CommandRow {
    name: &'static str,
    summary: &'static str,
    args: Args,
    run: Command,
}

/// The view commands, by name (no slash).
const COMMANDS: &[CommandRow] = &[
    CommandRow {
        name: "display",
        summary: "choose which panes (left/main/right) to show",
        args: NativeReducer::no_args,
        run: NativeReducer::display,
    },
    CommandRow {
        name: "panel",
        summary: "toggle the side panels, or send the external panel producer a verb",
        args: NativeReducer::panel_args,
        run: NativeReducer::panel,
    },
    CommandRow {
        name: "swarm",
        summary: "open or close the full-screen grid of external panels (Alt+S)",
        args: NativeReducer::swarm_args,
        run: NativeReducer::swarm,
    },
];

/// `/panel`'s display modes.
const PANEL_MODES: &[&str] = &["on", "off", "auto", "debug"];

/// What `/swarm` completes to.
const SWARM_ARGS: &[&str] = &["on", "off"];

/// A cursor move in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Move {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// A verb that acts on the selected cell; what it does depends on the
/// cell's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellVerb {
    /// A subagent: open its chat tab. A panel: whatever the producer
    /// binds to the key.
    Focus,
    /// A subagent: start a `/msg` to it. Nothing on a panel.
    Message,
}

/// What a grid key does. Only dirge's own actions: the producer's are
/// the [`ProducerKey`]s a [`ViewEvent::Producer`] brings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridVerb {
    Close,
    OnCell(CellVerb),
    Move(Move),
    /// Select the cell at this index (paint order).
    Nth(usize),
}

/// The grid keymap, by key name (see `promote::key_name`).
const GRID_KEYMAP: &[(&str, GridVerb)] = &[
    ("Esc", GridVerb::Close),
    ("q", GridVerb::Close),
    ("Enter", GridVerb::OnCell(CellVerb::Focus)),
    ("m", GridVerb::OnCell(CellVerb::Message)),
    ("Left", GridVerb::Move(Move::Left)),
    ("h", GridVerb::Move(Move::Left)),
    ("Right", GridVerb::Move(Move::Right)),
    ("l", GridVerb::Move(Move::Right)),
    ("Up", GridVerb::Move(Move::Up)),
    ("k", GridVerb::Move(Move::Up)),
    ("Down", GridVerb::Move(Move::Down)),
    ("j", GridVerb::Move(Move::Down)),
    ("Home", GridVerb::Move(Move::Home)),
    ("End", GridVerb::Move(Move::End)),
    ("1", GridVerb::Nth(0)),
    ("2", GridVerb::Nth(1)),
    ("3", GridVerb::Nth(2)),
    ("4", GridVerb::Nth(3)),
    ("5", GridVerb::Nth(4)),
    ("6", GridVerb::Nth(5)),
    ("7", GridVerb::Nth(6)),
    ("8", GridVerb::Nth(7)),
    ("9", GridVerb::Nth(8)),
];

/// The grid as a key sees it: cells in paint order, the cursor (the
/// selected cell, else the first) and the column count.
struct Grid<'a> {
    cells: &'a [GridCell],
    cur: usize,
    cols: usize,
}

impl<'a> Grid<'a> {
    fn new(cells: &'a [GridCell], selected: Option<&GridCell>, columns: usize) -> Self {
        let cur = selected
            .and_then(|sel| cells.iter().position(|c| c == sel))
            .unwrap_or(0);
        Self {
            cells,
            cur,
            cols: columns.max(1),
        }
    }

    fn last(&self) -> usize {
        self.cells.len().saturating_sub(1)
    }

    fn moved(&self, m: Move) -> usize {
        let (cur, last) = (self.cur, self.last());
        match m {
            Move::Left => cur.saturating_sub(1),
            Move::Right => (cur + 1).min(last),
            Move::Up => cur.saturating_sub(self.cols),
            Move::Down if cur + self.cols <= last => cur + self.cols,
            Move::Down => cur,
            Move::Home => 0,
            Move::End => last,
        }
    }

    /// The selected cell, `None` when the grid is empty.
    fn current(&self) -> Option<&'a GridCell> {
        self.cells.get(self.cur)
    }

    /// The cell at `index`, clamped to the last one; `None` when empty.
    fn cell_at(&self, index: usize) -> Option<&'a GridCell> {
        self.cells.get(index.min(self.last()))
    }
}

/// View state: `Some` while the swarm grid is open, and what the panel
/// producer accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeReducer {
    swarm: Option<SwarmModel>,
    replies: Vec<ProducerVerb>,
    keys: Vec<ProducerKey>,
}

impl Default for NativeReducer {
    /// Closed, with the producer defaults (as if no feed advertised).
    fn default() -> Self {
        let mut reducer = Self {
            swarm: None,
            replies: Vec::new(),
            keys: Vec::new(),
        };
        reducer.producer(&crate::extras::panel_feed::producer_event(None));
        reducer
    }
}

impl Reducer for NativeReducer {
    fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
        let effects = match event {
            ViewEvent::Init => Vec::new(),
            ViewEvent::Command { name, args } => self.command(name, args),
            ViewEvent::Grid {
                key,
                cells,
                columns,
            } => self.grid(key, cells, *columns),
            ViewEvent::Feed { op } => super::promote::open_file_effect(op).into_iter().collect(),
            ViewEvent::Key { .. } => Vec::new(),
            ViewEvent::Producer { .. } => self.producer(event),
        };
        Ok(ViewUpdate {
            model: self.model(),
            effects,
        })
    }
}

fn sorted<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = names.map(str::to_string).collect();
    out.sort();
    out
}

fn notify(level: NoticeLevel, text: impl Into<String>) -> ViewEffect {
    ViewEffect::notify(level, text)
}

fn reply(action: &str, target: Option<String>) -> ViewEffect {
    ViewEffect::Reply {
        action: action.to_string(),
        target,
        payload: None,
    }
}

impl NativeReducer {
    pub fn model(&self) -> ViewModel {
        ViewModel {
            swarm: self.swarm.clone(),
            grid_keys: {
                let mut keys = sorted(
                    GRID_KEYMAP
                        .iter()
                        .map(|(k, _)| *k)
                        .chain(self.keys.iter().map(|k| k.key.as_str())),
                );
                keys.dedup();
                keys
            },
            panel_keys: vec![],
            view_commands: self.view_commands(),
            // The native view leaves feed ops to the UI's own decoder.
            owns_feed: false,
        }
    }

    fn command(&mut self, name: &str, args: &[String]) -> Vec<ViewEffect> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match COMMANDS.iter().find(|c| c.name == name) {
            Some(c) => (c.run)(self, &args),
            None => vec![notify(
                NoticeLevel::Error,
                format!("not a view command: /{name}"),
            )],
        }
    }

    fn swarm(&mut self, args: &[&str]) -> Vec<ViewEffect> {
        let cmd = match SwarmCmd::parse(args) {
            Ok(cmd) => cmd,
            Err(usage) => return vec![notify(NoticeLevel::Error, usage)],
        };
        let open = self.swarm.is_some();
        let want = match cmd {
            SwarmCmd::Toggle => !open,
            SwarmCmd::Open => true,
            SwarmCmd::Close => false,
        };
        match (want, open) {
            (true, true) => Vec::new(),
            (true, false) => {
                self.swarm = Some(SwarmModel::default());
                Vec::new()
            }
            (false, _) => {
                self.swarm = None;
                vec![notify(NoticeLevel::Info, "swarm grid closed")]
            }
        }
    }

    fn panel(&mut self, args: &[&str]) -> Vec<ViewEffect> {
        let arg = args.first().map(|s| s.trim()).unwrap_or("");
        let mode = |scope, mode: &str| ViewEffect::PanelMode {
            scope,
            mode: mode.to_string(),
        };
        match arg {
            "" => vec![ViewEffect::PanelStatus],
            "on" | "off" | "auto" => vec![mode(PanelScope::Both, arg), ViewEffect::PanelStatus],
            "debug" => vec![mode(PanelScope::Right, "debug"), ViewEffect::PanelStatus],
            _ => match ReplyAction::parse_among(&self.reply_verbs(), args) {
                Ok(action) => {
                    vec![
                        reply(action.name(), action.target().map(str::to_owned)),
                        notify(
                            NoticeLevel::Info,
                            format!("panel reply '{}' requested", action.name()),
                        ),
                    ]
                }
                Err(usage) => vec![notify(
                    NoticeLevel::Error,
                    format!("{usage} (display modes: on|off|auto|debug)"),
                )],
            },
        }
    }

    fn display(&mut self, args: &[&str]) -> Vec<ViewEffect> {
        let spec = args.join(" ");
        if spec.trim().is_empty() {
            return vec![ViewEffect::DisplayStatus];
        }
        match parse_display_spec(&spec) {
            Ok(vis) => {
                let mut shown = vec!["main"];
                if vis.left {
                    shown.insert(0, "left");
                }
                if vis.right {
                    shown.push("right");
                }
                vec![
                    ViewEffect::Panes {
                        left: vis.left,
                        right: vis.right,
                    },
                    notify(NoticeLevel::Info, format!("display: {}", shown.join("|"))),
                ]
            }
            Err(msg) => vec![notify(NoticeLevel::Error, msg)],
        }
    }

    fn grid(&mut self, key: &str, cells: &[GridCell], columns: usize) -> Vec<ViewEffect> {
        let Some(swarm) = self.swarm.as_ref() else {
            return Vec::new();
        };
        let grid = Grid::new(cells, swarm.selected.as_ref(), columns);
        let local = GRID_KEYMAP.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        // A local verb wins, except Enter on a panel cell: that is the
        // producer's.
        let verb = match (local, grid.current()) {
            (Some(GridVerb::OnCell(CellVerb::Focus)), Some(GridCell::Panel(_))) | (None, _) => {
                return self.producer_key(key, grid.current());
            }
            (Some(verb), _) => verb,
        };
        match verb {
            GridVerb::Close => {
                self.swarm = None;
                Vec::new()
            }
            GridVerb::OnCell(verb) => self.on_cell(verb, grid.current()),
            GridVerb::Move(m) => {
                self.select(grid.cell_at(grid.moved(m)));
                Vec::new()
            }
            GridVerb::Nth(i) => {
                if i < cells.len() {
                    self.select(grid.cell_at(i));
                }
                Vec::new()
            }
        }
    }

    /// `verb` on the selected cell. Opening or messaging a subagent
    /// leaves the grid, so it closes it.
    fn on_cell(&mut self, verb: CellVerb, cell: Option<&GridCell>) -> Vec<ViewEffect> {
        match (verb, cell) {
            (CellVerb::Focus, Some(GridCell::Agent(id))) => {
                self.swarm = None;
                vec![ViewEffect::OpenAgent { id: id.clone() }]
            }
            (CellVerb::Message, Some(GridCell::Agent(id))) => {
                self.swarm = None;
                vec![ViewEffect::MessageAgent { id: id.clone() }]
            }
            _ => Vec::new(),
        }
    }

    /// Take what the producer accepts from a [`ViewEvent::Producer`].
    fn producer(&mut self, event: &ViewEvent) -> Vec<ViewEffect> {
        if let ViewEvent::Producer { replies, keys, .. } = event {
            self.replies = replies.clone();
            self.keys = keys.clone();
        }
        Vec::new()
    }

    /// The view commands as `/help` and completion show them, sorted.
    fn view_commands(&self) -> Vec<ViewCommand> {
        let mut out: Vec<ViewCommand> = COMMANDS
            .iter()
            .map(|c| ViewCommand {
                name: c.name.to_string(),
                summary: c.summary.to_string(),
                args: (c.args)(self),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn no_args(&self) -> Vec<String> {
        Vec::new()
    }

    fn swarm_args(&self) -> Vec<String> {
        SWARM_ARGS.iter().map(|s| s.to_string()).collect()
    }

    /// The display modes, then the producer's reply verbs.
    fn panel_args(&self) -> Vec<String> {
        PANEL_MODES
            .iter()
            .map(|s| s.to_string())
            .chain(self.replies.iter().map(|v| v.name.clone()))
            .collect()
    }

    fn reply_verbs(&self) -> Vec<ReplyVerb> {
        self.replies
            .iter()
            .map(|v| ReplyVerb {
                name: v.name.clone().into(),
                target: v.target,
            })
            .collect()
    }

    /// The reply the producer binds to grid `key`, on the selected
    /// `cell`: a verb that names an item names the selected panel, and
    /// an invoke needs one.
    fn producer_key(&self, key: &str, cell: Option<&GridCell>) -> Vec<ViewEffect> {
        let Some(binding) = self.keys.iter().find(|k| k.key == key) else {
            return Vec::new();
        };
        let panel = match cell {
            Some(GridCell::Panel(id)) => Some(id.clone()),
            _ => None,
        };
        if binding.invoke {
            return match panel {
                Some(id) => vec![ViewEffect::Reply {
                    action: "invoke".into(),
                    target: None,
                    payload: Some(serde_json::json!({
                        "panel": id, "verb": binding.verb, "row": null, "payload": {}
                    })),
                }],
                None => Vec::new(),
            };
        }
        let target = self
            .replies
            .iter()
            .find(|v| v.name == binding.verb)
            .map_or(ReplyTarget::None, |v| v.target);
        match (target, panel) {
            (ReplyTarget::None, _) => vec![reply(&binding.verb, None)],
            (ReplyTarget::Required, None) => Vec::new(),
            (_, panel) => vec![reply(&binding.verb, panel)],
        }
    }

    fn select(&mut self, cell: Option<&GridCell>) {
        if let Some(cell) = cell {
            self.swarm = Some(SwarmModel {
                selected: Some(cell.clone()),
            });
        }
    }
}
