//! The run's events as the `:dirge/event` hook reads them.
//!
//! One hook key hears every event the front end gets, so an addon can watch
//! a new part of the run without dirge growing a hook point for it. Each
//! event reaches the hook as a flat map whose `:event` names it
//! (`:turn-start`, `:tool-call`, `:tool-result`, `:done`, …). Answers are
//! ignored and nothing waits for them.
//!
//! An event crosses as it serializes: its variant name as `:event`, its
//! fields as the other keys, both kebab-case, every string cut to
//! [`MAX_TEXT_BYTES`]. The addon host's `event-ctx` renames what some
//! events are heard as.

use serde_json::{Map, Value};

use crate::event::AgentEvent;

/// The hook key events are posted to.
pub const EVENT_KEY: &str = "dirge/event";

/// Longest text an event carries, in bytes; longer text is cut at a char
/// boundary and marked.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

/// `event` as the `:dirge/event` hook's ctx, or `None` for the events that
/// never cross: streamed token and reasoning deltas and the tool-started
/// tick (the whole response arrives with `:done`, and `:tool-call` precedes
/// every start).
pub fn project(event: &AgentEvent) -> Option<Value> {
    match event {
        AgentEvent::Token(_) | AgentEvent::Reasoning(_) | AgentEvent::ToolStarted { .. } => None,
        _ => serialized(event),
    }
}

/// `event` serialized as one flat map: `:event` its variant name, then its
/// fields (a lone unnamed field as `:value`), every string [`clip`]ped.
fn serialized(event: &AgentEvent) -> Option<Value> {
    let (name, body) = match serde_json::to_value(event).ok()? {
        Value::String(name) => (name, Value::Null),
        Value::Object(tagged) => tagged.into_iter().next()?,
        _ => return None,
    };
    let mut ctx = Map::new();
    match body {
        Value::Object(fields) => ctx.extend(fields),
        Value::Null => {}
        value => {
            ctx.insert("value".into(), value);
        }
    }
    ctx.insert("event".into(), Value::String(name));
    Some(clip_all(Value::Object(ctx)))
}

/// `value` with every string in it [`clip`]ped.
fn clip_all(value: Value) -> Value {
    match value {
        Value::String(text) if text.len() > MAX_TEXT_BYTES => Value::String(clip(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(clip_all).collect()),
        Value::Object(fields) => {
            Value::Object(fields.into_iter().map(|(k, v)| (k, clip_all(v))).collect())
        }
        other => other,
    }
}

/// `text` cut to [`MAX_TEXT_BYTES`] at a char boundary, marked when cut.
fn clip(text: &str) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[{} more bytes]", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CompactionKind;
    use serde_json::json;

    #[test]
    fn deltas_and_the_started_tick_are_not_heard() {
        assert!(project(&AgentEvent::Token("a".into())).is_none());
        assert!(project(&AgentEvent::Reasoning("a".into())).is_none());
        assert!(project(&AgentEvent::ToolStarted { id: "1".into() }).is_none());
    }

    #[test]
    fn every_other_event_crosses_as_it_serializes() {
        let cases = [
            (
                AgentEvent::TurnStart { index: 3 },
                json!({"event": "turn-start", "index": 3}),
            ),
            (
                AgentEvent::TurnEnd { index: 2 },
                json!({"event": "turn-end", "index": 2}),
            ),
            (
                AgentEvent::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    args: json!({"path": "a.rs"}),
                },
                json!({"event": "tool-call", "id": "c1", "name": "read", "args": {"path": "a.rs"}}),
            ),
            (
                AgentEvent::CompactionStarted { tokens_before: 900 },
                json!({"event": "compaction-started", "tokens-before": 900}),
            ),
            (
                AgentEvent::Usage {
                    input_tokens: 1,
                    cached_input_tokens: 2,
                    cache_creation_input_tokens: 3,
                    output_tokens: 4,
                },
                json!({
                    "event": "usage",
                    "input-tokens": 1,
                    "cached-input-tokens": 2,
                    "cache-creation-input-tokens": 3,
                    "output-tokens": 4,
                }),
            ),
            (
                AgentEvent::Done {
                    response: "ok".into(),
                    tokens: 10,
                    cost: 0.5,
                },
                json!({"event": "done", "response": "ok", "tokens": 10, "cost": 0.5}),
            ),
            (
                AgentEvent::UserMessage {
                    content: "hi".into(),
                },
                json!({"event": "user-message", "content": "hi"}),
            ),
            (
                AgentEvent::ContextCompacted {
                    new_session_id: "s2".into(),
                    tokens_before: 9,
                    tokens_after: 4,
                    summary: "sum".into(),
                    first_kept_index: 1,
                    compaction_kind: CompactionKind::PruneAndFailedSummary,
                    summary_model: None,
                },
                json!({
                    "event": "context-compacted",
                    "new-session-id": "s2",
                    "tokens-before": 9,
                    "tokens-after": 4,
                    "summary": "sum",
                    "first-kept-index": 1,
                    "compaction-kind": "prune-and-failed-summary",
                    "summary-model": null,
                }),
            ),
            (
                AgentEvent::SystemNotice {
                    content: "cap".into(),
                },
                json!({"event": "system-notice", "content": "cap"}),
            ),
            (
                AgentEvent::ContextOverflow {
                    prompt: "p".into(),
                    error: "too long".into(),
                },
                json!({"event": "context-overflow", "prompt": "p", "error": "too long"}),
            ),
            (
                AgentEvent::Error("boom".into()),
                json!({"event": "error", "value": "boom"}),
            ),
            (
                AgentEvent::EscalationActivated {
                    provider: "big".into(),
                    reason: crate::agent::agent_loop::message::EscalationReason::RepairExhausted {
                        tool: "edit".into(),
                    },
                },
                json!({
                    "event": "escalation-activated",
                    "provider": "big",
                    "reason": "RepairExhausted { tool: \"edit\" }",
                }),
            ),
        ];
        for (event, want) in cases {
            assert_eq!(project(&event).unwrap(), want, "{event:?}");
        }
    }

    #[test]
    fn a_serialized_event_cuts_every_long_string_in_it() {
        let long = "x".repeat(MAX_TEXT_BYTES + 10);
        let ctx = serialized(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "write".into(),
            args: json!({"files": [{"text": long}]}),
        })
        .unwrap();
        let text = ctx["args"]["files"][0]["text"].as_str().unwrap();
        assert!(text.ends_with("…[10 more bytes]"));
        assert_eq!(ctx["name"], "write");
    }

    #[test]
    fn long_text_is_cut_at_a_char_boundary_and_marked() {
        let long = "é".repeat(MAX_TEXT_BYTES);
        let ctx = project(&AgentEvent::ToolResult {
            id: "c1".into(),
            output: long.as_str().into(),
            kind: Default::default(),
        })
        .unwrap();
        let output = ctx["output"].as_str().unwrap();
        assert!(output.len() < long.len());
        assert!(output.contains("more bytes]"));
    }
}
