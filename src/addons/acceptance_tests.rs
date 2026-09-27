//! End to end through the cljrs isolate with hive-dirge's probe addon.
//!
//! Needs checkouts of hive-dirge and hive-addon, found at `HIVE_DIRGE_ROOT`
//! (default `~/PP/hive/hive-dirge`). Run with
//! `cargo test --features addons addons::acceptance -- --ignored`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::discovery;
use super::port::{HarnessSink, Level};

#[derive(Default)]
struct RecordingSink(Mutex<Vec<(Level, String)>>);

impl HarnessSink for RecordingSink {
    fn notify(&self, level: Level, message: &str) {
        self.0.lock().unwrap().push((level, message.to_string()));
    }
}

fn hive_dirge_root() -> PathBuf {
    std::env::var_os("HIVE_DIRGE_ROOT")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join("PP/hive/hive-dirge")))
        .expect("HIVE_DIRGE_ROOT or a home directory")
}

#[test]
#[ignore = "needs hive-dirge and hive-addon checkouts"]
fn probe_loads_runs_its_tool_and_notifies() {
    let plan = discovery::plan(&[hive_dirge_root().join("resources")], &[]);
    let sink = Arc::new(RecordingSink::default());
    let host = super::start(plan, sink.clone()).expect("host starts");

    assert!(host.failures().is_empty(), "{:?}", host.failures());
    let ids: Vec<&str> = host.addons().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["hive.dirge.probe"]);

    let tool = host
        .tools()
        .iter()
        .find(|t| t.exposed_name == "swarm-view")
        .expect("probe tool")
        .clone();
    let (content, _) = host
        .call_tool(&tool, &json!({"rows": [1, 2, 3]}))
        .expect("tool runs");
    assert_eq!(content[0]["text"], "rows=3");

    assert_eq!(
        *sink.0.lock().unwrap(),
        vec![(Level::Info, "hive.dirge.probe loaded".to_string())]
    );

    host.shutdown();
    assert!(host.call_tool(&tool, &json!({})).is_err());
}
