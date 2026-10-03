//! The shell-agnostic generation host-glue extracted from
//! `src-tauri/src/generation.rs` (Phase 54.1, SC-5: ONE spend policy, ONE
//! generation path). Every item here moved — never copied — so exactly one
//! definition exists workspace-wide.
//!
//! # What lives here, and why it could travel unchanged
//!
//! 54.1-RESEARCH § Q2 category A measured every item below as **already
//! Tauri-free**: zero `tauri::` / `AppHandle` / `State<'_, T>` tokens inside
//! their own bodies. The `State<'_, T>` wrapping the three `Managed*` provider
//! slots wear in `src-tauri` is applied EXTERNALLY, at the `#[tauri::command]`
//! call site, never inside the types — which is what made this a cut rather
//! than a rewrite.
//!
//! # Plan 54.1-03: the job lifecycle followed, REDESIGNED rather than moved
//!
//! Plan 54.1-01 deliberately left behind `emit_job_event`, `spawn_poll_task`,
//! `start_generation_job`, the two terminal-event `scan_…_payload` helpers and
//! the three `generate_*_for_agent` seams: those were saturated with
//! `AppHandle`, and two of them used Tauri's event bus as an in-process RPC
//! return channel, which has no Tauri-free equivalent.
//!
//! Plan 54.1-03 severed that channel. The seams, the lifecycle and the ONE
//! landing+announce path now live at the bottom of this file:
//! [`poll_job_until_terminal`] (returns its terminal `JobStatus`),
//! [`finalize_ready_job`], [`start_generation_job`], [`GenHost`] and the three
//! `generate_*_for_agent` entries. The two scanners are DELETED workspace-wide
//! — including from comments, so the SC-5 guard could report a literal zero for
//! that identifier prefix — and no code in either host reads an event it
//! emitted to learn an outcome.
//!
//! ⚠ That guard was `src-tauri/tests/single_generation_path.rs`, DELETED with
//! that shell at Phase 55 (GATE-07). It has no successor, so the
//! single-generation-path property is now upheld by discipline alone, not by a
//! mechanical scan.
//!
//! What is left in `src-tauri/src/generation.rs` is only what is genuinely
//! Tauri-shaped: the `#[tauri::command]` wrappers, the two host adapters that
//! resolve managed state (`gen_host_from_app`, `finalize_ready_job_from_app`),
//! and the crate's test modules.
//!
//! # Key-material discipline (T-31-01), carried verbatim
//!
//! A provider key crosses IPC exactly once, inbound, and is handed straight to
//! the key store. It is NEVER written to a project file, NEVER logged, NEVER
//! returned to the renderer, and NEVER interpolated into an error string —
//! every validation failure names the RULE it violated, not the value that
//! violated it (the T-15-02 discipline established in
//! `crates/agent-llm/src/key_store.rs`). `provider_key_status` (which stays in
//! `src-tauri` as a `#[tauri::command]`) deliberately returns a BOOLEAN
//! presence flag, never the key.
//!
//! # Pitfall-2 standing note (54.1-RESEARCH)
//!
//! The three provider slots and [`ManagedProviderKeyStore`] are PLAIN STRUCTS.
//! A second host must hold them as ordinary `RudisCtx`-style struct fields —
//! NEVER route them through `tauri::State` in any new code.

// ---------------------------------------------------------------------------
// Provider credential slots (Phase 31 Wave 1, GEN-06)
// ---------------------------------------------------------------------------

/// Upper bound on a provider API key. Real provider keys run well under this
/// (Anthropic's are ~100 chars); the cap exists to reject hostile oversized
/// input cheaply, before any keychain call (T-31-03).
pub const MAX_PROVIDER_KEY_LEN: usize = 512;

/// The DEFAULT (production) keychain SERVICE every Rudis credential is filed
/// under — the same one the Anthropic key uses (`agent_llm::key_store`'s
/// `SERVICE`). Providers are separated by distinct ACCOUNTs, not distinct
/// services, so one OS credential group holds everything Rudis owns.
///
/// Phase 69 (D-69-02): this is now only the default. The service a process
/// actually addresses is decided by the validated
/// [`agent_llm::CredentialService`] passed to
/// [`ManagedProviderKeyStore::production_in`]; no construction site uses this
/// literal any more. Kept (and re-exported) as the named production value.
pub const KEYCHAIN_SERVICE: &str = "rudis";

/// Format-validate a generic provider API key. Deliberately NOT Anthropic's
/// `sk-ant-` rule: every provider's key format differs, and Phase 31 has no
/// real provider whose format could be asserted. So this checks only what is
/// universally true of an API key — non-empty, bounded, and a single token
/// (internal whitespace means a paste error, e.g. a copied "Bearer <key>" or a
/// wrapped line).
///
/// T-31-01: the error names the violated RULE and never echoes `key`.
pub fn validate_provider_key_format(key: &str) -> Result<(), agent_llm::KeyStoreError> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(agent_llm::KeyStoreError::Backend(
            "the provider API key is empty".to_string(),
        ));
    }
    if trimmed.len() > MAX_PROVIDER_KEY_LEN {
        return Err(agent_llm::KeyStoreError::Backend(format!(
            "the provider API key is too long (limit {MAX_PROVIDER_KEY_LEN} characters)"
        )));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(agent_llm::KeyStoreError::Backend(
            "the provider API key contains whitespace — paste the key alone, \
             without a \"Bearer \" prefix or a line break"
                .to_string(),
        ));
    }
    Ok(())
}

/// The compile-time registry of provider credential slots:
/// `(provider id, keychain account, key validator)`.
///
/// Provider ids resolve ONLY through this table, and every account is a
/// `&'static str` fixed at compile time — so a renderer-supplied provider id
/// can never address an arbitrary keychain entry (T-31-04). An unknown id is an
/// error, not a new slot.
///
/// One row per REAL provider that has cleared the GEN-08 licensing gate. Phases
/// 32/33 registered `"openai"` and `"google"` rows; **Phase 42.1 REMOVED both**
/// in the same commit that deleted those providers — a credential slot for a
/// provider no code can reach is a live key-storage surface guarding nothing.
/// The `"fixture"` row exists because
/// GEN-06's storage mechanism must be proven end-to-end on the EXACT path real
/// providers will use; note that `FixtureGenProvider` itself never READS this
/// key (it is zero-network by construction, 31-RESEARCH.md Pitfall 4) — the
/// slot proves the credential plumbing, not a credential's use.
pub const PROVIDER_KEY_SLOTS: &[(&str, &str, agent_llm::KeyValidator)] = &[
    ("fixture", "gen-fixture-api-key", validate_provider_key_format),
    // The ElevenLabs (audio TTS) slot, registered Wave 34-02. NOTE: this slot's
    // presence does NOT open a paid call — there is STILL no `(elevenlabs, *)`
    // production_allow_list row this wave (fail-closed), so every real elevenlabs
    // submit is gate-rejected until the 34-03 human GEN-08 sign-off. No
    // ElevenLabs-specific validator: their keys (a bare hex-ish token, no stable
    // documented prefix) pass the generic non-empty/<=512/no-whitespace check, and
    // a Rudis-side prefix assumption is the SAME maintenance liability the openai/
    // google rows deliberately avoid (32-RESEARCH.md Don't Hand-Roll) — not a
    // security gain.
    ("elevenlabs", "gen-elevenlabs-api-key", validate_provider_key_format),
    // Phase 42.1 (Plan 02): the Runway slot — the SINGLE provider that now serves
    // BOTH image and video generation, replacing the openai and google rows
    // outright rather than joining them. No Runway-specific validator: the generic
    // format check is DELIBERATE, for the SAME reason recorded on the openai /
    // google / elevenlabs rows before them — a vendor key prefix is not a stable
    // contract (OpenAI's churned sk- → sk-proj-), so a Rudis-side prefix
    // assumption is a maintenance trap, not a security gain (32-RESEARCH.md
    // Don't Hand-Roll).
    ("runway", "gen-runway-api-key", validate_provider_key_format),
];

/// Host-managed provider credential stores, keyed by provider id — the
/// multi-slot sibling of `ManagedKeyStore` (which holds the single Anthropic
/// slot). Kept as SEPARATE managed state rather than folded into
/// `ManagedKeyStore` so `configure_with_key_store`'s signature — which every
/// pre-existing Phase-15 test depends on — stays untouched.
///
/// Phase 54.1: moved here from `src-tauri/src/generation.rs`. It was never
/// Tauri-shaped — the Tauri shell wraps it in `State<'_, T>` at the command
/// boundary. A second host holds it as a plain struct field (54.1-RESEARCH
/// Pitfall 2).
pub struct ManagedProviderKeyStore(
    pub std::collections::HashMap<String, Box<dyn agent_llm::KeyStore>>,
);

impl ManagedProviderKeyStore {
    /// Production wiring under the PRODUCTION service (`"rudis"`) — exactly
    /// [`production_in`](Self::production_in)`(&CredentialService::PRODUCTION)`.
    /// Never constructed by a test (that would mutate the dev machine's real
    /// credential store) — tests use [`with_stores`](Self::with_stores).
    pub fn production() -> Self {
        Self::production_in(&agent_llm::CredentialService::PRODUCTION)
    }

    /// Phase 69 (D-69-02, D-69-04): one REAL OS-keychain slot per
    /// [`PROVIDER_KEY_SLOTS`] row, every one of them filed under `service`.
    ///
    /// EVERY slot — ElevenLabs included — takes the service, so a process
    /// running under a `rudis-test-*` service can read no production entry for
    /// any provider (criterion 4's "credential entries absent" must hold for
    /// every slot the process can read). The ffi host's `rudis_init` is the
    /// one production caller; the provider slots resolve ONLY through the
    /// store this builds (the retired per-use slot-lookup helper hardcoded the
    /// production service and is gone).
    pub fn production_in(service: &agent_llm::CredentialService) -> Self {
        Self(
            PROVIDER_KEY_SLOTS
                .iter()
                .map(|&(provider, account, validator)| {
                    let store: Box<dyn agent_llm::KeyStore> = Box::new(
                        agent_llm::KeyringStore::for_provider_in(service, account, validator),
                    );
                    (provider.to_string(), store)
                })
                .collect(),
        )
    }

    /// The Windows Credential Manager TargetName of every slot under `service`,
    /// in [`PROVIDER_KEY_SLOTS`] order: `(provider id, "{account}.{service}")`.
    /// A pure function — COORDINATES ONLY, no store is constructed and nothing
    /// is read. The harness uses it to know exactly which entries a test
    /// service could have created (D-69-04: every slot is namespaced).
    pub fn slot_target_names(
        service: &agent_llm::CredentialService,
    ) -> Vec<(&'static str, String)> {
        PROVIDER_KEY_SLOTS
            .iter()
            .map(|&(provider, account, _)| (provider, format!("{account}.{service}")))
            .collect()
    }

    /// Test seam: inject in-memory doubles, mirroring how `byo_key_gate`'s mock
    /// app wires an `InMemoryKeyStore` through `configure_with_key_store`.
    pub fn with_stores(
        stores: std::collections::HashMap<String, Box<dyn agent_llm::KeyStore>>,
    ) -> Self {
        Self(stores)
    }

    /// Resolve a host-supplied provider id to its slot. The error names the
    /// id (which the caller already knows) and never the key material.
    ///
    /// `pub` (was private) since Phase 54.1: the `set_provider_key` /
    /// `clear_provider_key` / `provider_key_status` `_inner` helpers stayed in
    /// `src-tauri` with their `#[tauri::command]` wrappers, so they now call
    /// this across a crate boundary.
    pub fn slot(&self, provider: &str) -> Result<&dyn agent_llm::KeyStore, String> {
        self.0
            .get(provider)
            .map(|b| b.as_ref())
            .ok_or_else(|| format!("unknown generation provider '{provider}' — no key slot registered"))
    }
}

/// Test tier: one EMPTY `InMemoryKeyStore` per [`PROVIDER_KEY_SLOTS`] row,
/// shared for the test process — the `stores` every fixture `GenHost` rig
/// carries. Rigs use PRESET slots, so nothing ever reads these; they exist so
/// the seams' "stores are managed" check passes without an OS keychain.
#[cfg(test)]
pub(crate) fn test_provider_stores() -> &'static ManagedProviderKeyStore {
    static STORES: std::sync::OnceLock<ManagedProviderKeyStore> = std::sync::OnceLock::new();
    STORES.get_or_init(|| {
        ManagedProviderKeyStore::with_stores(
            PROVIDER_KEY_SLOTS
                .iter()
                .map(|&(id, _, validator)| {
                    let store: Box<dyn agent_llm::KeyStore> =
                        Box::new(agent_llm::InMemoryKeyStore::with_validator(validator));
                    (id.to_string(), store)
                })
                .collect(),
        )
    })
}

/// The providers Settings may address (D-69-12, T-31-04 preserved): ElevenLabs stays on the
/// developer `.env` path and the fixture slot is a test seam — both are REFUSED here.
pub const SETTINGS_PROVIDER_ALLOW_LIST: &[&str] = &["runway"];

/// The allow-list check both Settings commands run FIRST, before any slot
/// lookup. The error names only the provider id the caller supplied.
fn settings_allowed(provider: &str) -> Result<(), String> {
    if SETTINGS_PROVIDER_ALLOW_LIST.contains(&provider) {
        Ok(())
    } else {
        Err(format!(
            "provider '{provider}' is not managed by Settings (allowed: runway)"
        ))
    }
}

/// Phase 69 (D-69-12, D-69-13): store a provider key from Settings, then
/// INVALIDATE that provider's slots so the key takes effect on the next use —
/// no restart. Order: allow-list → the slot's own validator + write →
/// invalidate. Errors carry the validator's rule text or the store's own
/// message, never the key (T-69-05).
pub fn run_set_provider_key(
    stores: &ManagedProviderKeyStore,
    gen: &GenHost<'_>,
    provider: &str,
    key: &str,
) -> Result<(), String> {
    settings_allowed(provider)?;
    stores.slot(provider)?.set(key).map_err(|e| e.to_string())?;
    gen.invalidate_provider(provider);
    Ok(())
}

/// Phase 69 (D-69-12, D-69-13): clear a provider key from Settings and
/// invalidate its slots. Idempotent (clearing an empty slot is `Ok`).
pub fn run_clear_provider_key(
    stores: &ManagedProviderKeyStore,
    gen: &GenHost<'_>,
    provider: &str,
) -> Result<(), String> {
    settings_allowed(provider)?;
    stores.slot(provider)?.clear().map_err(|e| e.to_string())?;
    gen.invalidate_provider(provider);
    Ok(())
}

// ---------------------------------------------------------------------------
// Async job lifecycle (Phase 31 Wave 2, GEN-05)
// ---------------------------------------------------------------------------

/// State transitions for one generation job — emitted ONCE when the
/// job is registered ("pending") and ONCE when it reaches a terminal state
/// ("ready" / "failed" / "cancelled"). Mirrors `EXPORT_PROGRESS_EVENT`'s
/// constant-plus-payload-struct shape (`lib.rs`).
pub const GEN_JOB_EVENT: &str = "gen:job";

/// Emitted on every poll that finds the job still pending, so a UI can show
/// liveness ("still working, poll 7") without inventing a fake percentage —
/// external providers do not report progress fractions.
pub const GEN_PROGRESS_EVENT: &str = "gen:progress";

/// The renderer's view of a job state change.
///
/// **Byte-free BY CONSTRUCTION** (T-31-06): there is no field of type
/// `AssetRef` or `Vec<u8>` here, and `AssetRef` has no `Serialize` derive to
/// make one accidentally possible. Landed assets reach the renderer as
/// `media_item_ids` — ids of real `MediaBinItem`s already on disk and in the
/// store — never as inline bytes.
#[derive(Clone, serde::Serialize)]
pub struct GenJobEventPayload {
    pub job_id: String,
    pub provider: String,
    pub model_id: String,
    /// `"pending" | "ready" | "failed" | "cancelled"` — sourced from
    /// `JobStatus::wire_state()` so the vocabulary cannot drift per emitter.
    pub state: String,
    /// Present only for `"failed"`.
    pub error: Option<String>,
    /// Filled by Wave 3's completion→MediaBin landing bridge; empty until then
    /// (and always empty for non-`"ready"` states).
    pub media_item_ids: Vec<String>,
    /// **THE user-facing provenance-disclosure hook (GEN-09).**
    ///
    /// Generated media from providers that embed AI-provenance watermarks
    /// (C2PA / SynthID on OpenAI + Google image/video output) is disclosed to
    /// the user AT GENERATION TIME via this flag on the `gen:job` completion
    /// event; the frontend listener surfaces it (UI in Phase 32). Resolved from
    /// the vetted model catalog at submit; a model missing from the catalog
    /// defaults to `true` — conservative OVER-disclosure, never silent
    /// under-disclosure (T-31-19).
    ///
    /// `Some(..)` ONLY on the terminal `"ready"` payload. Disclosure is a
    /// generation-time completion FACT, not ambient state, so `"pending"`,
    /// `"failed"` and `"cancelled"` carry `None` — there is no generated media
    /// to disclose anything about.
    ///
    /// Deliberately NOT persisted on `MediaBinItem` (31-RESEARCH.md Pitfall 2:
    /// that schema has ~128 literal construction sites and no `Default`).
    /// Durable per-asset provenance is a separately-scoped Phase-32+ schema
    /// decision, not a side effect of this wave.
    pub carries_provenance_watermark: Option<bool>,
}

/// Liveness ping for a still-pending job.
#[derive(Clone, serde::Serialize)]
pub struct GenProgressEventPayload {
    pub job_id: String,
    /// 1-based count of polls performed so far.
    pub polls: u32,
}

/// Host-managed job table. An `Arc` so the spawned poll task can own a handle
/// to it — a `State<'_, T>` cannot be held across an `.await` (Pitfall 5).
pub struct ManagedGenJobs(pub std::sync::Arc<agent_gen::JobRegistry>);

impl Default for ManagedGenJobs {
    fn default() -> Self {
        Self(std::sync::Arc::new(agent_gen::JobRegistry::new()))
    }
}

/// Resolve a model's GEN-09 provenance-watermark flag from the provider's
/// vetted catalog.
///
/// Called ONCE at submit — not per poll — and the result is carried in the
/// spawned poll task's captured state to the terminal emit. Reading the catalog
/// again at completion would be a second network round trip for a fact that
/// cannot change mid-job.
///
/// **Both failure modes default to `true`** (T-31-19): a model absent from the
/// catalog, or a catalog call that errors, yields "assume watermarked". Telling
/// a user their footage may carry an invisible provenance signal when it does
/// not is a harmless over-disclosure; the reverse is a product lie.
pub async fn resolve_provenance_flag<P: agent_gen::GenProvider>(
    provider: &P,
    model_id: &str,
) -> bool {
    match provider.list_models().await {
        Ok(models) => models
            .iter()
            .find(|m| m.id == model_id)
            .map(|m| m.carries_provenance_watermark)
            .unwrap_or(true),
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Provider slots (Phase 31 .. 43)
// ---------------------------------------------------------------------------

/// The concrete generation provider this build can submit to.
///
/// `start_generation_job` is generic over `P: GenProvider` (the trait is
/// deliberately never `dyn` — its methods are RPITIT, so `Box<dyn GenProvider>`
/// will not compile, exactly as `provider.rs:158-162` anticipated). A
/// `#[tauri::command]` is non-generic, so the managed state must name ONE
/// concrete type. That type is now [`agent_gen::ConcreteGenProvider`] — the
/// two-variant enum realizing that anticipation: `Fixture(FixtureGenProvider)`
/// (every existing test double), `Runway(RunwayProvider)` (the REAL image+video
/// provider) or `ElevenLabs(ElevenLabsProvider)` (audio). The enum's
/// `GenProvider` impl delegates each method to the active arm, so
/// `start_generation_job`'s `P: GenProvider` bound is satisfied by
/// monomorphization with zero boxing.
///
/// **A keyless production build manages `None`, on purpose.** When no Runway key
/// resolves (neither the keychain slot nor `RUNWAY_API_KEY`/`RUNWAYML_API_SECRET`),
/// pretending otherwise (wiring the fixture into a shipped build, or silently
/// succeeding) would be a fake feature. The command therefore returns an HONEST
/// error naming what is missing and how to supply it (see
/// [`NO_PROVIDER_CONFIGURED`]). A keyed build holds
/// `Some(ConcreteGenProvider::Runway(..))`; tests supply a
/// `Some(ConcreteGenProvider::Fixture(..))`.
///
/// **Phase 43 (LAT-05): the slot is resolved LAZILY.** It used to be a plain
/// `Option` filled by an eager keychain read inside
/// [`production`](Self::production), i.e. during `configure()`, before the window
/// existed. It is now empty until [`resolve_via`](Self::resolve_via) is first
/// called by a command handler that genuinely needs the provider. `Option` still
/// models honest absence — it just lives one layer in.
///
/// **Phase 69 (D-69-13): the slot is INVALIDATABLE** (a [`ProviderSlot`], no
/// longer a process-lifetime `OnceLock`): `run_set_provider_key` /
/// `run_clear_provider_key` invalidate it, so a Runway key set or cleared in
/// Settings takes effect on the next use, while construction still reads nothing.
pub struct ManagedGenProvider(pub ProviderSlot);

/// Resolve the ElevenLabs audio (TTS) provider from a credential store, or
/// honestly `None`.
///
/// The exact mirror of [`resolve_runway_provider`], registered Wave 34-02. [`ElevenLabsProvider::connect`](agent_gen::ElevenLabsProvider::connect)
/// owns ALL resolution ORDER (the store's key wins; then the `ELEVENLABS_API_KEY`
/// env fallback; NEITHER present ⇒ `None`, never a panic). This fn only lifts that
/// `Option<ElevenLabsProvider>` into the app's dispatch enum. No key is ever read,
/// logged, or interpolated here — the value never leaves `connect` except as an
/// opaque field inside the returned provider (T-34-10).
pub fn resolve_elevenlabs_provider(
    store: &dyn agent_llm::KeyStore,
) -> Option<agent_gen::ConcreteGenProvider> {
    agent_gen::ElevenLabsProvider::connect(store).map(agent_gen::ConcreteGenProvider::ElevenLabs)
}

/// Resolve the Runway provider from a credential store, or honestly `None`.
///
/// Phase 42.1 (Plan 02): the ONE resolver both the image slot and the video slot
/// now run — Runway serves both modalities from a single credential.
/// [`RunwayProvider::connect`](agent_gen::RunwayProvider::connect) owns ALL
/// resolution ORDER (the store's key wins; then `RUNWAY_API_KEY`; then Runway's
/// OWN documented `RUNWAYML_API_SECRET`; NONE present ⇒ `None`, never a panic).
/// This fn only lifts that `Option<RunwayProvider>` into the app's dispatch enum.
/// No key is ever read, logged, or interpolated here — the value never leaves
/// `connect` except as an opaque field inside the returned provider.
pub fn resolve_runway_provider(
    store: &dyn agent_llm::KeyStore,
) -> Option<agent_gen::ConcreteGenProvider> {
    agent_gen::RunwayProvider::connect(store).map(agent_gen::ConcreteGenProvider::Runway)
}

/// Phase 69 (D-69-13): an INVALIDATABLE, LAZY provider slot. `None` = unresolved (nothing
/// read yet — LAT-05's zero-reads-before-first-use is preserved); `Some(None)` = resolved,
/// honestly absent; `Some(Some(p))` = resolved. `invalidate()` returns it to unresolved so a
/// key set or cleared in Settings takes effect on the NEXT use with no restart.
///
/// Replaces the Phase-43 process-lifetime `OnceLock` (and the retired
/// once-only init helper): the laziness is kept — the resolver runs on
/// first real use, never at construction — while the once-for-the-whole-process
/// property is deliberately dropped. **This changes only WHEN the store is
/// read** (T-43-05-01 still holds): WHERE credentials live, which validator
/// guards each slot, and the rule that no key is ever logged, returned to the
/// renderer or written to a project file are untouched. A poisoned lock is
/// recovered, so a panic elsewhere cannot make providers permanently
/// unresolvable (T-69-06).
///
/// The retired per-use slot-lookup helper built a FRESH `KeyringStore` under the
/// hardcoded production service on every first use — the D-69-02 offender. The
/// slots now resolve only through the host-owned [`ManagedProviderKeyStore`]
/// (`resolve_via`), which is built under the host's validated credential
/// service, so a test service namespaces every provider (D-69-04).
pub struct ProviderSlot(
    std::sync::RwLock<Option<Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>>>,
);

impl ProviderSlot {
    /// An empty slot — constructing it reads nothing.
    pub const fn unresolved() -> Self {
        Self(std::sync::RwLock::new(None))
    }

    /// A PRE-RESOLVED slot: the resolver never runs (until an `invalidate()`).
    /// Test/fixture construction only — production must never pre-fill a
    /// credential slot.
    pub fn preset(value: Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>) -> Self {
        Self(std::sync::RwLock::new(Some(value)))
    }

    /// Whether a resolution (including an honest `None`) is currently cached.
    pub fn is_resolved(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Return the cached value, or run `resolver` (at most once per
    /// invalidation — double-checked under the write lock) and cache it.
    pub fn resolve_with(
        &self,
        resolver: impl FnOnce() -> Option<agent_gen::ConcreteGenProvider>,
    ) -> Option<std::sync::Arc<agent_gen::ConcreteGenProvider>> {
        if let Some(cached) = self.0.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            return cached.clone();
        }
        let mut guard = self.0.write().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            return cached.clone();
        }
        let resolved = resolver().map(std::sync::Arc::new);
        *guard = Some(resolved.clone());
        resolved
    }

    /// Drop the cached resolution; the next `resolve_with` re-reads.
    pub fn invalidate(&self) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl ManagedGenProvider {
    /// Production wiring (Phase 42.1): resolve **Runway** from its `"runway"`
    /// keychain slot / `RUNWAY_API_KEY` / `RUNWAYML_API_SECRET` env fallbacks.
    ///
    /// There is no `.or_else` chain any more, and that is the point. This used to
    /// be first-configured-wins across OpenAI-then-Google, whose recorded
    /// SINGLE-ACTIVE-PROVIDER limitation is what forced
    /// [`ManagedVideoGenProvider`] into existence in 33-02. **One provider now
    /// serves both modalities, so that limitation has dissolved** — there is
    /// nothing left for a fallback to disambiguate.
    ///
    /// The slot's account string is looked up from [`PROVIDER_KEY_SLOTS`] via
    /// `ManagedProviderKeyStore::slot` — the single source of truth — never retyped. Two
    /// honest facts survive the collapse:
    ///
    /// 1. **Resolution happens ONCE** — see [`resolve_via`](Self::resolve_via) for
    ///    *when*, which Phase 43 moved from startup to first use.
    /// 2. **No key ⇒ `None`, on purpose.** Absence maps to a resolved `None` →
    ///    the shared [`NO_PROVIDER_CONFIGURED`] error, NEVER a fixture or
    ///    placeholder substitute in a shipped build (31-03's honest-absence
    ///    pattern). The resolution path holds no `unwrap`/`expect` on the key —
    ///    absence is `Option`-shaped throughout.
    ///
    /// **Phase 43 (LAT-05): this constructor reads NOTHING.** It builds an empty
    /// unresolved [`ProviderSlot`], which is why `configure()` can call it (and its
    /// two siblings) before the window exists at zero credential-store cost. The
    /// real Windows Credential Manager read is deferred to the first
    /// [`resolve_via`](Self::resolve_via), made by whichever command handler actually
    /// needs the provider (D-14: "resolved on first actual use behind an
    /// on-demand accessor"). The `configure()` call site is unchanged — same
    /// name, same zero-arg signature.
    pub fn production() -> Self {
        Self(ProviderSlot::unresolved())
    }

    /// Test/fixture construction: a PRE-RESOLVED slot.
    ///
    /// Every test double goes through here rather than building the tuple
    /// directly, so `resolve_via()` on a double is free and — more importantly —
    /// structurally incapable of reaching a real [`agent_llm::KeyringStore`]
    /// (`resolve_with` short-circuits on a resolved [`ProviderSlot`], so the
    /// resolver closure never runs). This preserves the deliberate seam between
    /// the REAL store wired by `configure` and the `InMemoryKeyStore` doubles
    /// wired by `configure_with_key_store`.
    ///
    /// `pub` (not `#[cfg(test)]`) since Phase 54.1: `src-tauri`'s and
    /// `crates/ffi`'s test tiers construct fixture slots cross-crate, and a
    /// `#[cfg(test)]` item is invisible outside its OWN crate's test build.
    /// Production code must still never call this — it pre-fills a credential
    /// slot.
    pub fn preset(value: Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>) -> Self {
        Self(ProviderSlot::preset(value))
    }

    /// Resolve (lazily) and return the current provider — the on-demand
    /// accessor LAT-05 is built around.
    ///
    /// The FIRST call after construction or an [`invalidate`](Self::invalidate)
    /// reads the `"runway"` slot of `stores` (then the `RUNWAY_API_KEY` /
    /// `RUNWAYML_API_SECRET` env fallbacks, order owned by
    /// `RunwayProvider::connect`). Later calls return the cached value.
    ///
    /// Phase 69 (D-69-13): the old "a key added after launch takes effect on the
    /// next restart" caveat is gone — `run_set_provider_key` /
    /// `run_clear_provider_key` invalidate this slot, and it is still lazy.
    pub fn resolve_via(
        &self,
        stores: &ManagedProviderKeyStore,
    ) -> Option<std::sync::Arc<agent_gen::ConcreteGenProvider>> {
        self.0.resolve_with(|| {
            stores
                .slot(agent_gen::RUNWAY_PROVIDER_ID)
                .ok()
                .and_then(resolve_runway_provider)
        })
    }

    /// Drop the cached resolution (D-69-13).
    pub fn invalidate(&self) {
        self.0.invalidate();
    }
}

impl Default for ManagedGenProvider {
    fn default() -> Self {
        Self::production()
    }
}

/// The video-generation provider slot — **no longer modality-scoped in
/// substance, only in name** (Phase 42.1, Plan 02).
///
/// # Why this slot existed, and why that reason is gone
///
/// 33-02 created it as the honest answer to [`ManagedGenProvider::production`]'s
/// recorded SINGLE-ACTIVE-PROVIDER limitation: that slot was
/// first-configured-wins across OpenAI-then-Google, so a dual-key user would
/// hold OpenAI as the ONE active provider and every managed `(google, veo…)`
/// submit would be gate-rejected (the GEN-08 gate keys on the ACTIVE provider's
/// `id()`). A second, video-only slot was the narrow fix.
///
/// **Phase 42.1 dissolves that reason.** Runway serves video AND stills from ONE
/// credential, so [`ManagedGenProvider::production`] and
/// [`production_video`](Self::production_video) now resolve the SAME provider
/// from the SAME `"runway"` keychain row. The image/video split is therefore
/// **no longer load-bearing**: the two slots are structurally interchangeable
/// today, and either could hold the other's provider without changing a single
/// outcome.
///
/// Both are kept for now, deliberately and with the reason recorded here rather
/// than assumed:
///
/// - the AUDIO slot ([`ManagedAudioGenProvider`]) is genuinely a different
///   provider (ElevenLabs) and genuinely still needs its own slot, so the
///   modality-scoped SHAPE has a live inhabitant and is not dead machinery;
/// - collapsing image+video into one slot is a mechanical rename touching every
///   test that manages state, which would bury this wave's actual change
///   (deleting two providers) in churn;
/// - a future second image/video provider would need the split back.
///
/// It cannot make a paid call reachable early: with no runway key the slot is
/// `None` (the honest [`NO_VIDEO_PROVIDER_CONFIGURED`] error, never a fixture —
/// 31-03's honest-absence pattern), and the `(runway, …)` allow-list rows ship
/// only behind the human GEN-08 sign-off recorded in
/// [`agent_gen::production_allow_list`].
///
/// **Phase 43 (LAT-05) / Phase 69 (D-69-13): a lazily-resolved, invalidatable slot**, exactly like
/// [`ManagedGenProvider`] — see [`ProviderSlot`].
pub struct ManagedVideoGenProvider(pub ProviderSlot);

impl ManagedVideoGenProvider {
    /// Construct the video slot UNRESOLVED — zero keychain reads (LAT-05).
    ///
    /// The first [`resolve_via`](Self::resolve_via) reads **Runway** from its keychain
    /// slot / env fallback — the SAME row, the SAME resolver and therefore the
    /// SAME provider [`ManagedGenProvider`] resolves. No key ⇒ a resolved
    /// `None`. No `unwrap`/`expect` on the key — absence is `Option`-shaped
    /// throughout, and no key is ever read/logged here (it lives inside
    /// `resolve_runway_provider`'s returned provider).
    pub fn production_video() -> Self {
        Self(ProviderSlot::unresolved())
    }

    /// Test/fixture construction: a PRE-RESOLVED slot. See
    /// [`ManagedGenProvider::preset`] — including why Phase 54.1 dropped the
    /// `#[cfg(test)]` gate.
    pub fn preset(value: Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>) -> Self {
        Self(ProviderSlot::preset(value))
    }

    /// Resolve (lazily) and return the video provider. Same contract as
    /// [`ManagedGenProvider::resolve_via`], same `"runway"` row.
    pub fn resolve_via(
        &self,
        stores: &ManagedProviderKeyStore,
    ) -> Option<std::sync::Arc<agent_gen::ConcreteGenProvider>> {
        self.0.resolve_with(|| {
            stores
                .slot(agent_gen::RUNWAY_PROVIDER_ID)
                .ok()
                .and_then(resolve_runway_provider)
        })
    }

    /// Drop the cached resolution (D-69-13).
    pub fn invalidate(&self) {
        self.0.invalidate();
    }
}

impl Default for ManagedVideoGenProvider {
    fn default() -> Self {
        Self::production_video()
    }
}

/// Phase 34 (GEN-03): the MODALITY-SCOPED audio-generation provider slot.
///
/// The EXACT structural twin of [`ManagedVideoGenProvider`], one modality over —
/// and, since Phase 42.1, **the only one of the three slots whose scoping is
/// still load-bearing.** Image and video both resolve Runway now; audio resolves
/// a genuinely DIFFERENT provider (ElevenLabs) from a genuinely different
/// keychain row, so this slot is the one that still decides something.
///
/// It resolves the ElevenLabs provider SPECIFICALLY, from the elevenlabs keychain
/// row + `ELEVENLABS_API_KEY` env fallback ONLY (via the SAME
/// [`resolve_elevenlabs_provider`] the seam entry uses) — so image + video route
/// through Runway and audio through ElevenLabs, side by side.
///
/// **Untouched by the 42.1 provider migration (locked decision 2).** The audio
/// path — `crates/agent-gen/src/elevenlabs.rs`, this slot, its seam entry and its
/// tests — is byte-unchanged by the Runway single-provider collapse. It cannot
/// make a paid call reachable early: with no elevenlabs key the slot is `None`
/// (the honest [`NO_AUDIO_PROVIDER_CONFIGURED`] error, never a fixture — 31-03's
/// honest-absence pattern), behind its own 34-03 human GEN-08 sign-off row.
///
/// **Phase 43 (LAT-05) / Phase 69 (D-69-13): a lazily-resolved, invalidatable slot**, exactly like its two
/// siblings — see [`ProviderSlot`]. This was the third of the three
/// Credential Manager reads `configure()` used to make before the window existed.
pub struct ManagedAudioGenProvider(pub ProviderSlot);

impl ManagedAudioGenProvider {
    /// Construct the audio slot UNRESOLVED — zero keychain reads (LAT-05).
    ///
    /// The first [`resolve_via`](Self::resolve_via) reads the ElevenLabs provider from
    /// its keychain slot / env fallback — the elevenlabs row ONLY, no
    /// openai/google fallback. No key ⇒ a resolved `None`. No `unwrap`/`expect`
    /// on the key — absence is `Option`-shaped throughout, and no key is ever
    /// read/logged here (it lives inside `resolve_elevenlabs_provider`'s returned
    /// provider, T-34-10).
    pub fn production_audio() -> Self {
        Self(ProviderSlot::unresolved())
    }

    /// Test/fixture construction: a PRE-RESOLVED slot. See
    /// [`ManagedGenProvider::preset`] — including why Phase 54.1 dropped the
    /// `#[cfg(test)]` gate.
    pub fn preset(value: Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>) -> Self {
        Self(ProviderSlot::preset(value))
    }

    /// Resolve (lazily) and return the audio provider. Same contract as
    /// [`ManagedGenProvider::resolve_via`], but the `"elevenlabs"` row — the one
    /// slot whose modality scoping still decides something.
    pub fn resolve_via(
        &self,
        stores: &ManagedProviderKeyStore,
    ) -> Option<std::sync::Arc<agent_gen::ConcreteGenProvider>> {
        self.0.resolve_with(|| {
            stores
                .slot(agent_gen::ELEVENLABS_PROVIDER_ID)
                .ok()
                .and_then(resolve_elevenlabs_provider)
        })
    }

    /// Drop the cached resolution (D-69-13).
    pub fn invalidate(&self) {
        self.0.invalidate();
    }
}

impl Default for ManagedAudioGenProvider {
    fn default() -> Self {
        Self::production_audio()
    }
}

/// The GEN-08 clean-model allow list this build enforces.
///
/// Managed by `configure()` as [`agent_gen::production_allow_list`] — the two
/// vetted fixture models plus every row a human has signed off.
/// **Extending it is the GEN-08 HUMAN LEGAL GATE**, not a code change a phase can
/// make on its own: a row is added only after a person has confirmed (a) that
/// provider's live ToS permits a closed-source paid product on BYO keys AND (b)
/// that specific model's weight/output license is commercially clean. Each row's
/// clearance is recorded beside it in `agent_gen::allow_list`.
///
/// Tests inject their own list by calling the `_inner` functions directly — the
/// SC-4 fixture-rejection tests pin [`phase31_allow_list`](agent_gen::allow_list::phase31_allow_list)
/// EXPLICITLY so their "the fixture-only list rejects fixture-nonclean" meaning is
/// independent of what `Default` resolves to.
pub struct ManagedAllowList(pub agent_gen::AllowList);

impl Default for ManagedAllowList {
    fn default() -> Self {
        Self(agent_gen::production_allow_list())
    }
}

// ---------------------------------------------------------------------------
// Honest-absence messages, submit-error mapping, prompt caps
// ---------------------------------------------------------------------------

/// The single honest "no key, no provider" message, shared by
/// `submit_generation_job` and `list_generation_models` so the two cannot
/// drift into telling the user different stories about the same state.
///
/// Evergreen, actionable wording (not phase-framed): it names the missing
/// credential and the two ways to supply it (the OS-keychain slot via
/// `set_provider_key`, or the env var in `.env`/env). Phase 42.1 collapsed the
/// two-provider wording to the ONE provider that now serves image + video.
/// Asserted by equality from the tests, so the text lives in exactly this one
/// place — and it keeps the literal "Runway" substring the one contains-pin
/// asserts (every other pin tracks the const by equality).
///
/// `pub` (was private) since Phase 54.1: its two consumers — the
/// `#[tauri::command]`s `submit_generation_job` / `list_generation_models` —
/// stayed behind in `src-tauri`.
pub const NO_PROVIDER_CONFIGURED: &str = "no generation provider configured — add a Runway API key \
     in Settings (or set RUNWAY_API_KEY in the environment — developer setup); get one at \
     dev.runwayml.com";

/// The video-tool-SPECIFIC honest-absence message.
///
/// Kept distinct from [`NO_PROVIDER_CONFIGURED`] so each surface names the
/// modality the user was actually trying to use — even though, since Phase 42.1,
/// both resolve the SAME `"runway"` slot and therefore always agree about
/// whether a key exists. Equality-asserted from the tests (env-var NAMES only,
/// never a key value).
pub const NO_VIDEO_PROVIDER_CONFIGURED: &str =
    "no video generation provider configured — add a Runway API key \
     in Settings (or set RUNWAY_API_KEY in the environment — developer setup); get one at \
     dev.runwayml.com";

/// Phase 34 (GEN-03): the audio-tool-SPECIFIC honest-absence message.
///
/// Distinct from [`NO_PROVIDER_CONFIGURED`]/[`NO_VIDEO_PROVIDER_CONFIGURED`] so
/// each error stays precise: the audio tool resolves ONLY the elevenlabs slot, so
/// its absence names the ElevenLabs key exactly and points at elevenlabs.io as
/// where to get one. Equality-asserted from the tests (T-34-10: env-var NAMES
/// only — `ELEVENLABS_API_KEY` as a name, never a key value).
pub const NO_AUDIO_PROVIDER_CONFIGURED: &str =
    "no audio generation provider configured — set an ElevenLabs API key \
     (set_provider_key(\"elevenlabs\", ...) or ELEVENLABS_API_KEY in .env; get one at \
     elevenlabs.io) and restart Rudis";

/// Phase 32 (GEN-01): the `GenError` → String mapping every submit surface shares.
///
/// Extracted from `submit_generation_job_inner` so the IPC command AND the agent
/// tool (`generate_runway_image_for_agent`) surface the SAME wording — the
/// GEN-08 "clean-model allow-list" explanation lives in EXACTLY one place and the
/// two paths cannot drift into telling different stories about a rejected model.
pub fn gen_submit_error_message(e: agent_gen::GenError) -> String {
    match e {
        // GEN-08: spell out WHY. The bare `GenError` Display names the model;
        // this adds the rule it violated, so a caller (or an agent reading the
        // error) learns the model is unavailable by POLICY, not by an outage it
        // should retry.
        agent_gen::GenError::ModelNotAllowed(model) => format!(
            "model '{model}' is not on the clean-model allow-list (GEN-08): its provider \
             ToS or model-weight license has not been vetted"
        ),
        other => other.to_string(),
    }
}

/// Phase 32 (GEN-01 / T-32-21): the hard cap on an agent-authored image prompt.
/// The prompt is MODEL-authored (downstream of arbitrary user text) and reaches
/// a BILLED network call, so bounding its length bounds a single runaway paid
/// request. It reaches ONLY `GenRequest.prompt` — never a filename/endpoint/model.
pub const MAX_AGENT_IMAGE_PROMPT_CHARS: usize = 4000;

/// Phase 33 (GEN-02 / T-33-22): the hard cap on an agent-authored VIDEO prompt.
/// Same reasoning as [`MAX_AGENT_IMAGE_PROMPT_CHARS`] — the prompt is
/// MODEL-authored downstream of arbitrary user text and reaches a BILLED network
/// call, so bounding its length bounds a single runaway paid request. It reaches
/// ONLY `GenRequest.prompt` — never a filename/endpoint/model.
pub const MAX_AGENT_VIDEO_PROMPT_CHARS: usize = 4000;

/// Phase 34 (GEN-03 / T-34-13): the hard cap on an agent-authored AUDIO prompt —
/// the LITERAL text to be spoken aloud. Same reasoning as
/// [`MAX_AGENT_IMAGE_PROMPT_CHARS`]: the prompt is MODEL-authored downstream of
/// arbitrary user text and reaches a BILLED network call, so bounding its length
/// bounds a single runaway paid request. A Rudis-side sanity cap that sits UNDER
/// ElevenLabs' own ~10,000-char model limit (ASVS V5). It reaches ONLY
/// `GenRequest.prompt` — never a filename/endpoint/model/voice.
pub const MAX_AGENT_AUDIO_PROMPT_CHARS: usize = 4000;

// ---------------------------------------------------------------------------
// DELETED by Phase 55.1 plan 06: the capability-resolution layer.
//
// Eight items stood here — the still-image seam's pinned capability const, the
// video-legal capability list, the capability -> model lookup, the
// "can this capability serve the request shape?" predicate, the cheapest-capable
// default chooser, the resolver that turned an optional capability plus a shape
// into the one capability to submit, the schema's shape/stage -> capability
// translation, and the error-vocabulary rewriter that translated the resolver's
// refusals back into schema words.
//
// Plans 01-04 rewired every reader: 55.1-03 gave both tools a REQUIRED free-text
// `model` field, so the dispatch path and [`resolved_model_for_tool_input`] both
// read one `input["model"]`, and these eight had ZERO production callers left.
// Deleting them is the point rather than tidying — leaving a second, still-
// compiling way to decide which model runs is the "second source of truth"
// anti-pattern this phase exists to close (T-55.1-11).
//
// What replaced each job:
//   * choosing the image model     -> the caller's `model` field (was a const
//                                     that hardcoded `gen4_image`; D-10)
//   * choosing the video model     -> the caller's `model` field (D-02)
//   * refusing an incapable model  -> Runway's own 400 (D-05), no longer a local
//                                     $0.00 refusal — recorded, not glossed
//   * refusing a malformed call    -> `generation_bridge::model_from_input`,
//                                     which refuses a blank model and refuses the
//                                     RETIRED shape/stage/intent keys by name
//   * telling the agent what a     -> the rulebook's cost + first+last-pair table
//     model can do and costs          (55.1-05) and the spend confirmation's own
//                                     price (55.1-04)
//
// The refusals' pre-spend needles moved with them: 55.1-03 removed the six
// Phase-42.3 (G) entries from `PRE_SPEND_VALIDATION_SUBSTRINGS` and added the
// two the bridge now raises, so no needle points at a message nothing can emit.
// ---------------------------------------------------------------------------

/// The model a `generate_ai_*` tool call actually ran, resolved from the tool's
/// OWN input — what [`GenerationDisclosure::model_resolved`](crate::GenerationDisclosure)
/// reports to the user.
///
/// **Phase 55.1 (D-10):** it reads `input["model"]` — the SAME key the dispatch
/// path bills — for BOTH tools. That identity is the whole anti-drift property
/// (T-42.3-11 / T-55.1-07): the disclosure cannot name one model while another
/// was charged for, because there is only one string and both readers take it
/// from the same place. It replaces the pair of intent resolvers this used to
/// re-run, and the `generate_ai_image` arm that ignored its input entirely and
/// always reported `gen4_image`.
///
/// An ON-ROSTER id renders label-decorated — the user is reading this, and
/// "Google Veo 3.1 Fast (via Runway) (veo3.1_fast)" says more than
/// "veo3.1_fast". An OFF-ROSTER id renders as the BARE id: with the roster open,
/// a model Rudis has never catalogued is a normal outcome, and inventing a
/// friendly name for one would be a fabricated claim about what ran.
///
/// `None` for a tool with no model concept, and for an input naming no model at
/// all — the seam refuses such a call, so the disclosure renders nothing rather
/// than guessing at a generation that cannot happen.
///
/// # Phase 56 (plan 05): the clip-edit arm, and the CONTRACT it places on plan 06
///
/// `generate_ai_video_edit` is added here rather than in the plan that ships the
/// tool, because this resolution is PER-TOOL: a tool absent from this match
/// discloses **nothing at all**, and the edit path is the one path where the
/// user's OWN footage leaves the machine. Shipping it with a silent disclosure
/// axis is the failure T-56-D18-01 names, and it would be invisible in review.
///
/// Its ONE difference from the two arms above is the fallback: a call naming no
/// model resolves to [`agent_gen::advisory_video_edit_model`] rather than to
/// `None`, because the advisory default exists precisely to be what runs when a
/// tool call names nothing. **That is a contract on plan 06, stated here so it
/// cannot be discovered later:** if plan 06 instead makes `model` REQUIRED (as
/// both sibling tools are since 55.1-03), this fallback must be deleted in the
/// same commit — a disclosure naming a model that no call submitted is exactly
/// the fabricated claim 55.1-03 removed when it deleted the `generate_ai_image`
/// arm that always reported `gen4_image` regardless of its input.
///
/// The default is DERIVED from the roster, never retyped, so the disclosure
/// cannot name one model while the submission carries another.
pub fn resolved_model_for_tool_input(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    // `Some(..)` only for the arm that HAS a default to fall back to; the two
    // pre-56 tools keep their exact behaviour (no model named => no disclosure).
    let advisory_default = match tool_name {
        "generate_ai_image" | "generate_ai_video" => None,
        "generate_ai_video_edit" => agent_gen::advisory_video_edit_model(),
        _ => return None,
    };
    let model = input
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(advisory_default)?;
    Some(
        agent_gen::model_label(model)
            .map(|label| format!("{label} ({model})"))
            .unwrap_or_else(|| model.to_string()),
    )
}

// (The Phase-42.3 (G) error-vocabulary rewriter stood here; it is one of the
// eight items the block above records as deleted. Its job — never steer a
// retrying model toward a field the schema no longer offers — survives as
// behaviour in `generation_bridge::model_from_input`, whose refusal for a
// retired key NAMES the `model` field that replaced it.)

// ---------------------------------------------------------------------------
// The ONE pre-spend confirmation policy (Phase 42.3 item F, Phase 54.1 SC-5)
// ---------------------------------------------------------------------------

/// Phase 42.3 (F): the ONE pre-spend confirmation policy for every surface that
/// can reach `start_generation_job`.
///
/// Two surfaces exist and they converge here:
///
/// 1. the agent's `generate_ai_image` / `generate_ai_video` / `generate_ai_audio`
///    tool dispatch — wired in the agent turn's `apply_round` meta loop; and
/// 2. the host-initiated direct-submit surface. ROADMAP 42.3's note names the
///    two as one concern on two surfaces, and a second parallel policy is
///    exactly how a "confirmed" call on one surface becomes an unconfirmed one
///    on the other, which is why **this is the named extension point**:
///    ROADMAP **Backlog § 999.8 / WR-04** asked for "the cap dimension [added]
///    to the signature/return here — do not fork".
///
/// **Phase 55.1 (D-01/D-08/D-12) added that dimension: `resolved_cost`.** With
/// the Runway roster open (55.1-02/03) any model id can be submitted, including
/// ids that do not exist yet at prices nobody has seen, so the one thing the
/// user must still consent to knowingly is the PRICE. The question therefore
/// names either the advisory roster's own line for that model
/// ([`cost_signal_for_model`]) or the literal [`PRICE_UNKNOWN`] — never a
/// fabricated, rounded or interpolated number, because a number the user can
/// repeat as fact must be one Rudis actually knows. There is no hard local cap
/// (D-01 accepts that); the mitigation is informed consent, stated plainly.
///
/// **The direct-submit half of § 999.8 is closed too** — see
/// [`submit_generation_job_inner`], whose FIRST act is this function. The
/// `#[tauri::command] submit_generation_job` the backlog entry was written
/// against died with Phase 55's cutover, so its host-agnostic replacement was
/// created already gated rather than gated afterwards: there is no ungated
/// direct-submit body left anywhere for a future host to wrap.
///
/// **Phase 54.1 (SC-5) made that structural rather than advisory.** This
/// function used to live in `src-tauri/src/generation.rs` behind a `src-tauri`-
/// local `SpendGateDecision` mirror enum that `TauriAppCtx`'s [`crate::SpendPolicy`]
/// impl re-tagged onto [`crate::SpendGateDecision`]. The mirror enum is DELETED
/// and this is now the only definition in the workspace: ONE gate, ONE enum,
/// every host calling the same body.
///
/// The two surfaces deliberately do NOT share UX, and that asymmetry is expected:
/// the agent path has no synchronous dialog it can pop in the middle of a tool
/// call (it halts the turn and asks in Chat instead), while the IPC path is
/// already a direct, synchronous user action. They share the POLICY, not the
/// prompt.
///
/// `approved` is the caller's evidence of user consent: the agent surface passes
/// its per-turn one-shot ([`crate::AgentSession::spend_approved_turn`], set ONLY
/// by the resume of a spend-confirmation `PendingAskUser` — no tool input,
/// rulebook text or model output can set it, T-42.3-12); the user surface passes
/// `true` for a direct user-initiated submit (the click IS the consent — Backlog
/// § 999.8 adds the cap dimension on top). Fail-closed: no evidence, no spend.
pub fn spend_confirmation_gate(
    tool_name: &str,
    resolved_model: Option<&str>,
    // Phase 55.1 (D-01/D-12): `Some("~$0.20 / 4s clip (5 credits/s)")` for a
    // model the advisory roster prices, `Some(PRICE_UNKNOWN)` for one it cannot,
    // `None` for a spend with no model-cost concept at all (audio).
    resolved_cost: Option<&str>,
    approved: bool,
) -> crate::SpendGateDecision {
    if approved {
        return crate::SpendGateDecision::Proceed;
    }
    // T-42.3-13 (repudiation): name the model that will actually run, so the
    // thing the user approves is the thing they are billed for. `None` for a
    // modality with no model choice to report (audio) — the tool name still
    // identifies the spend.
    //
    // T-55.1-08: and name its PRICE beside it. A cost with no model to attach it
    // to is dropped rather than floated on its own — an unattached price would
    // be a claim about a spend the question cannot otherwise describe.
    let model_line = match (resolved_model, resolved_cost) {
        (Some(m), Some(c)) => format!(" (model: {m}, {c})"),
        (Some(m), None) => format!(" (model: {m})"),
        (None, _) => String::new(),
    };
    crate::SpendGateDecision::NeedsConfirmation {
        question: format!(
            "{tool_name} is a PAID call to an external provider{model_line}, billed to \
             your own account. Reply to confirm before I spend -- say yes to proceed, \
             or tell me what to change."
        ),
    }
}

/// Phase 55.1 (D-01): what the confirmation shows for a model the advisory
/// table cannot price. **The literal words, never a fabricated number.**
///
/// D-01's own wording — *"off-roster ids submit anyway and the confirmation says
/// the price is unknown"* — is a capability, not a formality: with an open
/// roster this is the honest disclosure for every model id Rudis has never
/// catalogued, which after this phase includes every model Runway ships next.
/// A guessed or "typical" figure would be worse than the gap, because the user
/// (or the agent quoting the question back to them) could repeat it as fact.
pub const PRICE_UNKNOWN: &str = "price unknown";

/// The advisory price line for a model id — **`RUNWAY_MODELS`' only surviving
/// job** (D-01).
///
/// 55.1-01 stopped `build_submission` consulting the table for endpoint
/// selection and 55.1-02 stopped the allow list consulting it for clearance, so
/// feeding this one string to the spend prompt is the whole of what the roster
/// still does. It gates nothing: an id with no row is priced [`PRICE_UNKNOWN`]
/// and submitted anyway.
pub fn cost_signal_for_model(model_id: &str) -> &'static str {
    agent_gen::model_caps(model_id)
        .and_then(|c| c.rough_cost_signal)
        .unwrap_or(PRICE_UNKNOWN)
}

/// The `resolved_cost` argument for [`spend_confirmation_gate`], read from a
/// tool call's OWN input.
///
/// `Some` for exactly the two Runway tools — a missing, blank or unknown model
/// is [`PRICE_UNKNOWN`], never silence, because "we could not price this" is
/// itself the disclosure. `None` for a tool with no model-cost concept, which
/// leaves `generate_ai_audio`'s question byte-identical to its pre-55.1 text.
///
/// **It reads the same `input["model"]` [`resolved_model_for_tool_input`] reads
/// and the dispatch bills** (T-55.1-09). One key, three readers: what the user
/// approves, what is priced, and what is charged cannot drift apart, because
/// there is only one string.
pub fn cost_signal_for_tool_input(
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<&'static str> {
    matches!(tool_name, "generate_ai_image" | "generate_ai_video").then(|| {
        input
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(cost_signal_for_model)
            .unwrap_or(PRICE_UNKNOWN)
    })
}

// ---------------------------------------------------------------------------
// The CLIP-EDIT path's per-input-second price (Phase 56 plan 05, GEN-11)
// ---------------------------------------------------------------------------

/// Runway's published per-second rate for the advisory `video_to_video` model,
/// in **US cents of input**.
///
/// `docs.dev.runwayml.com/guides/pricing` publishes `28` credits per second of
/// INPUT and states — in prose and in the page's own JS — that 1 credit is
/// $0.01. Re-confirmed 2026-08-01 and recorded in **56-01**-PROBE-RESULTS.md
/// § "Pricing facts", which is also where the `$0.56`-vs-28-credits/s
/// contradiction was adjudicated: `56` was the page's `minimumCredits` figure
/// (see [`RUNWAY_ALEPH2_MINIMUM_CENTS`]) transplanted into a cents-per-4s field,
/// i.e. exactly half the real price of 4 seconds. Corrected in the roster ahead
/// of this plan by quick `260801-r7s`; this const is the per-second basis that
/// correction implies, and `aleph2_cost_figures_cannot_drift_apart` pins the two
/// together so neither can be edited alone.
pub const RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND: u32 = 28;

/// Runway's minimum charge for one `video_to_video` call, in **US cents**.
///
/// The pricing page's `minimumCredits: { 'aleph2': 56 }` — 56 credits at $0.01.
/// It is exactly [`RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND`] x
/// `agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS`, i.e. the 2-second input floor
/// billed at the published rate, which is the independent corroboration 56-03
/// cited when it labelled that window const "documented, not live-verified".
///
/// A shorter range does not cost less. Rudis refuses sub-minimum ranges locally
/// at $0.00 (`crate::clip_edit_window_check`), so this floor should never be the
/// quoted number in practice — it is here so that if one ever slips through, the
/// estimate is too HIGH rather than too low.
pub const RUNWAY_ALEPH2_MINIMUM_CENTS: u32 = 56;

/// The estimated cost of ONE clip-edit call, in US cents — or `None` when Rudis
/// cannot price it.
///
/// **`None` means the price is UNKNOWN, and plan 07's confirmation must say so
/// IN WORDS** (the literal [`PRICE_UNKNOWN`], the same wording 55.1-04 already
/// ships for an off-roster id) — **never a substituted guess.** With the roster
/// open (55.1 D-01) an unpriceable id is the ordinary case, not the exotic one:
/// every model Runway ships next lands here, and a user who can repeat a number
/// as fact must have been given one Rudis actually knows.
///
/// Priced only for [`agent_gen::advisory_video_edit_model`] — compared by
/// DERIVATION, never against a re-typed id — because the per-second rate is that
/// model's published rate and nothing licenses applying it to another. Note that
/// an on-roster model with a perfectly good 4-second clip price (`gen4_turbo`,
/// say) is still `None` HERE: it has no per-INPUT-second basis, and quoting its
/// flat figure for a 30-second edit is precisely the under-quote this function
/// exists to prevent.
///
/// `visible_us` is the clip's VISIBLE (trim-respecting) length — what
/// `crate::extract_clip_range_mp4` will actually send, per D-02 — so the
/// estimate meters the same range the vendor will bill for.
///
/// Arithmetic: `max(ceil(seconds) x RATE, MINIMUM)`. A started second is a
/// billed second, so partial seconds round UP; rounding down would quote less
/// than the bill. Non-positive input clamps to the minimum rather than to zero,
/// because a free edit is not a thing that exists and a $0.00 quote in front of
/// a paid call is the worst possible direction to be wrong in.
///
/// **Pure: no I/O, no network, no clock.** Safe to call inside a gate.
/// The ONE normalization of "which model will this clip edit actually submit?"
/// — Phase 56 plan 07, and the reason the confirmation and the bill cannot name
/// different models.
///
/// Three readers need this answer and they must not each compute it:
///
/// 1. [`resolved_model_for_tool_input`]'s `generate_ai_video_edit` arm — what
///    the "Model that ran" disclosure reports (56-05);
/// 2. the spend gate's call site in [`crate::agent_turn`] — what the
///    confirmation NAMES and, through [`video_edit_cost_signal`], what it
///    PRICES;
/// 3. [`generate_runway_video_edit_for_agent`] — what is actually submitted.
///
/// A BLANK id normalizes to the advisory default rather than to a refusal,
/// matching 56-06's seam exactly: on this tool `model` is OPTIONAL, so blank
/// means UNNAMED rather than malformed (T-42.3-11 — one string, one meaning at
/// every reader).
///
/// `None` only if the advisory roster loses its `video_to_video` row entirely,
/// which would also make [`estimated_video_edit_cost_cents`] meaningless.
pub fn resolved_video_edit_model_id(named: Option<&str>) -> Option<String> {
    named
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| agent_gen::advisory_video_edit_model().map(str::to_string))
}

/// **D-56-05-01, closed.** The `resolved_cost` line for ONE clip-edit
/// confirmation: a real dollar figure for the range the user actually chose, or
/// the literal [`PRICE_UNKNOWN`] — **never neither.**
///
/// # The bug this exists to remove
///
/// [`cost_signal_for_tool_input`] is a per-tool `match` returning
/// `Option<&'static str>`, and it has no clip-edit arm. It cannot have one:
/// this tool's price is not a property of the model, it is a property of the
/// model AND the range, and a `&'static str` cannot carry a number computed at
/// the call site. So the edit tool would have reached
/// [`spend_confirmation_gate`] as `(Some(model), None)` — the branch that
/// renders `" (model: X)"` with **no price and no "price unknown"**. That is
/// silence where the user needed honest absence, on a path that reaches **$8.40
/// in one call** (30 s of input at the published per-second rate) — an order of
/// magnitude above the familiar draft tier, and 56-05 filed it as the sharp
/// thing plan 07 owed.
///
/// # Why the gate's signature was NOT changed to take cents
///
/// 55.1-04 already gave the gate its cost dimension, and it is `Option<&str>` —
/// a slot that composes with an owned per-call string exactly as well as with a
/// roster constant. Adding a second, numeric cost parameter would give one gate
/// TWO cost inputs that could disagree about the same call, and would touch
/// every existing call site for no behavioural gain. The one policy function
/// stays one policy function, unforked and byte-unchanged, and the § 999.8 rule
/// is honoured by EXTENDING what feeds its existing dimension.
///
/// # `PRICE_UNKNOWN` is reused, never re-worded
///
/// 55.1-04 shipped that literal for the off-roster case and asserts its question
/// contains no `$` at all. A second wording for the same fact would be two
/// vocabularies for "we cannot price this", and the user would have no way to
/// know they mean the same thing.
///
/// Unknown in three genuinely different ways, all rendered identically because
/// they are the same fact to the user: the roster cannot price the model
/// (off-roster, or on-roster-but-not-per-second — see
/// [`estimated_video_edit_cost_cents`]), the roster has no v2v model at all, or
/// the clip id names nothing on the timeline so there is no range to meter. The
/// third case is followed moments later by the handler's own clean `Err`.
///
/// Pure: no I/O, no store, no clock.
pub fn video_edit_cost_signal(model_id: Option<&str>, visible_us: Option<i64>) -> String {
    match model_id
        .zip(visible_us)
        .and_then(|(model, us)| estimated_video_edit_cost_cents(model, us).map(|cents| (cents, us)))
    {
        Some((cents, us)) => format!(
            "~${}.{:02} for this clip ({:.1}s of input at the per-second rate)",
            cents / 100,
            cents % 100,
            us as f64 / 1_000_000.0
        ),
        None => PRICE_UNKNOWN.to_string(),
    }
}

pub fn estimated_video_edit_cost_cents(model_id: &str, visible_us: i64) -> Option<u32> {
    if Some(model_id) != agent_gen::advisory_video_edit_model() {
        return None;
    }
    // u64 throughout, then saturate: the window check bounds real inputs to 30s,
    // but this is a pub fn over an i64 and a wrapped multiply at a MONEY surface
    // would be a silent under-quote of the worst kind.
    let seconds = (visible_us.max(0) as u64).div_ceil(1_000_000);
    let metered = seconds.saturating_mul(u64::from(RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND));
    Some(
        metered
            .max(u64::from(RUNWAY_ALEPH2_MINIMUM_CENTS))
            .min(u64::from(u32::MAX)) as u32,
    )
}

/// The clip-edit path's reference-count refusal — **the ONE site**, pure, local
/// and **$0.00** (Phase 56 plan 06, GEN-11 / D-08 / T-56-SPEND-06).
///
/// Deliberately shaped like [`crate::clip_edit_window_check`]: a pure `Result`
/// over one number, called by whoever is about to do the work it guards, so
/// there is exactly ONE message and exactly ONE threshold. 56-04 recorded why
/// that matters — two refusal sites become two messages and eventually two
/// thresholds — and the reason it is a free function rather than an inline
/// check is that TWO layers need it at different moments: the FFI host calls it
/// FIRST (before the store lock, so a bad count costs neither a decode nor a
/// raster), and [`generate_runway_video_edit_for_agent`] calls it again as the
/// backstop for any other `GenSubmission` host.
///
/// # It refuses ANY non-empty set today, and that is the honest state
///
/// Two different refusals, because they are two different facts:
///
/// | count | refusal | why |
/// |---|---|---|
/// | `> RUNWAY_V2V_MAX_REFERENCES` | names the cap and the count | a request-SHAPE bound — and, since 56-F1b, a **MEASURED** one: `{"code":"too_big","maximum":5,"inclusive":true}` |
/// | `1 ..= RUNWAY_V2V_MAX_REFERENCES` | names the unresolved field and **56-09** | there is no probe-confirmed key to serialize into |
///
/// **The reference field is still uncrowned, and 56-F1b is why that is now a
/// PAID question rather than a free one.** 56-01 killed `references` outright;
/// 56-F1 narrowed six documented spellings to two; 56-F1b (2026-08-09, 11
/// requests, 11 x HTTP 400, $0.00) then enumerated BOTH completely — a
/// `keyframes` item is `{uri, seconds}`|`{uri, at}`, a `promptImage` item is
/// `{uri, position}`, and **both fields cap at exactly 5 image-typed items**.
/// Closing the shape and the ceiling is precisely what destroyed the
/// discriminator: neither field can be excluded, so "which one does Aleph 2.0
/// actually consult" is a question about BEHAVIOUR, and a validation error
/// proves a schema rather than a behaviour. **56-09**, the owner-gated paid run,
/// is the only remaining route.
///
/// # Why refuse rather than drop
///
/// `RUNWAY_REFERENCE_CITATION`'s doc in `agent-gen` records 42.1-04 catching the
/// alternative live: an attached-but-uncited reference came back **HTTP 200**
/// with a plausible, unconditioned result that no caller could detect. A
/// wrongly-named field on a strict validator 400s loudly at $0.00; a
/// right-named-by-luck one on the wrong endpoint is the 200 case. Refusing is
/// the only option that cannot lie about what was applied — so **GEN-11's
/// "up to 5 reference images, the Canvas-annotated preview frame first among
/// them" clause is UNMET, and stated as unmet.**
///
/// `agent_gen::build_video_edit_submission` carries its own equivalent refusal
/// and keeps it: `agent-gen` cannot see `app-core` (the dependency edge runs the
/// other way), so a direct provider caller needs its own. The two messages name
/// the same blocker on purpose.
pub fn video_edit_reference_check(count: usize) -> Result<(), String> {
    let cap = agent_gen::RUNWAY_V2V_MAX_REFERENCES;
    if count > cap {
        return Err(format!(
            "a clip edit accepts at most {cap} reference images; {count} were supplied. \
             That ceiling is the endpoint's own, measured by probe F-1b \
             (\"maximum\": 5, inclusive) rather than merely documented."
        ));
    }
    if count > 0 {
        return Err(format!(
            "reference images cannot be sent with a clip edit yet, so this call was \
             refused rather than silently sent without them ({count} supplied). The \
             endpoint has no probe-confirmed reference field: 56-01 proved \
             `references` does not exist, and probe F-1b enumerated the two \
             surviving candidates -- `keyframes` ({{uri, seconds}}|{{uri, at}}) and \
             `promptImage` ({{uri, position}}) -- finding BOTH are {cap}-item arrays \
             of image references, which is exactly why neither could be crowned. \
             Which one the model actually consults is a question about BEHAVIOUR, \
             not about the schema, so no further free probe can settle it: 56-09, \
             the owner-approved paid run, is the one that would. Edit the clip \
             without references, or describe the look you want in the prompt."
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Completion → MediaBin asset-landing bridge (Phase 31 Wave 3, GEN-05)
// ---------------------------------------------------------------------------
//
// Phase 54.1 (plan 54.1-01, Task 2): moved here and converted from
// `<R: Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` to
// `<C: AppCtx>(ctx: &C, ..)`. Each of the old host calls had a 1:1 `AppCtx`
// equivalent that both hosts already implement identically, so the conversion
// is mechanical and the landed bytes, the probe, the poster location, the undo
// bracket and the emitted patch are all unchanged:
//
//   app.path().app_data_dir()          -> ctx.app_data_dir()
//   crate::TauriAppCtx::new(app,store) -> (deleted -- the ctx IS the parameter)
//   crate::poster_cache_dir(&ctx)      -> crate::import::poster_cache_dir(ctx)
//   crate::{ID_SEQ,next_id,map_kind}   -> crate::import::{..} (the SAME items;
//                                         `src-tauri` re-exports app-core's)
//   store.lock()                       -> ctx.store().lock()
//   crate::emit_changed(app, ..)       -> ctx.emit_patch(..)
//
// The two Tauri-shaped wrappers (`land_ready_assets_from_app`, which resolves
// the store out of Tauri managed state, and `land_ready_assets_blocking`, which
// spawns on Tauri's runtime) stayed in `src-tauri` and call
// [`land_ready_assets`] below.

/// DoS cap on UNTRUSTED provider bytes, checked BEFORE any disk write
/// (T-31-11). 256 MiB comfortably exceeds any single generated image or short
/// clip while bounding what a hostile/broken provider can force onto disk in
/// one completion. Mirrors the `MAX_IMAGE_SEQUENCE_FRAMES` cap precedent: a
/// cheap arithmetic rejection before the expensive resource is touched.
pub const MAX_GEN_ASSET_BYTES: usize = 256 * 1024 * 1024;

/// The ONLY extensions a provider may land under (T-31-12).
///
/// `AssetRef::suggested_ext` is provider-supplied UNTRUSTED input that enters a
/// filename, so it is matched EXACTLY against this list — not sanitized, not
/// escaped. A token containing a dot, a separator, an uppercase letter or
/// anything else simply is not in the list and is rejected. Combined with the
/// server-built stem (`gen-{registered provider id}-{millis}-{seq}`), NO
/// free-text from a prompt or model id can ever reach the path (T-24-10, the
/// property `run_generate_image` established for the agent's generate_image).
pub const ALLOWED_GEN_EXTS: &[&str] = &["png", "jpg", "jpeg", "mp4", "mov", "webm", "wav", "mp3"];

/// Validate a provider-supplied extension against [`ALLOWED_GEN_EXTS`].
///
/// The error names the RULE and the offending token (an extension, never key
/// material or bytes).
pub fn validated_ext(suggested: &str) -> Result<&'static str, String> {
    ALLOWED_GEN_EXTS
        .iter()
        .copied()
        .find(|allowed| *allowed == suggested)
        .ok_or_else(|| {
            format!(
                "generated asset extension '{suggested}' is not allowed \
                 (permitted: {})",
                ALLOWED_GEN_EXTS.join(", ")
            )
        })
}

/// Land ONE provider-returned asset into the MediaBin as a real, probe-measured
/// [`rudis_core::MediaBinItem`].
///
/// # This is a structural copy of `run_generate_image`
///
/// Deliberately so — that pipeline already handles every edge case this one
/// needs, and reinventing it would risk silently dropping one:
///
/// | Step | What it does |
/// |---|---|
/// | validate | byte cap + extension allow-list, BEFORE any disk write |
/// | 1 | confined `app_data_dir()/generated` + SERVER-BUILT filename |
/// | 2 | write the bytes (a plain `fs::write` — provider bytes are already encoded media, not a raw RGBA frame needing engine encoding) |
/// | 3 | `engine::probe` the JUST-WRITTEN file; on `Err`, remove the orphan (IN-01) |
/// | 4 | `next_id("media")` |
/// | 5 | poster into the ASSET-PROTOCOL-ALLOWLISTED poster cache dir, failure downgrades to `None` (MEDIABIN-STILL-THUMBNAIL) |
/// | 6 | a `MediaBinItem` whose every field is MEASURED from the probe |
/// | 7 | undoable `Command::AddMediaBinItem` dispatch, remove file+poster on `Err` (IN-01), then the `project:changed` emit |
///
/// **The one deliberate difference:** `media_kind` comes from the probe
/// (`Video` for an mp4, `Image` for a png) rather than being hardcoded `Image`
/// — this bridge lands both modalities, `generate_image` only ever made stills.
pub fn land_generated_asset<C: crate::AppCtx>(
    ctx: &C,
    provider_id: &str,
    asset: &agent_gen::AssetRef,
) -> Result<rudis_core::MediaBinItem, String> {
    use std::sync::atomic::Ordering;

    // T-31-11 / T-31-12: reject untrusted input BEFORE touching the disk.
    if asset.bytes.is_empty() {
        return Err("the provider returned an empty asset".to_string());
    }
    if asset.bytes.len() > MAX_GEN_ASSET_BYTES {
        return Err(format!(
            "the provider returned {} bytes, over the {MAX_GEN_ASSET_BYTES}-byte limit",
            asset.bytes.len()
        ));
    }
    let ext = validated_ext(&asset.suggested_ext)?;

    // 1. Confined dir + SERVER-BUILT filename. Only three values interpolate:
    //    the REGISTERED provider id (a fixed compile-time string, never request
    //    text), epoch millis, and the process-wide id sequence. No prompt, no
    //    model id, no provider free-text (T-24-10).
    let dir = ctx.app_data_dir()?.join("generated");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create generated dir: {e}"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Phase 54.1: `crate::import::ID_SEQ` is the SAME static this read before
    // the move — `src-tauri`'s `crate::ID_SEQ` has been a re-export of it since
    // plan 45-07. ONE process-wide sequence, not a second counter.
    let seq = crate::import::ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let out_path = dir.join(format!("gen-{provider_id}-{millis}-{seq}.{ext}"));

    // 2. Write the REAL provider bytes.
    std::fs::write(&out_path, &asset.bytes)
        .map_err(|e| format!("write generated asset {}: {e}", out_path.display()))?;

    // 3. Probe the JUST-WRITTEN file — every MediaBinItem field is MEASURED,
    //    never guessed. IN-01: an unprobeable payload leaves no orphan behind.
    let info = match engine::probe(&out_path) {
        Ok(info) => info,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(format!("probe generated asset: {e}"));
        }
    };
    // 4.
    let id = crate::import::next_id("media");

    // 5. MEDIABIN-STILL-THUMBNAIL: the poster goes to the asset-protocol-
    //    allowlisted poster cache dir ($APPCACHE/posters/**), NOT next to the
    //    asset in the un-scoped `generated/` dir where convertFileSrc URLs are
    //    blocked. A dir/poster failure downgrades to None, never aborts.
    let poster_path = {
        // Phase 45 (45-07) moved `poster_cache_dir` to `app_core::import` and
        // gave it an `AppCtx`; Phase 54.1 deleted the locally-built ctx that
        // used to satisfy it, because the ctx is now this function's own
        // parameter. Same directory, same failure downgrade.
        let written = crate::import::poster_cache_dir(ctx).and_then(|poster_dir| {
            let poster_out = poster_dir.join(format!("{id}.png"));
            engine::generate_poster(&out_path, &poster_out, 0.0)
                .map(|()| poster_out)
                .map_err(|e| e.to_string())
        });
        match written {
            Ok(poster_out) => Some(poster_out.to_string_lossy().into_owned()),
            Err(e) => {
                // Paths only — never bytes, never key material (T-31-01).
                eprintln!(
                    "land_generated_asset: no poster for {}: {e}",
                    out_path.display()
                );
                None
            }
        }
    };

    // 6. A real, probed item. A generated asset is never rotated.
    let item = rudis_core::MediaBinItem {
        id,
        path: out_path.to_string_lossy().into_owned(),
        media_kind: crate::import::map_kind(info.media_kind),
        duration_us: info.duration_us,
        width: info.width,
        height: info.height,
        fps: info.avg_frame_rate,
        is_vfr: info.is_vfr,
        rotation_degrees: 0,
        has_audio: info.has_audio,
        poster_path,
        folder: String::new(),
        display_name: None,
        is_image_sequence: false,
        // Phase 60 (OCCL-01): measured from the landed file's own probe, via
        // the one narrowing every import surface shares.
        reports_alpha: crate::import::probed_alpha(&info),
    };

    // 7. Backend-owned + undoable: the SAME dispatch path every import uses, so
    //    the asset joins whatever turn is open (see `land_ready_assets`) and
    //    emits project:changed. IN-01: on dispatch failure the just-written
    //    file + poster are orphaned (nothing references them), so remove both
    //    before propagating.
    let dispatched = ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())
        .and_then(|mut g| {
            g.dispatch(rudis_core::Command::AddMediaBinItem(item.clone()))
                .map_err(|e| e.to_string())
        });
    let (patch, base_seq, seq) = match dispatched {
        Ok(dispatched) => dispatched,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            if let Some(pp) = &item.poster_path {
                let _ = std::fs::remove_file(pp);
            }
            return Err(e);
        }
    };
    ctx.emit_patch(&patch, base_seq, seq)?;
    Ok(item)
}

/// Land EVERY asset of one job completion as **exactly ONE undo entry**.
///
/// One JOB COMPLETION = one undo entry, even when a `Ready` carries several
/// assets. That is the direct analog of `run_agent_turn`'s turn bracket (proven
/// by `place_overlay_one_turn_one_undo_composites_on_export`): `begin_turn()`
/// groups every subsequent `dispatch()` inverse so a single `undo()` reverts the
/// whole group.
///
/// Lock discipline mirrors that precedent — SHORT `store.lock()` scopes around
/// `begin_turn`/`end_turn`, with the (blocking, ffprobe-spawning) per-asset work
/// happening between them, never while holding the lock across I/O.
///
/// **Mid-loop failure still closes the turn** (T-31-15): whatever landed before
/// the failure stays one coherent undo entry rather than leaking an open turn
/// that would silently swallow the user's NEXT edit into this generation's undo
/// group.
pub fn land_ready_assets<C: crate::AppCtx>(
    ctx: &C,
    provider_id: &str,
    assets: &[agent_gen::AssetRef],
) -> Result<Vec<rudis_core::MediaBinItem>, String> {
    {
        ctx.store()
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?
            .begin_turn();
    }

    let mut landed = Vec::with_capacity(assets.len());
    let mut failure = None;
    for asset in assets {
        match land_generated_asset(ctx, provider_id, asset) {
            Ok(item) => landed.push(item),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    {
        // Closed on EVERY path, including the failure one.
        if let Ok(mut guard) = ctx.store().lock() {
            guard.end_turn();
        }
    }

    match failure {
        Some(e) => Err(e),
        None => Ok(landed),
    }
}

// ---------------------------------------------------------------------------
// The async job lifecycle (Phase 31 Wave 2, GEN-05 — REDESIGNED by plan 54.1-03)
// ---------------------------------------------------------------------------
//
// # What changed, and why it is a simplification for BOTH hosts
//
// Until plan 54.1-03 the three `generate_*_for_agent` seams learned a
// background poll task's outcome by reading their OWN emitted events back off
// Tauri's event bus — a subscribe-before-submit, rescan-the-captured-payloads-
// after-the-`JoinHandle`-resolves, unsubscribe-on-every-exit-path apparatus with
// two dedicated payload-scanning helpers. That is an in-process RPC channel
// wearing an event bus's clothes, and it has no Tauri-free equivalent — the C
// ABI host's `EventRing` is a bounded ring the C# side POLLS, not something a
// Rust caller can synchronously subscribe to and drain inside its own call
// (54.1-RESEARCH Pitfall 1).
//
// The deleted identifiers are deliberately not spelled anywhere in this crate:
// the SC-5 guard grepped for them and had to be able to report a literal ZERO.
// (That guard, `src-tauri/tests/single_generation_path.rs`, was deleted with
// that shell at Phase 55 / GATE-07 and has no successor — keep the discipline
// anyway.) `git log -S` on `src-tauri/src/generation.rs` recovers the exact
// prior code.
//
// The redesign, which deletes that machinery for both hosts:
//
// * [`poll_job_until_terminal`] IS the loop body, takes no ctx, lands nothing,
//   and RETURNS its terminal [`agent_gen::JobStatus`]. The `Ready` arm returns
//   the UNLANDED assets.
// * [`finalize_ready_job`] is the ONE landing + terminal-emit path, run by
//   whoever awaits the task (the agent seams directly; a small continuation for
//   the fire-and-forget `submit_generation_job` IPC command).
// * Job lifecycle events flow through [`crate::AppCtx::gen_event_sink`], the
//   host-agnostic emitter, for UI liveness ONLY — never read back.
//
// The WIRE SHAPE is frozen: [`GenJobEventPayload`] / [`GenProgressEventPayload`]
// under the [`GEN_JOB_EVENT`] / [`GEN_PROGRESS_EVENT`] names, byte-identical to
// what the Tauri renderer and the C# shell already parse.

/// An OWNED, `Send + Sync + 'static` job-lifecycle event emitter: the event
/// NAME (always [`GEN_JOB_EVENT`] or [`GEN_PROGRESS_EVENT`]) plus the
/// already-serialized payload.
///
/// `Arc<dyn Fn>` rather than `Box<dyn Fn>` (which is what
/// [`engine::ProgressFn`] is) because two owners need one: the spawned poll
/// task keeps a clone for its whole life, and the awaiting caller's
/// [`finalize_ready_job`] needs one at the same time.
///
/// `serde_json::Value` rather than a generic `impl Serialize` because the sink
/// is a trait-object return: `TauriAppCtx` hands it to `app.emit`, `FfiAppCtx`
/// pushes it into the reserved `EVENT_GEN_JOB`/`EVENT_GEN_PROGRESS` ring tags,
/// and `TestAppCtx` records it. `app.emit(name, value)` serializes a
/// `serde_json::Value` to the SAME bytes it serialized the payload struct to,
/// so the renderer contract is unchanged by the indirection.
pub type GenEventSink = std::sync::Arc<dyn Fn(&'static str, serde_json::Value) + Send + Sync>;

/// Emit a job state-change event through the host-agnostic sink.
///
/// Emission failure is deliberately swallowed: a closed window must never abort
/// a running job's bookkeeping (the pre-54.1 `let _ = app.emit(..)` discipline,
/// unchanged). `carries_provenance_watermark` is the flag resolved for this
/// job's model at submit (see [`resolve_provenance_flag`]). Every caller passes
/// it; this function is the ONE place that decides it reaches the wire only on a
/// terminal `"ready"`, so the "disclosure is a completion fact" invariant cannot
/// be broken by an emit site forgetting it (GEN-09).
pub fn emit_job_event(
    events: &GenEventSink,
    job_id: &agent_gen::JobId,
    req: &agent_gen::GenRequest,
    status: &agent_gen::JobStatus,
    media_item_ids: Vec<String>,
    carries_provenance_watermark: bool,
) {
    let payload = GenJobEventPayload {
        job_id: job_id.as_str().to_string(),
        provider: req.provider.as_str().to_string(),
        model_id: req.model_id.clone(),
        state: status.wire_state().to_string(),
        error: match status {
            agent_gen::JobStatus::Failed(e) => Some(e.clone()),
            _ => None,
        },
        media_item_ids,
        carries_provenance_watermark: match status {
            // Only a READY job produced media there is anything to disclose
            // about. Pending/failed/cancelled carry `None`.
            agent_gen::JobStatus::Ready(_) => Some(carries_provenance_watermark),
            _ => None,
        },
    };
    if let Ok(value) = serde_json::to_value(payload) {
        (**events)(GEN_JOB_EVENT, value);
    }
}

/// Emit the liveness ping for a still-pending job. Same swallow-on-failure
/// discipline as [`emit_job_event`].
pub fn emit_progress_event(events: &GenEventSink, job_id: &agent_gen::JobId, polls: u32) {
    let payload = GenProgressEventPayload {
        job_id: job_id.as_str().to_string(),
        polls,
    };
    if let Ok(value) = serde_json::to_value(payload) {
        (**events)(GEN_PROGRESS_EVENT, value);
    }
}

/// Drive one submitted job to a terminal state and RETURN that state.
///
/// # The return value IS the channel (plan 54.1-03)
///
/// This function's `Ready` arm deliberately does NOT land, does NOT touch the
/// registry and does NOT emit: it hands the unlanded `Vec<AssetRef>` back to
/// whoever awaits it, and [`finalize_ready_job`] — which needs an
/// [`crate::AppCtx`] this `'static` future cannot hold — does all three. The
/// non-`Ready` terminal arms need no landing, so they record + announce
/// themselves here and return the same status they emitted.
///
/// # Ownership discipline (Pitfall 5)
///
/// Every parameter is OWNED — `Arc`s, the minted [`agent_gen::JobId`], the
/// request, and the sink. Nothing borrowed crosses an `.await`, which is what
/// makes the returned future `Send + 'static` and therefore spawnable.
///
/// # It can never busy-spin (T-31-07)
///
/// The `Pending` arm is the ONLY branch that loops, and it always sleeps the
/// PROVIDER's own [`agent_gen::GenProvider::poll_interval`] first.
pub async fn poll_job_until_terminal<P: agent_gen::GenProvider>(
    provider: std::sync::Arc<P>,
    job_id: agent_gen::JobId,
    handle: agent_gen::JobHandle,
    req: agent_gen::GenRequest,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    registry: std::sync::Arc<agent_gen::JobRegistry>,
    // GEN-09: resolved ONCE at submit from the vetted catalog and MOVED into
    // this task alongside the other captured job state. Deliberately not a
    // `JobRecord` field — that would mean a new `JobRegistry` setter against
    // 31-02's frozen contract for a fact only the terminal emit reads.
    carries_provenance_watermark: bool,
    events: GenEventSink,
) -> agent_gen::JobStatus {
    use std::sync::atomic::Ordering;

    let mut polls: u32 = 0;
    loop {
        // (a) Cancellation is checked FIRST, before any provider work, so a
        //     cancel raised while we slept takes effect on this iteration
        //     rather than after one more pointless network call.
        if cancel.load(Ordering::Relaxed) {
            let _ = provider.cancel(&handle).await; // best-effort
            registry.set_status(&job_id, agent_gen::JobStatus::Cancelled);
            emit_job_event(
                &events,
                &job_id,
                &req,
                &agent_gen::JobStatus::Cancelled,
                Vec::new(),
                carries_provenance_watermark,
            );
            return agent_gen::JobStatus::Cancelled;
        }

        match provider.poll(&handle).await {
            // (b) Still working: report liveness, then back off. This is the
            //     ONLY branch that does not return, and it always sleeps.
            Ok(agent_gen::JobStatus::Pending) => {
                polls = polls.saturating_add(1);
                emit_progress_event(&events, &job_id, polls);
                // Per-provider cadence (GEN-05 "poll on backoff"): Fixture keeps
                // the historic 250ms default; the real video models override to
                // 10s for their 11s–6min remote jobs. Owned by the provider via
                // the additive `GenProvider::poll_interval()` (Phase 33).
                tokio::time::sleep(provider.poll_interval()).await;
            }

            // (c) READY — the one arm this function does NOT finish. The assets
            //     are handed BACK unlanded: landing needs an `AppCtx`, and
            //     announcing "ready" before the bytes are on disk would be a lie
            //     (the Phase-31 invariant, now enforced in `finalize_ready_job`).
            Ok(agent_gen::JobStatus::Ready(assets)) => {
                return agent_gen::JobStatus::Ready(assets);
            }

            // (d) The other terminal states: nothing to land, so record + tell
            //     the host here and return the same status that was emitted.
            Ok(terminal) => {
                registry.set_status(&job_id, terminal.clone());
                emit_job_event(
                    &events,
                    &job_id,
                    &req,
                    &terminal,
                    Vec::new(),
                    carries_provenance_watermark,
                );
                return terminal;
            }

            // (e) The provider itself errored: that IS the job's outcome.
            Err(e) => {
                let failed = agent_gen::JobStatus::Failed(e.to_string());
                registry.set_status(&job_id, failed.clone());
                emit_job_event(
                    &events,
                    &job_id,
                    &req,
                    &failed,
                    Vec::new(),
                    carries_provenance_watermark,
                );
                return failed;
            }
        }
    }
}

/// The ONE landing + terminal-emit path — for both hosts, and for both the
/// synchronous (zero-poll) and asynchronous completions.
///
/// A `Ready` becomes real, probe-measured [`rudis_core::MediaBinItem`]s on disk
/// BEFORE the terminal event is emitted, so `media_item_ids` names assets the
/// renderer (or the C# shell) can actually resolve. **A landing failure IS the
/// job's outcome** — announcing "ready" with nothing on disk stays impossible
/// (T-54.1-03, the Phase-31 invariant relocated intact).
///
/// Synchronous on purpose: it does blocking work (`std::fs::write` plus an
/// `ffprobe` PROCESS per asset), so callers on an async worker wrap it in
/// `tokio::task::block_in_place` / the host's blocking pool rather than this
/// function pretending to be async.
///
/// Returns the landed ids plus the GEN-09 provenance flag — exactly the pair the
/// three agent seams need for their disclosure, which is why they can now read
/// it from a return value instead of scanning an event.
pub fn finalize_ready_job<C: crate::AppCtx>(
    ctx: &C,
    job_id: &agent_gen::JobId,
    req: &agent_gen::GenRequest,
    assets: Vec<agent_gen::AssetRef>,
    registry: &agent_gen::JobRegistry,
    carries_provenance_watermark: bool,
) -> Result<(Vec<String>, bool), String> {
    let events = ctx.gen_event_sink();
    match land_ready_assets(ctx, req.provider.as_str(), &assets) {
        Ok(items) => {
            let media_item_ids: Vec<String> = items.into_iter().map(|item| item.id).collect();
            let terminal = agent_gen::JobStatus::Ready(assets);
            registry.set_status(job_id, terminal.clone());
            emit_job_event(
                &events,
                job_id,
                req,
                &terminal,
                media_item_ids.clone(),
                carries_provenance_watermark,
            );
            Ok((media_item_ids, carries_provenance_watermark))
        }
        Err(e) => {
            let msg = format!("the generation finished but its asset could not be landed: {e}");
            let failed = agent_gen::JobStatus::Failed(msg.clone());
            registry.set_status(job_id, failed.clone());
            emit_job_event(
                &events,
                job_id,
                req,
                &failed,
                Vec::new(),
                carries_provenance_watermark,
            );
            Err(msg)
        }
    }
}

/// What [`start_generation_job`] hands back: the job's id plus, for an async
/// job, the poll task's `JoinHandle` — whose OUTPUT is now the terminal
/// [`agent_gen::JobStatus`] rather than `()`. A synchronous provider spawns
/// nothing, hence `Option`.
pub struct StartedJob {
    pub job_id: agent_gen::JobId,
    /// Awaited by the three agent seams (bounded by their wait caps) and by the
    /// `submit_generation_job` IPC path's landing continuation, which is how a
    /// terminal outcome now travels — the pre-54.1 shape returned `()` and the
    /// outcome had to be recovered from the event bus.
    pub task: Option<tokio::task::JoinHandle<agent_gen::JobStatus>>,
    /// Phase 32 (GEN-01): the media ids landed on the SYNC Ready branch, so the
    /// agent tool can look up the new asset without any second channel. EMPTY on
    /// the Pending (async) path — nothing has landed yet there — and empty on a
    /// sync branch whose landing FAILED (the registry then holds the `Failed`
    /// status the seams report, unchanged from Phase 32).
    pub media_item_ids: Vec<String>,
    /// Phase 32 (GEN-09): the resolved provenance-watermark flag, carried so the
    /// agent tool can append the disclosure to its ToolResult without a second
    /// catalog lookup. A `bool` (Copy), so the sync path reuses it freely.
    pub carries_provenance_watermark: bool,
    /// Plan 54.1-03: the request as SUBMITTED, handed back so the awaiting
    /// caller can run [`finalize_ready_job`] with the byte-identical
    /// `GenRequest` the job was created from. `start_generation_job` consumes the
    /// caller's original; carrying it back here (rather than making every seam
    /// keep its own pre-submit clone) means the finalizer provably cannot be
    /// handed a DIFFERENT request than the one that was billed.
    pub req: agent_gen::GenRequest,
}

/// Submit a generation request and begin its lifecycle (GEN-05).
///
/// The split by [`agent_gen::SubmitOutcome`] is the whole point of the type — a
/// SYNC provider never enters the poll loop at all.
///
/// Phase 42.3 (F): [`spend_confirmation_gate`] is the pre-spend consent policy
/// guarding this choke point. It is consulted by the CALLER (before the request
/// is even built), not inside this function, because the agent surface has to
/// halt an entire LLM turn to ask — a decision that cannot be expressed as a
/// return value from a submit call.
///
/// # Why the spawn is a plain `tokio::spawn` (54.1-RESEARCH Open Question 2)
///
/// OQ2 offered a choice: a new `AppCtx::spawn_background` method, or two thin
/// per-host wrappers around one shared loop body. Neither is needed, because
/// BOTH hosts reach this function inside a real multi-threaded Tokio runtime
/// and therefore have an ambient handle:
///
/// * the Tauri host — `tauri::async_runtime` IS multi-thread Tokio (`mod spike`
///   in `src-tauri/src/generation.rs` is the standing committed proof), and
///   every path in reaches this through either a `#[tauri::command] async fn`
///   or `crate::generation_bridge`'s `ctx.block_on(..)`, both of which enter it;
/// * the C ABI host — `FfiAppCtx::block_on` builds and enters its own
///   `new_multi_thread().enable_all()` runtime.
///
/// So the "two runtimes' own idioms" the wrappers existed to preserve turn out
/// to be the SAME idiom, and one spawn call serves both — which is strictly
/// closer to SC-5 ("one generation path") than two wrappers would have been.
pub async fn start_generation_job<C: crate::AppCtx, P: agent_gen::GenProvider + 'static>(
    ctx: &C,
    provider: std::sync::Arc<P>,
    registry: std::sync::Arc<agent_gen::JobRegistry>,
    allow_list: agent_gen::AllowList,
    req: agent_gen::GenRequest,
) -> Result<StartedJob, agent_gen::GenError> {
    // 31-04 (Wave 4, GEN-08): the licensing gate. `submit_checked` is the ONE
    // gate every submit path goes through — and it runs HERE, at the very top,
    // BEFORE the job-registry insert, before any event emit, and before any
    // file work. A rejected model therefore leaves ZERO trace (asserted by
    // `submit_rejects_non_allow_listed_model_before_submit`). The allow list is
    // taken BY VALUE (owned, cheap to clone) rather than as a borrow, so nothing
    // borrowed crosses the `.await` (Pitfall 5).
    //
    // Deliberately NOT `provider.submit(..)` — a direct submit call anywhere in
    // this module would be an ungoverned second path (T-31-18).
    let outcome = agent_gen::submit_checked(provider.as_ref(), &allow_list, req.clone()).await?;

    // 31-04 (Wave 4, GEN-09): resolve the model's provenance-watermark flag
    // ONCE, here, from the provider's own catalog — the model just cleared the
    // allow list, so it is a vetted entry. From this point the flag is ordinary
    // owned job state: moved into the spawned poll task on the async path, used
    // directly on the sync path, and reaching the renderer only on the terminal
    // "ready" emit. It is the disclosure hook, not a domain-model field.
    let carries_provenance_watermark =
        resolve_provenance_flag(provider.as_ref(), &req.model_id).await;

    let job_id = agent_gen::JobId::mint();
    let cancel = registry.insert(&job_id);
    let events = ctx.gen_event_sink();

    match outcome {
        // Async: announce the job, then let the background task drive it.
        agent_gen::SubmitOutcome::Pending(handle) => {
            emit_job_event(
                &events,
                &job_id,
                &req,
                &agent_gen::JobStatus::Pending,
                Vec::new(),
                carries_provenance_watermark,
            );
            let task = tokio::spawn(poll_job_until_terminal(
                provider,
                job_id.clone(),
                handle,
                req.clone(),
                cancel,
                registry,
                carries_provenance_watermark,
                events,
            ));
            Ok(StartedJob {
                job_id,
                task: Some(task),
                // Async: nothing has landed yet (the awaiting caller's
                // `finalize_ready_job` lands it later).
                media_item_ids: Vec::new(),
                carries_provenance_watermark,
                req,
            })
        }

        // Sync: the degenerate zero-poll case — already terminal at submit, so
        // THIS call is the awaiting caller and finalizes inline. The two paths
        // therefore look IDENTICAL to the host (a populated `media_item_ids` on
        // "ready", a "failed" if the asset could not be landed), and there are
        // zero gen:progress events — the whole point of the sync case.
        agent_gen::SubmitOutcome::Ready(assets) => {
            // `block_in_place`, not an inline call: the landing writes files and
            // spawns an `ffprobe` PROCESS per asset, neither of which belongs on
            // an async worker (the same guard `crate::generation_bridge` applies
            // one layer out). Outside a runtime it simply runs the closure.
            let media_item_ids = tokio::task::block_in_place(|| {
                finalize_ready_job(
                    ctx,
                    &job_id,
                    &req,
                    assets,
                    &registry,
                    carries_provenance_watermark,
                )
            })
            // A landing failure is NOT a submit error: the registry holds the
            // `Failed` status and the terminal event already said so, exactly as
            // before 54.1-03. The seams below read that status and report it.
            .map(|(ids, _)| ids)
            .unwrap_or_default();
            Ok(StartedJob {
                job_id,
                task: None,
                media_item_ids,
                carries_provenance_watermark,
                req,
            })
        }
    }
}

/// Phase 55.1 (D-12): the ONE direct-submit entry for a HOST-initiated
/// (non-agent-tool) generation — **the WR-04 / Backlog § 999.8 closure.**
///
/// FIRST act, before any provider resolution, registry read, allow-list clone or
/// request construction: [`spend_confirmation_gate`]. Not "early in the body" —
/// literally first, which is why an unwired host bundle still yields the spend
/// question rather than a "state is not managed" error (pinned by
/// `the_gate_runs_before_the_host_bundle_is_consulted_at_all`).
///
/// `approved` is the caller's evidence of user consent. For a direct
/// user-initiated submit **the click IS the consent** — the precedent
/// [`crate::SpendPolicy`]'s own doc has stated for this exact surface since
/// Phase 42.3 — so an interactive host passes `true` from a real user gesture
/// **and nothing else may**. It is not a flag a tool input, a rulebook line or a
/// model's output can reach; there is no setter, only this argument. Fail-closed:
/// `approved == false` returns the gate's own question as the error and provably
/// does zero provider work (proven by call count, not asserted —
/// `FixtureGenProvider::submit_calls()` reads 0).
///
/// # Why this exists at all, given nothing calls it yet
///
/// The `#[tauri::command] submit_generation_job` WR-04 was written against lived
/// ONLY in `src-tauri` and died with Phase 55's cutover, so for a short window
/// the workspace had NO direct-submit body. Waiting for the next host to need
/// one would mean writing it ungated and gating it afterwards — precisely the
/// sequence § 999.8 exists to prevent. It is created gated instead, so there is
/// no ungated body anywhere for a future surface to wrap.
///
/// **Any future direct-submit surface — a Settings model picker, an FFI export,
/// a C# host command — MUST route through this function.** Building a sibling
/// that calls [`start_generation_job`] directly is the forked-policy trap: a
/// "confirmed" spend on one surface silently becoming an unconfirmed one on the
/// other. There is exactly one spend policy and this consults it.
pub async fn submit_generation_job_inner<C: crate::AppCtx>(
    ctx: &C,
    gen: &GenHost<'_>,
    model_id: String,
    modality: agent_gen::RequestModality,
    prompt: String,
    approved: bool,
) -> Result<StartedJob, String> {
    use agent_gen::GenProvider;

    // (a) THE GATE, first. The cost dimension is the advisory roster's own line
    //     for this model or the literal PRICE_UNKNOWN — with an open roster
    //     (D-01) a caller-chosen id may well be one Rudis has never priced, and
    //     that is exactly the case the user must be told about rather than
    //     shielded from (T-55.1-08).
    if let crate::SpendGateDecision::NeedsConfirmation { question } = spend_confirmation_gate(
        "submit_generation_job",
        Some(&model_id),
        Some(cost_signal_for_model(&model_id)),
        approved,
    ) {
        return Err(question);
    }

    // (a2) Argument shape, the same two refusals both agent seams carry — a $0
    //      local rejection instead of a wasted round trip. **Deliberately AFTER
    //      the gate, not before it.** Both checks are pure and spend nothing, so
    //      either order is safe for money; putting them second keeps "the gate
    //      is the first act" an unqualified claim with no carve-out for a
    //      reviewer to have to reason about. The cost is that a malformed call
    //      is confirmed before it is refused (the question would name an empty
    //      model). That is the right trade in a money gate: an exception list is
    //      how a first act stops being first.
    //
    //      Emptiness is NOT roster validation (D-01 forbids that) — a blank id
    //      is a malformed request, not an unrecognised model. There is
    //      deliberately no prompt LENGTH cap here either, unlike the agent
    //      seams: theirs exists because the prompt is MODEL-authored downstream
    //      of arbitrary user text (T-32-21). A host prompt is the user's own
    //      words, and silently refusing a long one is a product decision this
    //      plan has no mandate to make.
    if model_id.trim().is_empty() {
        return Err(
            "submit_generation_job requires a model -- an empty model id reached the \
             direct-submit path"
                .to_string(),
        );
    }
    if prompt.trim().is_empty() {
        return Err("submit_generation_job requires a non-empty prompt".to_string());
    }

    // (b) Only now: the provider slot, chosen by the CALLER's modality — the
    //     same D-05 rule the agent seams follow (the call's own context decides
    //     the endpoint family; the model id never does). Each arm is explicit;
    //     there is no wildcard, so a fourth modality breaks the build instead of
    //     silently routing audio at a video provider.
    let provider = match modality {
        agent_gen::RequestModality::Image => gen
            .image
            .ok_or_else(|| "generation provider state is not managed".to_string())?
            .resolve_via(gen.stores()?)
            .ok_or_else(|| NO_PROVIDER_CONFIGURED.to_string())?,
        agent_gen::RequestModality::Video => gen
            .video
            .ok_or_else(|| "video generation provider state is not managed".to_string())?
            .resolve_via(gen.stores()?)
            .ok_or_else(|| NO_VIDEO_PROVIDER_CONFIGURED.to_string())?,
        // Refused BY NAME, never by fallthrough. ElevenLabs TTS has no
        // caller-chosen model (its id is a server-side const) and no host
        // picker, so a direct audio submit is a category error — and the
        // refusal names the surface that does own it, so a caller on stale
        // context can correct itself.
        agent_gen::RequestModality::Audio => {
            return Err(
                "direct audio submission goes through generate_ai_audio -- ElevenLabs TTS \
                 has no caller-chosen model"
                    .to_string(),
            )
        }
    };
    let registry = gen.registry()?;
    let allow_list = gen.allow_list()?;

    // (c) The request. `model_id` is the caller's string verbatim — no roster
    //     lookup (D-01); the provider id is server-known; `background` is inert
    //     (no shipping provider reads it); this surface carries no conditioning
    //     frames, so a video submit here is always text-to-video.
    let req = agent_gen::GenRequest {
        provider: provider.id(),
        model_id,
        modality,
        prompt,
        background: agent_gen::BackgroundMode::Transparent,
        reference_image: None,
        destination_image: None,
        // Phase 56 (GEN-11): this seam re-renders nothing — it has no source
        // clip, so it stays on the byte-identical pre-56 path. `source_video`
        // is the ENDPOINT SELECTOR (`None` => never `video_to_video`), and the
        // v2v conditioning slot is a Plan 06 seam.
        source_video: None,
        reference_images: None,
    };

    // (d) Through the ONE governed submit path — the GEN-08 gate is inside it.
    //     Deliberately NOT `provider.submit(..)` (T-31-18).
    start_generation_job(ctx, provider, registry, allow_list, req)
        .await
        .map_err(gen_submit_error_message)
}

// ---------------------------------------------------------------------------
// The three agent-tool seam entries (GEN-01 / GEN-02 / GEN-03)
// ---------------------------------------------------------------------------

/// The host-managed generation state one seam call needs, gathered by the HOST
/// adapter and handed in as a bundle (plan 54.1-03).
///
/// Every field is a borrow of a plain struct. The Tauri shell resolves them out
/// of `tauri::State` at its `GenSubmission` impl and the C ABI host reads them
/// off `RudisCtx`'s own fields — so "which host am I" is answered ONCE, at the
/// adapter, and never inside a seam.
///
/// # Why `Option`, which the plan's sketch did not have — a MEASURED correction
///
/// The plan proposed five non-optional borrows, on the reasoning that "a missing
/// slot is impossible by construction". Against the tree that is false in a way
/// that matters: a slot is `Option` because a HOST MAY GENUINELY NOT HAVE IT,
/// and the pre-54.1 seams each consulted ONLY THEIR OWN MODALITY'S slot. Making
/// the bundle eagerly total moved that check to the adapter and turned
/// "no image provider configured" into "video generation provider state is not
/// managed" for an app that only wires the image slot — 16 red tests, and a real
/// cross-modality coupling that did not exist before (the image tool must not
/// care whether video is wired).
///
/// So the presence check stays per-modality, which means it stays here. The
/// `"… state is not managed"` messages moved with it, byte-identical: they name
/// the *slot* rather than any host mechanism, they are unreachable for a host
/// that always supplies its fields (the C ABI one will), and keeping them beside
/// the honest-absence messages means both hosts tell the same story about both
/// kinds of absence.
///
/// The two absences are genuinely different and both are preserved:
/// `None` here = "this host never wired that slot"; `Some(slot)` whose
/// `resolve()` yields `None` = "wired, but no credential" → the honest
/// [`NO_PROVIDER_CONFIGURED`] / [`NO_VIDEO_PROVIDER_CONFIGURED`] /
/// [`NO_AUDIO_PROVIDER_CONFIGURED`] messages.
///
/// **`resolve()` is deliberately NOT called by the adapter** (Phase 43, LAT-05):
/// it is the lazy, once-only OS-credential read, and firing it for all three
/// modalities on every seam call — or before the prompt has even been validated
/// — would undo that. The seams call it, for their own slot, after validation.
pub struct GenHost<'a> {
    pub image: Option<&'a ManagedGenProvider>,
    pub video: Option<&'a ManagedVideoGenProvider>,
    pub audio: Option<&'a ManagedAudioGenProvider>,
    pub jobs: Option<&'a ManagedGenJobs>,
    pub allow_list: Option<&'a ManagedAllowList>,
    /// Phase 69 (D-69-02/D-69-13): the host-owned provider credential stores —
    /// the ONLY path the three slots resolve through.
    pub stores: Option<&'a ManagedProviderKeyStore>,
}

impl GenHost<'_> {
    /// Phase 69 (D-69-13): invalidate every slot that resolves `provider_id`
    /// (`"runway"` → image + video; `"elevenlabs"` → audio; anything else is a
    /// no-op). Reads nothing — the next use re-resolves lazily.
    pub fn invalidate_provider(&self, provider_id: &str) {
        match provider_id {
            agent_gen::RUNWAY_PROVIDER_ID => {
                if let Some(image) = self.image {
                    image.invalidate();
                }
                if let Some(video) = self.video {
                    video.invalidate();
                }
            }
            agent_gen::ELEVENLABS_PROVIDER_ID => {
                if let Some(audio) = self.audio {
                    audio.invalidate();
                }
            }
            _ => {}
        }
    }

    /// The provider key stores, or a "not managed" error.
    fn stores(&self) -> Result<&ManagedProviderKeyStore, String> {
        self.stores
            .ok_or_else(|| "provider key store state is not managed".to_string())
    }

    /// The job registry, or the verbatim pre-54.1 "not managed" error.
    fn registry(&self) -> Result<std::sync::Arc<agent_gen::JobRegistry>, String> {
        self.jobs
            .map(|j| j.0.clone())
            .ok_or_else(|| "generation jobs state is not managed".to_string())
    }

    /// The GEN-08 allow list, or the verbatim pre-54.1 "not managed" error.
    /// Cloned, not borrowed: it must be OWNED before the first `.await`
    /// (Pitfall 5), and it is a short `Vec` of string pairs.
    fn allow_list(&self) -> Result<agent_gen::AllowList, String> {
        self.allow_list
            .map(|a| a.0.clone())
            .ok_or_else(|| "generation allow-list state is not managed".to_string())
    }
}

/// Phase 33 (GEN-02 / T-33-22): the upper bound on how long the async agent tool
/// will await a video job before cancelling it — the SAME 12-minute cap the
/// `gen02_live` test uses (research: 11s–6min real latency + margin). The tool
/// call NEVER blocks the turn unboundedly: on timeout it raises the job's cancel
/// latch and returns a clear error.
pub const AGENT_VIDEO_WAIT_CAP: std::time::Duration = std::time::Duration::from_secs(12 * 60);

/// Phase 42.1-04: the same bound for a still image, which is a much shorter job.
///
/// Needed because the image provider became ASYNCHRONOUS with the move to
/// Runway — the OpenAI path this replaced returned the PNG inline, so the image
/// seam had no wait at all and failed outright on the first live call. Four
/// minutes is generous for a `gen4_image` render (seconds in practice) while
/// still bounding a wedged turn far tighter than the video cap would.
pub const AGENT_IMAGE_WAIT_CAP: std::time::Duration = std::time::Duration::from_secs(4 * 60);

/// GEN-01: what [`generate_runway_image_for_agent`] hands back — the landed
/// media ids plus the GEN-09 provenance flag for the tool's disclosure.
pub struct AgentGeneratedImage {
    pub media_item_ids: Vec<String>,
    pub carries_provenance_watermark: bool,
}

/// Phase 34 (GEN-03): what [`generate_elevenlabs_audio_for_agent`] hands back —
/// the landed media ids plus the GEN-09 provenance flag for the tool's
/// disclosure. `Debug` carries only ids + a bool — no key material (T-34-10).
#[derive(Debug)]
pub struct AgentGeneratedAudio {
    pub media_item_ids: Vec<String>,
    pub carries_provenance_watermark: bool,
}

/// GEN-02: what [`generate_runway_video_for_agent`] hands back — the landed
/// media ids plus the GEN-09 provenance flag for the tool's disclosure.
pub struct AgentGeneratedVideo {
    pub media_item_ids: Vec<String>,
    pub carries_provenance_watermark: bool,
}

/// The agent-tool seam entry for `generate_ai_image` (GEN-01).
///
/// Routes a prompt through the ONE governed submit path — [`start_generation_job`],
/// whose FIRST act is the `submit_checked` GEN-08 gate (reject-before-submit) —
/// then the provider, then the unchanged Phase-31 landing bridge, landing an
/// undoable `MediaBinItem`. Deliberately NOT a second `provider.submit(..)` path
/// (T-32-20): `grep provider.submit(` in this module stays comment-only.
///
/// # Reference conditioning: RESTORED in 42.1-03 (the Wave-2 regression, closed)
///
/// The OpenAI path this replaced accepted a conditioning reference and posted it
/// to `/v1/images/edits`. Runway's still-image model expresses references
/// differently — a tagged `referenceImages: [{uri, tag}]` array cited as
/// `@MyTag` inside the prompt text — which 42.1-01 did not build, so 42.1-02
/// shipped a `generate_ai_image` that REFUSED a sketch. 42.1-03 builds it:
/// `agent_gen::build_text_to_image_request` attaches the tagged entry AND writes
/// the matching `@…` citation into `promptText` in one expression, so the
/// "is an uncited tag honoured?" question that blocked 42.1-01 is dissolved
/// rather than guessed at — Rudis never produces an uncited tag.
///
/// Two refusals survive, neither of them "not built": a DESTINATION frame on a
/// still image (a category error — a single image has no last frame), and a
/// reference on an image model whose caps do not document the field (it would be
/// silently ignored server-side, which is the undetectable lie the 42.1-02
/// refusal existed to prevent).
///
/// **Phase 55.1 (D-01/D-02/D-10): the model IS caller input now, by design.**
/// This doc used to say the opposite — *"the model id is SERVER-SIDE (from the
/// pinned image capability const), NEVER caller input"* — and that claim is
/// false as of this phase. `model` is the free-text Runway image model id the agent chose
/// (or the one the user named in chat), and it becomes `GenRequest.model_id`
/// verbatim, with no local roster check: Runway's own server-side enum is the
/// validator (an id it does not know is its own 400). The tool-schema field-name
/// ban that used to forbid exactly this field was retired ON PURPOSE with its
/// reason recorded in PROVENANCE.md Entry 17's 2026-08-01 amendment.
///
/// What has NOT changed: the provider id is still the server-known one (the gate
/// keys on it anyway, 31-04 Deviation 2); `prompt` still flows to exactly ONE
/// place, `GenRequest.prompt` — never a filename or an endpoint (T-32-17);
/// `background` still flows only to `GenRequest.background`, a closed
/// three-value enum; and the API key is still never touched here (it lives
/// inside the startup-resolved provider, T-32-18).
///
/// **Phase 54.1 (plan 03):** `AppCtx`-generic and host-agnostic. The
/// subscribe-to-our-own-events-and-rescan-them apparatus is GONE — the poll
/// task's own return value carries the outcome.
pub async fn generate_runway_image_for_agent<C: crate::AppCtx>(
    ctx: &C,
    gen: &GenHost<'_>,
    prompt: String,
    model: String,
    background: agent_gen::BackgroundMode,
    reference: Option<agent_gen::ReferenceImage>,
) -> Result<AgentGeneratedImage, String> {
    use agent_gen::GenProvider;

    // (a) Validate the prompt FIRST — a clear Err before ANY state/provider/
    //     network/paid work.
    if prompt.trim().is_empty() {
        return Err("generate_ai_image requires a non-empty prompt".to_string());
    }
    // (a2) Phase 55.1: the model must be SOMETHING. This is emptiness, not
    //      roster validation (D-01 forbids the latter) — the bridge already
    //      refuses a blank one, and this is the seam's own backstop for any
    //      other host that implements `GenSubmission`. Sharing the bridge's
    //      "requires a model" wording keeps ONE pre-spend needle covering both.
    if model.trim().is_empty() {
        return Err(
            "generate_ai_image requires a model -- an empty model id reached the seam".to_string(),
        );
    }
    let prompt_len = prompt.chars().count();
    if prompt_len > MAX_AGENT_IMAGE_PROMPT_CHARS {
        return Err(format!(
            "prompt is too long ({prompt_len} characters); the maximum is \
             {MAX_AGENT_IMAGE_PROMPT_CHARS}"
        ));
    }

    // (b) Resolve the provider from the host bundle. An unwired slot is a clear
    //     Err naming it; a wired-but-None provider is the shared honest message
    //     (the two surfaces cannot tell different stories, T-31-21/T-32-18).
    //     Only the IMAGE slot is consulted — this tool does not care whether the
    //     host wired video or audio.
    let provider = gen
        .image
        .ok_or_else(|| "generation provider state is not managed".to_string())?
        .resolve_via(gen.stores()?)
        .ok_or_else(|| NO_PROVIDER_CONFIGURED.to_string())?;
    let registry = gen.registry()?;
    let allow_list = gen.allow_list()?;

    // (c) Build the request: the model is the CALLER's string (Phase 55.1 D-10 —
    //     it was resolved from a pinned image capability const, i.e. `gen4_image`
    //     hardcoded, which is why `gen4_image_turbo` and every other image model
    //     was unreachable); the provider id is server-known; prompt + background
    //     are the other caller values.
    let req = agent_gen::GenRequest {
        provider: provider.id(),
        model_id: model,
        // Phase 55.1 (D-05): the IMAGE seam, so the endpoint family is decided
        // HERE — by which tool ran — not by looking the model up in a roster.
        modality: agent_gen::RequestModality::Image,
        prompt,
        background,
        // Phase 34.1 (GEN-10): the resolved conditioning reference (sketch /
        // annotated frame / media item), produced backend-side by
        // `resolve_reference_image` in the tool-call handler. `None` is the
        // unconditioned request; `Some` becomes Runway's tagged
        // `referenceImages` entry plus its `@…` citation (42.1-03 — see this
        // fn's doc; the 42.1-02 refusal is closed).
        reference_image: reference,
        // Quick 260726-t5z: a still image has no "last frame", so the IMAGE seam
        // never sets this -- exactly how `background` is image-only in the other
        // direction. Always `None` here, on purpose.
        destination_image: None,
        // Phase 56 (GEN-11): this seam re-renders nothing — it has no source
        // clip, so it stays on the byte-identical pre-56 path. `source_video`
        // is the ENDPOINT SELECTOR (`None` => never `video_to_video`), and the
        // v2v conditioning slot is a Plan 06 seam.
        source_video: None,
        reference_images: None,
    };

    // (d) Submit through the ONE gated path (the GEN-08 gate is INSIDE it).
    let started = start_generation_job(ctx, provider, registry.clone(), allow_list, req)
        .await
        .map_err(gen_submit_error_message)?;

    // (e) A synchronous provider (the fixture sync path, and the ElevenLabs
    //     shape) already landed its ids — unchanged.
    if !started.media_item_ids.is_empty() {
        return Ok(AgentGeneratedImage {
            media_item_ids: started.media_item_ids,
            carries_provenance_watermark: started.carries_provenance_watermark,
        });
    }

    // (f) The ASYNC path: await the spawned poll task, bounded, and take the
    //     terminal outcome DIRECTLY off its return value — never by reading an
    //     event back (54.1-RESEARCH Pitfall 1). Mirrors the video seam exactly,
    //     including never hanging: on timeout raise the cancel latch (so the
    //     orphaned task self-terminates on its next iteration) and return a
    //     clear Err. A still image is far quicker than a clip, so it gets its
    //     own tighter cap rather than borrowing the video one.
    let job_id = started.job_id.clone();
    let carries = started.carries_provenance_watermark;
    let outcome = if let Some(task) = started.task {
        match tokio::time::timeout(AGENT_IMAGE_WAIT_CAP, task).await {
            Err(_) => {
                registry.request_cancel(&job_id);
                Err(format!(
                    "the image generation did not finish within {} minutes; it was \
                     cancelled — try again",
                    AGENT_IMAGE_WAIT_CAP.as_secs() / 60
                ))
            }
            Ok(join) => match join.map_err(|e| format!("the generation poll task failed: {e}"))? {
                // THIS call is the awaiting caller, so it runs the ONE landing +
                // terminal-emit path. `block_in_place`: fs + ffprobe off the
                // async worker.
                agent_gen::JobStatus::Ready(assets) => tokio::task::block_in_place(|| {
                    finalize_ready_job(ctx, &job_id, &started.req, assets, &registry, carries)
                }),
                agent_gen::JobStatus::Failed(msg) => Err(msg),
                agent_gen::JobStatus::Cancelled => {
                    Err("the image generation was cancelled".to_string())
                }
                agent_gen::JobStatus::Pending => {
                    Err("the generation ended in an unknown state".to_string())
                }
            },
        }
    } else {
        match registry.get_status(&job_id) {
            Some(agent_gen::JobStatus::Failed(msg)) => Err(msg),
            _ => Err("the generation finished but no asset was landed".to_string()),
        }
    };
    outcome.map(|(media_item_ids, carries_provenance_watermark)| AgentGeneratedImage {
        media_item_ids,
        carries_provenance_watermark,
    })
}

/// Phase 34 (GEN-03): the agent-tool seam entry for `generate_ai_audio`.
///
/// The SYNC sibling of [`generate_runway_image_for_agent`] (NOT the async
/// Runway poll-await): ElevenLabs TTS returns the audio bytes on the submit call
/// itself (`SubmitOutcome::Ready`, zero polls — 34-01). Routes a prompt through
/// the ONE governed submit path — [`start_generation_job`], whose FIRST act is the
/// `submit_checked` GEN-08 gate (reject-before-submit) — then the provider, then
/// the UNCHANGED Phase-31 landing bridge, landing an undoable `MediaBinItem`.
/// Deliberately NOT a second `provider.submit(..)` path (T-34-12): `grep
/// provider.submit(` in this module stays comment-only.
///
/// The model id is a SERVER-SIDE const ([`agent_gen::ELEVENLABS_TTS_MODEL`]),
/// NEVER caller input; `background` is the image-only field ElevenLabs IGNORES.
/// The `prompt` — which IS the literal text to speak — is the SOLE caller value
/// and flows to exactly ONE place: `GenRequest.prompt` (the JSON `text` field
/// downstream) — never a filename, endpoint, model, or voice (T-34-11). The API
/// key is never touched here (it lives inside the startup-resolved provider,
/// T-34-10).
///
/// FAIL-CLOSED this wave: there is NO `(elevenlabs, *)` allow-list row yet, so a
/// REAL elevenlabs submit is rejected by the GEN-08 gate BEFORE any network call.
/// The offline fixture-provider tests prove this seam end-to-end by admitting the
/// fixture id under the const model.
pub async fn generate_elevenlabs_audio_for_agent<C: crate::AppCtx>(
    ctx: &C,
    gen: &GenHost<'_>,
    prompt: String,
) -> Result<AgentGeneratedAudio, String> {
    use agent_gen::GenProvider;

    // (a) Validate the prompt FIRST — a clear Err before ANY state/provider/
    //     network/paid work.
    if prompt.trim().is_empty() {
        return Err("generate_ai_audio requires a non-empty prompt".to_string());
    }
    let prompt_len = prompt.chars().count();
    if prompt_len > MAX_AGENT_AUDIO_PROMPT_CHARS {
        return Err(format!(
            "prompt is too long ({prompt_len} characters); the maximum is \
             {MAX_AGENT_AUDIO_PROMPT_CHARS}"
        ));
    }

    // (b) Resolve the provider: the MODALITY-SCOPED audio slot (elevenlabs only)
    //     — NOT the image or video slot. Unwired ⇒ a clear Err naming it;
    //     wired-but-None ⇒ the audio-specific honest message (T-34-10).
    let provider = gen
        .audio
        .ok_or_else(|| "audio generation provider state is not managed".to_string())?
        .resolve_via(gen.stores()?)
        .ok_or_else(|| NO_AUDIO_PROVIDER_CONFIGURED.to_string())?;
    let registry = gen.registry()?;
    let allow_list = gen.allow_list()?;

    // (c) Build the request: the model is a const, the provider id is
    //     server-known, prompt is the SOLE caller value. `background` is the
    //     image-only field ElevenLabs ignores (carried only because the frozen
    //     GenRequest has the field).
    let req = agent_gen::GenRequest {
        provider: provider.id(),
        model_id: agent_gen::ELEVENLABS_TTS_MODEL.to_string(),
        // Phase 55.1 (D-05): the AUDIO seam. `ElevenLabsProvider` does not read
        // this field — it serves exactly one modality — but it is set honestly
        // rather than to a convenient `Image`, so that if this request ever
        // reached Runway by a wiring mistake it would be refused explicitly
        // instead of being built into a video request from a voice prompt.
        modality: agent_gen::RequestModality::Audio,
        prompt,
        // Phase 34.1 decision 4: seam-caller default flipped to `Transparent`.
        // ElevenLabs' wire body has no `background` key at all (the provider
        // ignores this field entirely), so this flip is inert — carried only
        // because it is one of CONTEXT.md's 4 named sites.
        background: agent_gen::BackgroundMode::Transparent,
        // Phase 34.1 (GEN-10): audio has no image reference — always `None`.
        reference_image: None,
        destination_image: None,
        // Phase 56 (GEN-11): this seam re-renders nothing — it has no source
        // clip, so it stays on the byte-identical pre-56 path. `source_video`
        // is the ENDPOINT SELECTOR (`None` => never `video_to_video`), and the
        // v2v conditioning slot is a Plan 06 seam.
        source_video: None,
        reference_images: None,
    };

    // (d) Submit through the ONE gated path (the GEN-08 gate is INSIDE it). With
    //     no (elevenlabs, *) allow-list row this wave, a real elevenlabs submit is
    //     rejected HERE; the fixture tests admit the fixture id to exercise the
    //     rest of the path offline.
    let started = start_generation_job(ctx, provider, registry.clone(), allow_list, req)
        .await
        .map_err(gen_submit_error_message)?;

    // (e) On a landed sync asset, we are done (ElevenLabs is sync — the Ready
    //     branch finalized inline). Otherwise interrogate the registry: a
    //     `Failed` sync landing, or an async outcome (a sync provider going async
    //     is a contract violation worth surfacing plainly), or an empty-id
    //     completion.
    if !started.media_item_ids.is_empty() {
        return Ok(AgentGeneratedAudio {
            media_item_ids: started.media_item_ids,
            carries_provenance_watermark: started.carries_provenance_watermark,
        });
    }
    match registry.get_status(&started.job_id) {
        Some(agent_gen::JobStatus::Failed(msg)) => Err(msg),
        _ if started.task.is_some() => Err(format!(
            "the provider accepted the request as an asynchronous job ({}); the audio will \
             land in the media bin when it finishes — generate_ai_audio expects a provider \
             that returns the audio synchronously",
            started.job_id.as_str()
        )),
        _ => Err("the generation finished but no asset was landed".to_string()),
    }
}

/// The agent-tool seam entry for `generate_ai_video` (GEN-02).
///
/// The ASYNC sibling of [`generate_runway_image_for_agent`]. Routes a prompt
/// through the ONE governed submit path — [`start_generation_job`], whose FIRST
/// act is the `submit_checked` GEN-08 gate (reject-before-submit) — then the
/// async Runway provider, then AWAITS the spawned poll task (bounded by
/// [`AGENT_VIDEO_WAIT_CAP`]) and takes the landed ids from the ONE finalizer.
/// Deliberately NOT a second `provider.submit(..)` path (T-33-21): `grep
/// provider.submit(` in this module stays comment-only.
///
/// **Phase 55.1 (D-01/D-02/D-11): the model IS caller input now, by design.**
/// This doc used to say the model id was *"still SERVER-SIDE and NEVER caller
/// input"*, reached only through a closed capability word — both claims are
/// false as of this phase. `model` is the free-text Runway model id
/// the agent chose (or the one the user named in chat) and it becomes
/// `GenRequest.model_id` verbatim. There is no local roster check and no local
/// capability check: a model that cannot serve the frames supplied now fails at
/// Runway with Runway's own 400 (D-05), which is a real cost recorded rather
/// than glossed. T-33-19's field-name ban was retired for this ONE field on
/// purpose — PROVENANCE.md Entry 17, 2026-08-01 amendment.
///
/// What has NOT changed: `background` is still the image-only field the video
/// provider IGNORES (passed `Transparent`, inert); `prompt` still flows to
/// exactly ONE place, `GenRequest.prompt`; which of the two video endpoints this
/// becomes is still decided by the FRAMES, never by the model
/// (`agent_gen::build_submission`, 55.1-01); and the API key is still never
/// touched here (T-33-20).
pub async fn generate_runway_video_for_agent<C: crate::AppCtx>(
    ctx: &C,
    gen: &GenHost<'_>,
    prompt: String,
    model: String,
    reference: Option<agent_gen::ReferenceImage>,
    destination: Option<agent_gen::ReferenceImage>,
) -> Result<AgentGeneratedVideo, String> {
    use agent_gen::GenProvider;

    // (a) Validate the prompt FIRST — a clear Err before ANY state/provider/
    //     network/paid work.
    if prompt.trim().is_empty() {
        return Err("generate_ai_video requires a non-empty prompt".to_string());
    }
    let prompt_len = prompt.chars().count();
    if prompt_len > MAX_AGENT_VIDEO_PROMPT_CHARS {
        return Err(format!(
            "prompt is too long ({prompt_len} characters); the maximum is \
             {MAX_AGENT_VIDEO_PROMPT_CHARS}"
        ));
    }
    // (a2) Phase 55.1: emptiness only — the seam's backstop for any host
    //      implementing `GenSubmission`, sharing the bridge's "requires a model"
    //      wording so ONE pre-spend needle covers both. NOT a roster check
    //      (D-01), and NOT a capability check: the capability-resolution layer
    //      that used to live here was unwired by 55.1-03 and DELETED by 55.1-06
    //      (see the block comment above `resolved_model_for_tool_input`). Its
    //      refusals — a model that cannot bridge a first+last pair, or cannot do
    //      text-only — are now Runway 400s over the network. Still fail-closed on
    //      spend, but remote, and in the vendor's words rather than ours.
    if model.trim().is_empty() {
        return Err(
            "generate_ai_video requires a model -- an empty model id reached the seam".to_string(),
        );
    }

    // (b) Resolve the provider: the MODALITY-SCOPED video slot — NOT the image
    //     slot. Unwired ⇒ a clear Err naming it; wired-but-None ⇒ the
    //     video-specific honest message (T-33-20).
    let provider = gen
        .video
        .ok_or_else(|| "video generation provider state is not managed".to_string())?
        .resolve_via(gen.stores()?)
        .ok_or_else(|| NO_VIDEO_PROVIDER_CONFIGURED.to_string())?;
    let registry = gen.registry()?;
    let allow_list = gen.allow_list()?;

    // Quick 260726-ufz: the ONE place that can say what actually reaches the
    // provider. The disclosure in the agent turn reports what Claude ASKED for;
    // this reports what survived resolution and is about to become the
    // `promptImage` first/last keyframe pair. Both halves are needed: "the model
    // never asked" and "the backend dropped it" are different bugs that look
    // identical from the output alone (live UAT 2026-07-26 could not tell them
    // apart). Sizes+dims only -- never the bytes, never the key
    // (T-33-20/T-14.3-05).
    let describe_frame = |f: &Option<agent_gen::ReferenceImage>| match f {
        Some(r) => format!("{}B {}x{}", r.bytes.len(), r.width, r.height),
        None => "absent".to_string(),
    };
    eprintln!(
        "[E-01] runway request: model={model} firstFrame={} lastFrame={} \
         (both present => A->B transition)",
        describe_frame(&reference),
        describe_frame(&destination)
    );

    // (c) Build the request: the model is the CALLER's own string (Phase 55.1
    //     D-01/D-02), passed through with no roster lookup; provider id is
    //     server-known; background is the image-only field the video path
    //     ignores; prompt is the other free-text caller value.
    let req = agent_gen::GenRequest {
        provider: provider.id(),
        model_id: model,
        // Phase 55.1 (D-05): the VIDEO seam. Which of the two video endpoints
        // this becomes is decided downstream by the FRAMES below, never by the
        // model — see `agent_gen::build_submission`.
        modality: agent_gen::RequestModality::Video,
        prompt,
        // Phase 34.1 decision 4: seam-caller default flipped to `Transparent`.
        // No shipping provider reads `background`, so this flip is inert —
        // carried only because it is one of CONTEXT.md's 4 named sites.
        background: agent_gen::BackgroundMode::Transparent,
        // Phase 34.1 (GEN-10): the resolved conditioning reference becomes the
        // FIRST frame (image-to-video, decision 6), produced backend-side by
        // `resolve_reference_image`. `None` is the unconditioned text->video
        // request (the `promptImage` key is omitted entirely, never null).
        reference_image: reference,
        // Quick 260726-t5z, carried across to Runway: the resolved destination
        // becomes the LAST keyframe (`promptImage: [{uri, position:"last"}]`), so
        // with BOTH set the model interpolates a real A->B transition. Same
        // backend-side resolution, same omitted-when-`None` wire shape.
        //
        // Phase 55.1 (D-05): NOTHING local refuses a model that cannot carry a
        // first+last pair any more. The capability resolver used to refuse it
        // here at $0.00, and `build_submission` used to refuse it a second time
        // provider-side; 55.1-01 deleted the second, 55.1-03 unwired the first
        // and 55.1-06 deleted it. A model without keyframe support fails at
        // Runway with
        // Runway's own 400 — the accepted cost of an open roster, and the
        // reason the rulebook's table marks which models bridge a pair.
        destination_image: destination,
        // Phase 56 (GEN-11): this seam re-renders nothing — it has no source
        // clip, so it stays on the byte-identical pre-56 path. `source_video`
        // is the ENDPOINT SELECTOR (`None` => never `video_to_video`), and the
        // v2v conditioning slot is a Plan 06 seam.
        source_video: None,
        reference_images: None,
    };

    // (d) Submit through the ONE gated path (the GEN-08 gate is INSIDE it).
    let started = start_generation_job(ctx, provider, registry.clone(), allow_list, req)
        .await
        .map_err(gen_submit_error_message)?;

    // (e)+(f) The shared tail — see `await_video_generation_outcome`.
    await_video_generation_outcome(ctx, &registry, started).await
}

/// The post-submit tail BOTH video seams run — `generate_runway_video_for_agent`
/// and [`generate_runway_video_edit_for_agent`] (Phase 56 plan 06).
///
/// Extracted rather than copied, on the plan's own instruction: two copies of a
/// poll/cancel/land sequence are two places for a timeout to stop cancelling or
/// for a terminal state to stop landing. **Behaviour-preserving by
/// construction** — the body below is the pre-56 (e)+(f) block moved verbatim,
/// with `registry` taken by reference because the caller still needs its own
/// handle for the cancel latch. The legacy video seam's existing tests staying
/// green is what pins that claim.
///
/// (e) A synchronous provider (the fixture sync path) already landed its ids, so
/// this returns immediately with zero polls. (f) Otherwise AWAIT the poll task,
/// bounded by [`AGENT_VIDEO_WAIT_CAP`], and take the terminal outcome off its
/// return value; on timeout raise the cancel latch and return a clear `Err`,
/// never a hang.
async fn await_video_generation_outcome<C: crate::AppCtx>(
    ctx: &C,
    registry: &std::sync::Arc<agent_gen::JobRegistry>,
    started: StartedJob,
) -> Result<AgentGeneratedVideo, String> {
    if !started.media_item_ids.is_empty() {
        return Ok(AgentGeneratedVideo {
            media_item_ids: started.media_item_ids,
            carries_provenance_watermark: started.carries_provenance_watermark,
        });
    }

    let job_id = started.job_id.clone();
    let carries = started.carries_provenance_watermark;
    let outcome = if let Some(task) = started.task {
        match tokio::time::timeout(AGENT_VIDEO_WAIT_CAP, task).await {
            Err(_) => {
                registry.request_cancel(&job_id);
                Err("the video generation did not finish within 12 minutes; it was \
                     cancelled — try again"
                    .to_string())
            }
            Ok(join) => match join.map_err(|e| format!("the generation poll task failed: {e}"))? {
                agent_gen::JobStatus::Ready(assets) => tokio::task::block_in_place(|| {
                    finalize_ready_job(ctx, &job_id, &started.req, assets, registry, carries)
                }),
                agent_gen::JobStatus::Failed(msg) => Err(msg),
                agent_gen::JobStatus::Cancelled => {
                    Err("the video generation was cancelled".to_string())
                }
                agent_gen::JobStatus::Pending => {
                    Err("the video generation ended in an unknown state".to_string())
                }
            },
        }
    } else {
        // No landed ids and no task — an honest fallback (should be unreachable:
        // a real video submit is always async Pending, and the fixture sync path
        // returned above).
        Err("the generation finished but no asset was landed".to_string())
    };

    outcome.map(|(media_item_ids, carries_provenance_watermark)| AgentGeneratedVideo {
        media_item_ids,
        carries_provenance_watermark,
    })
}

/// **The governed clip-edit submit entry** — Phase 56 plan 06 (GEN-11), the arm
/// that re-renders an existing timeline clip's own pixels.
///
/// Structurally the sibling of `generate_runway_video_for_agent`, sharing its
/// prompt caps, its provider slot, its error vocabulary, its
/// [`start_generation_job`] call and its post-submit tail. **There is no second
/// submit path and no forked policy** (T-56-GEN08-08): this calls the ONE entry
/// the GEN-08 gate lives inside, and calls it once.
///
/// # What routes this to `/v1/video_to_video`
///
/// `source_video` being `Some`, and nothing else. `RunwayProvider::submit`
/// dispatches on exactly that field (56-03), so the endpoint follows the FRAMES
/// the call carries rather than the model id — which is what makes 55.1's
/// free-text roster safe here: `model_id` selects WHICH model, never WHERE the
/// request goes. It cannot name an endpoint, a host or a path (T-56-INJ-04).
///
/// # Model resolution — pass through, or the DERIVED default
///
/// The `Some(..)` branch NEVER inspects, validates or rewrites the caller's id
/// (55.1 decision 2): an off-roster id submits, and its price is disclosed as
/// [`PRICE_UNKNOWN`] rather than guessed. `None` — and a blank string, which
/// [`resolved_model_for_tool_input`] normalizes identically so the disclosure
/// and the bill cannot disagree about the same input (T-42.3-11) — substitutes
/// [`agent_gen::advisory_video_edit_model`], **derived from the roster's
/// `video_to_video` flag, never a retyped `"aleph2"`**.
///
/// This is the half of 56-05's contract that this plan owes: that arm's no-model
/// fallback is honest precisely because a model-less call really does submit
/// that default.
///
/// # The refusals, all of them local and all of them $0.00
///
/// Prompt (empty / over the cap) → empty extracted bytes → the reference gate,
/// every one of them before the provider slot is even resolved. The CLIP-side
/// refusals (unknown id, the D-01 window) happen one layer out, in the host impl
/// that turns a clip id into these bytes, because only the host can see the
/// store.
pub async fn generate_runway_video_edit_for_agent<C: crate::AppCtx>(
    ctx: &C,
    gen: &GenHost<'_>,
    prompt: String,
    model: Option<String>,
    source_video: Vec<u8>,
    references: Vec<agent_gen::ReferenceImage>,
) -> Result<AgentGeneratedVideo, String> {
    use agent_gen::GenProvider;

    // (a) Validate the prompt FIRST — a clear Err before ANY state/provider/
    //     network/paid work. Same caps as the sibling seam: one video prompt
    //     budget, not two.
    if prompt.trim().is_empty() {
        return Err("generate_ai_video_edit requires a non-empty prompt".to_string());
    }
    let prompt_len = prompt.chars().count();
    if prompt_len > MAX_AGENT_VIDEO_PROMPT_CHARS {
        return Err(format!(
            "prompt is too long ({prompt_len} characters); the maximum is \
             {MAX_AGENT_VIDEO_PROMPT_CHARS}"
        ));
    }

    // (a2) An empty extraction would still be `source_video: Some(vec![])`, i.e.
    //      it would still route to the PAID v2v endpoint while carrying nothing
    //      to edit. Refused here at $0.00 rather than discovered as a vendor 400
    //      — and it is a real reachable state, because a caller can hand this
    //      seam whatever its extraction produced.
    if source_video.is_empty() {
        return Err(
            "generate_ai_video_edit received no video data for the clip -- the \
             extracted range was empty, so there is nothing to edit"
                .to_string(),
        );
    }

    // (a3) The reference gate, belt-and-braces beside the host impl's earlier
    //      call. ONE shared message, so a host that skipped its own check gets
    //      the identical refusal rather than a second wording.
    video_edit_reference_check(references.len())?;

    // (b) Resolve the model. The pass-through branch is deliberately inert — no
    //     roster lookup, no capability check, no rewrite (55.1 decision 2). The
    //     fallback is DERIVED from the roster's own `video_to_video` flag; the
    //     `expect` fires only if that row is deleted, which would make plan 05's
    //     pricing helper meaningless too and should be loud rather than silently
    //     submitting an empty model id.
    //
    //     Phase 56 plan 07: this block's body moved to
    //     [`resolved_video_edit_model_id`] BEHAVIOUR-UNCHANGED, because the spend
    //     gate now has to answer the same question one moment earlier in order to
    //     quote a price. Two copies of that normalization would be two answers to
    //     "which model is this call about", and the confirmation naming a
    //     different model from the bill is the one failure T-42.3-13 exists to
    //     prevent.
    let model_id = resolved_video_edit_model_id(model.as_deref())
        .expect("the advisory roster lost its video_to_video entry -- see 56-05");

    // (c) Resolve the provider: the MODALITY-SCOPED video slot — a clip edit is
    //     a video generation, and reusing the image slot here would resolve the
    //     wrong credential.
    let provider = gen
        .video
        .ok_or_else(|| "video generation provider state is not managed".to_string())?
        .resolve_via(gen.stores()?)
        .ok_or_else(|| NO_VIDEO_PROVIDER_CONFIGURED.to_string())?;
    let registry = gen.registry()?;
    let allow_list = gen.allow_list()?;

    // The E-01 line's clip-edit counterpart: sizes and counts only, never the
    // bytes and never the key (T-33-20 / T-14.3-05). "The model never asked" and
    // "the backend dropped it" are different bugs that look identical from the
    // output alone, and this is the one place that can tell them apart.
    eprintln!(
        "[E-01] runway clip edit: model={model_id} sourceVideo={}B references={} \
         (source_video present => /v1/video_to_video)",
        source_video.len(),
        references.len()
    );

    // (d) Build the request. The endpoint selector is the BYTES.
    let req = agent_gen::GenRequest {
        provider: provider.id(),
        model_id,
        modality: agent_gen::RequestModality::Video,
        prompt,
        // Inert on every shipping provider; carried for shape parity.
        background: agent_gen::BackgroundMode::Transparent,
        // The image->video first/last keyframe pair means nothing on this
        // endpoint. Left `None` rather than quietly reused for the edit's
        // conditioning, which would be a different capability wearing the same
        // field.
        reference_image: None,
        destination_image: None,
        // D-02: the TRIM-RESPECTING extracted range — exactly what the timeline
        // clip currently shows, produced backend-side by
        // `crate::extract_clip_range_mp4`. This field IS the endpoint selector.
        source_video: Some(source_video),
        // Empty stays `None` rather than `Some(vec![])`: an empty array is a
        // value the wire layer would have to have an opinion about, and today it
        // has none. (Unreachable while the gate above refuses any non-empty set
        // — the branch exists so 56-09 unblocking references is a change at the
        // gate, not here.)
        reference_images: (!references.is_empty()).then_some(references),
    };

    // (e) Submit through the ONE gated path (the GEN-08 gate is INSIDE it).
    //     Deliberately NOT `provider.submit(..)` — a direct submit call here
    //     would be an ungoverned second path (T-31-18 / T-56-GEN08-08).
    let started = start_generation_job(ctx, provider, registry.clone(), allow_list, req)
        .await
        .map_err(gen_submit_error_message)?;

    // (f) The SAME poll/land tail the sibling seam runs.
    await_video_generation_outcome(ctx, &registry, started).await
}

/// Phase 55.1 (plan 03): the DISCLOSURE half of the open roster.
///
/// T-42.3-11's discipline is what these pin: [`resolved_model_for_tool_input`]
/// must read the SAME `input["model"]` the dispatch path bills, so the "Model
/// that ran (...)" line the user reads can never name a different model from the
/// one Runway charged for. With the roster open, that also means the disclosure
/// must be able to report a model Rudis has never heard of — and must report it
/// as the RAW id rather than inventing a friendly label for it (T-55.1-07).
#[cfg(test)]
mod disclosure_model_gate {
    use super::*;
    use serde_json::json;

    /// An ON-ROSTER id renders label-decorated (the user is reading this), an
    /// OFF-ROSTER id renders as the bare id, and BOTH tools answer the same way —
    /// `generate_ai_image` is no longer the special case that ignored its input
    /// and reported a hardcoded `gen4_image` (D-10).
    #[test]
    fn the_disclosure_reports_exactly_the_model_the_caller_named() {
        for tool in ["generate_ai_image", "generate_ai_video"] {
            // On-roster: the human label plus the id it decorates.
            let resolved = resolved_model_for_tool_input(tool, &json!({ "model": "gen4_turbo" }))
                .unwrap_or_else(|| panic!("{tool}: a named model resolves"));
            assert_eq!(
                resolved,
                format!(
                    "{} (gen4_turbo)",
                    agent_gen::model_label("gen4_turbo").expect("gen4_turbo is on the roster")
                ),
                "{tool}: an on-roster id is label-decorated"
            );

            // Off-roster: the raw id, never an invented label. `kling3.0_pro` is
            // real on Runway and absent from every Rudis table.
            assert_eq!(
                resolved_model_for_tool_input(tool, &json!({ "model": "kling3.0_pro" })).as_deref(),
                Some("kling3.0_pro"),
                "{tool}: an off-roster id renders raw"
            );

            // No model named: nothing to disclose. The seam refuses such a call
            // anyway, so guessing here would be inventing a claim about a
            // generation that never happened.
            assert_eq!(
                resolved_model_for_tool_input(tool, &json!({ "prompt": "x" })),
                None,
                "{tool}: a model-less input discloses nothing"
            );
            assert_eq!(
                resolved_model_for_tool_input(tool, &json!({ "model": "   " })),
                None,
                "{tool}: a blank model discloses nothing"
            );
        }

        // A tool with no model concept at all is still `None` — unchanged.
        assert_eq!(
            resolved_model_for_tool_input("generate_ai_audio", &json!({ "model": "gen4_turbo" })),
            None
        );
    }

    /// **Phase 56 (plan 05): the clip-edit tool discloses the model that ran,
    /// and it is the only arm with a default to fall back to.**
    ///
    /// The failing direction this exists for is the silent one: before this arm,
    /// `generate_ai_video_edit` resolved to `None` and the "Model that ran" line
    /// simply would not have rendered — on the one path that sends the user's own
    /// footage to a third party (T-56-D18-01). A missing disclosure looks like
    /// nothing at all in a transcript, which is why it is pinned rather than
    /// assumed.
    #[test]
    fn the_edit_tool_discloses_the_model_that_ran() {
        const EDIT_TOOL: &str = "generate_ai_video_edit";

        // A NAMED model wins, exactly as on the sibling tools — including an
        // off-roster id, rendered raw rather than given an invented label.
        assert_eq!(
            resolved_model_for_tool_input(EDIT_TOOL, &json!({ "model": "brand-new-model-2027" }))
                .as_deref(),
            Some("brand-new-model-2027"),
            "a user- or agent-named model is what ran, and what is disclosed"
        );

        // No model named => the ADVISORY DEFAULT, derived from the roster rather
        // than typed here, and label-decorated because it is on-roster.
        let default_id =
            agent_gen::advisory_video_edit_model().expect("the roster has a video_to_video row");
        let expected = agent_gen::model_label(default_id)
            .map(|l| format!("{l} ({default_id})"))
            .unwrap_or_else(|| default_id.to_string());
        for input in [json!({ "prompt": "relight his face" }), json!({ "model": "  " })] {
            assert_eq!(
                resolved_model_for_tool_input(EDIT_TOOL, &input).as_deref(),
                Some(expected.as_str()),
                "with no model named, the disclosure names the advisory default \
                 that will actually be submitted — never nothing. If plan 06 makes \
                 `model` REQUIRED on this tool, delete the fallback AND this \
                 expectation together (see the fn's doc): disclosing a model no \
                 call submitted is worse than disclosing none"
            );
        }
        // Non-vacuity: the two branches really are different strings.
        assert_ne!(expected, "brand-new-model-2027");

        // THE ARM IS NARROW. The two pre-56 tools are byte-unchanged by this
        // edit — a model-less input still discloses NOTHING there, because
        // neither has a default and inventing one would be the fabricated claim
        // 55.1-03 deleted.
        for sibling in ["generate_ai_image", "generate_ai_video"] {
            assert_eq!(
                resolved_model_for_tool_input(sibling, &json!({ "prompt": "x" })),
                None,
                "{sibling}: the edit tool's fallback must NOT leak into the seams \
                 that refuse a model-less call"
            );
        }
    }

    /// The disclosure does not consult the retired selection vocabulary. A stale
    /// `shape`/`stage` alongside a real model must not change what is reported —
    /// the dispatch path refuses that input outright, and a disclosure that
    /// quietly honoured it would be describing a call that cannot happen.
    #[test]
    fn the_disclosure_ignores_the_retired_shape_stage_vocabulary() {
        assert_eq!(
            resolved_model_for_tool_input(
                "generate_ai_video",
                &json!({ "model": "seedance2", "shape": "transition", "stage": "draft" })
            )
            .as_deref(),
            Some(
                agent_gen::model_label("seedance2")
                    .map(|l| format!("{l} (seedance2)"))
                    .unwrap_or_else(|| "seedance2".to_string())
            )
            .as_deref()
        );
    }
}

/// Phase 55.1 (D-01/D-08), plan 04 Task 1: **the confirmation names a PRICE.**
///
/// With an open roster (55.1-02/03) any Runway model id can be submitted,
/// including ids that do not exist yet at prices nobody has seen. The spend
/// confirmation is the last control before billed egress, so what it says about
/// cost has to be true in both directions: the roster's own line for a model it
/// prices, and the LITERAL words "price unknown" for one it cannot — never a
/// fabricated, rounded or interpolated number.
#[cfg(test)]
mod spend_gate_cost_dimension {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // These three are now ONE-LINE FORWARDS to production, and that is the whole
    // record of how they got here (the 55.1-03 discipline applied to a SIGNATURE
    // change). In the RED commit `69d6d844` each was written against the tree
    // **as it was** — `gate` dropped its cost argument, the other two answered
    // "<does not exist yet>" — so all three failures were ASSERTION failures on
    // a tree that still built, not a compile error, which proves nothing about
    // behavior. GREEN rewrote these three bodies and NOT ONE ASSERTION MOVED.
    // -----------------------------------------------------------------------

    fn gate(
        tool: &str,
        model: Option<&str>,
        cost: Option<&str>,
        approved: bool,
    ) -> crate::SpendGateDecision {
        spend_confirmation_gate(tool, model, cost, approved)
    }

    fn cost_for_model(model_id: &str) -> &'static str {
        cost_signal_for_model(model_id)
    }

    fn cost_for_tool(tool: &str, input: &serde_json::Value) -> Option<&'static str> {
        cost_signal_for_tool_input(tool, input)
    }

    fn question_of(d: crate::SpendGateDecision) -> String {
        match d {
            crate::SpendGateDecision::NeedsConfirmation { question } => question,
            crate::SpendGateDecision::Proceed => {
                panic!("an UNAPPROVED paid call must never Proceed — this is the money gate")
            }
        }
    }

    /// The roster's own price line for a model, read from the table itself so a
    /// roster edit can never be mistaken for a code defect (and so no dollar
    /// literal is ever hardcoded in a test).
    fn table_signal(model_id: &str) -> &'static str {
        agent_gen::model_caps(model_id)
            .unwrap_or_else(|| panic!("{model_id} is on the advisory roster"))
            .rough_cost_signal
            .unwrap_or_else(|| panic!("{model_id}'s roster row carries a price"))
    }

    /// **The honest-absence half.** `kling3.0_pro` is real on Runway and absent
    /// from every Rudis table, so the advisory roster cannot price it. The user
    /// is told exactly that, in the literal words — and NO dollar figure is
    /// invented to fill the gap.
    #[test]
    fn spend_confirmation_gate_names_price_unknown_for_an_off_roster_model() {
        assert_eq!(
            cost_for_model("kling3.0_pro"),
            "price unknown",
            "a model the roster cannot price yields the LITERAL words, never a guess"
        );

        let q = question_of(gate(
            "generate_ai_video",
            Some("kling3.0_pro"),
            Some(cost_for_model("kling3.0_pro")),
            false,
        ));
        assert!(
            q.contains("kling3.0_pro"),
            "the question names the model that would be billed: {q}"
        );
        assert!(
            q.contains("price unknown"),
            "T-55.1-08: the question states plainly that the price is unknown: {q}"
        );
        assert!(q.contains("PAID call"), "still a spend question: {q}");
        assert!(
            !q.contains('$'),
            "no fabricated number may stand in for an unknown price: {q}"
        );
    }

    /// **The real-price half.** Asserted by EQUALITY against the roster row, so
    /// the prose the user reads and the table it comes from cannot drift
    /// (T-55.1-09). No dollar literal is written anywhere in this module — the
    /// only place a price string exists is the roster row itself.
    #[test]
    fn spend_confirmation_gate_names_the_roster_cost_for_a_known_model() {
        let expected = table_signal("gen4_turbo");
        assert!(
            expected.contains('$'),
            "non-vacuity: the roster row really does carry a price: {expected}"
        );

        let q = question_of(gate(
            "generate_ai_video",
            Some("gen4_turbo"),
            Some(cost_for_model("gen4_turbo")),
            false,
        ));
        assert!(
            q.contains(expected),
            "the question carries the roster's OWN price line verbatim ({expected}): {q}"
        );
        assert!(q.contains("gen4_turbo"), "the model is named too: {q}");
        assert!(
            !q.contains("price unknown"),
            "a priced model must not ALSO claim its price is unknown: {q}"
        );
    }

    /// The cost argument is derived from the SAME `input["model"]` the dispatch
    /// bills (T-55.1-09) — one key, two readers — and exists for exactly the two
    /// Runway tools. A missing, blank or unknown model is "price unknown", never
    /// silence.
    #[test]
    fn cost_signal_for_tool_input_covers_both_runway_tools_and_no_others() {
        assert_eq!(
            cost_for_tool("generate_ai_video", &json!({ "model": "seedance2" })),
            Some(table_signal("seedance2")),
            "an on-roster video model gets the table's line"
        );
        assert_eq!(
            cost_for_tool("generate_ai_image", &json!({ "model": "gen4_image" })),
            Some(table_signal("gen4_image")),
            "the IMAGE tool is not the exception it used to be (D-10)"
        );
        for absent in [
            json!({ "model": "kling3.0_pro" }),
            json!({ "model": "totally-made-up-model" }),
            json!({ "prompt": "no model at all" }),
            json!({ "model": "   " }),
        ] {
            assert_eq!(
                cost_for_tool("generate_ai_video", &absent),
                Some("price unknown"),
                "off-roster/absent/blank all price the same honest way: {absent}"
            );
        }
        assert_eq!(
            cost_for_tool("generate_ai_audio", &json!({ "model": "gen4_turbo" })),
            None,
            "audio has no model-cost concept — its question is unchanged"
        );
        assert_eq!(
            cost_for_tool("split_clip", &json!({})),
            None,
            "an ordinary edit tool is not a spend at all"
        );
    }

    /// The `approved == true` short-circuit is untouched by the cost dimension:
    /// it still returns `Proceed` without building any question at all.
    #[test]
    fn the_approved_short_circuit_is_unchanged_by_the_cost_dimension() {
        for (model, cost) in [
            (Some("seedance2"), Some(cost_for_model("seedance2"))),
            (Some("kling3.0_pro"), Some(cost_for_model("kling3.0_pro"))),
            (None, None),
        ] {
            assert!(
                matches!(
                    gate("generate_ai_video", model, cost, true),
                    crate::SpendGateDecision::Proceed
                ),
                "real approval evidence still proceeds regardless of the cost line"
            );
        }
    }

    /// **A regression guard, not a target** — this passes BEFORE the change and
    /// must still pass after it (the discipline 55.1-02 established for the
    /// ElevenLabs D-03 guard). `generate_ai_audio` carries no model and no cost,
    /// so its question must be byte-identical to the pre-55.1 text.
    #[test]
    fn the_audio_question_is_byte_unchanged_by_the_cost_dimension() {
        assert_eq!(
            question_of(gate("generate_ai_audio", None, None, false)),
            "generate_ai_audio is a PAID call to an external provider, billed to your own \
             account. Reply to confirm before I spend -- say yes to proceed, or tell me what \
             to change."
        );
    }

    // -----------------------------------------------------------------------
    // Phase 56 plan 07: D-56-05-01, and the golden that guards the other paths
    // -----------------------------------------------------------------------

    /// **D-56-05-01, and it is a MONEY bug rather than a formatting one.**
    ///
    /// `cost_signal_for_tool_input` is a per-tool match with no clip-edit arm,
    /// so before this plan the edit tool would have reached the gate as
    /// `(Some(model), None)` — the branch that renders `" (model: X)"` and
    /// **neither a price nor the words "price unknown"**. Silence, not honest
    /// absence, on a path that reaches **$8.40 in one call**.
    ///
    /// The property asserted is the exact one 56-05 wrote down: **a `$` figure
    /// OR the `PRICE_UNKNOWN` literal, never neither** — and never both, because
    /// a question carrying a number AND "we cannot price this" would be worse
    /// than either. Asserted over the whole matrix that can actually occur, and
    /// against the SAME `PRICE_UNKNOWN` const 55.1-04 ships rather than a
    /// re-typed copy.
    #[test]
    fn the_video_edit_question_carries_a_price_or_says_it_is_unknown_never_neither() {
        let advisory = agent_gen::advisory_video_edit_model().expect("a v2v row exists");
        // The clip range decides the figure, so the cases differ by RANGE as
        // well as by model. 4s, and the window maximum.
        let four_s = 4_000_000i64;
        let max_s = i64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_SECONDS) * 1_000_000;

        let cases: Vec<(&str, Option<&str>, Option<i64>)> = vec![
            ("the advisory default, 4s", Some(advisory), Some(four_s)),
            ("the advisory default, window max", Some(advisory), Some(max_s)),
            // 55.1's open roster: a model Runway may ship tomorrow.
            ("an off-roster id", Some("brand-new-model-2027"), Some(four_s)),
            // On-roster, priced — but NOT on a per-INPUT-second basis, so
            // quoting its flat 4s figure for a 30s edit would under-quote.
            ("an on-roster wrong-axis id", Some("gen4_turbo"), Some(four_s)),
            // The clip id names nothing on the timeline: no range to meter.
            ("an unknown clip", Some(advisory), None),
            // The roster lost its v2v row (the `expect` case, defensively).
            ("no model at all", None, Some(four_s)),
        ];

        for (label, model, visible_us) in cases {
            let cost = video_edit_cost_signal(model, visible_us);
            let q = question_of(gate(
                "generate_ai_video_edit",
                Some("some model label"),
                Some(cost.as_str()),
                false,
            ));
            let has_figure = q.contains('$');
            let says_unknown = q.contains(PRICE_UNKNOWN);
            assert!(
                has_figure ^ says_unknown,
                "{label}: the spend question must carry a `$` figure OR the literal \
                 `{PRICE_UNKNOWN}`, exactly one of the two and never neither \
                 (D-56-05-01): {q}"
            );
            assert!(q.contains("PAID call"), "{label}: still a spend question: {q}");
        }

        // Non-vacuity, both directions: the priced case really produces a
        // figure, and it is DERIVED from the estimate rather than a literal
        // written here.
        let priced = video_edit_cost_signal(Some(advisory), Some(max_s));
        let cents =
            estimated_video_edit_cost_cents(advisory, max_s).expect("the advisory model is priced");
        assert!(
            priced.contains(&format!("${}.{:02}", cents / 100, cents % 100)),
            "the quoted figure must be the estimate's own arithmetic: {priced}"
        );
        assert!(
            !priced.contains(PRICE_UNKNOWN),
            "a priced clip must not also claim its price is unknown: {priced}"
        );
        // ...and the figure moves with the RANGE, which is the whole reason the
        // roster's flat 4-second number could not have been reused: a 4-second
        // sample under-quotes a window-maximum edit by 7.5x.
        let short = video_edit_cost_signal(Some(advisory), Some(four_s));
        assert_ne!(
            short, priced,
            "the estimate must scale with the input length, or it is measuring nothing"
        );
        assert!(
            priced.contains(&format!("{:.1}s", max_s as f64 / 1_000_000.0)),
            "the question shows the RANGE it priced, so the user can check it: {priced}"
        );
    }

    /// The ONE normalization, asserted at all four inputs it must agree on —
    /// this is what stops the confirmation naming one model while another is
    /// billed (T-42.3-13), now that the price is computed one moment before the
    /// submission resolves the same question.
    ///
    /// A BLANK id is ABSENT rather than malformed, matching 56-06's seam exactly:
    /// on this tool `model` is OPTIONAL, so blank means unnamed (T-42.3-11).
    #[test]
    fn resolved_video_edit_model_id_is_the_one_normalization() {
        let advisory = agent_gen::advisory_video_edit_model().expect("a v2v row exists");
        assert_eq!(
            resolved_video_edit_model_id(None).as_deref(),
            Some(advisory),
            "an unnamed model resolves to the DERIVED advisory default"
        );
        assert_eq!(
            resolved_video_edit_model_id(Some("   ")).as_deref(),
            Some(advisory),
            "blank means unnamed here, not malformed"
        );
        assert_eq!(
            resolved_video_edit_model_id(Some("brand-new-model-2027")).as_deref(),
            Some("brand-new-model-2027"),
            "an off-roster id passes through byte-identically — no roster rejection exists"
        );
        assert_eq!(
            resolved_video_edit_model_id(Some("  gen4_turbo  ")).as_deref(),
            Some("gen4_turbo"),
            "surrounding whitespace is trimmed, exactly as the seam trims it"
        );

        // And the DISCLOSURE resolves to the same id it was priced for: the
        // "Model that ran" line embeds it, so the two cannot name different
        // models for the same call.
        for named in [None, Some("brand-new-model-2027")] {
            let input = match named {
                Some(m) => json!({ "prompt": "p", "clipId": "c1", "model": m }),
                None => json!({ "prompt": "p", "clipId": "c1" }),
            };
            let priced = resolved_video_edit_model_id(named).expect("resolves");
            let disclosed = resolved_model_for_tool_input("generate_ai_video_edit", &input)
                .expect("the edit arm discloses a model");
            assert!(
                disclosed.contains(&priced),
                "the disclosed model ({disclosed}) must be the one the price was computed \
                 for ({priced})"
            );
        }
    }

    /// **The golden.** Phase 56 changed WHAT feeds the gate's cost dimension for
    /// one new tool; it changed nothing about the gate itself, and this is the
    /// assertion that says so in bytes rather than in a commit message.
    ///
    /// Every pre-existing call path's question text is pinned verbatim. If a
    /// later plan is tempted to add a second cost parameter, or to move the
    /// price out of the `(model: …)` parenthetical, three shipped surfaces move
    /// with it and this test names them.
    #[test]
    fn every_pre_existing_spend_question_is_byte_identical_after_the_extension() {
        assert_eq!(
            question_of(gate(
                "generate_ai_video",
                Some("gen4_turbo"),
                cost_for_tool("generate_ai_video", &json!({ "model": "gen4_turbo" })),
                false
            )),
            format!(
                "generate_ai_video is a PAID call to an external provider (model: gen4_turbo, \
                 {}), billed to your own account. Reply to confirm before I spend -- say yes \
                 to proceed, or tell me what to change.",
                table_signal("gen4_turbo")
            )
        );
        assert_eq!(
            question_of(gate(
                "generate_ai_image",
                Some("gen4_image"),
                cost_for_tool("generate_ai_image", &json!({ "model": "gen4_image" })),
                false
            )),
            format!(
                "generate_ai_image is a PAID call to an external provider (model: gen4_image, \
                 {}), billed to your own account. Reply to confirm before I spend -- say yes \
                 to proceed, or tell me what to change.",
                table_signal("gen4_image")
            )
        );
        assert_eq!(
            question_of(gate(
                "generate_ai_video",
                Some("kling3.0_pro"),
                cost_for_tool("generate_ai_video", &json!({ "model": "kling3.0_pro" })),
                false
            )),
            "generate_ai_video is a PAID call to an external provider (model: kling3.0_pro, \
             price unknown), billed to your own account. Reply to confirm before I spend -- \
             say yes to proceed, or tell me what to change."
        );
        // Audio, which has no model and no cost at all, is pinned by
        // `the_audio_question_is_byte_unchanged_by_the_cost_dimension` above.
    }
}

/// Phase 55.1 (D-12), plan 04 Task 2: **WR-04 / Backlog § 999.8, closed
/// structurally.**
///
/// The claim under test is not "the direct-submit path returns an error when
/// unapproved" — an `Err` return cannot be told apart from "submitted, then the
/// result was thrown away", and on a PAID provider those two differ by real
/// money. The claim is that the gate runs BEFORE any provider work at all, and
/// it is proven three independent ways:
///
/// 1. **By call count** — `FixtureGenProvider::submit_calls()` reads 0 after an
///    unapproved call and 1 after an approved one, so the zero is not vacuous.
///    Same discipline as `agent_gen`'s own `allow_list_rejects_before_submit`.
/// 2. **By ordering** — with a host bundle whose slots are ALL `None`, the
///    unapproved call still returns the spend question rather than
///    `"… state is not managed"`. A gate placed after provider resolution could
///    not produce that. Its non-vacuity is built in: the same call WITH consent
///    returns exactly the "not managed" error, because resolving the slot is
///    literally the next thing the body does.
/// 3. **By absence of trace** — zero job-registry rows and zero `gen:job`
///    events, the same inertness assertion the ffi tier makes about the store.
#[cfg(test)]
mod wr04_direct_submit_gate {
    use super::*;
    use crate::test_support::TestAppCtx;
    use crate::AppCtx as _;
    use std::sync::Arc;

    // In the RED commit `d14010e7` a local stub shadowed the (then absent)
    // production name and answered "<does not exist yet>", so all five failures
    // below were ASSERTION failures on a tree that still built. GREEN deleted
    // the stub; `use super::*` now resolves to the real function and NOT ONE
    // ASSERTION MOVED.

    /// Real on Runway, in no Rudis table — the unbounded-price case an open
    /// roster makes reachable, and therefore the one the gate must describe.
    const OFF_ROSTER: &str = "kling3.0_pro";

    /// `Result::expect_err` needs `T: Debug` and `StartedJob` holds a
    /// `JoinHandle`, so unwrap the refusal by hand. A surviving `Ok` here is
    /// always the same failure — a spend that was NOT refused.
    fn refusal(r: Result<StartedJob, String>, why: &str) -> String {
        match r {
            Err(e) => e,
            Ok(_) => panic!("{why} — but the submit went through"),
        }
    }

    /// A counting fixture provider plus a probe handle that SHARES its counter.
    ///
    /// `async_clip`, not `sync_still`, on purpose: the async arm makes
    /// `start_generation_job` return at the spawn, landing nothing — so no file
    /// is written, no `ffprobe` process is spawned and this suite stays in the
    /// default (no-decoder) tier while still exercising a REAL submit.
    fn counting_provider() -> (
        agent_gen::FixtureGenProvider,
        Arc<agent_gen::ConcreteGenProvider>,
    ) {
        let fx = agent_gen::FixtureGenProvider::async_clip(vec![0u8; 8], "mp4", 0);
        let probe = fx.clone();
        (probe, Arc::new(agent_gen::ConcreteGenProvider::Fixture(fx)))
    }

    /// The wired host bundle's owned parts. `GenHost` borrows, so the owner has
    /// to outlive it — this keeps every test's setup to two lines.
    struct Slots {
        image: ManagedGenProvider,
        video: ManagedVideoGenProvider,
        audio: ManagedAudioGenProvider,
        jobs: ManagedGenJobs,
        allow: ManagedAllowList,
    }

    impl Slots {
        fn wired(provider: Arc<agent_gen::ConcreteGenProvider>) -> Self {
            Self {
                image: ManagedGenProvider::preset(Some(provider.clone())),
                video: ManagedVideoGenProvider::preset(Some(provider)),
                audio: ManagedAudioGenProvider::preset(None),
                jobs: ManagedGenJobs::default(),
                // The PRODUCTION list: 2 fixture rows + 1 ElevenLabs row, and no
                // Runway row at all (55.1-02). Not a bespoke permissive list —
                // the GEN-08 gate stays exactly as production has it.
                allow: ManagedAllowList::default(),
            }
        }

        fn host(&self) -> GenHost<'_> {
            GenHost {
                image: Some(&self.image),
                video: Some(&self.video),
                audio: Some(&self.audio),
                jobs: Some(&self.jobs),
                allow_list: Some(&self.allow),
                stores: Some(test_provider_stores()),
            }
        }
    }

    /// **THE WR-04 proof.** An unapproved direct submit of an off-roster,
    /// unpriced model returns the gate's own question and provably does ZERO
    /// provider work.
    #[test]
    fn submit_generation_job_inner_consults_the_spend_gate_before_any_provider_work() {
        let ctx = TestAppCtx::new();
        let (probe, provider) = counting_provider();
        let slots = Slots::wired(provider);
        let gen = slots.host();

        let err = refusal(
            ctx.block_on(submit_generation_job_inner(
                &ctx,
                &gen,
                OFF_ROSTER.to_string(),
                agent_gen::RequestModality::Video,
                "a slow drone push over a canyon".to_string(),
                false,
            )),
            "fail-closed: no consent, no spend",
        );

        assert!(err.contains("PAID call"), "the refusal IS the question: {err}");
        assert!(err.contains(OFF_ROSTER), "it names the model: {err}");
        assert!(
            err.contains("price unknown"),
            "T-55.1-08: and states plainly that the price is unknown: {err}"
        );

        assert_eq!(
            probe.submit_calls(),
            0,
            "THE proof: the provider's submit was NEVER entered. An Err alone \
             could mean 'submitted, then discarded' — on a paid provider that is \
             the difference between $0 and a real charge."
        );
        assert_eq!(slots.jobs.0.len(), 0, "no job row was inserted: {err}");
        assert!(
            ctx.gen_events().is_empty(),
            "and nothing was announced to the host"
        );
    }

    /// The positive control that makes the zero above mean something, and the
    /// "click IS consent" precedent in code: with the caller's evidence of a
    /// real user gesture, the SAME call runs.
    #[test]
    fn submit_generation_job_inner_click_is_consent_proceeds() {
        let ctx = TestAppCtx::new();
        let (probe, provider) = counting_provider();
        let slots = Slots::wired(provider);
        let gen = slots.host();

        let started = ctx
            .block_on(async {
                let r = submit_generation_job_inner(
                    &ctx,
                    &gen,
                    "fixture-video".to_string(),
                    agent_gen::RequestModality::Video,
                    "a slow drone push over a canyon".to_string(),
                    true,
                )
                .await;
                // Detach the spawned poll task immediately: this test is about
                // the submit, not the lifecycle.
                if let Ok(s) = &r {
                    if let Some(t) = &s.task {
                        t.abort();
                    }
                }
                r
            })
            .expect("the click IS the consent — an approved direct submit runs");

        assert_eq!(probe.submit_calls(), 1, "the provider was reached exactly once");
        assert!(
            slots.jobs.0.get_status(&started.job_id).is_some(),
            "the job registry holds the row"
        );
        assert_eq!(
            started.req.model_id, "fixture-video",
            "the CALLER's model id is what was submitted, verbatim"
        );
        assert_eq!(
            started.req.modality,
            agent_gen::RequestModality::Video,
            "and the caller's modality chose the endpoint family (D-05)"
        );
    }

    /// The priced half of the same gate, on the direct-submit surface: an
    /// on-roster model names the roster's own line, asserted by equality against
    /// the table so prose and roster cannot drift (T-55.1-09).
    #[test]
    fn submit_generation_job_inner_names_the_roster_price_when_known() {
        let ctx = TestAppCtx::new();
        let (probe, provider) = counting_provider();
        let slots = Slots::wired(provider);
        let gen = slots.host();

        let expected = agent_gen::model_caps("gen4_turbo")
            .expect("gen4_turbo is on the advisory roster")
            .rough_cost_signal
            .expect("its row carries a price");

        let err = refusal(
            ctx.block_on(submit_generation_job_inner(
                &ctx,
                &gen,
                "gen4_turbo".to_string(),
                agent_gen::RequestModality::Video,
                "a slow drone push over a canyon".to_string(),
                false,
            )),
            "fail-closed: no consent, no spend",
        );

        assert!(
            err.contains(expected),
            "the roster's OWN price line ({expected}) is what the user is asked \
             to consent to: {err}"
        );
        assert!(
            !err.contains("price unknown"),
            "a priced model must not also claim its price is unknown: {err}"
        );
        assert_eq!(probe.submit_calls(), 0, "still no provider work");
    }

    /// **The ordering proof.** With NO host slots wired at all, an unapproved
    /// call still produces the spend question — which a gate placed after
    /// provider/registry/allow-list resolution could not do. The second half is
    /// what makes it non-vacuous: WITH consent, the very next thing the body
    /// does is resolve the slot, and it fails exactly there.
    #[test]
    fn the_gate_runs_before_the_host_bundle_is_consulted_at_all() {
        let ctx = TestAppCtx::new();
        let gen = GenHost {
            image: None,
            video: None,
            audio: None,
            jobs: None,
            allow_list: None,
            stores: None,
        };

        let refused = refusal(
            ctx.block_on(submit_generation_job_inner(
                &ctx,
                &gen,
                OFF_ROSTER.to_string(),
                agent_gen::RequestModality::Video,
                "a slow drone push over a canyon".to_string(),
                false,
            )),
            "unapproved",
        );
        assert!(
            refused.contains("PAID call") && refused.contains("price unknown"),
            "the GATE answered, on a host with nothing wired: {refused}"
        );
        assert!(
            !refused.contains("not managed"),
            "…and it answered FIRST — no slot was touched to produce it: {refused}"
        );

        let then_resolved = refusal(
            ctx.block_on(submit_generation_job_inner(
                &ctx,
                &gen,
                OFF_ROSTER.to_string(),
                agent_gen::RequestModality::Video,
                "a slow drone push over a canyon".to_string(),
                true,
            )),
            "consent does not conjure a provider",
        );
        assert!(
            then_resolved.contains("not managed"),
            "non-vacuity: past the gate, resolving the slot IS the next act: \
             {then_resolved}"
        );
    }

    /// A malformed request is a $0 LOCAL refusal, not a wasted round trip —
    /// the same two checks both agent seams carry. They sit AFTER the gate on
    /// purpose (both are pure and spend nothing, and an exception list is how a
    /// "first act" stops being first), so `approved = true` here proves the
    /// refusal is theirs and not the gate's.
    #[test]
    fn a_malformed_direct_submit_is_refused_locally_at_zero_cost() {
        let ctx = TestAppCtx::new();
        let (probe, provider) = counting_provider();
        let slots = Slots::wired(provider);
        let gen = slots.host();

        let blank_model = refusal(
            ctx.block_on(submit_generation_job_inner(
                &ctx,
                &gen,
                "   ".to_string(),
                agent_gen::RequestModality::Video,
                "a slow drone push over a canyon".to_string(),
                true,
            )),
            "a blank model id is not a submittable request",
        );
        assert!(
            blank_model.contains("requires a model"),
            "…and it says which field: {blank_model}"
        );

        let blank_prompt = refusal(
            ctx.block_on(submit_generation_job_inner(
                &ctx,
                &gen,
                "fixture-video".to_string(),
                agent_gen::RequestModality::Video,
                "  ".to_string(),
                true,
            )),
            "a blank prompt is not a submittable request",
        );
        assert!(
            blank_prompt.contains("non-empty prompt"),
            "…and it says which field: {blank_prompt}"
        );

        assert_eq!(
            probe.submit_calls(),
            0,
            "neither malformed call reached the provider"
        );
    }

    /// Audio is refused BY NAME, not by fallthrough — and the refusal points at
    /// the tool that does own the modality, so a caller on stale context can
    /// correct itself instead of failing identically forever (the pattern
    /// 55.1-03 established for retired vocabulary).
    #[test]
    fn a_direct_audio_submission_is_refused_by_name_not_by_fallthrough() {
        let ctx = TestAppCtx::new();
        let (probe, provider) = counting_provider();
        let slots = Slots::wired(provider);
        let gen = slots.host();

        // approved = true, so this refusal provably is NOT the spend gate's.
        let err = refusal(
            ctx.block_on(submit_generation_job_inner(
                &ctx,
                &gen,
                agent_gen::ELEVENLABS_TTS_MODEL.to_string(),
                agent_gen::RequestModality::Audio,
                "read this aloud".to_string(),
                true,
            )),
            "audio has no direct-submit arm",
        );
        assert!(
            err.contains("generate_ai_audio"),
            "the refusal names the surface that DOES own audio: {err}"
        );
        assert!(!err.contains("PAID call"), "not a gate refusal: {err}");
        assert_eq!(probe.submit_calls(), 0, "and nothing was submitted");
    }
}

/// Phase 56 plan 05 (GEN-11 / D-04, T-56-INJ-03) — **the legacy video seam
/// structurally cannot reach the clip-edit endpoint or its spend tier.**
///
/// Post-55.1 there is no roster, no allow-list row and no capability gate left
/// to keep these two apart, and D-04's separation is not enforced by any text
/// the agent writes. What enforces it is what the call CARRIES: `RunwayProvider`
/// dispatches to `/v1/video_to_video` on `GenRequest::source_video` being
/// `Some` and on nothing else (56-03), so the old text/image->video seam is
/// unable to reach that endpoint exactly as long as it cannot put bytes in that
/// field. This suite proves it cannot, at the one place it matters — the
/// request that crossed the provider boundary.
#[cfg(test)]
mod legacy_video_path_cannot_reach_the_edit_endpoint {
    use super::*;
    use crate::test_support::TestAppCtx;
    use crate::AppCtx as _;
    use std::sync::Arc;

    /// The provider plus a probe handle SHARING its capture slot.
    ///
    /// `sync_still` with a DELIBERATELY DISALLOWED extension: the submit is
    /// real and the request is really captured, then `land_generated_asset`
    /// refuses `"bin"` against `ALLOWED_GEN_EXTS` *before* it creates a
    /// directory, writes a byte or spawns `ffprobe`. So this test exercises the
    /// whole production path down to the wire and still touches no disk and no
    /// sidecar — it stays in the default no-decoder tier, and no
    /// `RUDIS_FFMPEG_DIR` pin is needed because no ffmpeg binary can be reached
    /// from here at all.
    fn capturing_provider() -> (
        agent_gen::FixtureGenProvider,
        Arc<agent_gen::ConcreteGenProvider>,
    ) {
        let fx = agent_gen::FixtureGenProvider::sync_still(vec![0u8; 8], "bin");
        let probe = fx.clone();
        (probe, Arc::new(agent_gen::ConcreteGenProvider::Fixture(fx)))
    }

    struct Slots {
        image: ManagedGenProvider,
        video: ManagedVideoGenProvider,
        audio: ManagedAudioGenProvider,
        jobs: ManagedGenJobs,
        allow: ManagedAllowList,
    }

    impl Slots {
        fn wired(provider: Arc<agent_gen::ConcreteGenProvider>) -> Self {
            Self {
                image: ManagedGenProvider::preset(Some(provider.clone())),
                video: ManagedVideoGenProvider::preset(Some(provider)),
                audio: ManagedAudioGenProvider::preset(None),
                jobs: ManagedGenJobs::default(),
                // The PRODUCTION allow list, not a permissive bespoke one.
                allow: ManagedAllowList::default(),
            }
        }

        fn host(&self) -> GenHost<'_> {
            GenHost {
                image: Some(&self.image),
                video: Some(&self.video),
                audio: Some(&self.audio),
                jobs: Some(&self.jobs),
                allow_list: Some(&self.allow),
                stores: Some(test_provider_stores()),
            }
        }
    }

    /// A real `ReferenceImage`, so the frame-carrying arms are exercised with
    /// something rather than skipped as `None`.
    fn frame() -> agent_gen::ReferenceImage {
        agent_gen::ReferenceImage {
            bytes: vec![1u8, 2, 3, 4],
            width: 2,
            height: 2,
        }
    }

    /// **THE D-04 PIN.** Whatever the legacy seam is asked for — text-only, a
    /// first frame, or a first+last pair — the request it puts on the wire
    /// carries NO source video and NO v2v reference set. Since the endpoint
    /// follows exactly those bytes, the old tool cannot reach `video_to_video`
    /// or be billed at its per-input-second tier.
    ///
    /// Asserted on the CAPTURED request — the one production built and handed to
    /// `submit` — never on a literal this test wrote. A constructed literal
    /// would assert only that the test author remembered to write `None`.
    #[test]
    fn the_legacy_video_path_never_carries_a_source_video() {
        // Every frame shape the seam accepts: text-only, image-to-video, and
        // the A->B keyframe pair.
        let shapes: [(&str, Option<agent_gen::ReferenceImage>, Option<agent_gen::ReferenceImage>); 3] = [
            ("text-only", None, None),
            ("image-to-video", Some(frame()), None),
            ("first+last pair", Some(frame()), Some(frame())),
        ];

        for (label, reference, destination) in shapes {
            let ctx = TestAppCtx::new();
            let (probe, provider) = capturing_provider();
            let slots = Slots::wired(provider);
            let gen = slots.host();

            // The landing fails on the disallowed extension; that is expected
            // and irrelevant. The claim is about what was SUBMITTED, and the
            // submit provably happened.
            let _ = ctx.block_on(generate_runway_video_for_agent(
                &ctx,
                &gen,
                "a slow drone push over a canyon".to_string(),
                "fixture-video".to_string(),
                reference,
                destination,
            ));

            assert_eq!(
                probe.submit_calls(),
                1,
                "{label}: NON-VACUITY — the seam must really have reached the \
                 provider, or the assertions below are about a request that was \
                 never built"
            );
            let req = probe
                .last_request()
                .unwrap_or_else(|| panic!("{label}: a submitted request was captured"));

            assert_eq!(
                req.source_video, None,
                "{label}: T-56-INJ-03 — the legacy seam put a SOURCE VIDEO on the \
                 wire. `RunwayProvider::submit` dispatches to /v1/video_to_video \
                 on exactly this field, so a Some here silently routes the old \
                 tool to the clip-edit endpoint and its per-input-second bill. \
                 The edit path is plan 06's OWN seam; it is not a mode of this one"
            );
            assert_eq!(
                req.reference_images, None,
                "{label}: and the v2v conditioning slot stays empty here — it is \
                 refused rather than shipped because no probe has named the field \
                 (56-03; F-1b then enumerated both candidates and crowned \
                 neither, leaving 56-09 as the only route), so a value in it \
                 could only be a guess"
            );
            // The two pre-56 frame slots are untouched by this property: the
            // request is a NORMAL video request, not a hollowed-out one. Without
            // this the pin would also pass on a seam that had stopped working.
            assert_eq!(
                req.reference_image.is_some(),
                label != "text-only",
                "{label}: the legacy frame slots still carry what was asked for"
            );
            assert_eq!(
                req.destination_image.is_some(),
                label == "first+last pair",
                "{label}: including the A->B destination frame"
            );
            assert_eq!(req.modality, agent_gen::RequestModality::Video);
        }
    }
}

/// Phase 56 plan 06 (GEN-11) — **the governed clip-edit entry, proven on the
/// request that crossed the provider boundary.**
///
/// Two properties, and both are about a REQUEST rather than a return value:
/// 55.1's open-selection contract (pass through what was named, default to the
/// DERIVED advisory entry, validate nothing) and the single-submit-path
/// guarantee that replaced the allow-list row 55.1-02 deleted.
#[cfg(test)]
mod video_edit_governed_path {
    use super::*;
    use crate::test_support::TestAppCtx;
    use crate::AppCtx as _;
    use std::sync::Arc;

    /// The provider plus a probe handle SHARING its capture slot — the same
    /// recipe the D-04 pin uses, and for the same reason: `sync_still` with a
    /// DELIBERATELY DISALLOWED extension means the submit is real and the
    /// request is really captured, then `land_generated_asset` refuses `"bin"`
    /// against [`ALLOWED_GEN_EXTS`] *before* it creates a directory, writes a
    /// byte or spawns `ffprobe`. Zero disk, zero sidecar, zero network — no
    /// `RUDIS_FFMPEG_DIR` pin is needed because no ffmpeg binary is reachable
    /// from here at all.
    fn capturing_provider() -> (
        agent_gen::FixtureGenProvider,
        Arc<agent_gen::ConcreteGenProvider>,
    ) {
        let fx = agent_gen::FixtureGenProvider::sync_still(vec![0u8; 8], "bin");
        let probe = fx.clone();
        (probe, Arc::new(agent_gen::ConcreteGenProvider::Fixture(fx)))
    }

    struct Slots {
        image: ManagedGenProvider,
        video: ManagedVideoGenProvider,
        audio: ManagedAudioGenProvider,
        jobs: ManagedGenJobs,
        allow: ManagedAllowList,
    }

    impl Slots {
        /// Wired with an allow list admitting EXACTLY the ids under test.
        ///
        /// The fixture provider is deliberately still GATED — only `runway` is
        /// in `agent_gen::allow_list::UNGATED_PROVIDERS` (55.1-02) — which is
        /// what makes this useful rather than merely permissive: a green
        /// `submit_calls() == 1` here proves the gate was handed the id this
        /// list names, i.e. **the string the seam actually submitted**. The
        /// production list is not used because it has no row for the advisory
        /// v2v model, and adding one would be a GEN-08 clearance rather than a
        /// test fixture.
        fn admitting(provider: Arc<agent_gen::ConcreteGenProvider>, models: &[&str]) -> Self {
            Self {
                image: ManagedGenProvider::preset(Some(provider.clone())),
                video: ManagedVideoGenProvider::preset(Some(provider)),
                audio: ManagedAudioGenProvider::preset(None),
                jobs: ManagedGenJobs::default(),
                allow: ManagedAllowList(agent_gen::AllowList::new(
                    models
                        .iter()
                        .map(|m| agent_gen::AllowListEntry::new(agent_gen::FIXTURE_PROVIDER_ID, m))
                        .collect(),
                )),
            }
        }

        fn host(&self) -> GenHost<'_> {
            GenHost {
                image: Some(&self.image),
                video: Some(&self.video),
                audio: Some(&self.audio),
                jobs: Some(&self.jobs),
                allow_list: Some(&self.allow),
                stores: Some(test_provider_stores()),
            }
        }
    }

    /// The advisory default, DERIVED — no test in this module types `"aleph2"`.
    fn derived() -> &'static str {
        agent_gen::advisory_video_edit_model().expect("the roster still has its video_to_video row")
    }

    /// The extracted range, standing in for what `extract_clip_range_mp4`
    /// produces. Its CONTENT is irrelevant here (the fixture provider opens no
    /// socket); what matters is that it arrives in `source_video`, because that
    /// field — and nothing else — is what routes a submit to
    /// `/v1/video_to_video`.
    fn clip_bytes() -> Vec<u8> {
        vec![7u8, 7, 7, 7, 7, 7, 7, 7]
    }

    /// **The 55.1 open-selection contract, on the CAPTURED request.**
    ///
    /// `None` yields the advisory default — asserted by CALLING
    /// `advisory_video_edit_model()`, never by re-typing `"aleph2"`, so a roster
    /// correction moves the code and this test together. A named id arrives
    /// **byte-identical**, including one that has never appeared in any Rudis
    /// table, which is the direction that proves nothing local rewrote it.
    #[test]
    fn video_edit_defaults_to_the_advisory_model_and_passes_free_text_through() {
        let derived = derived();

        // (id passed in, id expected on the wire, label)
        let cases: [(Option<&str>, &str, &str); 4] = [
            (None, derived, "no model named => the DERIVED advisory default"),
            (Some(derived), derived, "the advisory id named explicitly"),
            (
                Some("brand-new-model-2027"),
                "brand-new-model-2027",
                "an OFF-ROSTER id submits unmodified — no roster rejection exists",
            ),
            (
                // Blank is treated as ABSENT, exactly as
                // `resolved_model_for_tool_input` treats it, so the disclosure
                // and the bill cannot disagree on the same input (T-42.3-11).
                Some("   "),
                derived,
                "a blank id normalizes the SAME way the disclosure normalizes it",
            ),
        ];

        for (named, expected, label) in cases {
            let ctx = TestAppCtx::new();
            let (probe, provider) = capturing_provider();
            let slots = Slots::admitting(provider, &[expected]);
            let gen = slots.host();

            // The landing fails on the disallowed extension; that is expected
            // and irrelevant. The claim is about what was SUBMITTED.
            let _ = ctx.block_on(generate_runway_video_edit_for_agent(
                &ctx,
                &gen,
                "relight this shot as golden hour".to_string(),
                named.map(str::to_string),
                clip_bytes(),
                Vec::new(),
            ));

            assert_eq!(
                probe.submit_calls(),
                1,
                "{label}: NON-VACUITY — the seam must really have reached the \
                 provider, or the assertions below are about a request that was \
                 never built"
            );
            let req = probe
                .last_request()
                .unwrap_or_else(|| panic!("{label}: a submitted request was captured"));

            assert_eq!(req.model_id, expected, "{label}");
            assert_eq!(
                req.source_video,
                Some(clip_bytes()),
                "{label}: the extracted range IS the endpoint selector — without \
                 it this call is an ordinary text-to-video generation at a \
                 different price"
            );
            assert_eq!(
                req.reference_images, None,
                "{label}: no references were supplied, so the slot stays empty \
                 rather than carrying an empty array"
            );
            // The image->video keyframe slots mean nothing on this endpoint and
            // must not be quietly reused for the edit's conditioning.
            assert_eq!(req.reference_image, None, "{label}");
            assert_eq!(req.destination_image, None, "{label}");
            assert_eq!(req.prompt, "relight this shot as golden hour", "{label}");
            assert_eq!(req.modality, agent_gen::RequestModality::Video, "{label}");
        }
    }

    /// **ONE submit, through the ONE entry.**
    ///
    /// Post-55.1 there is no allow-list row and no caps gate keeping the edit
    /// path honest, so what the governed entry is actually worth is this: a
    /// single spend policy and a single submit, with no inline second path. The
    /// call count is the proof — a forked body that also called
    /// `provider.submit(..)` directly (T-31-18) would read 2.
    #[test]
    fn video_edit_submits_through_the_one_governed_path() {
        let ctx = TestAppCtx::new();
        let (probe, provider) = capturing_provider();
        let slots = Slots::admitting(provider, &[derived()]);
        let gen = slots.host();

        let _ = ctx.block_on(generate_runway_video_edit_for_agent(
            &ctx,
            &gen,
            "swap the background for a night street".to_string(),
            None,
            clip_bytes(),
            Vec::new(),
        ));
        assert_eq!(
            probe.submit_calls(),
            1,
            "exactly ONE submit per call — no second path, no re-submit"
        );
    }

    /// **The reference gate is enforced HERE too, and it refuses before the
    /// provider is even resolved.**
    ///
    /// The FFI host refuses earlier still (before the store lock), but this is
    /// the backstop for any other `GenSubmission` host: a host that forgot the
    /// gate must not be able to reach the wire with references Rudis cannot
    /// name a field for.
    #[test]
    fn video_edit_refuses_references_before_any_provider_work() {
        for n in [1usize, agent_gen::RUNWAY_V2V_MAX_REFERENCES + 1] {
            let ctx = TestAppCtx::new();
            let (probe, provider) = capturing_provider();
            let slots = Slots::admitting(provider, &[derived()]);
            let gen = slots.host();

            let refs: Vec<agent_gen::ReferenceImage> = (0..n)
                .map(|i| agent_gen::ReferenceImage {
                    bytes: vec![i as u8; 4],
                    width: 2,
                    height: 2,
                })
                .collect();

            let err = ctx
                .block_on(generate_runway_video_edit_for_agent(
                    &ctx,
                    &gen,
                    "match this look".to_string(),
                    None,
                    clip_bytes(),
                    refs,
                ))
                .err()
                .expect("references are refused, never dropped");
            assert_eq!(
                err,
                video_edit_reference_check(n).expect_err("the ONE refusal message"),
                "{n}: the seam raises the SHARED refusal verbatim — one message, \
                 one threshold, so the host and the backstop cannot drift"
            );
            assert_eq!(
                probe.submit_calls(),
                0,
                "{n}: T-56-SPEND-06 — a reference refusal costs $0.00 and reaches \
                 no provider at all"
            );
        }
    }

    /// A prompt refusal is still the FIRST act, before any state or provider
    /// work — the ordering the sibling seam has held since Phase 31.
    #[test]
    fn video_edit_refuses_an_empty_prompt_and_empty_bytes_before_the_provider() {
        let ctx = TestAppCtx::new();
        let (probe, provider) = capturing_provider();
        let slots = Slots::admitting(provider, &[derived()]);
        let gen = slots.host();

        let err = ctx
            .block_on(generate_runway_video_edit_for_agent(
                &ctx,
                &gen,
                "   ".to_string(),
                None,
                clip_bytes(),
                Vec::new(),
            ))
            .err()
            .expect("an empty prompt is refused");
        assert!(err.contains("prompt"), "the refusal names the prompt: {err}");

        // Empty extracted bytes would submit a `source_video: Some(vec![])`,
        // which still routes to the paid v2v endpoint while carrying nothing —
        // a spend on an empty edit. Refused locally at $0.00.
        let err = ctx
            .block_on(generate_runway_video_edit_for_agent(
                &ctx,
                &gen,
                "relight".to_string(),
                None,
                Vec::new(),
                Vec::new(),
            ))
            .err()
            .expect("empty source bytes are refused");
        assert!(
            err.contains("no video data") || err.contains("empty"),
            "the refusal names the empty extraction: {err}"
        );
        assert_eq!(probe.submit_calls(), 0, "neither refusal reached a provider");
    }
}

/// Phase 56 plan 05 (GEN-11 / D-01) — **the clip-edit path's per-call price.**
///
/// The advisory roster prices every other video model for the ONE clip length
/// its endpoint produces, so "the price of a 4s clip" is very nearly "the
/// price". `video_to_video` is the exception: it re-renders a range the USER
/// chose, so the billable quantity is an input length, and a confirmation that
/// quoted the roster's flat 4-second figure would under-quote a 30-second edit
/// by 7.5x. These tests pin the arithmetic that closes that gap, and the
/// honest-absence path beside it.
#[cfg(test)]
mod video_edit_cost_estimate {
    use super::*;

    // -----------------------------------------------------------------------
    // In the RED commit three local items SHADOWED the then-absent production
    // names (a local item beats a `use super::*` glob), so every failure below
    // was an ASSERTION failure on a tree that still BUILT rather than a compile
    // error, which proves nothing about behaviour — the 55.1-04 discipline:
    //     rate/floor = 0, estimated_video_edit_cost_cents(..) -> None
    //   left: None      right: Some(0)     (the scaling arm)
    //   left: Some(112) right: Some(0)     (the drift pin, quoting the REAL row)
    // GREEN deleted the scaffold; `use super::*` now resolves to production and
    // NOT ONE ASSERTION MOVED.
    // -----------------------------------------------------------------------

    /// One microsecond count, expressed in the units the timeline speaks.
    fn secs(n: i64) -> i64 {
        n * 1_000_000
    }

    /// The advisory default, DERIVED. No test in this module types the model id:
    /// it lives in the roster and in plan 05's `agent-gen` pin, and a third copy
    /// here would be the transcription this phase keeps catching.
    fn priced_model() -> &'static str {
        agent_gen::advisory_video_edit_model().expect("the roster has a video_to_video row")
    }

    /// **The estimate scales with the INPUT length, and the vendor's minimum is
    /// a floor rather than a suggestion.**
    ///
    /// Every expectation is ARITHMETIC over the two consts, never a re-typed
    /// figure — so a corrected rate moves the code and this test together, which
    /// is the property the `$0.56` halving lacked.
    #[test]
    fn estimated_video_edit_cost_scales_with_input_seconds() {
        let rate = RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND;
        let floor = RUNWAY_ALEPH2_MINIMUM_CENTS;
        let m = priced_model();

        // 4s: comfortably over the floor, so it is pure rate x seconds.
        assert_eq!(
            estimated_video_edit_cost_cents(m, secs(4)),
            Some(rate * 4),
            "4 input seconds bills at the published per-second rate"
        );
        // 30s (the documented ceiling): the number that makes the whole helper
        // worth having — 7.5x the roster's flat 4-second figure.
        assert_eq!(
            estimated_video_edit_cost_cents(m, secs(30)),
            Some(rate * 30),
            "a 30s edit is NOT the price of a 4s clip"
        );
        // 2s (the documented floor): rate x 2 and the minimum are the SAME
        // number by construction, which is exactly why the 56 was mistakable for
        // a 4-second price in the first place.
        assert_eq!(
            estimated_video_edit_cost_cents(m, secs(2)),
            Some(floor),
            "at the minimum input length the rate and the floor coincide"
        );
        assert_eq!(
            rate * 2,
            floor,
            "and that coincidence is the vendor's own arithmetic"
        );

        // Under the floor the FLOOR wins. `clip_edit_window_check` refuses these
        // ranges before any submit, so this is defence in depth rather than a
        // reachable quote — but a helper that under-quotes a short range is
        // exactly the direction that costs a user money.
        assert_eq!(
            estimated_video_edit_cost_cents(m, secs(1)),
            Some(floor),
            "a sub-minimum range still costs the minimum — never rate x 1"
        );

        // A started second is a billed second: 4.2s bills as 5.
        assert_eq!(
            estimated_video_edit_cost_cents(m, secs(4) + 200_000),
            Some(rate * 5),
            "partial seconds round UP — rounding down would quote less than the bill"
        );

        // Non-vacuity: the numbers above are not all the same number.
        assert_ne!(
            rate * 4,
            rate * 30,
            "the estimate must actually vary with length"
        );
    }

    /// **The honest-absence path.** An id the roster cannot price yields `None`,
    /// which plan 07 must render as the literal [`PRICE_UNKNOWN`] words — never a
    /// guessed figure. Post-55.1 this is the ORDINARY case, not the exotic one:
    /// every model Runway ships next lands here.
    #[test]
    fn estimated_video_edit_cost_is_unknown_for_an_off_roster_id() {
        assert_eq!(
            estimated_video_edit_cost_cents("brand-new-model-2027", secs(4)),
            None,
            "an off-roster id has no known price and must not be given one"
        );
        // Real on Runway, in no Rudis table — the same id 55.1-04 uses, so both
        // halves of the spend question agree about what "unknown" means.
        assert_eq!(
            estimated_video_edit_cost_cents("kling3.0_pro", secs(4)),
            None
        );
        // An ON-roster model that is NOT the v2v row is equally unknown HERE:
        // gen4_turbo has a price, but not a per-input-second one, and quoting
        // its 4-second clip figure for a 30-second edit is the precise error
        // this helper exists to stop.
        assert_eq!(
            estimated_video_edit_cost_cents("gen4_turbo", secs(4)),
            None,
            "a priced model with no per-second basis is still None on THIS axis"
        );
        // Non-vacuity: the priced model really does return Some here.
        assert!(estimated_video_edit_cost_cents(priced_model(), secs(4)).is_some());
    }

    /// **The two cost tables can never contradict each other again.**
    ///
    /// This is the pin that makes the resolved `$0.56` contradiction
    /// unrepeatable from the other side. `agent-gen`'s own
    /// `every_costed_video_row_satisfies_cents_equals_credits_per_second_times_4`
    /// pins the roster row against Runway's published rate; this pins app-core's
    /// per-second const against the roster row. Together the loop is closed, and
    /// neither const can be edited alone.
    #[test]
    fn aleph2_cost_figures_cannot_drift_apart() {
        let caps =
            agent_gen::model_caps(priced_model()).expect("the advisory default is on the roster");
        assert_eq!(
            caps.cost_cents_per_4s,
            Some(RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND * 4),
            "the roster's 4-second figure must be exactly 4x app-core's per-second \
             rate. If these disagree, one of them is a minimumCredits value in a \
             cents field again — re-read docs.dev.runwayml.com/guides/pricing and \
             56-01-PROBE-RESULTS.md before changing either"
        );
        // The floor is the vendor's 2-second minimum charge, and 2 is the
        // window's own documented minimum — the corroboration 56-03 recorded when
        // it labelled RUNWAY_V2V_INPUT_MIN_SECONDS "documented, not
        // live-verified". Pinning it here means a probe correction to the window
        // has to confront the price it implies.
        assert_eq!(
            agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS * RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND,
            RUNWAY_ALEPH2_MINIMUM_CENTS,
            "the minimum charge is exactly the minimum input length at the published \
             rate — if probe V-6 moves the window's floor, this pricing floor moves \
             with it or one of the two is wrong"
        );
    }
}

/// Phase 56 plan 06 (GEN-11 / D-08, D-09) — **the reference slot is BLOCKED,
/// and the refusal says so in the words a reader can act on.**
///
/// F-1b ran on 2026-08-09 and did NOT crown a field. These pin the two facts
/// that follow: the ceiling is now measured rather than documented, and any
/// non-empty set is refused rather than dropped — with 56-09 named as the only
/// remaining route, because a validation error proves a schema and the open
/// question is about behaviour.
#[cfg(test)]
mod video_edit_reference_gate {
    use super::*;

    /// **Zero references is the shipping path.** The whole clip-edit capability
    /// works today without a single reference; the blocked slot is a clause of
    /// GEN-11, not the feature.
    #[test]
    fn no_references_is_the_shipping_path_and_never_refuses() {
        assert!(
            video_edit_reference_check(0).is_ok(),
            "an edit with no references is fully unblocked and must not refuse"
        );
    }

    /// **1..=5 refuses by NAME, citing the paid run that would settle it.**
    ///
    /// The message must not send the reader after F-1b: that probe already ran,
    /// at $0.00, and came back AMBIGUOUS. Naming a probe that has already been
    /// spent is worse than naming nothing, because it looks actionable.
    #[test]
    fn a_reference_set_inside_the_cap_refuses_and_names_56_09_not_another_free_probe() {
        for n in 1..=agent_gen::RUNWAY_V2V_MAX_REFERENCES {
            let err = video_edit_reference_check(n)
                .expect_err("a reference has no probe-confirmed key to ride");
            assert!(
                err.contains("56-09"),
                "{n}: the refusal names the PAID run that would settle it — F-1b \
                 already ran and could not: {err}"
            );
            assert!(
                err.contains("keyframes") && err.contains("promptImage"),
                "{n}: and names both surviving candidates, so the refusal is \
                 actionable rather than a shrug: {err}"
            );
            assert!(
                err.contains("BEHAVIOUR"),
                "{n}: and says WHY no further free probe helps — the open question \
                 is behavioural, not schematic: {err}"
            );
            assert!(
                err.contains("refused rather than silently sent"),
                "{n}: and says the call was refused, never quietly stripped — a \
                 dropped reference is indistinguishable from a bad generation \
                 (42.1-04 caught exactly that, HTTP 200): {err}"
            );
        }
    }

    /// **Over the cap is a DIFFERENT refusal**, naming the number rather than
    /// the probe gap — and the two messages must not be the same sentence, or
    /// the reader learns nothing from which one they got.
    #[test]
    fn over_the_cap_refuses_on_the_measured_ceiling_not_on_the_probe_gap() {
        let cap = agent_gen::RUNWAY_V2V_MAX_REFERENCES;
        let err = video_edit_reference_check(cap + 1).expect_err("over the cap refuses");
        assert!(
            err.contains(&cap.to_string()) && err.contains(&(cap + 1).to_string()),
            "the refusal names the cap AND the count supplied: {err}"
        );
        assert!(
            err.contains("F-1b"),
            "and cites the probe that MEASURED the ceiling — since 2026-08-09 this \
             number is `\"maximum\": 5, inclusive` from the endpoint itself, not a \
             changelog sentence: {err}"
        );
        // The two arms are genuinely different messages, not one template with a
        // different comparison. A copy-paste would pass every assertion above.
        let inside = video_edit_reference_check(1).expect_err("inside the cap refuses too");
        assert_ne!(err, inside, "the two refusals are different sentences");
        assert!(
            !err.contains("56-09"),
            "the over-cap arm is a SHAPE bound and is not waiting on a paid run: {err}"
        );
    }

    /// The cap is READ from `agent-gen`, never re-declared here — the same
    /// property `clip_range.rs` holds for the window (56-04).
    #[test]
    fn the_reference_cap_is_the_agent_gen_const_and_its_edges_are_exact() {
        let cap = agent_gen::RUNWAY_V2V_MAX_REFERENCES;
        // Exactly at the cap: still refused, but by the BLOCKED arm, because the
        // shape bound is inclusive (`inclusive: true`, measured).
        let at = video_edit_reference_check(cap).expect_err("the slot is blocked at any count");
        assert!(
            at.contains("56-09"),
            "at exactly the cap the shape bound is satisfied, so the refusal must \
             be the blocked-slot one: {at}"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 69 (OSS-02 / D-69-04 / D-69-12 / D-69-13): invalidatable lazy slots,
// namespaced slot coordinates, and the Settings set/clear bodies.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod phase69_keys_you_own {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fixture() -> agent_gen::ConcreteGenProvider {
        agent_gen::ConcreteGenProvider::Fixture(agent_gen::FixtureGenProvider::sync_still(
            b"png".to_vec(),
            "png",
        ))
    }

    fn in_memory_stores() -> ManagedProviderKeyStore {
        ManagedProviderKeyStore::with_stores(
            PROVIDER_KEY_SLOTS
                .iter()
                .map(|&(id, _, validator)| {
                    let store: Box<dyn agent_llm::KeyStore> =
                        Box::new(agent_llm::InMemoryKeyStore::with_validator(validator));
                    (id.to_string(), store)
                })
                .collect(),
        )
    }

    #[test]
    fn provider_slot_resolves_once_until_invalidated() {
        let slot = ProviderSlot::unresolved();
        assert!(!slot.is_resolved(), "construction reads nothing (LAT-05)");
        let calls = AtomicUsize::new(0);
        let run = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(fixture())
        };
        assert!(slot.resolve_with(run).is_some());
        assert!(slot.resolve_with(run).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cached after the first resolve");
        assert!(slot.is_resolved());

        slot.invalidate();
        assert!(!slot.is_resolved(), "invalidate returns the slot to unresolved");
        assert!(slot.resolve_with(run).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2, "re-resolved after invalidate");
    }

    #[test]
    fn provider_slot_caches_an_honest_none_too() {
        let slot = ProviderSlot::unresolved();
        let calls = AtomicUsize::new(0);
        let run = || {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        };
        assert!(slot.resolve_with(run).is_none());
        assert!(slot.resolve_with(run).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(slot.is_resolved(), "a resolved None is still resolved");
    }

    #[test]
    fn provider_slot_preset_never_runs_the_resolver() {
        let slot = ProviderSlot::preset(Some(Arc::new(fixture())));
        let calls = AtomicUsize::new(0);
        let got = slot.resolve_with(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        });
        assert!(got.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_slot_survives_a_poisoned_lock() {
        let slot = Arc::new(ProviderSlot::unresolved());
        let poisoner = Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.0.write().unwrap();
            panic!("poison the slot lock");
        })
        .join();
        assert!(slot.0.is_poisoned());
        assert!(slot.resolve_with(|| Some(fixture())).is_some());
        slot.invalidate();
        assert!(!slot.is_resolved());
    }

    #[test]
    fn slot_target_names_namespace_every_slot() {
        let test = agent_llm::CredentialService::from_config(Some("rudis-test-69-x"));
        assert_eq!(
            ManagedProviderKeyStore::slot_target_names(&test),
            vec![
                ("fixture", "gen-fixture-api-key.rudis-test-69-x".to_string()),
                ("elevenlabs", "gen-elevenlabs-api-key.rudis-test-69-x".to_string()),
                ("runway", "gen-runway-api-key.rudis-test-69-x".to_string()),
            ]
        );
        assert_eq!(
            ManagedProviderKeyStore::slot_target_names(&agent_llm::CredentialService::PRODUCTION),
            vec![
                ("fixture", "gen-fixture-api-key.rudis".to_string()),
                ("elevenlabs", "gen-elevenlabs-api-key.rudis".to_string()),
                ("runway", "gen-runway-api-key.rudis".to_string()),
            ],
            "production coordinates are byte-identical (D-69-01)"
        );
    }

    #[test]
    fn set_provider_key_refuses_providers_outside_the_allow_list() {
        let stores = in_memory_stores();
        let (image, video, audio) = (
            ManagedGenProvider::production(),
            ManagedVideoGenProvider::production_video(),
            ManagedAudioGenProvider::production_audio(),
        );
        let gen = GenHost {
            image: Some(&image),
            video: Some(&video),
            audio: Some(&audio),
            jobs: None,
            allow_list: None,
            stores: Some(&stores),
        };
        for provider in ["elevenlabs", "fixture", "openai", ""] {
            let err = run_set_provider_key(&stores, &gen, provider, "abcd").unwrap_err();
            assert!(err.contains("not managed by Settings"), "{provider}: {err}");
            assert!(!err.contains("abcd"), "the key is never echoed");
            let err = run_clear_provider_key(&stores, &gen, provider).unwrap_err();
            assert!(err.contains("not managed by Settings"), "{provider}: {err}");
        }
        for id in ["elevenlabs", "fixture"] {
            assert_eq!(stores.slot(id).unwrap().get(), Ok(None), "{id} untouched");
        }

        let err = run_set_provider_key(&stores, &gen, "runway", "  ").unwrap_err();
        assert!(err.contains("empty"), "the validator's own rule text: {err}");
        assert_eq!(stores.slot("runway").unwrap().get(), Ok(None));

        run_set_provider_key(&stores, &gen, "runway", "RUDIS69KEY").unwrap();
        assert_eq!(
            stores.slot("runway").unwrap().get(),
            Ok(Some("RUDIS69KEY".to_string()))
        );
    }

    #[test]
    fn set_provider_key_then_clear_takes_effect_without_restart() {
        // The in-process scrub `crates/ffi/tests/contract.rs::scrub_ambient_api_key`
        // does for Anthropic: the Runway env fallbacks must not mask the store.
        std::env::remove_var("RUNWAY_API_KEY");
        std::env::remove_var("RUNWAYML_API_SECRET");

        let stores = in_memory_stores();
        let (image, video, audio) = (
            ManagedGenProvider::production(),
            ManagedVideoGenProvider::production_video(),
            ManagedAudioGenProvider::production_audio(),
        );
        let gen = GenHost {
            image: Some(&image),
            video: Some(&video),
            audio: Some(&audio),
            jobs: None,
            allow_list: None,
            stores: Some(&stores),
        };
        assert!(!gen.image.unwrap().0.is_resolved(), "LAT-05: zero reads at construction");
        assert!(!gen.video.unwrap().0.is_resolved());
        assert!(!gen.audio.unwrap().0.is_resolved());

        // Resolve BEFORE any key: an honest, cached None.
        assert!(image.resolve_via(&stores).is_none());
        assert!(video.resolve_via(&stores).is_none());
        assert!(image.0.is_resolved());

        run_set_provider_key(&stores, &gen, "runway", "RUDIS69KEY").unwrap();
        assert!(!image.0.is_resolved(), "set invalidates the image slot");
        assert!(!video.0.is_resolved(), "set invalidates the video slot");
        let p = image.resolve_via(&stores).expect("the new key takes effect immediately");
        assert_eq!(agent_gen::GenProvider::id(p.as_ref()).0, "runway");
        let p = video.resolve_via(&stores).expect("video too");
        assert_eq!(agent_gen::GenProvider::id(p.as_ref()).0, "runway");

        run_clear_provider_key(&stores, &gen, "runway").unwrap();
        assert!(image.resolve_via(&stores).is_none(), "clear takes effect immediately");
        assert!(video.resolve_via(&stores).is_none());
        assert_eq!(stores.slot("runway").unwrap().get(), Ok(None));
        assert!(!audio.0.is_resolved(), "the audio slot was never touched");
    }

    #[test]
    fn invalidate_provider_routes_by_provider_id() {
        let image = ManagedGenProvider::preset(Some(Arc::new(fixture())));
        let video = ManagedVideoGenProvider::preset(Some(Arc::new(fixture())));
        let audio = ManagedAudioGenProvider::preset(Some(Arc::new(fixture())));
        let gen = GenHost {
            image: Some(&image),
            video: Some(&video),
            audio: Some(&audio),
            jobs: None,
            allow_list: None,
            stores: None,
        };
        gen.invalidate_provider("fixture");
        assert!(image.0.is_resolved() && video.0.is_resolved() && audio.0.is_resolved());
        gen.invalidate_provider("elevenlabs");
        assert!(image.0.is_resolved() && video.0.is_resolved());
        assert!(!audio.0.is_resolved());
        gen.invalidate_provider("runway");
        assert!(!image.0.is_resolved() && !video.0.is_resolved());
    }

    #[test]
    fn runway_absence_messages_point_at_settings_and_audio_is_unchanged() {
        for msg in [NO_PROVIDER_CONFIGURED, NO_VIDEO_PROVIDER_CONFIGURED] {
            assert!(msg.contains("Settings"), "{msg}");
            assert!(msg.contains("Runway"), "{msg}");
            assert!(msg.contains("RUNWAY_API_KEY"), "{msg}");
            assert!(!msg.contains("restart"), "{msg}");
        }
        // D-69-04: the ElevenLabs text is deliberately untouched (it stays on the
        // developer `.env` path); `git diff` proves byte-identity, this pins its gist.
        assert!(NO_AUDIO_PROVIDER_CONFIGURED.contains("ElevenLabs"));
        assert!(NO_AUDIO_PROVIDER_CONFIGURED.contains("ELEVENLABS_API_KEY"));
        assert!(!NO_AUDIO_PROVIDER_CONFIGURED.contains("Settings"));
    }
}
