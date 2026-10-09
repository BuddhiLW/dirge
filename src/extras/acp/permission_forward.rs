//! D4b: forward dirge permission asks to the ACP client.
//!
//! A tool that needs confirmation sends an `AskRequest` on the ask channel.
//! With a connected editor client, each ask becomes a
//! `session/request_permission` request; the client's choice is folded back
//! into a `UserDecision`. A client that cannot answer (transport error,
//! method not found, a cancelled turn, an unknown option) yields a deny, so
//! the tool never hangs and never runs unconfirmed.
//!
//! The client sits behind [`PermissionPort`], so the fold and the forwarder
//! are tested against a fake client with no ACP connection.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo};

use crate::permission::ask::{AskRequest, UserDecision};

/// The answer of one `session/request_permission` round trip.
pub(super) type PermissionFuture = Pin<
    Box<
        dyn Future<Output = Result<RequestPermissionResponse, agent_client_protocol::Error>> + Send,
    >,
>;

/// Port the forwarder sends a permission request through. The live adapter
/// is [`client_port`]; tests pass a fake client.
pub(super) type PermissionPort =
    Arc<dyn Fn(RequestPermissionRequest) -> PermissionFuture + Send + Sync>;

/// Option id of the "allow once" choice offered to the client.
const ALLOW_ONCE: &str = "allow-once";
/// Option id of the "reject once" choice offered to the client.
const REJECT_ONCE: &str = "reject-once";

/// The live adapter: send the request over the ACP connection and await the
/// client's response. The forwarder runs on its own task, outside the
/// dispatch loop, so `block_task` cannot deadlock it.
pub(super) fn client_port(cx: ConnectionTo<Client>) -> PermissionPort {
    Arc::new(move |req: RequestPermissionRequest| {
        let cx = cx.clone();
        Box::pin(async move { cx.send_request(req).block_task().await }) as PermissionFuture
    })
}

/// Build the `session/request_permission` request for one dirge ask. Only
/// once-scoped options are offered: "allow always" needs a rule pattern
/// derived the way the TUI derives it, which this path does not do yet.
pub(super) fn permission_request(
    session_id: &SessionId,
    ask: &AskRequest,
) -> RequestPermissionRequest {
    let mut title = format!("{}: {}", ask.tool, ask.input);
    if let Some(reason) = &ask.reason {
        title.push_str(&format!(" (flagged: {reason})"));
    }
    let mut raw = serde_json::json!({ "tool": ask.tool, "input": ask.input });
    if let Some(details) = &ask.details {
        raw["details"] = serde_json::Value::String(details.clone());
    }
    let fields = ToolCallUpdateFields::new().title(title).raw_input(raw);
    let call_id = format!("perm-{}", uuid::Uuid::new_v4());
    RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(call_id, fields),
        vec![
            PermissionOption::new(ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new(REJECT_ONCE, "Reject", PermissionOptionKind::RejectOnce),
        ],
    )
}

/// Fold the client's answer into a dirge decision. Anything but an explicit
/// "allow once" denies.
pub(super) fn decision(
    answer: Result<RequestPermissionResponse, agent_client_protocol::Error>,
) -> UserDecision {
    match answer {
        Ok(resp) => match resp.outcome {
            RequestPermissionOutcome::Selected(sel) if &*sel.option_id.0 == ALLOW_ONCE => {
                UserDecision::AllowOnce
            }
            _ => UserDecision::deny(),
        },
        Err(e) => {
            tracing::info!("ACP request_permission failed, denying: {e}");
            UserDecision::deny()
        }
    }
}

/// Forward every ask on `ask_rx` to the client through `port` and reply with
/// its decision. Each ask runs on its own task so a slow client never stalls
/// the queue.
pub(super) fn spawn_acp_ask_forwarder(
    mut ask_rx: tokio::sync::mpsc::Receiver<AskRequest>,
    session_id: SessionId,
    port: PermissionPort,
) {
    tokio::spawn(async move {
        while let Some(req) = ask_rx.recv().await {
            let request = permission_request(&session_id, &req);
            let port = port.clone();
            tokio::spawn(async move {
                let answer = port(request).await;
                let _ = req.reply.send(decision(answer));
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A fake client that answers every request with `answer` and records
    /// what it was asked.
    fn fake_client(
        answer: fn() -> Result<RequestPermissionResponse, agent_client_protocol::Error>,
        seen: Arc<Mutex<Vec<RequestPermissionRequest>>>,
    ) -> PermissionPort {
        Arc::new(move |req| {
            seen.lock().unwrap().push(req);
            Box::pin(async move { answer() }) as PermissionFuture
        })
    }

    fn selected(
        id: &'static str,
    ) -> Result<RequestPermissionResponse, agent_client_protocol::Error> {
        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        ))
    }

    async fn ask_through(port: PermissionPort) -> UserDecision {
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel::<AskRequest>(8);
        spawn_acp_ask_forwarder(ask_rx, SessionId::new("sess-perm".to_string()), port);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        ask_tx
            .send(AskRequest {
                tool: "bash".to_string(),
                input: "rm -rf build".to_string(),
                details: Some("cwd /tmp".to_string()),
                reason: None,
                reply: reply_tx,
            })
            .await
            .expect("send must succeed");
        tokio::time::timeout(std::time::Duration::from_millis(500), reply_rx)
            .await
            .expect("forwarder must reply promptly")
            .expect("reply channel must not be dropped")
    }

    #[tokio::test]
    async fn client_allow_once_allows() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let resp = ask_through(fake_client(|| selected(ALLOW_ONCE), seen.clone())).await;
        assert!(matches!(resp, UserDecision::AllowOnce), "got {resp:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one request reaches the client");
        let req = &seen[0];
        assert_eq!(req.session_id.to_string(), "sess-perm");
        assert_eq!(
            req.tool_call.fields.title.as_deref(),
            Some("bash: rm -rf build")
        );
        let raw = req.tool_call.fields.raw_input.as_ref().expect("raw input");
        assert_eq!(raw["details"], "cwd /tmp");
        let ids: Vec<&str> = req.options.iter().map(|o| &*o.option_id.0).collect();
        assert_eq!(ids, vec![ALLOW_ONCE, REJECT_ONCE]);
    }

    #[tokio::test]
    async fn client_reject_denies() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let resp = ask_through(fake_client(|| selected(REJECT_ONCE), seen)).await;
        assert!(matches!(resp, UserDecision::Deny { .. }), "got {resp:?}");
    }

    #[tokio::test]
    async fn client_cancelled_denies() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cancelled = || {
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
        };
        let resp = ask_through(fake_client(cancelled, seen)).await;
        assert!(matches!(resp, UserDecision::Deny { .. }), "got {resp:?}");
    }

    /// A client that cannot answer (no request_permission support, broken
    /// transport) must deny, never allow and never hang.
    #[tokio::test]
    async fn client_that_cannot_answer_denies() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let failing = || {
            Err(agent_client_protocol::util::internal_error(
                "method not found",
            ))
        };
        let resp = ask_through(fake_client(failing, seen)).await;
        assert!(matches!(resp, UserDecision::Deny { .. }), "got {resp:?}");
    }

    #[test]
    fn unknown_option_id_denies() {
        assert!(matches!(
            decision(selected("allow-always")),
            UserDecision::Deny { .. }
        ));
    }

    #[test]
    fn flagged_reason_is_shown_in_the_title() {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let ask = AskRequest {
            tool: "write".to_string(),
            input: "/etc/hosts".to_string(),
            details: None,
            reason: Some("system path".to_string()),
            reply,
        };
        let req = permission_request(&SessionId::new("s".to_string()), &ask);
        assert_eq!(
            req.tool_call.fields.title.as_deref(),
            Some("write: /etc/hosts (flagged: system path)")
        );
        assert!(
            req.tool_call
                .fields
                .raw_input
                .as_ref()
                .unwrap()
                .get("details")
                .is_none()
        );
    }
}
