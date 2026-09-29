//! `type: "addon"` command hooks: the entry's addon answers through the
//! handler it registered under `:dirge/command-hooks`, in the host running
//! now. The answer is read as the process it stands in for
//! ([`policy::addon_answer`]), so `policy::interpret` decodes both kinds of
//! entry alike. Every way of getting no answer fails open: no verdict, the
//! action allowed.
//!
//! An event dirge does not fire itself reaches addons with no entry at all:
//! each addon that registered the open hook key [`event_key`] answers it,
//! read the same way.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use super::host::AddonHost;
use crate::agent::command_hooks::boundary::{HookListener, HookRunner};
use crate::agent::command_hooks::domain::{DEFAULT_TIMEOUT_SECS, Exited, HookCommand, HookError};
use crate::agent::command_hooks::policy;

/// Adapter: answers addon entries from the process-wide addon host.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveAddonHookRunner;

impl HookRunner for LiveAddonHookRunner {
    fn run(
        &self,
        cmd: &HookCommand,
        payload: &str,
        _project_dir: &Path,
    ) -> Result<Exited, HookError> {
        let (addon, handler) = cmd
            .addon_target()
            .ok_or_else(|| HookError::SpawnFailed("not an addon hook entry".to_string()))?;
        let host = super::global()
            .ok_or_else(|| HookError::SpawnFailed("the addon host is not running".to_string()))?;
        let payload: Value = serde_json::from_str(payload)
            .map_err(|e| HookError::SpawnFailed(format!("hook payload is not JSON: {e}")))?;
        let (addon, handler) = (addon.to_string(), handler.to_string());
        let call = move || host.run_hook_handler(&addon, &handler, &payload);
        bounded(cmd.timeout_secs(), call)?
            .map(|value| policy::addon_answer(&value))
            .map_err(failed)
    }
}

/// The hook key an addon registers to hear command-hook event `event`,
/// e.g. `:dirge.hook/Notification`.
pub fn event_key(event: &str) -> String {
    format!("dirge.hook/{event}")
}

/// Every addon's answer to `event`, each read as a command's.
pub fn answers(host: &AddonHost, event: &str, payload: &Value) -> Vec<Result<Exited, HookError>> {
    host.emit(&event_key(event), payload)
        .into_iter()
        .map(|reply| {
            reply
                .result
                .map(|value| policy::addon_answer(&value))
                .map_err(failed)
        })
        .collect()
}

/// Adapter: open events to the process-wide addon host.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveAddonHookListener;

impl HookListener for LiveAddonHookListener {
    fn listens(&self, event: &str) -> bool {
        super::global().is_some_and(|host| host.listens_key(&event_key(event)))
    }

    fn hear(&self, event: &str, payload: &str) -> Vec<Result<Exited, HookError>> {
        let Some(host) = super::global() else {
            return Vec::new();
        };
        let payload: Value = match serde_json::from_str(payload) {
            Ok(payload) => payload,
            Err(e) => {
                return vec![Err(HookError::SpawnFailed(format!(
                    "hook payload is not JSON: {e}"
                )))];
            }
        };
        let event = event.to_string();
        bounded(DEFAULT_TIMEOUT_SECS, move || {
            answers(&host, &event, &payload)
        })
        .unwrap_or_else(|timed_out| vec![Err(timed_out)])
    }
}

fn failed(stderr: String) -> HookError {
    HookError::NonZeroExit { code: None, stderr }
}

/// `call`'s answer, or `TimedOut` after `secs`. On the event-loop thread it
/// runs in place: the isolate bounds that caller itself and refuses it
/// anything that waits on the loop (an MCP call), so it cannot hang it.
fn bounded<T: Send + 'static>(
    secs: u64,
    call: impl FnOnce() -> T + Send + 'static,
) -> Result<T, HookError> {
    if super::cljrs::isolate::on_event_loop_thread() {
        return Ok(call());
    }
    within(Duration::from_secs(secs), call).ok_or(HookError::TimedOut(secs))
}

/// `work`'s answer, or `None` when it has none within `budget`. The work
/// runs on its own thread, outside any async runtime, and is left to finish
/// on its own when it overruns.
fn within<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("dirge-addon-hook".into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.recv_timeout(budget).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_answers_in_time_and_gives_up_after() {
        assert_eq!(within(Duration::from_secs(2), || 7), Some(7));
        let slow = within(Duration::from_millis(20), || {
            std::thread::sleep(Duration::from_millis(500));
            7
        });
        assert_eq!(slow, None);
    }

    #[test]
    fn without_a_host_an_addon_entry_fails_open() {
        let cmd: HookCommand =
            serde_json::from_str(r#"{"type": "addon", "addon": "a", "handler": "h"}"#).unwrap();
        if super::super::global().is_none() {
            assert!(matches!(
                LiveAddonHookRunner.run(&cmd, "{}", Path::new(".")),
                Err(HookError::SpawnFailed(_))
            ));
        }
    }
}
