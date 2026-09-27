//! Adapters from the addon host onto dirge's hook points: the agent loop's
//! before/after tool-call slots, and the main session's system prompt and
//! submitted prompt.
//!
//! Composition reuses the command-hooks combinators, so addon hooks chain
//! after Janet plugin and command hooks with one semantics: a block from an
//! earlier hook short-circuits, args flow forward, contexts concatenate.

use std::sync::Arc;

use serde_json::{Value, json};

use super::domain::{BeforeOutcome, HookPoint};
use super::host::AddonHost;
use super::policy;
use crate::agent::agent_loop::hooks::{
    AfterToolCallContext, AfterToolCallFn, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallReturn,
};
use crate::agent::agent_loop::result::{AfterToolCallResult, BeforeToolCallResult, LoopToolResult};
use crate::agent::agent_loop::types::LoopConfig;
use crate::agent::command_hooks::loop_hooks::{compose_after, compose_before};

/// `:dirge/before-tool-call`, adapted onto the loop's slot.
pub fn before_hook(host: Arc<AddonHost>) -> BeforeToolCallFn {
    Arc::new(move |ctx: BeforeToolCallContext| {
        let host = host.clone();
        Box::pin(async move {
            let payload = json!({
                "tool": ctx.tool_call_name,
                "args": ctx.args,
                "tool-call-id": ctx.tool_call_id,
            });
            let outcome = tokio::task::spawn_blocking(move || host.before_tool_call(&payload))
                .await
                .unwrap_or_default();
            before_return(ctx, outcome)
        })
    })
}

/// The loop's answer for a folded addon outcome.
fn before_return(ctx: BeforeToolCallContext, outcome: BeforeOutcome) -> BeforeToolCallReturn {
    BeforeToolCallReturn {
        result: outcome.block.map(|(addon, reason)| BeforeToolCallResult {
            block: Some(true),
            reason: Some(format!("blocked by addon {addon}: {reason}")),
        }),
        args: outcome.args.unwrap_or(ctx.args),
        context: outcome
            .context
            .iter()
            .map(|text| policy::reminder(HookPoint::BeforeToolCall, text))
            .collect(),
    }
}

/// `:dirge/after-tool-call`: addon context is appended to the result the
/// model sees.
pub fn after_hook(host: Arc<AddonHost>) -> AfterToolCallFn {
    Arc::new(move |ctx: AfterToolCallContext| {
        let host = host.clone();
        Box::pin(async move {
            let payload = json!({
                "tool": ctx.tool_call_name,
                "args": ctx.args,
                "result": policy::content_text(&ctx.result.content),
                "error?": ctx.is_error,
            });
            let texts =
                tokio::task::spawn_blocking(move || host.texts(HookPoint::AfterToolCall, &payload))
                    .await
                    .unwrap_or_default();
            after_override(&ctx.result, &texts)
        })
    })
}

fn after_override(result: &LoopToolResult, texts: &[String]) -> Option<AfterToolCallResult> {
    if texts.is_empty() {
        return None;
    }
    let notes: Vec<String> = texts
        .iter()
        .map(|t| policy::reminder(HookPoint::AfterToolCall, t))
        .collect();
    let mut content = result.content.clone();
    content.push(json!({ "type": "text", "text": notes.join("\n") }));
    Some(AfterToolCallResult {
        content: Some(content),
        ..AfterToolCallResult::default()
    })
}

/// Install the host's tool-call hooks on `config`, after whatever is there.
/// Points no addon listens on install nothing.
pub fn install(config: &mut LoopConfig, host: &Arc<AddonHost>) {
    if host.listens(HookPoint::BeforeToolCall) {
        config.before_tool_call = Some(compose_before(
            config.before_tool_call.take(),
            before_hook(host.clone()),
        ));
    }
    if host.listens(HookPoint::AfterToolCall) {
        config.after_tool_call = Some(compose_after(
            config.after_tool_call.take(),
            after_hook(host.clone()),
        ));
    }
}

/// `system_prompt` with `:dirge/system-prompt` contributions appended.
pub fn with_system_prompt(
    host: &AddonHost,
    system_prompt: String,
    session_id: Option<&str>,
) -> String {
    let ctx = json!({ "cwd": cwd(), "session-id": session_id });
    append(
        system_prompt,
        host.texts(HookPoint::SystemPrompt, &ctx),
        "\n\n",
    )
}

/// `prompt` with `:dirge/on-prompt` contributions prepended as reminders.
pub fn with_prompt_context(host: &AddonHost, prompt: String, session_id: Option<&str>) -> String {
    let ctx = json!({ "prompt": prompt, "session-id": session_id });
    let notes: Vec<String> = host
        .texts(HookPoint::OnPrompt, &ctx)
        .iter()
        .map(|t| policy::reminder(HookPoint::OnPrompt, t))
        .collect();
    if notes.is_empty() {
        prompt
    } else {
        format!("{}\n\n{prompt}", notes.join("\n"))
    }
}

fn append(base: String, texts: Vec<String>, sep: &str) -> String {
    if texts.is_empty() {
        base
    } else {
        format!("{base}{sep}{}", texts.join(sep))
    }
}

fn cwd() -> Value {
    std::env::current_dir()
        .map(|p| Value::String(p.display().to_string()))
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::domain::HookReply;
    use crate::addons::host::tests::{ScriptedRuntime, summary};
    use crate::agent::agent_loop::message::{AssistantMessage, StopReason};

    fn host_with(points: &[HookPoint], answers: Vec<HookReply>) -> Arc<AddonHost> {
        let rt = Arc::new(ScriptedRuntime {
            hook_answers: answers,
            ..Default::default()
        });
        Arc::new(AddonHost::new(
            rt,
            vec![summary("hd", &[], points)],
            Vec::new(),
        ))
    }

    fn reply(v: Value) -> HookReply {
        HookReply {
            addon_id: "hd".into(),
            result: Ok(v),
        }
    }

    fn ctx() -> BeforeToolCallContext {
        BeforeToolCallContext {
            assistant_message: AssistantMessage::new(Vec::new(), StopReason::ToolUse),
            tool_call_name: "bash".into(),
            tool_call_id: "t1".into(),
            args: json!({"command": "ls"}),
        }
    }

    #[test]
    fn a_block_becomes_a_refusal_naming_the_addon() {
        let out = before_return(
            ctx(),
            BeforeOutcome {
                block: Some(("hd".into(), "no rm".into())),
                ..BeforeOutcome::default()
            },
        );
        let result = out.result.expect("blocked");
        assert_eq!(result.block, Some(true));
        assert_eq!(result.reason.as_deref(), Some("blocked by addon hd: no rm"));
        assert_eq!(out.args, json!({"command": "ls"}));
    }

    #[test]
    fn replacement_args_and_context_flow_through() {
        let out = before_return(
            ctx(),
            BeforeOutcome {
                args: Some(json!({"command": "ls -la"})),
                context: vec!["mind the cwd".into()],
                ..BeforeOutcome::default()
            },
        );
        assert!(out.result.is_none());
        assert_eq!(out.args, json!({"command": "ls -la"}));
        assert!(out.context[0].contains("mind the cwd"));
    }

    #[test]
    fn after_context_is_appended_not_replacing() {
        let result = LoopToolResult {
            content: vec![json!({"type": "text", "text": "out"})],
            details: Value::Null,
            terminate: None,
        };
        assert!(after_override(&result, &[]).is_none());
        let over = after_override(&result, &["noted".into()]).unwrap();
        let content = over.content.unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0], json!({"type": "text", "text": "out"}));
        assert!(content[1]["text"].as_str().unwrap().contains("noted"));
    }

    #[test]
    fn system_prompt_gains_addon_text_only_when_listened() {
        let host = host_with(
            &[HookPoint::SystemPrompt],
            vec![reply(json!("hive is here"))],
        );
        assert_eq!(
            with_system_prompt(&host, "base".into(), None),
            "base\n\nhive is here"
        );
        let deaf = host_with(&[], vec![reply(json!("never"))]);
        assert_eq!(with_system_prompt(&deaf, "base".into(), None), "base");
    }

    #[test]
    fn prompt_context_is_prepended_as_a_reminder() {
        let host = host_with(
            &[HookPoint::OnPrompt],
            vec![reply(json!({"context": "3 lings running"}))],
        );
        let out = with_prompt_context(&host, "do it".into(), Some("s1"));
        assert!(out.starts_with("<system-reminder>"));
        assert!(out.contains("3 lings running"));
        assert!(out.ends_with("\n\ndo it"));
    }
}
