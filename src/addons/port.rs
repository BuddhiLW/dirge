//! Traits the addon host depends on: the runtime and the notification sink.

use serde_json::Value;

use super::domain::{HookPoint, HookReply};

/// A running addon runtime. Calls are synchronous round trips: the only
/// adapter serializes them onto one interpreter thread, so an async caller
/// wraps them in `spawn_blocking`.
pub trait AddonRuntime: Send + Sync + 'static {
    /// Invoke `tool` of `addon_id` with JSON `args`. `Ok` carries the
    /// handler's return value, `Err` a message fit for the model.
    fn call_tool(&self, addon_id: &str, tool: &str, args: &Value) -> Result<Value, String>;

    /// Call every addon's `point` hook with `ctx`, in load order.
    fn run_hook(&self, point: HookPoint, ctx: &Value) -> Vec<HookReply>;

    /// Shut every addon down. Idempotent.
    fn shutdown(&self);
}

/// Severity of a harness notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    /// `:info` / `:warn` / `:error` (colon optional); anything else is info.
    pub fn parse(s: &str) -> Level {
        match s.trim_start_matches(':') {
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }
}

/// Where `dirge.harness` natives deliver what an addon says to the user.
pub trait HarnessSink: Send + Sync + 'static {
    fn notify(&self, addon_level: Level, message: &str);
}

#[cfg(test)]
mod tests {
    use super::Level;

    #[test]
    fn levels_parse_with_or_without_colon_and_default_to_info() {
        assert_eq!(Level::parse(":warn"), Level::Warn);
        assert_eq!(Level::parse("error"), Level::Error);
        assert_eq!(Level::parse(":debug"), Level::Info);
    }
}
