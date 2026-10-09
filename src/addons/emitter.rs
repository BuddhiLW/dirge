//! The [`AddonEmitter`] adapter: the running [`AddonHost`] behind the narrow
//! emit/post port, and the one place outside `mod.rs` that may hand out the
//! process-wide host as that port.

use std::sync::Arc;

use serde_json::Value;

use super::domain::HookReply;
use super::host::AddonHost;
use super::port::AddonEmitter;

impl AddonEmitter for AddonHost {
    fn listens_key(&self, key: &str) -> bool {
        AddonHost::listens_key(self, key)
    }

    fn emit(&self, key: &str, ctx: &Value) -> Vec<HookReply> {
        AddonHost::emit(self, key, ctx)
    }

    fn post(&self, key: &str, ctx: &Value) {
        AddonHost::post(self, key, ctx)
    }
}

/// The addon host as an [`AddonEmitter`], for a composition root to inject.
/// `None` when no host runs.
pub fn current() -> Option<Arc<dyn AddonEmitter>> {
    super::global().map(|host| host as Arc<dyn AddonEmitter>)
}
