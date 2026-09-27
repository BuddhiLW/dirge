//! L2 boundary: the only code that touches processes and files. Every
//! function returns a `Result`; failures are values.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
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
