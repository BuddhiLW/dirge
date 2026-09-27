//! The thread that owns the clojurust runtime. cljrs values are not `Send`,
//! so callers send [`Command`]s and get JSON back.

use std::cell::{Cell, RefCell};
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
use crate::addons::port::{AddonRuntime, Harness};
use crate::addons::{layout, policy};

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
    Unload {
        addon_id: String,
        reply: Sender<()>,
    },
    ReloadSources {
        files: Vec<PathBuf>,
        reply: Sender<Vec<(PathBuf, String)>>,
    },
    SetRoots {
        roots: Vec<PathBuf>,
        reply: Sender<()>,
    },
    CallTool {
        addon_id: String,
        tool: String,
        args: Json,
        reply: Sender<Result<Json, String>>,
    },
    Slash {
        addon_id: String,
        name: String,
        ctx: Json,
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

/// A command, and whether its caller is the thread running dirge's event
/// loop. dirge runs a single-threaded runtime, so that caller stops the loop
/// until the answer comes: nothing the isolate does meanwhile may wait on it
/// (an MCP call does).
type Envelope = (bool, Command);

thread_local! {
    /// Set on the thread that runs dirge's event loop. A runtime context is
    /// not the test: `spawn_blocking` threads have one too, and blocking them
    /// is exactly how the rest of dirge reaches the isolate safely.
    static EVENT_LOOP: Cell<bool> = const { Cell::new(false) };
}

/// Mark the calling thread as the one that runs dirge's single-threaded
/// event loop: while it waits on the isolate, addon code may not wait on
/// that loop in turn.
pub fn mark_event_loop_thread() {
    EVENT_LOOP.with(|marked| marked.set(true));
}

/// Handle to the isolate thread. Cloning is not offered: one owner, shared
/// behind the host's `Arc`.
pub struct Isolate {
    tx: Sender<Envelope>,
}

impl Isolate {
    /// Start the thread and boot the runtime with `source_roots` on the
    /// classpath, `harness` behind `dirge.harness`, and the IAddon protocol
    /// of `protocol_ns` bound. Returns once the host is ready, or why it is
    /// not.
    pub fn spawn(
        source_roots: Vec<PathBuf>,
        harness: Harness,
        protocol_ns: &str,
    ) -> Result<Self, String> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let protocol_ns = protocol_ns.to_string();
        std::thread::Builder::new()
            .name("dirge-addons".into())
            .stack_size(ISOLATE_STACK_BYTES)
            .spawn(move || serve(source_roots, harness, protocol_ns, ready_tx, rx))
            .map_err(|e| format!("cannot start the addon isolate: {e}"))?;
        ready_rx
            .recv()
            .map_err(|_| "the addon isolate died while booting".to_string())??;
        Ok(Self { tx })
    }

    fn ask<T>(&self, command: impl FnOnce(Sender<T>) -> Command) -> Result<T, String> {
        let (reply, answer) = channel();
        let on_event_loop = EVENT_LOOP.with(Cell::get);
        self.tx
            .send((on_event_loop, command(reply)))
            .map_err(|_| GONE.to_string())?;
        answer.recv().map_err(|_| GONE.to_string())
    }
}

impl AddonRuntime for Isolate {
    fn load(&self, manifest: &Path, host_config: &Json) -> Json {
        self.ask(|reply| Command::Load {
            manifest: manifest.to_path_buf(),
            host_config: host_config.clone(),
            reply,
        })
        .unwrap_or_else(|e| json!({ "error": e }))
    }

    fn unload(&self, addon_id: &str) {
        let _ = self.ask(|reply| Command::Unload {
            addon_id: addon_id.to_string(),
            reply,
        });
    }

    fn reload_sources(&self, files: &[PathBuf]) -> Vec<(PathBuf, String)> {
        self.ask(|reply| Command::ReloadSources {
            files: files.to_vec(),
            reply,
        })
        .unwrap_or_else(|e| files.iter().map(|f| (f.clone(), e.clone())).collect())
    }

    fn set_source_roots(&self, roots: &[PathBuf]) {
        let _ = self.ask(|reply| Command::SetRoots {
            roots: roots.to_vec(),
            reply,
        });
    }

    fn call_tool(&self, addon_id: &str, tool: &str, args: &Json) -> Result<Json, String> {
        self.ask(|reply| Command::CallTool {
            addon_id: addon_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            reply,
        })?
    }

    fn run_command(&self, addon_id: &str, name: &str, ctx: &Json) -> Result<Json, String> {
        self.ask(|reply| Command::Slash {
            addon_id: addon_id.to_string(),
            name: name.to_string(),
            ctx: ctx.clone(),
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
    harness: Harness,
    protocol_ns: String,
    ready: Sender<Result<(), String>>,
    rx: Receiver<Envelope>,
) {
    let mut interp = match Interp::boot(roots, harness, &protocol_ns) {
        Ok(interp) => {
            let _ = ready.send(Ok(()));
            interp
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    for (on_runtime, command) in rx {
        interp.caller_on_runtime.set(on_runtime);
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
            Command::Unload { addon_id, reply } => {
                if let Err(e) = interp.call("shutdown-addon!", vec![addon_id.clone().into()]) {
                    tracing::warn!(target: "dirge::addon", addon = %addon_id, error = %e, "unload failed");
                }
                let _ = reply.send(());
            }
            Command::ReloadSources { files, reply } => {
                let _ = reply.send(interp.reload_sources(&files));
            }
            Command::SetRoots { roots, reply } => {
                interp.set_roots(roots);
                let _ = reply.send(());
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
            Command::Slash {
                addon_id,
                name,
                ctx,
                reply,
            } => {
                let out = interp
                    .call("run-command", vec![addon_id.into(), name.into(), ctx])
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
    globals: Arc<GlobalEnv>,
    /// The roots `require` searches, kept to name the namespace of a file.
    roots: Vec<PathBuf>,
    /// Arguments of the call in flight, read by `dirge.bridge/args`.
    inbox: Rc<RefCell<Vec<Json>>>,
    /// True while serving a caller that blocks dirge's runtime; the harness
    /// refuses anything that would wait on it.
    caller_on_runtime: Rc<Cell<bool>>,
    stopped: bool,
}

impl Interp {
    fn boot(roots: Vec<PathBuf>, harness: Harness, protocol_ns: &str) -> Result<Self, String> {
        let runtime = Runtime::builder()
            .execution_mode(ExecutionMode::Tiered)
            .source_paths(roots.clone())
            .builtin_source(HOST_NS, HOST_SRC)
            .build()
            .map_err(|e| format!("cannot build the cljrs runtime: {e}"))?;
        cljrs_stdlib::install(&runtime);
        let caller_on_runtime = Rc::new(Cell::new(false));
        harness::install(runtime.globals(), harness, caller_on_runtime.clone());
        let inbox = Rc::new(RefCell::new(Vec::new()));
        install_bridge(runtime.globals(), inbox.clone());
        let mut interp = Self {
            env: runtime.env("user"),
            globals: runtime.globals().clone(),
            roots,
            inbox,
            caller_on_runtime,
            stopped: false,
        };
        interp
            .eval_str(&format!("(require '{HOST_NS})"))
            .map_err(|e| format!("cannot load {HOST_NS}: {e}"))?;
        interp
            .call("use-protocol!", vec![protocol_ns.into()])
            .and_then(|answer| policy::tool_reply(&answer))
            .map_err(|e| format!("cannot bind the IAddon protocol {protocol_ns}: {e}"))?;
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

    fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.globals.set_source_paths(roots.clone());
        self.roots = roots;
    }

    /// Evaluate `files` again; the ones that failed, with why. Files outside
    /// every root cannot be named as a namespace and are left alone.
    fn reload_sources(&mut self, files: &[PathBuf]) -> Vec<(PathBuf, String)> {
        let sources: Vec<Json> = files
            .iter()
            .filter_map(|file| {
                let ns = self
                    .roots
                    .iter()
                    .find_map(|root| layout::namespace_of(root, file))?;
                Some(json!({ "file": file.display().to_string(), "ns": ns }))
            })
            .collect();
        match self.call("reload-sources!", vec![Json::Array(sources)]) {
            Ok(answer) => policy::source_errors(&answer),
            Err(e) => files.iter().map(|f| (f.clone(), e.clone())).collect(),
        }
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
