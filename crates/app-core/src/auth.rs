//! The BYO-key auth trio (`agent_status` / `set_api_key` / `clear_api_key`)
//! and its one view type — relocated from `src-tauri/src/lib.rs` by plan 47-01
//! (Phase 47, FFI-01; RESEARCH A1 rows 13, 16, 17).
//!
//! These three were the phase's easiest movers: their `*_inner` fns were
//! ALREADY tauri-free (each takes only `&dyn agent_llm::KeyStore`), just
//! misplaced in the shell. The bodies below are byte-copies; only the names
//! changed (`agent_status_inner` → [`run_agent_status`] etc.), following the
//! `run_*` convention every other `app-core` command home uses. The
//! `#[tauri::command]`s stay in `src-tauri` as one-line wrappers over the
//! managed [`ManagedKeyStore`]'s `.0.as_ref()`, exactly as before.
//!
//! # T-12-11 / T-47-08 / T-47-13 — only booleans and an enum ever cross
//!
//! Phase 69 (D-69-14, D-69-06) widened [`AgentStatusView`] ADDITIVELY to
//! `{"key_configured": bool, "source": "credential_manager"|"environment"|"none",
//! "providers": {"runway": {"key_configured": bool, "source": ..}}}`. Only
//! booleans and a [`KeySource`] enum cross — never key material, never its
//! length, never a prefix. The precedence it reports (Credential Manager →
//! process environment → none) is a tested contract ([`key_source_with`]).

/// Where a key resolved from (D-69-06): the OS Credential Manager slot, the
/// process environment, or nowhere. Serialised snake_case.
#[derive(serde::Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    CredentialManager,
    Environment,
    None,
}

/// One provider's key status — a boolean and a source, nothing else.
#[derive(serde::Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderKeyStatus {
    pub key_configured: bool,
    pub source: KeySource,
}

/// What the Chat UI's connection pill and the Settings surface read (T-12-11 —
/// ONLY booleans and a source enum cross to the renderer, never the key).
///
/// Phase 69 (D-69-14): `source` and `providers` are ADDITIVE — the pre-69
/// `key_configured` field keeps its meaning (`source != none`).
#[derive(serde::Serialize)]
pub struct AgentStatusView {
    pub key_configured: bool,
    pub source: KeySource,
    pub providers: std::collections::BTreeMap<&'static str, ProviderKeyStatus>,
}

/// The Anthropic key's environment fallback (the name `agent_llm` reads).
pub const ANTHROPIC_ENV_FALLBACKS: &[&str] = &["ANTHROPIC_API_KEY"];
/// Runway's environment fallbacks, in `RunwayProvider::connect`'s order.
pub const RUNWAY_ENV_FALLBACKS: &[&str] = &["RUNWAY_API_KEY", "RUNWAYML_API_SECRET"];

/// D-69-06's precedence CONTRACT: Credential Manager → process environment → none. `.env`
/// is not a tier (the shell pre-fills the environment before init). `env` is injected so
/// tests never mutate the process environment.
///
/// A whitespace-only value (stored or in the environment) is not a key. A
/// store read error counts as "not in the store" and falls through.
pub fn key_source_with(
    store: &dyn agent_llm::KeyStore,
    env_names: &[&str],
    env: impl Fn(&str) -> Option<String>,
) -> KeySource {
    if matches!(store.get(), Ok(Some(k)) if !k.trim().is_empty()) {
        return KeySource::CredentialManager;
    }
    if env_names
        .iter()
        .any(|name| env(name).is_some_and(|v| !v.trim().is_empty()))
    {
        return KeySource::Environment;
    }
    KeySource::None
}

/// [`key_source_with`] against the real process environment.
pub fn key_source(store: &dyn agent_llm::KeyStore, env_names: &[&str]) -> KeySource {
    key_source_with(store, env_names, |n| {
        std::env::var(n).ok().filter(|v| !v.trim().is_empty())
    })
}

fn provider_status(
    provider_stores: &crate::ManagedProviderKeyStore,
    provider: &str,
    env_names: &[&str],
) -> ProviderKeyStatus {
    let source = provider_stores
        .slot(provider)
        .map(|store| key_source(store, env_names))
        .unwrap_or(KeySource::None);
    ProviderKeyStatus {
        key_configured: source != KeySource::None,
        source,
    }
}

/// Chat connection + Settings status. Returns ONLY booleans and sources — never
/// the key material itself (T-12-11). The Anthropic key may live in the OS
/// keychain (BYO-key) OR the `ANTHROPIC_API_KEY` env fallback; the Runway key in
/// its provider slot OR `RUNWAY_API_KEY` / `RUNWAYML_API_SECRET`.
///
/// Phase 69 (D-69-14): `providers` carries exactly one entry, `"runway"` — the
/// one provider Settings manages (`SETTINGS_PROVIDER_ALLOW_LIST`).
pub fn run_agent_status(
    key_store: &dyn agent_llm::KeyStore,
    provider_stores: &crate::ManagedProviderKeyStore,
) -> AgentStatusView {
    let source = key_source(key_store, ANTHROPIC_ENV_FALLBACKS);
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        agent_gen::RUNWAY_PROVIDER_ID,
        provider_status(provider_stores, agent_gen::RUNWAY_PROVIDER_ID, RUNWAY_ENV_FALLBACKS),
    );
    AgentStatusView {
        key_configured: source != KeySource::None,
        source,
        providers,
    }
}

/// Persist the user's OWN Anthropic Console API key (BYO-key, AUTH-02). The key
/// is format-validated then written to the OS keychain by the `KeyStore` impl;
/// a malformed key returns an `Err` (its message names the expected `sk-ant-`
/// prefix) and is NEVER stored — it cannot clobber a previously-good key
/// (T-15-04). The key material NEVER crosses back to the renderer.
///
/// The `#[tauri::command] fn set_api_key` in `src-tauri` is now exactly this
/// call (plan 47-01; origin: that file's `set_api_key_inner`, byte-copied).
pub fn run_set_api_key(key_store: &dyn agent_llm::KeyStore, key: &str) -> Result<(), String> {
    key_store.set(key).map_err(|e| e.to_string())
}

/// Remove the stored key from the OS keychain; the Chat pill returns to
/// "disconnected". Idempotent — clearing an already-empty store is `Ok(())`.
///
/// The `#[tauri::command] fn clear_api_key` in `src-tauri` is now exactly this
/// call (plan 47-01; origin: that file's `clear_api_key_inner`, byte-copied).
pub fn run_clear_api_key(key_store: &dyn agent_llm::KeyStore) -> Result<(), String> {
    key_store.clear().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_llm::{InMemoryKeyStore, KeyStore};

    /// A well-formed but FAKE key — never a real credential.
    const FAKE_KEY: &str = "sk-ant-api03-FAKE0000000000000000000000000000000000";
    const FAKE_RUNWAY: &str = "RUDIS69FAKERUNWAYKEY";

    #[test]
    fn key_source_store_beats_environment() {
        let store = InMemoryKeyStore::seeded(FAKE_KEY);
        assert_eq!(
            key_source_with(&store, ANTHROPIC_ENV_FALLBACKS, |_| Some("env-key".into())),
            KeySource::CredentialManager
        );
    }

    #[test]
    fn key_source_environment_when_store_empty() {
        let store = InMemoryKeyStore::new();
        assert_eq!(
            key_source_with(&store, ANTHROPIC_ENV_FALLBACKS, |n| {
                (n == "ANTHROPIC_API_KEY").then(|| "x".into())
            }),
            KeySource::Environment
        );
        // Runway's SECOND fallback name counts too.
        assert_eq!(
            key_source_with(&store, RUNWAY_ENV_FALLBACKS, |n| {
                (n == "RUNWAYML_API_SECRET").then(|| "x".into())
            }),
            KeySource::Environment
        );
    }

    #[test]
    fn key_source_whitespace_env_is_none() {
        let store = InMemoryKeyStore::new();
        assert_eq!(
            key_source_with(&store, ANTHROPIC_ENV_FALLBACKS, |_| Some("   ".into())),
            KeySource::None
        );
        assert_eq!(
            key_source_with(&store, ANTHROPIC_ENV_FALLBACKS, |_| None),
            KeySource::None
        );
    }

    fn provider_stores(runway: Option<&str>) -> crate::ManagedProviderKeyStore {
        let mut map: std::collections::HashMap<String, Box<dyn agent_llm::KeyStore>> =
            std::collections::HashMap::new();
        for &(id, _, validator) in crate::PROVIDER_KEY_SLOTS {
            let store = InMemoryKeyStore::with_validator(validator);
            if id == "runway" {
                if let Some(k) = runway {
                    store.set(k).unwrap();
                }
            }
            map.insert(id.to_string(), Box::new(store));
        }
        crate::ManagedProviderKeyStore::with_stores(map)
    }

    #[test]
    fn key_source_agent_status_wire_shape_is_booleans_and_enums_only() {
        let key_store = InMemoryKeyStore::seeded(FAKE_KEY);
        let stores = provider_stores(Some(FAKE_RUNWAY));
        let view = run_agent_status(&key_store, &stores);
        let value = serde_json::to_value(&view).unwrap();

        let keys: std::collections::BTreeSet<&str> =
            value.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["key_configured", "providers", "source"].into_iter().collect()
        );
        assert_eq!(value["key_configured"], serde_json::json!(true));
        assert_eq!(value["source"], serde_json::json!("credential_manager"));

        let providers = value["providers"].as_object().unwrap();
        assert_eq!(providers.len(), 1, "exactly one provider entry: runway");
        let runway = providers["runway"].as_object().unwrap();
        let rkeys: std::collections::BTreeSet<&str> = runway.keys().map(String::as_str).collect();
        assert_eq!(rkeys, ["key_configured", "source"].into_iter().collect());
        assert_eq!(runway["key_configured"], serde_json::json!(true));
        assert_eq!(runway["source"], serde_json::json!("credential_manager"));

        let text = value.to_string();
        assert!(!text.contains(FAKE_KEY), "the Anthropic key never crosses");
        assert!(!text.contains(FAKE_RUNWAY), "the Runway key never crosses");
    }

    #[test]
    fn key_source_serialises_snake_case() {
        assert_eq!(
            serde_json::to_value(KeySource::CredentialManager).unwrap(),
            serde_json::json!("credential_manager")
        );
        assert_eq!(
            serde_json::to_value(KeySource::Environment).unwrap(),
            serde_json::json!("environment")
        );
        assert_eq!(serde_json::to_value(KeySource::None).unwrap(), serde_json::json!("none"));
    }
}
