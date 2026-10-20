//! Vigil heartbeat/wakeup runtime.
//!
//! Public API:
//! - `VigilKeeper::from_entries()` — build keeper from config entries.
//! - `VigilKeeper::run()` — start the reaper and all triggers, return when shutdown.
//!
//! Internal modules:
//! - `types` — VigilEvent, VigilInstance, VigilCtl
//! - `rite` — gate check evaluation
//! - `dispatch` — commands-mode template substitution
//! - `toll` — timer trigger
//! - `watcher` — filesystem trigger
//! - `harbinger` — socket trigger
//! - `reaper` — event drain + coalesce + observance dispatch

pub mod dispatch;
pub mod harbinger;
pub mod reaper;
pub mod rite;
pub mod toll;
pub mod types;
pub mod watcher;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::VigilEntry;

use self::reaper::Observance;
use self::types::{
    EnrichOutcome, GateVerdict, HookDispatchRequest, HookResponse, TriggerKind, VigilCtl,
    VigilEvent, VigilInstance, VigilReapInput,
};

/// Simple runtime state for vigil mode — exposed to the UI loop so it knows
/// whether to sleep between observances and carries pending observance data
/// so the post-turn handler can dispatch on-vigil-observance with :response.
#[allow(dead_code)] // consumed by the interactive TUI loop (slice 3)
pub struct VigilState {
    pub active: bool,
    /// If set, the current agent turn is a vigil observance. The post-turn
    /// handler reads this to dispatch `on-vigil-observance` with the agent's
    /// response text. Cleared after dispatch.
    pub pending_observance: Option<PendingObservance>,
}

/// Metadata for a vigil observance that will fire after the agent turn.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by the interactive TUI loop (slice 3)
pub struct PendingObservance {
    pub vigil_name: String,
    pub event_count: usize,
    pub running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Coalesced event context (JSON) that triggered this observance — the
    /// "signal" half of the (signal, outcome) pair persisted post-turn.
    pub signal: String,
}

/// Format the `on-vigil-observance` hook context string, escaping the vigil
/// name and agent response for the Janet `@{...}` template.
pub fn observance_context(vigil_name: &str, event_count: usize, response: &str) -> String {
    let escaped_name = vigil_name.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_response = response.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "@{{:vigil \"{}\" :count {} :response \"{}\" :exit :ok}}",
        escaped_name, event_count, escaped_response
    )
}

/// Dispatch the post-turn vigil hooks after an agent observance and run any
/// shell commands the act hook emitted.
///
/// `on-vigil-observance` is the "act" half of act/chaining: the hook runs
/// `(harness/toil ...)` to ask the host to execute shell commands directly,
/// and chains by calling `(vigil/emit next-vigil {...})`, which the keeper
/// routes back onto the event bus (see the `vigil/emit` Janet prelude in
/// `worker.rs`).
///
/// `on-vigil-outcome` is the outcome loop: the hook classifies the finished
/// observance via `(harness/outcome label useful?)` and the host persists the
/// (signal, outcome) pair to the vigil store so `GateFail::Prior` can consult
/// the empirical positive rate.
///
/// Shared by the interactive turn handler (`done.rs`) and the `--vigil-once`
/// headless driver (`main.rs`) so a vigil observance actuates the same way
/// in both modes.
#[cfg(feature = "plugin")]
pub async fn dispatch_observance_and_act(
    plugin_manager: &std::sync::Arc<std::sync::Mutex<crate::plugin::PluginManager>>,
    vigil_name: &str,
    event_count: usize,
    response: &str,
    signal: &str,
) {
    use crate::sync_util::LockExt;
    let ctx = observance_context(vigil_name, event_count, response);

    // Act + chaining: run the observance hook, then execute any toil.
    let pm = std::sync::Arc::clone(plugin_manager);
    let observance_ctx = ctx.clone();
    let observance_result = tokio::task::spawn_blocking(move || {
        pm.lock_ignore_poison()
            .dispatch_tool_hook("on-vigil-observance", &observance_ctx)
    })
    .await
    .unwrap_or_else(|join_err| {
        warn!(%vigil_name, error = %join_err, "on-vigil-observance hook panicked");
        Err("on-vigil-observance hook panicked".to_string())
    });

    if let Ok(result) = observance_result
        && let Some(commands) = result.toil
    {
        for command in commands {
            info!(%vigil_name, %command, "observance act dispatch");
            match tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output()
                .await
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let code = output.status.code().unwrap_or(-1);
                    info!(
                        %vigil_name,
                        exit_code = code,
                        stdout = %stdout.trim(),
                        "observance act finished"
                    );
                }
                Err(e) => {
                    warn!(%vigil_name, %command, "observance act failed: {e}");
                }
            }
        }
    }

    // Outcome loop: classify the finished observance, then persist the
    // (signal, outcome) pair for the empirical prior.
    let pm = std::sync::Arc::clone(plugin_manager);
    let outcome_ctx = ctx.clone();
    let outcome_result = tokio::task::spawn_blocking(move || {
        pm.lock_ignore_poison()
            .dispatch_tool_hook("on-vigil-outcome", &outcome_ctx)
    })
    .await
    .unwrap_or_else(|join_err| {
        warn!(%vigil_name, error = %join_err, "on-vigil-outcome hook panicked");
        Err("on-vigil-outcome hook panicked".to_string())
    });

    let Some((label, useful)) = outcome_result
        .ok()
        .and_then(|result| result.outcome)
        .and_then(|raw| parse_outcome(&raw))
    else {
        return;
    };
    persist_outcome(vigil_name, signal, &label, useful);
}

/// Parse a `harness/outcome` slot value (`{"label":...,"useful":...}`) into
/// its parts. `useful` defaults to true when absent.
#[cfg(feature = "plugin")]
fn parse_outcome(raw: &str) -> Option<(String, bool)> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let label = value.get("label")?.as_str()?.to_string();
    let useful = value
        .get("useful")
        .and_then(|u| u.as_bool())
        .unwrap_or(true);
    Some((label, useful))
}

/// Best-effort write of an observance's (signal, outcome) pair. Reopens the
/// shared session DB (the keeper's own store is consumed by the reaper, not
/// reachable from the post-turn path); a missing DB or write failure is
/// logged, never fatal to the turn.
#[cfg(feature = "plugin")]
fn persist_outcome(vigil_name: &str, signal: &str, outcome: &str, useful: bool) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let db_path = crate::extras::dirge_paths::ProjectPaths::new(&cwd).session_db_path();
    let store = crate::extras::vigil_db::VigilStore::open_at(&db_path);
    match store {
        Ok(store) => {
            if let Err(e) = store.record_outcome(vigil_name, signal, outcome, useful) {
                warn!(%vigil_name, "failed to persist vigil outcome: {e}");
            }
        }
        Err(e) => {
            warn!(%vigil_name, "cannot open vigil store for outcome: {e}");
        }
    }
}

/// Spawn a background drainer that consumes hook-dispatch requests and runs
/// them through the plugin manager. A synchronous `on-vigil-rite` request
/// carries a oneshot, which the drainer answers with a `GateVerdict`.
/// Running this in both interactive and headless (`--vigil-once`) modes means
/// the synchronous rite gate never deadlocks waiting on the UI loop to drain
/// the channel.
pub fn spawn_hook_drainer(
    hook_rx: mpsc::Receiver<HookDispatchRequest>,
    #[cfg(feature = "plugin")] plugin_manager: Option<
        Arc<std::sync::Mutex<crate::plugin::PluginManager>>,
    >,
) {
    tokio::spawn(async move {
        let mut hook_rx = hook_rx;
        while let Some(req) = hook_rx.recv().await {
            let hook_name = req.hook_name.clone();
            let context = req.context.clone();
            let respond_to = req.respond_to;
            #[cfg(feature = "plugin")]
            let threshold = req.threshold;

            // Dispatch through the plugin manager when present; a missing
            // manager or plugin error yields `None` and the caller's default
            // (fail posture for the gate, `Pass` for enrich) applies.
            #[cfg(feature = "plugin")]
            let result: Option<crate::plugin::ToolHookResult> = match plugin_manager.as_ref() {
                Some(pm) => {
                    let pm = pm.clone();
                    let hook = hook_name.clone();
                    let ctx = context.clone();
                    tokio::task::spawn_blocking(move || {
                        use crate::sync_util::LockExt;
                        pm.lock_ignore_poison().dispatch_tool_hook(&hook, &ctx).ok()
                    })
                    .await
                    .unwrap_or(None)
                }
                None => None,
            };

            #[cfg(feature = "plugin")]
            let verdict = result
                .clone()
                .map(|r| verdict_from_hook_result(r, threshold))
                .unwrap_or(GateVerdict::Rouse);
            #[cfg(not(feature = "plugin"))]
            let verdict = GateVerdict::Rouse;

            #[cfg(feature = "plugin")]
            let enrich = result
                .map(enrich_from_hook_result)
                .unwrap_or(EnrichOutcome::Pass);
            #[cfg(not(feature = "plugin"))]
            let enrich = EnrichOutcome::Pass;

            // Only the two synchronous vigil hooks carry a oneshot; every
            // other hook is fire-and-forget (respond_to is `None`).
            let response = match hook_name.as_str() {
                "on-vigil-rite" => Some(HookResponse::Verdict(verdict)),
                "on-vigil-enrich" => Some(HookResponse::Enrich(enrich)),
                _ => None,
            };

            if let (Some(tx), Some(response)) = (respond_to, response) {
                let _ = tx.send(response);
            }
        }
    });
}

/// Interpret a plugin hook result for the synchronous rite gate into a
/// typed verdict. Precedence: `block` (explicit shroud) wins, then `toil`
/// (explicit act), then the first-class `verdict` confidence (`p` compared
/// against the vigil's wake threshold), then a default rouse.
#[cfg(feature = "plugin")]
fn verdict_from_hook_result(
    r: crate::plugin::ToolHookResult,
    threshold: Option<f64>,
) -> GateVerdict {
    if let Some(reason) = r.block {
        GateVerdict::Shroud { reason }
    } else if let Some(commands) = r.toil {
        GateVerdict::Toil { commands }
    } else if let Some(raw) = r.verdict {
        parse_verdict(&raw, threshold).unwrap_or(GateVerdict::Rouse)
    } else {
        GateVerdict::Rouse
    }
}

/// Decode a `harness/verdict` slot (`{"p":0.0..1.0}`) into a typed verdict
/// by comparing `p` to the wake threshold. `None` threshold (the hook is not
/// a rite gate) yields `None` so the caller keeps its rouse default.
#[cfg(feature = "plugin")]
fn parse_verdict(raw: &str, threshold: Option<f64>) -> Option<GateVerdict> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let p = value.get("p")?.as_f64()?;
    match threshold {
        Some(t) if p > t => Some(GateVerdict::Rouse),
        Some(t) => Some(GateVerdict::Shroud {
            reason: format!("gate confidence {p} not above threshold {t}"),
        }),
        None => None,
    }
}

/// Interpret a plugin hook result for the `on-vigil-enrich` stage. `block`
/// drops the event (filter), `enrich` merges JSON context, otherwise pass.
#[cfg(feature = "plugin")]
fn enrich_from_hook_result(r: crate::plugin::ToolHookResult) -> EnrichOutcome {
    if let Some(reason) = r.block {
        EnrichOutcome::Drop { reason }
    } else if let Some(json) = r.enrich {
        EnrichOutcome::Enriched(json)
    } else {
        EnrichOutcome::Pass
    }
}

/// The vigil-keeper — owns all active vigils, starts triggers, runs the reaper.
pub struct VigilKeeper {
    pub vigils: Vec<VigilInstance>,
    #[allow(dead_code)]
    pub ctl_tx: Option<mpsc::Sender<VigilCtl>>,
    pub observance_rx: Option<mpsc::Receiver<Observance>>,
    /// Untyped wake channel — fires on every observance so the select! loop
    /// (which can't cfg-gate arms) can wake and drain the typed receiver.
    pub wake_rx: Option<mpsc::UnboundedReceiver<()>>,
    /// Hook dispatch channel — trigger producers and reaper send hook requests;
    /// a dedicated drainer task consumes them.
    pub hook_rx: Option<mpsc::Receiver<HookDispatchRequest>>,
    /// Janet plugin event sender — installed into the plugin bridge at startup.
    /// Plugins call `(vigil/emit name data)` and the keeper routes events to
    /// the correct vigil's event queue.
    #[allow(dead_code)]
    pub vigil_plugin_tx: Option<mpsc::Sender<String>>,
    /// Whether a plugin registered `on-vigil-rite`; when true the reaper runs
    /// the synchronous System-1 gate before each observance. Set by the host
    /// after plugins load (default false).
    pub rite_gate_enabled: Arc<AtomicBool>,
    /// Whether a plugin registered `on-vigil-enrich`; when true the reaper
    /// runs the filter/enrich stage before the gate. Set by the host after
    /// plugins load (default false).
    pub enrich_enabled: Arc<AtomicBool>,
}

impl VigilKeeper {
    /// Build a vigil-keeper from config entries. Creates per-vigil channels
    /// and spawns trigger tasks.
    pub fn from_entries(
        entries: Vec<VigilEntry>,
        paused_names: std::collections::HashSet<String>,
        verdict_store: Option<crate::extras::vigil_db::VigilStore>,
    ) -> Result<Self, String> {
        let (ctl_tx, ctl_rx) = mpsc::channel::<VigilCtl>(32);
        let (obs_tx, obs_rx) = mpsc::channel::<Observance>(64);
        let (wake_tx, wake_rx) = mpsc::unbounded_channel::<()>();
        let (hook_tx, hook_rx) = mpsc::channel::<HookDispatchRequest>(64);
        let rite_gate_enabled = Arc::new(AtomicBool::new(false));
        let enrich_enabled = Arc::new(AtomicBool::new(false));

        let mut vigils = Vec::new();
        let mut reap_inputs: Vec<VigilReapInput> = Vec::new();

        for entry in entries {
            let (tx, rx) = types::make_vigil_channel(256);
            let running = Arc::new(AtomicBool::new(false));

            let name = entry.name.clone();
            if entry.reap_interval_secs == 0 {
                return Err(format!(
                    "vigil {name}: reap_interval_secs must be greater than zero"
                ));
            }
            if let crate::config::VigilTrigger::Toll { interval_secs: 0 } = &entry.trigger {
                return Err(format!(
                    "vigil {name}: toll interval_secs must be greater than zero"
                ));
            }
            let interval = entry.reap_interval_secs;
            let prompt = entry.prompt.clone();
            let procession = entry.procession.clone();

            let trigger_kind = match &entry.trigger {
                crate::config::VigilTrigger::Toll { .. } => TriggerKind::Toll,
                crate::config::VigilTrigger::Watcher { .. } => TriggerKind::Watcher,
                crate::config::VigilTrigger::Harbinger { .. } => TriggerKind::Harbinger,
            };

            // Spawn trigger(s) based on type.
            match entry.trigger {
                crate::config::VigilTrigger::Toll { interval_secs } => {
                    toll::spawn_toll(name.clone(), interval_secs, tx.clone(), hook_tx.clone());
                }
                crate::config::VigilTrigger::Watcher { path, .. } => {
                    let watch_path = std::path::PathBuf::from(&path);
                    if let Err(e) = watcher::spawn_watcher(
                        name.clone(),
                        watch_path,
                        tx.clone(),
                        hook_tx.clone(),
                    ) {
                        warn!(%name, "failed to spawn watcher, skipping vigil: {e}");
                        continue;
                    }
                }
                crate::config::VigilTrigger::Harbinger {
                    address,
                    protocol,
                    socket_mode,
                    commands,
                } => {
                    let port: u16 = address
                        .strip_prefix("127.0.0.1:")
                        .or_else(|| address.strip_prefix("localhost:"))
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(0);
                    if port == 0 {
                        return Err(format!(
                            "vigil {name}: invalid harbinger address '{address}'"
                        ));
                    }

                    if !protocol.is_empty() && protocol != "tcp" {
                        return Err(format!(
                            "vigil {name}: unsupported harbinger protocol '{protocol}' (only 'tcp' is supported)"
                        ));
                    }

                    let has_commands = matches!(socket_mode, crate::config::SocketMode::Commands);
                    if has_commands && commands.is_empty() {
                        return Err(format!(
                            "vigil {name}: commands mode requires non-empty commands map"
                        ));
                    }

                    if let Err(e) = harbinger::spawn_harbinger(
                        name.clone(),
                        port,
                        commands,
                        has_commands,
                        tx.clone(),
                        hook_tx.clone(),
                    ) {
                        warn!(%name, "failed to spawn harbinger, skipping vigil: {e}");
                        continue;
                    }
                }
            }

            let rite = entry.rite.clone();

            vigils.push(VigilInstance {
                name: name.clone(),
                reap_interval_secs: interval,
                prompt: prompt.clone(),
                procession: procession.clone(),
                tx: tx.clone(),
                running: running.clone(),
            });

            reap_inputs.push(VigilReapInput {
                name: name.clone(),
                trigger: trigger_kind,
                reap_interval_secs: interval,
                cooldown_secs: entry.cooldown_secs,
                rx,
                running,
                rite,
                prompt,
                procession,
                gate: entry.gate.clone(),
            });
        }

        // Build a map of vigil name → sender for procession chaining.
        let senders: std::collections::HashMap<String, mpsc::Sender<VigilEvent>> = vigils
            .iter()
            .map(|v| (v.name.clone(), v.tx.clone()))
            .collect();

        // Clone senders for the Janet plugin bridge router so plugins
        // calling (vigil/emit name data) can push events into any vigil's queue.
        let router_senders = senders.clone();
        let (vigil_plugin_tx, mut vigil_plugin_rx) = mpsc::channel::<String>(256);
        tokio::spawn(async move {
            while let Some(msg) = vigil_plugin_rx.recv().await {
                match msg.split_once('\t') {
                    Some((name, payload)) => {
                        if let Some(sender) = router_senders.get(name) {
                            let context: serde_json::Value = serde_json::from_str(payload)
                                .unwrap_or_else(|_| serde_json::json!({"data": payload}));
                            let event = VigilEvent {
                                vigil_name: name.to_string(),
                                trigger: crate::extras::vigil::types::TriggerKind::Toll,
                                context,
                                timestamp: chrono::Utc::now(),
                            };
                            if sender.try_send(event).is_err() {
                                warn!(%name, "vigil plugin event queue full, dropping");
                            }
                        } else {
                            warn!(%name, "vigil/emit for unknown vigil, dropping event");
                        }
                    }
                    None => {
                        warn!("vigil/emit received malformed message, dropping");
                    }
                }
            }
        });

        // Launch the reaper in a background task.
        let reaper_wake_tx = wake_tx;
        let reaper_hook_tx = hook_tx;
        let reaper_gate_flag = rite_gate_enabled.clone();
        let reaper_enrich_flag = enrich_enabled.clone();
        tokio::spawn(async move {
            let paused = paused_names;
            reaper::run_reaper(
                reap_inputs,
                obs_tx,
                ctl_rx,
                Some(reaper_wake_tx),
                senders,
                reaper_hook_tx,
                paused,
                reaper_gate_flag,
                reaper_enrich_flag,
                verdict_store,
            )
            .await;
        });

        Ok(Self {
            vigils,
            ctl_tx: Some(ctl_tx),
            observance_rx: Some(obs_rx),
            wake_rx: Some(wake_rx),
            hook_rx: Some(hook_rx),
            vigil_plugin_tx: Some(vigil_plugin_tx),
            rite_gate_enabled,
            enrich_enabled,
        })
    }

    /// Build a vigil-keeper from config entries + `.dirge/vigils/*.json` files.
    /// Filesystem entries are merged by name; config entries win on collision.
    pub fn from_config_and_filesystem(
        entries: Vec<VigilEntry>,
        paused_names: std::collections::HashSet<String>,
    ) -> Result<Self, String> {
        let mut merged = entries;

        // Scan .dirge/vigils/*.json for filesystem-defined vigils.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let vigils_dir = crate::extras::dirge_paths::ProjectPaths::new(&cwd).vigils_dir();
        #[allow(clippy::collapsible_if)]
        if vigils_dir.is_dir() {
            if let Ok(readdir) = std::fs::read_dir(&vigils_dir) {
                for entry in readdir.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    match std::fs::read_to_string(&path) {
                        Ok(content) => match serde_json::from_str::<VigilEntry>(&content) {
                            Ok(file_entry) => {
                                // Config wins — only add if not already present.
                                let name = &file_entry.name;
                                if !merged.iter().any(|e| e.name == *name) {
                                    let file_path = path.display();
                                    info!(%name, file = %file_path, "imported vigil from filesystem");
                                    merged.push(file_entry);
                                } else {
                                    info!(%name, "vigil from filesystem skipped: config entry wins on name collision");
                                }
                            }
                            Err(e) => {
                                let file_path = path.display();
                                warn!(file = %file_path, "invalid vigil JSON, skipping: {e}");
                            }
                        },
                        Err(e) => {
                            let file_path = path.display();
                            warn!(file = %file_path, "cannot read vigil file, skipping: {e}");
                        }
                    }
                }
            }
        }

        // Import vigils added via `dirge vigil add` / `/vigil add`, which are
        // persisted to the SQLite store rather than config or the filesystem
        // dir. Without this the add paths are dead ends: the keeper would never
        // load them. Config/filesystem entries win on name collision, and
        // `list_non_resting` already excludes vigils the user put to rest.
        let db_path = crate::extras::dirge_paths::ProjectPaths::new(&cwd).session_db_path();
        // Open the store for both import (below) and verdict persistence.
        // `open_at` is idempotent; on a fresh project it just creates the
        // (empty) session DB the whole app shares.
        let verdict_store = crate::extras::vigil_db::VigilStore::open_at(&db_path).ok();
        if let Some(store) = verdict_store.as_ref() {
            for row in store.list_non_resting().unwrap_or_default() {
                let name = row.name.clone();
                match serde_json::from_str::<VigilEntry>(&row.payload_json) {
                    Ok(db_entry) => {
                        if !merged.iter().any(|e| e.name == name) {
                            info!(%name, "imported vigil from store");
                            merged.push(db_entry);
                        } else {
                            info!(%name, "vigil from store skipped: config/filesystem entry wins on name collision");
                        }
                    }
                    Err(e) => {
                        warn!(%name, "invalid vigil payload in store, skipping: {e}");
                    }
                }
            }

            // DB state is authoritative even when the payload came from
            // config or a filesystem file (which win on name collision
            // above): a vigil laid to rest must not be reaped on the next
            // run. `list_non_resting` only covers store-only vigils, so
            // drop any merged entry whose DB row is resting.
            merged.retain(|e| {
                let resting = matches!(
                    store.get(&e.name),
                    Ok(Some(crate::extras::vigil_db::VigilRow {
                        status: crate::extras::vigil_db::VigilStatus::Resting,
                        ..
                    }))
                );
                if resting {
                    info!(name = %e.name, "vigil laid to rest - skipping on next run");
                }
                !resting
            });
        }

        Self::from_entries(merged, paused_names, verdict_store)
    }

    /// Signal the reaper to stop.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        if let Some(ref tx) = self.ctl_tx {
            let _ = tx.send(VigilCtl::Shutdown).await;
        }
        info!("vigil-keeper shutdown complete");
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod observance_context_tests {
    use super::observance_context;

    #[test]
    fn formats_simple_event() {
        assert_eq!(
            observance_context("jenkins-remediate", 2, "done"),
            "@{:vigil \"jenkins-remediate\" :count 2 :response \"done\" :exit :ok}"
        );
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(
            observance_context("a\"b", 1, "c\\d"),
            "@{:vigil \"a\\\"b\" :count 1 :response \"c\\\\d\" :exit :ok}"
        );
    }
}

#[cfg(all(test, feature = "plugin"))]
mod gate_verdict_tests {
    use super::types::{EnrichOutcome, GateVerdict};
    use super::{enrich_from_hook_result, verdict_from_hook_result};
    use crate::plugin::ToolHookResult;

    #[test]
    fn block_wins_over_toil() {
        let r = ToolHookResult {
            block: Some("nope".to_string()),
            mutate_input: None,
            replace_result: None,
            toil: Some(vec!["echo hi".to_string()]),
            enrich: None,
            outcome: None,
            verdict: None,
        };
        match verdict_from_hook_result(r, None) {
            GateVerdict::Shroud { reason } => assert_eq!(reason, "nope"),
            other => panic!("expected Shroud, got {other:?}"),
        }
    }

    #[test]
    fn toil_parses_into_commands() {
        let r = ToolHookResult {
            block: None,
            mutate_input: None,
            replace_result: None,
            toil: Some(vec!["echo hi".to_string()]),
            enrich: None,
            outcome: None,
            verdict: None,
        };
        match verdict_from_hook_result(r, None) {
            GateVerdict::Toil { commands } => assert_eq!(commands, vec!["echo hi".to_string()]),
            other => panic!("expected Toil, got {other:?}"),
        }
    }

    #[test]
    fn empty_result_rouses() {
        match verdict_from_hook_result(ToolHookResult::default(), None) {
            GateVerdict::Rouse => {}
            other => panic!("expected Rouse, got {other:?}"),
        }
    }

    #[test]
    fn verdict_confidence_compared_to_threshold() {
        let rouse = ToolHookResult {
            verdict: Some(r#"{"p":0.9}"#.to_string()),
            ..ToolHookResult::default()
        };
        assert!(matches!(
            verdict_from_hook_result(rouse, Some(0.8)),
            GateVerdict::Rouse
        ));

        let shroud = ToolHookResult {
            verdict: Some(r#"{"p":0.2}"#.to_string()),
            ..ToolHookResult::default()
        };
        match verdict_from_hook_result(shroud, Some(0.8)) {
            GateVerdict::Shroud { reason } => {
                assert!(reason.contains("not above threshold"), "{reason}")
            }
            other => panic!("expected Shroud, got {other:?}"),
        }

        // A verdict without a threshold (non-rite hook) keeps the default.
        let no_threshold = ToolHookResult {
            verdict: Some(r#"{"p":0.9}"#.to_string()),
            ..ToolHookResult::default()
        };
        assert!(matches!(
            verdict_from_hook_result(no_threshold, None),
            GateVerdict::Rouse
        ));
    }

    #[test]
    fn enrich_drop_wins_over_enrichment() {
        let r = ToolHookResult {
            block: Some("filtered".to_string()),
            mutate_input: None,
            replace_result: None,
            toil: None,
            enrich: Some(r#"{"author":"jane"}"#.to_string()),
            outcome: None,
            verdict: None,
        };
        match enrich_from_hook_result(r) {
            EnrichOutcome::Drop { reason } => assert_eq!(reason, "filtered"),
            other => panic!("expected Drop, got {other:?}"),
        }
    }

    #[test]
    fn enrich_passes_json_through() {
        let r = ToolHookResult {
            block: None,
            mutate_input: None,
            replace_result: None,
            toil: None,
            enrich: Some(r#"{"author":"jane"}"#.to_string()),
            outcome: None,
            verdict: None,
        };
        match enrich_from_hook_result(r) {
            EnrichOutcome::Enriched(json) => assert_eq!(json, r#"{"author":"jane"}"#),
            other => panic!("expected Enriched, got {other:?}"),
        }
    }

    #[test]
    fn enrich_empty_result_passes() {
        match enrich_from_hook_result(ToolHookResult::default()) {
            EnrichOutcome::Pass => {}
            other => panic!("expected Pass, got {other:?}"),
        }
    }
}

#[cfg(all(test, feature = "plugin"))]
mod parse_outcome_tests {
    use super::parse_outcome;

    #[test]
    fn parses_label_and_useful() {
        assert_eq!(
            parse_outcome(r#"{"label":"resolved","useful":true}"#),
            Some(("resolved".to_string(), true))
        );
    }

    #[test]
    fn useful_defaults_to_true() {
        assert_eq!(
            parse_outcome(r#"{"label":"resolved"}"#),
            Some(("resolved".to_string(), true))
        );
    }

    #[test]
    fn rejects_non_json_or_missing_label() {
        assert_eq!(parse_outcome("not json"), None);
        assert_eq!(parse_outcome(r#"{"useful":true}"#), None);
    }
}
