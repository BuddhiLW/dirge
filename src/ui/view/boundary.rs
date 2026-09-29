//! Boundary: carry out a view update. The only stratum of `ui::view`
//! with effects: it sets the renderer's view state and side-panel modes
//! and fires producer replies. Chat lines are returned, not written, so
//! the UI loop keeps owning chat output (tool chambers, scroll).

use crossterm::style::Color;

use super::domain::{NoticeLevel, PanelScope, ViewEffect, ViewUpdate};
use crate::extras::panel_feed::{self, ReplyAction};
use crate::ui::colors::{c_agent, c_error};
use crate::ui::renderer::{PaneVisibility, PanelMode, Renderer};

/// A line for the chat area.
pub type ChatLine = (String, Color);

/// Apply `update` to `renderer`; the chat lines it produced.
pub fn apply(renderer: &mut Renderer, update: &ViewUpdate) -> Vec<ChatLine> {
    renderer.set_swarm(update.model.swarm.as_ref());
    update
        .effects
        .iter()
        .filter_map(|effect| interpret(renderer, effect))
        .collect()
}

fn interpret(renderer: &mut Renderer, effect: &ViewEffect) -> Option<ChatLine> {
    match effect {
        ViewEffect::Notify { level, text } => Some((text.clone(), notice_color(*level))),
        ViewEffect::Reply { action, target } => match reply_action(action, target.as_deref()) {
            Some(reply) => {
                panel_feed::spawn_reply(reply);
                None
            }
            None => Some((format!("unknown panel reply '{action}'"), c_error())),
        },
        ViewEffect::PanelMode { scope, mode } => match panel_mode(mode) {
            Some(mode) => {
                match scope {
                    PanelScope::Both => renderer.set_panel_mode(mode),
                    PanelScope::Right => renderer.set_right_panel_mode(mode),
                }
                None
            }
            None => Some((format!("unknown panel mode '{mode}'"), c_error())),
        },
        ViewEffect::Panes { left, right } => {
            renderer.set_pane_visibility(PaneVisibility {
                left: *left,
                right: *right,
            });
            None
        }
        ViewEffect::PanelStatus => Some((panel_status_line(renderer), c_agent())),
        ViewEffect::DisplayStatus => Some((display_status_line(renderer), c_agent())),
    }
}

fn notice_color(level: NoticeLevel) -> Color {
    match level {
        NoticeLevel::Info => c_agent(),
        NoticeLevel::Error => c_error(),
    }
}

/// The producer reply a `reply` effect names; `None` for an unknown
/// action or a `focus` without a target.
pub fn reply_action(action: &str, target: Option<&str>) -> Option<ReplyAction> {
    Some(match (action, target) {
        ("focus", Some(id)) => ReplyAction::Focus(id.to_string()),
        ("unfocus", _) => ReplyAction::Unfocus,
        ("next-tab", _) => ReplyAction::NextTab,
        ("prev-tab", _) => ReplyAction::PrevTab,
        ("refresh", _) => ReplyAction::Refresh,
        _ => return None,
    })
}

fn panel_mode(name: &str) -> Option<PanelMode> {
    Some(match name {
        "on" => PanelMode::On,
        "off" => PanelMode::Off,
        "auto" => PanelMode::Auto,
        "debug" => PanelMode::Debug,
        _ => return None,
    })
}

fn shown_panes(left: bool, right: bool) -> String {
    let mut shown = vec!["main"];
    if left {
        shown.insert(0, "left");
    }
    if right {
        shown.push("right");
    }
    shown.join("|")
}

fn panel_status_line(renderer: &Renderer) -> String {
    let shown = |on: bool| if on { "shown" } else { "hidden" };
    format!(
        "left panel: {:?} ({})  right panel: {:?} ({}). Use /display for per-pane control.",
        renderer.left_panel_mode(),
        shown(renderer.left_panel_visible()),
        renderer.right_panel_mode(),
        shown(renderer.right_panel_visible()),
    )
}

fn display_status_line(renderer: &Renderer) -> String {
    format!(
        "display: {} (usage: /display left|main|right)",
        shown_panes(
            renderer.left_panel_visible(),
            renderer.right_panel_visible()
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_effects_name_producer_replies() {
        assert_eq!(
            reply_action("focus", Some("a")),
            Some(ReplyAction::Focus("a".into()))
        );
        assert_eq!(reply_action("focus", None), None);
        assert_eq!(reply_action("next-tab", None), Some(ReplyAction::NextTab));
        assert_eq!(reply_action("prev-tab", None), Some(ReplyAction::PrevTab));
        assert_eq!(reply_action("refresh", None), Some(ReplyAction::Refresh));
        assert_eq!(reply_action("unfocus", None), Some(ReplyAction::Unfocus));
        assert_eq!(reply_action("warp", None), None);
    }

    #[test]
    fn every_wire_reply_round_trips() {
        for action in [
            ReplyAction::Focus("x".into()),
            ReplyAction::Unfocus,
            ReplyAction::NextTab,
            ReplyAction::PrevTab,
            ReplyAction::Refresh,
        ] {
            let target = match &action {
                ReplyAction::Focus(id) => Some(id.as_str()),
                _ => None,
            };
            assert_eq!(reply_action(action.name(), target), Some(action.clone()));
        }
    }

    #[test]
    fn panel_modes_and_panes_read_as_the_renderer_names_them() {
        assert_eq!(panel_mode("on"), Some(PanelMode::On));
        assert_eq!(panel_mode("debug"), Some(PanelMode::Debug));
        assert_eq!(panel_mode("sideways"), None);
        assert_eq!(shown_panes(true, false), "left|main");
        assert_eq!(shown_panes(false, true), "main|right");
    }
}
