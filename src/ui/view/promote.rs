//! Promote: raw UI input (a key, a submitted line) into view events,
//! decided from the latest model alone so the UI loop never asks the
//! engine. Pure.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::domain::{GridCell, ViewEvent, ViewModel};
use crate::ui::keymap::KeyAction;

/// Where a key goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRoute {
    /// A grid key, by name.
    Grid(String),
    /// A key claimed by the focused panel.
    Panel(String),
    /// Hand key focus to the focused panel (`focus_panel` from the prompt).
    EnterPanel,
    /// Give key focus back to the prompt (Esc or `focus_panel` while a
    /// panel holds it).
    LeavePanel,
    /// Not a view key: the normal dispatch handles it (global commands,
    /// Ctrl+C, typing into the prompt).
    PassThrough,
    /// Swallow it (the editor is inert while the grid is open).
    Swallow,
}

/// Who receives unmodified keys. The prompt is the default: while it has
/// focus, letters, Enter and Backspace always reach the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The prompt editor (the default).
    #[default]
    Prompt,
    /// The focused external panel: its declared keys are its verbs.
    Panel,
    /// The swarm grid is open; the editor is inert.
    Grid,
}

/// Whether the prompt editor is inserting text. A modal (vim-style)
/// editor in `Normal` mode is not, so a bare panel key may go to the
/// panel; in `Insert` mode it never does. dirge's own editor is always
/// `Insert`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    #[default]
    Insert,
    Normal,
}

/// The focus a key is routed under: the open grid wins, then a panel the
/// user handed focus to (`engaged`) while one still claims keys, else
/// the prompt.
pub fn focus_of(model: &ViewModel, engaged: bool) -> Focus {
    if model.swarm_open() {
        Focus::Grid
    } else if engaged && !model.panel_keys.is_empty() {
        Focus::Panel
    } else {
        Focus::Prompt
    }
}

/// The name an unmodified key goes by in a grid keymap; `None` with
/// Ctrl or Alt held, or for a key no keymap can name.
pub fn key_name(key: &KeyEvent) -> Option<String> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    let named = match key.code {
        KeyCode::Esc => "Esc",
        KeyCode::Tab => "Tab",
        KeyCode::BackTab => "BackTab",
        KeyCode::Enter => "Enter",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PgUp",
        KeyCode::PageDown => "PgDn",
        KeyCode::Char(c) => return Some(c.to_string()),
        _ => return None,
    };
    Some(named.to_string())
}

/// Route `key` under `focus`. Pure: decided from the model, the key,
/// the action the global keymap resolved, the focus and the editor mode.
///
/// - `Grid`: grid keys win over the global keymap (Shift+Tab is
///   `cycle_prompt` elsewhere, prev-tab here), then the focused panel's
///   keys; other resolved actions and Ctrl+C pass through, the rest is
///   swallowed.
/// - `Panel`: Esc and `focus_panel` give focus back to the prompt; the
///   panel's keys are its verbs; everything else passes through.
/// - `Prompt`: `focus_panel` hands focus to the panel when one claims
///   keys; in `EditorMode::Normal` a bare panel key (not Esc) goes to the
///   panel; everything else passes through to the editor.
pub fn route_key(
    model: &ViewModel,
    key: &KeyEvent,
    action: Option<KeyAction>,
    focus: Focus,
    mode: EditorMode,
) -> KeyRoute {
    let name = key_name(key);
    let claimed = |n: &str| model.panel_consumes(n);
    match focus {
        Focus::Grid => {
            if let Some(name) = name {
                if model.grid_consumes(&name) {
                    return KeyRoute::Grid(name);
                }
                if claimed(&name) {
                    return KeyRoute::Panel(name);
                }
            }
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if action.is_some() || ctrl_c {
                KeyRoute::PassThrough
            } else {
                KeyRoute::Swallow
            }
        }
        Focus::Panel => match name {
            _ if action == Some(KeyAction::FocusPanel) => KeyRoute::LeavePanel,
            Some(n) if n == "Esc" => KeyRoute::LeavePanel,
            Some(n) if claimed(&n) => KeyRoute::Panel(n),
            _ => KeyRoute::PassThrough,
        },
        Focus::Prompt => match name {
            _ if action == Some(KeyAction::FocusPanel) => {
                if model.panel_keys.is_empty() {
                    KeyRoute::PassThrough
                } else {
                    KeyRoute::EnterPanel
                }
            }
            Some(n) if mode == EditorMode::Normal && n != "Esc" && claimed(&n) => {
                KeyRoute::Panel(n)
            }
            _ => KeyRoute::PassThrough,
        },
    }
}

/// The grid event for key `name`, with the cells the grid paints right
/// now (in paint order).
pub fn grid_event(name: String, cells: Vec<GridCell>, columns: usize) -> ViewEvent {
    ViewEvent::Grid {
        key: name,
        cells,
        columns,
    }
}

/// Generic file-open feed op, independent of the panel producer's schema.
pub fn open_file_effect(op: &serde_json::Value) -> Option<super::domain::ViewEffect> {
    if op.get("op")?.as_str()? != "open-file" {
        return None;
    }
    Some(super::domain::ViewEffect::OpenFile {
        path: op.get("path")?.as_str()?.to_owned(),
        line: op.get("line").and_then(|n| n.as_u64()).map(|n| n as usize),
        diff: op.get("diff").and_then(|d| d.as_str()).map(str::to_owned),
    })
}

/// The event for `text` when it is a slash command the view owns.
/// View commands never reach the agent's busy gate.
pub fn view_command(model: &ViewModel, text: &str) -> Option<ViewEvent> {
    let mut words = text.split_whitespace();
    let name = words.next()?.strip_prefix('/')?;
    if !model.owns_command(name) {
        return None;
    }
    let args: Vec<&str> = words.collect();
    Some(ViewEvent::command(name, &args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(open: bool) -> ViewModel {
        ViewModel {
            swarm: open.then(Default::default),
            grid_keys: vec!["BackTab".into(), "Esc".into(), "q".into()],
            panel_keys: vec![],
            producer_keys: vec![],
            view_commands: vec!["panel".into(), "swarm".into()],
            owns_feed: false,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn keys_are_named_only_without_ctrl_or_alt() {
        assert_eq!(
            key_name(&key(KeyCode::Esc, KeyModifiers::NONE)).as_deref(),
            Some("Esc")
        );
        assert_eq!(
            key_name(&key(KeyCode::Char('q'), KeyModifiers::NONE)).as_deref(),
            Some("q")
        );
        assert_eq!(
            key_name(&key(KeyCode::BackTab, KeyModifiers::SHIFT)).as_deref(),
            Some("BackTab")
        );
        assert_eq!(key_name(&key(KeyCode::Char('s'), KeyModifiers::ALT)), None);
        assert_eq!(
            key_name(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(key_name(&key(KeyCode::F(2), KeyModifiers::NONE)), None);
    }

    /// Route as the UI loop does: the focus follows the model.
    fn route(m: &ViewModel, k: &KeyEvent, a: Option<KeyAction>, engaged: bool) -> KeyRoute {
        route_key(m, k, a, focus_of(m, engaged), EditorMode::Insert)
    }

    #[test]
    fn grid_keys_win_other_actions_pass_the_rest_is_swallowed() {
        let m = model(true);
        assert_eq!(focus_of(&m, false), Focus::Grid);
        let back = key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(
            route(&m, &back, Some(KeyAction::CyclePrompt), false),
            KeyRoute::Grid("BackTab".into())
        );
        let alt_s = key(KeyCode::Char('s'), KeyModifiers::ALT);
        assert_eq!(
            route(&m, &alt_s, Some(KeyAction::ToggleSwarm), false),
            KeyRoute::PassThrough
        );
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(route(&m, &ctrl_c, None, false), KeyRoute::PassThrough);
        assert_eq!(
            route(&m, &key(KeyCode::Char('x'), KeyModifiers::NONE), None, false),
            KeyRoute::Swallow
        );
    }

    #[test]
    fn open_file_op_only_promotes_valid_path() {
        use serde_json::json;
        assert_eq!(
            open_file_effect(
                &json!({"op":"open-file", "path":"src/main.rs", "line":7, "diff":"+x"})
            ),
            Some(super::super::domain::ViewEffect::OpenFile {
                path: "src/main.rs".into(),
                line: Some(7),
                diff: Some("+x".into())
            })
        );
        assert_eq!(
            open_file_effect(&json!({"op":"open-file", "path":"src/main.rs"})),
            Some(super::super::domain::ViewEffect::OpenFile {
                path: "src/main.rs".into(),
                line: None,
                diff: None,
            })
        );
        assert_eq!(open_file_effect(&json!({"op":"open-file"})), None);
        assert_eq!(
            open_file_effect(&json!({"op":"notify", "path":"src/main.rs"})),
            None
        );
    }

    #[test]
    fn focused_panel_claims_keys_while_the_grid_is_open() {
        let mut m = model(true);
        m.panel_keys = vec!["n".into()];
        assert_eq!(
            route(
                &m,
                &key(KeyCode::Char('n'), KeyModifiers::NONE),
                Some(KeyAction::ToggleSwarm),
                false
            ),
            KeyRoute::Panel("n".into())
        );
    }

    /// The reported bug: a focused panel's letter verbs stole the prompt.
    #[test]
    fn prompt_focus_keeps_letters_enter_and_backspace() {
        let mut m = model(false);
        m.panel_keys = vec!["Enter".into(), "Esc".into(), "g".into(), "j".into(), "p".into()];
        assert_eq!(focus_of(&m, false), Focus::Prompt);
        for code in [
            KeyCode::Char('p'),
            KeyCode::Char('g'),
            KeyCode::Char('j'),
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Esc,
        ] {
            assert_eq!(
                route(&m, &key(code, KeyModifiers::NONE), None, false),
                KeyRoute::PassThrough,
                "{code:?}"
            );
        }
    }

    #[test]
    fn focus_panel_enters_and_esc_or_focus_panel_leaves() {
        let mut m = model(false);
        m.panel_keys = vec!["Esc".into(), "PgDn".into(), "j".into(), "n".into()];
        let alt_p = key(KeyCode::Char('p'), KeyModifiers::ALT);
        assert_eq!(
            route(&m, &alt_p, Some(KeyAction::FocusPanel), false),
            KeyRoute::EnterPanel
        );
        assert_eq!(focus_of(&m, true), Focus::Panel);
        assert_eq!(
            route(&m, &key(KeyCode::Char('j'), KeyModifiers::NONE), None, true),
            KeyRoute::Panel("j".into())
        );
        assert_eq!(
            route(&m, &key(KeyCode::PageDown, KeyModifiers::NONE), None, true),
            KeyRoute::Panel("PgDn".into())
        );
        // An undeclared letter still types.
        assert_eq!(
            route(&m, &key(KeyCode::Char('z'), KeyModifiers::NONE), None, true),
            KeyRoute::PassThrough
        );
        assert_eq!(
            route(&m, &key(KeyCode::Esc, KeyModifiers::NONE), None, true),
            KeyRoute::LeavePanel
        );
        assert_eq!(
            route(&m, &alt_p, Some(KeyAction::FocusPanel), true),
            KeyRoute::LeavePanel
        );
        // Ctrl+C passes through under every focus.
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for engaged in [false, true] {
            assert_eq!(route(&m, &ctrl_c, None, engaged), KeyRoute::PassThrough);
        }
        // No panel claims keys: nothing to enter, and engaged focus lapses.
        m.panel_keys.clear();
        assert_eq!(
            route(&m, &alt_p, Some(KeyAction::FocusPanel), false),
            KeyRoute::PassThrough
        );
        assert_eq!(focus_of(&m, true), Focus::Prompt);
        assert_eq!(
            route(&m, &key(KeyCode::Char('j'), KeyModifiers::NONE), None, true),
            KeyRoute::PassThrough
        );
    }

    #[test]
    fn vim_normal_routes_bare_panel_keys_insert_never() {
        let mut m = model(false);
        m.panel_keys = vec!["Esc".into(), "j".into()];
        let j = key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(
            route_key(&m, &j, None, Focus::Prompt, EditorMode::Normal),
            KeyRoute::Panel("j".into())
        );
        assert_eq!(
            route_key(&m, &j, None, Focus::Prompt, EditorMode::Insert),
            KeyRoute::PassThrough
        );
        // Esc stays the editor's in Normal mode.
        assert_eq!(
            route_key(
                &m,
                &key(KeyCode::Esc, KeyModifiers::NONE),
                None,
                Focus::Prompt,
                EditorMode::Normal
            ),
            KeyRoute::PassThrough
        );
    }

    proptest::proptest! {
        /// Prompt focus without vim Normal never hands an unmodified
        /// printable key to a panel or the grid, whatever the panels claim.
        #[test]
        fn prompt_focus_never_steals_typing(
            c in proptest::char::any(),
            shift in proptest::bool::ANY,
            claimed in proptest::collection::vec("[ -~]|Enter|Esc|Backspace|PgDn|Up", 0..12),
            producer in proptest::bool::ANY,
        ) {
            let mut m = model(false);
            m.panel_keys = claimed.clone();
            m.panel_keys.push(c.to_string());
            if producer {
                m.grid_keys.push(c.to_string());
            }
            let mods = if shift { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
            for code in [KeyCode::Char(c), KeyCode::Enter, KeyCode::Backspace] {
                let r = route_key(&m, &key(code, mods), None, Focus::Prompt, EditorMode::Insert);
                proptest::prop_assert_eq!(r, KeyRoute::PassThrough);
            }
        }
    }

    #[test]
    fn only_owned_commands_become_view_events() {
        let m = model(false);
        assert_eq!(
            view_command(&m, "/swarm on"),
            Some(ViewEvent::command("swarm", &["on"]))
        );
        assert_eq!(
            view_command(&m, "/panel focus  a1 "),
            Some(ViewEvent::command("panel", &["focus", "a1"]))
        );
        assert_eq!(view_command(&m, "/model"), None);
        assert_eq!(view_command(&m, "swarm"), None);
        assert_eq!(view_command(&m, ""), None);
    }
}
