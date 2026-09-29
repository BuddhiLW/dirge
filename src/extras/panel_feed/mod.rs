//! Panel feed: a generic Server-Sent Events subscription that drives
//! the external panels in the left side panel and posts one-line
//! notifications, plus a small reply channel back to the producer.
//!
//! Off by default. Enabled by the `panel_feed` config block (see
//! [`discovery::PanelFeedConfig`]); the wire format is documented in
//! `docs/panel-feed.md`.
//!
//! Layout:
//! - [`sse`] — pure incremental event-stream parser;
//! - [`ops`] — pure op decoding into [`ops::FeedEffect`] plus the
//!   [`ops::FeedSink`] port (production: [`ops::UiSink`]);
//! - [`discovery`] — config -> endpoint, private-file checks;
//! - [`client`] — the HTTP boundary (subscription loop, reply POST).

pub mod client;
pub mod discovery;
pub mod ops;
pub mod sse;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::watch;

use crate::sync_util::LockExt;
use crate::ui::notifications::Notification;
use client::{FeedOptions, ReplyError};
use discovery::{PanelFeedConfig, Source};

/// The source of the running feed, so replies re-resolve the
/// endpoint (a restarted producer has a new port and token).
static ACTIVE: Mutex<Option<Source>> = Mutex::new(None);

/// One reply the user can send back to the producer. The verbs are the
/// producer's: dirge carries them as opaque names and knows only which
/// ones need a target ([`DEFAULT_VERBS`] until the producer advertises
/// its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyAction {
    /// A producer reply verb, with the item it names when it takes one.
    Verb {
        action: String,
        target: Option<String>,
    },
    /// A producer-defined verb on a panel and optional row.
    Invoke {
        panel: String,
        verb: String,
        row: Option<String>,
        payload: serde_json::Value,
    },
}

/// A reply verb the producer accepts, and whether it names an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyVerb {
    pub name: &'static str,
    pub takes_target: bool,
}

/// The reply verbs assumed when the producer advertises none: the ones
/// every panel-feed producer so far has accepted.
pub const DEFAULT_VERBS: &[ReplyVerb] = &[
    ReplyVerb {
        name: "focus",
        takes_target: true,
    },
    ReplyVerb {
        name: "unfocus",
        takes_target: false,
    },
    ReplyVerb {
        name: "next-tab",
        takes_target: false,
    },
    ReplyVerb {
        name: "prev-tab",
        takes_target: false,
    },
    ReplyVerb {
        name: "refresh",
        takes_target: false,
    },
];

/// Short names `/panel` accepts for a verb.
const VERB_ALIASES: &[(&str, &str)] = &[("next", "next-tab"), ("prev", "prev-tab")];

/// Usage line for the reply verbs of `/panel`.
pub const REPLY_USAGE: &str = "usage: /panel next|prev|refresh|unfocus|focus <id>";

/// The verb named `name` (after aliases) among `verbs`.
pub fn find_verb<'a>(verbs: &'a [ReplyVerb], name: &str) -> Option<&'a ReplyVerb> {
    let name = VERB_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, verb)| verb);
    verbs.iter().find(|v| v.name == name)
}

impl ReplyAction {
    /// The reply `verb` with no target.
    pub fn verb(action: &str) -> Self {
        Self::Verb {
            action: action.to_string(),
            target: None,
        }
    }

    /// The reply `verb` naming the item `target`.
    pub fn verb_on(action: &str, target: &str) -> Self {
        Self::Verb {
            action: action.to_string(),
            target: Some(target.to_string()),
        }
    }

    /// `name` as a reply when `verbs` has it and `target` fits it: a
    /// verb that takes a target needs one, and one that does not takes
    /// none.
    pub fn checked(verbs: &[ReplyVerb], name: &str, target: Option<&str>) -> Option<Self> {
        let verb = find_verb(verbs, name)?;
        match (verb.takes_target, target) {
            (true, Some(t)) if !t.trim().is_empty() => Some(Self::verb_on(verb.name, t.trim())),
            (false, None) => Some(Self::verb(verb.name)),
            _ => None,
        }
    }

    /// Parse the words after `/panel` into a reply (pure). `Err`
    /// carries a user-facing usage message.
    pub fn parse(args: &[&str]) -> Result<Self, String> {
        let name = args.first().map(|s| s.trim()).unwrap_or("");
        let rest = &args[args.len().min(1)..];
        if name.is_empty() {
            return Err(REPLY_USAGE.to_string());
        }
        let Some(verb) = find_verb(DEFAULT_VERBS, name) else {
            return Err(format!("unknown /panel action '{name}' ({REPLY_USAGE})"));
        };
        match (verb.takes_target, rest) {
            (true, [id]) if !id.trim().is_empty() => Ok(Self::verb_on(verb.name, id.trim())),
            (true, []) => Err(format!("/panel {name} needs an item id ({REPLY_USAGE})")),
            (true, _) => Err(format!("/panel {name} takes one id ({REPLY_USAGE})")),
            (false, []) => Ok(Self::verb(verb.name)),
            (false, _) => Err(format!("/panel {name} takes no argument ({REPLY_USAGE})")),
        }
    }

    /// The item a verb reply names, if any.
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Verb { target, .. } => target.as_deref(),
            Self::Invoke { .. } => None,
        }
    }

    /// The wire name of the action.
    pub fn name(&self) -> &str {
        match self {
            Self::Verb { action, .. } => action,
            Self::Invoke { .. } => "invoke",
        }
    }

    /// The JSON body POSTed to `<url>/reply` (pure).
    pub fn to_json(&self) -> String {
        let value = match self {
            Self::Verb {
                action,
                target: Some(target),
            } => json!({"action": action, "target": target}),
            Self::Verb {
                action,
                target: None,
            } => json!({"action": action}),
            Self::Invoke {
                panel,
                verb,
                row,
                payload,
            } => {
                json!({"action": "invoke", "panel": panel, "verb": verb, "row": row, "payload": payload})
            }
        };
        value.to_string()
    }
}

/// Send `action` to the running feed's producer.
pub async fn reply(action: ReplyAction) -> Result<(), ReplyError> {
    let source = ACTIVE
        .lock_ignore_poison()
        .clone()
        .ok_or(ReplyError::NotRunning)?;
    reply_to(&source, &action).await
}

/// Where replies are sent. Production: [`LiveFeed`]; tests record.
pub trait ReplyTransport: Send + Sync {
    fn send(&self, action: &ReplyAction) -> impl Future<Output = Result<(), ReplyError>> + Send;
}

/// The running feed's producer (re-resolved on every reply).
pub struct LiveFeed;

impl ReplyTransport for LiveFeed {
    async fn send(&self, action: &ReplyAction) -> Result<(), ReplyError> {
        reply(action.clone()).await
    }
}

/// The notification a failed reply surfaces (pure). No running feed
/// is a warning; a refused or broken request is an error.
pub fn failure_notice(action: &ReplyAction, err: &ReplyError) -> Notification {
    let message = crate::ui::ansi::strip_escapes(
        &format!("panel reply '{}' failed: {err}", action.name()),
        crate::ui::ansi::StripPolicy::STRICT,
    );
    match err {
        ReplyError::NotRunning => Notification::Warn(message),
        _ => Notification::Error(message),
    }
}

/// Send `action` through `transport`; a failure comes back as the
/// notification to show.
pub async fn send_reply<T: ReplyTransport>(
    transport: &T,
    action: ReplyAction,
) -> Option<Notification> {
    match transport.send(&action).await {
        Ok(()) => None,
        Err(err) => Some(failure_notice(&action, &err)),
    }
}

/// Fire `action` at the running feed without blocking the caller; a
/// failure is posted on the notification channel. Must be called
/// inside a tokio runtime.
pub fn spawn_reply(action: ReplyAction) {
    tokio::spawn(async move {
        if let Some(notice) = send_reply(&LiveFeed, action).await {
            crate::ui::notifications::notify_send(notice);
        }
    });
}

/// Send `action` to the producer behind `source`.
pub async fn reply_to(source: &Source, action: &ReplyAction) -> Result<(), ReplyError> {
    let ep = discovery::resolve(source)?;
    client::post_reply(&ep, action.to_json()).await
}

/// A running feed. Dropping it stops the subscription loop and
/// forgets the reply target.
pub struct FeedHandle {
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl FeedHandle {
    /// Stop the loop and wait for it to finish.
    #[allow(dead_code)] // production relies on Drop; tests await it
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for FeedHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        *ACTIVE.lock_ignore_poison() = None;
    }
}

/// The directory a relative `discovery_dir` resolves under:
/// `$XDG_RUNTIME_DIR`, else the system temp dir.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Start the feed described by `cfg`, or `None` when it is disabled
/// or has no source. Must be called inside a tokio runtime.
pub fn start(cfg: Option<&PanelFeedConfig>) -> Option<FeedHandle> {
    let source = cfg?.source(Some(&runtime_dir()))?;
    Some(spawn(source, Arc::new(ops::UiSink), FeedOptions::default()))
}

/// Spawn the subscription loop for `source` into `sink`.
pub fn spawn(source: Source, sink: Arc<dyn ops::FeedSink>, opts: FeedOptions) -> FeedHandle {
    *ACTIVE.lock_ignore_poison() = Some(source.clone());
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(client::run(source, sink, opts, rx));
    FeedHandle {
        stop,
        task: Some(task),
    }
}

#[cfg(test)]
mod tests;
