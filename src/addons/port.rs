//! Traits the addon host depends on: the runtime and the notification sink.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::domain::{HookPoint, HookReply, PanelRequest, SourceOutcome};

/// A running addon runtime. Calls are synchronous round trips: the only
/// adapter serializes them onto one interpreter thread, so an async caller
/// wraps them in `spawn_blocking`.
pub trait AddonRuntime: Send + Sync + 'static {
    /// Load one manifest: construct, initialize, register. An addon already
    /// loaded under the same id is shut down first. Answers the load report,
    /// a summary or `{"error": msg}`.
    fn load(&self, manifest: &Path, host_config: &Value) -> Value;

    /// Shut one addon down and forget it. Idempotent.
    fn unload(&self, addon_id: &str);

    /// Evaluate `files` again so their namespaces run the code now on disk.
    /// Answers the files that failed, with why.
    fn reload_sources(&self, files: &[PathBuf]) -> Vec<(PathBuf, String)>;

    /// [`AddonRuntime::reload_sources`], also answering the files left
    /// alone because nothing loaded their namespace. A runtime that never
    /// skips keeps the default.
    fn reload_sources_outcome(&self, files: &[PathBuf]) -> SourceOutcome {
        SourceOutcome {
            errors: self.reload_sources(files),
            skipped: Vec::new(),
        }
    }

    /// Replace the roots `require` searches.
    fn set_source_roots(&self, roots: &[PathBuf]);

    /// Invoke `tool` of `addon_id` with JSON `args`. `Ok` carries the
    /// handler's return value, `Err` a message fit for the model.
    fn call_tool(&self, addon_id: &str, tool: &str, args: &Value) -> Result<Value, String>;

    /// Run slash command `name` of `addon_id` with `ctx`.
    fn run_command(&self, addon_id: &str, name: &str, ctx: &Value) -> Result<Value, String>;

    /// Call every addon's `point` hook with `ctx`, in load order.
    fn run_hook(&self, point: HookPoint, ctx: &Value) -> Vec<HookReply>;

    /// Call every addon's hook keyed `key` (a keyword without its colon,
    /// e.g. `dirge/event`) with `ctx`, in load order. Unlike [`run_hook`] the
    /// key is open: a seam needs no [`HookPoint`] of its own to reach addons.
    /// The default reaches only the keys a [`HookPoint`] names.
    ///
    /// [`run_hook`]: AddonRuntime::run_hook
    fn run_hook_key(&self, key: &str, ctx: &Value) -> Vec<HookReply> {
        HookPoint::from_key(key)
            .map(|point| self.run_hook(point, ctx))
            .unwrap_or_default()
    }

    /// Hand `ctx` to every addon's hook keyed `key` without waiting for the
    /// answers, which are dropped. May drop the call itself when the runtime
    /// is backed up. The default runs it in place.
    fn post_hook(&self, key: &str, ctx: &Value) {
        let _ = self.run_hook_key(key, ctx);
    }

    /// The addons' summaries as the runtime re-read them since the last
    /// call, when it did: after a REPL evaluation, or when addon code asked
    /// with `dirge.harness/refresh!`. One load report per addon, in load
    /// order.
    fn take_refreshed(&self) -> Option<Vec<Value>> {
        None
    }

    /// Where a REPL into this runtime listens (`host:port`), if one does.
    fn repl_endpoint(&self) -> Option<String> {
        None
    }

    /// Run command-hook `handler` of `addon_id` (its `:dirge/command-hooks`
    /// entry) with `ctx`. `Ok` carries the handler's answer.
    fn run_hook_handler(
        &self,
        addon_id: &str,
        handler: &str,
        _ctx: &Value,
    ) -> Result<Value, String> {
        Err(format!(
            "this addon runtime cannot run command hook {handler} of {addon_id}"
        ))
    }

    /// Shut every addon down. Idempotent.
    fn shutdown(&self);
}

/// Where `dirge.harness/panel` delivers side-panel changes.
pub trait PanelSink: Send + Sync + 'static {
    /// `false` when the change was dropped (no UI, or it is backed up).
    fn panel(&self, request: PanelRequest) -> bool;
}

/// The MCP servers dirge is connected to, as `dirge.harness/mcp-call`
/// reaches them. Calls block the calling thread until the server answers.
pub trait McpGateway: Send + Sync + 'static {
    /// Names of the servers currently connected.
    fn servers(&self) -> Vec<String>;

    /// Call `tool` on `server` with JSON object `args`. `Ok` carries the
    /// tool result (`{"content": [...], "isError": bool, ...}`).
    fn call(&self, server: &str, tool: &str, args: &Value) -> Result<Value, String>;
}

/// dirge's own loop tools, as `dirge.harness/call-tool` reaches them. Calls
/// block the calling thread until the tool answers.
pub trait ToolGateway: Send + Sync + 'static {
    /// Names of the tools addon code may call.
    fn names(&self) -> Vec<String>;

    /// Run `tool` with JSON object `args`. `Ok` carries its output as text.
    fn call(&self, tool: &str, args: &Value) -> Result<String, String>;
}

/// Where `dirge.harness/emit` posts an open hook key, addon to addon. The
/// key is already checked ([`crate::addons::emit::emit_key`]); the post is
/// queued and never awaited.
pub trait AddonEmitter: Send + Sync + 'static {
    /// Post `ctx` to every addon's hook keyed `key`. `Err` when it cannot
    /// be posted at all (no host); a post dropped because the runtime is
    /// backed up is still `Ok`.
    fn emit(&self, key: &str, ctx: &Value) -> Result<(), String>;
}

/// A host where addons cannot reach each other.
pub struct NoEmitter;

impl AddonEmitter for NoEmitter {
    fn emit(&self, _key: &str, _ctx: &Value) -> Result<(), String> {
        Err("emit is unavailable: no addon host receives it".to_string())
    }
}

/// The per-addon settings the user wrote under `addons.settings` in
/// `config.json`, as `dirge.harness/config` and `:dirge/host :config` read
/// them. Answers are read at call time, so an adapter may change underneath.
pub trait ConfigSource: Send + Sync + 'static {
    /// Every addon's settings, keyed by addon id.
    fn settings(&self) -> serde_json::Map<String, Value>;
}

/// The settings of `addon_id` in `settings`: its entry, or an empty map
/// when it has none.
pub fn addon_settings(settings: &serde_json::Map<String, Value>, addon_id: &str) -> Value {
    settings
        .get(addon_id)
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()))
}

/// A host without addon settings.
pub struct NoConfig;

impl ConfigSource for NoConfig {
    fn settings(&self) -> serde_json::Map<String, Value> {
        serde_json::Map::new()
    }
}

/// Everything the `dirge.harness` natives reach, injected when the runtime
/// boots.
#[derive(Clone)]
pub struct Harness {
    pub sink: Arc<dyn HarnessSink>,
    pub panels: Arc<dyn PanelSink>,
    pub mcp: Arc<dyn McpGateway>,
    pub tools: Arc<dyn ToolGateway>,
    pub config: Arc<dyn ConfigSource>,
    pub emitter: Arc<dyn AddonEmitter>,
}

impl Harness {
    /// Notifications to `sink`; no panels, no MCP servers, no tools, no
    /// addon settings, no addon-to-addon emits.
    pub fn with_sink(sink: Arc<dyn HarnessSink>) -> Self {
        Self {
            sink,
            panels: Arc::new(NoPanels),
            mcp: Arc::new(NoMcp),
            tools: Arc::new(NoTools),
            config: Arc::new(NoConfig),
            emitter: Arc::new(NoEmitter),
        }
    }
}

/// A host without callable tools.
pub struct NoTools;

impl ToolGateway for NoTools {
    fn names(&self) -> Vec<String> {
        Vec::new()
    }

    fn call(&self, tool: &str, _args: &Value) -> Result<String, String> {
        Err(format!("no tool named '{tool}'"))
    }
}

/// A host whose build cannot run tools: no names, and every call answers
/// the reason it holds.
#[cfg_attr(feature = "plugin", allow(dead_code))]
pub struct ToolsUnavailable(pub &'static str);

impl ToolGateway for ToolsUnavailable {
    fn names(&self) -> Vec<String> {
        Vec::new()
    }

    fn call(&self, _tool: &str, _args: &Value) -> Result<String, String> {
        Err(self.0.to_string())
    }
}

/// A host without a side panel.
pub struct NoPanels;

impl PanelSink for NoPanels {
    fn panel(&self, _request: PanelRequest) -> bool {
        false
    }
}

/// A host without MCP servers.
pub struct NoMcp;

impl McpGateway for NoMcp {
    fn servers(&self) -> Vec<String> {
        Vec::new()
    }

    fn call(&self, server: &str, _tool: &str, _args: &Value) -> Result<Value, String> {
        Err(format!("no MCP server named {server} is connected"))
    }
}

/// Severity of a harness notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    /// `:info` / `:warn` / `:error` (colon optional); anything else is info.
    pub fn parse(s: &str) -> Level {
        match s.trim_start_matches(':') {
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }
}

/// Where `dirge.harness` natives deliver what an addon says to the user.
pub trait HarnessSink: Send + Sync + 'static {
    fn notify(&self, addon_level: Level, message: &str);
}

#[cfg(test)]
mod tests {
    use super::{ConfigSource, Level, NoConfig, ToolGateway, ToolsUnavailable, addon_settings};

    #[test]
    fn an_addon_without_settings_reads_an_empty_map() {
        let mut settings = serde_json::Map::new();
        settings.insert("echo".into(), serde_json::json!({"greeting": "hi"}));
        assert_eq!(
            addon_settings(&settings, "echo"),
            serde_json::json!({"greeting": "hi"})
        );
        assert_eq!(addon_settings(&settings, "other"), serde_json::json!({}));
        assert!(NoConfig.settings().is_empty());
    }

    #[test]
    fn unavailable_tools_offer_nothing_and_answer_why() {
        let gateway = ToolsUnavailable("not in this build");
        assert!(gateway.names().is_empty());
        assert_eq!(
            gateway.call("read", &serde_json::json!({})),
            Err("not in this build".to_string())
        );
    }

    #[test]
    fn levels_parse_with_or_without_colon_and_default_to_info() {
        assert_eq!(Level::parse(":warn"), Level::Warn);
        assert_eq!(Level::parse("error"), Level::Error);
        assert_eq!(Level::parse(":debug"), Level::Info);
    }
}
