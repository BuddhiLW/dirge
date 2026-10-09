//! What addons bring to the view runtime (view seam stage 3). An addon
//! manifest may name a `:dirge/view-ns`; dirge requires it in the VIEW
//! runtime (`view_isolate`) after `dirge.view`, never in the addon
//! isolate, so an addon view stays off the agent loop. There the addon
//! calls `dirge.view/register-view-command!` / `register-grid-key!`; the
//! registry, its conflict rule (built-in wins, warn) and dispatch live in
//! `view.cljc`. Rust only finds the namespaces and passes them in.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::addons::manifest::AddonManifest;

/// The source roots addon view namespaces load from, and the
/// `:dirge/view-ns` of each manifest, in manifest order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewAddons {
    pub roots: Vec<PathBuf>,
    pub namespaces: Vec<String>,
}

impl ViewAddons {
    /// The `:dirge/view-ns` of every manifest in `manifests` that parses
    /// and declares one (each once), with `roots`.
    pub fn from_manifests(manifests: &[PathBuf], roots: &[PathBuf]) -> Self {
        let mut namespaces = Vec::new();
        for path in manifests {
            if let Ok(m) = AddonManifest::from_path(path)
                && let Some(ns) = m.view_ns
                && !namespaces.contains(&ns)
            {
                namespaces.push(ns);
            }
        }
        Self {
            roots: roots.to_vec(),
            namespaces,
        }
    }
}

/// Set once at startup by the addon host, before the view starts.
static ADDONS: OnceLock<ViewAddons> = OnceLock::new();

/// Record what addons bring to the view runtime. A second call is ignored.
pub fn set(addons: ViewAddons) {
    let _ = ADDONS.set(addons);
}

/// The recorded addons; none when the addon host never started.
pub fn current() -> ViewAddons {
    ADDONS.get().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::cljrs::isolate::ISOLATE_STACK_BYTES;
    use crate::addons::cljrs::view_isolate::CljrsReducer;
    use crate::ui::view::ViewEvent;
    use crate::ui::view::domain::{NoticeLevel, ViewEffect};
    use crate::ui::view::port::Reducer;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A fixture addon: manifest with `:dirge/view-ns` and that namespace,
    /// which registers `/hello`, a grid key, and tries to take `/swarm`.
    fn fixture() -> (TempDir, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("dirge-view-addon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let manifest = dir.join("hello/resources/META-INF/addons/hello.edn");
        write(
            &manifest,
            "{:addon/id hello :addon/init-ns hello.core :addon/init-fn ctor :dirge/view-ns hello.view}",
        );
        let src = dir.join("hello/src");
        write(
            &src.join("hello/view.cljc"),
            r#"(ns hello.view (:require [dirge.view :as view]))
(view/register-view-command! "hello"
  {:summary "say hello"
   :run (fn [state args] [state [(view/notify :info (str "hello " (count args)))]])})
(view/register-grid-key! "x" (fn [state g] [state [(view/notify :info "x")]]))
(view/register-view-command! "swarm" {:run (fn [s _] [s []])})
"#,
        );
        (TempDir(dir), manifest, src)
    }

    #[test]
    fn reads_view_namespaces_from_manifests() {
        let (_dir, manifest, src) = fixture();
        let addons = ViewAddons::from_manifests(&[manifest.clone(), manifest], &[src.clone()]);
        assert_eq!(addons.namespaces, vec!["hello.view".to_string()]);
        assert_eq!(addons.roots, vec![src]);
    }

    #[test]
    fn a_fixture_addons_view_ns_is_loaded_and_hello_is_a_view_command() {
        let (_dir, manifest, src) = fixture();
        let addons = ViewAddons::from_manifests(&[manifest], &[src]);
        std::thread::Builder::new()
            .stack_size(ISOLATE_STACK_BYTES)
            .spawn(move || {
                let mut r = CljrsReducer::boot_with(addons).unwrap();
                let u = r.step(&ViewEvent::Init).unwrap();
                assert!(u.model.owns_command("hello"), "{:?}", u.model);
                assert!(u.model.owns_command("swarm"));
                assert!(u.model.grid_keys.contains(&"x".to_string()));
                // The refused /swarm registration surfaces once, as a warning.
                assert!(u.effects.iter().any(|e| matches!(
                    e,
                    ViewEffect::Notify { level: NoticeLevel::Warn, text } if text.contains("swarm")
                )));
                let u = r.step(&ViewEvent::command("hello", &["a", "b"])).unwrap();
                assert_eq!(u.effects, vec![ViewEffect::notify(NoticeLevel::Info, "hello 2")]);
                // Built-in wins: /swarm still opens the grid.
                let u = r.step(&ViewEvent::command("swarm", &["on"])).unwrap();
                assert!(u.model.swarm_open());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn a_missing_view_ns_never_keeps_the_view_from_booting() {
        let addons = ViewAddons {
            roots: vec![],
            namespaces: vec!["no.such.view".into()],
        };
        std::thread::Builder::new()
            .stack_size(ISOLATE_STACK_BYTES)
            .spawn(move || {
                let mut r = CljrsReducer::boot_with(addons).unwrap();
                assert!(r.step(&ViewEvent::Init).unwrap().model.owns_command("swarm"));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
