//! L2 boundary: the only code that touches processes and files. Every
//! function returns a `Result`; failures are values.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};

use super::domain::{Exited, HookCommand, HookError};

/// Port: runs one hook command against a serialized payload.
pub trait HookRunner: Send + Sync {
    fn run(
        &self,
        cmd: &HookCommand,
        payload: &str,
        project_dir: &Path,
    ) -> Result<Exited, HookError>;
}

/// Adapter: `sh -c <command>`, payload on stdin, killed at its timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShellRunner;

impl HookRunner for ShellRunner {
    fn run(
        &self,
        cmd: &HookCommand,
        payload: &str,
        project_dir: &Path,
    ) -> Result<Exited, HookError> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&cmd.command)
            .env("CLAUDE_PROJECT_DIR", project_dir)
            .env("DIRGE_PROJECT_DIR", project_dir)
            .env("DIRGE_HOOK", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| HookError::SpawnFailed(e.to_string()))?;

        let input = payload.to_string();
        let stdin = child.stdin.take();
        let writer = std::thread::spawn(move || {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(input.as_bytes());
            }
        });
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);

        let secs = cmd.timeout_secs();
        let deadline = Instant::now() + Duration::from_secs(secs);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(HookError::TimedOut(secs));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => break Err(HookError::SpawnFailed(e.to_string())),
            }
        };
        let _ = writer.join();
        let stdout = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
        let stderr = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
        status.map(|status| Exited {
            code: status.code(),
            stdout,
            stderr,
        })
    }
}

/// Port: hears an event outside [`super::domain::HookEvent::ALL`] beside the
/// entries configured for it. The events dirge fires itself reach addons
/// through their own hook points instead.
pub trait HookListener: Send + Sync {
    /// Whether anything would hear `event`; checked before any payload is
    /// built.
    fn listens(&self, event: &str) -> bool;

    /// One answer per party that heard `event`, each read like a command's.
    fn hear(&self, event: &str, payload: &str) -> Vec<Result<Exited, HookError>>;
}

/// Parts keyed by name, open: a new one is one [`Registry::install`], and
/// installing under a taken name replaces the part (an addon reload does).
pub struct Registry<T: ?Sized>(RwLock<BTreeMap<String, Arc<T>>>);

impl<T: ?Sized> Default for Registry<T> {
    fn default() -> Self {
        Self(RwLock::new(BTreeMap::new()))
    }
}

impl<T: ?Sized> Registry<T> {
    pub fn install(&self, name: &str, part: Arc<T>) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), part);
    }

    pub fn get(&self, name: &str) -> Option<Arc<T>> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Every part with its name, in name order.
    pub fn all(&self) -> Vec<(String, Arc<T>)> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, part)| (name.clone(), part.clone()))
            .collect()
    }
}

/// Runners by the entry `type` they answer.
pub type Runners = Registry<dyn HookRunner>;
/// Listeners for open events, by name.
pub type Listeners = Registry<dyn HookListener>;

impl Runners {
    /// `command` entries answered by [`ShellRunner`], and nothing else yet.
    pub fn with_shell() -> Self {
        let runners = Self::default();
        runners.install("command", Arc::new(ShellRunner));
        runners
    }
}

/// Adapter: each entry to the runner installed for its `type`, looked up at
/// each call: the addon host starts after the hook registry is built, and a
/// reload may replace its runner. An entry no runner answers fails open (no
/// verdict, action allowed).
pub struct DispatchRunner {
    runners: Arc<Runners>,
}

impl DispatchRunner {
    pub fn new(runners: Arc<Runners>) -> Self {
        Self { runners }
    }

    /// The process-wide runners.
    pub fn live() -> Self {
        Self::new(RUNNERS.clone())
    }
}

impl HookRunner for DispatchRunner {
    fn run(
        &self,
        cmd: &HookCommand,
        payload: &str,
        project_dir: &Path,
    ) -> Result<Exited, HookError> {
        match self.runners.get(&cmd.kind) {
            Some(runner) => runner.run(cmd, payload, project_dir),
            None => Err(HookError::NoRunner(cmd.kind.clone())),
        }
    }
}

static RUNNERS: LazyLock<Arc<Runners>> = LazyLock::new(|| Arc::new(Runners::with_shell()));
static LISTENERS: LazyLock<Arc<Listeners>> = LazyLock::new(Default::default);

/// Make `runner` answer `type: <kind>` entries in this process, replacing
/// the one installed before.
#[cfg_attr(not(feature = "addons"), allow(dead_code))]
pub fn install_runner(kind: &str, runner: Arc<dyn HookRunner>) {
    RUNNERS.install(kind, runner);
}

/// Make `listener` hear open events in this process under `name`,
/// replacing the one installed before.
#[cfg_attr(not(feature = "addons"), allow(dead_code))]
pub fn install_listener(name: &str, listener: Arc<dyn HookListener>) {
    LISTENERS.install(name, listener);
}

/// The process-wide listeners.
pub fn listeners() -> Arc<Listeners> {
    LISTENERS.clone()
}

fn drain<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// A settings file's text. An absent file is `Ok(None)`.
pub fn read_settings(path: &Path) -> Result<Option<String>, HookError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(HookError::Unreadable {
            path: path.display().to_string(),
            detail: e.to_string(),
        }),
    }
}
