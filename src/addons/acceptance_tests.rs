//! End to end through the cljrs isolate with the fixture addons under
//! `tests/fixtures/addons`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::discovery;
use super::domain::HookPoint;
use super::port::{HarnessSink, Level};

#[derive(Default)]
struct RecordingSink(Mutex<Vec<(Level, String)>>);

impl HarnessSink for RecordingSink {
    fn notify(&self, level: Level, message: &str) {
        self.0.lock().unwrap().push((level, message.to_string()));
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/addons")
}

#[test]
fn fixture_addon_loads_runs_hooks_and_notifies() {
    let plan = discovery::plan(
        &[fixtures().join("echo")],
        &[fixtures().join("protocol/src")],
    );
    assert_eq!(plan.manifests.len(), 2, "{:?}", plan.manifests);
    let sink = Arc::new(RecordingSink::default());
    let host = super::start(plan, sink.clone(), "fixture.addon-protocol").expect("host starts");

    assert!(host.failures().is_empty(), "{:?}", host.failures());
    let ids: Vec<&str> = host.addons().iter().map(|a| a.id.as_str()).collect();
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
    let plan = discovery::plan(
        &[fixtures().join("echo")],
        &[fixtures().join("protocol/src")],
    );
    let err = super::start(plan, Arc::new(RecordingSink::default()), "no.such.protocol")
        .expect_err("boot fails");
    assert!(err.contains("no.such.protocol"), "{err}");
}
