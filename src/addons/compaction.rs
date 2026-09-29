//! Adapters from the addon host onto the agent loop's compaction hooks:
//! `:dirge/before-compact` hears a fold about to run, `:dirge/compact` may
//! answer the summary for the span it folds. Both run on a blocking thread
//! of the agent runtime, where addon code may reach MCP, within a budget;
//! no answer in time is no answer, and the fold goes on as it would without
//! them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::domain::HookPoint;
use super::host::AddonHost;
use super::policy;
use crate::agent::agent_loop::types::{
    CompactionFacts, CompactionHooks, OnBeforeCompactFn, OnCompactFn,
};
use crate::agent::compression::{estimate_messages_tokens, validate_summary};
use crate::runtime::blocking_within;

/// How long one compaction hook may take when `addons.compact_timeout_secs`
/// is not set.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(60);

/// Characters of a call's JSON arguments its `args` summary keeps.
const ARGS_SUMMARY_CHARS: usize = 200;

/// The host's compaction hooks for the runs of `session_id`, each call
/// bounded by `budget`. `None` when no addon listens on either point.
pub fn hooks(
    host: Arc<AddonHost>,
    session_id: Option<String>,
    budget: Duration,
) -> Option<CompactionHooks> {
    if !host.listens(HookPoint::Compact) && !host.listens(HookPoint::BeforeCompact) {
        return None;
    }
    let (before_host, before_session) = (host.clone(), session_id.clone());
    let on_before: OnBeforeCompactFn = Arc::new(move |count, tokens, facts| {
        let (host, session_id) = (before_host.clone(), before_session.clone());
        Box::pin(async move {
            let heard = blocking_within(budget, move || {
                host.before_compact(&before_ctx(count, tokens, &facts, session_id.as_deref()))
            })
            .await;
            if let Err(why) = heard {
                tracing::warn!(target: "dirge::addon", %why, "addon before-compact hook skipped");
            }
        })
    });
    let on_compact: OnCompactFn = Arc::new(move |messages, facts| {
        let (host, session_id) = (host.clone(), session_id.clone());
        Box::pin(async move {
            blocking_within(budget, move || {
                let ctx = compact_ctx(&messages, &facts, session_id.as_deref());
                host.compact(&ctx, validate_summary)
            })
            .await
            .unwrap_or_else(|why| {
                tracing::warn!(
                    target: "dirge::addon",
                    %why,
                    "addon compact hook skipped; dirge summarizes"
                );
                None
            })
        })
    });
    Some(CompactionHooks {
        on_before,
        on_compact,
    })
}

/// `:dirge/before-compact`'s ctx for a fold of `count` messages holding
/// `tokens`.
pub fn before_ctx(
    count: usize,
    tokens: u64,
    facts: &CompactionFacts,
    session_id: Option<&str>,
) -> Value {
    json!({
        "count": count,
        "tokens": tokens,
        "reason": facts.reason,
        "ctx-max": facts.usage.ctx_max,
        "pressure": facts.usage.pressure(),
        "session-id": session_id,
    })
}

/// `:dirge/compact`'s ctx for folding `messages`.
pub fn compact_ctx(messages: &[Value], facts: &CompactionFacts, session_id: Option<&str>) -> Value {
    json!({
        "span": span(messages),
        "tokens": estimate_messages_tokens(messages),
        "reason": facts.reason,
        "focus": facts.focus,
        "ctx-max": facts.usage.ctx_max,
        "pressure": facts.usage.pressure(),
        "session-id": session_id,
    })
}

/// `messages` as `:dirge/compact`'s `:span`, in order: one entry for each
/// user and system message and each assistant text, one for each tool call
/// (role `assistant`), one for each tool result (role `tool`). A call and
/// its result carry the same `tool-use-id`, the tool's name as `tool`, and
/// the call's arguments cut to a summary as `args`; a call's `text` is that
/// summary too.
pub fn span(messages: &[Value]) -> Vec<Value> {
    let args = call_args(messages);
    let mut out = Vec::new();
    for message in messages {
        let content = message.get("content");
        match message.get("role").and_then(Value::as_str) {
            Some(role @ ("user" | "system")) => out.push(entry(role, text_of(content))),
            Some("assistant") => {
                let calls = calls_of(content);
                let text = text_of(content);
                if !text.is_empty() || calls.is_empty() {
                    out.push(entry("assistant", text));
                }
                for call in calls {
                    let summary = args_summary(call.get("arguments"));
                    let mut e = entry("assistant", summary.clone());
                    tag(&mut e, "tool", str_of(call, &["name"]));
                    tag(&mut e, "tool-use-id", str_of(call, &["id"]));
                    e.insert("args".into(), summary.into());
                    out.push(Value::Object(e));
                }
            }
            Some("toolResult" | "tool") => {
                let id = str_of(message, &["toolCallId", "tool_call_id"]);
                let mut e = entry("tool", text_of(content));
                tag(&mut e, "tool", str_of(message, &["toolName", "tool_name"]));
                tag(&mut e, "tool-use-id", id);
                if let Some(summary) = id.and_then(|id| args.get(id)) {
                    e.insert("args".into(), summary.clone().into());
                }
                out.push(Value::Object(e));
            }
            _ => {}
        }
    }
    out
}

fn entry(role: &str, text: String) -> Map<String, Value> {
    let mut e = Map::new();
    e.insert("role".into(), role.into());
    e.insert("text".into(), text.into());
    e
}

fn tag(e: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        e.insert(key.into(), value.into());
    }
}

fn str_of<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str))
}

/// A message's text as the tool-call hooks read it: a string as is, the
/// text blocks of an array joined by newlines.
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => policy::content_text(blocks),
        _ => String::new(),
    }
}

/// The `toolCall` blocks of an assistant message's content.
fn calls_of(content: Option<&Value>) -> Vec<&Value> {
    match content {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"))
            .collect(),
        _ => Vec::new(),
    }
}

/// The args summary of every tool call in `messages`, by call id.
fn call_args(messages: &[Value]) -> HashMap<&str, String> {
    messages
        .iter()
        .flat_map(|m| calls_of(m.get("content")))
        .filter_map(|call| Some((str_of(call, &["id"])?, args_summary(call.get("arguments")))))
        .collect()
}

/// `args` as compact JSON, cut to [`ARGS_SUMMARY_CHARS`] characters with a
/// trailing `…` when longer.
fn args_summary(args: Option<&Value>) -> String {
    let json = args.map(Value::to_string).unwrap_or_default();
    match json.char_indices().nth(ARGS_SUMMARY_CHARS) {
        Some((cut, _)) => format!("{}…", &json[..cut]),
        None => json,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::domain::HookReply;
    use crate::addons::host::tests::{ScriptedRuntime, summary};
    use crate::agent::agent_loop::context_manager::ContextUsage;

    const VALID: &str = "## Active Task\nFix the fold.\n## Completed Actions\nRead the loop.";

    fn call(id: &str, name: &str, args: Value) -> Value {
        json!({"type": "toolCall", "id": id, "name": name, "arguments": args})
    }

    fn result(id: &str, name: &str, text: &str) -> Value {
        json!({
            "role": "toolResult",
            "toolCallId": id,
            "toolName": name,
            "content": [{"type": "text", "text": text}],
        })
    }

    fn conversation() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "fix the fold"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "reading it"},
                    call("c1", "read", json!({"path": "run.rs"})),
                    call("c2", "bash", json!({"command": "cargo check"})),
                ],
            }),
            result("c1", "read", "fn run() {}"),
            result("c2", "bash", "ok"),
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        ]
    }

    fn facts() -> CompactionFacts {
        CompactionFacts::pressure(ContextUsage {
            tokens: 90_000,
            ctx_max: 120_000,
        })
    }

    fn ids(span: &[Value], role: &str) -> Vec<String> {
        span.iter()
            .filter(|e| e["role"] == role && e.get("tool-use-id").is_some())
            .map(|e| e["tool-use-id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_span_lists_texts_calls_and_results_in_order() {
        let span = span(&conversation());
        let roles: Vec<&str> = span.iter().map(|e| e["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            vec![
                "user",
                "assistant",
                "assistant",
                "assistant",
                "tool",
                "tool",
                "assistant"
            ]
        );
        assert_eq!(span[0], json!({"role": "user", "text": "fix the fold"}));
        assert_eq!(span[1], json!({"role": "assistant", "text": "reading it"}));
        assert_eq!(
            span[2],
            json!({
                "role": "assistant",
                "text": r#"{"path":"run.rs"}"#,
                "tool": "read",
                "tool-use-id": "c1",
                "args": r#"{"path":"run.rs"}"#,
            })
        );
        assert_eq!(
            span[4],
            json!({
                "role": "tool",
                "text": "fn run() {}",
                "tool": "read",
                "tool-use-id": "c1",
                "args": r#"{"path":"run.rs"}"#,
            })
        );
    }

    #[test]
    fn every_call_in_a_span_keeps_its_result_and_every_result_its_call() {
        let span = span(&conversation());
        let mut calls = ids(&span, "assistant");
        let mut results = ids(&span, "tool");
        calls.sort();
        results.sort();
        assert_eq!(calls, vec!["c1", "c2"]);
        assert_eq!(calls, results);
    }

    #[test]
    fn only_the_messages_offered_are_sent() {
        let messages = conversation();
        let offered = &messages[1..4];
        let ctx = compact_ctx(offered, &facts(), Some("s1"));
        let span = ctx["span"].as_array().unwrap();
        assert!(span.iter().all(|e| e["text"] != "fix the fold"));
        assert!(span.iter().all(|e| e["text"] != "done"));
        assert_eq!(span.len(), 5);
        assert_eq!(ctx["tokens"], json!(estimate_messages_tokens(offered)));
    }

    #[test]
    fn the_compact_ctx_carries_the_fold_facts() {
        let mut facts = facts();
        facts.focus = Some("the loop".into());
        let ctx = compact_ctx(&conversation(), &facts, Some("s1"));
        assert_eq!(ctx["reason"], "pressure");
        assert_eq!(ctx["focus"], "the loop");
        assert_eq!(ctx["ctx-max"], 120_000);
        assert_eq!(ctx["pressure"], 0.75);
        assert_eq!(ctx["session-id"], "s1");
        let bare = compact_ctx(&[], &CompactionFacts::default(), None);
        assert_eq!(bare["focus"], Value::Null);
        assert_eq!(bare["session-id"], Value::Null);
        assert_eq!(bare["pressure"], 0.0);
    }

    #[test]
    fn the_before_ctx_counts_the_fold() {
        assert_eq!(
            before_ctx(12, 90_000, &facts(), None),
            json!({
                "count": 12,
                "tokens": 90_000,
                "reason": "pressure",
                "ctx-max": 120_000,
                "pressure": 0.75,
                "session-id": null,
            })
        );
    }

    #[test]
    fn long_arguments_are_cut_to_a_summary() {
        let body = "x".repeat(1_000);
        let summary = args_summary(Some(&json!({ "content": body })));
        assert_eq!(summary.chars().count(), ARGS_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
        assert_eq!(args_summary(None), "");
    }

    #[test]
    fn a_summary_marker_rides_as_a_system_entry() {
        let span = span(&[json!({"role": "system", "content": "[summary]\n## Goal\nx"})]);
        assert_eq!(
            span,
            vec![json!({"role": "system", "text": "[summary]\n## Goal\nx"})]
        );
    }

    fn host_with(points: &[HookPoint], answers: Vec<Value>) -> Arc<AddonHost> {
        let rt = Arc::new(ScriptedRuntime {
            hook_answers: answers
                .into_iter()
                .map(|v| HookReply {
                    addon_id: "hd".into(),
                    result: Ok(v),
                })
                .collect(),
            ..Default::default()
        });
        Arc::new(AddonHost::new(
            rt,
            vec![summary("hd", &[], points)],
            Vec::new(),
        ))
    }

    #[test]
    fn no_hooks_when_no_addon_listens() {
        let deaf = host_with(&[HookPoint::OnPrompt], vec![json!({"summary": VALID})]);
        assert!(hooks(deaf, None, DEFAULT_BUDGET).is_none());
    }

    #[tokio::test]
    async fn a_valid_summary_is_the_answer_and_an_invalid_one_is_none() {
        let host = host_with(&[HookPoint::Compact], vec![json!({"summary": VALID})]);
        let hooks = hooks(host, Some("s1".into()), DEFAULT_BUDGET).expect("listened");
        assert_eq!(
            (hooks.on_compact)(conversation(), facts()).await.as_deref(),
            Some(VALID)
        );

        for answer in [
            json!({"summary": "no sections"}),
            Value::Null,
            json!("text"),
        ] {
            let host = host_with(&[HookPoint::Compact], vec![answer.clone()]);
            let hooks = super::hooks(host, None, DEFAULT_BUDGET).expect("listened");
            assert_eq!(
                (hooks.on_compact)(conversation(), facts()).await,
                None,
                "{answer}"
            );
        }
    }

    #[tokio::test]
    async fn before_compact_alone_answers_no_summary() {
        let host = host_with(&[HookPoint::BeforeCompact], vec![json!({"summary": VALID})]);
        let hooks = hooks(host, None, DEFAULT_BUDGET).expect("listened");
        (hooks.on_before)(3, 100, facts()).await;
        assert_eq!((hooks.on_compact)(conversation(), facts()).await, None);
    }
}
