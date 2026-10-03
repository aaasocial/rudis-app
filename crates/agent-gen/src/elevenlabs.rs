//! `ElevenLabsProvider` — the FOURTH real [`GenProvider`](crate::GenProvider)
//! and the first AUDIO-modality one: a hand-rolled `reqwest` client for
//! ElevenLabs' SYNCHRONOUS Text-to-Speech REST API (`api.elevenlabs.io`).
//!
//! # Licensing hygiene (CLAUDE.md rule 6 / PROVENANCE.md Entry 15)
//!
//! This is a hand-rolled client written against ElevenLabs' PUBLIC HTTP API
//! documentation (`elevenlabs.io/docs` — request/response shapes only). There is
//! **no vendor SDK** dependency and **no third-party source copied** — the same
//! "thin, fully-audited typed `reqwest` client we control" stance
//! [`OpenAiProvider`](crate::OpenAiProvider) (Entry 13) and
//! [`VeoProvider`](crate::VeoProvider) (Entry 14) take. Rust has no official
//! ElevenLabs SDK to reach for anyway.
//!
//! # Fail-closed this wave (34-01): NO allow-list row, NO key slot
//!
//! This wave ships the offline provider ONLY. There is **no `(elevenlabs, *)`
//! allow-list row**, no `"elevenlabs"` key slot, and no live call path — every
//! production submit of any elevenlabs model is fail-closed rejected by
//! `submit_checked` until a human records
//! `.planning/phases/34-audio-generation/GEN-08-SIGNOFF.md` (the gated Wave 3,
//! 34-03), which does not exist yet.
//!
//! # Scope: TTS only (SFX / Music / voice-cloning DEFERRED)
//!
//! Only text-to-speech (`POST /v1/text-to-speech/{voice_id}`) is implemented.
//! SFX (`/v1/sound-generation`) and Music (`/v1/music`) are DEFERRED: their
//! guardrails — "no standalone reusable-asset export" (SFX) and "no film/TV
//! clearance claim" (Music) — are GEN-03's own required product-policy work
//! (34-RESEARCH.md Pitfall 3), not yet built. Voice CLONING is NEVER driven by
//! Rudis: this provider only READS the account's existing voices and prefers a
//! `premade` one, so GEN-03's consent-affirmation clause is satisfied by
//! construction (T-34-08) — there is nothing to affirm because Rudis never
//! creates a voice.
//!
//! # Why synchronous (`SubmitOutcome::Ready` only)
//!
//! ElevenLabs' TTS endpoint blocks until the audio is generated and returns the
//! bytes in the SAME HTTP response — no job id, no polling endpoint. `submit`
//! therefore does the entire call inline and returns `Ready`, mirroring
//! [`OpenAiProvider`](crate::OpenAiProvider) byte-for-byte in shape. It NEVER
//! returns `Pending`; `poll` is honestly unreachable and `cancel` is a no-op.
//! The ONE structural difference from OpenAI: the 2xx body is NOT JSON — it IS
//! the raw mp3 bytes (`application/octet-stream`), read with `resp.bytes()`
//! directly, like Veo's DOWNLOAD step, never `.json()` (34-RESEARCH.md Pitfall 2).
//!
//! # Key hygiene (T-34-01, inherited from T-31-01)
//!
//! The struct is deliberately NOT `Debug`/`Serialize`-derived; the API key is a
//! private field interpolated ONLY into the two `xi-api-key` header builds and
//! NOWHERE else. [`non_success_to_gen_error`] never receives the key — the only
//! thing it surfaces is ElevenLabs' own error JSON body, which never contains
//! the auth header. `AssetRef.suggested_ext` is a FIXED `"mp3"` literal, never
//! derived from any response header (T-34-06).

use crate::job::JobHandle;
use crate::provider::{
    AssetRef, GenError, GenProvider, GenRequest, JobStatus, ModelInfo, ProviderId, SubmitOutcome,
};

/// This provider's stable registered id — also what a future GEN-08 allow-list
/// row would key on and what the landing bridge interpolates into a generated
/// filename, so it must never be free text.
pub const ELEVENLABS_PROVIDER_ID: &str = "elevenlabs";

/// The ElevenLabs API base. A `&'static str` const — NEVER request data (T-34-03
/// SSRF discipline). Both endpoints (`/v2/voices`, `/v1/text-to-speech/{id}`)
/// are pinned onto THIS host; unlike Veo, NO response here ever supplies a URL
/// to follow (the audio bytes ARE the response body), so there is no
/// download-URI SSRF gate to build at all.
const ELEVENLABS_BASE_URL: &str = "https://api.elevenlabs.io";

/// The pinned TTS model id. `eleven_multilingual_v2` is "most stable on
/// long-form generations" (34-RESEARCH.md § Model choice) — the conservative
/// first-ship choice over the cheaper/faster `eleven_flash_v2_5` or the
/// expressive `eleven_v3`. Always sent explicitly (openai.rs discipline), never
/// relying on an implicit provider default. Extending the allow-list to any
/// other model id is a human GEN-08 gate, never a silent code change.
pub const ELEVENLABS_TTS_MODEL: &str = "eleven_multilingual_v2";

/// The ElevenLabs TTS request body — EXACTLY the two documented fields this
/// build sends.
///
/// `voice_settings` is deliberately omitted (ElevenLabs applies sane server-side
/// defaults) and `output_format` is deliberately omitted as a query param (the
/// default `mp3_44100_128` is a real, correctly-extensioned mp3 — see [`submit`]
/// comment). `model_id` is `&'static str`, so a prompt can NEVER steer it: the
/// prompt reaches only the `text` field (T-34-09). The struct derives
/// `Serialize` (it is the wire body) but the parent [`ElevenLabsProvider`] does
/// not — the request carries no credentials.
///
/// [`submit`]: ElevenLabsProvider::submit
#[derive(Debug, Clone, serde::Serialize)]
pub struct ElevenLabsTtsRequest {
    pub text: String,
    pub model_id: &'static str,
}

/// The `GET /v2/voices` response envelope. `serde` ignores unknown fields by
/// default — right for a third-party response body.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ElevenLabsVoicesResponse {
    pub voices: Vec<ElevenLabsVoice>,
}

/// One voice on the authenticated account. Only `voice_id` is load-bearing;
/// `category` (when present) lets [`pick_voice`] prefer a `premade` voice, and
/// `name` is tolerated-but-unused. Both non-id fields carry `#[serde(default)]`
/// so a body that omits them still parses (a fresh account's shape is not
/// guaranteed field-for-field).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ElevenLabsVoice {
    pub voice_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
}

// NOTE: there is deliberately NO ElevenLabs TTS *response* struct. The 2xx body
// is NOT JSON — it IS the mp3 bytes (`application/octet-stream`), read with
// `resp.bytes()` directly (34-RESEARCH.md Pitfall 2). Modelling it as a type
// would invite a `.json()` parse that silently corrupts the audio.

/// Build the TTS request body for a prompt, ALWAYS setting `model_id` explicitly
/// to [`ELEVENLABS_TTS_MODEL`] (never relying on an implicit provider default —
/// openai.rs discipline). Pure — no `Client`, no network.
pub fn build_tts_request(text: String) -> ElevenLabsTtsRequest {
    ElevenLabsTtsRequest {
        text,
        model_id: ELEVENLABS_TTS_MODEL,
    }
}

/// Pick a usable `voice_id` from a parsed voices response: the first `premade`
/// voice if any, else the first voice at all, else an honest actionable error.
///
/// PURE — no `Client`, no network — which is what makes voice-picking hermetic
/// to unit-test without a mock HTTP server. Resolution is DYNAMIC (never a
/// hardcoded literal) because ElevenLabs' legacy "Default voices" (Rachel, Aria,
/// …) retire entirely on 31 December 2026 (34-RESEARCH.md Pitfall 1), and a
/// brand-new free-tier BYO-key account may have ZERO usable voices until the
/// user visits the dashboard once (Assumptions Log A1) — so the empty case is a
/// first-class, actionable error, never a panic and never a silent fallback to
/// a fabricated voice_id.
pub(crate) fn pick_voice(parsed: ElevenLabsVoicesResponse) -> Result<String, GenError> {
    if let Some(v) = parsed
        .voices
        .iter()
        .find(|v| v.category.as_deref() == Some("premade"))
    {
        return Ok(v.voice_id.clone());
    }
    parsed
        .voices
        .into_iter()
        .next()
        .map(|v| v.voice_id)
        .ok_or_else(|| {
            GenError::Provider(
                "your ElevenLabs account has no voices available — add one at \
                 elevenlabs.io/app/voice-library, then try again"
                    .to_string(),
            )
        })
}

/// Map a non-2xx HTTP status + body to a [`GenError::Provider`].
///
/// The `body` is ElevenLabs' own error JSON (`{"detail":{"status":..,
/// "message":..}}`) — it never contains the `xi-api-key` header, so surfacing it
/// verbatim is safe (T-34-01). Crucially this helper does NOT receive the API
/// key, so there is structurally no interpolation path by which the key could
/// reach the error string.
pub(crate) fn non_success_to_gen_error(status: u16, body: &str) -> GenError {
    GenError::Provider(format!("ElevenLabs API error (HTTP {status}): {body}"))
}

/// The REAL, key-gated ElevenLabs TTS provider (synchronous, zero-poll).
///
/// Mirrors [`OpenAiProvider`](crate::OpenAiProvider)'s `{reqwest::Client,
/// api_key, models}` shape. Deliberately NOT `Debug`/`Serialize`-derived —
/// nothing here can leak the key into a log line (T-34-01). `models` is a
/// hardcoded single-entry catalog (ElevenLabs has no "list TTS models"
/// discovery contract we depend on), mirroring `OpenAiProvider::catalog`. There
/// is NO voice cache field: the provider stays `Send + Sync` with no interior
/// mutability (the `FixtureGenProvider` "no `RefCell`" discipline), so
/// [`resolve_voice_id`](Self::resolve_voice_id) runs once per `submit` — one
/// small GET is the deliberate price of correct, per-account voice resolution.
pub struct ElevenLabsProvider {
    client: reqwest::Client,
    api_key: String,
    models: Vec<ModelInfo>,
}

impl ElevenLabsProvider {
    /// The hardcoded catalog GEN-07 surfaces and GEN-09 reads its provenance
    /// flag from. `carries_provenance_watermark: true` — ElevenLabs adopted
    /// SynthID (inaudible) + C2PA in May 2026, so every API audio carries a
    /// provenance signal (GEN-09's disclosure source).
    fn catalog() -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: ELEVENLABS_TTS_MODEL.to_string(),
            label: "ElevenLabs Multilingual v2".to_string(),
            modality: "audio".to_string(),
            carries_provenance_watermark: true,
            rough_cost_signal: Some("~$0.15-0.30 / 1,000 characters".to_string()),
        }]
    }

    /// Construct directly from a resolved key (the constructor tests and
    /// [`connect`](Self::connect) both funnel through). Builds a plain
    /// `reqwest::Client` — no runtime is needed to CONSTRUCT one; only `.send()`
    /// needs the async executor, and only the two HTTP methods call it.
    pub fn with_key(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            models: Self::catalog(),
        }
    }

    /// BYO-key resolution mirroring `OpenAiProvider::connect` /
    /// `VeoProvider::connect`: the `store`'s key WINS; only when the store is
    /// empty (or errors) does it fall back to `ELEVENLABS_API_KEY` in the
    /// process env (the canonical env name the official ElevenLabs SDKs read).
    /// `Option::or_else` short-circuits, so a seeded store answers
    /// deterministically regardless of ambient env — which makes
    /// [`connect`](Self::connect) unit-testable.
    ///
    /// The store is the OS-keychain `"elevenlabs"` slot (registered in the
    /// gated Wave 34-02, not this wave). `None` (never panics) when NEITHER
    /// source has a key → the caller surfaces an honest "no provider
    /// configured" state. `cargo test` does not load `.env` (dotenvy runs only
    /// in `run()`), so an empty store with no host env resolves to `None`
    /// deterministically in the routine offline suite.
    pub fn connect(store: &dyn agent_llm::KeyStore) -> Option<Self> {
        let api_key = store
            .get()
            .ok()
            .flatten()
            .or_else(|| std::env::var("ELEVENLABS_API_KEY").ok())?;
        Some(Self::with_key(api_key))
    }

    /// Test-only accessor proving [`connect`](Self::connect) held the STORE's
    /// key. Not part of the public API — the key is otherwise unreachable.
    #[cfg(test)]
    pub(crate) fn api_key_for_test(&self) -> &str {
        &self.api_key
    }

    /// Resolve a usable `voice_id` for THIS account via `GET /v2/voices`, then
    /// [`pick_voice`]. Per-`submit` (no cache field), so the provider stays
    /// interior-mutability-free. Transport error → `Provider`; non-2xx →
    /// [`non_success_to_gen_error`] (surfaces a tier/permission gate on LISTING
    /// itself honestly — A4); 2xx → parse then premade-first pick.
    async fn resolve_voice_id(&self) -> Result<String, GenError> {
        let resp = self
            .client
            .get(format!("{ELEVENLABS_BASE_URL}/v2/voices"))
            .header("xi-api-key", &self.api_key)
            .send()
            .await
            .map_err(|e| GenError::Provider(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(non_success_to_gen_error(status, &text));
        }
        let parsed: ElevenLabsVoicesResponse = resp
            .json()
            .await
            .map_err(|e| GenError::Provider(format!("bad ElevenLabs voices response: {e}")))?;
        pick_voice(parsed)
    }
}

impl GenProvider for ElevenLabsProvider {
    fn id(&self) -> ProviderId {
        ProviderId(ELEVENLABS_PROVIDER_ID.to_string())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, GenError> {
        // Zero network — the catalog is Rudis's own curated, allow-list-aligned
        // list, exactly like OpenAiProvider's.
        Ok(self.models.clone())
    }

    async fn submit(&self, req: GenRequest) -> Result<SubmitOutcome, GenError> {
        // `req.background` is IGNORED: audio has no background concept (mirrors
        // VeoProvider's ignore posture, 33-01). The prompt reaches ONLY the
        // `text` field (T-34-09).
        let voice_id = self.resolve_voice_id().await?;
        // The TTS POST. `output_format` query param is deliberately OMITTED —
        // the default `mp3_44100_128` is a real, correctly-`.mp3`-extensioned
        // stream, so (unlike OpenAI's output_format) there is no implicit-default
        // risk to guard against here.
        let resp = self
            .client
            .post(format!("{ELEVENLABS_BASE_URL}/v1/text-to-speech/{voice_id}"))
            .header("xi-api-key", &self.api_key)
            .json(&build_tts_request(req.prompt))
            .send()
            .await
            .map_err(|e| GenError::Provider(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(non_success_to_gen_error(status, &text));
        }
        // The 2xx body IS the mp3 (application/octet-stream) — read the raw
        // bytes directly, NEVER `.json()`/`.text()` on the success branch
        // (34-RESEARCH.md Pitfall 2), mirroring Veo's download read.
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| GenError::Provider(e.to_string()))?;
        if bytes.is_empty() {
            // Never Ready-with-nothing — the lie the landing bridge forbids.
            return Err(GenError::Provider(
                "ElevenLabs returned an empty audio body".to_string(),
            ));
        }
        Ok(SubmitOutcome::Ready(vec![AssetRef {
            bytes: bytes.to_vec(),
            // FIXED literal — never derived from any response header (T-34-06).
            suggested_ext: "mp3".to_string(),
        }]))
    }

    async fn poll(&self, _job: &JobHandle) -> Result<JobStatus, GenError> {
        Err(GenError::InvalidRequest(
            "ElevenLabsProvider is synchronous; poll should never be called".to_string(),
        ))
    }

    async fn cancel(&self, _job: &JobHandle) -> Result<(), GenError> {
        // Nothing remote to abandon — no `.send()` here; the caller stops
        // polling regardless (mirrors OpenAiProvider::cancel).
        Ok(())
    }

    // NO poll_interval override — inherits DEFAULT_POLL_INTERVAL (250ms). This
    // provider is synchronous, so the poll loop is never entered anyway.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobHandle, JobId};
    use agent_llm::{InMemoryKeyStore, KeyStore as _};

    /// A well-formed but FAKE sentinel key — never a real credential. Kept in
    /// scope near the error-mapping call to PROVE no interpolation path lets it
    /// reach the surfaced error string.
    const SENTINEL_KEY: &str = "xi-SENTINEL-not-a-real-key";

    /// Drive an `async fn` from a plain `#[test]` without a runtime dependency —
    /// the `pollster` precedent. Constructing a `reqwest::Client` needs no
    /// runtime; only `.send()` does, and no unit test here calls a network
    /// method except `cancel`, whose body provably contains none.
    fn run<T>(fut: impl std::future::Future<Output = T>) -> T {
        pollster::block_on(fut)
    }

    /// A permissive validator so a non-`sk-ant-` test key is accepted by the
    /// in-memory store double. NOTHING about it hits the network.
    fn accept_any(_key: &str) -> Result<(), agent_llm::KeyStoreError> {
        Ok(())
    }

    #[test]
    fn elevenlabs_provider_id_is_elevenlabs() {
        let provider = ElevenLabsProvider::with_key("k".to_string());
        assert_eq!(provider.id().as_str(), "elevenlabs");
        assert_eq!(ELEVENLABS_PROVIDER_ID, "elevenlabs");
    }

    #[test]
    fn elevenlabs_list_models_is_the_single_tts_catalog_row() {
        let provider = ElevenLabsProvider::with_key("k".to_string());
        let models = run(provider.list_models()).expect("list_models succeeds offline");

        assert_eq!(models.len(), 1, "exactly one curated TTS model");
        let m = &models[0];
        assert_eq!(m.id, "eleven_multilingual_v2");
        assert_eq!(m.modality, "audio");
        assert!(
            m.carries_provenance_watermark,
            "SynthID + C2PA since May 2026 — GEN-09's disclosure source"
        );
        assert!(
            m.rough_cost_signal
                .as_deref()
                .is_some_and(|s| s.contains("1,000 characters")),
            "a per-1,000-character affordability hint is offered: {:?}",
            m.rough_cost_signal
        );
    }

    #[test]
    fn elevenlabs_poll_is_honestly_unreachable() {
        let provider = ElevenLabsProvider::with_key("k".to_string());
        let handle = JobHandle::new(JobId::mint(), "x".to_string());
        let err = run(provider.poll(&handle)).expect_err("a sync provider never polls");
        match err {
            GenError::InvalidRequest(msg) => assert!(
                msg.contains("synchronous"),
                "the error names WHY poll is unreachable: {msg}"
            ),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn elevenlabs_cancel_is_a_no_op_ok() {
        let provider = ElevenLabsProvider::with_key("k".to_string());
        let handle = JobHandle::new(JobId::mint(), "x".to_string());
        assert_eq!(
            run(provider.cancel(&handle)),
            Ok(()),
            "cancel is a no-op Ok — nothing remote to abandon, no .send() in the body"
        );
    }

    /// `connect` prefers the STORE's key over ambient env — deterministic
    /// regardless of the host's `ELEVENLABS_API_KEY`, because `Option::or_else`
    /// short-circuits before env is read.
    #[test]
    fn elevenlabs_connect_prefers_the_store_then_env() {
        let store = InMemoryKeyStore::with_validator(accept_any);
        store.set("store-key-123").expect("seed the store double");
        let provider =
            ElevenLabsProvider::connect(&store).expect("a seeded store builds a provider");
        assert_eq!(
            provider.api_key_for_test(),
            "store-key-123",
            "the STORE's key wins over any ambient ELEVENLABS_API_KEY"
        );

        // An empty store + (documented assumption) no ELEVENLABS_API_KEY in the
        // test process env resolves to None.
        if std::env::var("ELEVENLABS_API_KEY").is_ok() {
            eprintln!("skipping the None half: ELEVENLABS_API_KEY is set in the process env");
            return;
        }
        let empty = InMemoryKeyStore::with_validator(accept_any);
        assert!(
            ElevenLabsProvider::connect(&empty).is_none(),
            "no store key + no env key → None (honest absence, never a panic)"
        );
    }

    /// `pick_voice` prefers a `premade` voice over a cloned one regardless of
    /// order, and falls back to the first entry when no `category` is present.
    #[test]
    fn elevenlabs_voices_response_parses_and_picks_a_premade_voice_first() {
        let body = r#"{"voices":[
            {"voice_id":"cloned1","name":"My Clone","category":"cloned"},
            {"voice_id":"pre1","name":"Rachel","category":"premade"}
        ]}"#;
        let parsed: ElevenLabsVoicesResponse =
            serde_json::from_str(body).expect("documented voices shape parses");
        assert_eq!(
            pick_voice(parsed).expect("a premade voice exists"),
            "pre1",
            "premade is preferred over an earlier cloned voice"
        );

        // No category field at all → serde default tolerates its absence, and
        // the first-entry fallback returns it.
        let body2 = r#"{"voices":[{"voice_id":"abc123","name":"Test Voice"}]}"#;
        let parsed2: ElevenLabsVoicesResponse =
            serde_json::from_str(body2).expect("a category-less voice still parses");
        assert_eq!(
            pick_voice(parsed2).expect("first-entry fallback"),
            "abc123",
            "with no premade/category, the first voice is used"
        );
    }

    /// An empty voice list is an honest, actionable provider error naming the
    /// dashboard remedy — never a panic, never a silent fallback voice.
    #[test]
    fn elevenlabs_empty_voice_list_is_a_clean_provider_error_not_a_panic() {
        let parsed: ElevenLabsVoicesResponse =
            serde_json::from_str(r#"{"voices":[]}"#).expect("an empty list parses");
        let err = pick_voice(parsed).expect_err("an empty account is an error");
        match err {
            GenError::Provider(msg) => assert!(
                msg.contains("elevenlabs.io/app/voice-library"),
                "the error points the user at the remedy: {msg}"
            ),
            other => panic!("expected GenError::Provider, got {other:?}"),
        }
    }

    /// A non-2xx error body maps to `GenError::Provider` naming the status and
    /// the provider's own message — and CANNOT contain the API key, which the
    /// helper never even receives (T-34-01).
    #[test]
    fn elevenlabs_non_success_maps_to_provider_error_without_the_key() {
        let body = r#"{"detail":{"status":"invalid_api_key","message":"Invalid API key"}}"#;
        // The key exists in scope but is structurally NOT an argument to the
        // mapping helper — there is no path to interpolate it.
        let _key_in_scope = SENTINEL_KEY;
        let err = non_success_to_gen_error(401, body);
        let text = err.to_string();
        assert!(text.contains("401"), "the status is surfaced: {text}");
        assert!(
            text.contains("Invalid API key"),
            "the provider's message is surfaced verbatim: {text}"
        );
        assert!(
            !text.contains(SENTINEL_KEY),
            "the API key must NEVER appear in an error string: {text}"
        );
    }
}
