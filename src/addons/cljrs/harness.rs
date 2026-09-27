//! The `dirge.harness` namespace addon code calls.

use std::sync::Arc;

use cljrs_gc::GcPtr;
use cljrs_runtime::env::env::GlobalEnv;
use cljrs_value::{Arity, NativeFn, Value, ValueResult};

use crate::addons::port::{HarnessSink, Level};

/// Namespace addon code requires to reach dirge.
pub const HARNESS_NS: &str = "dirge.harness";

/// Register `dirge.harness` into `globals`.
///
/// - `(notify msg)` / `(notify msg level)`: a line in dirge's chat area;
///   `level` is `:info` (default), `:warn` or `:error`.
/// - `(log level msg)`: a `tracing` event on the `dirge::addon` target.
/// - `(cwd)`: dirge's working directory.
/// - `(version)`: the dirge version string.
pub fn install(globals: &Arc<GlobalEnv>, sink: Arc<dyn HarnessSink>) {
    define(globals, "notify", Arity::Variadic { min: 1 }, move |args| {
        let level = args.get(1).map_or(Level::Info, level_of);
        sink.notify(level, &text(&args[0]));
        Ok(Value::Nil)
    });
    define(globals, "log", Arity::Fixed(2), |args| {
        let message = text(&args[1]);
        match level_of(&args[0]) {
            Level::Error => tracing::error!(target: "dirge::addon", "{message}"),
            Level::Warn => tracing::warn!(target: "dirge::addon", "{message}"),
            Level::Info => tracing::info!(target: "dirge::addon", "{message}"),
        }
        Ok(Value::Nil)
    });
    define(globals, "cwd", Arity::Fixed(0), |_| {
        let cwd = std::env::current_dir().unwrap_or_default();
        Ok(Value::Str(GcPtr::new(cwd.display().to_string())))
    });
    define(globals, "version", Arity::Fixed(0), |_| {
        Ok(Value::Str(GcPtr::new(
            env!("CARGO_PKG_VERSION").to_string(),
        )))
    });
    globals.mark_loaded(HARNESS_NS);
}

fn define(
    globals: &Arc<GlobalEnv>,
    name: &str,
    arity: Arity,
    f: impl Fn(&[Value]) -> ValueResult<Value> + 'static,
) {
    let native = NativeFn::with_closure(format!("{HARNESS_NS}/{name}"), arity, f);
    globals.intern(
        HARNESS_NS,
        Arc::from(name),
        Value::NativeFunction(GcPtr::new(native)),
    );
}

/// A string argument as text; anything else printed.
fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.get().clone(),
        other => other.to_string(),
    }
}

fn level_of(v: &Value) -> Level {
    match v {
        Value::Keyword(k) => Level::parse(&k.get().name),
        Value::Str(s) => Level::parse(s.get()),
        _ => Level::Info,
    }
}
