//! Per-session MCP servers an ACP client declares in `session/new`.
//!
//! The ACP spec lets the client hand the agent the MCP servers a session
//! should reach (`NewSessionRequest::mcp_servers`). dirge used to drop that
//! list: every ACP prompt built its agent with no MCP manager at all. This
//! module is the pure half of honouring it, lowering the wire shape onto
//! dirge's own [`McpServerConfig`] table so the session can connect them with
//! the same [`McpClientManager`](crate::extras::mcp::McpClientManager) the
//! interactive UI uses.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{McpCapabilities, McpServer};

use crate::extras::mcp::McpClientManager;
use crate::extras::mcp::config::McpServerConfig;

/// The MCP transports dirge can reach for a client-declared server. Stdio is
/// implied by the protocol; HTTP rides dirge's URL transport. SSE is not
/// advertised: dirge's URL transport speaks streamable HTTP.
pub fn capabilities() -> McpCapabilities {
    let mut caps = McpCapabilities::default();
    caps.http = true;
    caps
}

/// The dirge config table for a session's declared servers, keyed by the
/// client's server name. A later entry with a name already seen replaces the
/// earlier one, as a JSON object would. Transports dirge cannot reach (SSE,
/// and anything a newer protocol adds) are left out and named in the second
/// value so the caller can log them.
pub fn to_configs(servers: &[McpServer]) -> (HashMap<String, McpServerConfig>, Vec<String>) {
    let mut configs = HashMap::new();
    let mut skipped = Vec::new();
    for server in servers {
        match server {
            McpServer::Stdio(s) => {
                configs.insert(
                    s.name.clone(),
                    McpServerConfig::Command {
                        command: s.command.to_string_lossy().into_owned(),
                        args: s.args.clone(),
                        env: s
                            .env
                            .iter()
                            .map(|e| (e.name.clone(), e.value.clone()))
                            .collect(),
                        allow_external_paths: false,
                    },
                );
            }
            McpServer::Http(h) => {
                configs.insert(
                    h.name.clone(),
                    McpServerConfig::Url {
                        url: h.url.clone(),
                        headers: h
                            .headers
                            .iter()
                            .map(|e| (e.name.clone(), e.value.clone()))
                            .collect(),
                        allow_external_paths: false,
                    },
                );
            }
            McpServer::Sse(s) => skipped.push(s.name.clone()),
            #[allow(unreachable_patterns)]
            _ => skipped.push("<unknown transport>".to_string()),
        }
    }
    (configs, skipped)
}

/// Connect a session's declared servers. `None` when the client declared
/// none that dirge can reach, so a session without servers costs nothing.
/// A server that fails to connect is recorded as failed by the manager and
/// the rest still connect, as in the interactive UI.
pub async fn connect(servers: &[McpServer]) -> Option<Arc<McpClientManager>> {
    let (configs, skipped) = to_configs(servers);
    for name in &skipped {
        tracing::warn!(
            "ACP session MCP server '{name}' uses a transport dirge does not reach; skipped"
        );
    }
    if configs.is_empty() {
        return None;
    }
    Some(Arc::new(McpClientManager::connect_all(&configs).await))
}

/// The MCP manager the session `id` connected in `session/new`, if any.
pub(super) async fn for_session(
    sessions: &super::SessionMap,
    id: &str,
) -> Option<Arc<McpClientManager>> {
    sessions.lock().await.get(id).and_then(|s| s.mcp.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(json: serde_json::Value) -> Vec<McpServer> {
        serde_json::from_value(json).expect("valid ACP mcpServers")
    }

    #[test]
    fn stdio_server_lowers_to_a_command_with_its_args_and_env() {
        let servers = decode(serde_json::json!([{
            "name": "fs",
            "command": "/usr/bin/fs-mcp",
            "args": ["--root", "/tmp"],
            "env": [{"name": "TOKEN", "value": "x"}]
        }]));
        let (configs, skipped) = to_configs(&servers);
        assert!(skipped.is_empty());
        match configs.get("fs") {
            Some(McpServerConfig::Command {
                command,
                args,
                env,
                allow_external_paths,
            }) => {
                assert_eq!(command, "/usr/bin/fs-mcp");
                assert_eq!(args, &vec!["--root".to_string(), "/tmp".to_string()]);
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("x"));
                assert!(!allow_external_paths);
            }
            other => panic!("expected a command config, got {other:?}"),
        }
    }

    #[test]
    fn http_server_lowers_to_a_url_with_headers() {
        let servers = decode(serde_json::json!([{
            "type": "http",
            "name": "hive",
            "url": "http://localhost:7910/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer t"}]
        }]));
        let (configs, skipped) = to_configs(&servers);
        assert!(skipped.is_empty());
        match configs.get("hive") {
            Some(McpServerConfig::Url { url, headers, .. }) => {
                assert_eq!(url, "http://localhost:7910/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer t")
                );
            }
            other => panic!("expected a url config, got {other:?}"),
        }
    }

    #[test]
    fn sse_server_is_skipped_and_named() {
        let servers = decode(serde_json::json!([{
            "type": "sse", "name": "old", "url": "http://x/sse", "headers": []
        }]));
        let (configs, skipped) = to_configs(&servers);
        assert!(configs.is_empty());
        assert_eq!(skipped, vec!["old".to_string()]);
    }

    #[test]
    fn empty_list_yields_no_servers() {
        let (configs, skipped) = to_configs(&[]);
        assert!(configs.is_empty() && skipped.is_empty());
    }

    #[test]
    fn capabilities_advertise_http_but_not_sse() {
        let caps = capabilities();
        assert!(caps.http);
        assert!(!caps.sse);
    }
}
