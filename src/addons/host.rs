//! The loaded addons and the runtime that runs them.

use std::sync::Arc;

use serde_json::Value;

use super::domain::{AddonSummary, BeforeOutcome, HookPoint, LoadFailure, ToolSpec};
use super::policy;
use super::port::AddonRuntime;

pub struct AddonHost {
    runtime: Arc<dyn AddonRuntime>,
    addons: Vec<AddonSummary>,
    failures: Vec<LoadFailure>,
    tools: Vec<ToolSpec>,
}

impl std::fmt::Debug for AddonHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddonHost")
            .field("addons", &self.addons)
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

impl AddonHost {
    pub fn new(
        runtime: Arc<dyn AddonRuntime>,
        addons: Vec<AddonSummary>,
        failures: Vec<LoadFailure>,
    ) -> Self {
        let (tools, dropped) = policy::unique_tools(&addons);
        for (addon, tool) in dropped {
            tracing::warn!(
                target: "dirge::addon",
                %addon, %tool,
                "addon tool dropped: an earlier addon already exposes that name"
            );
        }
        Self {
            runtime,
            addons,
            failures,
            tools,
        }
    }

    pub fn addons(&self) -> &[AddonSummary] {
        &self.addons
    }

    pub fn failures(&self) -> &[LoadFailure] {
        &self.failures
    }

    /// The tools offered to the model, collisions already resolved.
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// True when at least one addon contributes `point`, so callers can skip
    /// the interpreter round trip on the hot path.
    pub fn listens(&self, point: HookPoint) -> bool {
        self.addons.iter().any(|a| a.hooks.contains(&point))
    }

    /// Run a tool: the runtime call, then the handler's return value read as
    /// `(content blocks, details)`.
    pub fn call_tool(&self, tool: &ToolSpec, args: &Value) -> Result<(Vec<Value>, Value), String> {
        self.runtime
            .call_tool(&tool.addon_id, &tool.name, args)
            .and_then(|out| policy::tool_output(&out))
    }

    /// Texts every addon answered `point` with.
    pub fn texts(&self, point: HookPoint, ctx: &Value) -> Vec<String> {
        if !self.listens(point) {
            return Vec::new();
        }
        let replies = self.runtime.run_hook(point, ctx);
        log_failures(point, &replies);
        policy::texts(&replies)
    }

    /// The folded `BeforeToolCall` answer.
    pub fn before_tool_call(&self, ctx: &Value) -> BeforeOutcome {
        if !self.listens(HookPoint::BeforeToolCall) {
            return BeforeOutcome::default();
        }
        let replies = self.runtime.run_hook(HookPoint::BeforeToolCall, ctx);
        log_failures(HookPoint::BeforeToolCall, &replies);
        policy::fold_before(&replies)
    }

    pub fn shutdown(&self) {
        self.runtime.shutdown();
    }
}

/// Logs failed hook replies.
fn log_failures(point: HookPoint, replies: &[super::domain::HookReply]) {
    for reply in replies {
        if let Err(error) = &reply.result {
            tracing::warn!(
                target: "dirge::addon",
                addon = %reply.addon_id,
                hook = point.key(),
                %error,
                "addon hook failed; ignored"
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::addons::domain::HookReply;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Scripted runtime: answers from fixed tables and records every call.
    #[derive(Default)]
    pub(crate) struct ScriptedRuntime {
        pub tool_answer: Option<Result<Value, String>>,
        pub hook_answers: Vec<HookReply>,
        pub calls: Mutex<Vec<String>>,
    }

    impl AddonRuntime for ScriptedRuntime {
        fn call_tool(&self, addon_id: &str, tool: &str, args: &Value) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("tool {addon_id}/{tool} {args}"));
            self.tool_answer.clone().unwrap_or(Ok(Value::Null))
        }

        fn run_hook(&self, point: HookPoint, ctx: &Value) -> Vec<HookReply> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("hook {} {ctx}", point.key()));
            self.hook_answers.clone()
        }

        fn shutdown(&self) {
            self.calls.lock().unwrap().push("shutdown".into());
        }
    }

    pub(crate) fn summary(id: &str, tools: &[&str], hooks: &[HookPoint]) -> AddonSummary {
        AddonSummary {
            id: id.into(),
            manifest: PathBuf::from(format!("{id}.edn")),
            tools: tools
                .iter()
                .map(|t| ToolSpec {
                    addon_id: id.into(),
                    name: t.to_string(),
                    exposed_name: policy::exposed_tool_name(t),
                    description: String::new(),
                    input_schema: json!({"type": "object"}),
                })
                .collect(),
            hooks: hooks.to_vec(),
            health: Value::Null,
        }
    }

    fn host(
        runtime: ScriptedRuntime,
        addons: Vec<AddonSummary>,
    ) -> (AddonHost, Arc<ScriptedRuntime>) {
        let rt = Arc::new(runtime);
        (AddonHost::new(rt.clone(), addons, Vec::new()), rt)
    }

    #[test]
    fn unhooked_points_never_reach_the_runtime() {
        let (host, rt) = host(
            ScriptedRuntime::default(),
            vec![summary("a", &[], &[HookPoint::OnPrompt])],
        );
        assert!(host.texts(HookPoint::SystemPrompt, &json!({})).is_empty());
        assert_eq!(host.before_tool_call(&json!({})), BeforeOutcome::default());
        assert!(rt.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn hooked_points_fold_replies_and_drop_failures() {
        let replies = vec![
            HookReply {
                addon_id: "a".into(),
                result: Err("boom".into()),
            },
            HookReply {
                addon_id: "b".into(),
                result: Ok(json!("from b")),
            },
        ];
        let (host, rt) = host(
            ScriptedRuntime {
                hook_answers: replies,
                ..Default::default()
            },
            vec![summary("a", &[], &[HookPoint::SystemPrompt])],
        );
        assert_eq!(
            host.texts(HookPoint::SystemPrompt, &json!({"cwd": "/w"})),
            vec!["from b".to_string()]
        );
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![r#"hook dirge/system-prompt {"cwd":"/w"}"#.to_string()]
        );
    }

    #[test]
    fn tool_calls_run_through_the_runtime_then_the_output_policy() {
        let (host, rt) = host(
            ScriptedRuntime {
                tool_answer: Some(Ok(json!({"content": [{"type": "text", "text": "rows=3"}]}))),
                ..Default::default()
            },
            vec![summary("hd", &["swarm-view"], &[])],
        );
        let tool = host.tools()[0].clone();
        let (content, _) = host.call_tool(&tool, &json!({"rows": [1, 2, 3]})).unwrap();
        assert_eq!(content[0]["text"], "rows=3");
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![r#"tool hd/swarm-view {"rows":[1,2,3]}"#.to_string()]
        );
    }

    #[test]
    fn runtime_errors_stay_on_the_failure_track() {
        let (host, _) = host(
            ScriptedRuntime {
                tool_answer: Some(Err("handler threw".into())),
                ..Default::default()
            },
            vec![summary("hd", &["t"], &[])],
        );
        let tool = host.tools()[0].clone();
        assert_eq!(
            host.call_tool(&tool, &json!({})).unwrap_err(),
            "handler threw"
        );
    }
}
