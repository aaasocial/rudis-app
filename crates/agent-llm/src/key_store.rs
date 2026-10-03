//! BYO-key credential seam (Phase 15, AUTH-02; generalized Phase 31, GEN-06).
//!
//! A user connects their OWN Anthropic Console API key in-app; it is
//! format-validated, then persisted in the OS-native credential store
//! (Windows Credential Manager via the `keyring` crate) — NEVER a plaintext
//! file, NEVER logged or serialized (T-15-01/T-15-02).
//!
//! `KeyringStore` is the REAL, production store. It is compiled but NEVER
//! constructed by `cargo test` (that would mutate the dev machine's real OS
//! credential store — non-hermetic). Every automated test drives
//! [`InMemoryKeyStore`] instead — the same "always-compiled test double"
//! precedent as `FixtureTransport` (PROVENANCE Entry 7/9), which is why the
//! double is NOT `#[cfg(test)]`-gated (rudis-app's own tests need it too).
//!
//! # Phase 31 (GEN-06): one store type, N provider slots
//!
//! `KeyringStore` was a bare unit struct with a hardcoded `SERVICE`/`ACCOUNT`
//! pair and a hardcoded `sk-ant-` validator. External generation providers
//! (Phase 31+) each need their OWN credential slot with their OWN key format,
//! so those three things became CONSTRUCTOR PARAMETERS
//! (`{service, account, validator}`) rather than constants baked into the impl.
//! [`KeyringStore::anthropic`] reproduces the original triple exactly, so the
//! Phase-15 path is behaviorally identical — the pre-Phase-31 tests below pass
//! unmodified, which is the regression proof (T-31-05). New slots are built
//! with [`KeyringStore::for_provider`] from a compile-time table of
//! `&'static str` coordinates (`crates/app-core/src/generation_host.rs`), so no caller can
//! address an arbitrary keychain entry (T-31-04). The [`KeyStore`] TRAIT is
//! unchanged — it was already provider-agnostic in shape.
//!
//! # Phase 69 (OSS-02): the test-scoped credential SERVICE
//!
//! The SERVICE half of the coordinates is no longer a hardcoded `"rudis"` at
//! every construction site: a [`CredentialService`] is threaded from the host's
//! `InitConfig` to every `KeyringStore` the process builds
//! ([`KeyringStore::anthropic_in`], [`KeyringStore::for_provider_in`]), so a UIA
//! harness can run the whole app under `rudis-test-<run>` and provably never
//! address the owner's real `*.rudis` entries. The rule is FAIL-CLOSED: only a
//! value matching `^rudis-test-[A-Za-z0-9-]{1,48}$` is honoured; anything else
//! (a typo, `"rudis"` itself, a foreign service name) falls back to the
//! production service. Production coordinates are byte-identical to before —
//! the Windows TargetName is `{account}.{service}`, so the owner's existing
//! `anthropic-api-key.rudis` entry keeps working with no migration (D-69-01).

/// The credential-store coordinates the OS keychain entry is filed under.
const SERVICE: &str = "rudis";
const ACCOUNT: &str = "anthropic-api-key";

/// The only prefix a non-production credential service may carry (D-69-02).
pub const TEST_SERVICE_PREFIX: &str = "rudis-test-";
/// Upper bound (in bytes) on the suffix after [`TEST_SERVICE_PREFIX`].
pub const TEST_SERVICE_SUFFIX_MAX: usize = 48;

/// `Some(raw)` iff `raw` is `rudis-test-` + 1..=48 chars of `[A-Za-z0-9-]`.
///
/// No regex crate: a manual byte loop (the ffi crate must not grow a
/// dependency for this). A non-ASCII char therefore rejects, as does any
/// whitespace, dot, or upper-case prefix.
pub fn validate_credential_service(raw: &str) -> Option<&str> {
    let suffix = raw.strip_prefix(TEST_SERVICE_PREFIX)?;
    if suffix.is_empty() || suffix.len() > TEST_SERVICE_SUFFIX_MAX {
        return None;
    }
    if suffix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Some(raw)
    } else {
        None
    }
}

/// The credential-store SERVICE every Rudis entry is filed under (Windows TargetName is
/// `{account}.{service}`). Production is `"rudis"`. Phase 69 (OSS-02, D-69-02): a
/// harness may namespace the process under a TEST service so no test ever addresses a
/// real credential — accepted ONLY when it matches `^rudis-test-[A-Za-z0-9-]{1,48}$`;
/// anything else (a typo, `"rudis"` itself, a foreign service) is never honoured — a bad
/// value can never point at someone else's entry. [`from_config`](Self::from_config) maps it
/// to production; the host-config path ([`try_from_config`](Self::try_from_config), used by
/// `rudis_init`) REFUSES it instead (review 69 WR-01), so a harness can never be silently
/// pointed at the owner's real production entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialService(std::borrow::Cow<'static, str>);

impl CredentialService {
    /// The production service, `"rudis"`.
    pub const PRODUCTION: Self = Self(std::borrow::Cow::Borrowed(SERVICE));

    /// The production service, `"rudis"`.
    pub fn production() -> Self {
        Self::PRODUCTION
    }

    /// The host's (optional) configured service, validated. `None` or any value
    /// failing [`validate_credential_service`] yields [`PRODUCTION`](Self::PRODUCTION)
    /// — a fallback, not an error (fail-closed).
    pub fn from_config(raw: Option<&str>) -> Self {
        match raw.and_then(validate_credential_service) {
            Some(s) => Self(std::borrow::Cow::Owned(s.to_owned())),
            None => Self::PRODUCTION,
        }
    }

    /// The host-config form (review 69 WR-01): an ABSENT service is production, but a
    /// SUPPLIED value that fails [`validate_credential_service`] is REFUSED (`None`) —
    /// the one caller that sets the field is a test harness, and silently falling back
    /// to production there would point it at the owner's real entries. The refusal
    /// carries no value (the rejected string could be anything, even a key).
    pub fn try_from_config(raw: Option<&str>) -> Option<Self> {
        match raw {
            None => Some(Self::PRODUCTION),
            Some(raw) => validate_credential_service(raw)
                .map(|s| Self(std::borrow::Cow::Owned(s.to_owned()))),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `true` iff this is a validated `rudis-test-*` service (never production).
    pub fn is_test_scoped(&self) -> bool {
        self.0.starts_with(TEST_SERVICE_PREFIX)
    }
}

impl std::fmt::Display for CredentialService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Anthropic Console API keys are `sk-ant-...`; anything shorter than this
/// (even with the right prefix) is a paste error, never a real key. Real keys
/// run ~100+ chars — this floor only rejects obvious garbage without pinning a
/// brittle exact length the vendor may change.
const KEY_PREFIX: &str = "sk-ant-";
const MIN_KEY_LEN: usize = 20;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyStoreError {
    #[error("that doesn't look like an Anthropic API key (expected it to start with \"sk-ant-\")")]
    InvalidFormat,
    // T-15-02: carries ONLY the keyring crate's own error text, never the key.
    #[error("secure storage error: {0}")]
    Backend(String),
}

/// Validate the SHAPE of a candidate key (prefix + minimum length) WITHOUT ever
/// contacting the network — a malformed key is rejected here, before any store
/// write, so it can never clobber a previously-good key (T-15-04). The caller is
/// expected to have trimmed already; this also tolerates surrounding whitespace
/// defensively.
pub fn validate_key_format(key: &str) -> Result<(), KeyStoreError> {
    let key = key.trim();
    if key.starts_with(KEY_PREFIX) && key.len() >= MIN_KEY_LEN {
        Ok(())
    } else {
        Err(KeyStoreError::InvalidFormat)
    }
}

/// The credential-store seam. `Send + Sync` because it is held as
/// Tauri-managed state (`Box<dyn KeyStore>`) shared across command handlers.
pub trait KeyStore: Send + Sync {
    /// Format-validate the TRIMMED key, then persist it. A rejected key is
    /// NEVER stored and NEVER clobbers an existing one (T-15-04).
    fn set(&self, api_key: &str) -> Result<(), KeyStoreError>;
    /// The stored key, or `Ok(None)` when nothing is stored (an empty store is
    /// NOT an error).
    fn get(&self) -> Result<Option<String>, KeyStoreError>;
    /// Remove the stored key. Clearing an already-empty store is `Ok(())`
    /// (idempotent, not an error).
    fn clear(&self) -> Result<(), KeyStoreError>;
}

/// A key-format validator: the per-provider shape check run on the TRIMMED key
/// before any store write. Phase 31 (GEN-06) makes this a field rather than a
/// hardcoded call, because every provider's key format differs — Anthropic's
/// `sk-ant-` rule is now just the value [`KeyringStore::anthropic`] supplies.
pub type KeyValidator = fn(&str) -> Result<(), KeyStoreError>;

/// REAL, production store: the OS-native credential manager via `keyring`.
/// NEVER constructed by a `cargo test` — see the module doc.
///
/// Phase 31 (GEN-06): generalized from a bare unit struct to a
/// `{service, account, validator}` triple, so ONE type serves N provider
/// credential slots. The Anthropic path is byte-identical to the pre-Phase-31
/// behavior — see [`KeyringStore::anthropic`].
///
/// Phase 69 (OSS-02): the service is an owned `Cow` so a validated
/// [`CredentialService`] can scope the store; `Copy` was dropped (it had no
/// dependants).
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: std::borrow::Cow<'static, str>,
    account: &'static str,
    validator: KeyValidator,
}

impl KeyringStore {
    /// The Anthropic BYO-key slot (Phase 15, AUTH-02) — behaviorally identical
    /// to the pre-Phase-31 unit struct: `SERVICE`/`ACCOUNT`/[`validate_key_format`].
    /// The one production caller is the host cdylib's context construction
    /// (`crates/ffi/src/lib.rs`).
    pub const fn anthropic() -> Self {
        Self {
            service: std::borrow::Cow::Borrowed(SERVICE),
            account: ACCOUNT,
            validator: validate_key_format,
        }
    }

    /// The Anthropic slot under a caller-chosen (validated) service — `rudis_init`'s path.
    pub fn anthropic_in(service: &CredentialService) -> Self {
        Self {
            service: service.0.clone(),
            account: ACCOUNT,
            validator: validate_key_format,
        }
    }

    /// A provider slot under a validated service — `ManagedProviderKeyStore::production_in`'s path.
    pub fn for_provider_in(
        service: &CredentialService,
        account: &'static str,
        validator: KeyValidator,
    ) -> Self {
        Self {
            service: service.0.clone(),
            account,
            validator,
        }
    }

    /// The exact Windows Credential Manager TargetName this store addresses
    /// (`{account}.{service}` — keyring's windows-native store mapping). COORDINATES ONLY;
    /// there is no secret in this string. Used by tests and by the harness's cleanup rule.
    pub fn windows_target_name(&self) -> String {
        format!("{}.{}", self.account, self.service)
    }

    /// A SECOND (or Nth) credential slot (Phase 31, GEN-06): service, account
    /// and validator are all caller-supplied, so no `sk-ant-` assumption leaks
    /// into a non-Anthropic provider's key. Callers pass `&'static str`
    /// coordinates from a compile-time table (see
    /// `app_core::generation_host::PROVIDER_KEY_SLOTS`), so a caller can
    /// never address an arbitrary keychain entry (T-31-04).
    pub const fn for_provider(
        service: &'static str,
        account: &'static str,
        validator: KeyValidator,
    ) -> Self {
        Self {
            service: std::borrow::Cow::Borrowed(service),
            account,
            validator,
        }
    }

    fn entry(&self) -> Result<keyring::Entry, KeyStoreError> {
        keyring::Entry::new(&self.service, self.account)
            .map_err(|e| KeyStoreError::Backend(e.to_string()))
    }
}

impl KeyStore for KeyringStore {
    fn set(&self, api_key: &str) -> Result<(), KeyStoreError> {
        let trimmed = api_key.trim();
        (self.validator)(trimmed)?; // validate BEFORE any write (T-15-04)
        self.entry()?
            .set_password(trimmed)
            .map_err(|e| KeyStoreError::Backend(e.to_string()))
    }

    fn get(&self) -> Result<Option<String>, KeyStoreError> {
        match self.entry()?.get_password() {
            Ok(pw) => Ok(Some(pw)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(KeyStoreError::Backend(e.to_string())),
        }
    }

    fn clear(&self) -> Result<(), KeyStoreError> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()), // idempotent
            Err(e) => Err(KeyStoreError::Backend(e.to_string())),
        }
    }
}

/// Always-compiled test double (mirrors `FixtureTransport`): an in-memory
/// `Mutex<Option<String>>`. Every automated test across the workspace drives
/// this, so `cargo test` NEVER touches the real OS credential store.
#[derive(Debug)]
pub struct InMemoryKeyStore {
    key: std::sync::Mutex<Option<String>>,
    /// Phase 31 (GEN-06): mirrors [`KeyringStore`]'s validator field so a
    /// provider-slot test double validates like its real counterpart.
    /// [`new`](Self::new)/[`seeded`](Self::seeded) default it to
    /// [`validate_key_format`], keeping every pre-Phase-31 call site
    /// byte-compatible.
    validator: KeyValidator,
}

impl Default for InMemoryKeyStore {
    fn default() -> Self {
        Self {
            key: std::sync::Mutex::new(None),
            validator: validate_key_format,
        }
    }
}

impl InMemoryKeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a key DIRECTLY, bypassing format validation — test setup only (so a
    /// test can assert a rejected `set` does not clobber a pre-existing key
    /// without depending on `set`'s own validation to have stored it).
    pub fn seeded(key: &str) -> Self {
        Self {
            key: std::sync::Mutex::new(Some(key.to_string())),
            validator: validate_key_format,
        }
    }

    /// An empty double validating with a CALLER-SUPPLIED rule (Phase 31,
    /// GEN-06) — the in-memory counterpart of
    /// [`KeyringStore::for_provider`], so a provider key slot can be exercised
    /// in tests without touching the real OS credential store.
    pub fn with_validator(validator: KeyValidator) -> Self {
        Self {
            key: std::sync::Mutex::new(None),
            validator,
        }
    }
}

impl KeyStore for InMemoryKeyStore {
    fn set(&self, api_key: &str) -> Result<(), KeyStoreError> {
        let trimmed = api_key.trim();
        (self.validator)(trimmed)?; // validate BEFORE mutating (T-15-04)
        *self
            .key
            .lock()
            .map_err(|_| KeyStoreError::Backend("in-memory key store poisoned".to_string()))? =
            Some(trimmed.to_string());
        Ok(())
    }

    fn get(&self) -> Result<Option<String>, KeyStoreError> {
        Ok(self
            .key
            .lock()
            .map_err(|_| KeyStoreError::Backend("in-memory key store poisoned".to_string()))?
            .clone())
    }

    fn clear(&self) -> Result<(), KeyStoreError> {
        *self
            .key
            .lock()
            .map_err(|_| KeyStoreError::Backend("in-memory key store poisoned".to_string()))? =
            None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed but FAKE key (never a real credential) — the only shape
    /// every test may store. Real keys never appear in the test suite.
    const FAKE_KEY: &str = "sk-ant-api03-FAKE0000000000000000000000000000000000";

    #[test]
    fn validate_rejects_empty() {
        assert_eq!(validate_key_format(""), Err(KeyStoreError::InvalidFormat));
    }

    #[test]
    fn validate_rejects_wrong_prefix() {
        assert_eq!(
            validate_key_format("sk-openai-1234567890123456789012345678"),
            Err(KeyStoreError::InvalidFormat)
        );
    }

    #[test]
    fn validate_rejects_too_short_even_with_right_prefix() {
        assert_eq!(
            validate_key_format("sk-ant-x"),
            Err(KeyStoreError::InvalidFormat)
        );
    }

    #[test]
    fn validate_accepts_well_formed_fake_key() {
        assert_eq!(validate_key_format(FAKE_KEY), Ok(()));
    }

    #[test]
    fn in_memory_new_is_empty() {
        let store = InMemoryKeyStore::new();
        assert_eq!(store.get(), Ok(None));
    }

    #[test]
    fn in_memory_set_then_get_roundtrips() {
        let store = InMemoryKeyStore::new();
        store.set(FAKE_KEY).expect("valid key stores");
        assert_eq!(store.get(), Ok(Some(FAKE_KEY.to_string())));
    }

    #[test]
    fn in_memory_clear_empties() {
        let store = InMemoryKeyStore::new();
        store.set(FAKE_KEY).unwrap();
        store.clear().expect("clear ok");
        assert_eq!(store.get(), Ok(None));
    }

    #[test]
    fn in_memory_set_trims_surrounding_whitespace() {
        let store = InMemoryKeyStore::new();
        store.set(&format!("  {FAKE_KEY}\n")).expect("trims then stores");
        assert_eq!(store.get(), Ok(Some(FAKE_KEY.to_string())));
    }

    #[test]
    fn in_memory_store_rejects_malformed_without_clobbering_existing_key() {
        let store = InMemoryKeyStore::seeded(FAKE_KEY);
        assert_eq!(
            store.set("not-a-key"),
            Err(KeyStoreError::InvalidFormat),
            "a malformed set is rejected"
        );
        assert_eq!(
            store.get(),
            Ok(Some(FAKE_KEY.to_string())),
            "the previously stored key is NOT clobbered by a rejected set"
        );
    }

    // ---------------------------------------------------------------------
    // Phase 31 (GEN-06): validator parameterization. Everything ABOVE this
    // line is the pre-Phase-31 suite, unmodified — the regression proof that
    // generalizing `KeyringStore` to {service, account, validator} left the
    // Anthropic path behaviorally identical (T-31-05).
    // ---------------------------------------------------------------------

    /// A stand-in for a non-Anthropic provider's key rule: anything with at
    /// least 4 characters. Deliberately NOTHING like `sk-ant-`, so a test can
    /// tell which validator actually ran.
    fn min_four_chars(key: &str) -> Result<(), KeyStoreError> {
        if key.trim().len() >= 4 {
            Ok(())
        } else {
            // T-15-02/T-31-01: names the RULE, never echoes the key.
            Err(KeyStoreError::Backend(
                "provider key must be at least 4 characters".to_string(),
            ))
        }
    }

    #[test]
    fn supplied_validator_dispatches_instead_of_the_anthropic_rule() {
        let store = InMemoryKeyStore::with_validator(min_four_chars);

        assert!(
            store.set("abc").is_err(),
            "the SUPPLIED validator rejects a 3-char key"
        );
        store
            .set("abcd")
            .expect("the SUPPLIED validator accepts a 4-char key");
        assert_eq!(
            store.get(),
            Ok(Some("abcd".to_string())),
            "a key the supplied validator accepts is stored verbatim — the \
             sk-ant- rule did NOT run"
        );
    }

    #[test]
    fn default_store_still_requires_the_anthropic_prefix() {
        // The mirror image of the test above: with no validator supplied, the
        // pre-Phase-31 sk-ant- rule is still in force.
        let store = InMemoryKeyStore::new();
        assert_eq!(
            store.set("abcd"),
            Err(KeyStoreError::InvalidFormat),
            "the DEFAULT validator is still validate_key_format"
        );
        assert_eq!(store.get(), Ok(None), "the rejected key was never stored");
    }

    #[test]
    fn validator_rejection_error_never_echoes_the_submitted_key() {
        // T-15-02 discipline must survive validator dispatch: an error names
        // the violated rule, never the key material.
        const SUBMITTED: &str = "xyz";
        let provider_store = InMemoryKeyStore::with_validator(min_four_chars);
        let err = provider_store
            .set(SUBMITTED)
            .expect_err("too-short key is rejected");
        assert!(
            !err.to_string().contains(SUBMITTED),
            "the error must not echo the submitted key, got: {err}"
        );

        const BAD_ANTHROPIC: &str = "sk-openai-secret-value";
        let default_store = InMemoryKeyStore::new();
        let err = default_store
            .set(BAD_ANTHROPIC)
            .expect_err("wrong-prefix key is rejected");
        assert!(
            !err.to_string().contains(BAD_ANTHROPIC),
            "the error must not echo the submitted key, got: {err}"
        );
    }

    #[test]
    fn anthropic_constructor_reproduces_the_pre_phase_31_coordinates() {
        // `KeyringStore` is NEVER exercised against the real OS keychain by a
        // test (module doc), so this asserts on its CONSTRUCTED FIELDS: the
        // exact SERVICE/ACCOUNT/validator triple the unit struct hardcoded.
        let anthropic = KeyringStore::anthropic();
        assert_eq!(anthropic.service, SERVICE);
        assert_eq!(anthropic.account, ACCOUNT);
        assert_eq!(
            (anthropic.validator)(FAKE_KEY),
            Ok(()),
            "the Anthropic slot's validator accepts a well-formed sk-ant- key"
        );
        assert_eq!(
            (anthropic.validator)("abcd"),
            Err(KeyStoreError::InvalidFormat),
            "the Anthropic slot's validator is validate_key_format"
        );
    }

    #[test]
    fn for_provider_carries_its_own_coordinates_and_validator() {
        let slot = KeyringStore::for_provider("rudis", "gen-fixture-api-key", min_four_chars);
        assert_eq!(
            slot.service,
            SERVICE,
            "provider slots share the Anthropic keychain SERVICE ..."
        );
        assert_eq!(
            slot.account, "gen-fixture-api-key",
            "... but occupy a DISTINCT account, so the Anthropic entry is untouched"
        );
        assert_ne!(
            slot.account, ACCOUNT,
            "a provider slot must never collide with the Anthropic account"
        );
        assert_eq!((slot.validator)("abcd"), Ok(()));
        assert!(
            (slot.validator)(FAKE_KEY).is_ok(),
            "the provider slot applies ITS rule, not the sk-ant- one"
        );
    }

    // ---------------------------------------------------------------------
    // Phase 69 (OSS-02): the test-scoped credential service
    // ---------------------------------------------------------------------

    #[test]
    fn credential_service_validator_accept_reject_table() {
        let long_ok = format!("rudis-test-{}", "a".repeat(48));
        let too_long = format!("rudis-test-{}", "a".repeat(49));
        let rows: &[(&str, bool)] = &[
            ("", false),
            ("rudis", false),
            ("rudis-test-", false),
            (too_long.as_str(), false),
            (long_ok.as_str(), true),
            ("rudis-test-a.b", false),
            ("rudis-test-a b", false),
            ("rudis-test-é", false),
            ("rudis-test-a\n", false),
            ("RUDIS-TEST-a", false),
            ("rudis-test-..x", false),
            ("anthropic-api-key.rudis", false),
            (" rudis-test-a", false),
            ("rudis-test-69-0123456789abcdef", true),
            ("rudis-test-69-x", true),
            ("rudis-test-A-z-9", true),
        ];
        for &(raw, accepted) in rows {
            assert_eq!(
                validate_credential_service(raw).is_some(),
                accepted,
                "row {raw:?}: expected accepted={accepted}"
            );
            if accepted {
                assert_eq!(validate_credential_service(raw), Some(raw), "row {raw:?}");
            }
        }
    }

    #[test]
    fn credential_service_from_config_is_fail_closed() {
        assert_eq!(CredentialService::from_config(None), CredentialService::PRODUCTION);
        assert_eq!(
            CredentialService::from_config(Some("rudis")),
            CredentialService::PRODUCTION,
            "\"rudis\" itself is not a test service — falls back, not an error"
        );
        assert_eq!(
            CredentialService::from_config(Some("foreign-service")),
            CredentialService::PRODUCTION
        );
        assert_eq!(CredentialService::PRODUCTION.as_str(), "rudis");
        assert!(!CredentialService::production().is_test_scoped());

        let test = CredentialService::from_config(Some("rudis-test-69-x"));
        assert_eq!(test.as_str(), "rudis-test-69-x");
        assert!(test.is_test_scoped());
        assert_eq!(test.to_string(), "rudis-test-69-x");
    }

    /// Review 69 WR-01: the host-config form refuses a SUPPLIED-but-invalid service
    /// instead of falling back to production; only an ABSENT one means production.
    #[test]
    fn credential_service_try_from_config_refuses_a_supplied_invalid_value() {
        assert_eq!(
            CredentialService::try_from_config(None),
            Some(CredentialService::PRODUCTION)
        );
        for bad in ["rudis", "../evil", "foreign-service", "rudis-test-", "rudis-test-a.b"] {
            assert_eq!(CredentialService::try_from_config(Some(bad)), None, "row {bad:?}");
        }
        let test = CredentialService::try_from_config(Some("rudis-test-69-x")).expect("accepted");
        assert_eq!(test.as_str(), "rudis-test-69-x");
        assert!(test.is_test_scoped());
    }

    #[test]
    fn credential_service_windows_target_names_are_account_dot_service() {
        let test = CredentialService::from_config(Some("rudis-test-69-x"));
        assert_eq!(
            KeyringStore::anthropic().windows_target_name(),
            "anthropic-api-key.rudis",
            "production Anthropic coordinates are byte-identical (D-69-01)"
        );
        assert_eq!(
            KeyringStore::anthropic_in(&CredentialService::PRODUCTION).windows_target_name(),
            "anthropic-api-key.rudis"
        );
        assert_eq!(
            KeyringStore::anthropic_in(&test).windows_target_name(),
            "anthropic-api-key.rudis-test-69-x"
        );
        assert_eq!(
            KeyringStore::for_provider_in(&test, "gen-runway-api-key", min_four_chars)
                .windows_target_name(),
            "gen-runway-api-key.rudis-test-69-x"
        );
        assert_eq!(
            KeyringStore::for_provider("rudis", "gen-fixture-api-key", min_four_chars)
                .windows_target_name(),
            "gen-fixture-api-key.rudis"
        );
    }
}
