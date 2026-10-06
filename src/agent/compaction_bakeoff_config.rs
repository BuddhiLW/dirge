//! Provider resolution for the compaction bake-off (DIRGE_BAKEOFF_CONFIG).
//!
//! The bake-off originally built its provider from a base_url plus an API-key
//! env var, which shuts out subscription OAuth routes (ChatGPT/Codex,
//! `auth: chatgpt`): they have no base_url and no key. Reading the `providers`
//! map from a dirge config file lets DIRGE_BAKEOFF_PROVIDER name any provider
//! the loop itself can run on, so I1 and I2 can share one base config.

use std::collections::HashMap;

use crate::config::{ProviderAuth, ProviderEntry};

/// Parse the `providers` map and top-level `auth` out of a dirge config.json.
/// Only these two keys are read; the rest is ignored, so a loop-ab base config
/// can be reused verbatim.
pub(crate) fn from_json(
    text: &str,
) -> anyhow::Result<(HashMap<String, ProviderEntry>, Option<ProviderAuth>)> {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Partial {
        providers: HashMap<String, ProviderEntry>,
        auth: Option<ProviderAuth>,
    }
    let p: Partial = serde_json::from_str(text)?;
    Ok((p.providers, p.auth))
}

/// Providers + auth from DIRGE_BAKEOFF_CONFIG; empty when unset. `None`
/// (bake-off skipped, reason printed) when the file is unreadable or invalid.
pub(crate) fn from_env() -> Option<(HashMap<String, ProviderEntry>, Option<ProviderAuth>)> {
    let Ok(path) = std::env::var("DIRGE_BAKEOFF_CONFIG") else {
        return Some((HashMap::new(), None));
    };
    match std::fs::read_to_string(&path)
        .map_err(anyhow::Error::from)
        .and_then(|t| from_json(&t))
    {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[bakeoff] DIRGE_BAKEOFF_CONFIG {path}: {e}");
            None
        }
    }
}

/// DIRGE_BAKEOFF_MODEL wins; otherwise the named provider's configured
/// `model`. `None` (bake-off skipped) when neither is set.
pub(crate) fn model_name(
    env_model: Option<String>,
    provider: &str,
    providers: &HashMap<String, ProviderEntry>,
) -> Option<String> {
    env_model
        .filter(|m| !m.trim().is_empty())
        .or_else(|| providers.get(provider).and_then(|e| e.model.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OAUTH_BASE: &str = r#"{
      "provider": "chatgpt",
      "providers": {
        "chatgpt": { "provider_type": "openai", "auth": "chatgpt", "model": "gpt-6-sol" }
      }
    }"#;

    /// An OAuth (subscription) provider is reachable by name from a config
    /// file, with no base_url and no API key, and its model is the default.
    #[test]
    fn oauth_provider_resolves_from_a_config_file() {
        let (providers, auth) = from_json(OAUTH_BASE).unwrap();
        let entry = providers.get("chatgpt").expect("provider parsed");
        assert_eq!(entry.auth, Some(ProviderAuth::ChatGpt));
        assert!(entry.base_url.is_none() && entry.api_key.is_none() && entry.api_key_env.is_none());
        assert_eq!(auth, None);
        assert_eq!(
            model_name(None, "chatgpt", &providers).as_deref(),
            Some("gpt-6-sol")
        );
    }

    /// The parsed map builds a real ChatGPT-auth client through the same
    /// factory the bake-off calls, given a Codex token in the environment.
    #[test]
    fn oauth_provider_builds_a_codex_client() {
        let (providers, auth) = from_json(OAUTH_BASE).unwrap();
        let _guard = crate::auth::DIRGE_DATA_DIR_ENV_LOCK.lock().unwrap();
        // The env-token branch needs an account id too; set both so the
        // test never reads the real ~/.codex/auth.json.
        let vars = [
            ("CODEX_ACCESS_TOKEN", "FAKE-CODEX-TOKEN"),
            ("CHATGPT_ACCOUNT_ID", "acct-test"),
        ];
        let prev: Vec<_> = vars.iter().map(|(k, _)| std::env::var_os(k)).collect();
        for (k, v) in vars {
            // SAFETY: env mutation serialized by DIRGE_DATA_DIR_ENV_LOCK, restored below.
            unsafe { std::env::set_var(k, v) };
        }
        let client = crate::provider::create_client_with_auth("chatgpt", None, &providers, auth);
        for ((k, _), p) in vars.iter().zip(prev) {
            // SAFETY: as above.
            unsafe {
                match p {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        assert!(client.expect("chatgpt client builds").is_codex());
    }

    #[test]
    fn env_model_overrides_the_configured_one() {
        let (providers, _) = from_json(OAUTH_BASE).unwrap();
        assert_eq!(
            model_name(Some("gpt-6-luna".into()), "chatgpt", &providers).as_deref(),
            Some("gpt-6-luna")
        );
        assert_eq!(
            model_name(Some("  ".into()), "chatgpt", &providers).as_deref(),
            Some("gpt-6-sol")
        );
    }

    #[test]
    fn no_model_anywhere_skips() {
        assert_eq!(model_name(None, "venice", &HashMap::new()), None);
    }

    #[test]
    fn top_level_auth_is_carried() {
        let (_, auth) = from_json(r#"{"auth":"codex","providers":{}}"#).unwrap();
        assert_eq!(auth, Some(ProviderAuth::ChatGpt));
    }
}
