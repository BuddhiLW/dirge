//! The live [`ConfigSource`]: the `addons.settings` of the config dirge
//! loaded, as `dirge.harness/config` and `:dirge/host :config` read it.
//!
//! The composition root publishes the settings at startup and on every
//! `/addons reload`, so a reload picks up edited settings.

use std::sync::{LazyLock, RwLock};

use serde_json::{Map, Value};

use super::port::ConfigSource;

static SETTINGS: LazyLock<RwLock<Map<String, Value>>> = LazyLock::new(Default::default);

/// Make `settings.settings` what [`LiveConfig`] answers from now on.
pub fn publish(settings: &crate::config::AddonsConfig) {
    let mut slot = SETTINGS.write().unwrap_or_else(|e| e.into_inner());
    *slot = settings.settings.clone();
}

/// The settings last [`publish`]ed.
pub struct LiveConfig;

impl ConfigSource for LiveConfig {
    fn settings(&self) -> Map<String, Value> {
        SETTINGS.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AddonsConfig;
    use serde_json::json;

    #[test]
    fn a_config_without_the_key_reads_no_settings() {
        let addons: AddonsConfig =
            serde_json::from_value(json!({"enabled": true, "paths": ["~/a"]})).unwrap();
        assert!(addons.settings.is_empty());
        assert_eq!(addons.paths, vec!["~/a"]);
        assert!(AddonsConfig::default().settings.is_empty());
    }

    #[test]
    fn settings_are_kept_per_addon_as_written() {
        let written = json!({"echo": {"greeting": "hi", "n": [1, 2]}, "other": {}});
        let addons: AddonsConfig =
            serde_json::from_value(json!({"settings": written.clone()})).unwrap();
        assert_eq!(serde_json::Value::Object(addons.settings), written);
    }

    #[test]
    fn an_existing_config_json_still_loads() {
        let cfg: crate::config::Config = serde_json::from_value(json!({
            "addons": {"enabled": true, "manifest_dirs": ["x"], "turn_timeout_secs": 5}
        }))
        .unwrap();
        let addons = cfg.addons.expect("addons key");
        assert!(addons.settings.is_empty());
        assert_eq!(addons.turn_timeout_secs, Some(5));
    }
}
