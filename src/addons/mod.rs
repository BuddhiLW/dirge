//! Clojure addon host (cargo feature `addons`).
//!
//! Loads IAddons (`hive-addon.protocol/IAddon`, portable `.cljc`) into an
//! embedded clojurust interpreter. An addon ships
//! `resources/META-INF/hive-addons/<id>.edn`; its `tools` become loop tools
//! and its `:dirge/*` hooks run at dirge's hook points. See docs/addons.md.

pub mod cljrs;
pub mod discovery;
pub mod domain;
pub mod host;
pub mod layout;
pub mod loop_hooks;
pub mod manifest;
pub mod policy;
pub mod port;
pub mod sink;
pub mod tool;

#[cfg(test)]
mod acceptance_tests;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::json;

use domain::{AddonPlan, LoadFailure};
use host::AddonHost;
use port::HarnessSink;

static HOST: OnceLock<Arc<AddonHost>> = OnceLock::new();

/// The process-wide host, once [`install_from_config`] started one.
pub fn global() -> Option<Arc<AddonHost>> {
    HOST.get().cloned()
}

/// Discover and load addons for this process. A no-op when disabled or when
/// no manifest is found, so a build with the feature costs nothing until an
/// addon is installed. Failures are logged, never fatal.
pub fn install_from_config(cfg: &crate::config::Config) {
    let settings = cfg.addons.clone().unwrap_or_default();
    if settings.enabled == Some(false) {
        return;
    }
    let plan = discovery::plan(&search_dirs(&settings), &extra_roots(&settings));
    if plan.is_empty() {
        return;
    }
    match start(plan, Arc::new(sink::TuiSink)) {
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
            let _ = HOST.set(Arc::new(host));
        }
        Err(error) => {
            tracing::warn!(target: "dirge::addon", %error, "addon host did not start");
        }
    }
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
pub fn start(plan: AddonPlan, sink: Arc<dyn HarnessSink>) -> Result<AddonHost, String> {
    let (valid, mut failures) = validate(&plan.manifests, &plan.source_roots);
    if valid.is_empty() {
        return Err(describe_failures(&failures));
    }
    let isolate = Arc::new(cljrs::Isolate::spawn(plan.source_roots, sink)?);
    let host_config = json!({ "harness": "dirge", "version": env!("CARGO_PKG_VERSION") });
    let mut addons = Vec::new();
    for manifest in valid {
        match policy::parse_summary(&manifest, &isolate.load(&manifest, &host_config)) {
            Ok(summary) => addons.push(summary),
            Err(failure) => failures.push(failure),
        }
    }
    Ok(AddonHost::new(isolate, addons, failures))
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
