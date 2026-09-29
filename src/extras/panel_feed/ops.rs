//! Wire ops -> UI effects. Pure: one event's `data` (a JSON object)
//! decodes into a [`FeedEffect`], which a [`FeedSink`] port carries
//! out. The production sink forwards to the global panel and
//! notification channels; tests record into a vector instead.
//!
//! See `docs/panel-feed.md` for the wire format.

use serde_json::{Map, Value};

use crate::ui::notifications::Notification;
use crate::ui::panels_ext::{PanelFace, PanelLine, PanelOp};
use crate::ui::view::ViewEvent;

/// What one op asks the UI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEffect {
    Panel(PanelOp),
    Notify { level: NotifyLevel, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyLevel {
    Info,
    Warn,
    Error,
}

impl NotifyLevel {
    fn from_name(name: Option<&str>) -> Self {
        match name.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("warn" | "warning") => Self::Warn,
            Some("error" | "err") => Self::Error,
            _ => Self::Info,
        }
    }
}

/// Why an event produced no effect. Never fatal: the stream goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// `data` was not a JSON object.
    NotAnObject,
    /// The object had no string `op`.
    NoOp,
    /// An op this client does not handle (a newer producer).
    UnknownOp(String),
    /// A known op missing a required field.
    Missing {
        op: &'static str,
        field: &'static str,
    },
}

/// Port the feed writes effects into.
pub trait FeedSink: Send + Sync {
    fn apply(&self, effect: FeedEffect);
}

/// The production sink: the process-global panel and notification
/// channels (both callable from any thread, both drop on overflow).
pub struct UiSink;

impl FeedSink for UiSink {
    fn apply(&self, effect: FeedEffect) {
        match effect {
            FeedEffect::Panel(op) => {
                crate::ui::panels_ext::panel_send(op);
            }
            FeedEffect::Notify { level, message } => {
                let message =
                    crate::ui::ansi::strip_escapes(&message, crate::ui::ansi::StripPolicy::STRICT);
                crate::ui::notifications::notify_send(match level {
                    NotifyLevel::Info => Notification::Info(message),
                    NotifyLevel::Warn => Notification::Warn(message),
                    NotifyLevel::Error => Notification::Error(message),
                });
            }
        }
    }
}

/// Map a producer face name onto a panel face. Extends the panel
/// module's names with a few document roles; anything unknown falls
/// back to the normal face.
pub fn face_of(name: Option<&str>) -> PanelFace {
    match name.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("heading" | "link" | "hunk") => PanelFace::Accent,
        Some("added") => PanelFace::Success,
        Some("removed") => PanelFace::Error,
        Some("code" | "plain") => PanelFace::Normal,
        Some(other) => PanelFace::from_name(other),
        None => PanelFace::Normal,
    }
}

/// One op as the view engines read it (pure): one with no `title` takes
/// its `doc`'s title when that is a string.
pub fn normalize(mut value: Value) -> Value {
    let Some(obj) = value.as_object_mut() else {
        return value;
    };
    if !obj.contains_key("title") {
        let doc_title = obj
            .get("doc")
            .and_then(Value::as_object)
            .and_then(|d| d.get("title"))
            .filter(|t| t.is_string())
            .cloned();
        if let Some(title) = doc_title {
            obj.insert("title".into(), title);
        }
    }
    value
}

fn str_field<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
}

fn panel_id(obj: &Map<String, Value>) -> Option<String> {
    str_field(obj, &["id"])
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn line_of(v: &Value) -> Option<PanelLine> {
    match v {
        Value::String(s) => Some(PanelLine::new(s.clone(), PanelFace::Normal)),
        Value::Object(o) => Some(PanelLine::new(
            str_field(o, &["text"]).unwrap_or_default(),
            face_of(str_field(o, &["face"])),
        )),
        _ => None,
    }
}

/// A multi-line `text` becomes several rows with the same face.
fn push_split(out: &mut Vec<PanelLine>, line: PanelLine) {
    if line.text.contains('\n') {
        out.extend(
            line.text
                .split('\n')
                .map(|t| PanelLine::new(t.trim_end_matches('\r'), line.face)),
        );
    } else {
        out.push(line);
    }
}

fn show_title(obj: &Map<String, Value>, id: &str) -> String {
    str_field(obj, &["title"]).unwrap_or(id).to_string()
}

fn show_panel(obj: &Map<String, Value>) -> Result<FeedEffect, Skip> {
    const OP: &str = "show";
    let id = panel_id(obj).ok_or(Skip::Missing {
        op: OP,
        field: "id",
    })?;
    let title = show_title(obj, &id);
    let mut lines = Vec::new();
    match obj.get("lines") {
        Some(Value::Array(items)) => {
            for l in items.iter().filter_map(line_of) {
                push_split(&mut lines, l);
            }
        }
        _ => {
            if let Some(text) = str_field(obj, &["text"]) {
                push_split(&mut lines, PanelLine::new(text, PanelFace::Normal));
            }
        }
    }
    // A producer that renders its document title as the first line
    // would repeat the box title; drop that one row.
    if lines
        .first()
        .is_some_and(|l| l.text == title && l.face == PanelFace::Accent)
    {
        lines.remove(0);
        if lines.first().is_some_and(|l| l.text.is_empty()) {
            lines.remove(0);
        }
    }
    Ok(FeedEffect::Panel(PanelOp::Show { id, title, lines }))
}

fn append_tab(obj: &Map<String, Value>) -> Result<FeedEffect, Skip> {
    const OP: &str = "append";
    let id = panel_id(obj).ok_or(Skip::Missing {
        op: OP,
        field: "id",
    })?;
    let line = obj
        .get("line")
        .and_then(line_of)
        .or_else(|| {
            str_field(obj, &["text"]).map(|t| PanelLine::new(t, face_of(str_field(obj, &["face"]))))
        })
        .ok_or(Skip::Missing {
            op: OP,
            field: "line",
        })?;
    Ok(FeedEffect::Panel(PanelOp::AppendTab { id, line }))
}

/// Decode one event's `data`. The event type is not consulted: every
/// op names itself in its `op` field ([`normalize`]d first).
pub fn decode(data: &str) -> Result<FeedEffect, Skip> {
    let value: Value = serde_json::from_str(data).map_err(|_| Skip::NotAnObject)?;
    let value = normalize(value);
    let obj = value.as_object().ok_or(Skip::NotAnObject)?;
    let op = str_field(obj, &["op"]).ok_or(Skip::NoOp)?;
    match op {
        "show" => show_panel(obj),
        "close" => Ok(FeedEffect::Panel(PanelOp::Close {
            id: panel_id(obj).ok_or(Skip::Missing {
                op: "close",
                field: "id",
            })?,
        })),
        "focus" => {
            let id = panel_id(obj).ok_or(Skip::Missing {
                op: "focus",
                field: "id",
            })?;
            let title = str_field(obj, &["title"]).unwrap_or(&id).to_string();
            Ok(FeedEffect::Panel(PanelOp::FocusTab { id, title }))
        }
        "append" => append_tab(obj),
        "notify" => {
            let message = str_field(obj, &["message", "text"]).ok_or(Skip::Missing {
                op: "notify",
                field: "message",
            })?;
            Ok(FeedEffect::Notify {
                level: NotifyLevel::from_name(str_field(obj, &["level"])),
                message: message.to_string(),
            })
        }
        other => Err(Skip::UnknownOp(other.to_string())),
    }
}

/// Decode and apply one event; returns the panel id a `Show` /
/// `FocusTab` / `AppendTab` touched (so the caller can close what it
/// opened when the stream ends) or `None`.
pub fn route(data: &str, sink: &dyn FeedSink) -> Option<String> {
    if let Some(event @ ViewEvent::Feed { .. }) = feed_event(data)
        && matches!(&event, ViewEvent::Feed { op } if op.get("op").and_then(Value::as_str) == Some("open-file"))
    {
        crate::ui::view::submit(event);
        return None;
    }
    match decode(data) {
        Ok(effect) => {
            let touched = match &effect {
                FeedEffect::Panel(
                    PanelOp::Show { id, .. }
                    | PanelOp::FocusTab { id, .. }
                    | PanelOp::AppendTab { id, .. },
                ) => Some(id.clone()),
                _ => None,
            };
            sink.apply(effect);
            touched
        }
        Err(skip) => {
            tracing::debug!(target: "dirge::panel_feed", ?skip, "panel feed op ignored");
            None
        }
    }
}

/// One event's `data` as a view event, for a view engine that owns the
/// panels: any JSON object passes through undecoded but [`normalize`]d
/// (the engine names what it understands); anything else is dropped here.
pub fn feed_event(data: &str) -> Option<ViewEvent> {
    match serde_json::from_str::<Value>(data) {
        Ok(op @ Value::Object(_)) => Some(ViewEvent::Feed { op: normalize(op) }),
        _ => None,
    }
}

/// Told to the view engine when the feed's stream ends, so it can
/// close the panels the producer left open.
pub fn feed_ended() -> ViewEvent {
    ViewEvent::Feed {
        op: serde_json::json!({"op": "feed/ended"}),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every effect instead of touching the global channels.
    #[derive(Default)]
    pub(crate) struct RecordingSink(pub Mutex<Vec<FeedEffect>>);

    impl RecordingSink {
        pub(crate) fn take(&self) -> Vec<FeedEffect> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    impl FeedSink for RecordingSink {
        fn apply(&self, effect: FeedEffect) {
            self.0.lock().unwrap().push(effect);
        }
    }

    fn line(text: &str, face: PanelFace) -> PanelLine {
        PanelLine::new(text, face)
    }

    #[test]
    fn show_panel_with_rendered_lines() {
        let data = r#"{"op":"show","id":"feed/main",
            "doc":{"title":"Workers","blocks":[]},
            "lines":[{"text":"Workers","face":"title"},{"text":"","face":"plain"},
                     {"text":"a  running","face":"success"},
                     {"text":"b  failed","face":"error"},
                     {"text":"note","face":"muted"}]}"#;
        assert_eq!(
            decode(data),
            Ok(FeedEffect::Panel(PanelOp::Show {
                id: "feed/main".into(),
                title: "Workers".into(),
                lines: vec![
                    line("a  running", PanelFace::Success),
                    line("b  failed", PanelFace::Error),
                    line("note", PanelFace::Dim),
                ],
            }))
        );
    }

    #[test]
    fn show_panel_title_and_multiline_text() {
        let data = r#"{"op":"show","id":"p","title":"T",
            "lines":[{"text":"one\ntwo","face":"warn"},"bare"]}"#;
        assert_eq!(
            decode(data),
            Ok(FeedEffect::Panel(PanelOp::Show {
                id: "p".into(),
                title: "T".into(),
                lines: vec![
                    line("one", PanelFace::Warn),
                    line("two", PanelFace::Warn),
                    line("bare", PanelFace::Normal),
                ],
            }))
        );
    }

    #[test]
    fn show_panel_without_title_uses_id() {
        let data = r#"{"op":"show","id":"p","lines":[]}"#;
        let Ok(FeedEffect::Panel(PanelOp::Show { title, lines, .. })) = decode(data) else {
            panic!("show expected");
        };
        assert_eq!(title, "p");
        assert!(lines.is_empty());
    }

    #[test]
    fn close_focus_append_notify() {
        assert_eq!(
            decode(r#"{"op":"close","id":"p"}"#),
            Ok(FeedEffect::Panel(PanelOp::Close { id: "p".into() }))
        );
        assert_eq!(
            decode(r#"{"op":"focus","id":"t","title":"Tab"}"#),
            Ok(FeedEffect::Panel(PanelOp::FocusTab {
                id: "t".into(),
                title: "Tab".into()
            }))
        );
        assert_eq!(
            decode(r#"{"op":"append","id":"t","line":{"text":"x","face":"dim"}}"#),
            Ok(FeedEffect::Panel(PanelOp::AppendTab {
                id: "t".into(),
                line: line("x", PanelFace::Dim)
            }))
        );
        assert_eq!(
            decode(r#"{"op":"append","id":"t","text":"y","face":"added"}"#),
            Ok(FeedEffect::Panel(PanelOp::AppendTab {
                id: "t".into(),
                line: line("y", PanelFace::Success)
            }))
        );
        assert_eq!(
            decode(r#"{"op":"notify","message":"hi","level":"warn"}"#),
            Ok(FeedEffect::Notify {
                level: NotifyLevel::Warn,
                message: "hi".into()
            })
        );
        assert_eq!(
            decode(r#"{"op":"notify","message":"hi"}"#),
            Ok(FeedEffect::Notify {
                level: NotifyLevel::Info,
                message: "hi".into()
            })
        );
    }

    #[test]
    fn unknown_and_malformed_ops_are_skipped() {
        assert_eq!(
            decode(r#"{"op":"open-file","file":"x"}"#),
            Err(Skip::UnknownOp("open-file".into())),
            "open-file belongs to the view engine, not the panel decoder"
        );
        assert_eq!(decode("not json"), Err(Skip::NotAnObject));
        assert_eq!(decode("[1]"), Err(Skip::NotAnObject));
        assert_eq!(decode(r#"{"x":1}"#), Err(Skip::NoOp));
        assert_eq!(
            decode(r#"{"op":"close","id":"  "}"#),
            Err(Skip::Missing {
                op: "close",
                field: "id"
            })
        );
        assert!(matches!(
            decode(r#"{"op":"append","id":"t"}"#),
            Err(Skip::Missing { field: "line", .. })
        ));
    }

    #[test]
    fn route_records_effects_and_reports_touched_panels() {
        let sink = RecordingSink::default();
        assert_eq!(route(r#"{"op":"show","id":"a"}"#, &sink), Some("a".into()));
        assert_eq!(route(r#"{"op":"notify","message":"m"}"#, &sink), None);
        assert_eq!(route(r#"{"op":"json/event","event":"e"}"#, &sink), None);
        let got = sink.take();
        assert_eq!(got.len(), 2, "unknown op produced no effect: {got:?}");
        assert!(matches!(got[0], FeedEffect::Panel(PanelOp::Show { .. })));
        assert!(matches!(got[1], FeedEffect::Notify { .. }));
    }

    #[test]
    fn feed_events_carry_objects_undecoded() {
        let Some(ViewEvent::Feed { op }) = feed_event(r#"{"op":"ui/whatever","x":[1]}"#) else {
            panic!("feed event expected");
        };
        assert_eq!(op, serde_json::json!({"op": "ui/whatever", "x": [1]}));
        assert_eq!(feed_event("not json"), None);
        assert_eq!(feed_event("[1]"), None);
        assert_eq!(
            feed_ended(),
            ViewEvent::Feed {
                op: serde_json::json!({"op": "feed/ended"})
            }
        );
    }

    #[test]
    fn the_retired_hive_vessel_names_are_unknown_ops() {
        for op in [
            "ui/show-panel",
            "ui/close-panel",
            "ui/focus-tab",
            "ui/append-tab",
            "ui/notify",
        ] {
            let data = format!(r#"{{"op":"{op}","id":"p","message":"m","text":"x"}}"#);
            assert_eq!(decode(&data), Err(Skip::UnknownOp(op.into())), "{op}");
        }
        assert!(
            decode(r#"{"op":"close","panel/id":"p"}"#).is_err(),
            "panel/id is no longer read as id"
        );
    }

    #[test]
    fn normalize_lifts_the_doc_title_and_keeps_everything_else() {
        use serde_json::json;
        assert_eq!(
            normalize(json!({"op": "show", "id": "p", "doc": {"title": "D"}})),
            json!({"op": "show", "id": "p", "title": "D", "doc": {"title": "D"}})
        );
        assert_eq!(
            normalize(json!({"op": "show", "title": "T", "doc": {"title": "D"}})),
            json!({"op": "show", "title": "T", "doc": {"title": "D"}}),
            "an explicit title is kept"
        );
        assert_eq!(
            normalize(json!({"op": "ui/open-file", "panel/id": "p"})),
            json!({"op": "ui/open-file", "panel/id": "p"}),
            "no names are rewritten"
        );
        assert_eq!(normalize(json!([1])), json!([1]));
    }

    #[test]
    fn face_mapping() {
        assert_eq!(face_of(Some("title")), PanelFace::Accent);
        assert_eq!(face_of(Some("heading")), PanelFace::Accent);
        assert_eq!(face_of(Some("muted")), PanelFace::Dim);
        assert_eq!(face_of(Some("removed")), PanelFace::Error);
        assert_eq!(face_of(Some("whatever")), PanelFace::Normal);
        assert_eq!(face_of(None), PanelFace::Normal);
    }
}
