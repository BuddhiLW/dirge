//! The ACP server's open seam to addons: extension methods and
//! notifications (ACP's `_`-prefixed methods) and response `_meta`, each on
//! an open hook key, so an addon extends the ACP surface without a new
//! [`HookPoint`](super::domain::HookPoint) and without an edit here.
//!
//! Free of ACP types: `_meta` is a JSON object, which is all ACP's `Meta`
//! is. `extras::acp` bridges the protocol types onto these.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::domain::HookReply;
use super::host::AddonHost;
use crate::runtime::blocking_within;

/// An ACP extension request: ctx `{"method" "params"}`, the method as the
/// client sent it (leading `_` kept). The first non-null answer, in load
/// order, is the result.
pub const EXT_METHOD_KEY: &str = "dirge/acp-ext-method";

/// An ACP extension notification: same ctx, answers dropped.
pub const EXT_NOTIFICATION_KEY: &str = "dirge/acp-ext-notification";

/// A response's `_meta`: ctx `{"method" "session-id" "meta"
/// "response-meta"}`. Each answer that is a JSON object is merged into the
/// response's `_meta`; a key already there is never overwritten.
pub const META_KEY: &str = "dirge/acp-meta";

/// Longest the addons may take to answer one ACP request.
pub const BUDGET: Duration = Duration::from_secs(30);

/// The ctx of an extension method or notification.
pub fn ext_ctx(method: &str, params: Value) -> Value {
    json!({ "method": method, "params": params })
}

/// The first non-null answer, in load order.
pub fn ext_result(replies: &[HookReply]) -> Option<Value> {
    replies
        .iter()
        .filter_map(|r| r.result.as_ref().ok())
        .find(|v| !v.is_null())
        .cloned()
}

/// `base` with every object answer's keys added. What `base` already holds
/// wins, then earlier addons win over later ones. `None` when nothing is
/// left.
pub fn merged_meta(base: Option<Map<String, Value>>, replies: &[HookReply]) -> Option<Map<String, Value>> {
    let mut meta = base.unwrap_or_default();
    for answer in replies.iter().filter_map(|r| r.result.as_ref().ok()) {
        if let Value::Object(keys) = answer {
            for (key, value) in keys {
                meta.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    (!meta.is_empty()).then_some(meta)
}

/// What `_meta` is being built for.
#[derive(Debug, Clone, Default)]
pub struct MetaRequest {
    /// The ACP method answered (`session/prompt`, `initialize`, ...).
    pub method: String,
    pub session_id: Option<String>,
    /// The `_meta` the client sent with the request.
    pub meta: Option<Map<String, Value>>,
}

impl MetaRequest {
    fn ctx(&self, base: &Option<Map<String, Value>>) -> Value {
        json!({
            "method": self.method,
            "session-id": self.session_id,
            "meta": self.meta,
            "response-meta": base,
        })
    }
}

/// The addons' answer to extension method `method`: `Ok(None)` when none
/// answered, `Err` when they did not answer within [`BUDGET`].
pub async fn ext_method(host: Arc<AddonHost>, method: &str, params: Value) -> Result<Option<Value>, String> {
    if !host.listens_key(EXT_METHOD_KEY) {
        return Ok(None);
    }
    let ctx = ext_ctx(method, params);
    blocking_within(BUDGET, move || ext_result(&host.emit(EXT_METHOD_KEY, &ctx)))
        .await
        .map_err(|why| why.to_string())
}

/// Hand extension notification `method` to the addons without waiting.
pub fn ext_notification(host: &AddonHost, method: &str, params: Value) {
    host.post(EXT_NOTIFICATION_KEY, &ext_ctx(method, params));
}

/// `base` with the addons' `_meta` merged in. Addons that do not answer
/// within [`BUDGET`] leave `base` as it is.
pub async fn meta(host: Arc<AddonHost>, request: MetaRequest, base: Option<Map<String, Value>>) -> Option<Map<String, Value>> {
    if !host.listens_key(META_KEY) {
        return base;
    }
    let ctx = request.ctx(&base);
    let replies = blocking_within(BUDGET, move || host.emit(META_KEY, &ctx)).await;
    match replies {
        Ok(replies) => merged_meta(base, &replies),
        Err(why) => {
            tracing::warn!(target: "dirge::addon", %why, method = %request.method, "addon ACP _meta skipped");
            base
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::domain::HookPoint;
    use crate::addons::host::LoadSet;
    use crate::addons::port::AddonRuntime;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// A runtime whose one addon registered `keys` and answers each with
    /// `answers`, recording every hook call.
    #[derive(Default)]
    struct KeyedRuntime {
        keys: Vec<&'static str>,
        answers: Vec<HookReply>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl AddonRuntime for KeyedRuntime {
        fn load(&self, _manifest: &Path, _host_config: &Value) -> Value {
            json!({ "id": "stub", "hooks": self.keys })
        }
        fn unload(&self, _addon_id: &str) {}
        fn reload_sources(&self, _files: &[PathBuf]) -> Vec<(PathBuf, String)> {
            Vec::new()
        }
        fn set_source_roots(&self, _roots: &[PathBuf]) {}
        fn call_tool(&self, _addon_id: &str, _tool: &str, _args: &Value) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn run_command(&self, _addon_id: &str, _name: &str, _ctx: &Value) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn run_hook(&self, point: HookPoint, ctx: &Value) -> Vec<HookReply> {
            self.run_hook_key(point.key(), ctx)
        }
        fn run_hook_key(&self, key: &str, ctx: &Value) -> Vec<HookReply> {
            self.calls.lock().unwrap().push((key.to_string(), ctx.clone()));
            self.answers.clone()
        }
        fn shutdown(&self) {}
    }

    fn host(keys: &[&'static str], answers: Vec<Value>) -> (Arc<AddonHost>, Arc<KeyedRuntime>) {
        let rt = Arc::new(KeyedRuntime {
            keys: keys.to_vec(),
            answers: answers.into_iter().map(reply).collect(),
            ..Default::default()
        });
        let set = LoadSet {
            manifests: vec![PathBuf::from("stub.edn")],
            ..Default::default()
        };
        (Arc::new(AddonHost::load(rt.clone(), set, json!({}))), rt)
    }

    fn reply(v: Value) -> HookReply {
        HookReply {
            addon_id: "stub".into(),
            result: Ok(v),
        }
    }

    fn usage() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("usage".into(), json!({ "totalTokens": 12 }));
        m
    }

    #[tokio::test]
    async fn an_ext_method_reaches_the_addon_and_its_answer_comes_back() {
        let (host, rt) = host(&[EXT_METHOD_KEY], vec![json!({ "pong": 1 })]);
        let out = ext_method(host, "_zed/ping", json!({ "n": 1 })).await;
        assert_eq!(out, Ok(Some(json!({ "pong": 1 }))));
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![(
                EXT_METHOD_KEY.to_string(),
                json!({ "method": "_zed/ping", "params": { "n": 1 } })
            )]
        );
    }

    #[tokio::test]
    async fn no_listening_addon_means_no_answer_and_no_call() {
        let (host, rt) = host(&[META_KEY], vec![json!({ "pong": 1 })]);
        assert_eq!(ext_method(host, "_zed/ping", Value::Null).await, Ok(None));
        assert!(rt.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn null_answers_and_failures_are_passes() {
        let replies = vec![
            HookReply {
                addon_id: "a".into(),
                result: Err("boom".into()),
            },
            reply(Value::Null),
            reply(json!("second")),
            reply(json!("third")),
        ];
        assert_eq!(ext_result(&replies), Some(json!("second")));
        assert_eq!(ext_result(&[reply(Value::Null)]), None);
    }

    #[tokio::test]
    async fn addon_meta_merges_without_clobbering_usage() {
        let (host, rt) = host(
            &[META_KEY],
            vec![
                json!({ "usage": "forged", "zed.dev/panel": { "open": true } }),
                json!({ "zed.dev/panel": "later loses", "x/y": 2 }),
                json!("not an object"),
            ],
        );
        let request = MetaRequest {
            method: "session/prompt".into(),
            session_id: Some("s1".into()),
            meta: Some(Map::from_iter([("client".into(), json!("zed"))])),
        };
        let meta = super::meta(host, request, Some(usage())).await.unwrap();
        assert_eq!(meta["usage"], json!({ "totalTokens": 12 }));
        assert_eq!(meta["zed.dev/panel"], json!({ "open": true }));
        assert_eq!(meta["x/y"], json!(2));
        let (key, ctx) = rt.calls.lock().unwrap()[0].clone();
        assert_eq!(key, META_KEY);
        assert_eq!(ctx["method"], "session/prompt");
        assert_eq!(ctx["session-id"], "s1");
        assert_eq!(ctx["meta"], json!({ "client": "zed" }));
        assert_eq!(ctx["response-meta"]["usage"]["totalTokens"], 12);
    }

    #[tokio::test]
    async fn meta_is_untouched_when_no_addon_listens() {
        let (host, rt) = host(&[EXT_METHOD_KEY], vec![json!({ "x": 1 })]);
        assert_eq!(meta(host.clone(), MetaRequest::default(), None).await, None);
        assert_eq!(
            meta(host, MetaRequest::default(), Some(usage())).await,
            Some(usage())
        );
        assert!(rt.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn notifications_are_posted_with_the_same_ctx() {
        let (host, rt) = host(&[EXT_NOTIFICATION_KEY], Vec::new());
        ext_notification(&host, "_zed/saved", json!({ "path": "a.rs" }));
        assert_eq!(
            *rt.calls.lock().unwrap(),
            vec![(
                EXT_NOTIFICATION_KEY.to_string(),
                json!({ "method": "_zed/saved", "params": { "path": "a.rs" } })
            )]
        );
    }
}
