//! ACP types bridged onto the addons' open hook keys (`crate::addons::acp`):
//! extension requests and notifications, and the `_meta` of the responses
//! dirge sends. Without the `addons` feature, or with no addon listening,
//! every call answers as if no addon were there: an extension request gets
//! method-not-found and `_meta` stays what dirge built.

use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::{ExtNotification, ExtRequest, Meta};
use serde_json::Value;

/// Answer extension request `ext` with the addons' result, or with
/// method-not-found when no addon answers.
pub(super) async fn answer_ext_method(
    ext: ExtRequest,
    responder: Responder<Value>,
) -> Result<(), agent_client_protocol::Error> {
    match ext_response(ext_method(&ext).await) {
        Ok(result) => responder.respond(result),
        Err(error) => responder.respond_with_error(error),
    }
}

/// The JSON-RPC answer for what the addons made of an extension request.
fn ext_response(answer: Result<Option<Value>, String>) -> Result<Value, agent_client_protocol::Error> {
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
async fn ext_method(ext: &ExtRequest) -> Result<Option<Value>, String> {
    match crate::addons::global() {
        Some(host) => {
            crate::addons::acp::ext_method(host, &wire_method(&ext.method), params(&ext.params))
                .await
        }
        None => Ok(None),
    }
}

#[cfg(not(feature = "addons"))]
async fn ext_method(_ext: &ExtRequest) -> Result<Option<Value>, String> {
    Ok(None)
}

/// Hand extension notification `notif` to the addons without waiting.
#[cfg(feature = "addons")]
pub(super) fn ext_notification(notif: &ExtNotification) {
    if let Some(host) = crate::addons::global() {
        crate::addons::acp::ext_notification(
            &host,
            &wire_method(&notif.method),
            params(&notif.params),
        );
    }
}

#[cfg(not(feature = "addons"))]
pub(super) fn ext_notification(_notif: &ExtNotification) {}

/// The `_meta` of dirge's response to `method`: `base` (what dirge put
/// there, e.g. `usage`) with the addons' keys merged in, never over
/// `base`'s. `incoming` is the `_meta` the client sent with the request.
#[cfg(feature = "addons")]
pub(super) async fn meta(
    method: &str,
    session_id: Option<&str>,
    incoming: Option<&Meta>,
    base: Option<Meta>,
) -> Option<Meta> {
    let Some(host) = crate::addons::global() else {
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
        assert_eq!(ext_response(Ok(Some(json!({"pong": 1})))).unwrap(), json!({"pong": 1}));
    }

    #[test]
    fn no_answer_is_method_not_found() {
        assert_eq!(code(ext_response(Ok(None)).unwrap_err()), json!(-32601));
    }

    #[test]
    fn a_timeout_is_an_internal_error() {
        assert_eq!(code(ext_response(Err("no answer".into())).unwrap_err()), json!(-32603));
    }

    /// No addon host is installed in tests, which is what an ACP server
    /// without addons (or without the feature) sees.
    #[tokio::test]
    async fn without_addons_an_ext_method_is_not_found_and_meta_is_kept() {
        let raw = serde_json::value::RawValue::from_string("{}".into()).unwrap();
        let ext = ExtRequest::new("zed/ping", std::sync::Arc::from(raw));
        assert_eq!(code(ext_response(ext_method(&ext).await).unwrap_err()), json!(-32601));
        let mut base = Meta::new();
        base.insert("usage".into(), json!({"totalTokens": 3}));
        assert_eq!(
            meta("session/prompt", Some("s1"), None, Some(base.clone())).await,
            Some(base)
        );
        assert_eq!(meta("initialize", None, None, None).await, None);
    }
}
