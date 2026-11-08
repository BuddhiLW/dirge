//! What each addon is handed as `:dirge/host` when it initializes.
//!
//! Pure: the composition root builds one host config for the whole host
//! (harness, version, cwd and every addon's `addons.settings`), and this
//! module narrows it to the one addon being loaded, so an addon never sees
//! another addon's settings.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::port::addon_settings;

/// Key of the host-wide config holding every addon's `addons.settings`,
/// keyed by addon id. It never reaches an addon: each sees only its own
/// entry, under [`CONFIG_KEY`].
pub const SETTINGS_KEY: &str = "settings";

/// Key under which an addon finds its own settings in `:dirge/host`.
pub const CONFIG_KEY: &str = "config";

/// The host-wide config: harness name, `version`, `cwd` and `settings`
/// (every addon's, keyed by id).
pub fn host_wide(version: &str, cwd: &Path, settings: Map<String, Value>) -> Value {
    json!({
        "harness": "dirge",
        "version": version,
        "cwd": cwd.display().to_string(),
        SETTINGS_KEY: settings,
    })
}

/// What addon `addon_id` is handed as `:dirge/host`: `host_config` without
/// the settings of every addon, with its own under `config` (`{}` when it
/// has none).
pub fn for_addon(host_config: &Value, addon_id: &str) -> Value {
    let mut out = host_config.as_object().cloned().unwrap_or_default();
    let settings = match out.remove(SETTINGS_KEY) {
        Some(Value::Object(settings)) => settings,
        _ => Map::new(),
    };
    out.insert(CONFIG_KEY.into(), addon_settings(&settings, addon_id));
    Value::Object(out)
}

/// [`for_addon`] for the addon the manifest at `manifest` declares. A
/// manifest that does not parse gets no settings; its load reports why.
pub fn for_manifest(host_config: &Value, manifest: &Path) -> Value {
    let id = super::manifest::AddonManifest::from_path(manifest)
        .map(|m| m.id)
        .unwrap_or_default();
    for_addon(host_config, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Map<String, Value> {
        let mut settings = Map::new();
        settings.insert("echo".into(), json!({"greeting": "hi"}));
        settings.insert("other".into(), json!({"secret": 1}));
        settings
    }

    #[test]
    fn an_addon_sees_only_its_own_settings_under_config() {
        let wide = host_wide("1.2.3", Path::new("/w"), settings());
        assert_eq!(
            for_addon(&wide, "echo"),
            json!({"harness": "dirge", "version": "1.2.3", "cwd": "/w",
                   "config": {"greeting": "hi"}})
        );
    }

    #[test]
    fn an_addon_without_settings_gets_an_empty_config() {
        let wide = host_wide("1.2.3", Path::new("/w"), settings());
        assert_eq!(for_addon(&wide, "absent")["config"], json!({}));
        assert_eq!(for_addon(&json!({}), "echo"), json!({"config": {}}));
    }
}
