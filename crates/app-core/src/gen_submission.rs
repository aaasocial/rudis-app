//! The EXTERNAL-provider generation bridge (Phase 45, XTRC-01 — plan 45-11).
//!
//! A SEPARATE trait from [`AppCtx`](crate::AppCtx), not one more method on it:
//! [`AppCtx`](crate::AppCtx) is the common-case synchronous managed-state
//! surface every migrated function needs, while this is the narrow, mostly-async
//! surface that only [`crate::generation_bridge`]'s three modality handlers use.
//! Keeping them apart means the other ~40 relocated functions never have to name
//! an `agent_gen` type, and 45-14 can re-privatize one without touching the
//! other.
//!
//! # The coupling this exists to resolve
//!
//! `run_generate_ai_image` / `_video` / `_audio` call, in `src-tauri`:
//!
//! | Callee | Where it lives | Why it cannot move |
//! |---|---|---|
//! | `generation::generate_runway_image_for_agent` / `_video` / `_audio` | `src-tauri/src/generation.rs` | `pub(crate)`, `AppHandle<R>`-generic, and `generation.rs` (10,201 lines) is explicitly OUT of this phase's scope |
//! | `resolve_reference_image` | `src-tauri/src/lib.rs` | reads `app.try_state::<Mutex<WhiteboardAspect>>()` and awaits the whiteboard/vision snapshot rasterizers, all of which belong to the `run_agent_turn` vision path (45-12/45-13) |
//! | ~~`parse_video_shape_stage`~~ | ~~`src-tauri/src/lib.rs`~~ | **HISTORICAL.** A one-line delegator to the shape/stage → capability translation. Both the schema fields and the translation are gone (Phase 55.1, plans 03 and 06), and `src-tauri/` itself died with Phase 55's cutover. |
//!
//! `app-core` cannot depend back on `src-tauri`, so each of those five became a
//! method here (four survive — see the struck row above), implemented in
//! `src-tauri` — the crate `generation.rs`'s
//! `pub(crate)` already scopes to. **That is the whole point of the design:
//! ZERO visibility widening in `generation.rs`.** Its three seam functions and
//! its three `AgentGenerated*` structs are byte-untouched by this plan (threat
//! T-45-07), in deliberate contrast to the `AgentSession` case (T-45-02), where
//! widening genuinely was unavoidable.
//!
//! # Shape: `agent_llm::LlmTransport`, copied
//!
//! `crates/agent-llm/src/transport.rs`:
//!
//! ```text
//! #[allow(async_fn_in_trait)]
//! pub trait LlmTransport {
//!     async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse, LlmError>;
//! }
//! ```
//!
//! Generic-only (`<C: AppCtx + GenSubmission>`), never `dyn`, native `async fn`
//! under `#[allow(async_fn_in_trait)]`, no `async-trait` crate. Exactly the
//! in-repo precedent 45-RESEARCH.md identified as the template to copy rather
//! than invent.
//!
//! ## Why a native `async fn` is safe HERE but was not for [`AppCtx::run_blocking`]
//!
//! 45-10 discovered — after two rejected forms — that a trait method returning a
//! future must return `Pin<Box<dyn Future + Send>>` when it is awaited inside a
//! `#[tauri::command] async fn` carrying a `State<'_, T>` parameter: both
//! `async fn` (AFIT) and `-> impl Future + Send` (RPITIT) carry `&self`'s
//! lifetime into the future type, and Tauri makes the `Send` obligation
//! higher-ranked over that lifetime. That reasoning is recorded in full on
//! [`AppCtx::run_blocking`](crate::AppCtx::run_blocking).
//!
//! It does not apply to these methods, and the difference is structural rather
//! than lucky: every future produced here is created AND driven to completion
//! inside one SYNCHRONOUS call — `run_generate_ai_*` are `fn`, not `async fn`,
//! and they await through [`AppCtx::block_on`](crate::AppCtx::block_on) under
//! the pre-existing `block_in_place` guard. The future is therefore never part
//! of an enclosing command future's state machine, so no `Send` obligation
//! (higher-ranked or otherwise) is ever imposed on it. Verified by building the
//! workspace, not assumed.
//!
//! [`GeneratedAsset`] exists because `generation.rs`'s three result structs —
//! `AgentGeneratedImage`, `AgentGeneratedVideo`, `AgentGeneratedAudio` — are
//! `pub(crate)` (so they cannot cross the crate boundary as return types) AND
//! identically shaped. The Tauri-side impl does a two-field copy from whichever
//! it got back.

/// What a successful external generation landed: the media-bin ids the landing
/// bridge created, and GEN-09's provenance-watermark flag.
///
/// The `app-core`-owned mirror of `generation.rs`'s `AgentGeneratedImage` /
/// `AgentGeneratedVideo` / `AgentGeneratedAudio`, which are `pub(crate)` and
/// identically shaped. Field names and semantics are the same, so the
/// `src-tauri` impls copy across without a single rename.
#[derive(Debug, Clone)]
pub struct GeneratedAsset {
    pub media_item_ids: Vec<String>,
    pub carries_provenance_watermark: bool,
}

/// Which edge of a timeline clip a conditioning frame comes from (Quick
/// 260726-t5z). Named rather than a bool so a call site cannot silently mean the
/// opposite edge — inverting these is exactly how a transition would generate
/// backwards while still looking correct in review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipEdge {
    /// The clip's FIRST visible frame — `in_us`. The right END for a transition
    /// arriving INTO this clip.
    Start,
    /// The clip's LAST visible frame. `out_us` is EXCLUSIVE, so this resolves
    /// one microsecond before it — the exact frame the viewer last sees. The
    /// right START for a transition leaving this clip.
    End,
}

/// Phase 34.1 (GEN-10): the CLOSED set of reference sources the agent may name
/// (T-34.1-01: an unrecognized string is a clean `Err` before any resolution
/// work — never a free path/string reaching disk). `Media` carries the already-
/// validated MediaBin id (never a filesystem path).
///
/// Quick 260726-t5z adds the two CLIP sources. They exist because `Media`
/// resolves a media item's frame 0, which CANNOT express "the last frame the
/// viewer actually sees": a trimmed clip's `out_us` is not its media's end, so a
/// transition seeded from frame 0 would start on a frame nobody watched. These
/// carry a validated timeline Clip id (never a path), resolved against the live
/// timeline at tool-call time.
#[derive(Debug, Clone, PartialEq)]
pub enum ReferenceSource {
    Sketch,
    Frame,
    Media(String),
    Clip(String, ClipEdge),
}

// `GenSubmission` is used ONLY generically
// (`fn run_generate_ai_image<C: AppCtx + GenSubmission>(..)`), never as
// `dyn GenSubmission`, so native `async fn` in a trait is exactly the case this
// lint says is safe to suppress. Copied verbatim in shape from
// `agent_llm::LlmTransport`, this codebase's own precedent (see the module doc);
// no `async-trait` dependency is needed.
#[allow(async_fn_in_trait)]
pub trait GenSubmission {
    /// Submit an image generation through the ONE governed submit path
    /// (`generate_runway_image_for_agent`: the GEN-08 allow-list gate, the
    /// provider dispatch and the landing bridge all live INSIDE it).
    ///
    /// Never a new submit path. `model` is the CALLER's free-text Runway image
    /// model id (Phase 55.1, D-01/D-02/D-10) — the string the agent chose, or
    /// the one the user named in chat — and it is passed through VERBATIM to
    /// `GenRequest.model_id`. Nothing here validates that the id exists: with
    /// the roster open by owner decision, that judgement belongs to Runway's own
    /// server-side model enum, which answers an unknown id with its own `400`.
    /// It replaces the `gen4_image` const this seam used to hardcode through a
    /// pinned capability word, which was the reason `gen4_image_turbo` and the
    /// other image models were unreachable.
    async fn submit_image(
        &self,
        prompt: String,
        model: String,
        background: agent_gen::BackgroundMode,
        reference: Option<agent_gen::ReferenceImage>,
    ) -> Result<GeneratedAsset, String>;

    /// Submit a video generation through `generate_runway_video_for_agent`.
    ///
    /// `model` is the CALLER's free-text Runway model id, on exactly the terms
    /// [`GenSubmission::submit_image`]'s doc states: chosen by the agent or
    /// named by the user, carried through to `GenRequest.model_id` unchanged,
    /// validated only by Runway. It replaces the optional capability word this
    /// method used to take, which existed so a caller could name an OUTCOME it
    /// was not trusted to turn into a model (Phase 55.1 D-11 deletes both axes
    /// of that vocabulary; plan 06 deleted the enum itself).
    async fn submit_video(
        &self,
        prompt: String,
        model: String,
        reference: Option<agent_gen::ReferenceImage>,
        destination: Option<agent_gen::ReferenceImage>,
    ) -> Result<GeneratedAsset, String>;

    /// Phase 56 (GEN-11): submit a video-to-video EDIT of an existing timeline
    /// clip through the ONE governed submit path
    /// (`generate_runway_video_edit_for_agent`, which calls
    /// `start_generation_job` and nothing else).
    ///
    /// `source_clip_id` is a store-validated timeline `Clip` id — **never a
    /// path and never a URL** (T-56-INJ-04: no path-shaped parameter exists on
    /// this seam at all, so there is nothing for a prompt injection to point at
    /// a file or a host). The host resolves the clip's CURRENT trim
    /// (`in_us..out_us`, `out_us` **EXCLUSIVE** — D-02), refuses a range outside
    /// the probe-derived window (D-01, [`crate::clip_edit_window_check`]),
    /// extracts exactly that range video-only
    /// ([`crate::extract_clip_range_mp4`]), and submits the bytes. The result
    /// lands an undoable MediaBinItem through the SAME landing bridge every
    /// generation tool uses (D-06) — **never replace-in-place**, so the
    /// before/after comparison survives.
    ///
    /// # The ORDERING CONTRACT, which is a spend property (T-56-SPEND-06)
    ///
    /// Every LOCAL refusal must cost **$0.00** and must happen before the work
    /// the next step would waste. The host implements, in this order:
    ///
    /// 1. **the reference gate** ([`crate::video_edit_reference_check`]) —
    ///    cheapest, pure, and it touches neither the store nor a decoder;
    /// 2. **clip lookup** — an unknown id is a clean `Err` naming it;
    /// 3. **the window check** — `clip_edit_window_check(out_us - in_us)`;
    /// 4. **the extraction** — only now does a decoder or an encoder run;
    /// 5. **reference resolution** through the EXISTING
    ///    [`GenSubmission::resolve_reference_image`], so a Canvas-annotated
    ///    preview frame (`ReferenceSource::Frame`, D-09) would ride the same
    ///    path with zero new raster code;
    /// 6. **the one submit entry** — the only step that can spend.
    ///
    /// A count error must therefore cost zero raster work, and a window error
    /// zero decode work.
    ///
    /// # `references` is a SLOT, and today it is BLOCKED — by design, loudly
    ///
    /// `crate::video_edit_reference_check` refuses **any non-empty set**, not
    /// only an over-cap one, and names why. `/v1/video_to_video` has no
    /// probe-confirmed reference field: 56-01 proved `references` does not
    /// exist, 56-F1 narrowed six candidate spellings to two, and **56-F1b then
    /// enumerated both fully and still crowned neither** — `keyframes` and
    /// `promptImage` are both `maximum: 5` arrays of `{uri, <anchor>}` image
    /// references, which is precisely what destroyed the discriminator. The
    /// remaining question is BEHAVIOURAL ("which does the model consult") and no
    /// validation error at any price can answer it; **56-09**, the owner-gated
    /// paid run, is the only remaining route.
    ///
    /// Refusing rather than dropping is the whole point: 42.1-04 caught the
    /// alternative live, when an attached-but-uncited reference returned HTTP
    /// **200** with a plausible unconditioned result no caller could detect.
    /// **GEN-11's five-reference / Canvas clause is unmet, and the seam says so
    /// instead of pretending otherwise.**
    ///
    /// # `model` (post-55.1, open selection) is OPTIONAL here
    ///
    /// A free-text Runway model id passed through **VERBATIM** — no roster
    /// validation anywhere on this path (55.1 decision 2). `None` means the host
    /// substitutes the **ADVISORY default**
    /// ([`agent_gen::advisory_video_edit_model`]), which is priced data, not a
    /// gate. An off-roster id submits; its price is unknown at the spend gate
    /// and is disclosed as such rather than guessed.
    ///
    /// **Why `Option` and not the required `String` both siblings take:** it
    /// keeps [`crate::resolved_model_for_tool_input`]'s `generate_ai_video_edit`
    /// arm honest. That arm falls back to the same
    /// `advisory_video_edit_model()` when a call names no model, and 56-05 wrote
    /// the contract at its own site: **the fallback may only survive if this
    /// seam really submits that default for a model-less call.** It does — see
    /// `generate_runway_video_edit_for_agent` (plan 06) step 1 — so the
    /// disclosure names the model that actually ran, never one no call
    /// submitted.
    async fn submit_video_edit(
        &self,
        prompt: String,
        source_clip_id: String,
        model: Option<String>,
        references: Vec<ReferenceSource>,
    ) -> Result<GeneratedAsset, String>;

    /// Submit a text-to-speech generation through
    /// `generation::generate_elevenlabs_audio_for_agent`. `prompt` IS the
    /// literal text to speak (T-34-11).
    async fn submit_audio(&self, prompt: String) -> Result<GeneratedAsset, String>;

    /// Resolve a chosen [`ReferenceSource`] to real PNG bytes, entirely
    /// host-side, at tool-call time on a FRESH store snapshot (Phase 34.1,
    /// GEN-10 Pattern 2).
    ///
    /// # Why this is a HOST capability and not code in this crate
    ///
    /// Two of the four sources are pure host surface: `Sketch` reads
    /// `app.try_state::<Mutex<WhiteboardAspect>>()` — Tauri managed state with
    /// no [`AppCtx`](crate::AppCtx) accessor — and both `Sketch` and `Frame`
    /// then await `build_whiteboard_snapshot_png` /
    /// `build_vision_snapshot_png`, the SAME Pattern-1 byte producers the
    /// agent's own vision snapshot uses. Those producers belong to
    /// `run_agent_turn`'s vision path, which moves in 45-12/45-13, not here.
    /// Bridging keeps the produced bytes byte-identical to what the agent saw —
    /// which is the entire point of the feature — instead of re-deriving them.
    ///
    /// `TauriAppCtx`'s impl is literally `resolve_reference_image(self.app,
    /// self.store, source).await`, the call the three handlers made before they
    /// moved. No reference bytes ever cross the renderer/IPC boundary
    /// (T-34.1-09 / CLAUDE.md rule 4), and that is unchanged: this method is
    /// called from inside the same backend-side async block it always was.
    async fn resolve_reference_image(
        &self,
        source: ReferenceSource,
    ) -> Result<agent_gen::ReferenceImage, String>;

    // A fifth method stood here until Phase 55.1 plan 06: the sync translation
    // from `generate_ai_video`'s `shape`/`stage` schema fields onto a closed
    // capability enum (Phase 42.3 item (G)). 55.1-03 deleted both schema fields
    // and rewired both of its callers — the dispatch path and the disclosure —
    // onto the caller's own `model` string, leaving it with no production caller
    // at all; 55.1-06 removed it from the trait, from `app-core`, and from the
    // ffi impl in the same commit, because a trait method with no callers is a
    // seam a future host will re-implement and then find something to feed.
    //
    // The property it enforced survives, moved rather than dropped: a lingering
    // `shape` / `stage` / `intent` key is STILL a loud, pre-spend refusal, raised
    // by `crate::generation_bridge`'s own `model_from_input` and naming the
    // `model` field that replaced them, so a model on stale context can correct
    // itself instead of failing identically forever.
}
