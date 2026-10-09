//! `dirge.harness/emit`: addon code posting an open hook key that other
//! addons listen on, addon to addon.
//!
//! The key rule ([`emit_key`]) is pure; the post goes through the
//! [`AddonEmitter`] port, whose adapter ([`HostEmitter`]) hands it to
//! [`AddonHost::post`]: queued, never awaited, dropped past the runtime's
//! posted-call bound.

use std::sync::Arc;

use serde_json::Value;

use super::host::AddonHost;
use super::port::AddonEmitter;

/// The key `raw` names, as hook keys are spelled (a keyword without its
/// colon), when an addon may emit it: namespaced (`acme/x`), and outside
/// the `dirge` namespaces, whose keys only dirge fires. Answers why not
/// otherwise.
pub fn emit_key(raw: &str) -> Result<String, String> {
    let key = raw.trim();
    let key = key.strip_prefix(':').unwrap_or(key);
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        return Err(format!(
            "emit needs a keyword key such as :acme/x, got {raw:?}"
        ));
    }
    let Some((ns, name)) = key.split_once('/') else {
        return Err(format!(
            "emit needs a namespaced key such as :acme/x, got {raw:?}"
        ));
    };
    if ns.is_empty() || name.is_empty() {
        return Err(format!(
            "emit needs a namespaced key such as :acme/x, got {raw:?}"
        ));
    }
    if ns == "dirge" || ns.starts_with("dirge.") {
        return Err(format!(
            "emit cannot fire :{key}: the {ns} namespace is reserved for dirge's own hooks"
        ));
    }
    Ok(key.to_string())
}

/// The context an emit hands the listeners: the payload when it is a map,
/// an empty map when there is none (nil). Hook contexts are maps, so
/// anything else is refused.
pub fn emit_ctx(payload: Option<&Value>) -> Result<Value, String> {
    match payload {
        None | Some(Value::Null) => Ok(Value::Object(Default::default())),
        Some(map @ Value::Object(_)) => Ok(map.clone()),
        Some(other) => Err(format!("emit needs a map payload, got {other}")),
    }
}

/// Where [`HostEmitter`] finds the host each time an addon emits.
type HostSource = Arc<dyn Fn() -> Option<Arc<AddonHost>> + Send + Sync>;

/// Posts through whichever [`AddonHost`] its source answers.
pub struct HostEmitter {
    host: HostSource,
}

impl HostEmitter {
    /// The process-wide host, whichever is running when an addon emits:
    /// the host does not exist yet when its runtime boots.
    pub fn live() -> Self {
        Self::from_source(Arc::new(super::global))
    }

    /// The host `source` answers at emit time.
    pub fn from_source(source: HostSource) -> Self {
        Self { host: source }
    }
}

impl AddonEmitter for HostEmitter {
    fn emit(&self, key: &str, ctx: &Value) -> Result<(), String> {
        let host = (self.host)().ok_or_else(|| "no addon host is running".to_string())?;
        host.post(key, ctx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{emit_ctx, emit_key};
    use serde_json::json;

    #[test]
    fn payloads_are_maps_and_nil_is_an_empty_one() {
        assert_eq!(emit_ctx(None), Ok(json!({})));
        assert_eq!(emit_ctx(Some(&json!(null))), Ok(json!({})));
        assert_eq!(emit_ctx(Some(&json!({"n": 1}))), Ok(json!({"n": 1})));
        assert!(emit_ctx(Some(&json!([1]))).is_err());
        assert!(emit_ctx(Some(&json!("x"))).is_err());
    }

    #[test]
    fn namespaced_open_keys_pass_without_their_colon() {
        assert_eq!(emit_key(":acme/x"), Ok("acme/x".to_string()));
        assert_eq!(emit_key("acme/x"), Ok("acme/x".to_string()));
        assert_eq!(emit_key("acme.sub/ping"), Ok("acme.sub/ping".to_string()));
        // `dirgex` is not a dirge namespace.
        assert_eq!(emit_key(":dirgex/y"), Ok("dirgex/y".to_string()));
    }

    #[test]
    fn dirge_namespaces_are_reserved() {
        for key in [
            ":dirge/event",
            "dirge/on-prompt",
            ":dirge.hook/Notification",
        ] {
            let error = emit_key(key).unwrap_err();
            assert!(error.contains("reserved"), "{key}: {error}");
        }
    }

    #[test]
    fn keys_without_a_namespace_or_a_name_are_refused() {
        for key in ["x", ":x", "", ":", "/x", "acme/", "acme /x"] {
            assert!(emit_key(key).is_err(), "{key:?} accepted");
        }
    }
}
