//! The `dirge.harness` namespace: host functions callable from addon code.
//!
//! Functions here are registered with the interpreter on the isolate thread
//! and forward to dirge subsystems that are safe to reach from any thread
//! (for example the notification queue). Nothing is registered yet.

/// Namespace under which host functions are exposed to addons.
pub const HARNESS_NS: &str = "dirge.harness";
