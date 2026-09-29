//! Adapters from the addon host onto the agent loop's turn hooks, reached
//! through open hook keys (no [`HookPoint`](super::domain::HookPoint) names
//! them):
//!
//! - `:dirge/transform-context` may rewrite the messages one model call
//!   sees; the saved conversation is untouched.
//! - `:dirge/prepare-next-turn` may set the next turn's thinking level and
//!   add a note to its context.
//! - `:dirge/should-stop-after-turn` may end the run after the turn.
//!
//! Each runs on a blocking thread of the agent runtime, where addon code
//! may reach MCP, within a budget. No answer in time, a failed hook, or an
//! answer of the wrong shape is no answer, and the loop goes on as it would
//! without them.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::host::AddonHost;
use super::policy;
use crate::agent::agent_loop::hooks::{
    PrepareNextTurnFn, ShouldStopAfterTurnFn, TurnHookContext, compose_prepare_next_turn,
    compose_should_stop_after_turn,
};
use crate::agent::agent_loop::message::{
    ContentBlock, LoopMessage, UserMessage, loop_message_to_value,
};
use crate::agent::agent_loop::types::{
    Context, LoopConfig, ThinkingLevel, TransformContextFn, TurnUpdate, compose_transform_context,
};
use crate::agent::compression::estimate_messages_tokens;
use crate::runtime::blocking_within;

pub const TRANSFORM_CONTEXT: &str = "dirge/transform-context";
pub const PREPARE_NEXT_TURN: &str = "dirge/prepare-next-turn";
pub const SHOULD_STOP_AFTER_TURN: &str = "dirge/should-stop-after-turn";

/// How long one turn hook may take before the loop goes on without it.
pub const BUDGET: Duration = Duration::from_secs(10);

/// Install the host's turn hooks on `config`, each after the one already
/// there. Keys no addon listens on install nothing.
pub fn install(config: &mut LoopConfig, host: &Arc<AddonHost>, budget: Duration) {
    let session_id = config.session_id.clone();
    if host.listens_key(TRANSFORM_CONTEXT) {
        config.transform_context = compose_transform_context(
            config.transform_context.take(),
            Some(transform_context(host.clone(), session_id.clone(), budget)),
        );
    }
    if host.listens_key(PREPARE_NEXT_TURN) {
        config.prepare_next_turn = compose_prepare_next_turn(
            config.prepare_next_turn.take(),
            Some(prepare_next_turn(host.clone(), session_id.clone(), budget)),
        );
    }
    if host.listens_key(SHOULD_STOP_AFTER_TURN) {
        config.should_stop_after_turn = compose_should_stop_after_turn(
            config.should_stop_after_turn.take(),
            Some(should_stop_after_turn(host.clone(), session_id, budget)),
        );
    }
}

/// `:dirge/transform-context`: the first well-formed `{:messages [...]}`
/// answer replaces the messages of this one model call.
pub fn transform_context(
    host: Arc<AddonHost>,
    session_id: Option<String>,
    budget: Duration,
) -> TransformContextFn {
    Arc::new(move |messages: Vec<Value>| {
        let (host, session_id) = (host.clone(), session_id.clone());
        Box::pin(async move {
            let ctx = transform_ctx(&messages, session_id.as_deref());
            let answered = blocking_within(budget, move || {
                policy::messages(&host.emit(TRANSFORM_CONTEXT, &ctx))
            })
            .await;
            match answered {
                Ok(Some(replaced)) => replaced,
                Ok(None) => messages,
                Err(why) => {
                    tracing::warn!(target: "dirge::addon", %why, "addon transform-context skipped");
                    messages
                }
            }
        })
    })
}

/// `:dirge/prepare-next-turn`: a `{:thinking level}` answer sets the next
/// turn's thinking level, and text answers ride into the next turn's
/// context as a reminder.
pub fn prepare_next_turn(
    host: Arc<AddonHost>,
    session_id: Option<String>,
    budget: Duration,
) -> PrepareNextTurnFn {
    Arc::new(move |turn: TurnHookContext| {
        let (host, session_id) = (host.clone(), session_id.clone());
        Box::pin(async move {
            let ctx = turn_ctx(&turn, session_id.as_deref());
            let answered = blocking_within(budget, move || {
                let replies = host.emit(PREPARE_NEXT_TURN, &ctx);
                let thinking =
                    policy::thinking(&replies, |s| ThinkingLevel::from_effort_str(s).is_some());
                (thinking, policy::texts(&replies))
            })
            .await;
            match answered {
                Ok((thinking, notes)) => turn_update(turn.context, thinking, &notes),
                Err(why) => {
                    tracing::warn!(target: "dirge::addon", %why, "addon prepare-next-turn skipped");
                    None
                }
            }
        })
    })
}

/// `:dirge/should-stop-after-turn`: `true`, `{:stop true}` or `{:stop
/// "reason"}` from any addon ends the run after this turn.
pub fn should_stop_after_turn(
    host: Arc<AddonHost>,
    session_id: Option<String>,
    budget: Duration,
) -> ShouldStopAfterTurnFn {
    Arc::new(move |turn: TurnHookContext| {
        let (host, session_id) = (host.clone(), session_id.clone());
        Box::pin(async move {
            let ctx = turn_ctx(&turn, session_id.as_deref());
            let answered = blocking_within(budget, move || {
                policy::stop(&host.emit(SHOULD_STOP_AFTER_TURN, &ctx))
            })
            .await;
            match answered {
                Ok(Some((addon, reason))) => {
                    tracing::info!(
                        target: "dirge::addon",
                        %addon,
                        reason = reason.as_deref().unwrap_or(""),
                        "addon ended the run after this turn"
                    );
                    true
                }
                Ok(None) => false,
                Err(why) => {
                    tracing::warn!(target: "dirge::addon", %why, "addon should-stop-after-turn skipped");
                    false
                }
            }
        })
    })
}

/// `:dirge/transform-context`'s ctx for a call about to send `messages`.
pub fn transform_ctx(messages: &[Value], session_id: Option<&str>) -> Value {
    json!({
        "messages": messages,
        "tokens": estimate_messages_tokens(messages),
        "session-id": session_id,
    })
}

/// The ctx `:dirge/prepare-next-turn` and `:dirge/should-stop-after-turn`
/// get for the turn that just ended.
pub fn turn_ctx(turn: &TurnHookContext, session_id: Option<&str>) -> Value {
    let results: Vec<Value> = turn
        .tool_results
        .iter()
        .map(|r| {
            json!({
                "tool": r.tool_name,
                "tool-use-id": r.tool_call_id,
                "text": blocks_text(&r.content),
                "error?": r.is_error,
            })
        })
        .collect();
    json!({
        "text": turn.message.text_joined(),
        "stop-reason": turn.message.stop_reason,
        "tool-results": results,
        "messages": turn.context.messages.len(),
        "tokens": estimate_messages_tokens(&turn.context.messages),
        "session-id": session_id,
    })
}

fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The loop's update for a `thinking` level and `notes`; `None` when there
/// is neither.
fn turn_update(
    mut context: Context,
    thinking: Option<String>,
    notes: &[String],
) -> Option<TurnUpdate> {
    let thinking_level = thinking.as_deref().and_then(ThinkingLevel::from_effort_str);
    let context = (!notes.is_empty()).then(|| {
        let reminders: Vec<String> = notes
            .iter()
            .map(|t| policy::key_reminder(PREPARE_NEXT_TURN, t))
            .collect();
        let note = LoopMessage::User(UserMessage::text(reminders.join("\n")));
        context.messages.push(loop_message_to_value(&note));
        context
    });
    if thinking_level.is_none() && context.is_none() {
        return None;
    }
    Some(TurnUpdate {
        context,
        model: None,
        thinking_level,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::*;
    use crate::addons::domain::{HookPoint, HookReply};
    use crate::addons::host::LoadSet;
    use crate::addons::port::AddonRuntime;
    use crate::agent::agent_loop::message::{AssistantMessage, StopReason, ToolResultMessage};

    /// One addon, `hd`, that registers the keys it answers and records every
    /// ctx it is handed.
    #[derive(Default)]
    struct KeyedRuntime {
        answers: HashMap<&'static str, Value>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl AddonRuntime for KeyedRuntime {
        fn load(&self, _manifest: &Path, _host_config: &Value) -> Value {
            json!({"id": "hd", "hooks": self.answers.keys().collect::<Vec<_>>()})
        }
        fn unload(&self, _addon_id: &str) {}
        fn reload_sources(&self, _files: &[PathBuf]) -> Vec<(PathBuf, String)> {
            Vec::new()
        }
        fn set_source_roots(&self, _roots: &[PathBuf]) {}
        fn call_tool(&self, _: &str, _: &str, _: &Value) -> Result<Value, String> {
            Err("no tools".into())
        }
        fn run_command(&self, _: &str, _: &str, _: &Value) -> Result<Value, String> {
            Err("no commands".into())
        }
        fn run_hook(&self, point: HookPoint, ctx: &Value) -> Vec<HookReply> {
            self.run_hook_key(point.key(), ctx)
        }
        fn run_hook_key(&self, key: &str, ctx: &Value) -> Vec<HookReply> {
            self.calls.lock().unwrap().push((key.into(), ctx.clone()));
            self.answers
                .get(key)
                .map(|v| {
                    vec![HookReply {
                        addon_id: "hd".into(),
                        result: Ok(v.clone()),
                    }]
                })
                .unwrap_or_default()
        }
        fn shutdown(&self) {}
    }

    fn host_answering(answers: &[(&'static str, Value)]) -> (Arc<AddonHost>, Arc<KeyedRuntime>) {
        let rt = Arc::new(KeyedRuntime {
            answers: answers.iter().cloned().collect(),
            ..Default::default()
        });
        let set = LoadSet {
            manifests: vec![PathBuf::from("hd.edn")],
            failures: Vec::new(),
            source_roots: Vec::new(),
            sources: Vec::new(),
        };
        let host = AddonHost::load(rt.clone(), set, json!({}));
        (Arc::new(host), rt)
    }

    fn only_ctx(rt: &KeyedRuntime, key: &str) -> Value {
        let calls = rt.calls.lock().unwrap();
        let heard: Vec<&Value> = calls
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, c)| c)
            .collect();
        assert_eq!(heard.len(), 1, "{key} heard {heard:?}");
        heard[0].clone()
    }

    fn msgs() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "fix it"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "on it"}]}),
        ]
    }

    fn turn() -> TurnHookContext {
        TurnHookContext {
            message: AssistantMessage::new(
                vec![ContentBlock::Text {
                    text: "read it".into(),
                }],
                StopReason::ToolUse,
            ),
            tool_results: vec![ToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: "read".into(),
                content: vec![ContentBlock::Text {
                    text: "fn a() {}".into(),
                }],
                details: Value::Null,
                is_error: false,
            }],
            context: Context {
                messages: msgs(),
                ..Default::default()
            },
            new_messages: Vec::new(),
        }
    }

    fn config() -> LoopConfig {
        let mut config = LoopConfig::for_tests(Arc::new(|m: &[Value]| m.to_vec()));
        config.session_id = Some("s1".into());
        config
    }

    #[test]
    fn keys_no_addon_listens_on_install_nothing() {
        let (host, _) = host_answering(&[("acme/other", json!(true))]);
        let mut config = config();
        install(&mut config, &host, BUDGET);
        assert!(config.transform_context.is_none());
        assert!(config.prepare_next_turn.is_none());
        assert!(config.should_stop_after_turn.is_none());
    }

    #[tokio::test]
    async fn transform_context_reaches_the_addon_and_its_messages_replace_the_call() {
        let only = json!([{"role": "user", "content": "just this"}]);
        let (host, rt) = host_answering(&[(TRANSFORM_CONTEXT, json!({ "messages": only }))]);
        let mut config = config();
        install(&mut config, &host, BUDGET);
        let transform = config.transform_context.expect("installed");

        assert_eq!(Value::Array(transform(msgs()).await), only);
        let ctx = only_ctx(&rt, TRANSFORM_CONTEXT);
        assert_eq!(ctx["messages"], Value::Array(msgs()));
        assert_eq!(ctx["tokens"], json!(estimate_messages_tokens(&msgs())));
        assert_eq!(ctx["session-id"], "s1");
    }

    #[tokio::test]
    async fn a_malformed_transform_leaves_the_messages_alone() {
        for answer in [
            Value::Null,
            json!({"messages": []}),
            json!({"messages": [{"content": "no role"}]}),
            json!({"messages": "text"}),
            json!("text"),
        ] {
            let (host, _) = host_answering(&[(TRANSFORM_CONTEXT, answer.clone())]);
            let transform = transform_context(host, None, BUDGET);
            assert_eq!(transform(msgs()).await, msgs(), "{answer}");
        }
    }

    #[tokio::test]
    async fn prepare_next_turn_sets_the_thinking_level_and_adds_a_note() {
        let (host, rt) = host_answering(&[(
            PREPARE_NEXT_TURN,
            json!({"thinking": "high", "context": "3 lings running"}),
        )]);
        let mut config = config();
        install(&mut config, &host, BUDGET);
        let prepare = config.prepare_next_turn.expect("installed");

        let update = prepare(turn()).await.expect("an update");
        assert_eq!(update.thinking_level, Some(ThinkingLevel::High));
        assert!(update.model.is_none());
        let messages = update.context.expect("a note").messages;
        assert_eq!(messages.len(), 3);
        assert_eq!(&messages[..2], &msgs()[..]);
        let note = messages[2].to_string();
        assert!(
            note.contains("dirge/prepare-next-turn addon context"),
            "{note}"
        );
        assert!(note.contains("3 lings running"), "{note}");

        let ctx = only_ctx(&rt, PREPARE_NEXT_TURN);
        assert_eq!(ctx["text"], "read it");
        assert_eq!(ctx["stop-reason"], "toolUse");
        assert_eq!(
            ctx["tool-results"],
            json!([{"tool": "read", "tool-use-id": "c1", "text": "fn a() {}", "error?": false}])
        );
        assert_eq!(ctx["messages"], 2);
        assert_eq!(ctx["session-id"], "s1");
    }

    #[tokio::test]
    async fn an_unknown_level_or_no_answer_changes_nothing() {
        for answer in [Value::Null, json!({"thinking": "loud"}), json!({})] {
            let (host, _) = host_answering(&[(PREPARE_NEXT_TURN, answer.clone())]);
            let prepare = prepare_next_turn(host, None, BUDGET);
            assert!(prepare(turn()).await.is_none(), "{answer}");
        }
        let (host, _) = host_answering(&[(PREPARE_NEXT_TURN, json!({"thinking": "low"}))]);
        let update = prepare_next_turn(host, None, BUDGET)(turn()).await.unwrap();
        assert_eq!(update.thinking_level, Some(ThinkingLevel::Low));
        assert!(update.context.is_none(), "no note, the context stays");
    }

    #[tokio::test]
    async fn should_stop_after_turn_reaches_the_addon_and_its_verdict_stops() {
        for (answer, stops) in [
            (json!(true), true),
            (json!({"stop": true}), true),
            (json!({"stop": "goal met"}), true),
            (json!({"stop": false}), false),
            (json!({"stop": ""}), false),
            (json!(false), false),
            (Value::Null, false),
        ] {
            let (host, rt) = host_answering(&[(SHOULD_STOP_AFTER_TURN, answer.clone())]);
            let mut config = config();
            install(&mut config, &host, BUDGET);
            let stop = config.should_stop_after_turn.expect("installed");
            assert_eq!(stop(turn()).await, stops, "{answer}");
            assert_eq!(only_ctx(&rt, SHOULD_STOP_AFTER_TURN)["text"], "read it");
        }
    }

    #[tokio::test]
    async fn addon_hooks_run_after_the_ones_already_installed() {
        let (host, _) = host_answering(&[
            (
                TRANSFORM_CONTEXT,
                json!({"messages": [{"role": "user", "content": "addon"}]}),
            ),
            (SHOULD_STOP_AFTER_TURN, json!(false)),
        ]);
        let mut config = config();
        let seen = Arc::new(Mutex::new(0usize));
        let counted = seen.clone();
        config.transform_context = Some(Arc::new(move |messages: Vec<Value>| {
            *counted.lock().unwrap() = messages.len();
            Box::pin(async move { messages })
        }));
        config.should_stop_after_turn = Some(Arc::new(|_| Box::pin(async { true })));
        install(&mut config, &host, BUDGET);

        let out = (config.transform_context.unwrap())(msgs()).await;
        assert_eq!(
            *seen.lock().unwrap(),
            2,
            "the earlier transform saw the call first"
        );
        assert_eq!(out, vec![json!({"role": "user", "content": "addon"})]);
        assert!((config.should_stop_after_turn.unwrap())(turn()).await);
    }
}
