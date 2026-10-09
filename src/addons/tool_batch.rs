//! `:dirge/after-tool-batch`, an open hook key (no
//! [`HookPoint`](super::domain::HookPoint) names it): heard once per tool
//! batch, after every call of the turn has answered and before the next
//! model call. The counterpart of Claude Code's PostToolBatch.
//!
//! `ctx` = `{:calls [{:tool :args :tool-use-id :error?}] :session-id}`.
//! Text answers (`"text"` or `{:context text}`) ride into the next turn as
//! one reminder; each is spilled to an ARC handle past
//! [`spill::CAP`](super::spill::CAP).
//!
//! Reached through [`AddonHost::emit`] on the loop's prepare-next-turn
//! slot, composed after whatever is there, within a budget.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::host::AddonHost;
use super::policy;
use super::spill::{self, SpillStore};
use crate::agent::agent_loop::hooks::{PrepareNextTurnFn, TurnHookContext, compose_prepare_next_turn};
use crate::agent::agent_loop::message::{ContentBlock, LoopMessage, UserMessage, loop_message_to_value};
use crate::agent::agent_loop::types::{LoopConfig, TurnUpdate};
use crate::runtime::blocking_within;

pub const AFTER_TOOL_BATCH: &str = "dirge/after-tool-batch";

/// The batch's ctx for the turn that just ended; `None` when the turn ran
/// no tool, so a text-only turn is never heard as a batch.
pub fn batch_ctx(turn: &TurnHookContext, session_id: Option<&str>) -> Option<Value> {
    if turn.tool_results.is_empty() {
        return None;
    }
    let args_of = |id: &str| {
        turn.message
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolCall { id: call, arguments, .. } if call == id => {
                    Some(arguments.clone())
                }
                _ => None,
            })
            .unwrap_or(Value::Null)
    };
    let calls: Vec<Value> = turn
        .tool_results
        .iter()
        .map(|r| {
            json!({
                "tool": r.tool_name,
                "args": args_of(&r.tool_call_id),
                "tool-use-id": r.tool_call_id,
                "error?": r.is_error,
            })
        })
        .collect();
    Some(json!({ "calls": calls, "session-id": session_id }))
}

/// The prepare-next-turn step that emits `:dirge/after-tool-batch` and
/// adds the answers, spilled through `store`, as one note.
pub fn after_tool_batch(
    host: Arc<AddonHost>,
    session_id: Option<String>,
    budget: Duration,
    store: Arc<dyn SpillStore>,
) -> PrepareNextTurnFn {
    Arc::new(move |turn: TurnHookContext| {
        let (host, session_id, store) = (host.clone(), session_id.clone(), store.clone());
        Box::pin(async move {
            let ctx = batch_ctx(&turn, session_id.as_deref())?;
            let answered = blocking_within(budget, move || {
                policy::texts(&host.emit(AFTER_TOOL_BATCH, &ctx))
                    .iter()
                    .map(|t| spill::spill(t, spill::CAP, session_id.as_deref(), store.as_ref()))
                    .collect::<Vec<_>>()
            })
            .await;
            match answered {
                Ok(notes) if !notes.is_empty() => Some(note_update(turn, &notes)),
                Ok(_) => None,
                Err(why) => {
                    tracing::warn!(target: "dirge::addon", %why, "addon after-tool-batch skipped");
                    None
                }
            }
        })
    })
}

fn note_update(turn: TurnHookContext, notes: &[String]) -> TurnUpdate {
    let mut context = turn.context;
    let reminders: Vec<String> = notes
        .iter()
        .map(|t| policy::key_reminder(AFTER_TOOL_BATCH, t))
        .collect();
    let note = LoopMessage::User(UserMessage::text(reminders.join("\n")));
    context.messages.push(loop_message_to_value(&note));
    TurnUpdate {
        context: Some(context),
        model: None,
        thinking_level: None,
    }
}

/// Install `:dirge/after-tool-batch` on `config` after whatever is there,
/// when an addon listens on it.
pub fn install(
    config: &mut LoopConfig,
    host: &Arc<AddonHost>,
    budget: Duration,
    store: Arc<dyn SpillStore>,
) {
    if host.listens_key(AFTER_TOOL_BATCH) {
        let session_id = config.session_id.clone();
        config.prepare_next_turn = compose_prepare_next_turn(
            config.prepare_next_turn.take(),
            Some(after_tool_batch(host.clone(), session_id, budget, store)),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::*;
    use crate::addons::domain::{HookPoint, HookReply};
    use crate::addons::host::tests::summary;
    use crate::addons::port::AddonRuntime;
    use crate::addons::spill::tests::MemorySpillStore;
    use crate::agent::agent_loop::message::{AssistantMessage, StopReason, ToolResultMessage};
    use crate::agent::agent_loop::types::Context;

    /// Answers every key with `answer` and records the keys and ctxs heard.
    struct Recording {
        answer: Value,
        heard: Mutex<Vec<(String, Value)>>,
    }

    impl AddonRuntime for Recording {
        fn load(&self, _: &Path, _: &Value) -> Value {
            json!({"error": "unscripted"})
        }
        fn unload(&self, _: &str) {}
        fn reload_sources(&self, _: &[PathBuf]) -> Vec<(PathBuf, String)> {
            Vec::new()
        }
        fn set_source_roots(&self, _: &[PathBuf]) {}
        fn call_tool(&self, _: &str, _: &str, _: &Value) -> Result<Value, String> {
            Err("no tools".into())
        }
        fn run_command(&self, _: &str, _: &str, _: &Value) -> Result<Value, String> {
            Err("no commands".into())
        }
        fn run_hook(&self, point: HookPoint, _: &Value) -> Vec<HookReply> {
            panic!("closed dispatch path reached for {}", point.key())
        }
        fn run_hook_key(&self, key: &str, ctx: &Value) -> Vec<HookReply> {
            self.heard.lock().unwrap().push((key.into(), ctx.clone()));
            vec![HookReply {
                addon_id: "b".into(),
                result: Ok(self.answer.clone()),
            }]
        }
        fn shutdown(&self) {}
    }

    fn host(keys: &[&str], answer: Value) -> (Arc<AddonHost>, Arc<Recording>) {
        let rt = Arc::new(Recording {
            answer,
            heard: Mutex::new(Vec::new()),
        });
        let host = AddonHost::with_reports(
            rt.clone(),
            vec![summary("b", &[], &[])],
            &[("b".to_string(), json!({ "hooks": keys }))],
        );
        (Arc::new(host), rt)
    }

    fn result(id: &str, tool: &str, error: bool) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: tool.into(),
            content: vec![ContentBlock::Text { text: "ok".into() }],
            details: Value::Null,
            is_error: error,
        }
    }

    fn turn(results: Vec<ToolResultMessage>) -> TurnHookContext {
        TurnHookContext {
            message: AssistantMessage::new(
                vec![
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: json!({"path": "a.rs"}),
                        signature: None,
                        signature_model: None,
                    },
                    ContentBlock::ToolCall {
                        id: "c2".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "ls"}),
                        signature: None,
                        signature_model: None,
                    },
                ],
                StopReason::ToolUse,
            ),
            tool_results: results,
            context: Context::default(),
            new_messages: Vec::new(),
        }
    }

    fn config() -> LoopConfig {
        let mut config = LoopConfig::for_tests(Arc::new(|m: &[Value]| m.to_vec()));
        config.session_id = Some("s1".into());
        config
    }

    const BUDGET: Duration = Duration::from_secs(5);

    #[test]
    fn nothing_installs_when_no_addon_listens() {
        let (host, _) = host(&["acme/other"], json!("x"));
        let mut config = config();
        install(&mut config, &host, BUDGET, Arc::new(MemorySpillStore::default()));
        assert!(config.prepare_next_turn.is_none());
    }

    #[tokio::test]
    async fn one_batch_is_heard_once_with_every_call_and_its_answer_becomes_one_note() {
        let (host, rt) = host(&[AFTER_TOOL_BATCH], json!({"context": "2 calls, 1 failed"}));
        let mut config = config();
        install(&mut config, &host, BUDGET, Arc::new(MemorySpillStore::default()));
        let prepare = config.prepare_next_turn.expect("installed");

        let update = prepare(turn(vec![result("c1", "read", false), result("c2", "bash", true)]))
            .await
            .expect("a note");

        let heard = rt.heard.lock().unwrap().clone();
        assert_eq!(heard.len(), 1, "once per batch");
        assert_eq!(heard[0].0, AFTER_TOOL_BATCH);
        assert_eq!(
            heard[0].1,
            json!({
                "calls": [
                    {"tool": "read", "args": {"path": "a.rs"}, "tool-use-id": "c1", "error?": false},
                    {"tool": "bash", "args": {"command": "ls"}, "tool-use-id": "c2", "error?": true}
                ],
                "session-id": "s1"
            })
        );
        let messages = update.context.expect("context").messages;
        assert_eq!(messages.len(), 1);
        let note = messages[0].to_string();
        assert!(note.contains("2 calls, 1 failed"), "{note}");
        assert!(note.contains(AFTER_TOOL_BATCH), "{note}");
    }

    #[tokio::test]
    async fn a_turn_without_tools_is_not_a_batch() {
        let (host, rt) = host(&[AFTER_TOOL_BATCH], json!("x"));
        let prepare = after_tool_batch(host, None, BUDGET, Arc::new(MemorySpillStore::default()));
        assert!(prepare(turn(Vec::new())).await.is_none());
        assert!(rt.heard.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_oversized_answer_spills_to_a_handle() {
        let big = "z".repeat(spill::CAP + 1);
        let (host, _) = host(&[AFTER_TOOL_BATCH], json!(big.clone()));
        let store = Arc::new(MemorySpillStore::default());
        let prepare = after_tool_batch(host, Some("s1".into()), BUDGET, store.clone());

        let update = prepare(turn(vec![result("c1", "read", false)])).await.unwrap();

        let kept = store.kept.lock().unwrap().clone();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].2, big);
        let note = update.context.unwrap().messages[0].to_string();
        assert!(note.contains(&kept[0].1), "the note names the handle");
        assert!(note.chars().count() < spill::CAP);
    }
}
