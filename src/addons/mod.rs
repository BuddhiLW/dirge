//! Clojure addon host (cargo feature `addons`).
//!
//! Loads addons written against an IAddon protocol in portable `.cljc` into
//! an embedded clojurust interpreter. An addon ships
//! `resources/META-INF/addons/<id>.edn`; its `tools` become loop tools, its
//! `:dirge/*` hooks run at dirge's hook points and its `:dirge/commands`
//! become slash commands. `/addons reload` swaps all of it in place. See
//! docs/addons.md.

pub mod cljrs;
pub mod compaction;
pub mod discovery;
pub mod domain;
pub mod host;
pub mod layout;
pub mod lifecycle;
pub mod loop_hooks;
pub mod manifest;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod policy;
pub mod port;
pub mod sink;
pub mod tool;
#[cfg(feature = "plugin")]
pub mod tool_calls;

#[cfg(test)]
mod acceptance_tests;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::json;

use domain::{AddonPlan, LoadFailure, ReloadReport};
use host::{AddonHost, LoadSet};
use port::Harness;

/// Protocol namespace used when `addons.protocol_ns` is not set.
pub const DEFAULT_PROTOCOL_NS: &str = "hive-addon.protocol";

static HOST: OnceLock<Arc<AddonHost>> = OnceLock::new();

/// The process-wide host, once [`install_from_config`] or [`reload`]
/// started one.
pub fn global() -> Option<Arc<AddonHost>> {
    HOST.get().cloned()
}

/// Discover and load addons for this process. A no-op when disabled or when
/// no manifest is found, so a build with the feature costs nothing until an
/// addon is installed. Failures are logged, never fatal.
///
/// Called on the thread that runs dirge's single-threaded event loop, which
/// it marks: prompt hooks, loading and shutdown reach the isolate from that
/// thread, and addon code must not wait on the loop while it waits.
pub fn install_from_config(cfg: &crate::config::Config) {
    cljrs::isolate::mark_event_loop_thread();
    let settings = cfg.addons.clone().unwrap_or_default();
    if settings.enabled == Some(false) {
        return;
    }
    let plan = discovery::plan(&search_dirs(&settings), &extra_roots(&settings));
    if plan.is_empty() {
        return;
    }
    match start(plan, harness(), protocol_ns(&settings)) {
        Ok(host) => {
            for failure in host.failures() {
                tracing::warn!(
                    target: "dirge::addon",
                    manifest = %failure.manifest.display(),
                    error = %failure.error,
                    "addon failed to load"
                );
            }
            tracing::info!(
                target: "dirge::addon",
                addons = host.addons().len(),
                tools = host.tools().len(),
                "addon host started"
            );
            publish(Arc::new(host));
        }
        Err(error) => {
            tracing::warn!(target: "dirge::addon", %error, "addon host did not start");
        }
    }
}

/// Discover again and replace every addon in place: the host's reload when
/// one is running, a fresh start when dirge booted without addons. Blocks
/// on the isolate; call it off the async runtime (`spawn_blocking`).
pub fn reload(
    settings: &crate::config::AddonsConfig,
) -> Result<(Arc<AddonHost>, ReloadReport), String> {
    if settings.enabled == Some(false) {
        return Err("addons are disabled (addons.enabled is false)".to_string());
    }
    let plan = discovery::plan(&search_dirs(settings), &extra_roots(settings));
    if let Some(host) = global() {
        let report = host.reload(load_set(&plan, true));
        register_commands(&host);
        return Ok((host, report));
    }
    let host = Arc::new(start(plan, harness(), protocol_ns(settings))?);
    let report = ReloadReport {
        loaded: host.addons().into_iter().map(|a| a.id).collect(),
        failures: host.failures(),
        tools_added: host.tools().into_iter().map(|t| t.exposed_name).collect(),
        ..ReloadReport::default()
    };
    publish(host.clone());
    Ok((global().unwrap_or(host), report))
}

/// Shut every addon down. Call once on exit.
pub fn shutdown() {
    if let Some(host) = global() {
        host.shutdown();
    }
}

/// Validate, boot, load: the plan becomes a running host. Manifests that
/// fail validation or loading are kept as [`LoadFailure`]s beside the
/// addons that loaded.
pub fn start(plan: AddonPlan, harness: Harness, protocol_ns: &str) -> Result<AddonHost, String> {
    let set = load_set(&plan, false);
    if set.manifests.is_empty() {
        return Err(describe_failures(&set.failures));
    }
    let isolate = Arc::new(cljrs::Isolate::spawn(
        set.source_roots.clone(),
        harness,
        protocol_ns,
    )?);
    let host_config = json!({ "harness": "dirge", "version": env!("CARGO_PKG_VERSION") });
    Ok(AddonHost::load(isolate, set, host_config))
}

/// What loading `plan` works from. `with_sources` lists the addons' own
/// source files, which only a reload evaluates.
fn load_set(plan: &AddonPlan, with_sources: bool) -> LoadSet {
    let (manifests, failures) = validate(&plan.manifests, &plan.source_roots);
    let sources = if with_sources {
        discovery::own_sources(&manifests)
    } else {
        Vec::new()
    };
    LoadSet {
        manifests,
        failures,
        source_roots: plan.source_roots.clone(),
        sources,
    }
}

fn publish(host: Arc<AddonHost>) {
    register_commands(&host);
    let _ = HOST.set(host);
}

/// Hand `host`'s addons the command names left to them: names a built-in
/// or plugin command takes are withheld, the rest complete on Tab.
fn register_commands(host: &AddonHost) {
    host.reserve_commands(Arc::new(taken_by_dirge));
    #[cfg(feature = "slash-completion")]
    crate::ui::slash::register_addon_commands(
        host.commands().into_iter().map(|c| c.name).collect(),
    );
}

/// True when `name` (without the `/`) is a built-in or plugin slash
/// command, which dirge dispatches before any addon command.
fn taken_by_dirge(name: &str) -> bool {
    if crate::ui::slash::is_known_slash_command(&format!("/{name}")) {
        return true;
    }
    #[cfg(feature = "plugin")]
    if let Some(plugins) = crate::plugin::hook::global() {
        use crate::sync_util::LockExt;
        return plugins
            .lock_ignore_poison()
            .list_commands()
            .iter()
            .any(|(taken, _)| taken == name);
    }
    false
}

/// What `dirge.harness` reaches in this process: the TUI for notifications
/// and panels, dirge's loop tools, and the MCP servers dirge connects to.
fn harness() -> Harness {
    let tui = Arc::new(sink::TuiSink);
    let mut harness = Harness::with_sink(tui.clone());
    harness.panels = tui;
    #[cfg(feature = "plugin")]
    if let Some(live) = tool_calls::LoopTools::live() {
        harness.tools = Arc::new(live);
    }
    #[cfg(not(feature = "plugin"))]
    {
        harness.tools = Arc::new(port::ToolsUnavailable(
            "call-tool is unavailable in this build: dirge was built without the `plugin` feature",
        ));
    }
    #[cfg(feature = "mcp")]
    if let Some(live) = mcp::LiveMcp::current() {
        harness.mcp = Arc::new(live);
    }
    harness
}

fn protocol_ns(settings: &crate::config::AddonsConfig) -> &str {
    settings
        .protocol_ns
        .as_deref()
        .unwrap_or(DEFAULT_PROTOCOL_NS)
}

/// Manifests that parse and whose init namespace has portable source.
/// JVM-only addons sharing a repo with portable ones are skipped quietly.
fn validate(manifests: &[PathBuf], roots: &[PathBuf]) -> (Vec<PathBuf>, Vec<LoadFailure>) {
    let mut valid = Vec::new();
    let mut failures = Vec::new();
    for path in manifests {
        match manifest::AddonManifest::from_path(path) {
            Ok(m) if discovery::has_portable_source(roots, &m.init_ns) => valid.push(path.clone()),
            Ok(m) => tracing::debug!(
                target: "dirge::addon",
                manifest = %path.display(),
                init_ns = %m.init_ns,
                "skipped: no .cljc/.cljrs source for the init namespace"
            ),
            Err(e) => failures.push(LoadFailure {
                manifest: path.clone(),
                error: e.to_string(),
            }),
        }
    }
    (valid, failures)
}

fn describe_failures(failures: &[LoadFailure]) -> String {
    let lines: Vec<String> = failures
        .iter()
        .map(|f| format!("{}: {}", f.manifest.display(), f.error))
        .collect();
    format!("no loadable addon manifest ({})", lines.join("; "))
}

/// Where manifests are searched: the project's `.dirge/addons/`, the user's
/// `~/.config/dirge/addons/`, then configured `addons.paths`.
fn search_dirs(settings: &crate::config::AddonsConfig) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dirs = vec![
        crate::extras::dirge_paths::ProjectPaths::new(&cwd)
            .dirge_dir()
            .join("addons"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".config").join("dirge").join("addons"));
    }
    dirs.extend(settings.paths.iter().map(|p| expand_home(p)));
    dirs
}

/// Extra source roots: configured `addons.source_paths`, then
/// `DIRGE_ADDON_PATH` (a PATH-style list).
fn extra_roots(settings: &crate::config::AddonsConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = settings
        .source_paths
        .iter()
        .map(|p| expand_home(p))
        .collect();
    if let Some(list) = std::env::var_os("DIRGE_ADDON_PATH") {
        roots.extend(std::env::split_paths(&list));
    }
    roots
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}
