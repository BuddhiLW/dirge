//! One dispatch path: every host hook, the compaction keys included, reaches
//! the runtime through the open key path (`run_hook_key`), never through the
//! closed `run_hook(HookPoint)` path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::compaction::{BEFORE_COMPACT, COMPACT};
use super::domain::{HookPoint, HookReply};
use super::host::AddonHost;
use super::host::tests::summary;
use super::port::AddonRuntime;

/// A runtime that answers only the open key path: `run_hook` (the closed
/// [`HookPoint`] path) panics when reached.
#[derive(Default)]
struct KeyOnlyRuntime {
    keys: Mutex<Vec<String>>,
}

impl AddonRuntime for KeyOnlyRuntime {
    fn load(&self, _manifest: &Path, _host_config: &Value) -> Value {
        json!({"error": "unscripted load"})
    }
    fn unload(&self, _addon_id: &str) {}
    fn reload_sources(&self, _files: &[PathBuf]) -> Vec<(PathBuf, String)> {
        Vec::new()
    }
    fn set_source_roots(&self, _roots: &[PathBuf]) {}
    fn call_tool(&self, _a: &str, _t: &str, _args: &Value) -> Result<Value, String> {
        Ok(Value::Null)
    }
    fn run_command(&self, _a: &str, _n: &str, _ctx: &Value) -> Result<Value, String> {
        Ok(Value::Null)
    }
    fn run_hook(&self, point: HookPoint, _ctx: &Value) -> Vec<HookReply> {
        panic!("closed dispatch path reached for {}", point.key())
    }
    fn run_hook_key(&self, key: &str, _ctx: &Value) -> Vec<HookReply> {
        self.keys.lock().unwrap().push(key.to_string());
        vec![HookReply {
            addon_id: "k".into(),
            result: Ok(json!({"summary": "## Summary\nkept"})),
        }]
    }
    fn shutdown(&self) {}
}

#[test]
fn a_runtime_overriding_only_run_hook_key_sees_the_compact_keys() {
    let rt = Arc::new(KeyOnlyRuntime::default());
    let host = AddonHost::with_reports(
        rt.clone(),
        vec![summary("k", &[], &[])],
        &[("k".to_string(), json!({"hooks": [COMPACT, BEFORE_COMPACT]}))],
    );

    host.before_compact(&json!({"count": 1}));
    let summary = host.compact(&json!({}), |s| s.starts_with("##"));

    assert_eq!(summary.as_deref(), Some("## Summary\nkept"));
    assert_eq!(
        *rt.keys.lock().unwrap(),
        vec![BEFORE_COMPACT.to_string(), COMPACT.to_string()]
    );
}

#[test]
fn an_addon_not_listening_on_the_compact_keys_is_never_called() {
    let rt = Arc::new(KeyOnlyRuntime::default());
    let host = AddonHost::with_reports(
        rt.clone(),
        vec![summary("k", &[], &[])],
        &[("k".to_string(), json!({"hooks": ["acme/other"]}))],
    );

    host.before_compact(&json!({}));
    assert_eq!(host.compact(&json!({}), |_| true), None);
    assert!(rt.keys.lock().unwrap().is_empty());
}
