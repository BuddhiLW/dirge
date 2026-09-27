//! The thread that owns the clojurust runtime. cljrs values are not `Send`,
//! so callers send [`Command`]s and get JSON back.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use cljrs_gc::GcPtr;
use cljrs_runtime::env::env::GlobalEnv;
use cljrs_runtime::tiered::{Env, eval};
use cljrs_runtime::{ExecutionMode, Runtime};
use cljrs_value::{Arity, NativeFn, PersistentVector, Value};
use serde_json::{Value as Json, json};

use super::{bridge, harness};
use crate::addons::domain::{HookPoint, HookReply};
use crate::addons::policy;
use crate::addons::port::{AddonRuntime, HarnessSink};

/// Stack for the isolate thread. The tree-walking evaluator recurses deeply;
/// the cljrs CLI runs with the same 64 MiB.
pub const ISOLATE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// dirge's Clojure host namespace, embedded in the binary.
const HOST_NS: &str = "dirge.addon.host";
const HOST_SRC: &str = include_str!("host.cljc");

/// Private namespace through which a call's arguments reach Clojure.
const BRIDGE_NS: &str = "dirge.bridge";

const GONE: &str = "the addon isolate has stopped";

enum Command {
    Load {
        manifest: PathBuf,
        host_config: Json,
        reply: Sender<Json>,
    },
    CallTool {
        addon_id: String,
        tool: String,
        args: Json,
        reply: Sender<Result<Json, String>>,
    },
    Hook {
        point: HookPoint,
        ctx: Json,
        reply: Sender<Vec<HookReply>>,
    },
    Shutdown {
        reply: Sender<()>,
    },
}

/// Handle to the isolate thread. Cloning is not offered: one owner, shared
/// behind the host's `Arc`.
pub struct Isolate {
    tx: Sender<Command>,
}

impl Isolate {
    /// Start the thread and boot the runtime with `source_roots` on the
    /// classpath. Returns once the host namespace has loaded, or why it did
    /// not.
    pub fn spawn(source_roots: Vec<PathBuf>, sink: Arc<dyn HarnessSink>) -> Result<Self, String> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        std::thread::Builder::new()
            .name("dirge-addons".into())
            .stack_size(ISOLATE_STACK_BYTES)
            .spawn(move || serve(source_roots, sink, ready_tx, rx))
            .map_err(|e| format!("cannot start the addon isolate: {e}"))?;
        ready_rx
            .recv()
            .map_err(|_| "the addon isolate died while booting".to_string())??;
        Ok(Self { tx })
    }

    /// Load one manifest. Answers the host's report: a summary or `{:error}`.
    pub fn load(&self, manifest: &Path, host_config: &Json) -> Json {
        self.ask(|reply| Command::Load {
            manifest: manifest.to_path_buf(),
            host_config: host_config.clone(),
            reply,
        })
        .unwrap_or_else(|e| json!({ "error": e }))
    }

    fn ask<T>(&self, command: impl FnOnce(Sender<T>) -> Command) -> Result<T, String> {
        let (reply, answer) = channel();
        self.tx.send(command(reply)).map_err(|_| GONE.to_string())?;
        answer.recv().map_err(|_| GONE.to_string())
    }
}

impl AddonRuntime for Isolate {
    fn call_tool(&self, addon_id: &str, tool: &str, args: &Json) -> Result<Json, String> {
        self.ask(|reply| Command::CallTool {
            addon_id: addon_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            reply,
        })?
    }

    fn run_hook(&self, point: HookPoint, ctx: &Json) -> Vec<HookReply> {
        self.ask(|reply| Command::Hook {
            point,
            ctx: ctx.clone(),
            reply,
        })
        .unwrap_or_default()
    }

    fn shutdown(&self) {
        let _ = self.ask(|reply| Command::Shutdown { reply });
    }
}

/// The thread body: boot, then answer commands until shut down or dropped.
fn serve(
    roots: Vec<PathBuf>,
    sink: Arc<dyn HarnessSink>,
    ready: Sender<Result<(), String>>,
    rx: Receiver<Command>,
) {
    let mut interp = match Interp::boot(roots, sink) {
        Ok(interp) => {
            let _ = ready.send(Ok(()));
            interp
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    for command in rx {
        match command {
            Command::Load {
                manifest,
                host_config,
                reply,
            } => {
                let path = Json::String(manifest.display().to_string());
                let report = interp
                    .call("load-addon!", vec![path, host_config])
                    .unwrap_or_else(|e| json!({ "error": e }));
                let _ = reply.send(report);
            }
            Command::CallTool {
                addon_id,
                tool,
                args,
                reply,
            } => {
                let out = interp
                    .call("call-tool", vec![addon_id.into(), tool.into(), args])
                    .and_then(|envelope| policy::tool_reply(&envelope));
                let _ = reply.send(out);
            }
            Command::Hook { point, ctx, reply } => {
                let replies = interp
                    .call("run-hook", vec![point.key().into(), ctx])
                    .map(|answer| policy::hook_replies(&answer))
                    .unwrap_or_else(|e| {
                        tracing::warn!(target: "dirge::addon", hook = point.key(), error = %e, "run-hook failed");
                        Vec::new()
                    });
                let _ = reply.send(replies);
            }
            Command::Shutdown { reply } => {
                interp.shutdown();
                let _ = reply.send(());
                return;
            }
        }
    }
    // Every handle dropped without an explicit shutdown.
    interp.shutdown();
}

/// The runtime as the thread holds it.
struct Interp {
    env: Env,
    /// Arguments of the call in flight, read by `dirge.bridge/args`.
    inbox: Rc<RefCell<Vec<Json>>>,
    stopped: bool,
}

impl Interp {
    fn boot(roots: Vec<PathBuf>, sink: Arc<dyn HarnessSink>) -> Result<Self, String> {
        let runtime = Runtime::builder()
            .execution_mode(ExecutionMode::Tiered)
            .source_paths(roots)
            .builtin_source(HOST_NS, HOST_SRC)
            .build()
            .map_err(|e| format!("cannot build the cljrs runtime: {e}"))?;
        cljrs_stdlib::install(&runtime);
        harness::install(runtime.globals(), sink);
        let inbox = Rc::new(RefCell::new(Vec::new()));
        install_bridge(runtime.globals(), inbox.clone());
        let mut interp = Self {
            env: runtime.env("user"),
            inbox,
            stopped: false,
        };
        interp
            .eval_str(&format!("(require '{HOST_NS})"))
            .map_err(|e| format!("cannot load {HOST_NS}: {e}"))?;
        Ok(interp)
    }

    /// `(apply dirge.addon.host/<f> args)`, arguments passed as data rather
    /// than spliced into source text.
    fn call(&mut self, f: &str, args: Vec<Json>) -> Result<Json, String> {
        *self.inbox.borrow_mut() = args;
        let out = self.eval_str(&format!("(apply {HOST_NS}/{f} ({BRIDGE_NS}/args))"));
        self.inbox.borrow_mut().clear();
        out
    }

    fn eval_str(&mut self, src: &str) -> Result<Json, String> {
        let mut parser = cljrs_reader::Parser::new(src.to_string(), "<dirge>".to_string());
        let forms = parser.parse_all().map_err(|e| format!("{e:?}"))?;
        let _frame = cljrs_gc::push_alloc_frame();
        let mut last = Json::Null;
        for form in &forms {
            let value = eval(form, &mut self.env).map_err(|e| e.to_string())?;
            last = bridge::to_json(&value);
        }
        Ok(last)
    }

    fn shutdown(&mut self) {
        if !self.stopped {
            self.stopped = true;
            if let Err(e) = self.call("shutdown-all!", Vec::new()) {
                tracing::warn!(target: "dirge::addon", error = %e, "addon shutdown failed");
            }
        }
    }
}

/// `(dirge.bridge/args)`: the in-flight call's arguments as a vector,
/// converted on the isolate thread so the values are born inside the eval
/// that uses them.
fn install_bridge(globals: &Arc<GlobalEnv>, inbox: Rc<RefCell<Vec<Json>>>) {
    let native = NativeFn::with_closure(format!("{BRIDGE_NS}/args"), Arity::Fixed(0), move |_| {
        let items = inbox
            .borrow()
            .iter()
            .map(bridge::to_clj)
            .collect::<Vec<_>>();
        Ok(Value::Vector(GcPtr::new(PersistentVector::from_iter(
            items,
        ))))
    });
    globals.intern(
        BRIDGE_NS,
        Arc::from("args"),
        Value::NativeFunction(GcPtr::new(native)),
    );
    globals.mark_loaded(BRIDGE_NS);
}
