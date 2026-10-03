//! The provider-agnostic generation contract (Phase 31, GEN-05/07).
//!
//! Mirrors `agent_llm::transport`'s `LlmTransport` shape deliberately: native
//! `async fn` in a trait (no `async-trait` crate), used ONLY generically, with
//! the wire types living beside it. Everything here is pure data + one trait —
//! no sleeping, no timers, no runtime (31-RESEARCH.md's Anti-Pattern list keeps
//! `tokio` out of this crate entirely; the poll loop's backoff lives in the
//! host, `crates/app-core/src/generation_host.rs`).

/// Identifies a generation provider (`"fixture"`, later `"openai"`, …).
///
/// Deliberately a `String` newtype and **not** a closed enum: v5 adds a new
/// provider roughly every phase (32–35), and an enum would levy a
/// variant-per-phase churn tax on every `match` in the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ProviderId(pub String);

impl ProviderId {
    /// Borrow the id as a string slice — the form event payloads and the
    /// allow-list gate (Wave 4) actually compare.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One model a provider offers (GEN-07's `list_models` element).
///
/// Derives `Serialize` — unlike [`AssetRef`]/[`JobStatus`]/[`SubmitOutcome`],
/// this type is *meant* to cross IPC (Wave 4 surfaces the catalog to the
/// renderer). It carries no bytes and no credentials, only descriptive
/// metadata.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelInfo {
    /// Provider-side model identifier, as sent in [`GenRequest::model_id`].
    pub id: String,
    /// Human-facing name for a picker UI.
    pub label: String,
    /// `"image" | "video" | "audio" | "upscale"`. A string, not an enum, for
    /// the same reason [`ProviderId`] is: modalities keep arriving.
    pub modality: String,
    /// GEN-09: whether output from this model carries a provenance watermark
    /// (e.g. C2PA / SynthID). Rides the job-completion event payload to a
    /// user-facing disclosure point — deliberately NOT a `MediaBinItem` field
    /// (31-RESEARCH.md Pitfall 2: that schema has 90+ literal construction
    /// sites and no `Default`).
    pub carries_provenance_watermark: bool,
    /// Free-form affordability hint (`"~$0.04 / image"`), never a parsed price.
    pub rough_cost_signal: Option<String>,
}

/// Real, provider-produced asset bytes plus the extension they should land
/// under.
///
/// **No `Serialize` derive, on purpose.** Provider bytes must never cross IPC
/// (CLAUDE.md: "IPC carries commands/state/events only — no raw frames").
/// The host (`crates/app-core/src/generation_host.rs`) writes these to disk and
/// builds separate, byte-free payload views for the renderer (T-31-06).
#[derive(Debug, Clone, PartialEq)]
pub struct AssetRef {
    pub bytes: Vec<u8>,
    /// Extension WITHOUT a leading dot (`"png"`, `"mp4"`).
    pub suggested_ext: String,
}

/// A real, already-known-dimensions reference image conditioning a
/// generation (Phase 34.1, GEN-10). Always PNG bytes (produced upstream in
/// the host by `engine::encode_png_bytes`); width/height are ALREADY
/// KNOWN integers at every call site (whiteboard raster dims, the decoded
/// vision-frame's dims, or a probed MediaBinItem's dims) — this type never
/// re-parses a PNG header (Pattern 3, 34.1-RESEARCH.md). Deliberately
/// carries NO url/path/mime field (T-31-09 SSRF discipline, extended):
/// only bytes + dims, mirroring `AssetRef`'s "no endpoint data" shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Which tool/surface produced a request — and therefore which family of
/// endpoints it may reach.
///
/// **Phase 55.1 (D-05).** This REPLACES `ModelCaps.modality` as the routing
/// truth. With free-text model ids (D-01) there are no capabilities at submit
/// time — `model_caps()` returns `None` for every legitimate model the vendor
/// ships after this binary was compiled — so modality can no longer be looked
/// up from the model. It has to arrive WITH the request, from the one place
/// that genuinely knows it: which tool the caller invoked
/// (`generate_ai_image` → [`Image`](Self::Image), `generate_ai_video` →
/// [`Video`](Self::Video), `generate_ai_audio` → [`Audio`](Self::Audio)).
///
/// Kept as a closed enum rather than a string so a provider's `match` on it is
/// exhaustive at COMPILE time — the property that lets
/// `runway::build_submission` be a total function with no fallthrough arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestModality {
    /// A still image.
    Image,
    /// A clip.
    Video,
    /// ElevenLabs TTS. Structurally never routed to Runway; Runway's
    /// `build_submission` refuses it EXPLICITLY rather than by fallthrough, so
    /// a future mis-wiring is a loud `InvalidRequest` and not a video request
    /// built from a voice prompt.
    Audio,
}

/// What a provider wants generated.
///
/// **Deliberately minimal, and deliberately without any URL/endpoint/path
/// field** (T-31-09): provider base URLs are constants on the provider struct
/// itself, never request data, so a prompt-injected model can never steer a
/// request at an attacker-chosen host (SSRF). Phase 32 extends the field set
/// (size, seed, reference image, …) — it does NOT add a URL.
///
/// `background` (debug session `canvas-background-leaks-into-agent-vision`,
/// 2026-07-22) is the first such extension: whether the generated image
/// should carry real transparency. `Auto` (the `Default`) is BYTE-IDENTICAL
/// to the pre-existing behavior of every caller that does not set it — no
/// silent behavior change for the renderer/model-picker submit path.
///
/// # Phase 56 and the "no URL field" claim — restated, not weakened
///
/// [`source_video`](Self::source_video) is an ENDPOINT SELECTOR: its presence
/// is what routes a Runway submit to `video_to_video`. Read carelessly that
/// sounds like the endpoint field T-31-09 forbids, so state the difference
/// precisely. What the field holds is **bytes** — no scheme, no host, no path,
/// nothing parseable as a location. What it SELECTS is which
/// `&'static str` const endpoint name gets appended to a `&'static str` const
/// base URL, both of which live on the provider. A prompt injection that
/// controlled this field could at most change WHICH of Rudis's own two
/// hard-coded endpoints is used; it could not name a third, and it could not
/// name a host. The tripwire below destructures it for exactly that reason.
#[derive(Debug, Clone, PartialEq)]
pub struct GenRequest {
    pub provider: ProviderId,
    pub model_id: String,
    /// Phase 55.1 (D-05): the routing truth that used to be read off the
    /// model's `ModelCaps` row. See [`RequestModality`] — with `model_id` now
    /// free text there is no caps row to read, so the calling tool supplies it.
    pub modality: RequestModality,
    pub prompt: String,
    pub background: BackgroundMode,
    /// Phase 34.1 (GEN-10): a real reference image (canvas sketch / current
    /// preview frame / media-bin item) conditioning this generation —
    /// whole-image reference only (img2img), never a mask/region. `None`
    /// reproduces the exact pre-existing (unconditioned) request byte-for-byte
    /// — this is the "reference image" extension the `background` field's own
    /// doc comment (above) and the tripwire test below already anticipated.
    ///
    /// For a VIDEO provider this is the FIRST frame; see
    /// [`destination_image`](Self::destination_image) for the last.
    pub reference_image: Option<ReferenceImage>,
    /// Quick 260726-t5z: the image the generation should END on — the second
    /// half of a first+last-frame interpolation, which is what makes a real
    /// A-to-B transition (fly from the end of one shot to the start of the
    /// next) expressible at all. `reference_image` is the first frame, this is
    /// the last.
    ///
    /// **VIDEO-ONLY, like `background` is image-only.** A still image has no
    /// "last frame", so the TTS (`ElevenLabsProvider`) provider IGNORES this
    /// field entirely rather than inventing a meaning for it; the video path of
    /// `RunwayProvider` reads it (into the `promptImage` keyframe array's
    /// `position: "last"` entry). `RunwayProvider`'s still-image path REFUSES it
    /// rather than ignoring it, because a dropped conditioning frame is
    /// indistinguishable from a bad generation at the output.
    ///
    /// `None` — the state of every pre-existing call site — keeps the wire
    /// request byte-for-byte identical (the key is omitted entirely, never sent
    /// as `null`).
    pub destination_image: Option<ReferenceImage>,
    /// Phase 56 (GEN-11): the **source clip** to re-render — an existing
    /// timeline clip's own pixels, not a still to animate from.
    ///
    /// **Backend-produced bytes ONLY, never a path and never a URL.** These are
    /// the TRIM-RESPECTING extracted range (D-02) that Plan 04's extraction
    /// writes — exactly what the timeline clip currently shows, not the
    /// underlying source media. Carrying bytes rather than a location is the
    /// same T-31-09 discipline [`ReferenceImage`] follows: there is no field
    /// here a prompt injection could point at an arbitrary host or local path.
    ///
    /// **It is also the ENDPOINT SELECTOR.** Its presence — never the model id
    /// — is what routes a submit to `video_to_video`
    /// (`runway::build_video_edit_submission`). That is Phase 55.1's central
    /// consequence made structural: with free-text model ids there is no
    /// capability table to route from, so the routing truth has to be what the
    /// call CARRIES.
    ///
    /// `None` — the state of every pre-existing call site — reproduces the
    /// exact prior wire request byte-for-byte.
    pub source_video: Option<Vec<u8>>,
    /// Phase 56 (GEN-11 / D-08): conditioning frames for a video edit — "match
    /// this look, this wardrobe, this lighting" — capped at
    /// `runway::RUNWAY_V2V_MAX_REFERENCES`, enforced in the builder.
    ///
    /// Separate from [`reference_image`](Self::reference_image), which is the
    /// image→video FIRST FRAME and means something different.
    ///
    /// ⚠ **Currently unshippable, and the builder says so loudly rather than
    /// dropping them.** `56-01` proved the endpoint has no `references` field
    /// and `56-F1`'s $0.00 sweep left two array-typed candidates with neither
    /// confirmed, so there is no probe-confirmed key to serialize into; probe
    /// F-1b is the named unblocker. A non-empty value here is an
    /// `InvalidRequest` naming that gap, never a silently-ignored field.
    pub reference_images: Option<Vec<ReferenceImage>>,
}

/// Background treatment for a generation (debug session
/// `canvas-background-leaks-into-agent-vision`).
///
/// Named after the now-retired `OpenAiProvider`, which mapped each variant onto
/// the OpenAI Images API's own `background` parameter vocabulary
/// (`"transparent" | "opaque" | "auto"`). **No shipping provider interprets it
/// today** — Runway's request bodies have no background key, and
/// `FixtureGenProvider` ignores it (a canned asset has no background to treat).
/// The field is retained rather than removed because the frozen `GenRequest` is
/// a contract several seams fill in, and a transparent-background capability is
/// a live product ask; it is honestly inert until a provider expresses it. Kept
/// as a provider-agnostic enum rather than a raw string so a typo cannot
/// silently no-op (an unmapped string sent straight through would just be
/// ignored server-side by a real provider, with no compile-time signal).
///
/// **`Auto` is `Default`, on purpose:** it reproduces the exact wire request
/// this codebase sent BEFORE this field existed (no `background` key at all,
/// which OpenAI's own API defaults to `"auto"`). Every pre-existing call site
/// that does not opt in is therefore unaffected byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundMode {
    /// Let the provider/model decide from the prompt — the pre-existing
    /// (implicit) behavior.
    #[default]
    Auto,
    /// Real alpha transparency where the model judges there is no subject —
    /// correct for icons/stickers/logos/overlay graphics, especially anything
    /// derived from a Canvas sketch (itself annotations on a transparent
    /// surface, not an opaque picture). Known to risk unwanted transparent
    /// holes in some photographic/full-scene subjects — never forced
    /// unconditionally for that reason (see `crates/agent-gen/src/openai.rs`).
    Transparent,
    /// A filled-in, opaque background — correct for a photo, full scene, or
    /// background plate meant to fill the frame.
    Opaque,
}

/// The result of a `submit`, encoding GEN-05's "async is primary, sync is the
/// degenerate zero-poll case" **at the type level**.
///
/// A synchronous provider (image/TTS: bytes come back on the same call)
/// returns `Ready` and the caller never enters the poll loop at all. An
/// asynchronous provider (video/upscale) returns `Pending` with a handle to
/// poll. Neither the caller nor the registry has to ask "is this provider
/// sync?" — the type answers it.
#[derive(Debug, Clone, PartialEq)]
pub enum SubmitOutcome {
    Ready(Vec<AssetRef>),
    Pending(crate::job::JobHandle),
}

/// Where a submitted job stands. Three of the four variants are terminal —
/// only `Pending` causes the poll loop to sleep and go around again.
///
/// No `Serialize` derive (it can contain [`AssetRef`] bytes); the host
/// (`crates/app-core/src/generation_host.rs`) projects it into a byte-free
/// event payload instead.
#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Pending,
    Ready(Vec<AssetRef>),
    Failed(String),
    Cancelled,
}

impl JobStatus {
    /// `true` for every variant the poll loop stops on.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, JobStatus::Pending)
    }

    /// The stable lowercase wire word for this status — the single source of
    /// truth for the `state` field of the host's `gen:job` event payload
    /// (`app_core::generation_host::GEN_JOB_EVENT`), so the string can't drift
    /// between emitters.
    pub fn wire_state(&self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Ready(_) => "ready",
            JobStatus::Failed(_) => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

/// Generation-seam failures. Mirrors `agent_llm::LlmError`'s `thiserror` shape.
///
/// T-31-01 discipline is inherited: no variant ever carries key material, and
/// callers must not interpolate a key into `Provider`/`InvalidRequest`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GenError {
    /// GEN-08: the model is not on the clean/commercially-licensed allow list.
    /// The message NAMES the model id (Wave 4's SC-4 assertions match on it).
    #[error("model '{0}' is not on the clean-model allow list")]
    ModelNotAllowed(String),
    /// The provider rejected the call, or its response was unusable.
    #[error("generation provider error: {0}")]
    Provider(String),
    /// The request was malformed before it ever reached a provider.
    #[error("invalid generation request: {0}")]
    InvalidRequest(String),
}

/// The historic poll-loop cadence, now owned by the trait as the DEFAULT
/// [`GenProvider::poll_interval`].
///
/// This was a module-global `const POLL_INTERVAL` in
/// `src-tauri/src/generation.rs` (250ms), tuned for `FixtureGenProvider`'s
/// instant scripted countdown. Phase 33 moves it here so the poll cadence is a
/// per-provider property with a backward-compatible default: every pre-Phase-33
/// implementor keeps the exact 250ms it always had.
pub const DEFAULT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// The provider-agnostic generation seam (GEN-05).
///
/// # This trait is used GENERICALLY ONLY — never `dyn GenProvider`
///
/// Every call site takes `P: GenProvider` (see
/// `crates/app-core/src/generation_host.rs`'s
/// `spawn_poll_task`/`start_generation_job`).
/// This is the same choice `agent_llm::LlmTransport` made and for the same
/// reason: native `async fn` in a trait is not `dyn`-safe, and boxing it would
/// mean either an `async-trait` dependency or hand-rolled boxing gymnastics —
/// paid *now*, to solve a runtime-selection problem Phase 31 does not have
/// (it ships exactly one concrete provider, just as `AnthropicTransport` is
/// `LlmTransport`'s only real impl). Phase 32, when a second real provider
/// exists, picks the dispatch mechanism it actually needs (enum dispatch or
/// boxing) against a real requirement rather than a speculative one.
///
/// # Why `-> impl Future<..> + Send` rather than bare `async fn`
///
/// `agent_llm::LlmTransport` declares plain `async fn` (under
/// `#[allow(async_fn_in_trait)]`). This trait deliberately cannot: a bare
/// `async fn` in a trait returns a future with NO `Send` bound, and the host's
/// poll loop hands these futures to `tokio::spawn`
/// (`crates/app-core/src/generation_host.rs`; historically
/// `tauri::async_runtime::spawn`, a wrapper over the same thing), whose
/// `F: Future + Send + 'static` bound then fails to be satisfied (a hard
/// compile error, not a lint). `LlmTransport`
/// never hits this because its futures are always awaited in place, never
/// spawned. Return-position `impl Trait` in trait (stable since Rust 1.75) is
/// the zero-dependency way to state the bound; implementors still write an
/// ordinary `async fn` in their `impl` block. The alternative — the
/// `trait_variant` crate or `async-trait` boxing — would add a dependency to
/// express what one bound already says.
pub trait GenProvider: Send + Sync {
    /// This provider's stable id — also what the allow-list gate keys on.
    fn id(&self) -> ProviderId;

    /// GEN-07: the models this provider offers.
    fn list_models(&self) -> impl std::future::Future<Output = Result<Vec<ModelInfo>, GenError>> + Send;

    /// Start a generation. `Ready` = synchronous (zero-poll); `Pending` = the
    /// caller must poll the returned handle.
    fn submit(
        &self,
        req: GenRequest,
    ) -> impl std::future::Future<Output = Result<SubmitOutcome, GenError>> + Send;

    /// Check a pending job. Returning any terminal [`JobStatus`] ends the
    /// caller's poll loop.
    fn poll(
        &self,
        job: &crate::job::JobHandle,
    ) -> impl std::future::Future<Output = Result<JobStatus, GenError>> + Send;

    /// Ask the provider to abandon a job. Best-effort: the caller stops polling
    /// regardless of the outcome.
    fn cancel(
        &self,
        job: &crate::job::JobHandle,
    ) -> impl std::future::Future<Output = Result<(), GenError>> + Send;

    /// How long the app-side poll loop should sleep between `poll()` calls for
    /// THIS provider. Defaulted so every pre-Phase-33 implementor (Fixture,
    /// OpenAI) inherits the historic 250ms unchanged — an ADDITIVE,
    /// backward-compatible extension, not a break of the frozen contract. Real
    /// long-running providers (Veo: 11s–6min jobs) override to an
    /// API-citizenship cadence (GEN-05's own "poll on backoff" clause; 250ms
    /// flat would be ~1,440 requests per job).
    ///
    /// Non-`async`, non-`dyn`-affecting: a plain method with a default body, so
    /// it adds no future/`Send` bound and does not disturb the RPITIT methods
    /// above.
    fn poll_interval(&self) -> std::time::Duration {
        DEFAULT_POLL_INTERVAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobHandle, JobId};

    fn png_asset() -> AssetRef {
        AssetRef {
            bytes: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            suggested_ext: "png".to_string(),
        }
    }

    /// The GEN-05 shape proof: sync `Ready` carries assets whose bytes survive
    /// the round trip byte-identically, and both outcomes pattern-match
    /// exhaustively (no catch-all arm needed).
    #[test]
    fn submit_outcome_encodes_sync_as_zero_poll_and_carries_real_bytes() {
        let original = png_asset();
        let sync = SubmitOutcome::Ready(vec![original.clone()]);

        match &sync {
            SubmitOutcome::Ready(assets) => {
                assert_eq!(assets.len(), 1);
                assert_eq!(
                    assets[0].bytes, original.bytes,
                    "sync Ready carries byte-identical asset content"
                );
                assert_eq!(assets[0].suggested_ext, "png");
            }
            SubmitOutcome::Pending(_) => panic!("a sync provider's submit returns Ready"),
        }

        let async_outcome = SubmitOutcome::Pending(JobHandle::new(
            JobId::mint(),
            "provider-side-token".to_string(),
        ));
        assert!(
            matches!(async_outcome, SubmitOutcome::Pending(_)),
            "an async provider's submit returns Pending with a pollable handle"
        );
    }

    #[test]
    fn job_status_terminality_and_wire_words_are_stable() {
        assert!(!JobStatus::Pending.is_terminal(), "only Pending keeps polling");
        assert!(JobStatus::Ready(vec![png_asset()]).is_terminal());
        assert!(JobStatus::Failed("boom".into()).is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());

        // These four strings are the `state` field of the gen:job event; the
        // frontend contract depends on them, so pin them literally.
        assert_eq!(JobStatus::Pending.wire_state(), "pending");
        assert_eq!(JobStatus::Ready(vec![]).wire_state(), "ready");
        assert_eq!(JobStatus::Failed(String::new()).wire_state(), "failed");
        assert_eq!(JobStatus::Cancelled.wire_state(), "cancelled");
    }

    /// Wave 4's SC-4 assertions match on the model id inside this message, so
    /// the Display shape is a contract, not an implementation detail.
    #[test]
    fn gen_errors_display_cleanly_and_name_the_rejected_model() {
        let not_allowed = GenError::ModelNotAllowed("sketchy-model-v1".to_string());
        let msg = not_allowed.to_string();
        assert!(
            msg.contains("sketchy-model-v1"),
            "the rejection names the model id: {msg}"
        );
        assert!(
            msg.contains("allow list"),
            "the rejection names the reason: {msg}"
        );

        assert_eq!(
            GenError::Provider("upstream 503".into()).to_string(),
            "generation provider error: upstream 503"
        );
        assert_eq!(
            GenError::InvalidRequest("empty prompt".into()).to_string(),
            "invalid generation request: empty prompt"
        );
    }

    /// T-31-09: a `GenRequest` has no field a prompt injection could use to
    /// point the seam at an arbitrary host. This test is a tripwire — if a
    /// later phase adds a URL-ish field, the exhaustive destructuring below
    /// fails to compile and forces a deliberate security review. `background`
    /// (debug session `canvas-background-leaks-into-agent-vision`) is exactly
    /// the kind of non-URL extension the original T-31-09 comment anticipated
    /// ("size, seed, reference image, …") — it is destructured here too, so
    /// this tripwire keeps covering every field, not just the original three.
    #[test]
    fn gen_request_carries_no_endpoint_field() {
        let req = GenRequest {
            provider: ProviderId("fixture".to_string()),
            model_id: "fixture-image-v1".to_string(),
            modality: RequestModality::Image,
            prompt: "a red square".to_string(),
            background: BackgroundMode::Auto,
            reference_image: None,
            destination_image: None,
            source_video: None,
            reference_images: None,
        };
        let GenRequest {
            provider,
            model_id,
            modality,
            prompt,
            background,
            reference_image,
            destination_image,
            source_video,
            reference_images,
        } = &req;
        assert_eq!(provider.as_str(), "fixture");
        assert_eq!(model_id, "fixture-image-v1");
        // Phase 55.1: `modality` is a CLOSED enum with no url/path/host
        // inhabitant, so it extends the tripwire's cover without widening the
        // T-31-09 surface — the same standing this field's review recorded for
        // `background` and `reference_image`.
        assert_eq!(*modality, RequestModality::Image);
        assert_eq!(prompt, "a red square");
        assert_eq!(*background, BackgroundMode::Auto, "the default, unless a caller opts in");
        assert!(
            reference_image.is_none(),
            "no reference by default — an unconditioned request is byte-identical to the pre-34.1 shape"
        );
        // Quick 260726-t5z: the destination frame is a `ReferenceImage` like the
        // first frame, so it inherits that type's OWN tripwire below (bytes +
        // dims only, no url/path/mime). Absent by default for the same
        // byte-identity reason.
        assert!(
            destination_image.is_none(),
            "no destination frame by default — the `lastFrame` key stays skip-serialized"
        );
        // Phase 56 (GEN-11): both v2v slots carry BYTES, never a path or a URL
        // — the same shape `ReferenceImage` was cleared under, extended to a
        // clip. `source_video` is additionally the endpoint SELECTOR, which is
        // a routing fact rather than a URL surface: it decides WHICH const
        // endpoint name is appended to the const base URL, and holds no host of
        // its own.
        assert!(
            source_video.is_none(),
            "no source clip by default — its presence is what routes to \
             video_to_video, so a default of None keeps every pre-56 caller on \
             the byte-identical legacy path"
        );
        assert!(
            reference_images.is_none(),
            "no v2v conditioning frames by default"
        );
    }

    /// T-31-09 (extended, Phase 34.1): `ReferenceImage` carries ONLY bytes +
    /// already-known dims — no url/path/mime field a prompt injection could use
    /// to point conditioning at an attacker-chosen host or an arbitrary local
    /// path. This tripwire destructures every field, so a later url/path-shaped
    /// field addition fails to compile and forces a deliberate security review
    /// (mirrors `gen_request_carries_no_endpoint_field`).
    #[test]
    fn reference_image_carries_only_bytes_and_known_dims() {
        let img = ReferenceImage {
            bytes: vec![1, 2, 3],
            width: 10,
            height: 20,
        };
        let ReferenceImage {
            bytes,
            width,
            height,
        } = &img;
        assert_eq!(bytes, &vec![1, 2, 3]);
        assert_eq!(*width, 10);
        assert_eq!(*height, 20);
    }

    /// `BackgroundMode::default()` is `Auto` — the exact pre-existing (implicit)
    /// behavior, so every caller that does not opt in is byte-for-byte
    /// unaffected by this field's addition.
    #[test]
    fn background_mode_default_is_auto() {
        assert_eq!(BackgroundMode::default(), BackgroundMode::Auto);
    }

    /// A minimal in-test `GenProvider` WITHOUT a `poll_interval` override
    /// inherits `DEFAULT_POLL_INTERVAL` (250ms) — proving the trait change is
    /// backward-compatible by default (Fixture/OpenAI touch nothing).
    #[test]
    fn default_poll_interval_is_the_historic_250ms() {
        struct Bare;
        impl GenProvider for Bare {
            fn id(&self) -> ProviderId {
                ProviderId("bare".to_string())
            }
            async fn list_models(&self) -> Result<Vec<ModelInfo>, GenError> {
                Ok(Vec::new())
            }
            async fn submit(&self, _req: GenRequest) -> Result<SubmitOutcome, GenError> {
                Ok(SubmitOutcome::Ready(Vec::new()))
            }
            async fn poll(&self, _job: &JobHandle) -> Result<JobStatus, GenError> {
                Ok(JobStatus::Pending)
            }
            async fn cancel(&self, _job: &JobHandle) -> Result<(), GenError> {
                Ok(())
            }
            // NOTE: no poll_interval override — the whole point of this test.
        }
        assert_eq!(DEFAULT_POLL_INTERVAL, std::time::Duration::from_millis(250));
        assert_eq!(
            Bare.poll_interval(),
            DEFAULT_POLL_INTERVAL,
            "an implementor with no override inherits the historic 250ms"
        );
    }

    #[test]
    fn model_info_serializes_without_bytes() {
        let model = ModelInfo {
            id: "fixture-image-v1".to_string(),
            label: "Fixture Image v1".to_string(),
            modality: "image".to_string(),
            carries_provenance_watermark: true,
            rough_cost_signal: Some("free (fixture)".to_string()),
        };
        let json = serde_json::to_string(&model).expect("ModelInfo serializes for IPC");
        assert!(json.contains("\"carries_provenance_watermark\":true"));
        assert!(
            !json.contains("bytes"),
            "a model catalog entry never carries asset bytes: {json}"
        );
    }
}
