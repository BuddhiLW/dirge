//! `:dirge/event` projection of the host's own lifecycle events.

use serde_json::json;

use super::events::{HostEvent, mcp_changes, project_host};

#[test]
fn mcp_connections_are_heard_by_server_name() {
    assert_eq!(
        project_host(&HostEvent::McpConnected {
            server: "hive".into()
        }),
        json!({"event": "mcp-connected", "server": "hive"})
    );
    assert_eq!(
        project_host(&HostEvent::McpDisconnected {
            server: "hive".into()
        }),
        json!({"event": "mcp-disconnected", "server": "hive"})
    );
}

#[test]
fn session_bounds_are_heard_with_their_ids() {
    assert_eq!(
        project_host(&HostEvent::SessionStart {
            session_id: Some("s1".into()),
            cwd: "/w".into(),
            first_prompt: true,
            mcp_servers: vec!["hive".into()],
        }),
        json!({"event": "session-start", "session-id": "s1", "cwd": "/w",
               "first-prompt?": true, "mcp-servers": ["hive"]})
    );
    assert_eq!(
        project_host(&HostEvent::SessionEnd {
            session_id: None,
            cwd: "/w".into(),
            reason: "exit".into(),
        }),
        json!({"event": "session-end", "session-id": null, "cwd": "/w", "reason": "exit"})
    );
}

#[test]
fn a_reload_is_heard_with_what_it_changed() {
    assert_eq!(
        project_host(&HostEvent::AddonsReloaded {
            loaded: vec!["echo".into()],
            failed: 1,
            tools_added: vec!["count".into()],
            tools_removed: vec![],
        }),
        json!({"event": "addons-reloaded", "loaded": ["echo"], "failed": 1,
               "tools-added": ["count"], "tools-removed": []})
    );
}

#[test]
fn mcp_changes_name_servers_that_came_and_went() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        mcp_changes(&s(&["a", "b"]), &s(&["b", "c"])),
        vec![
            HostEvent::McpConnected { server: "c".into() },
            HostEvent::McpDisconnected { server: "a".into() },
        ]
    );
    assert!(mcp_changes(&s(&["a"]), &s(&["a"])).is_empty());
}
