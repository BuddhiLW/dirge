//! The run's events as the `:dirge/event` hook reads them.
//!
//! One hook key hears every event the front end gets, so an addon can watch
//! a new part of the run without dirge growing a hook point for it. Each
//! event reaches the hook as a flat map whose `:event` names it
//! (`:turn-start`, `:tool-call`, `:tool-result`, `:done`, …). Answers are
//! ignored and nothing waits for them.
//!
//! An event is heard as it serializes: its variant name as `:event`, its
//! fields as the other keys, both kebab-case, every string cut to
//! [`MAX_TEXT_BYTES`]. [`shape`] names the events heard differently.

use serde_json::{Map, Value, json};

use crate::event::AgentEvent;

/// The hook key events are posted to.
pub const EVENT_KEY: &str = "dirge/event";

/// Longest text an event carries, in bytes; longer text is cut at a char
/// boundary and marked.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

/// `event` as the `:dirge/event` hook's ctx, or `None` for the events it
/// does not hear.
pub fn project(event: &AgentEvent) -> Option<Value> {
    match shape(event) {
        Shape::Unheard => None,
        Shape::Map(ctx) => Some(ctx),
        Shape::Serialized => serialized(event),
    }
}

/// How the hook hears one event.
enum Shape {
    /// Not at all.
    Unheard,
    /// As this map.
    Map(Value),
    /// As [`serialized`] makes it.
    Serialized,
}

/// The events not heard as they serialize: streamed token and reasoning
/// deltas and the tool-started tick are not heard (the whole response
/// arrives with `:done`, and `:tool-call` precedes every start); the rest
/// keep the names and fields they were first heard with.
fn shape(event: &AgentEvent) -> Shape {
    let ctx = match event {
        AgentEvent::Token(_) | AgentEvent::Reasoning(_) | AgentEvent::ToolStarted { .. } => {
            return Shape::Unheard;
        }
        AgentEvent::ToolCall { id, name, args } => {
            json!({ "event": "tool-call", "id": id.as_str(), "tool": name.as_str(), "args": args })
        }
        AgentEvent::ToolResult { id, output, .. } => {
            json!({ "event": "tool-result", "id": id.as_str(), "output": clip(output) })
        }
        AgentEvent::Error(message) => json!({ "event": "error", "message": clip(message) }),
        AgentEvent::ContextOverflow { error, .. } => {
            json!({ "event": "context-overflow", "message": clip(error) })
        }
        AgentEvent::ContextCompacted {
            new_session_id,
            tokens_before,
            tokens_after,
            summary,
            compaction_kind,
            ..
        } => json!({
            "event": "context-compacted",
            "session-id": new_session_id.as_str(),
            "tokens-before": tokens_before,
            "tokens-after": tokens_after,
            "summary": clip(summary),
            "kind": compaction_kind,
        }),
        AgentEvent::CheckpointRefresh { summary } => {
            json!({ "event": "checkpoint", "summary": clip(summary) })
        }
        AgentEvent::CustomMessage { payload } => {
            json!({ "event": "custom-message", "payload": payload })
        }
        AgentEvent::Interjected {
            partial_response,
            tokens,
        } => json!({
            "event": "interjected",
            "response": clip(partial_response),
            "tokens": tokens,
        }),
        AgentEvent::RetryNotice {
            attempt,
            delay_ms,
            error,
        } => json!({
            "event": "retry",
            "attempt": attempt,
            "delay-ms": delay_ms,
            "message": clip(error),
        }),
        AgentEvent::SystemNotice { content } => {
            json!({ "event": "notice", "content": clip(content) })
        }
        AgentEvent::RepairStats { .. } => json!({ "event": "repair-stats" }),
        AgentEvent::EscalationActivated { provider, reason } => json!({
            "event": "escalation",
            "provider": provider.as_str(),
            "reason": format!("{reason:?}"),
        }),
        _ => return Shape::Serialized,
    };
    Shape::Map(ctx)
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

    #[test]
    fn deltas_and_the_started_tick_are_not_heard() {
        assert!(project(&AgentEvent::Token("a".into())).is_none());
        assert!(project(&AgentEvent::Reasoning("a".into())).is_none());
        assert!(project(&AgentEvent::ToolStarted { id: "1".into() }).is_none());
    }

    #[test]
    fn a_tool_call_names_its_tool_and_args() {
        let ctx = project(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: json!({"path": "a.rs"}),
        })
        .unwrap();
        assert_eq!(
            ctx,
            json!({"event": "tool-call", "id": "c1", "tool": "read", "args": {"path": "a.rs"}})
        );
    }

    #[test]
    fn turn_bounds_and_the_end_of_a_run_are_heard() {
        assert_eq!(
            project(&AgentEvent::TurnEnd { index: 2 }).unwrap(),
            json!({"event": "turn-end", "index": 2})
        );
        let done = project(&AgentEvent::Done {
            response: "ok".into(),
            tokens: 10,
            cost: 0.5,
        })
        .unwrap();
        assert_eq!(done["event"], "done");
        assert_eq!(done["response"], "ok");
    }

    #[test]
    fn events_heard_as_they_serialize_keep_the_maps_they_were_heard_as() {
        let cases = [
            (
                AgentEvent::TurnStart { index: 3 },
                json!({"event": "turn-start", "index": 3}),
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
                    "session-id": "s2",
                    "tokens-before": 9,
                    "tokens-after": 4,
                    "summary": "sum",
                    "kind": "prune-and-failed-summary",
                }),
            ),
        ];
        for (event, want) in cases {
            assert_eq!(project(&event).unwrap(), want, "{event:?}");
        }
    }

    #[test]
    fn an_event_without_a_shape_of_its_own_is_heard_by_variant_name_and_fields() {
        assert_eq!(
            serialized(&AgentEvent::SystemNotice {
                content: "cap".into()
            })
            .unwrap(),
            json!({"event": "system-notice", "content": "cap"})
        );
        assert_eq!(
            serialized(&AgentEvent::ContextOverflow {
                prompt: "p".into(),
                error: "too long".into(),
            })
            .unwrap(),
            json!({"event": "context-overflow", "prompt": "p", "error": "too long"})
        );
        assert_eq!(
            serialized(&AgentEvent::Error("boom".into())).unwrap(),
            json!({"event": "error", "value": "boom"})
        );
        assert_eq!(
            serialized(&AgentEvent::EscalationActivated {
                provider: "big".into(),
                reason: crate::agent::agent_loop::message::EscalationReason::RepairExhausted {
                    tool: "edit".into()
                },
            })
            .unwrap(),
            json!({"event": "escalation-activated", "provider": "big"})
        );
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
