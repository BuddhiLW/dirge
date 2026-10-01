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
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use super::host::AddonHost;
use crate::agent::command_hooks::boundary::{HookListener, HookRunner};
use crate::agent::command_hooks::domain::{DEFAULT_TIMEOUT_SECS, Exited, HookCommand, HookError};
use crate::agent::command_hooks::policy;

/// Port: runs one addon's command-hook handler.
pub trait HookHandlers: Send + Sync {
    /// `handler` of `addon` on a hook's JSON `payload`. `Ok` carries the
    /// handler's answer, `Err` why there is none.
    fn run_hook_handler(
        &self,
        addon: &str,
        handler: &str,
        payload: &Value,
    ) -> Result<Value, String>;
}

impl HookHandlers for super::host::AddonHost {
    fn run_hook_handler(
        &self,
        addon: &str,
        handler: &str,
        payload: &Value,
    ) -> Result<Value, String> {
        super::host::AddonHost::run_hook_handler(self, addon, handler, payload)
    }
}

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
        let host = super::global().map(|host| host as Arc<dyn HookHandlers>);
        answer(host, cmd, payload)
    }
}

/// `cmd`'s answer from `handlers`, read as the process it stands in for.
fn answer(
    handlers: Option<Arc<dyn HookHandlers>>,
    cmd: &HookCommand,
    payload: &str,
) -> Result<Exited, HookError> {
    let (addon, handler) = cmd
        .addon_target()
        .ok_or_else(|| HookError::SpawnFailed("not an addon hook entry".to_string()))?;
    let handlers = handlers
        .ok_or_else(|| HookError::SpawnFailed("the addon host is not running".to_string()))?;
    let payload: Value = serde_json::from_str(payload)
        .map_err(|e| HookError::SpawnFailed(format!("hook payload is not JSON: {e}")))?;
    let (addon, handler) = (addon.to_string(), handler.to_string());
    let call = move || handlers.run_hook_handler(&addon, &handler, &payload);
    bounded(cmd.timeout_secs(), call)?
        .map(|value| policy::addon_answer(&value))
        .map_err(failed)
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
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    use serde_json::json;

    use crate::agent::agent_loop::hooks::{OpenRunFn, RunOpening};
    use crate::agent::command_hooks::CommandHooks;
    use crate::agent::command_hooks::domain::Submission;
    use crate::agent::command_hooks::loop_hooks;

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

    /// The addon runner over `handlers` instead of the process-wide host.
    struct Answering(Arc<dyn HookHandlers>);

    impl HookRunner for Answering {
        fn run(&self, cmd: &HookCommand, payload: &str, _: &Path) -> Result<Exited, HookError> {
            answer(Some(self.0.clone()), cmd, payload)
        }
    }

    /// A guard whose handlers judge through an MCP call: refused on the
    /// event-loop thread, as the isolate refuses one there; elsewhere the
    /// call blocks for `takes`, then answers. Records each handler run and
    /// whether it ran on the event loop.
    struct McpJudge {
        takes: Duration,
        seen: Mutex<Vec<(String, bool)>>,
    }

    impl McpJudge {
        fn taking(takes: Duration) -> Arc<Self> {
            Arc::new(Self {
                takes,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn seen(&self) -> Vec<(String, bool)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl HookHandlers for McpJudge {
        fn run_hook_handler(&self, _: &str, handler: &str, _: &Value) -> Result<Value, String> {
            let on_loop = crate::addons::cljrs::isolate::on_event_loop_thread();
            self.seen
                .lock()
                .unwrap()
                .push((handler.to_string(), on_loop));
            if on_loop {
                return Err("mcp-call is unavailable while dirge waits on the addon".into());
            }
            std::thread::sleep(self.takes);
            Ok(match handler {
                "prompt" => json!({"exit": 2, "stderr": "judged: no secrets"}),
                other => json!(format!("judged {other}")),
            })
        }
    }

    /// A registry whose three start events each run one `guard` handler,
    /// under a one-second entry timeout.
    fn registry(judge: Arc<McpJudge>) -> Arc<CommandHooks> {
        let entry = |handler: &str| json!([{"hooks": [{"type": "addon", "addon": "guard", "handler": handler, "timeout": 1}]}]);
        let events = serde_json::from_value(json!({
            "SessionStart": entry("session"),
            "UserPromptSubmit": entry("prompt"),
            "SubagentStart": entry("subagent"),
        }))
        .unwrap();
        let runner = Arc::new(Answering(judge));
        Arc::new(CommandHooks::new(events, PathBuf::from("."), runner))
    }

    fn opening() -> RunOpening {
        RunOpening {
            system_prompt: "sys".into(),
            prompt: "my password is x".into(),
            reminders: Vec::new(),
            refusal: None,
        }
    }

    /// `open` run from a thread marked as dirge's event loop, on a
    /// current-thread runtime like the loop's own; with how long it took.
    fn open_from_the_event_loop(open: OpenRunFn) -> (RunOpening, Duration) {
        std::thread::spawn(move || {
            crate::addons::cljrs::isolate::mark_event_loop_thread();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let started = Instant::now();
            let opened = runtime.block_on(open(opening()));
            (opened, started.elapsed())
        })
        .join()
        .unwrap()
    }

    #[test]
    fn start_hooks_are_judged_off_the_event_loop() {
        let judge = McpJudge::taking(Duration::from_millis(50));
        let hooks = registry(judge.clone());

        let main = loop_hooks::open_run(hooks.clone(), Some("s".into()), false).expect("hooked");
        let (opened, _) = open_from_the_event_loop(main);
        assert!(
            opened.system_prompt.contains("judged session"),
            "{opened:?}"
        );
        let refusal = opened.refusal.unwrap_or_default();
        assert!(refusal.contains("judged: no secrets"), "{refusal}");

        let child = loop_hooks::subagent_open_run(hooks, "child-1".into()).expect("hooked");
        let (opened, _) = open_from_the_event_loop(child);
        assert!(
            opened.system_prompt.contains("judged subagent"),
            "{opened:?}"
        );

        let off_loop = |name: &str| (name.to_string(), false);
        let expected = vec![
            off_loop("session"),
            off_loop("prompt"),
            off_loop("subagent"),
        ];
        assert_eq!(judge.seen(), expected);
    }

    #[test]
    fn run_inline_on_the_event_loop_a_start_hook_goes_unjudged() {
        let judge = McpJudge::taking(Duration::ZERO);
        let hooks = registry(judge.clone());
        let submitted = std::thread::spawn(move || {
            crate::addons::cljrs::isolate::mark_event_loop_thread();
            loop_hooks::submitted_prompt(&hooks, Some("s"), "my password is x".into())
        })
        .join()
        .unwrap();
        assert!(matches!(submitted, Submission::Proceed(_)), "{submitted:?}");
        assert_eq!(judge.seen(), vec![("prompt".to_string(), true)]);
    }

    #[test]
    fn a_start_hook_past_its_timeout_fails_open() {
        let judge = McpJudge::taking(Duration::from_secs(5));
        let hooks = registry(judge);

        let main = loop_hooks::open_run(hooks.clone(), Some("s".into()), false).expect("hooked");
        let (opened, took) = open_from_the_event_loop(main);
        assert_eq!(opened, opening(), "the run opens as if unhooked");
        assert!(
            took < Duration::from_secs(4),
            "two 1s entries took {took:?}"
        );

        let child = loop_hooks::subagent_open_run(hooks, "child-1".into()).expect("hooked");
        let (opened, took) = open_from_the_event_loop(child);
        assert_eq!(opened, opening());
        assert!(took < Duration::from_secs(3), "one 1s entry took {took:?}");
    }
}
