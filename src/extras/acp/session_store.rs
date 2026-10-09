//! D3: ACP sessions persist and `session/load` replays them.
//!
//! Layers, bottom up:
//! - [`SessionStore`]: the port the ACP agent depends on. dirge's own session
//!   files (`crate::session::storage`) are the one adapter,
//!   [`FileSessionStore`]; there is no second store.
//! - [`replay_updates`]: a pure mapping from a stored [`Session`] to the ACP
//!   `session/update` payloads that redraw it in the client.
//! - [`restore`]: load through the port and turn "no such session" into the
//!   ACP error the client sees.
//!
//! The connection (sending each update) stays in `mod.rs`, the boundary.

use agent_client_protocol::schema::v1::*;

use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};

/// Where ACP sessions live between connections.
pub trait SessionStore: Send + Sync {
    /// The stored session `id`, `Ok(None)` when there is none.
    fn load(&self, id: &str) -> anyhow::Result<Option<Session>>;
    /// Write `session` under its own `id`.
    fn save(&self, session: &mut Session) -> anyhow::Result<()>;
}

/// dirge's session files (`<data-dir>/sessions/<id>.json`).
pub struct FileSessionStore;

impl SessionStore for FileSessionStore {
    fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
        match crate::session::storage::load_session(id) {
            Ok(session) => Ok(Some(session)),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn save(&self, session: &mut Session) -> anyhow::Result<()> {
        crate::session::storage::save_session(session)
    }
}

/// Load `id` through `store`. An unknown id is `resource_not_found`; a store
/// failure (bad id, unreadable file) is `invalid_params` carrying the reason.
pub fn restore(
    store: &dyn SessionStore,
    id: &str,
) -> Result<Session, agent_client_protocol::Error> {
    match store.load(id) {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(agent_client_protocol::Error::resource_not_found(Some(
            id.to_string(),
        ))),
        Err(e) => Err(agent_client_protocol::Error::invalid_params()
            .data(serde_json::Value::String(e.to_string()))),
    }
}

fn text_chunk(text: &str) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())))
}

/// One stored tool call as a finished ACP `tool_call`: its id, name, input,
/// final status and the output the model saw.
fn replay_tool_call(entry: &ToolCallEntry) -> ToolCall {
    let (status, output) = match &entry.state {
        ToolCallState::Completed { result } => (ToolCallStatus::Completed, Some(result.as_str())),
        ToolCallState::Failed { error } => (ToolCallStatus::Failed, Some(error.as_str())),
        ToolCallState::Interrupted => (ToolCallStatus::Failed, None),
    };
    let content = output
        .map(|text| {
            vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
                text.to_string(),
            )))]
        })
        .unwrap_or_default();
    ToolCall::new(ToolCallId::new(entry.id.clone()), entry.name.clone())
        .raw_input(Some(entry.args.clone()))
        .status(status)
        .content(content)
}

/// The `session/update`s that redraw `session` in a client, in conversation
/// order: a user message is a `user_message_chunk`; an assistant message is
/// an `agent_message_chunk` (when it has text) followed by one `tool_call`
/// per tool it ran. System messages are dirge's own context, not chat, and
/// are not replayed.
pub fn replay_updates(session: &Session) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for msg in &session.messages {
        match msg.role {
            MessageRole::User => {
                updates.push(SessionUpdate::UserMessageChunk(text_chunk(&msg.content)));
            }
            MessageRole::Assistant => {
                if !msg.content.is_empty() {
                    updates.push(SessionUpdate::AgentMessageChunk(text_chunk(&msg.content)));
                }
                for call in &msg.tool_calls {
                    updates.push(SessionUpdate::ToolCall(replay_tool_call(call)));
                }
            }
            MessageRole::System => {}
        }
    }
    updates
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory [`SessionStore`] for tests.
    #[derive(Default)]
    pub(crate) struct StubStore {
        pub(crate) sessions: Mutex<HashMap<String, Session>>,
    }

    impl SessionStore for StubStore {
        fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
            Ok(self.sessions.lock().unwrap().get(id).cloned())
        }
        fn save(&self, session: &mut Session) -> anyhow::Result<()> {
            self.sessions
                .lock()
                .unwrap()
                .insert(session.id.to_string(), session.clone());
            Ok(())
        }
    }

    pub(crate) fn saved_conversation() -> Session {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::System, "system prompt");
        s.add_message(MessageRole::User, "list files");
        s.add_message_with_tool_calls(
            MessageRole::Assistant,
            "running ls",
            vec![ToolCallEntry {
                id: "call_1".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
                state: ToolCallState::Completed {
                    result: "a.txt".into(),
                },
            }],
        );
        s.add_message(MessageRole::Assistant, "one file: a.txt");
        s
    }

    #[test]
    fn replay_maps_user_agent_and_tool_messages_in_order() {
        let updates = replay_updates(&saved_conversation());
        assert_eq!(updates.len(), 4, "system message is not replayed");
        assert!(matches!(&updates[0], SessionUpdate::UserMessageChunk(c)
            if matches!(&c.content, ContentBlock::Text(t) if t.text == "list files")));
        assert!(matches!(&updates[1], SessionUpdate::AgentMessageChunk(c)
            if matches!(&c.content, ContentBlock::Text(t) if t.text == "running ls")));
        match &updates[2] {
            SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.tool_call_id.0.as_ref(), "call_1");
                assert_eq!(tc.title, "bash");
                assert_eq!(tc.status, ToolCallStatus::Completed);
                assert_eq!(tc.raw_input, Some(serde_json::json!({"command": "ls"})));
                assert_eq!(tc.content.len(), 1);
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
        assert!(matches!(&updates[3], SessionUpdate::AgentMessageChunk(c)
            if matches!(&c.content, ContentBlock::Text(t) if t.text == "one file: a.txt")));
    }

    #[test]
    fn interrupted_tool_call_replays_as_failed_without_output() {
        let mut s = Session::new("p", "m", 0);
        s.add_message_with_tool_calls(
            MessageRole::Assistant,
            "",
            vec![ToolCallEntry {
                id: "c".into(),
                name: "bash".into(),
                args: serde_json::json!({}),
                state: ToolCallState::Interrupted,
            }],
        );
        let updates = replay_updates(&s);
        assert_eq!(updates.len(), 1, "empty assistant text is not replayed");
        match &updates[0] {
            SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::Failed);
                assert!(tc.content.is_empty());
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[test]
    fn restore_returns_the_saved_session() {
        let store = StubStore::default();
        let mut s = saved_conversation();
        store.save(&mut s).unwrap();
        let got = restore(&store, &s.id).expect("saved session loads");
        assert_eq!(got.messages.len(), s.messages.len());
    }

    #[test]
    fn restore_of_an_unknown_id_is_resource_not_found() {
        let err = restore(&StubStore::default(), "nope").unwrap_err();
        assert_eq!(err.code, agent_client_protocol::ErrorCode::ResourceNotFound);
    }
}
