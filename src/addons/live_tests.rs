//! Acceptance tests for changing addons while they run: `refresh!`, the
//! REPL, open hook keys and the `:dirge/event` stream, against the `live`
//! fixture addon on a real isolate.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::cljrs::isolate::IsolateOptions;
use super::discovery;
use super::domain::{AddonPlan, ReloadReport};
use super::host::AddonHost;
use super::port::{Harness, HarnessSink, Level};

#[derive(Default)]
struct QuietSink(Mutex<Vec<String>>);

impl HarnessSink for QuietSink {
    fn notify(&self, _level: Level, message: &str) {
        self.0.lock().unwrap().push(message.to_string());
    }
}

const PROTOCOL: &str = "fixture.addon-protocol";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/addons")
}

fn live_plan(addons: &Path) -> AddonPlan {
    discovery::plan(
        &[addons.to_path_buf()],
        &[fixtures().join("protocol/src")],
        &super::layout::default_manifest_dirs(),
    )
}

fn live_host(options: IsolateOptions) -> AddonHost {
    let host = super::start_with(
        live_plan(&fixtures().join("live")),
        Harness::with_sink(Arc::new(QuietSink::default())),
        PROTOCOL,
        options,
    )
    .expect("host starts");
    assert!(host.failures().is_empty(), "{:?}", host.failures());
    host
}

fn tool_text(host: &AddonHost, name: &str, args: Value) -> String {
    let tool = host
        .tools()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no tool {name}"));
    let (content, _) = host.call_tool(&tool, &args).expect("tool runs");
    content[0]["text"].as_str().unwrap_or_default().to_string()
}

fn tool_names(host: &AddonHost) -> Vec<String> {
    host.tools().into_iter().map(|t| t.name).collect()
}

/// The next in-place change the host takes in, waiting up to five seconds:
/// the isolate re-reads the addons just after the call that asked returns.
fn next_sync(host: &AddonHost) -> ReloadReport {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(report) = host.sync() {
            return report;
        }
        assert!(Instant::now() < deadline, "no refresh within 5s");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_tool_that_asks_for_a_refresh_offers_its_new_tool_without_a_reload() {
    let host = live_host(IsolateOptions::default());
    assert_eq!(tool_names(&host), vec!["grow", "heard"]);
    assert!(host.sync().is_none(), "nothing re-read yet");

    assert_eq!(
        tool_text(&host, "grow", json!({"name": "wave"})),
        "grew wave"
    );
    let report = next_sync(&host);

    assert_eq!(report.tools_added, vec!["wave".to_string()]);
    assert!(report.tools_removed.is_empty());
    assert_eq!(tool_names(&host), vec!["grow", "heard", "wave"]);
    assert_eq!(tool_text(&host, "wave", json!({})), "hello from wave");
}

#[test]
fn open_hook_keys_reach_addons_by_name() {
    let host = live_host(IsolateOptions::default());
    let keys: Vec<String> = host
        .hook_keys()
        .into_iter()
        .find(|(id, _)| id == "live")
        .map(|(_, keys)| keys)
        .unwrap_or_default();
    assert!(keys.contains(&"acme/ping".to_string()), "{keys:?}");
    assert!(keys.contains(&"dirge/event".to_string()), "{keys:?}");

    assert!(host.listens_key("acme/ping"));
    assert!(!host.listens_key("acme/other"));
    let replies = host.emit("acme/ping", &json!({"n": 3}));
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].addon_id, "live");
    assert_eq!(replies[0].result, Ok(json!("pong 3")));
    assert!(host.emit("acme/other", &json!({})).is_empty());
}

/// A command-hook event dirge has no variant for reaches the addon that
/// registered `:dirge.hook/<Event>`, and its answer reads as a command's.
#[test]
fn an_open_command_hook_event_reaches_the_addon_keyed_on_it() {
    use crate::agent::command_hooks::domain::HookEvent;
    use crate::agent::command_hooks::policy;

    let host = live_host(IsolateOptions::default());
    let event = HookEvent::named("Notification");
    let payload = json!({"hook_event_name": "Notification", "message": "idle"});

    let answers = super::command_hooks::answers(&host, event.as_str(), &payload);

    assert_eq!(answers.len(), 1, "{answers:?}");
    let exited = answers[0].clone().expect("the addon answers");
    let outcome = policy::interpret(event, exited).expect("a verdict");
    assert_eq!(outcome.context, vec!["live heard idle".to_string()]);
    assert!(super::command_hooks::answers(&host, "PreCompact", &json!({})).is_empty());
}

#[test]
fn compaction_hooks_reach_a_running_addon() {
    use super::domain::HookPoint;
    use crate::agent::compression::validate_summary;

    let host = live_host(IsolateOptions::default());
    assert!(host.listens(HookPoint::Compact));
    assert!(host.listens(HookPoint::BeforeCompact));

    host.before_compact(&json!({"count": 2, "tokens": 100, "reason": "pressure"}));
    assert_eq!(tool_text(&host, "heard", json!({})), "before-compact");

    let ctx = json!({
        "span": [{"role": "user", "text": "a"}, {"role": "assistant", "text": "b"}],
        "reason": "pressure",
    });
    assert_eq!(
        host.compact(&ctx, validate_summary).as_deref(),
        Some("## Active Task\nFold 2 entries (pressure).\n## Completed Actions\nRead the span.")
    );
    // A summary the validator refuses is no answer: dirge summarizes.
    assert_eq!(host.compact(&ctx, |_| false), None);
}

#[tokio::test]
async fn turn_hooks_reach_a_running_addon_and_their_answers_fold_back() {
    use super::turn_hooks::{self, PREPARE_NEXT_TURN, SHOULD_STOP_AFTER_TURN, TRANSFORM_CONTEXT};
    use crate::agent::agent_loop::hooks::TurnHookContext;
    use crate::agent::agent_loop::message::{
        AssistantMessage, ContentBlock, StopReason, ToolResultMessage,
    };
    use crate::agent::agent_loop::types::{Context, LoopConfig, ThinkingLevel};

    let host = Arc::new(live_host(IsolateOptions::default()));
    for key in [TRANSFORM_CONTEXT, PREPARE_NEXT_TURN, SHOULD_STOP_AFTER_TURN] {
        assert!(host.listens_key(key), "{key}");
    }
    let mut config = LoopConfig::for_tests(Arc::new(|m: &[Value]| m.to_vec()));
    turn_hooks::install(&mut config, &host, turn_hooks::BUDGET);

    // The addon keeps the last message only.
    let messages = vec![
        json!({"role": "user", "content": "a"}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
    ];
    let transform = config.transform_context.expect("transform installed");
    assert_eq!(transform(messages.clone()).await, vec![messages[1].clone()]);

    let turn = |text: &str| TurnHookContext {
        message: AssistantMessage::new(
            vec![ContentBlock::Text { text: text.into() }],
            StopReason::Stop,
        ),
        tool_results: vec![ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text { text: "x".into() }],
            details: Value::Null,
            is_error: false,
        }],
        context: Context {
            messages: messages.clone(),
            ..Default::default()
        },
        new_messages: Vec::new(),
    };

    // A thinking level and a note for the next turn.
    let prepare = config.prepare_next_turn.expect("prepare installed");
    let update = prepare(turn("working")).await.expect("an update");
    assert_eq!(update.thinking_level, Some(ThinkingLevel::High));
    let context = update.context.expect("a note");
    assert_eq!(context.messages.len(), 3);
    assert!(context.messages[2].to_string().contains("1 tool results"));

    // The addon stops the run only once the turn says done.
    let stop = config.should_stop_after_turn.expect("stop installed");
    assert!(!stop(turn("working")).await);
    assert!(stop(turn("done")).await);
}

#[test]
fn posted_events_reach_the_event_hook_in_order() {
    use crate::event::AgentEvent;

    let host = live_host(IsolateOptions::default());
    for event in [
        AgentEvent::TurnStart { index: 0 },
        AgentEvent::Token("not heard".into()),
        AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: json!({}),
        },
        AgentEvent::Done {
            response: "ok".into(),
            tokens: 1,
            cost: 0.0,
        },
    ] {
        if let Some(ctx) = super::events::project(&event) {
            host.post(super::events::EVENT_KEY, &ctx);
        }
    }

    // Commands run in the order they were queued, so the posts ran first.
    // `turn-start` is heard as it serializes, its `:index` included.
    assert_eq!(
        tool_text(&host, "heard", json!({})),
        "turn-start:0,tool-call,done"
    );
}

/// A host running the `folds` fixture, whose tools run the addon host's
/// `fold-answers` and `shape-ctx` on the data they are given.
fn folds_host() -> AddonHost {
    let host = super::start_with(
        live_plan(&fixtures().join("folds")),
        Harness::with_sink(Arc::new(QuietSink::default())),
        PROTOCOL,
        IsolateOptions::default(),
    )
    .expect("host starts");
    assert!(host.failures().is_empty(), "{:?}", host.failures());
    host
}

/// What the `folds` tool `name` answers for `args`.
fn result(host: &AddonHost, name: &str, args: Value) -> Value {
    let tool = host
        .tools()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no tool {name}"));
    let (_, details) = host.call_tool(&tool, &args).expect("tool runs");
    details["result"].clone()
}

/// The host's fold of `answers` to hook `key` heard with `ctx`.
fn fold(host: &AddonHost, key: &str, ctx: Value, answers: Value) -> Value {
    result(
        host,
        "fold",
        json!({"key": key, "ctx": ctx, "answers": answers}),
    )
}

fn ok(addon: &str, v: Value) -> Value {
    json!({"addon": addon, "ok": v})
}

fn failed(addon: &str) -> Value {
    json!({"addon": addon, "error": "boom"})
}

#[test]
fn the_host_folds_turn_hook_answers() {
    use super::turn_hooks::{PREPARE_NEXT_TURN, SHOULD_STOP_AFTER_TURN, TRANSFORM_CONTEXT};

    let host = folds_host();
    let user = |text: &str| json!({"role": "user", "content": text});

    // The first answer of one or more messages, each with a string role.
    let answers = json!([
        failed("a"),
        ok("b", json!({"messages": []})),
        ok("c", json!({"messages": [{"content": "no role"}]})),
        ok("d", json!({"messages": "text"})),
        ok("e", json!({"messages": [user("kept")]})),
        ok("f", json!({"messages": [user("late")]})),
    ]);
    assert_eq!(
        fold(&host, TRANSFORM_CONTEXT, json!({}), answers),
        json!({"messages": [user("kept")]})
    );
    let none = json!([ok("a", json!("text")), ok("b", Value::Null)]);
    assert_eq!(fold(&host, TRANSFORM_CONTEXT, json!({}), none), Value::Null);

    // The first level the loop knows, and every note in load order.
    let answers = json!([
        ok("a", json!({"thinking": "loud", "context": " one "})),
        ok("b", json!("two")),
        failed("c"),
        ok("d", json!({"thinking": "High"})),
        ok("e", json!({"thinking": "low", "context": "  "})),
    ]);
    assert_eq!(
        fold(&host, PREPARE_NEXT_TURN, json!({}), answers),
        json!({"thinking": "High", "context": ["one", "two"]})
    );
    let none = json!([ok("a", json!({})), ok("b", json!(3))]);
    assert_eq!(fold(&host, PREPARE_NEXT_TURN, json!({}), none), Value::Null);

    // The first addon asking to stop, with its reason when it gave one.
    let answers = json!([
        ok("a", json!(false)),
        ok("b", json!({"stop": "  "})),
        failed("c"),
        ok("d", json!({"stop": " goal met "})),
        ok("e", json!(true)),
    ]);
    assert_eq!(
        fold(&host, SHOULD_STOP_AFTER_TURN, json!({}), answers),
        json!({"addon": "d", "reason": "goal met"})
    );
    for answer in [json!(true), json!({"stop": true})] {
        assert_eq!(
            fold(
                &host,
                SHOULD_STOP_AFTER_TURN,
                json!({}),
                json!([ok("a", answer)])
            ),
            json!({"addon": "a", "reason": null})
        );
    }
    let none = json!([ok("a", json!({"stop": false})), ok("b", Value::Null)]);
    assert_eq!(
        fold(&host, SHOULD_STOP_AFTER_TURN, json!({}), none),
        Value::Null
    );
}

#[test]
fn the_host_folds_acp_answers() {
    use super::acp::{EXT_METHOD_KEY, META_KEY};

    let host = folds_host();

    // The first non-null answer, false included.
    let answers = json!([
        failed("a"),
        ok("b", Value::Null),
        ok("c", json!(false)),
        ok("d", json!("x"))
    ]);
    assert_eq!(
        fold(&host, EXT_METHOD_KEY, json!({}), answers),
        json!(false)
    );
    let none = json!([ok("a", Value::Null)]);
    assert_eq!(fold(&host, EXT_METHOD_KEY, json!({}), none), Value::Null);

    // The response's own _meta wins, then earlier addons; non-maps add nothing.
    let ctx = json!({"response-meta": {"usage": {"totalTokens": 12}}});
    let answers = json!([
        ok(
            "a",
            json!({"usage": "forged", "zed.dev/panel": {"open": true}})
        ),
        ok("b", json!({"zed.dev/panel": "later loses", "x/y": 2})),
        ok("c", json!("not an object")),
    ]);
    assert_eq!(
        fold(&host, META_KEY, ctx, answers),
        json!({"usage": {"totalTokens": 12}, "zed.dev/panel": {"open": true}, "x/y": 2})
    );
    assert_eq!(fold(&host, META_KEY, json!({}), json!([])), Value::Null);
}

#[test]
fn a_key_without_a_fold_answers_every_reply() {
    let host = folds_host();
    let answers = json!([ok("a", json!(1)), failed("b")]);
    assert_eq!(
        fold(&host, "acme/other", json!({}), answers.clone()),
        answers
    );
}

#[test]
fn the_host_names_events_the_way_addons_hear_them() {
    use super::events::{EVENT_KEY, project};
    use crate::agent::agent_loop::message::EscalationReason;
    use crate::event::{AgentEvent, CompactionKind};

    let host = folds_host();
    let cases = [
        (
            AgentEvent::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                args: json!({"path": "a.rs"}),
            },
            json!({"event": "tool-call", "id": "c1", "tool": "read", "args": {"path": "a.rs"}}),
        ),
        (
            AgentEvent::ToolResult {
                id: "c1".into(),
                output: "out".into(),
                kind: Default::default(),
            },
            json!({"event": "tool-result", "id": "c1", "output": "out"}),
        ),
        (
            AgentEvent::Error("boom".into()),
            json!({"event": "error", "message": "boom"}),
        ),
        (
            AgentEvent::ContextOverflow {
                prompt: "p".into(),
                error: "too long".into(),
            },
            json!({"event": "context-overflow", "message": "too long"}),
        ),
        (
            AgentEvent::ContextCompacted {
                new_session_id: "s2".into(),
                tokens_before: 9,
                tokens_after: 4,
                summary: "sum".into(),
                first_kept_index: 1,
                compaction_kind: CompactionKind::PruneAndFailedSummary,
                summary_model: None,
            },
            json!({
                "event": "context-compacted",
                "session-id": "s2",
                "tokens-before": 9,
                "tokens-after": 4,
                "summary": "sum",
                "kind": "prune-and-failed-summary",
            }),
        ),
        (
            AgentEvent::CheckpointRefresh {
                summary: "s".into(),
            },
            json!({"event": "checkpoint", "summary": "s"}),
        ),
        (
            AgentEvent::CustomMessage {
                payload: json!({"type": "x"}),
            },
            json!({"event": "custom-message", "payload": {"type": "x"}}),
        ),
        (
            AgentEvent::Interjected {
                partial_response: "part".into(),
                tokens: 3,
            },
            json!({"event": "interjected", "response": "part", "tokens": 3}),
        ),
        (
            AgentEvent::RetryNotice {
                attempt: 2,
                delay_ms: 500,
                error: "slow".into(),
            },
            json!({"event": "retry", "attempt": 2, "delay-ms": 500, "message": "slow"}),
        ),
        (
            AgentEvent::SystemNotice {
                content: "cap".into(),
            },
            json!({"event": "notice", "content": "cap"}),
        ),
        (
            AgentEvent::EscalationActivated {
                provider: "big".into(),
                reason: EscalationReason::RepairExhausted {
                    tool: "edit".into(),
                },
            },
            json!({
                "event": "escalation",
                "provider": "big",
                "reason": "RepairExhausted { tool: \"edit\" }",
            }),
        ),
        (
            AgentEvent::TurnStart { index: 0 },
            json!({"event": "turn-start", "index": 0}),
        ),
    ];
    for (event, want) in cases {
        let ctx = project(&event).expect("heard");
        let heard = result(&host, "shape", json!({"key": EVENT_KEY, "ctx": ctx}));
        assert_eq!(heard, want, "{event:?}");
    }
}

#[cfg(feature = "addons-nrepl")]
mod repl {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use super::*;
    use crate::addons::cljrs::isolate::ReplOptions;

    /// One bencoded nREPL `eval` of `code`; answers every byte read back
    /// until the server says the request is done.
    fn eval(endpoint: &str, code: &str) -> String {
        let mut stream = TcpStream::connect(endpoint).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let request = format!("d4:code{}:{}2:id1:12:op4:evale", code.len(), code);
        stream.write_all(request.as_bytes()).unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&seen).contains("4:done") {
            let n = stream.read(&mut buf).expect("the server answers");
            assert!(
                n > 0,
                "connection closed: {}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    fn a_repl_evaluation_changes_the_running_addon_and_dirge_takes_it_in() {
        let host = live_host(IsolateOptions {
            repl: Some(ReplOptions {
                addr: ([127, 0, 0, 1], 0).into(),
                port_file: None,
            }),
            refresh_after_eval: true,
        });
        let endpoint = host.repl_endpoint().expect("the REPL listens");

        let answer = eval(&endpoint, "(swap! live.addon/!extra conj \"from-repl\")");
        assert!(answer.contains("from-repl"), "{answer}");
        let report = next_sync(&host);

        assert_eq!(report.tools_added, vec!["from-repl".to_string()]);
        assert_eq!(
            tool_text(&host, "from-repl", json!({})),
            "hello from from-repl"
        );
        let version = eval(&endpoint, "(dirge.harness/version)");
        assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
    }

    #[test]
    fn without_live_refresh_an_evaluation_changes_nothing_until_asked() {
        let host = live_host(IsolateOptions {
            repl: Some(ReplOptions {
                addr: ([127, 0, 0, 1], 0).into(),
                port_file: None,
            }),
            refresh_after_eval: false,
        });
        let endpoint = host.repl_endpoint().expect("the REPL listens");

        eval(&endpoint, "(swap! live.addon/!extra conj \"quiet\")");
        std::thread::sleep(Duration::from_millis(200));
        assert!(host.sync().is_none(), "no refresh without being asked");

        eval(&endpoint, "(dirge.harness/refresh!)");
        assert_eq!(next_sync(&host).tools_added, vec!["quiet".to_string()]);
    }

    /// Bytes read from `stream` until `needle` shows up, within ten seconds.
    fn read_until(stream: &mut TcpStream, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&seen).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "no {needle} within 10s: {}",
                String::from_utf8_lossy(&seen)
            );
            let n = stream.read(&mut buf).expect("the server answers");
            assert!(
                n > 0,
                "connection closed: {}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    fn an_interrupt_frees_the_isolate_from_a_runaway_repl_form() {
        let host = live_host(IsolateOptions {
            repl: Some(ReplOptions {
                addr: ([127, 0, 0, 1], 0).into(),
                port_file: None,
            }),
            refresh_after_eval: false,
        });
        let endpoint = host.repl_endpoint().expect("the REPL listens");

        // The runaway form holds the isolate thread, and with it every hook.
        let code = "(loop [] (recur))";
        let mut runaway = TcpStream::connect(&endpoint).expect("connects");
        runaway
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let request = format!(
            "d4:code{}:{}2:id3:run2:op4:eval7:session7:defaulte",
            code.len(),
            code
        );
        runaway.write_all(request.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let mut control = TcpStream::connect(&endpoint).expect("connects");
        control
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        control
            .write_all(b"d2:id4:stop12:interrupt-id3:run2:op9:interrupt7:session7:defaulte")
            .unwrap();
        read_until(&mut control, "4:done");

        let answer = read_until(&mut runaway, "4:done");
        assert!(answer.contains("11:interrupted"), "{answer}");

        // The isolate is free again: hooks and the REPL both answer.
        let replies = host.emit("acme/ping", &json!({"n": 7}));
        assert_eq!(replies[0].result, Ok(json!("pong 7")));
        assert!(eval(&endpoint, "(+ 1 2)").contains("1:3"));
    }
}
