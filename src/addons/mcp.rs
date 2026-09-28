//! `dirge.harness/mcp-call`: addon code reaching the MCP servers dirge is
//! connected to, over the same connections dirge's own MCP tools use.
//!
//! Addons are code the user installed and run in-process, so a call here is
//! not put through the per-tool permission prompt, the same trust Janet
//! plugins get for `harness/call-tool`.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use super::port::McpGateway;
use crate::extras::mcp::McpClientManager;
use crate::extras::mcp::client::SharedConnection;
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// Longest an addon waits for one MCP call.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

type Connections = Vec<(String, Arc<SharedConnection>)>;

static LIVE: OnceLock<Mutex<Connections>> = OnceLock::new();

fn live() -> &'static Mutex<Connections> {
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Make `manager`'s connections reachable from addon code. Called wherever
/// a manager finishes connecting; a reconnect swaps the peer inside the
/// shared connection, so it needs no republish.
pub fn publish(manager: &McpClientManager) {
    *live().lock_ignore_poison() = manager.connections_snapshot();
}

/// The published connections, driven on the runtime dirge started on.
pub struct LiveMcp {
    handle: tokio::runtime::Handle,
}

impl LiveMcp {
    /// The gateway for the runtime this is called on; `None` outside one.
    pub fn current() -> Option<Self> {
        tokio::runtime::Handle::try_current()
            .ok()
            .map(|handle| Self { handle })
    }

    fn connection(&self, server: &str) -> Result<Arc<SharedConnection>, String> {
        live()
            .lock_ignore_poison()
            .iter()
            .find(|(name, _)| name == server)
            .map(|(_, conn)| conn.clone())
            .ok_or_else(|| format!("no MCP server named {server} is connected"))
    }
}

impl McpGateway for LiveMcp {
    fn servers(&self) -> Vec<String> {
        live()
            .lock_ignore_poison()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn call(&self, server: &str, tool: &str, args: &Value) -> Result<Value, String> {
        // The addon isolate is a plain thread. Anywhere else, blocking here
        // would park a runtime worker the call itself needs.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err("mcp-call must not run on an async runtime thread".to_string());
        }
        let conn = self.connection(server)?;
        let mut params = rmcp::model::CallToolRequestParams::new(tool.to_string());
        if let Some(object) = args.as_object() {
            params = params.with_arguments(object.clone());
        }
        let answer = self.handle.block_on(async move {
            let peer = conn.current_peer().await;
            tokio::time::timeout(CALL_TIMEOUT, peer.call_tool(params)).await
        });
        match answer {
            Ok(Ok(result)) => serde_json::to_value(result).map_err(|e| e.to_string()),
            Ok(Err(e)) => Err(format!("MCP tool error ({server}::{tool}): {e}")),
            Err(_) => Err(format!(
                "MCP tool {server}::{tool} timed out after {}s",
                CALL_TIMEOUT.as_secs()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_servers_are_an_error_not_a_hang() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let gateway = LiveMcp {
            handle: rt.handle().clone(),
        };
        let err = std::thread::spawn(move || gateway.call("nowhere", "t", &json!({})))
            .join()
            .unwrap()
            .unwrap_err();
        assert!(err.contains("nowhere"), "{err}");
    }

    #[test]
    fn calls_from_a_runtime_thread_are_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            LiveMcp::current()
                .expect("inside a runtime")
                .call("any", "t", &json!({}))
                .unwrap_err()
        });
        assert!(err.contains("async runtime"), "{err}");
    }
}
