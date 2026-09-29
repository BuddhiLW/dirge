//! The port the agent reaches addons through: their tools, their tool-call
//! and compaction hooks, and the step that opens a run with their prompt
//! hooks. The composition root installs the one this process uses; a build
//! or a process without one gets nothing from any of them.

use std::sync::{Arc, OnceLock};

use crate::agent::agent_loop::LoopTool;
use crate::agent::agent_loop::hooks::OpenRunFn;
use crate::agent::agent_loop::types::{CompactionHooks, LoopConfig};
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

/// What addons add to an agent and to its runs.
pub trait AddonHooks: Send + Sync + 'static {
    /// The addons' tools, authorized through `permission` and `ask_tx`.
    fn loop_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<Arc<dyn LoopTool>>;

    /// Install the addons' tool-call hooks on `config`, after whatever is
    /// there.
    fn install_tool_hooks(&self, config: &mut LoopConfig);

    /// The step that runs the addons' prompt hooks as a run of `session_id`
    /// opens. `None` when no addon listens on them.
    fn open_run(&self, session_id: Option<String>, first_prompt: bool) -> Option<OpenRunFn>;

    /// The addons' compaction hooks for the runs of `session_id`. `None`
    /// when no addon listens on them.
    fn compaction_hooks(&self, session_id: Option<String>) -> Option<CompactionHooks>;
}

static INSTALLED: OnceLock<Arc<dyn AddonHooks>> = OnceLock::new();

/// Make `hooks` the addons this process reaches. The first install wins.
#[cfg_attr(not(feature = "addons"), allow(dead_code))]
pub fn install(hooks: Arc<dyn AddonHooks>) {
    let _ = INSTALLED.set(hooks);
}

/// The addons of this process, once some are installed.
pub fn installed() -> Option<Arc<dyn AddonHooks>> {
    INSTALLED.get().cloned()
}
