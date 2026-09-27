//! `dirge.harness/call-tool`: addon code calling dirge's own loop tools,
//! built-ins and MCP tools alike, the way Janet plugins do through
//! `harness/call-tool`. Permission checks stay inside each tool.

use std::sync::Arc;

use serde_json::Value;
use tokio::runtime::Handle;

use super::port::ToolGateway;
use crate::agent::agent_loop::LoopTool;
use crate::plugin::tool_bridge;

/// The tools a call may reach, read when the call is made.
pub type ToolSource = Arc<dyn Fn() -> Vec<Arc<dyn LoopTool>> + Send + Sync>;

/// Names of the tools addons registered, read when the call is made.
pub type AddonToolNames = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Why `name` may not be called from addon code, or `None` if it may.
pub fn refusal(name: &str, addon_tools: &[String]) -> Option<String> {
    if tool_bridge::NEVER_CALLABLE.contains(&name) {
        return Some(format!(
            "'{name}' cannot be called from an addon: subagents run isolated from addon code"
        ));
    }
    if addon_tools.iter().any(|n| n == name) {
        return Some(format!(
            "'{name}' is an addon tool and cannot be called from an addon: its handler \
             needs the addon isolate, which is blocked awaiting this call"
        ));
    }
    None
}

/// Loop tools driven on the runtime dirge started on.
pub struct LoopTools {
    handle: Handle,
    tools: ToolSource,
    addon_tools: AddonToolNames,
}

impl LoopTools {
    pub fn new(handle: Handle, tools: ToolSource, addon_tools: AddonToolNames) -> Self {
        Self {
            handle,
            tools,
            addon_tools,
        }
    }

    /// The agent's published tool set on the current runtime; `None`
    /// outside one.
    pub fn live(addon_tools: AddonToolNames) -> Option<Self> {
        let tools: ToolSource = Arc::new(tool_bridge::live_tools);
        Handle::try_current()
            .ok()
            .map(|handle| Self::new(handle, tools, addon_tools))
    }
}

impl ToolGateway for LoopTools {
    fn names(&self) -> Vec<String> {
        let refused = (self.addon_tools)();
        (self.tools)()
            .iter()
            .map(|t| t.name().to_string())
            .filter(|name| refusal(name, &refused).is_none())
            .collect()
    }

    fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        // The addon isolate is a plain thread. Anywhere else, blocking here
        // would park a runtime worker the call itself needs.
        if Handle::try_current().is_ok() {
            return Err("call-tool must not run on an async runtime thread".to_string());
        }
        if let Some(reason) = refusal(name, &(self.addon_tools)()) {
            return Err(reason);
        }
        let tools = (self.tools)();
        self.handle.block_on(tool_bridge::execute_in(
            &tools,
            name,
            args.clone(),
            "addon-call-tool",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use serde_json::json;

    use super::*;
    use crate::agent::agent_loop::LoopToolResult;
    use crate::agent::agent_loop::tool::{AbortSignal, LoopToolUpdate};

    #[derive(Debug)]
    struct Echo(&'static str);

    impl LoopTool for Echo {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(|| json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                Ok(LoopToolResult {
                    content: vec![
                        json!({"type": "text", "text": format!("echo {}", args["text"])}),
                    ],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    fn gateway(rt: &tokio::runtime::Runtime) -> LoopTools {
        LoopTools::new(
            rt.handle().clone(),
            Arc::new(|| -> Vec<Arc<dyn LoopTool>> {
                vec![
                    Arc::new(Echo("read")),
                    Arc::new(Echo("count-rows")),
                    Arc::new(Echo("task")),
                ]
            }),
            Arc::new(|| vec!["count-rows".to_string()]),
        )
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn built_in_tools_are_callable_addon_tools_and_subagents_are_not() {
        assert!(refusal("read", &["count-rows".into()]).is_none());
        let addon = refusal("count-rows", &["count-rows".into()]).expect("refused");
        assert!(addon.contains("addon isolate"), "{addon}");
        let task = refusal("task", &[]).expect("refused");
        assert!(task.contains("isolated"), "{task}");
    }

    #[test]
    fn names_advertise_only_what_is_callable() {
        let rt = runtime();
        assert_eq!(gateway(&rt).names(), vec!["read".to_string()]);
    }

    #[test]
    fn a_call_from_a_plain_thread_runs_the_tool_on_the_runtime() {
        let rt = runtime();
        let gw = gateway(&rt);
        let answer = std::thread::spawn(move || gw.call("read", &json!({"text": "hi"})))
            .join()
            .unwrap();
        assert_eq!(answer, Ok("echo \"hi\"".to_string()));
    }

    #[test]
    fn refused_and_unknown_tools_answer_errors() {
        let rt = runtime();
        let gw = gateway(&rt);
        let answers = std::thread::spawn(move || {
            [
                gw.call("count-rows", &json!({})),
                gw.call("nowhere", &json!({})),
                gw.call("read", &json!("not an object")),
            ]
        })
        .join()
        .unwrap();
        assert!(answers[0].as_ref().unwrap_err().contains("addon tool"));
        assert!(answers[1].as_ref().unwrap_err().contains("no tool named"));
        assert!(answers[2].as_ref().unwrap_err().contains("JSON object"));
    }

    #[test]
    fn calls_from_a_runtime_thread_are_refused() {
        let rt = runtime();
        let gw = gateway(&rt);
        let err = rt.block_on(async move { gw.call("read", &json!({})).unwrap_err() });
        assert!(err.contains("async runtime"), "{err}");
    }
}
