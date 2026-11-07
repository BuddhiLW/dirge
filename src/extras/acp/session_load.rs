//! D3: the `session/load` handler and per-turn persistence, the effectful
//! half of [`super::session_store`]. Restoring and mapping stay pure there;
//! this module touches the session map, the store and the connection.

use std::path::PathBuf;

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo, Responder};
use compact_str::CompactString;

use super::session_store::{self, SessionStore};
use super::{AcpSession, AcpState, SessionMap, addon_seam, session_config_options};
use crate::session::Session;

/// A fresh dirge session filed under the ACP session id, so the id a client
/// holds is the id `session/load` finds on disk.
pub(super) fn stored_session(provider: &str, model: &str, acp_id: &str) -> Session {
    let mut session = Session::new(provider, model, 0);
    session.id = CompactString::new(acp_id);
    session
}

/// Where replayed history goes: the client connection in production, a
/// recorder in tests.
pub(super) trait UpdateSink: Send + Sync {
    fn send(&self, notification: SessionNotification);
}

impl UpdateSink for ConnectionTo<Client> {
    fn send(&self, notification: SessionNotification) {
        // A dropped replay line only leaves the client's view short; the
        // session itself is loaded either way.
        let _ = self.send_notification(notification);
    }
}

/// Write the session `id` through `store` so a later connection can
/// `session/load` it. No store (`--no-session`) or an unknown id is a no-op;
/// a failed write is logged, never fatal to the turn.
pub(super) async fn persist_session(
    sessions: &SessionMap,
    store: Option<&dyn SessionStore>,
    id: &str,
) {
    let Some(store) = store else {
        return;
    };
    let mut map = sessions.lock().await;
    if let Some(s) = map.get_mut(id)
        && let Err(e) = store.save(&mut s.session)
    {
        tracing::warn!("ACP session {} not persisted: {}", id, e);
    }
}

/// Restore `id` from `store` into `sessions` (with the client's `cwd` and
/// MCP servers) and replay its history through `sink`, in order.
pub(super) async fn load_and_replay(
    sessions: &SessionMap,
    store: &dyn SessionStore,
    id: &SessionId,
    cwd: PathBuf,
    sink: &dyn UpdateSink,
) -> Result<(), agent_client_protocol::Error> {
    let session = session_store::restore(store, &id.0)?;
    let replay = session_store::replay_updates(&session);
    sessions.lock().await.insert(
        id.to_string(),
        AcpSession {
            session,
            cwd,
            model_override: None,
            provider_override: None,
            mode_override: None,
            #[cfg(feature = "mcp")]
            mcp: None,
            run: None,
        },
    );
    for update in replay {
        sink.send(SessionNotification::new(id.clone(), update));
    }
    Ok(())
}

/// `session/load`: restore a persisted session, replay its history as
/// `session/update` notifications, then answer. Only reachable when
/// `initialize` advertised `loadSession`, i.e. a store is present.
pub(super) async fn handle_load_session(
    req: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    cx: ConnectionTo<Client>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let Some(store) = state.store.as_deref() else {
        return responder.respond_with_error(agent_client_protocol::Error::method_not_found());
    };
    tracing::info!(
        "ACP load session: {} (cwd: {})",
        req.session_id,
        req.cwd.display()
    );
    if let Err(err) = load_and_replay(
        &state.sessions,
        store,
        &req.session_id,
        req.cwd.clone(),
        &cx,
    )
    .await
    {
        return responder.respond_with_error(err);
    }
    // D4a: the client re-declares its MCP servers on load, as on `session/new`.
    #[cfg(feature = "mcp")]
    {
        let mcp = super::mcp_servers::connect(&req.mcp_servers).await;
        if let Some(s) = state
            .sessions
            .lock()
            .await
            .get_mut(&req.session_id.to_string())
        {
            s.mcp = mcp;
        }
    }
    let options =
        session_config_options(&state.sessions, &state.cli, &state.cfg, &req.session_id.0).await;
    let meta = addon_seam::meta(
        "session/load",
        Some(&req.session_id.to_string()),
        req.meta.as_ref(),
        None,
    )
    .await;
    responder.respond(
        LoadSessionResponse::new()
            .config_options(options)
            .meta(meta),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::acp::session_store::tests::{StubStore, saved_conversation};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingClient(Mutex<Vec<SessionNotification>>);

    impl UpdateSink for RecordingClient {
        fn send(&self, notification: SessionNotification) {
            self.0.lock().unwrap().push(notification);
        }
    }

    fn kinds(recorded: &[SessionNotification]) -> Vec<&'static str> {
        recorded
            .iter()
            .map(|n| match &n.update {
                SessionUpdate::UserMessageChunk(_) => "user",
                SessionUpdate::AgentMessageChunk(_) => "agent",
                SessionUpdate::ToolCall(_) => "tool",
                _ => "other",
            })
            .collect()
    }

    async fn load(
        sessions: &SessionMap,
        store: &StubStore,
        id: &str,
        client: &RecordingClient,
    ) -> Result<(), agent_client_protocol::Error> {
        load_and_replay(
            sessions,
            store,
            &SessionId::new(id.to_string()),
            PathBuf::from("/work"),
            client,
        )
        .await
    }

    #[tokio::test]
    async fn load_session_replays_saved_history_in_order() {
        let store = StubStore::default();
        let mut saved = saved_conversation();
        saved.id = "acp-1".into();
        store.save(&mut saved).unwrap();

        let sessions: SessionMap = tokio::sync::Mutex::new(HashMap::new());
        let client = RecordingClient::default();
        load(&sessions, &store, "acp-1", &client).await.unwrap();

        {
            let recorded = client.0.lock().unwrap();
            assert_eq!(kinds(&recorded), ["user", "agent", "tool", "agent"]);
            assert!(recorded.iter().all(|n| n.session_id.0.as_ref() == "acp-1"));
        }
        let map = sessions.lock().await;
        let restored = map.get("acp-1").expect("session registered for prompts");
        assert_eq!(restored.session.messages.len(), saved.messages.len());
        assert_eq!(restored.cwd, PathBuf::from("/work"));
    }

    #[tokio::test]
    async fn load_session_of_unknown_id_is_an_acp_error_and_replays_nothing() {
        let sessions: SessionMap = tokio::sync::Mutex::new(HashMap::new());
        let client = RecordingClient::default();
        let err = load(&sessions, &StubStore::default(), "missing", &client)
            .await
            .unwrap_err();
        assert_eq!(err.code, agent_client_protocol::ErrorCode::ResourceNotFound);
        assert!(client.0.lock().unwrap().is_empty());
        assert!(sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn a_persisted_turn_round_trips_through_load() {
        let store = StubStore::default();
        let sessions: SessionMap = tokio::sync::Mutex::new(HashMap::new());
        sessions.lock().await.insert(
            "acp-2".into(),
            AcpSession {
                session: stored_session("p", "m", "acp-2"),
                cwd: PathBuf::from("/w"),
                model_override: None,
                provider_override: None,
                mode_override: None,
                #[cfg(feature = "mcp")]
                mcp: None,
                run: None,
            },
        );
        super::super::finish_turn(&sessions, "acp-2", 0, "hi", "hello", Vec::new()).await;
        persist_session(&sessions, Some(&store as &dyn SessionStore), "acp-2").await;

        let fresh: SessionMap = tokio::sync::Mutex::new(HashMap::new());
        let client = RecordingClient::default();
        load(&fresh, &store, "acp-2", &client).await.unwrap();
        assert_eq!(kinds(&client.0.lock().unwrap()), ["user", "agent"]);
    }

    #[tokio::test]
    async fn persist_without_a_store_writes_nothing() {
        let sessions: SessionMap = tokio::sync::Mutex::new(HashMap::new());
        persist_session(&sessions, None, "x").await;
    }
}
