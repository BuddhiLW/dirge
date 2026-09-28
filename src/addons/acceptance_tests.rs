//! End to end through the cljrs isolate with the fixture addons under
//! `tests/fixtures/addons`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::discovery;
use super::domain::{HookPoint, PanelRequest};
use super::port::{Harness, HarnessSink, Level, McpGateway, NoTools, PanelSink, ToolGateway};

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

/// One dirge tool, `read`, answering the path it was handed.
#[derive(Default)]
struct ScriptedTools(Mutex<Vec<(String, Value)>>);

impl ToolGateway for ScriptedTools {
    fn names(&self) -> Vec<String> {
        vec!["read".to_string()]
    }

    fn call(&self, tool: &str, args: &Value) -> Result<String, String> {
        self.0.lock().unwrap().push((tool.into(), args.clone()));
        match tool {
            "read" => Ok(format!("contents of {}", args["path"])),
            other => Err(format!("no tool named '{other}'")),
        }
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
        tools: Arc::new(NoTools),
    };
    let host =
        super::start(echo_plan(&fixtures().join("echo")), harness, PROTOCOL).expect("host starts");

    let names: Vec<String> = host.commands().into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["ask", "echo", "run", "shout"]);

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

    // From the event-loop thread, as spawn_runner calls the prompt hooks: the
    // gateway must not be reached. A thread of its own keeps the mark from
    // leaking into other tests.
    let (h, a) = (host.clone(), ask.clone());
    let refused = std::thread::spawn(move || {
        super::cljrs::isolate::mark_event_loop_thread();
        h.run_command(&a, "x").unwrap()
    })
    .join()
    .unwrap();
    assert!(
        refused
            .text
            .unwrap()
            .contains("unavailable while dirge waits"),
        "refused"
    );
    assert!(mcp.0.lock().unwrap().is_empty());

    // From a blocking-pool thread, the way commands and tools reach the
    // isolate: a runtime context, but not the loop, so the call goes through.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let h = host.clone();
    let allowed = rt
        .block_on(async { tokio::task::spawn_blocking(move || h.run_command(&ask, "y")).await })
        .unwrap()
        .unwrap();
    assert_eq!(allowed.text.as_deref(), Some("found \"y\""));
    assert_eq!(mcp.0.lock().unwrap().len(), 1);
    host.shutdown();
}

#[test]
fn addon_commands_call_dirge_tools_through_the_harness() {
    let tools = Arc::new(ScriptedTools::default());
    let mut harness = Harness::with_sink(Arc::new(RecordingSink::default()));
    harness.tools = tools.clone();
    let host = Arc::new(
        super::start(echo_plan(&fixtures().join("echo")), harness, PROTOCOL).expect("host starts"),
    );
    let run = host.command("run").expect("run registered");

    let out = host.run_command(&run, "read").expect("run runs");
    assert_eq!(out.text.as_deref(), Some("contents of \"README.md\""));
    let out = host.run_command(&run, "nowhere").expect("run runs");
    assert_eq!(out.text.as_deref(), Some("error: no tool named 'nowhere'"));
    assert_eq!(
        *tools.0.lock().unwrap(),
        vec![
            ("read".into(), json!({"path": "README.md"})),
            ("nowhere".into(), json!({"path": "README.md"})),
        ]
    );

    // From the event-loop thread, as spawn_runner calls the prompt hooks; a
    // thread of its own keeps the mark from leaking into other tests.
    let h = host.clone();
    let out = std::thread::spawn(move || {
        super::cljrs::isolate::mark_event_loop_thread();
        h.run_command(&run, "read").unwrap()
    })
    .join()
    .unwrap();
    assert!(
        out.text.unwrap().contains("call-tool is unavailable"),
        "refused on the event-loop thread"
    );
    assert_eq!(tools.0.lock().unwrap().len(), 2, "gateway not reached");
    host.shutdown();
}

#[test]
fn a_command_registered_with_a_leading_slash_runs_by_its_bare_name() {
    let host = super::start(
        echo_plan(&fixtures().join("echo")),
        Harness::with_sink(Arc::new(RecordingSink::default())),
        PROTOCOL,
    )
    .expect("host starts");

    let shout = host.command("shout").expect("listed without the slash");
    let out = host.run_command(&shout, "hi").expect("shout runs");
    assert_eq!(out.text.as_deref(), Some("shout: hi"));
    host.shutdown();
}

/// A copy of one fixture addon the test may rewrite.
struct Scratch(PathBuf);

impl Scratch {
    fn of(fixture: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dirge-addon-reload-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        copy_dir(&fixtures().join(fixture), &dir.join(fixture));
        Self(dir)
    }

    fn echo() -> Self {
        Self::of("echo")
    }

    fn addon_source(&self) -> PathBuf {
        self.file("echo/src/echo/addon.cljc")
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Replace `from` with `to` in `rel`, which must contain it.
    fn edit(&self, rel: &str, from: &str, to: &str) {
        let path = self.file(rel);
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            source.contains(from),
            "{rel} changed shape; update this test"
        );
        std::fs::write(&path, source.replace(from, to)).unwrap();
    }

    /// A host over this copy, and the plan it was started from.
    fn start(&self) -> (super::host::AddonHost, super::domain::AddonPlan) {
        let plan = echo_plan(&self.0);
        let harness = Harness::with_sink(Arc::new(RecordingSink::default()));
        let host = super::start(plan.clone(), harness, PROTOCOL).expect("host starts");
        (host, plan)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// The text a tool answers with no arguments.
fn tool_text(host: &super::host::AddonHost, tool: &super::domain::ToolSpec) -> String {
    let (content, _) = host.call_tool(tool, &json!({})).expect("tool runs");
    content[0]["text"].as_str().unwrap_or_default().to_string()
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

/// The manifest sits at `<repo>/META-INF/addons`, so `<repo>` and
/// `<repo>/src` are both source roots.
#[test]
fn reload_of_a_flat_layout_addon_runs_the_edited_code() {
    let scratch = Scratch::of("flat");
    let (host, plan) = scratch.start();
    let tool = host.tools()[0].clone();
    assert_eq!(tool_text(&host, &tool), "flat v1");

    scratch.edit("flat/src/flat/addon.cljc", "\"flat v1\"", "\"flat v2\"");
    let report = host.reload(super::load_set(&plan, true));

    assert!(
        report.source_errors.is_empty(),
        "{:?}",
        report.source_errors
    );
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(tool_text(&host, &tool), "flat v2");
    host.shutdown();
}

/// layered/addon.cljc sorts before the layered/util.cljc it requires, and
/// computes `greeting` from it when it loads.
#[test]
fn reload_evaluates_a_required_namespace_before_the_one_requiring_it() {
    let scratch = Scratch::of("layered");
    let (host, plan) = scratch.start();
    let tool = host.tools()[0].clone();
    assert_eq!(tool_text(&host, &tool), "hello reload");

    scratch.edit(
        "layered/src/layered/util.cljc",
        "\"hello \"",
        "\"goodbye \"",
    );
    let report = host.reload(super::load_set(&plan, true));

    assert!(
        report.source_errors.is_empty(),
        "{:?}",
        report.source_errors
    );
    assert_eq!(
        tool_text(&host, &tool),
        "goodbye reload",
        "layered.addon read the edited layered.util when it loaded again"
    );
    host.shutdown();
}

#[test]
fn a_require_cycle_is_reported_and_not_evaluated() {
    let scratch = Scratch::of("layered");
    let (host, plan) = scratch.start();
    let tool = host.tools()[0].clone();

    scratch.edit(
        "layered/src/layered/util.cljc",
        "\"A namespace layered.addon requires and reads at load time.\")",
        "(:require [layered.addon]))",
    );
    scratch.edit(
        "layered/src/layered/util.cljc",
        "\"hello \"",
        "\"goodbye \"",
    );
    let report = host.reload(super::load_set(&plan, true));

    let mut cyclic: Vec<String> = report
        .source_errors
        .iter()
        .filter(|e| e.error.contains("cycle"))
        .map(|e| file_name(&e.manifest))
        .collect();
    cyclic.sort();
    assert_eq!(
        cyclic,
        vec!["addon.cljc", "util.cljc"],
        "{:?}",
        report.source_errors
    );
    assert_eq!(report.loaded, vec!["layered".to_string()]);
    assert_eq!(
        tool_text(&host, &tool),
        "hello reload",
        "neither file of the cycle was evaluated"
    );
    host.shutdown();
}

#[test]
fn a_source_whose_ns_form_disagrees_with_its_path_is_reported() {
    let scratch = Scratch::of("layered");
    let (host, plan) = scratch.start();
    let src = scratch.file("layered/src/layered");
    std::fs::write(src.join("stray.cljc"), "(ns layered.elsewhere)\n").unwrap();
    std::fs::write(src.join("unused.cljc"), "(ns layered.unused)\n").unwrap();

    let report = host.reload(super::load_set(&plan, true));

    let reported: Vec<(String, String)> = report
        .source_errors
        .iter()
        .map(|e| (file_name(&e.manifest), e.error.clone()))
        .collect();
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0].0, "stray.cljc");
    assert!(
        reported[0].1.contains("layered.elsewhere") && reported[0].1.contains("layered.stray"),
        "{reported:?}"
    );
    assert_eq!(report.loaded, vec!["layered".to_string()]);
    host.shutdown();
}
