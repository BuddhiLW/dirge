//! The [`AddonHooks`] adapter: the running [`AddonHost`] behind the narrow
//! fold/post port, and the one place outside `mod.rs` that may hand out the
//! process-wide host as that port.

use std::sync::Arc;

use serde_json::Value;

use super::host::AddonHost;
use super::port::AddonHooks;

impl AddonHooks for AddonHost {
    fn listens_key(&self, key: &str) -> bool {
        AddonHost::listens_key(self, key)
    }

    fn fold(&self, key: &str, ctx: &Value) -> Option<Value> {
        AddonHost::fold(self, key, ctx)
    }

    fn post(&self, key: &str, ctx: &Value) {
        AddonHost::post(self, key, ctx)
    }
}

/// The addon host as an [`AddonHooks`] port, for a composition root to
/// inject. `None` when no host runs.
pub fn current() -> Option<Arc<dyn AddonHooks>> {
    super::global().map(|host| host as Arc<dyn AddonHooks>)
}
