//! The view reducer's behaviour, through the [`Reducer`] port. The
//! native reducer is the reference; `addons::cljrs::view_isolate` runs
//! the same scripts against `dirge.view` for parity.

use super::domain::{NoticeLevel, PanelScope, SwarmModel, ViewEffect, ViewEvent, ViewUpdate};
use super::native::NativeReducer;
use super::port::Reducer;
use super::*;

fn grid(key: &str, panels: &[&str], columns: usize) -> ViewEvent {
    ViewEvent::Grid {
        key: key.into(),
        panels: panels.iter().map(|p| p.to_string()).collect(),
        columns,
    }
}

fn cmd(name: &str, args: &[&str]) -> ViewEvent {
    ViewEvent::command(name, args)
}

fn info(text: &str) -> ViewEffect {
    ViewEffect::notify(NoticeLevel::Info, text)
}

fn run(r: &mut impl Reducer, event: ViewEvent) -> ViewUpdate {
    r.step(&event).unwrap()
}

fn selected(u: &ViewUpdate) -> Option<Option<String>> {
    u.model.swarm.as_ref().map(|s| s.selected.clone())
}

/// Events every reducer must answer alike, covering each command, each
/// grid verb and the refusals. Shared with the cljrs parity test.
pub(crate) fn parity_script() -> Vec<ViewEvent> {
    let ids = ["a", "b", "c", "d", "e"];
    let s = vec![
        ViewEvent::Init,
        cmd("swarm", &[]),
        cmd("swarm", &["on"]),
        grid("Right", &ids, 3),
        grid("Down", &ids, 3),
        grid("Down", &ids, 3),
        grid("Up", &ids, 3),
        grid("End", &ids, 3),
        grid("Home", &ids, 3),
        grid("3", &ids, 3),
        grid("9", &ids, 3),
        grid("l", &ids, 3),
        grid("Enter", &ids, 3),
        grid("Tab", &ids, 3),
        grid("BackTab", &ids, 3),
        grid("r", &ids, 3),
        grid("u", &ids, 3),
        grid("z", &ids, 3),
        grid("Enter", &[], 1),
        grid("Right", &[], 1),
        grid("Enter", &["x", "y"], 0),
        grid("Esc", &ids, 3),
        grid("Tab", &ids, 3),
        cmd("swarm", &["off"]),
        cmd("swarm", &["toggle"]),
        grid("q", &ids, 3),
        cmd("swarm", &["sideways"]),
        cmd("swarm", &["on", "now"]),
        cmd("panel", &[]),
        cmd("panel", &["on"]),
        cmd("panel", &["off"]),
        cmd("panel", &["auto"]),
        cmd("panel", &["debug"]),
        cmd("panel", &["next"]),
        cmd("panel", &["prev-tab"]),
        cmd("panel", &["refresh"]),
        cmd("panel", &["unfocus"]),
        cmd("panel", &["focus", "a1"]),
        cmd("panel", &["focus"]),
        cmd("panel", &["focus", "a", "b"]),
        cmd("panel", &["next", "x"]),
        cmd("panel", &["warp"]),
        cmd("display", &[]),
        cmd("display", &["left|main|right"]),
        cmd("display", &["MAIN,", "right"]),
        cmd("display", &["main"]),
        cmd("display", &["center"]),
        cmd("display", &["|"]),
        cmd("quit", &[]),
    ];
    s
}

#[test]
fn the_model_publishes_what_the_view_owns() {
    let u = run(&mut NativeReducer::default(), ViewEvent::Init);
    assert_eq!(u.model.view_commands, ["display", "panel", "swarm"]);
    assert!(u.model.grid_consumes("BackTab") && u.model.grid_consumes("9"));
    assert!(!u.model.grid_consumes("x"));
    let mut sorted = u.model.grid_keys.clone();
    sorted.sort();
    assert_eq!(u.model.grid_keys, sorted);
    assert!(u.effects.is_empty());
}

#[test]
fn swarm_opens_toggles_and_closes() {
    let mut r = NativeReducer::default();
    let u = run(&mut r, cmd("swarm", &[]));
    assert_eq!(selected(&u), Some(None));
    assert!(u.effects.is_empty());
    // Opening again is a no-op.
    assert!(run(&mut r, cmd("swarm", &["on"])).effects.is_empty());
    let u = run(&mut r, cmd("swarm", &[]));
    assert_eq!(selected(&u), None);
    assert_eq!(u.effects, vec![info("swarm grid closed")]);
    let u = run(&mut r, cmd("swarm", &["sideways"]));
    assert!(
        matches!(&u.effects[0], ViewEffect::Notify { level: NoticeLevel::Error, text } if text.contains("usage: /swarm"))
    );
}

#[test]
fn grid_keys_move_the_selection_by_panel_id() {
    let mut r = NativeReducer::default();
    let ids = ["a", "b", "c", "d", "e"];
    run(&mut r, cmd("swarm", &["on"]));
    assert_eq!(
        selected(&run(&mut r, grid("Right", &ids, 3))),
        Some(Some("b".into()))
    );
    assert_eq!(
        selected(&run(&mut r, grid("Down", &ids, 3))),
        Some(Some("e".into()))
    );
    // No cell below the last row: stay put.
    assert_eq!(
        selected(&run(&mut r, grid("Down", &ids, 3))),
        Some(Some("e".into()))
    );
    assert_eq!(
        selected(&run(&mut r, grid("Up", &ids, 3))),
        Some(Some("b".into()))
    );
    assert_eq!(
        selected(&run(&mut r, grid("3", &ids, 3))),
        Some(Some("c".into()))
    );
    // Out of range digits change nothing.
    assert_eq!(
        selected(&run(&mut r, grid("9", &ids, 3))),
        Some(Some("c".into()))
    );
    // A producer reorder keeps the highlight on the same panel.
    let u = run(&mut r, grid("Enter", &["c", "a", "b"], 3));
    assert_eq!(
        u.effects,
        vec![ViewEffect::Reply {
            action: "focus".into(),
            target: Some("c".into())
        }]
    );
    // A vanished selection falls back to the first panel.
    let u = run(&mut r, grid("Enter", &["x"], 1));
    assert_eq!(
        u.effects,
        vec![ViewEffect::Reply {
            action: "focus".into(),
            target: Some("x".into())
        }]
    );
    let u = run(&mut r, grid("Esc", &ids, 3));
    assert_eq!(selected(&u), None);
}

#[test]
fn grid_keys_do_nothing_while_closed_or_empty() {
    let mut r = NativeReducer::default();
    assert!(run(&mut r, grid("Tab", &["a"], 1)).effects.is_empty());
    run(&mut r, cmd("swarm", &["on"]));
    let u = run(&mut r, grid("Right", &[], 1));
    assert_eq!(selected(&u), Some(None));
    assert!(run(&mut r, grid("Enter", &[], 1)).effects.is_empty());
    // Replies still go out on an empty grid.
    assert_eq!(
        run(&mut r, grid("r", &[], 1)).effects,
        vec![ViewEffect::Reply {
            action: "refresh".into(),
            target: None
        }]
    );
}

#[test]
fn panel_sets_modes_and_replies() {
    let mut r = NativeReducer::default();
    assert_eq!(
        run(&mut r, cmd("panel", &[])).effects,
        vec![ViewEffect::PanelStatus]
    );
    assert_eq!(
        run(&mut r, cmd("panel", &["debug"])).effects,
        vec![
            ViewEffect::PanelMode {
                scope: PanelScope::Right,
                mode: "debug".into()
            },
            ViewEffect::PanelStatus
        ]
    );
    assert_eq!(
        run(&mut r, cmd("panel", &["focus", "a1"])).effects,
        vec![
            ViewEffect::Reply {
                action: "focus".into(),
                target: Some("a1".into())
            },
            info("panel reply 'focus' requested")
        ]
    );
    let u = run(&mut r, cmd("panel", &["warp"]));
    assert!(
        matches!(&u.effects[0], ViewEffect::Notify { level: NoticeLevel::Error, text } if text.contains("display modes"))
    );
}

#[test]
fn display_forces_panes() {
    let mut r = NativeReducer::default();
    assert_eq!(
        run(&mut r, cmd("display", &[])).effects,
        vec![ViewEffect::DisplayStatus]
    );
    assert_eq!(
        run(&mut r, cmd("display", &["main", "right"])).effects,
        vec![
            ViewEffect::Panes {
                left: false,
                right: true
            },
            info("display: main|right")
        ]
    );
}

#[test]
fn unknown_commands_are_refused_not_run() {
    let u = run(&mut NativeReducer::default(), cmd("quit", &[]));
    assert!(
        matches!(&u.effects[0], ViewEffect::Notify { level: NoticeLevel::Error, text } if text == "not a view command: /quit")
    );
}

#[test]
fn the_native_script_runs_clean() {
    let mut r = NativeReducer::default();
    for event in parity_script() {
        r.step(&event).unwrap();
    }
    assert_eq!(r.model().swarm, None::<SwarmModel>);
}

#[test]
fn a_wanted_engine_goes_first() {
    fn a(_: port::UpdateSink) -> Result<(engine::ThreadEngine, ViewModel), String> {
        Err("a".into())
    }
    fn b(_: port::UpdateSink) -> Result<(engine::ThreadEngine, ViewModel), String> {
        Err("b".into())
    }
    let all: Vec<(&'static str, EngineFactory)> = vec![("a", a), ("b", b)];
    let names =
        |v: Vec<(&'static str, EngineFactory)>| v.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
    assert_eq!(names(preferred(all.clone(), Some("b"))), ["b", "a"]);
    assert_eq!(names(preferred(all.clone(), Some("zzz"))), ["a", "b"]);
    assert_eq!(names(preferred(all, None)), ["a", "b"]);
}
