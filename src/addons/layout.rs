//! Addon filesystem layout (the hive layout):
//!
//! ```text
//! <repo>/deps.edn                                  :local/root deps
//! <repo>/src/...                                   the .cljc sources
//! <repo>/resources/META-INF/hive-addons/<id>.edn   the manifest
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Directory holding manifests, relative to a resources root.
pub const MANIFEST_DIR: [&str; 2] = ["META-INF", "hive-addons"];

/// True when `path` is `.../META-INF/hive-addons/<name>.edn`.
pub fn is_manifest(path: &Path) -> bool {
    let is_edn = path.extension().is_some_and(|e| e == "edn");
    let mut dirs = path.parent().into_iter().flat_map(Path::iter).rev();
    let parent = dirs.next();
    let grandparent = dirs.next();
    is_edn
        && parent.is_some_and(|p| p == MANIFEST_DIR[1])
        && grandparent.is_some_and(|g| g == MANIFEST_DIR[0])
}

/// The resources root a manifest sits in (the parent of `META-INF`).
pub fn resources_root(manifest: &Path) -> Option<&Path> {
    manifest.parent()?.parent()?.parent()
}

/// The addon repository: the parent of `resources/`, or the resources root
/// itself when the manifest is not under a directory named `resources`.
pub fn repo_root(manifest: &Path) -> Option<&Path> {
    let resources = resources_root(manifest)?;
    if resources.file_name().is_some_and(|n| n == "resources") {
        resources.parent()
    } else {
        Some(resources)
    }
}

/// Source roots a manifest's own repository contributes, before existence
/// is checked: `<repo>/src` and the resources root.
pub fn own_roots(manifest: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(repo) = repo_root(manifest) {
        roots.push(repo.join("src"));
    }
    if let Some(resources) = resources_root(manifest) {
        roots.push(resources.to_path_buf());
    }
    roots
}

/// Every `:local/root "<dir>"` in a deps.edn text, in order of appearance.
pub fn local_roots(deps_edn: &str) -> Vec<String> {
    const KEY: &str = ":local/root";
    let mut out = Vec::new();
    let mut rest = deps_edn;
    while let Some(at) = rest.find(KEY) {
        rest = &rest[at + KEY.len()..];
        let trimmed = rest.trim_start();
        if let Some(body) = trimmed.strip_prefix('"')
            && let Some(end) = body.find('"')
        {
            out.push(body[..end].to_string());
        }
    }
    out
}

/// A `:local/root` dependency's source root, resolved against the repo that
/// declared it.
pub fn dependency_root(repo: &Path, local_root: &str) -> PathBuf {
    let dep = Path::new(local_root);
    let dep = if dep.is_absolute() {
        dep.to_path_buf()
    } else {
        repo.join(dep)
    };
    dep.join("src")
}

/// Relative paths a namespace can load from in cljrs: `a.b-c.d` gives
/// `a/b_c/d.cljc` and `a/b_c/d.cljrs`.
pub fn portable_sources(ns: &str) -> [PathBuf; 2] {
    let stem = ns.replace('.', "/").replace('-', "_");
    [
        PathBuf::from(format!("{stem}.cljc")),
        PathBuf::from(format!("{stem}.cljrs")),
    ]
}

/// Sorted, de-duplicated source roots.
pub fn merge_roots<I: IntoIterator<Item = PathBuf>>(roots: I) -> Vec<PathBuf> {
    roots
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIVE_STYLE: &str = "/w/hive-dirge/resources/META-INF/hive-addons/hive-dirge.edn";

    #[test]
    fn recognizes_only_manifests_under_meta_inf() {
        assert!(is_manifest(Path::new(HIVE_STYLE)));
        assert!(!is_manifest(Path::new("/w/x/resources/hive-addons/a.edn")));
        assert!(!is_manifest(Path::new("/w/x/META-INF/hive-addons/a.clj")));
        assert!(!is_manifest(Path::new("a.edn")));
    }

    #[test]
    fn hive_layout_yields_src_and_resources() {
        let m = Path::new(HIVE_STYLE);
        assert_eq!(repo_root(m), Some(Path::new("/w/hive-dirge")));
        assert_eq!(
            own_roots(m),
            vec![
                PathBuf::from("/w/hive-dirge/src"),
                PathBuf::from("/w/hive-dirge/resources")
            ]
        );
    }

    #[test]
    fn flat_layout_treats_the_resources_root_as_the_repo() {
        let m = Path::new("/w/probe/META-INF/hive-addons/probe.edn");
        assert_eq!(repo_root(m), Some(Path::new("/w/probe")));
    }

    #[test]
    fn reads_local_roots_and_ignores_other_coordinates() {
        let deps = r#"{:deps {io.github.hive-agi/hive-addon {:local/root "../hive-addon"}
                          metosin/malli {:mvn/version "0.20.1"}
                          x/y {:local/root   "/abs/y"}}}"#;
        assert_eq!(local_roots(deps), vec!["../hive-addon", "/abs/y"]);
        assert!(local_roots("{:deps {}}").is_empty());
        assert!(local_roots(":local/root").is_empty());
    }

    #[test]
    fn portable_sources_munge_the_namespace() {
        assert_eq!(
            portable_sources("hive-dirge.probe.addon"),
            [
                PathBuf::from("hive_dirge/probe/addon.cljc"),
                PathBuf::from("hive_dirge/probe/addon.cljrs")
            ]
        );
    }

    #[test]
    fn dependency_roots_resolve_relative_to_the_declaring_repo() {
        let repo = Path::new("/w/hive-dirge");
        assert_eq!(
            dependency_root(repo, "../hive-addon"),
            PathBuf::from("/w/hive-dirge/../hive-addon/src")
        );
        assert_eq!(dependency_root(repo, "/abs/y"), PathBuf::from("/abs/y/src"));
    }

    /// The classpath is a function of the SET of roots, never of the order
    /// they were found in.
    #[test]
    fn merge_is_order_independent_and_idempotent() {
        let a = vec![
            PathBuf::from("/b"),
            PathBuf::from("/a"),
            PathBuf::from("/b"),
        ];
        let mut reversed = a.clone();
        reversed.reverse();
        assert_eq!(merge_roots(a.clone()), merge_roots(reversed));
        assert_eq!(merge_roots(merge_roots(a.clone())), merge_roots(a.clone()));
        assert_eq!(
            merge_roots(a),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }
}
