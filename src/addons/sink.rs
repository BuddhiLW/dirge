//! `dirge.harness` output in the TUI: `notify` as a chat-area line, `panel`
//! as a box in the left side panel.

use crate::ui::notifications::{Notification, notify_send};
use serde_json::{Value as Json, json};

use crate::ui::panels_ext::{self, PanelFace, PanelLine, PanelOp};
use crate::ui::view::ViewEvent;

use super::domain::PanelRequest;
use super::port::{HarnessSink, Level, PanelSink};

/// Width markdown panel bodies are wrapped at; the painter clips anything
/// wider to the panel.
const PANEL_WIDTH: usize = 40;

pub struct TuiSink;

impl HarnessSink for TuiSink {
    fn notify(&self, level: Level, message: &str) {
        let line = format!("[addon] {message}");
        notify_send(match level {
            Level::Info => Notification::Info(line),
            Level::Warn => Notification::Warn(line),
            Level::Error => Notification::Error(line),
        });
    }
}

impl PanelSink for TuiSink {
    /// When the running view engine owns the panels, an addon's request
    /// goes to it as a feed op, so addon panels get the same spans,
    /// eviction and focus policy as a producer's (`dirge.panels`).
    /// Otherwise the UI applies it directly, as before.
    fn panel(&self, request: PanelRequest) -> bool {
        if crate::ui::view::owns_feed() {
            crate::ui::view::submit(ViewEvent::Feed {
                op: feed_op(request),
            });
            true
        } else {
            panels_ext::panel_send(panel_op(request))
        }
    }
}

/// Who opened a panel, as the view's panel policy records it: an
/// addon's panels outlive the external producer's stream.
const ADDON_OWNER: &str = "addon";

/// The wire name of a face, for a Markdown body lowered in Rust.
fn face_name(face: PanelFace) -> &'static str {
    match face {
        PanelFace::Dim => "dim",
        PanelFace::Accent => "accent",
        PanelFace::Success => "success",
        PanelFace::Warn => "warn",
        PanelFace::Error => "error",
        PanelFace::Normal | PanelFace::Fixed(_) => "",
    }
}

/// The panel feed's neutral op for an addon's request (pure): the same
/// shape an external producer sends, marked with the addon as owner.
fn feed_op(request: PanelRequest) -> Json {
    let line = |text: String, face: String| json!({"text": text, "face": face});
    let mut op = match request {
        PanelRequest::Show { id, title, lines } => json!({
            "op": "show", "id": id, "title": title,
            "lines": lines.into_iter().map(|(t, f)| line(t, f)).collect::<Vec<_>>(),
        }),
        PanelRequest::Markdown {
            id,
            title,
            markdown,
        } => json!({
            "op": "show", "id": id, "title": title,
            "lines": panels_ext::lines_from_markdown(&markdown, PANEL_WIDTH)
                .into_iter()
                .map(|l| line(l.text, face_name(l.face).to_string()))
                .collect::<Vec<_>>(),
        }),
        PanelRequest::Append { id, text, face } => {
            json!({"op": "append", "id": id, "line": line(text, face)})
        }
        PanelRequest::Focus { id, title } => json!({"op": "focus", "id": id, "title": title}),
        PanelRequest::Close { id } => json!({"op": "close", "id": id}),
    };
    op["owner"] = Json::from(ADDON_OWNER);
    op
}

/// The panel channel's op for an addon's request.
fn panel_op(request: PanelRequest) -> PanelOp {
    let line = |text: String, face: &str| PanelLine::new(text, PanelFace::from_name(face));
    match request {
        PanelRequest::Show { id, title, lines } => PanelOp::Show {
            id,
            title,
            lines: lines
                .into_iter()
                .map(|(text, face)| line(text, &face))
                .collect(),
        },
        PanelRequest::Markdown {
            id,
            title,
            markdown,
        } => PanelOp::Show {
            id,
            title,
            lines: panels_ext::lines_from_markdown(&markdown, PANEL_WIDTH),
        },
        PanelRequest::Append { id, text, face } => PanelOp::AppendTab {
            id,
            line: line(text, &face),
        },
        PanelRequest::Focus { id, title } => PanelOp::FocusTab { id, title },
        PanelRequest::Close { id } => PanelOp::Close { id },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_map_onto_panel_ops_with_named_faces() {
        let op = panel_op(PanelRequest::Show {
            id: "swarm".into(),
            title: "Swarm".into(),
            lines: vec![("3 running".into(), "success".into())],
        });
        assert_eq!(
            op,
            PanelOp::Show {
                id: "swarm".into(),
                title: "Swarm".into(),
                lines: vec![PanelLine::new("3 running", PanelFace::Success)],
            }
        );
        assert_eq!(
            panel_op(PanelRequest::Append {
                id: "log".into(),
                text: "x".into(),
                face: "warn".into()
            }),
            PanelOp::AppendTab {
                id: "log".into(),
                line: PanelLine::new("x", PanelFace::Warn)
            }
        );
        assert_eq!(
            panel_op(PanelRequest::Close { id: "log".into() }),
            PanelOp::Close { id: "log".into() }
        );
    }

    #[test]
    fn requests_become_neutral_feed_ops_owned_by_the_addon() {
        assert_eq!(
            feed_op(PanelRequest::Show {
                id: "swarm".into(),
                title: "Swarm".into(),
                lines: vec![("3 running".into(), "success".into())],
            }),
            json!({"op": "show", "id": "swarm", "title": "Swarm", "owner": "addon",
                   "lines": [{"text": "3 running", "face": "success"}]})
        );
        assert_eq!(
            feed_op(PanelRequest::Append {
                id: "log".into(),
                text: "x".into(),
                face: "warn".into()
            }),
            json!({"op": "append", "id": "log", "owner": "addon",
                   "line": {"text": "x", "face": "warn"}})
        );
        assert_eq!(
            feed_op(PanelRequest::Focus {
                id: "log".into(),
                title: "Log".into()
            }),
            json!({"op": "focus", "id": "log", "title": "Log", "owner": "addon"})
        );
        assert_eq!(
            feed_op(PanelRequest::Close { id: "log".into() }),
            json!({"op": "close", "id": "log", "owner": "addon"})
        );
        let md = feed_op(PanelRequest::Markdown {
            id: "k".into(),
            title: "Kanban".into(),
            markdown: "- **todo** 16".into(),
        });
        assert_eq!(md["op"], "show");
        assert!(md["lines"].to_string().contains("todo"));
    }

    /// One panel policy: an addon's request, as a feed op, is painted by
    /// the cljrs `dirge.panels` reducer, and survives the end of the
    /// external producer's stream, which closes only the producer's.
    #[test]
    fn addon_panels_go_through_the_view_reducer_and_outlive_the_feed() {
        use crate::addons::cljrs::view_isolate::CljrsReducer;
        use crate::ui::view::domain::ViewEffect;
        use crate::ui::view::port::Reducer;
        std::thread::Builder::new()
            .stack_size(crate::addons::cljrs::isolate::ISOLATE_STACK_BYTES)
            .spawn(|| {
                let mut view = CljrsReducer::boot().unwrap();
                let step = |view: &mut CljrsReducer, op: Json| {
                    view.step(&ViewEvent::Feed { op }).unwrap().effects
                };
                let effects = step(
                    &mut view,
                    feed_op(PanelRequest::Show {
                        id: "kanban".into(),
                        title: "Kanban".into(),
                        lines: vec![("16 todo".into(), "warn".into())],
                    }),
                );
                assert!(matches!(
                    &effects[..],
                    [ViewEffect::Paint { id, rows, .. }]
                        if id == "kanban" && rows[0][0].text == "16 todo" && rows[0][0].face == "warn"
                ));
                step(&mut view, json!({"op": "show", "id": "flow", "lines": ["x"]}));
                assert_eq!(
                    step(&mut view, json!({"op": "feed/ended"})),
                    vec![ViewEffect::Unpaint { id: "flow".into() }],
                    "only the producer's panel closes"
                );
                assert_eq!(
                    step(&mut view, feed_op(PanelRequest::Close { id: "kanban".into() })),
                    vec![ViewEffect::Unpaint { id: "kanban".into() }]
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn markdown_bodies_become_plain_lines() {
        let PanelOp::Show { lines, .. } = panel_op(PanelRequest::Markdown {
            id: "k".into(),
            title: "Kanban".into(),
            markdown: "- **todo** 16".into(),
        }) else {
            panic!("markdown is a show");
        };
        assert!(lines.iter().any(|l| l.text.contains("todo")));
        assert!(lines.iter().all(|l| !l.text.contains('\u{1b}')));
    }
}
