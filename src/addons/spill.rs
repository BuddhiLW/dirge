//! Spill for oversized hook output. Hook text that would ride into the
//! model's context past a cap is cut to a head+tail preview, and the full
//! text is kept behind an ARC handle (`§` + 8 hex digits of its SHA-256,
//! the handle shape `context_retrieve` takes) so it can be recalled.
//!
//! Strata: [`plan`] and [`handle_for`] are pure; [`SpillStore`] is the port
//! the full text goes through; [`FileSpillStore`] is the adapter that keeps
//! it under `.dirge/hook_outputs/<session>/<hex>.txt`; [`spill`] composes
//! them at the boundary.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// Characters of hook text that ride into the context inline. Past this
/// the text spills (Claude Code caps `additionalContext` at 10k chars).
pub const CAP: usize = 10_000;
/// Characters of a spilled text's head kept in its preview.
pub const PREVIEW_HEAD: usize = 1_500;
/// Characters of a spilled text's tail kept in its preview.
pub const PREVIEW_TAIL: usize = 500;

/// What to do with one hook text: keep it, or spill it behind `handle`
/// and show `preview` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spill {
    Inline(String),
    Spilled { preview: String, handle: String },
}

/// The ARC handle of `text`: `§` and the first 8 hex digits of its SHA-256.
/// Content-addressed, so the same output always answers the same handle.
pub fn handle_for(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("§{hex}")
}

/// The size policy: `text` up to `cap` characters stays inline; a longer
/// one becomes a head+tail preview naming its handle.
pub fn plan(text: &str, cap: usize) -> Spill {
    let chars = text.chars().count();
    if chars <= cap {
        return Spill::Inline(text.to_string());
    }
    let handle = handle_for(text);
    // A cap below the preview shrinks it, so head and tail never overlap.
    let (head_len, tail_len) = (PREVIEW_HEAD.min(cap * 3 / 4), PREVIEW_TAIL.min(cap / 4));
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(chars - tail_len).collect();
    let cut = chars - head_len - tail_len;
    let preview = format!(
        "{head}\n[... {cut} chars spilled to {handle}; recall with context_retrieve {handle} ...]\n{tail}"
    );
    Spill::Spilled { preview, handle }
}

/// Where spilled hook output is kept.
pub trait SpillStore: Send + Sync + 'static {
    /// Keep `text` under `handle` for `session`. `Err` says why it was not.
    fn put(&self, session: Option<&str>, handle: &str, text: &str) -> Result<(), String>;
}

/// `text` as it may enter the context: inline when small, else its preview
/// once `store` has kept the full text. When the store fails the preview
/// still goes out (the cap holds) with the failure logged.
pub fn spill(text: &str, cap: usize, session: Option<&str>, store: &dyn SpillStore) -> String {
    match plan(text, cap) {
        Spill::Inline(text) => text,
        Spill::Spilled { preview, handle } => {
            if let Err(error) = store.put(session, &handle, text) {
                tracing::warn!(target: "dirge::addon", %handle, %error, "hook output spill not kept");
            }
            preview
        }
    }
}

/// [`SpillStore`] over files: `<root>/<session>/<hex>.txt`, `root` being
/// `.dirge/hook_outputs` of the project.
pub struct FileSpillStore {
    pub root: PathBuf,
}

impl FileSpillStore {
    /// The store under the project `.dirge/` of `cwd`.
    pub fn for_cwd(cwd: &std::path::Path) -> Self {
        let paths = crate::extras::dirge_paths::ProjectPaths::new(cwd);
        Self {
            root: paths.dirge_dir().join("hook_outputs"),
        }
    }

    /// Where `handle` of `session` is kept.
    pub fn path(&self, session: Option<&str>, handle: &str) -> PathBuf {
        let session = session
            .filter(|s| !s.is_empty() && !s.contains(['/', '\\', '.']))
            .unwrap_or("no-session");
        self.root
            .join(session)
            .join(format!("{}.txt", handle.trim_start_matches('§')))
    }
}

impl SpillStore for FileSpillStore {
    fn put(&self, session: Option<&str>, handle: &str, text: &str) -> Result<(), String> {
        let path = self.path(session, handle);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every put; fails them all when `fail` is set.
    #[derive(Default)]
    pub(crate) struct MemorySpillStore {
        pub kept: Mutex<Vec<(Option<String>, String, String)>>,
        pub fail: bool,
    }

    impl SpillStore for MemorySpillStore {
        fn put(&self, session: Option<&str>, handle: &str, text: &str) -> Result<(), String> {
            if self.fail {
                return Err("disk full".into());
            }
            self.kept.lock().unwrap().push((
                session.map(str::to_string),
                handle.to_string(),
                text.to_string(),
            ));
            Ok(())
        }
    }

    #[test]
    fn the_handle_is_section_sign_and_eight_hex_digits_of_the_content() {
        let h = handle_for("abc");
        assert_eq!(h, "§ba7816bf");
        assert_eq!(h, handle_for("abc"), "content-addressed");
        assert_ne!(h, handle_for("abd"));
    }

    #[test]
    fn text_at_the_cap_stays_inline() {
        let text = "x".repeat(CAP);
        assert_eq!(plan(&text, CAP), Spill::Inline(text.clone()));
    }

    #[test]
    fn text_past_the_cap_becomes_a_head_tail_preview_naming_its_handle() {
        let text = format!("{}{}{}", "h".repeat(PREVIEW_HEAD), "m".repeat(CAP), "t".repeat(PREVIEW_TAIL));
        let Spill::Spilled { preview, handle } = plan(&text, CAP) else {
            panic!("spilled");
        };
        assert_eq!(handle, handle_for(&text));
        assert!(preview.starts_with(&"h".repeat(PREVIEW_HEAD)));
        assert!(preview.ends_with(&"t".repeat(PREVIEW_TAIL)));
        assert!(!preview.contains('m'), "the middle is cut");
        assert!(preview.contains(&format!("{CAP} chars spilled to {handle}")));
        assert!(preview.chars().count() < CAP);
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let text = "é".repeat(20);
        assert_eq!(plan(&text, 20), Spill::Inline(text.clone()));
        assert!(matches!(plan(&text, 19), Spill::Spilled { .. }));
    }

    #[test]
    fn a_cap_below_the_preview_shrinks_the_preview_to_fit() {
        let text = format!("{}{}", "a".repeat(30), "b".repeat(10));
        let Spill::Spilled { preview, .. } = plan(&text, 20) else {
            panic!("spilled");
        };
        assert!(preview.starts_with(&"a".repeat(15)));
        assert!(preview.ends_with(&"b".repeat(5)));
        assert!(preview.contains("20 chars spilled"));
    }

    #[test]
    fn spill_keeps_the_full_text_in_the_store_and_answers_the_preview() {
        let store = MemorySpillStore::default();
        let text = "y".repeat(30);
        assert_eq!(spill("small", 30, Some("s1"), &store), "small");
        assert!(store.kept.lock().unwrap().is_empty());

        let out = spill(&format!("{text}z"), 30, Some("s1"), &store);
        let kept = store.kept.lock().unwrap().clone();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0.as_deref(), Some("s1"));
        assert_eq!(kept[0].2, format!("{text}z"));
        assert!(out.contains(&kept[0].1));
    }

    #[test]
    fn a_failing_store_still_caps_the_text() {
        let store = MemorySpillStore {
            fail: true,
            ..Default::default()
        };
        let text = "q".repeat(CAP * 2);
        let out = spill(&text, CAP, None, &store);
        assert!(out.chars().count() < CAP);
    }

    #[test]
    fn the_file_store_keeps_the_text_under_session_and_handle() {
        let dir = std::env::temp_dir().join(format!("dirge-spill-{}", uuid::Uuid::new_v4()));
        let store = FileSpillStore { root: dir.clone() };
        store.put(Some("s1"), "§deadbeef", "full").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("s1/deadbeef.txt")).unwrap(), "full");
        assert_eq!(
            store.path(Some("../etc"), "§00"),
            dir.join("no-session/00.txt"),
            "a session id cannot climb out of the root"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
