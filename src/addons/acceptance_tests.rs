//! End to end through the cljrs isolate with the fixture addons under
//! `tests/fixtures/addons`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::discovery;
use super::domain::{HookPoint, PanelRequest};
use super::port::{Harness, HarnessSink, Level, McpGateway, PanelSink};

#[derive(Default)]
struct RecordingSink(Mutex<Vec<(Level, String)>>);

impl HarnessSink for RecordingSink {
    fn notify(&self, level: Level, message: &str) {
        self.0.lock().unwrap().push((level, message.to_string()));
    }
}

#[derive(Default)]
struct RecordingPanels(Mutex<Vec<PanelRequest>>);

impl PanelSink for RecordingPanels {
    fn panel(&self, request: PanelRequest) -> bool {
        self.0.lock().unwrap().push(request);
        true
    }
}

/// One MCP server, `fixture`, whose `lookup` tool echoes `q`.
#[derive(Default)]
struct ScriptedMcp(Mutex<Vec<(String, String, Value)>>);

impl McpGateway for ScriptedMcp {
    fn servers(&self) -> Vec<String> {
        vec!["fixture".to_string()]
    }

    fn call(&self, server: &str, tool: &str, args: &Value) -> Result<Value, String> {
        self.0
            .lock()
            .unwrap()
            .push((server.into(), tool.into(), args.clone()));
        Ok(json!({"content": [{"type": "text", "text": format!("found {}", args["q"])}]}))
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/addons")
}

const PROTOCOL: &str = "fixture.addon-protocol";

fn echo_plan(addons: &Path) -> super::domain::AddonPlan {
    discovery::plan(&[addons.to_path_buf()], &[fixtures().join("protocol/src")])
}

#[test]
fn fixture_addon_loads_runs_hooks_and_notifies() {
    let plan = echo_plan(&fixtures().join("echo"));
    assert_eq!(plan.manifests.len(), 2, "{:?}", plan.manifests);
    let sink = Arc::new(RecordingSink::default());
    let host = super::start(plan, Harness::with_sink(sink.clone()), PROTOCOL).expect("host starts");

    assert!(host.failures().is_empty(), "{:?}", host.failures());
    let ids: Vec<String> = host.addons().into_iter().map(|a| a.id).collect();
    assert_eq!(ids, vec!["echo"], "the JVM-only manifest is skipped");

    let tool = host.tools()[0].clone();
    assert_eq!(tool.model_name(), "count-rows");
    let (content, _) = host
        .call_tool(&tool, &json!({"rows": [1, 2, 3]}))
        .expect("tool runs");
    assert_eq!(content[0]["text"], "rows=3");

    assert_eq!(
        host.texts(HookPoint::SystemPrompt, &json!({})),
        vec!["echo addon active".to_string()]
    );
    assert_eq!(
        *sink.0.lock().unwrap(),
        vec![(Level::Info, "echo loaded".to_string())]
    );

    host.shutdown();
    assert!(host.call_tool(&tool, &json!({})).is_err());
}

#[test]
fn an_unknown_protocol_namespace_stops_the_host() {
    let plan = echo_plan(&fixtures().join("echo"));
    let err = super::start(
        plan,
        Harness::with_sink(Arc::new(RecordingSink::default())),
        "no.such.protocol",
    )
    .expect_err("boot fails");
    assert!(err.contains("no.such.protocol"), "{err}");
}

#[test]
fn addon_commands_reach_the_panel_and_mcp_through_the_harness() {
    let panels = Arc::new(RecordingPanels::default());
    let mcp = Arc::new(ScriptedMcp::default());
    let harness = Harness {
        sink: Arc::new(RecordingSink::default()),
        panels: panels.clone(),
        mcp: mcp.clone(),
    };
    let host =
        super::start(echo_plan(&fixtures().join("echo")), harness, PROTOCOL).expect("host starts");

    let names: Vec<String> = host.commands().into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["ask", "echo"]);

    let echo = host.command("echo").expect("echo registered");
    let out = host.run_command(&echo, "hello there").expect("echo runs");
    assert_eq!(out.text.as_deref(), Some("echo: hello there"));
    assert_eq!(out.prompt, None);
    assert_eq!(
        *panels.0.lock().unwrap(),
        vec![PanelRequest::Show {
            id: "echo".into(),
            title: "Echo".into(),
            lines: vec![("hello there".into(), "normal".into())],
        }]
    );

    let ask = host.command("ask").expect("ask registered");
    let out = host.run_command(&ask, "kanban").expect("ask runs");
    assert_eq!(out.text.as_deref(), Some("found \"kanban\""));
    assert_eq!(out.prompt.as_deref(), Some("summarize kanban"));
    assert_eq!(
        *mcp.0.lock().unwrap(),
        vec![("fixture".into(), "lookup".into(), json!({"q": "kanban"}))]
    );
    host.shutdown();
}

#[test]
fn mcp_calls_are_refused_while_the_event_loop_waits_on_the_addon() {
    let mcp = Arc::new(ScriptedMcp::default());
    let mut harness = Harness::with_sink(Arc::new(RecordingSink::default()));
    harness.mcp = mcp.clone();
    let host = Arc::new(
        super::start(echo_plan(&fixtures().join("echo")), harness, PROTOCOL).expect("host starts"),
    );
    let ask = host.command("ask").expect("ask registered");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    // Called straight from a runtime thread, as spawn_runner calls the
    // prompt hooks: the gateway must not be reached.
    let out = rt.block_on(async { host.run_command(&ask, "x") }).unwrap();
    assert!(
        out.text.unwrap().contains("unavailable while dirge waits"),
        "refused"
    );
    assert!(mcp.0.lock().unwrap().is_empty());
    host.shutdown();
}

/// A copy of the echo fixture the test may rewrite.
struct Scratch(PathBuf);

impl Scratch {
    fn echo() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dirge-addon-reload-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        copy_dir(&fixtures().join("echo"), &dir.join("echo"));
        Self(dir)
    }

    fn addon_source(&self) -> PathBuf {
        self.0.join("echo/src/echo/addon.cljc")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn reload_runs_the_code_now_on_disk_and_reports_the_tool_change() {
    let scratch = Scratch::echo();
    let plan = echo_plan(&scratch.0);
    let sink = Arc::new(RecordingSink::default());
    let host = super::start(plan.clone(), Harness::with_sink(sink.clone()), PROTOCOL)
        .expect("host starts");
    let tool = host.tools()[0].clone();
    assert_eq!(
        host.call_tool(&tool, &json!({"rows": [1]})).unwrap().0[0]["text"],
        "rows=1"
    );

    let source = std::fs::read_to_string(scratch.addon_source()).unwrap();
    let edited = source
        .replace("(str \"rows=\"", "(str \"v2 rows=\"")
        .replace(
            ":handler     count-rows}]",
            ":handler     count-rows}\n     {:name \"echo-v2\" :handler (fn [_] \"v2\")}]",
        );
    assert_ne!(
        source, edited,
        "the fixture changed shape; update this test"
    );
    std::fs::write(scratch.addon_source(), edited).unwrap();

    let report = host.reload(super::load_set(&plan, true));

    assert!(
        report.source_errors.is_empty(),
        "{:?}",
        report.source_errors
    );
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.loaded, vec!["echo".to_string()]);
    assert_eq!(report.tools_added, vec!["echo-v2".to_string()]);
    assert!(report.tools_removed.is_empty());
    assert_eq!(
        host.call_tool(&tool, &json!({"rows": [1, 2]})).unwrap().0[0]["text"],
        "v2 rows=2",
        "the tool registered before the reload runs the new code"
    );
    assert_eq!(
        sink.0.lock().unwrap().len(),
        2,
        "initialize! ran again: {:?}",
        sink.0.lock().unwrap()
    );
    host.shutdown();
}
