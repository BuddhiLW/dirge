//! Swarm view: a full-screen grid of the external panels.
//!
//! The left side panel shows external panels as compact boxes; the
//! swarm view paints the same panels at full size, one grid cell per
//! panel, above the input strip. It repaints the latest frame the
//! producer sent (no accumulating timeline): it reads
//! [`ExternalPanels`] exactly as the compact box does.
//!
//! This module is the pure half: the view state ([`SwarmView`]), the
//! key mapping while the grid is open ([`grid_key`]), `/swarm`
//! argument parsing ([`SwarmCmd::parse`]) and the grid geometry
//! ([`grid_geometry`]). The painter lives in `ui::tui::swarm`; replies
//! go through the panel feed's existing reply channel.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::extras::panel_feed::ReplyAction;
use crate::ui::keymap::KeyAction;
use crate::ui::panels_ext::ExternalPanels;

/// Narrowest grid cell (columns) before the grid drops a column.
pub const MIN_CELL_W: u16 = 24;
/// Shortest grid cell (rows: two borders plus three body rows).
pub const MIN_CELL_H: u16 = 5;

/// Key hint painted in the grid's header row.
pub const GRID_HINT: &str =
    "Tab/S-Tab view · r refresh · arrows/1-9 select · Enter focus · u unfocus · Esc close";

/// State of the open swarm view. Selection is kept by panel id so a
/// producer that reorders its panels (a new focus) does not move the
/// highlight to a different panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SwarmView {
    selected: Option<String>,
}

impl SwarmView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index of the selected panel in paint order (`panels.panels()`),
    /// falling back to the first panel when the selection is gone.
    pub fn selected_index(&self, panels: &ExternalPanels) -> usize {
        self.selected
            .as_deref()
            .and_then(|id| panels.panels().iter().position(|p| p.id == id))
            .unwrap_or(0)
    }

    /// Select the panel at `index` in paint order (clamped).
    pub fn select_index(&mut self, panels: &ExternalPanels, index: usize) {
        let all = panels.panels();
        if all.is_empty() {
            self.selected = None;
            return;
        }
        let i = index.min(all.len() - 1);
        self.selected = Some(all[i].id.clone());
    }

    /// Id of the selected panel, if any panel exists.
    pub fn selected_id(&self, panels: &ExternalPanels) -> Option<String> {
        let all = panels.panels();
        all.get(self.selected_index(panels)).map(|p| p.id.clone())
    }
}

/// `/swarm` arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwarmCmd {
    Toggle,
    Open,
    Close,
}

/// Usage line for `/swarm`.
pub const SWARM_USAGE: &str = "usage: /swarm [on|off]";

impl SwarmCmd {
    /// Parse the words after `/swarm` (pure). `Err` carries the
    /// user-facing usage message.
    pub fn parse(args: &[&str]) -> Result<Self, String> {
        match args {
            [] => Ok(Self::Toggle),
            [one] => match one.trim() {
                "" | "toggle" => Ok(Self::Toggle),
                "on" | "open" | "show" => Ok(Self::Open),
                "off" | "close" | "hide" => Ok(Self::Close),
                other => Err(format!("unknown /swarm argument '{other}' ({SWARM_USAGE})")),
            },
            _ => Err(format!("/swarm takes at most one argument ({SWARM_USAGE})")),
        }
    }
}

/// What a key does while the grid is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridKey {
    /// Close the grid.
    Close,
    /// Move the selection to this index (paint order).
    Select(usize),
    /// Send this reply to the producer.
    Reply(ReplyAction),
    /// Not a grid key: let the normal dispatch handle it (global
    /// commands, Ctrl+C).
    PassThrough,
    /// Swallow the key (the editor is inert while the grid is open).
    Swallow,
}

/// Map `key` to a grid command (pure). Unmodified grid keys win over
/// the global keymap (Shift+Tab is `cycle_prompt` globally but
/// prev-tab here); any other key the global keymap resolved (`action`)
/// passes through, so scrolling, redraw, the panel reply keys and the
/// swarm toggle keep working, and so does Ctrl+C. `columns` is the
/// grid's current column count, used by the vertical arrows.
pub fn grid_key(
    key: &KeyEvent,
    action: Option<KeyAction>,
    view: &SwarmView,
    panels: &ExternalPanels,
    columns: usize,
) -> GridKey {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if !ctrl
        && !alt
        && let Some(g) = plain_grid_key(key.code, view, panels, columns)
    {
        return g;
    }
    if action.is_some() || (ctrl && key.code == KeyCode::Char('c')) {
        GridKey::PassThrough
    } else {
        GridKey::Swallow
    }
}

/// The grid meaning of an unmodified key, `None` when it has none.
fn plain_grid_key(
    code: KeyCode,
    view: &SwarmView,
    panels: &ExternalPanels,
    columns: usize,
) -> Option<GridKey> {
    let n = panels.len();
    let cur = view.selected_index(panels);
    let cols = columns.max(1);
    let last = n.saturating_sub(1);
    let select = |i: usize| {
        Some(if n == 0 {
            GridKey::Swallow
        } else {
            GridKey::Select(i)
        })
    };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => Some(GridKey::Close),
        KeyCode::Tab => Some(GridKey::Reply(ReplyAction::NextTab)),
        KeyCode::BackTab => Some(GridKey::Reply(ReplyAction::PrevTab)),
        KeyCode::Char('r') => Some(GridKey::Reply(ReplyAction::Refresh)),
        KeyCode::Char('u') => Some(GridKey::Reply(ReplyAction::Unfocus)),
        KeyCode::Enter => Some(match view.selected_id(panels) {
            Some(id) => GridKey::Reply(ReplyAction::Focus(id)),
            None => GridKey::Swallow,
        }),
        KeyCode::Left | KeyCode::Char('h') => select(cur.saturating_sub(1)),
        KeyCode::Right | KeyCode::Char('l') => select((cur + 1).min(last)),
        KeyCode::Up | KeyCode::Char('k') => select(cur.saturating_sub(cols)),
        KeyCode::Down | KeyCode::Char('j') => {
            select(if cur + cols <= last { cur + cols } else { cur })
        }
        KeyCode::Home => select(0),
        KeyCode::End => select(last),
        KeyCode::Char(c @ '1'..='9') => {
            let i = (c as usize) - ('1' as usize);
            Some(if i < n {
                GridKey::Select(i)
            } else {
                GridKey::Swallow
            })
        }
        _ => None,
    }
}

/// Grid geometry for `n` panels in a `width` x `height` region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridGeometry {
    /// Columns of cells.
    pub cols: u16,
    /// Rows of cells shown at once.
    pub rows: u16,
    /// Index (paint order) of the first panel shown: the page that
    /// holds the selected panel.
    pub first: usize,
}

impl GridGeometry {
    /// Cells shown at once.
    pub fn per_page(&self) -> usize {
        self.cols as usize * self.rows as usize
    }
}

/// Lay out `n` panels in a `width` x `height` region (pure). The grid
/// is as square as the cell minimums allow (`ceil(sqrt(n))` columns);
/// when not every panel fits, the page holding `selected` is shown.
pub fn grid_geometry(n: usize, width: u16, height: u16, selected: usize) -> GridGeometry {
    let n = n.max(1);
    let max_cols = (width / MIN_CELL_W).max(1) as usize;
    let mut cols = 1usize;
    while cols * cols < n {
        cols += 1;
    }
    let cols = cols.min(max_cols).min(n);
    let want_rows = n.div_ceil(cols);
    let max_rows = (height / MIN_CELL_H).max(1) as usize;
    let rows = want_rows.min(max_rows);
    let per_page = cols * rows;
    let first = (selected.min(n - 1) / per_page) * per_page;
    GridGeometry {
        cols: cols as u16,
        rows: rows as u16,
        first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::panels_ext::{PanelFace, PanelLine, PanelOp};

    fn panels(ids: &[&str]) -> ExternalPanels {
        let mut s = ExternalPanels::default();
        for id in ids {
            s.apply(PanelOp::Show {
                id: (*id).into(),
                title: id.to_uppercase(),
                lines: vec![PanelLine::new("x", PanelFace::Normal)],
            });
        }
        s
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn swarm_args_parse() {
        assert_eq!(SwarmCmd::parse(&[]), Ok(SwarmCmd::Toggle));
        assert_eq!(SwarmCmd::parse(&["on"]), Ok(SwarmCmd::Open));
        assert_eq!(SwarmCmd::parse(&["off"]), Ok(SwarmCmd::Close));
        assert_eq!(SwarmCmd::parse(&["toggle"]), Ok(SwarmCmd::Toggle));
        let bad = SwarmCmd::parse(&["sideways"]).unwrap_err();
        assert!(bad.contains("usage: /swarm"), "{bad}");
        let extra = SwarmCmd::parse(&["on", "now"]).unwrap_err();
        assert!(extra.contains("at most one"), "{extra}");
    }

    #[test]
    fn selection_follows_the_panel_id() {
        let mut s = panels(&["a", "b", "c"]);
        let mut v = SwarmView::new();
        assert_eq!(v.selected_index(&s), 0);
        v.select_index(&s, 2);
        assert_eq!(v.selected_id(&s).as_deref(), Some("c"));
        // A producer focus moves `c` first; the highlight stays on it.
        s.apply(PanelOp::FocusTab {
            id: "c".into(),
            title: "C".into(),
        });
        assert_eq!(v.selected_index(&s), 0);
        assert_eq!(v.selected_id(&s).as_deref(), Some("c"));
        // A closed panel falls back to the first one.
        s.apply(PanelOp::Close { id: "c".into() });
        assert_eq!(v.selected_id(&s).as_deref(), Some("a"));
        v.select_index(&s, 99);
        assert_eq!(v.selected_id(&s).as_deref(), Some("b"));
    }

    #[test]
    fn grid_keys_reply_through_the_feed() {
        let s = panels(&["a", "b"]);
        let mut v = SwarmView::new();
        v.select_index(&s, 1);
        let k = |code| grid_key(&key(code), None, &v, &s, 2);
        assert_eq!(k(KeyCode::Tab), GridKey::Reply(ReplyAction::NextTab));
        assert_eq!(k(KeyCode::BackTab), GridKey::Reply(ReplyAction::PrevTab));
        assert_eq!(k(KeyCode::Char('r')), GridKey::Reply(ReplyAction::Refresh));
        assert_eq!(k(KeyCode::Char('u')), GridKey::Reply(ReplyAction::Unfocus));
        assert_eq!(
            k(KeyCode::Enter),
            GridKey::Reply(ReplyAction::Focus("b".into()))
        );
        assert_eq!(k(KeyCode::Esc), GridKey::Close);
        assert_eq!(k(KeyCode::Char('q')), GridKey::Close);
        assert_eq!(k(KeyCode::Char('z')), GridKey::Swallow);
    }

    #[test]
    fn grid_keys_move_the_selection() {
        let s = panels(&["a", "b", "c", "d", "e"]);
        let mut v = SwarmView::new();
        v.select_index(&s, 1);
        let k = |code, v: &SwarmView| grid_key(&key(code), None, v, &s, 3);
        assert_eq!(k(KeyCode::Right, &v), GridKey::Select(2));
        assert_eq!(k(KeyCode::Left, &v), GridKey::Select(0));
        assert_eq!(k(KeyCode::Down, &v), GridKey::Select(4));
        assert_eq!(k(KeyCode::Up, &v), GridKey::Select(0));
        assert_eq!(k(KeyCode::Char('3'), &v), GridKey::Select(2));
        assert_eq!(k(KeyCode::Char('9'), &v), GridKey::Swallow);
        assert_eq!(k(KeyCode::End, &v), GridKey::Select(4));
        v.select_index(&s, 2);
        // No cell below the last row's end: stay put.
        assert_eq!(k(KeyCode::Down, &v), GridKey::Select(2));
    }

    #[test]
    fn global_commands_and_ctrl_c_pass_through() {
        let s = panels(&["a"]);
        let v = SwarmView::new();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(grid_key(&ctrl_c, None, &v, &s, 1), GridKey::PassThrough);
        let alt_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT);
        assert_eq!(
            grid_key(&alt_s, Some(KeyAction::ToggleSwarm), &v, &s, 1),
            GridKey::PassThrough
        );
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(grid_key(&ctrl_w, None, &v, &s, 1), GridKey::Swallow);
        // Shift+Tab is `cycle_prompt` globally; in the grid it is prev-tab.
        let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(
            grid_key(&back, Some(KeyAction::CyclePrompt), &v, &s, 1),
            GridKey::Reply(ReplyAction::PrevTab)
        );
        // Unbound unmodified keys never reach the (hidden) editor.
        assert_eq!(
            grid_key(&key(KeyCode::Char('x')), None, &v, &s, 1),
            GridKey::Swallow
        );
    }

    #[test]
    fn empty_grid_only_closes_and_replies() {
        let s = ExternalPanels::default();
        let v = SwarmView::new();
        let k = |code| grid_key(&key(code), None, &v, &s, 1);
        assert_eq!(k(KeyCode::Right), GridKey::Swallow);
        assert_eq!(k(KeyCode::Enter), GridKey::Swallow);
        assert_eq!(k(KeyCode::Char('r')), GridKey::Reply(ReplyAction::Refresh));
        assert_eq!(k(KeyCode::Esc), GridKey::Close);
    }

    #[test]
    fn geometry_is_square_ish_and_pages() {
        // Three panels on a roomy screen: 2 x 2, all shown.
        let g = grid_geometry(3, 200, 50, 0);
        assert_eq!((g.cols, g.rows, g.first), (2, 2, 0));
        // One panel fills the region.
        assert_eq!(grid_geometry(1, 80, 20, 0).cols, 1);
        // Narrow: one column; short: two rows per page.
        let g = grid_geometry(6, 30, 10, 0);
        assert_eq!((g.cols, g.rows), (1, 2));
        assert_eq!(g.per_page(), 2);
        // The page holding the selection is shown.
        assert_eq!(grid_geometry(6, 30, 10, 5).first, 4);
        assert_eq!(grid_geometry(6, 30, 10, 99).first, 4);
        // Nothing to show still yields a 1 x 1 grid.
        let g = grid_geometry(0, 10, 3, 0);
        assert_eq!((g.cols, g.rows, g.first), (1, 1, 0));
    }
}
