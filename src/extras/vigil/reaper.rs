//! Reaper — drains per-vigil event channels on configurable cadences.
//! Uses `FuturesUnordered` so each vigil reaps independently; one vigil's
//! slow observance doesn't delay another's reap.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{GateFail, VigilGate};
use crate::extras::vigil_db::VigilStore;

use super::dispatch::build_prompt;
use super::rite::evaluate_rite;
use super::types::{
    CoalescedBatch, EnrichOutcome, GateVerdict, HookResponse, RiteResult, TriggerKind, VigilEvent,
    VigilReapInput, VigilStatusInfo,
};

/// Context passed to the agent executor for an observance.
#[derive(Debug, Clone)]
pub struct Observance {
    pub vigil_name: String,
    pub prompt: String,
    pub context: serde_json::Value,
    pub event_count: usize,
    pub running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Run the reaper loop. Drains events from all active vigils, coalesces them
/// per reap window, runs rite gates, and produces `Observance`s.
/// Observances are sent to `observance_tx` for the vigil-keeper to dispatch.
#[allow(clippy::too_many_arguments)]
pub async fn run_reaper(
    vigils: Vec<VigilReapInput>,
    observance_tx: mpsc::Sender<Observance>,
    mut ctl_rx: mpsc::Receiver<super::types::VigilCtl>,
    wake_tx: Option<mpsc::UnboundedSender<()>>,
    senders: HashMap<String, mpsc::Sender<VigilEvent>>,
    hook_tx: mpsc::Sender<super::types::HookDispatchRequest>,
    initial_paused: std::collections::HashSet<String>,
    rite_gate_enabled: Arc<AtomicBool>,
    enrich_enabled: Arc<AtomicBool>,
    verdict_store: Option<VigilStore>,
) {
    type ReapTask = tokio::task::JoinHandle<(String, Vec<VigilEvent>, mpsc::Receiver<VigilEvent>)>;
    let mut reap_tasks: FuturesUnordered<ReapTask> = FuturesUnordered::new();

    // Lookup maps for metadata accessed in the reap-results arm.
    let running: HashMap<String, Arc<AtomicBool>> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.running.clone()))
        .collect();
    let prompt_map: HashMap<String, String> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.prompt.clone()))
        .collect();
    let rite_map: HashMap<String, Option<crate::config::VigilRite>> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.rite.clone()))
        .collect();
    let procession_map: HashMap<String, Option<String>> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.procession.clone()))
        .collect();
    // Infer trigger kind from the vigil config.
    let trigger_map: HashMap<String, TriggerKind> =
        vigils.iter().map(|v| (v.name.clone(), v.trigger)).collect();
    // Cost-matrix policy per vigil; a missing `gate` config falls back to
    // the default (4 : 1, fail open).
    let gate_map: HashMap<String, VigilGate> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.gate.clone().unwrap_or_default()))
        .collect();

    let mut paused: std::collections::HashSet<String> = initial_paused;

    let reap_interval_map: HashMap<String, u64> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.reap_interval_secs))
        .collect();

    // Per-vigil observance throttle: minimum seconds between observances.
    let cooldown_map: HashMap<String, u64> = vigils
        .iter()
        .map(|v| (v.name.clone(), v.cooldown_secs))
        .collect();

    let mut rxs: HashMap<String, mpsc::Receiver<VigilEvent>> = vigils
        .into_iter()
        .map(|input| (input.name.clone(), input.rx))
        .collect();

    for (name, &interval) in &reap_interval_map {
        let rx = rxs.remove(name).expect("receiver for every reap input");
        let name = name.clone();
        reap_tasks.push(tokio::spawn(async move {
            reap_interval(name, interval, rx).await
        }));
    }

    // Per-vigil reap statistics, updated each reap window and exposed via StatusReq.
    type ReapStats = HashMap<String, (usize, chrono::DateTime<chrono::Utc>)>;
    let reap_stats: Arc<Mutex<ReapStats>> = Arc::new(Mutex::new(HashMap::new()));
    // Last observance production time per vigil, for the cooldown throttle.
    let mut last_observance_at: HashMap<String, std::time::Instant> = HashMap::new();

    loop {
        tokio::select! {
            maybe_ctl = ctl_rx.recv() => {
                let ctl = match maybe_ctl {
                    Some(ctl) => ctl,
                    None => break, // ctl channel closed — keeper dropped
                };
                match ctl {
                    super::types::VigilCtl::Shutdown => {
                        info!("reaper shutting down");
                        break;
                    }
                    super::types::VigilCtl::Pause { name } => {
                        debug!(%name, "reaper pausing vigil");
                        paused.insert(name);
                    }
                    super::types::VigilCtl::PauseAll => {
                        debug!("reaper pausing all vigils");
                        for name in running.keys() {
                            paused.insert(name.clone());
                        }
                    }
                    super::types::VigilCtl::Resume { name } => {
                        debug!(%name, "reaper resuming vigil");
                        paused.remove(&name);
                    }
                    super::types::VigilCtl::ResumeAll => {
                        debug!("reaper resuming all vigils");
                        paused.clear();
                    }
                    super::types::VigilCtl::StatusReq { respond_to } => {
                        let mut statuses = Vec::new();
                        let stats = reap_stats.lock().unwrap();
                        for (name, run_flag) in &running {
                            let trigger = trigger_map.get(name).copied().unwrap_or(TriggerKind::Toll);
                            let interval = reap_interval_map.get(name).copied().unwrap_or(0);
                            let (count, ts) = stats.get(name).copied().unwrap_or((0, chrono::Utc::now()));
                            statuses.push(VigilStatusInfo {
                                name: name.clone(),
                                trigger,
                                reap_interval_secs: interval,
                                running: run_flag.load(std::sync::atomic::Ordering::Relaxed),
                                paused: paused.contains(name),
                                last_event_count: count,
                                last_event_at: Some(ts.to_rfc3339()),
                            });
                        }
                        let _ = respond_to.send(statuses);
                    }
                }
            }
            Some(result) = reap_tasks.next() => {
                match result {
                    Ok((vigil_name, mut events, rx)) => {
                        // Re-spawn this vigil's reap task so it keeps reaping on
                        // its cadence instead of stopping after the first window.
                        let interval = reap_interval_map
                            .get(&vigil_name)
                            .copied()
                            .unwrap_or(0);
                        let next_name = vigil_name.clone();
                        reap_tasks.push(tokio::spawn(async move {
                            reap_interval(next_name, interval, rx).await
                        }));

                        if events.is_empty() {
                            continue;
                        }

                        // Track event count and timestamp for the panel indicator.
                        {
                            let mut stats = reap_stats.lock().unwrap();
                            stats.insert(vigil_name.clone(), (events.len(), chrono::Utc::now()));
                        }

                        if paused.contains(&vigil_name) {
                            warn!(%vigil_name, "skipping reap — vigil paused");
                            continue;
                        }

                        // Check if an observance is already running for this vigil.
                        let run_flag = running.get(&vigil_name).cloned();
                        if let Some(ref flag) = run_flag
                            && flag.load(Ordering::SeqCst)
                        {
                            // The agent is still handling the previous observance, so
                            // don't drop these events: send them back into this
                            // vigil's own queue for the next reap window. Without
                            // this, a failure that arrives mid-turn is silently lost
                            // rather than queued.
                            warn!(%vigil_name, "observance in flight — re-queueing {} event(s)", events.len());
                            if let Some(back) = senders.get(&vigil_name) {
                                for event in events {
                                    if back.try_send(event).is_err() {
                                        warn!(%vigil_name, "re-queue dropped — event queue full");
                                        break;
                                    }
                                }
                            }
                            continue;
                        }

                        // Cooldown throttle: after an observance is produced,
                        // suppress further wakes until the vigil's cooldown
                        // elapses. Re-queue events so a still-flapping alarm
                        // coalesces into the next window instead of being lost.
                        let cooldown = cooldown_map.get(&vigil_name).copied().unwrap_or(0);
                        if within_cooldown(last_observance_at.get(&vigil_name), cooldown) {
                            warn!(%vigil_name, cooldown, "observance in cooldown — re-queueing {} event(s)", events.len());
                            if let Some(back) = senders.get(&vigil_name) {
                                for event in events {
                                    if back.try_send(event).is_err() {
                                        warn!(%vigil_name, "re-queue dropped — event queue full");
                                        break;
                                    }
                                }
                            }
                            continue;
                        }

                        let trigger = trigger_map
                            .get(&vigil_name)
                            .copied()
                            .unwrap_or(TriggerKind::Toll);

                        // Dispatch on-vigil-reap hook pre-rite.
                        let reap_ctx = format!(
                            "@{{:vigil \"{}\" :event_count {} :trigger :{}}}",
                            vigil_name,
                            events.len(),
                            trigger.as_str()
                        );
                        let _ = hook_tx.try_send(
                            super::types::HookDispatchRequest {
                                hook_name: "on-vigil-reap".into(),
                                context: reap_ctx,
                                respond_to: None,
                                threshold: None,
                            },
                        );

                        // Rite gate check — skip observance if the rite fails.
                        let (rite_output, rite_exit_code) =
                            if let Some(Some(rite)) = rite_map.get(&vigil_name) {
                                match evaluate_rite(rite).await {
                                    RiteResult::Pass { output, exit_code } => {
                                        (output, exit_code)
                                    }
                                    RiteResult::Fail { reason } => {
                                        warn!(%vigil_name, %reason, "rite gate failed, skipping observance");
                                        continue;
                                    }
                                }
                            } else {
                                (None, None)
                            };

                        // Filter/enrich stage — a synchronous plugin predicate
                        // (`on-vigil-enrich`) that runs before the rite gate.
                        // It can drop the event outright (filter) or
                        // shallow-merge a JSON object into the event context
                        // (enrich) so both the gate and the observance prompt
                        // see the enriched payload.
                        if enrich_enabled.load(Ordering::Relaxed) {
                            let enrich_ctx = enrich_context(&vigil_name, trigger, &events);
                            let (enrich_tx, enrich_rx) =
                                tokio::sync::oneshot::channel::<HookResponse>();
                            let enrich_req = super::types::HookDispatchRequest {
                                hook_name: "on-vigil-enrich".into(),
                                context: enrich_ctx,
                                respond_to: Some(enrich_tx),
                                threshold: None,
                            };

                            let outcome = if hook_tx.send(enrich_req).await.is_err() {
                                EnrichOutcome::Pass
                            } else {
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    enrich_rx,
                                )
                                .await
                                {
                                    Ok(Ok(HookResponse::Enrich(outcome))) => outcome,
                                    Ok(Ok(_)) => EnrichOutcome::Pass,
                                    Ok(Err(_)) | Err(_) => EnrichOutcome::Pass,
                                }
                            };

                            match outcome {
                                EnrichOutcome::Drop { reason } => {
                                    warn!(%vigil_name, %reason, "on-vigil-enrich dropped event, skipping observance");
                                    continue;
                                }
                                EnrichOutcome::Enriched(json) => {
                                    if let Err(e) = merge_enrichment(&mut events, &json) {
                                        warn!(%vigil_name, %e, "on-vigil-enrich returned invalid enrichment, ignoring");
                                    }
                                }
                                EnrichOutcome::Pass => {}
                            }
                        }

                        // System-1 rite gate — a synchronous plugin predicate
                        // (e.g. the lev sidecar). Only runs when a plugin has
                        // registered `on-vigil-rite`. The verdict is typed:
                        // Shroud skips the observance, Rouse wakes the agent,
                        // Toil runs shell commands directly. A gate failure
                        // (drainer gone, plugin error, timeout) is resolved by
                        // the vigil's fail posture — default open, matching the
                        // previous behavior.
                        if rite_gate_enabled.load(Ordering::Relaxed) {
                            let gate = gate_map.get(&vigil_name).cloned().unwrap_or_default();
                            let threshold = gate.threshold();
                            let rite_ctx = rite_context(&vigil_name, trigger, &events, threshold);
                            let (gate_tx, gate_rx) =
                                tokio::sync::oneshot::channel::<HookResponse>();
                            let gate_req = super::types::HookDispatchRequest {
                                hook_name: "on-vigil-rite".into(),
                                context: rite_ctx,
                                respond_to: Some(gate_tx),
                                threshold: Some(threshold),
                            };

                            let verdict = if hook_tx.send(gate_req).await.is_err() {
                                fail_verdict(&gate, "on-vigil-rite drainer gone", &vigil_name, &verdict_store)
                            } else {
                                // Must stay above the hook budget (HOOK_TIMEOUT
                                // 5 s + INTERRUPT_GRACE 2 s) so a completed
                                // verdict is never lost to this outer timeout.
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    gate_rx,
                                )
                                .await
                                {
                                    Ok(Ok(HookResponse::Verdict(verdict))) => verdict,
                                    Ok(Ok(HookResponse::Enrich(_))) => {
                                        fail_verdict(&gate, "on-vigil-rite got an enrich reply", &vigil_name, &verdict_store)
                                    }
                                    Ok(Err(_)) => {
                                        fail_verdict(&gate, "on-vigil-rite responder dropped", &vigil_name, &verdict_store)
                                    }
                                    Err(_) => {
                                        fail_verdict(&gate, "on-vigil-rite gate timed out", &vigil_name, &verdict_store)
                                    }
                                }
                            };

                            persist_verdict(
                                &verdict_store,
                                &vigil_name,
                                trigger,
                                threshold,
                                &verdict,
                            );

                            match verdict {
                                GateVerdict::Shroud { reason } => {
                                    warn!(%vigil_name, %reason, "on-vigil-rite gate blocked, skipping observance");
                                    continue;
                                }
                                GateVerdict::Rouse => {}
                                GateVerdict::Toil { commands } => {
                                    for command in commands {
                                        info!(%vigil_name, %command, "gate toil dispatch");
                                        match tokio::process::Command::new("sh")
                                            .arg("-c")
                                            .arg(&command)
                                            .output()
                                            .await
                                        {
                                            Ok(output) => {
                                                let stdout =
                                                    String::from_utf8_lossy(&output.stdout);
                                                let code = output.status.code().unwrap_or(-1);
                                                info!(
                                                    %vigil_name,
                                                    exit_code = code,
                                                    stdout = %stdout.trim(),
                                                    "gate toil finished"
                                                );
                                            }
                                            Err(e) => {
                                                warn!(%vigil_name, %command, "gate toil failed: {e}");
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }
                        }

                        // Commands-mode harbinger: execute the resolved shell
                        // command directly — no agent turn, no LLM cost. The rite
                        // gate above still applies.
                        if let Some(commands) = extract_resolved_commands(&events) {
                            for command in commands {
                                info!(%vigil_name, %command, "commands-mode dispatch");
                                match tokio::process::Command::new("sh")
                                    .arg("-c")
                                    .arg(&command)
                                    .output()
                                    .await
                                {
                                    Ok(output) => {
                                        let stdout =
                                            String::from_utf8_lossy(&output.stdout);
                                        let code = output.status.code().unwrap_or(-1);
                                        info!(
                                            %vigil_name,
                                            exit_code = code,
                                            stdout = %stdout.trim(),
                                            "commands-mode dispatch finished"
                                        );
                                    }
                                    Err(e) => {
                                        warn!(%vigil_name, %command, "commands-mode dispatch failed: {e}");
                                    }
                                }
                            }
                            continue;
                        }

                        let batch = CoalescedBatch::from_events(
                            vigil_name.clone(),
                            trigger,
                            &events,
                            rite_output,
                            rite_exit_code,
                        );

                        let prompt_template = prompt_map
                            .get(&vigil_name)
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let prompt = if prompt_template.is_empty() {
                            String::new()
                        } else {
                            build_prompt(prompt_template, &batch)
                        };

                        // Skip if the prompt still has unresolved {placeholders}
                        // — happens when the batch has only toll ticks and the
                        // template expects plugin-emitted context (job, etc.).
                        // Use a regex to match only template-variable patterns like
                        // {job} or {build_number}, not JSON object braces from
                        // substituted {harbinger_data} values.
                        if !prompt.is_empty() {
                            static RE: std::sync::LazyLock<regex::Regex> =
                                std::sync::LazyLock::new(|| {
                                    regex::Regex::new(r"\{[a-zA-Z_][a-zA-Z0-9_]*\}").unwrap()
                                });
                            if RE.is_match(&prompt) {
                                warn!(%vigil_name, "skipping observance — prompt has unresolved placeholders");
                                continue;
                            }
                        }

                        let context = coalesce_events(&events);

                        // Mark in-flight so overlapping reaps for this vigil are skipped.
                        if let Some(ref flag) = run_flag {
                            flag.store(true, Ordering::SeqCst);
                        }

                        let running_flag = run_flag.unwrap_or_else(|| {
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
                        });

                        let observance = Observance {
                            vigil_name: vigil_name.clone(),
                            prompt,
                            context,
                            event_count: batch.event_count,
                            running: running_flag.clone(),
                        };

                        if observance_tx.try_send(observance).is_err() {
                            // The flag was set above; clear it so this vigil can
                            // reap again instead of being stuck "in flight" forever.
                            running_flag.store(false, Ordering::SeqCst);
                            warn!(%vigil_name, "observance queue full, dropping");
                        } else {
                            // Start the cooldown clock once the observance is
                            // actually delivered, so a dropped observance
                            // (queue full) doesn't throttle the next window.
                            last_observance_at.insert(vigil_name.clone(), std::time::Instant::now());
                            if let Some(ref wt) = wake_tx {
                                let _ = wt.send(());
                            }
                        }

                        // Procession: inject event into next vigil's queue.
                        if let Some(Some(next_name)) = procession_map.get(&vigil_name) {
                            if let Some(next_tx) = senders.get(next_name) {
                                let chain_event = VigilEvent {
                                    vigil_name: next_name.clone(),
                                    trigger: TriggerKind::Toll,
                                    context: serde_json::json!({
                                        "procession_from": vigil_name,
                                        "event_count": batch.event_count,
                                    }),
                                    timestamp: chrono::Utc::now(),
                                };
                                if next_tx.try_send(chain_event).is_err() {
                                    warn!(%next_name, from=%vigil_name,
                                        "procession queue full for next vigil");
                                } else {
                                    debug!(%next_name, from=%vigil_name,
                                        "procession: injected event into next vigil");
                                }
                            } else {
                                warn!(%next_name, from=%vigil_name,
                                    "procession target not found among active vigils");
                            }
                        }
                    }
                    Err(e) => {
                        warn!("reap task panicked: {e}");
                    }
                }
            }
        }
    }
}

async fn reap_interval(
    name: String,
    interval_secs: u64,
    mut rx: mpsc::Receiver<VigilEvent>,
) -> (String, Vec<VigilEvent>, mpsc::Receiver<VigilEvent>) {
    let mut events: Vec<VigilEvent> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(interval_secs);

    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(event)) => {
                events.push(event);
                // Drain any additional events without blocking.
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
            }
            Ok(None) => break, // Channel closed.
            Err(_) => break,   // Timeout — reap window elapsed.
        }
    }

    (name, events, rx)
}

fn coalesce_events(events: &[VigilEvent]) -> serde_json::Value {
    if events.len() == 1 {
        return events[0].context.clone();
    }

    let mut files: Vec<String> = Vec::new();
    let mut payloads: Vec<serde_json::Value> = Vec::new();

    for event in events {
        if let Some(fs) = event.context.get("files").and_then(|v| v.as_array()) {
            for f in fs {
                if let Some(s) = f.as_str() {
                    files.push(s.to_string());
                }
            }
        }
        payloads.push(event.context.clone());
    }

    serde_json::json!({
        "events": payloads,
        "files": files,
        "event_count": events.len(),
    })
}

/// True when the vigil's cooldown throttle should suppress an observance:
/// a cooldown is configured and the last observance was produced fewer than
/// `cooldown_secs` ago. `last` is `None` on the first reap, so it never
/// throttles a cold vigil.
fn within_cooldown(last: Option<&std::time::Instant>, cooldown_secs: u64) -> bool {
    match last {
        Some(last) if cooldown_secs > 0 => {
            last.elapsed() < std::time::Duration::from_secs(cooldown_secs)
        }
        _ => false,
    }
}

/// Build the Janet context string for the `on-vigil-rite` hook. Carries the
/// coalesced event payload as a JSON string so a gate plugin (e.g. lev) can
/// judge the full observance state, plus the derived wake threshold so the
/// plugin applies the vigil's cost-matrix policy instead of its own magic
/// number.
fn rite_context(
    vigil_name: &str,
    trigger: TriggerKind,
    events: &[VigilEvent],
    threshold: f64,
) -> String {
    let payload =
        serde_json::to_string(&coalesce_events(events)).unwrap_or_else(|_| "{}".to_string());
    let escaped_name = vigil_name.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_payload = payload.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "@{{:vigil \"{}\" :trigger :{} :event_count {} :payload \"{}\" :threshold {}}}",
        escaped_name,
        trigger.as_str(),
        events.len(),
        escaped_payload,
        threshold,
    )
}

/// Build the Janet context string for the `on-vigil-enrich` hook. Same
/// payload as the rite gate, minus the threshold — enrichment runs before
/// the gate, so no cost-matrix policy is in scope yet.
fn enrich_context(vigil_name: &str, trigger: TriggerKind, events: &[VigilEvent]) -> String {
    let payload =
        serde_json::to_string(&coalesce_events(events)).unwrap_or_else(|_| "{}".to_string());
    let escaped_name = vigil_name.replace('\\', "\\\\").replace('"', "\\\"");
    let escaped_payload = payload.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "@{{:vigil \"{}\" :trigger :{} :event_count {} :payload \"{}\"}}",
        escaped_name,
        trigger.as_str(),
        events.len(),
        escaped_payload,
    )
}

/// Shallow-merge a plugin-supplied JSON object into every event's context.
/// Enrichment keys win over existing trigger fields (the plugin is the
/// authoritative new context). Returns an error when `json` is not a JSON
/// object so the caller can log and ignore it.
fn merge_enrichment(events: &mut [VigilEvent], json: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let serde_json::Value::Object(enrichment) = value else {
        return Err("enrichment must be a JSON object".to_string());
    };
    for event in events {
        if let serde_json::Value::Object(ctx) = &mut event.context {
            for (key, val) in &enrichment {
                ctx.insert(key.clone(), val.clone());
            }
        } else {
            // Non-object context: wrap it so enrichment keys stay visible to
            // the prompt template's {key} substitution.
            let mut map = serde_json::Map::new();
            map.insert("context".to_string(), event.context.clone());
            for (key, val) in &enrichment {
                map.insert(key.clone(), val.clone());
            }
            event.context = serde_json::Value::Object(map);
        }
    }
    Ok(())
}

/// Resolve a gate failure into a verdict using the vigil's fail posture.
/// `open` wakes; `closed` skips; `prior` wakes iff the vigil's empirical
/// positive rate over the trailing week exceeds the wake threshold. A vigil
/// with no recorded outcomes yet is uncalibrated and falls back to waking.
fn fail_verdict(
    gate: &VigilGate,
    reason: &str,
    vigil_name: &str,
    store: &Option<VigilStore>,
) -> GateVerdict {
    match gate.fail {
        GateFail::Open => GateVerdict::Rouse,
        GateFail::Closed => GateVerdict::Shroud {
            reason: reason.to_string(),
        },
        GateFail::Prior => {
            let threshold = gate.threshold();
            let prior = store
                .as_ref()
                .and_then(|s| s.positive_rate(vigil_name).ok().flatten());
            prior_verdict(threshold, prior, reason)
        }
    }
}

/// Resolve a `GateFail::Prior` posture: wake only when the empirical positive
/// rate exceeds the wake threshold. `None` (uncalibrated) wakes.
fn prior_verdict(threshold: f64, rate: Option<f64>, reason: &str) -> GateVerdict {
    match rate {
        Some(r) if r > threshold => GateVerdict::Rouse,
        Some(_) => GateVerdict::Shroud {
            reason: format!("{reason}; prior positive rate below threshold"),
        },
        None => GateVerdict::Rouse,
    }
}

/// Best-effort write of a gate verdict to the vigil store. A missing store
/// (no session DB) or a write failure is logged, never fatal to the reap.
fn persist_verdict(
    store: &Option<VigilStore>,
    vigil: &str,
    trigger: TriggerKind,
    threshold: f64,
    verdict: &GateVerdict,
) {
    let Some(store) = store else { return };
    let (reason, commands) = match verdict {
        GateVerdict::Shroud { reason } => (Some(reason.clone()), None),
        GateVerdict::Rouse => (None, None),
        GateVerdict::Toil { commands } => (None, serde_json::to_string(commands).ok()),
    };
    if let Err(e) = store.record_verdict(
        vigil,
        trigger.as_str(),
        verdict.as_str(),
        reason.as_deref(),
        commands.as_deref(),
        Some(threshold),
    ) {
        warn!(%vigil, "failed to persist vigil verdict: {e}");
    }
}

/// Extract the resolved shell commands from a commands-mode harbinger batch.
/// Returns `None` when no event carries a resolved dispatch (template mode).
fn extract_resolved_commands(events: &[VigilEvent]) -> Option<Vec<String>> {
    let mut commands = Vec::with_capacity(events.len());
    let mut is_commands_batch = false;

    for event in events {
        if let Some(args) = event.context.get("_resolved_args") {
            is_commands_batch = true;
            match args.get("command").and_then(|c| c.as_str()) {
                Some(command) => commands.push(command.to_string()),
                None => warn!("commands-mode event missing resolved command"),
            }
        }
    }

    if is_commands_batch {
        Some(commands)
    } else {
        None
    }
}

#[cfg(test)]
mod cooldown_tests {
    use super::within_cooldown;

    #[test]
    fn no_last_observance_never_throttles() {
        assert!(!within_cooldown(None, 60));
    }

    #[test]
    fn zero_cooldown_never_throttles() {
        let now = std::time::Instant::now();
        assert!(!within_cooldown(Some(&now), 0));
    }

    #[test]
    fn throttles_within_window() {
        let now = std::time::Instant::now();
        assert!(within_cooldown(Some(&now), 60));
    }

    #[test]
    fn does_not_throttle_after_window() {
        let past = std::time::Instant::now() - std::time::Duration::from_secs(61);
        assert!(!within_cooldown(Some(&past), 60));
    }
}

#[cfg(test)]
mod prior_verdict_tests {
    use super::prior_verdict;
    use crate::extras::vigil::types::GateVerdict;

    #[test]
    fn uncalibrated_prior_rouses() {
        assert!(matches!(
            prior_verdict(0.8, None, "oracle down"),
            GateVerdict::Rouse
        ));
    }

    #[test]
    fn prior_above_threshold_rouses() {
        assert!(matches!(
            prior_verdict(0.8, Some(0.9), "oracle down"),
            GateVerdict::Rouse
        ));
    }

    #[test]
    fn prior_below_threshold_shrouds() {
        match prior_verdict(0.8, Some(0.25), "oracle down") {
            GateVerdict::Shroud { reason } => {
                assert!(reason.contains("prior positive rate below threshold"))
            }
            other => panic!("expected Shroud, got {other:?}"),
        }
    }
}
