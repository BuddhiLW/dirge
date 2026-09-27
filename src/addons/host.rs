//! The loaded addons and the runtime that runs them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::domain::{
    AddonSummary, BeforeOutcome, CommandOutput, CommandSpec, HookPoint, LoadFailure, ReloadReport,
    ToolSpec,
};
use super::policy;
use super::port::AddonRuntime;
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// What a load works from: the manifests that validated, the ones that did
/// not, the classpath, and the addons' own source files, which a reload
/// evaluates again.
#[derive(Debug, Clone, Default)]
pub struct LoadSet {
    pub manifests: Vec<PathBuf>,
    pub failures: Vec<LoadFailure>,
    pub source_roots: Vec<PathBuf>,
    pub sources: Vec<PathBuf>,
}

/// The addons one load produced, collisions resolved.
#[derive(Debug, Clone, Default)]
struct Loaded {
    addons: Vec<AddonSummary>,
    failures: Vec<LoadFailure>,
    tools: Vec<ToolSpec>,
    commands: Vec<CommandSpec>,
}

impl Loaded {
    fn new(addons: Vec<AddonSummary>, failures: Vec<LoadFailure>) -> Self {
        let (tools, dropped) = policy::unique_tools(&addons);
        for (addon, tool) in dropped {
            tracing::warn!(
                target: "dirge::addon",
                %addon, %tool,
                "addon tool dropped: an earlier addon already exposes that name"
            );
        }
        let (commands, dropped) = policy::unique_commands(&addons);
        for (addon, command) in dropped {
            tracing::warn!(
                target: "dirge::addon",
                %addon, %command,
                "addon command dropped: an earlier addon already registered that name"
            );
        }
        Self {
            addons,
            failures,
            tools,
            commands,
        }
    }
}

/// Load every manifest of `set` in order; failures stay beside the addons
/// that loaded.
fn load_all(runtime: &dyn AddonRuntime, set: &LoadSet, host_config: &Value) -> Loaded {
    let mut failures = set.failures.clone();
    let mut addons = Vec::new();
    for manifest in &set.manifests {
        match policy::parse_summary(manifest, &runtime.load(manifest, host_config)) {
            Ok(summary) => addons.push(summary),
            Err(failure) => failures.push(failure),
        }
    }
    Loaded::new(addons, failures)
}

pub struct AddonHost {
    runtime: Arc<dyn AddonRuntime>,
    host_config: Value,
    loaded: Mutex<Loaded>,
}

impl std::fmt::Debug for AddonHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = self.loaded.lock_ignore_poison();
        f.debug_struct("AddonHost")
            .field("addons", &loaded.addons)
            .field("failures", &loaded.failures)
            .finish_non_exhaustive()
    }
}

impl AddonHost {
    /// A host over `addons` already loaded on `runtime`; tests build hosts
    /// this way around a scripted runtime.
    #[cfg(test)]
    pub fn new(
        runtime: Arc<dyn AddonRuntime>,
        addons: Vec<AddonSummary>,
        failures: Vec<LoadFailure>,
    ) -> Self {
        Self {
            runtime,
            host_config: Value::Object(Default::default()),
            loaded: Mutex::new(Loaded::new(addons, failures)),
        }
    }

    /// Boot: load every manifest of `set` on a freshly started `runtime`.
    pub fn load(runtime: Arc<dyn AddonRuntime>, set: LoadSet, host_config: Value) -> Self {
        let loaded = load_all(runtime.as_ref(), &set, &host_config);
        Self {
            runtime,
            host_config,
            loaded: Mutex::new(loaded),
        }
    }

    /// Replace every addon with what `set` loads now, in the order the
    /// runtime's reload rules demand: every addon is shut down BEFORE its
    /// code is evaluated again, only the addons' own sources are evaluated
    /// (never the libraries they depend on), then each is constructed and
    /// initialized afresh. New tools and hooks reach the agent at its next
    /// run.
    pub fn reload(&self, set: LoadSet) -> ReloadReport {
        let before = self.loaded.lock_ignore_poison().clone();
        for addon in &before.addons {
            self.runtime.unload(&addon.id);
        }
        self.runtime.set_source_roots(&set.source_roots);
        let source_errors = self
            .runtime
            .reload_sources(&set.sources)
            .into_iter()
            .map(|(manifest, error)| LoadFailure { manifest, error })
            .collect();
        let after = load_all(self.runtime.as_ref(), &set, &self.host_config);
        let (tools_added, tools_removed) = policy::tool_diff(&before.tools, &after.tools);
        let report = ReloadReport {
            loaded: after.addons.iter().map(|a| a.id.clone()).collect(),
            failures: after.failures.clone(),
            source_errors,
            tools_added,
            tools_removed,
        };
        *self.loaded.lock_ignore_poison() = after;
        report
    }

    pub fn addons(&self) -> Vec<AddonSummary> {
        self.loaded.lock_ignore_poison().addons.clone()
    }

    pub fn failures(&self) -> Vec<LoadFailure> {
        self.loaded.lock_ignore_poison().failures.clone()
    }

    /// The tools offered to the model, collisions already resolved.
    pub fn tools(&self) -> Vec<ToolSpec> {
        self.loaded.lock_ignore_poison().tools.clone()
    }

    /// The slash commands addons registered, collisions already resolved.
    pub fn commands(&self) -> Vec<CommandSpec> {
        self.loaded.lock_ignore_poison().commands.clone()
    }

    /// The command typed as `/name`, if an addon registered it.
    pub fn command(&self, name: &str) -> Option<CommandSpec> {
        self.loaded
            .lock_ignore_poison()
            .commands
            .iter()
            .find(|c| c.name == name)
            .cloned()
    }

    /// True when at least one addon contributes `point`, so callers can skip
    /// the interpreter round trip on the hot path.
    pub fn listens(&self, point: HookPoint) -> bool {
        self.loaded
            .lock_ignore_poison()
            .addons
            .iter()
            .any(|a| a.hooks.contains(&point))
    }

    /// Run a tool: the runtime call, then the handler's return value read as
    /// `(content blocks, details)`.
    pub fn call_tool(&self, tool: &ToolSpec, args: &Value) -> Result<(Vec<Value>, Value), String> {
        self.runtime
            .call_tool(&tool.addon_id, &tool.name, args)
            .and_then(|out| policy::tool_output(&out))
    }

    /// Run a slash command with the text typed after its name.
    pub fn run_command(&self, command: &CommandSpec, args: &str) -> Result<CommandOutput, String> {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.runtime
            .run_command(
                &command.addon_id,
                &command.name,
                &policy::command_ctx(args, &cwd),
            )
            .map(|answer| policy::command_output(&answer))
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
    use std::collections::VecDeque;
    use std::path::Path;

    /// Scripted runtime: answers from fixed tables and records every call.
    #[derive(Default)]
    pub(crate) struct ScriptedRuntime {
        pub tool_answer: Option<Result<Value, String>>,
        pub command_answer: Option<Result<Value, String>>,
        pub hook_answers: Vec<HookReply>,
        /// Load reports, answered in order.
        pub load_answers: Mutex<VecDeque<Value>>,
        pub source_errors: Vec<(PathBuf, String)>,
        pub calls: Mutex<Vec<String>>,
    }

    impl ScriptedRuntime {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl AddonRuntime for ScriptedRuntime {
        fn load(&self, manifest: &Path, _host_config: &Value) -> Value {
            self.record(format!("load {}", manifest.display()));
            self.load_answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| json!({"error": "unscripted load"}))
        }

        fn unload(&self, addon_id: &str) {
            self.record(format!("unload {addon_id}"));
        }

        fn reload_sources(&self, files: &[PathBuf]) -> Vec<(PathBuf, String)> {
            self.record(format!("reload-sources {}", files.len()));
            self.source_errors.clone()
        }

        fn set_source_roots(&self, roots: &[PathBuf]) {
            self.record(format!("roots {}", roots.len()));
        }

        fn call_tool(&self, addon_id: &str, tool: &str, args: &Value) -> Result<Value, String> {
            self.record(format!("tool {addon_id}/{tool} {args}"));
            self.tool_answer.clone().unwrap_or(Ok(Value::Null))
        }

        fn run_command(&self, addon_id: &str, name: &str, ctx: &Value) -> Result<Value, String> {
            self.record(format!("command {addon_id}/{name} {}", ctx["args"]));
            self.command_answer.clone().unwrap_or(Ok(Value::Null))
        }

        fn run_hook(&self, point: HookPoint, ctx: &Value) -> Vec<HookReply> {
            self.record(format!("hook {} {ctx}", point.key()));
            self.hook_answers.clone()
        }

        fn shutdown(&self) {
            self.record("shutdown".into());
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
            commands: Vec::new(),
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

    fn load_set(manifests: &[&str]) -> LoadSet {
        LoadSet {
            manifests: manifests.iter().map(PathBuf::from).collect(),
            failures: Vec::new(),
            source_roots: vec![PathBuf::from("/addons/a/src")],
            sources: vec![PathBuf::from("/addons/a/src/a/core.cljc")],
        }
    }

    #[test]
    fn reload_shuts_down_before_evaluating_then_loads_afresh() {
        let rt = ScriptedRuntime::default();
        rt.load_answers.lock().unwrap().push_back(json!({
            "id": "a",
            "tools": [{"name": "y"}, {"name": "z"}],
            "commands": [{"name": "go", "description": "run it"}]
        }));
        let (host, rt) = host(rt, vec![summary("a", &["x", "y"], &[])]);

        let report = host.reload(load_set(&["a.edn"]));

        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec!["unload a", "roots 1", "reload-sources 1", "load a.edn"]
        );
        assert_eq!(report.loaded, vec!["a".to_string()]);
        assert_eq!(report.tools_added, vec!["z".to_string()]);
        assert_eq!(report.tools_removed, vec!["x".to_string()]);
        let tools: Vec<String> = host.tools().into_iter().map(|t| t.exposed_name).collect();
        assert_eq!(tools, vec!["y", "z"]);
        assert_eq!(host.command("go").map(|c| c.addon_id), Some("a".into()));
    }

    #[test]
    fn a_reload_that_fails_keeps_the_reason_and_drops_the_tools() {
        let rt = ScriptedRuntime {
            source_errors: vec![(PathBuf::from("/addons/a/src/a/core.cljc"), "eof".into())],
            ..Default::default()
        };
        rt.load_answers
            .lock()
            .unwrap()
            .push_back(json!({"error": "init-fn not found"}));
        let (host, _) = host(rt, vec![summary("a", &["x"], &[])]);

        let report = host.reload(load_set(&["a.edn"]));

        assert!(report.loaded.is_empty());
        assert_eq!(report.failures[0].error, "init-fn not found");
        assert_eq!(report.source_errors[0].error, "eof");
        assert_eq!(report.tools_removed, vec!["x".to_string()]);
        assert!(host.tools().is_empty());
        assert_eq!(host.failures().len(), 1);
    }

    #[test]
    fn commands_run_with_the_typed_text_and_read_the_answer() {
        let mut a = summary("a", &[], &[]);
        a.commands = vec![CommandSpec {
            addon_id: "a".into(),
            name: "go".into(),
            description: String::new(),
        }];
        let (host, rt) = host(
            ScriptedRuntime {
                command_answer: Some(Ok(json!({"text": "ok", "prompt": "continue"}))),
                ..Default::default()
            },
            vec![a],
        );
        let command = host.command("go").expect("registered");
        let out = host.run_command(&command, "fast please").unwrap();
        assert_eq!(out.text.as_deref(), Some("ok"));
        assert_eq!(out.prompt.as_deref(), Some("continue"));
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![r#"command a/go "fast please""#.to_string()]
        );
        assert!(host.command("missing").is_none());
    }
}
