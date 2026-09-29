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
use crate::ui::view::ViewEvent;
use crate::ui::view::domain::{ProducerKey, ProducerVerb};
use client::{FeedOptions, ReplyError};
use discovery::{PanelFeedConfig, Source};

/// The source of the running feed, so replies re-resolve the
/// endpoint (a restarted producer has a new port and token).
static ACTIVE: Mutex<Option<Source>> = Mutex::new(None);

/// One reply the user can send back to the producer. The verbs are the
/// producer's: dirge carries them as opaque names and knows only which
/// ones need a target ([`reply_verbs`]: the producer's, else
/// [`DEFAULT_VERBS`]).
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

/// Whether a reply verb names an item.
pub use crate::ui::view::domain::ReplyTarget as Target;

/// A reply verb the producer accepts, and whether it names an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyVerb {
    pub name: std::borrow::Cow<'static, str>,
    pub target: Target,
}

/// The reply verbs assumed when the producer advertises none: the ones
/// every panel-feed producer so far has accepted.
const DEFAULT_VERBS: &[(&str, Target)] = &[
    ("focus", Target::Required),
    ("unfocus", Target::None),
    ("next-tab", Target::None),
    ("prev-tab", Target::None),
    ("refresh", Target::None),
];

/// Short names `/panel` accepts for a verb.
const VERB_ALIASES: &[(&str, &str)] = &[("next", "next-tab"), ("prev", "prev-tab")];

/// Usage line for the reply verbs of `/panel` when the producer
/// advertises none.
pub const REPLY_USAGE: &str = "usage: /panel next|prev|refresh|unfocus|focus <id>";

/// What the running producer advertised in its discovery document
/// (`capabilities`); `None` until one does.
static ADVERTISED: Mutex<Option<discovery::Capabilities>> = Mutex::new(None);

/// The grid keys bound when the producer advertises none, by grid key
/// name: the bindings every panel-feed producer so far has expected.
const DEFAULT_KEYS: &[(&str, &str)] = &[
    ("Tab", "next-tab"),
    ("BackTab", "prev-tab"),
    ("r", "refresh"),
    ("u", "unfocus"),
    ("Enter", "focus"),
];

/// [`DEFAULT_VERBS`] as a verb list.
pub fn default_verbs() -> Vec<ReplyVerb> {
    DEFAULT_VERBS
        .iter()
        .map(|(name, target)| ReplyVerb {
            name: (*name).into(),
            target: *target,
        })
        .collect()
}

/// The verbs `names` advertises. A name that is a default verb keeps its
/// target rule; any other may carry a target. `invoke` is a reply of its
/// own ([`ReplyAction::Invoke`]), not a verb.
pub fn advertised_verbs(names: &[String]) -> Vec<ReplyVerb> {
    names
        .iter()
        .filter(|n| n.as_str() != "invoke" && !n.trim().is_empty())
        .map(|name| ReplyVerb {
            target: DEFAULT_VERBS
                .iter()
                .find(|(d, _)| d == name)
                .map_or(Target::Optional, |(_, t)| *t),
            name: name.clone().into(),
        })
        .collect()
}

/// The verbs `caps` allows: its replies, else the defaults.
fn verbs_of(caps: Option<&discovery::Capabilities>) -> Vec<ReplyVerb> {
    match caps.and_then(|c| c.replies.as_deref()) {
        Some(names) => advertised_verbs(names),
        None => default_verbs(),
    }
}

/// The verbs replies may use now: the producer's, else the defaults.
pub fn reply_verbs() -> Vec<ReplyVerb> {
    verbs_of(ADVERTISED.lock_ignore_poison().as_ref())
}

/// Record what the producer just advertised (`None`: nothing). When it
/// changed, the view hears it as a [`ViewEvent::Producer`].
pub fn set_advertised(caps: Option<discovery::Capabilities>) {
    let changed = {
        let mut current = ADVERTISED.lock_ignore_poison();
        let changed = *current != caps;
        *current = caps;
        changed
    };
    if changed {
        crate::ui::view::submit(producer_event_now());
    }
}

/// The grid key name a chord (keymap syntax: `enter`, `shift-tab`, `u`)
/// goes by; `None` for one the grid cannot receive (Ctrl or Alt held).
pub fn grid_key(chord: &str) -> Option<String> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (code, mods) = crate::ui::keymap::parse_chord(chord)?;
    if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
        return Some("BackTab".to_string());
    }
    crate::ui::view::promote::key_name(&KeyEvent::new(code, mods))
}

/// The grid keys `caps` binds, checked against `verbs`: its keys, else
/// [`DEFAULT_KEYS`] whose verb is in force. A binding to a verb not
/// advertised, or a chord the grid cannot receive, is dropped and logged.
fn keys_of(caps: Option<&discovery::Capabilities>, verbs: &[ReplyVerb]) -> Vec<ProducerKey> {
    use discovery::AdvertisedKey;
    let Some(advertised) = caps.and_then(|c| c.keys.as_ref()) else {
        return DEFAULT_KEYS
            .iter()
            .filter(|(_, verb)| find_verb(verbs, verb).is_some())
            .map(|(key, verb)| ProducerKey {
                key: key.to_string(),
                verb: verb.to_string(),
                invoke: false,
            })
            .collect();
    };
    let invokes = caps.map(|c| c.invokes.as_slice()).unwrap_or_default();
    let mut keys: Vec<ProducerKey> = advertised
        .iter()
        .filter_map(|(chord, binding)| {
            let key = grid_key(chord);
            let (verb, invoke, known) = match binding {
                AdvertisedKey::Reply(v) => (v, false, find_verb(verbs, v).is_some()),
                AdvertisedKey::Invoke(v) => (v, true, invokes.is_empty() || invokes.contains(v)),
            };
            if key.is_none() || !known {
                tracing::warn!(target: "dirge::panel_feed", chord, verb, "advertised key dropped");
                return None;
            }
            Some(ProducerKey {
                key: key?,
                verb: verb.clone(),
                invoke,
            })
        })
        .collect();
    keys.sort_by(|a, b| a.key.cmp(&b.key));
    keys
}

/// The [`ViewEvent::Producer`] for `caps` (pure).
pub fn producer_event(caps: Option<&discovery::Capabilities>) -> ViewEvent {
    let verbs = verbs_of(caps);
    ViewEvent::Producer {
        keys: keys_of(caps, &verbs),
        usage: usage(&verbs),
        replies: verbs
            .into_iter()
            .map(|v| ProducerVerb {
                name: v.name.into_owned(),
                target: v.target,
            })
            .collect(),
    }
}

/// [`producer_event`] for what the producer advertises now.
pub fn producer_event_now() -> ViewEvent {
    producer_event(ADVERTISED.lock_ignore_poison().as_ref())
}

/// The usage line for `verbs`.
pub fn usage(verbs: &[ReplyVerb]) -> String {
    if verbs == default_verbs().as_slice() {
        return REPLY_USAGE.to_string();
    }
    let names: Vec<String> = verbs
        .iter()
        .map(|v| match v.target {
            Target::None => v.name.to_string(),
            Target::Required => format!("{} <id>", v.name),
            Target::Optional => format!("{} [id]", v.name),
        })
        .collect();
    format!("usage: /panel {}", names.join("|"))
}

/// The verb named `name` (after aliases) among `verbs`.
pub fn find_verb<'a>(verbs: &'a [ReplyVerb], name: &str) -> Option<&'a ReplyVerb> {
    let name = VERB_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, verb)| verb);
    verbs.iter().find(|v| v.name == name)
}

/// The reply a global panel key sends for `name` when `verbs` accepts
/// it with no target (pure). `Err` is the warning to show instead.
pub fn global_reply_among(verbs: &[ReplyVerb], name: &str) -> Result<ReplyAction, Notification> {
    ReplyAction::checked(verbs, name, None).ok_or_else(|| {
        Notification::Warn(crate::ui::ansi::strip_escapes(
            &format!(
                "the panel producer does not accept '{name}' ({})",
                usage(verbs)
            ),
            crate::ui::ansi::StripPolicy::STRICT,
        ))
    })
}

/// [`global_reply_among`] the verbs the producer accepts now.
pub fn global_reply(name: &str) -> Result<ReplyAction, Notification> {
    global_reply_among(&reply_verbs(), name)
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
    /// verb that needs a target needs one, one that takes none takes
    /// none, and an optional one takes either.
    pub fn checked(verbs: &[ReplyVerb], name: &str, target: Option<&str>) -> Option<Self> {
        let verb = find_verb(verbs, name)?;
        let target = target.map(str::trim).filter(|t| !t.is_empty());
        match (verb.target, target) {
            (Target::Required | Target::Optional, Some(t)) => Some(Self::verb_on(&verb.name, t)),
            (Target::None | Target::Optional, None) => Some(Self::verb(&verb.name)),
            _ => None,
        }
    }

    /// Parse the words after `/panel` into a reply among `verbs` (pure).
    /// `Err` carries a user-facing usage message.
    pub fn parse_among(verbs: &[ReplyVerb], args: &[&str]) -> Result<Self, String> {
        let usage = usage(verbs);
        let name = args.first().map(|s| s.trim()).unwrap_or("");
        let rest = &args[args.len().min(1)..];
        if name.is_empty() {
            return Err(usage);
        }
        let Some(verb) = find_verb(verbs, name) else {
            return Err(format!("unknown /panel action '{name}' ({usage})"));
        };
        let id = |id: &str| Self::verb_on(&verb.name, id.trim());
        match (verb.target, rest) {
            (Target::Required | Target::Optional, [one]) if !one.trim().is_empty() => Ok(id(one)),
            (Target::Required, []) => Err(format!("/panel {name} needs an item id ({usage})")),
            (Target::None | Target::Optional, []) => Ok(Self::verb(&verb.name)),
            (Target::None, _) => Err(format!("/panel {name} takes no argument ({usage})")),
            _ => Err(format!("/panel {name} takes one id ({usage})")),
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
    set_advertised(ep.capabilities.clone());
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
        set_advertised(None);
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
