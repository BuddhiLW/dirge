//! The addon isolate: one OS thread that owns the clojurust runtime.
//!
//! clojurust values are garbage-collected and not `Send`, so every
//! interpreter object lives on this thread. Callers send [`AddonCmd`]s over
//! a channel and receive JSON-string replies; nothing but plain strings
//! crosses the thread boundary.

use std::path::PathBuf;

/// Stack size for the isolate thread. Deeply recursive Clojure code and the
/// tree-walking evaluator need far more than the default 2 MiB.
pub const ISOLATE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// A request to the isolate thread.
#[derive(Debug)]
pub enum AddonCmd {
    /// Load the addon described by the manifest at `manifest_path` and
    /// run its constructor and initializer.
    Load { manifest_path: PathBuf },
    /// Invoke an addon tool with JSON-encoded arguments.
    CallTool {
        addon_id: String,
        tool: String,
        json_args: String,
    },
    /// Shut every loaded addon down and stop the thread.
    Shutdown,
}

/// Lifecycle state of one loaded addon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonState {
    Loaded,
    Initialized,
    Failed,
    Stopped,
}
