//! Swarm view: a full-screen grid of the external panels.
//!
//! The left side panel shows external panels as compact boxes; the
//! swarm view paints the same panels at full size, one grid cell per
//! panel, above the input strip. It repaints the latest frame the
//! producer sent (no accumulating timeline): it reads
//! [`ExternalPanels`] exactly as the compact box does.
//!
//! This module is the pure half the painter needs: the selection it
//! paints ([`SwarmView`]), `/swarm` argument parsing
//! ([`SwarmCmd::parse`]) and the grid geometry ([`grid_geometry`]). The
//! grid's state and keys belong to the view seam (`ui::view`); the
//! painter lives in `ui::tui::swarm`.

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

    /// The grid with `selected` highlighted (`None`: the first panel).
    pub fn selecting(selected: Option<String>) -> Self {
        Self { selected }
    }

    /// Index of the selected panel in paint order (`panels.panels()`),
    /// falling back to the first panel when the selection is gone.
    pub fn selected_index(&self, panels: &ExternalPanels) -> usize {
        self.selected
            .as_deref()
            .and_then(|id| panels.panels().iter().position(|p| p.id == id))
            .unwrap_or(0)
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
    fn the_painted_selection_follows_the_panel_id() {
        let mut s = panels(&["a", "b", "c"]);
        assert_eq!(SwarmView::new().selected_index(&s), 0);
        let v = SwarmView::selecting(Some("c".into()));
        assert_eq!(v.selected_index(&s), 2);
        // A producer focus moves `c` first; the highlight stays on it.
        s.apply(PanelOp::FocusTab {
            id: "c".into(),
            title: "C".into(),
        });
        assert_eq!(v.selected_index(&s), 0);
        // A closed panel falls back to the first one.
        s.apply(PanelOp::Close { id: "c".into() });
        assert_eq!(v.selected_index(&s), 0);
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
