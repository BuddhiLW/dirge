//! ACP types bridged onto the addons' open hook keys (`crate::addons::acp`):
//! extension requests and notifications, and the `_meta` of the responses
//! dirge sends. Without the `addons` feature, or with no addon listening,
//! every call answers as if no addon were there: an extension request gets
//! method-not-found and `_meta` stays what dirge built.
//!
//! The addons are reached through the [`AddonSeam`] the composition root
//! (`main`) hands [`serve`](super::serve): an
//! [`AddonEmitter`](crate::addons::port::AddonEmitter) port, never the
//! process-wide host.

use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::{ExtNotification, ExtRequest, Meta};
use serde_json::Value;

/// What the ACP server reaches the addons through: the emitter injected at
/// startup, or nothing (no host running, or a build without `addons`).
#[derive(Clone, Default)]
pub struct AddonSeam {
    #[cfg(feature = "addons")]
    emitter: Option<std::sync::Arc<dyn crate::addons::port::AddonEmitter>>,
}

impl AddonSeam {
    /// A seam onto `emitter`.
    #[cfg(feature = "addons")]
    pub fn new(emitter: Option<std::sync::Arc<dyn crate::addons::port::AddonEmitter>>) -> Self {
        Self { emitter }
    }

    /// The seam onto the addon host this process started, if any.
    pub fn current() -> Self {
        #[cfg(feature = "addons")]
        {
            Self::new(crate::addons::emitter::current())
        }
        #[cfg(not(feature = "addons"))]
        {
            Self::default()
        }
    }
}

/// Answer extension request `ext` with the addons' result, or with
/// method-not-found when no addon answers.
pub(super) async fn answer_ext_method(
    seam: AddonSeam,
    ext: ExtRequest,
    responder: Responder<Value>,
) -> Result<(), agent_client_protocol::Error> {
    match ext_response(ext_method(&seam, &ext).await) {
        Ok(result) => responder.respond(result),
        Err(error) => responder.respond_with_error(error),
    }
}

/// The JSON-RPC answer for what the addons made of an extension request.
fn ext_response(
    answer: Result<Option<Value>, String>,
) -> Result<Value, agent_client_protocol::Error> {
    match answer {
        Ok(Some(result)) => Ok(result),
        Ok(None) => Err(agent_client_protocol::Error::method_not_found()),
        Err(why) => Err(agent_client_protocol::util::internal_error(why)),
    }
}

/// The method name as the client sent it: the crate strips the leading `_`
/// when it routes an extension message.
#[cfg(feature = "addons")]
fn wire_method(method: &str) -> String {
    format!("_{method}")
}

#[cfg(feature = "addons")]
fn params(raw: &serde_json::value::RawValue) -> Value {
    serde_json::from_str(raw.get()).unwrap_or(Value::Null)
}

#[cfg(feature = "addons")]
async fn ext_method(seam: &AddonSeam, ext: &ExtRequest) -> Result<Option<Value>, String> {
    match seam.emitter.clone() {
        Some(host) => {
            crate::addons::acp::ext_method(host, &wire_method(&ext.method), params(&ext.params))
                .await
        }
        None => Ok(None),
    }
}

#[cfg(not(feature = "addons"))]
async fn ext_method(_seam: &AddonSeam, _ext: &ExtRequest) -> Result<Option<Value>, String> {
    Ok(None)
}

/// Hand extension notification `notif` to the addons without waiting.
#[cfg(feature = "addons")]
pub(super) fn ext_notification(seam: &AddonSeam, notif: &ExtNotification) {
    if let Some(host) = seam.emitter.as_deref() {
        crate::addons::acp::ext_notification(
            host,
            &wire_method(&notif.method),
            params(&notif.params),
        );
    }
}

#[cfg(not(feature = "addons"))]
pub(super) fn ext_notification(_seam: &AddonSeam, _notif: &ExtNotification) {}

/// The `_meta` of dirge's response to `method`: `base` (what dirge put
/// there, e.g. `usage`) with the addons' keys merged in, never over
/// `base`'s. `incoming` is the `_meta` the client sent with the request.
#[cfg(feature = "addons")]
pub(super) async fn meta(
    seam: &AddonSeam,
    method: &str,
    session_id: Option<&str>,
    incoming: Option<&Meta>,
    base: Option<Meta>,
) -> Option<Meta> {
    let Some(host) = seam.emitter.clone() else {
        return base;
    };
    let request = crate::addons::acp::MetaRequest {
        method: method.to_string(),
        session_id: session_id.map(str::to_string),
        meta: incoming.cloned(),
    };
    crate::addons::acp::meta(host, request, base).await
}

#[cfg(not(feature = "addons"))]
pub(super) async fn meta(
    _seam: &AddonSeam,
    _method: &str,
    _session_id: Option<&str>,
    _incoming: Option<&Meta>,
    base: Option<Meta>,
) -> Option<Meta> {
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn code(error: agent_client_protocol::Error) -> Value {
        serde_json::to_value(error).unwrap()["code"].clone()
    }

    #[test]
    fn an_answer_is_the_result() {
        assert_eq!(
            ext_response(Ok(Some(json!({"pong": 1})))).unwrap(),
            json!({"pong": 1})
        );
    }

    #[test]
    fn no_answer_is_method_not_found() {
        assert_eq!(code(ext_response(Ok(None)).unwrap_err()), json!(-32601));
    }

    #[test]
    fn a_timeout_is_an_internal_error() {
        assert_eq!(
            code(ext_response(Err("no answer".into())).unwrap_err()),
            json!(-32603)
        );
    }

    /// An empty seam is what an ACP server without addons (or without the
    /// feature) sees.
    #[tokio::test]
    async fn without_addons_an_ext_method_is_not_found_and_meta_is_kept() {
        let seam = AddonSeam::default();
        let raw = serde_json::value::RawValue::from_string("{}".into()).unwrap();
        let ext = ExtRequest::new("zed/ping", std::sync::Arc::from(raw));
        assert_eq!(
            code(ext_response(ext_method(&seam, &ext).await).unwrap_err()),
            json!(-32601)
        );
        let mut base = Meta::new();
        base.insert("usage".into(), json!({"totalTokens": 3}));
        assert_eq!(
            meta(&seam, "session/prompt", Some("s1"), None, Some(base.clone())).await,
            Some(base)
        );
        assert_eq!(meta(&seam, "initialize", None, None, None).await, None);
    }

    /// An injected emitter is reached with the method as the client sent it.
    #[cfg(feature = "addons")]
    #[tokio::test]
    async fn an_injected_emitter_answers_ext_methods() {
        use crate::addons::domain::HookReply;
        use crate::addons::port::AddonEmitter;

        struct Pong;
        impl AddonEmitter for Pong {
            fn listens_key(&self, key: &str) -> bool {
                key == crate::addons::acp::EXT_METHOD_KEY
            }
            fn emit(&self, _key: &str, ctx: &Value) -> Vec<HookReply> {
                vec![HookReply {
                    addon_id: "pong".into(),
                    result: Ok(json!({ "method": ctx["method"] })),
                }]
            }
            fn post(&self, _key: &str, _ctx: &Value) {}
        }

        let seam = AddonSeam::new(Some(std::sync::Arc::new(Pong)));
        let raw = serde_json::value::RawValue::from_string("{}".into()).unwrap();
        let ext = ExtRequest::new("zed/ping", std::sync::Arc::from(raw));
        assert_eq!(
            ext_method(&seam, &ext).await,
            Ok(Some(json!({ "method": "_zed/ping" })))
        );
    }
}
