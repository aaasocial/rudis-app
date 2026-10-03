//! External-generation seam (Phase 31, GEN-05..09).
//!
//! This crate owns the provider-agnostic abstraction over EXTERNAL generation
//! services (image / video / audio / upscale): the `GenProvider` trait, its
//! request/response wire types, the async job registry, and the GEN-08
//! allow-list gate. It is one of exactly TWO sanctioned network-egress crates
//! in the workspace (the other is `agent-llm`, which owns LLM chat) — every
//! generation-provider HTTP call is confined here, so `crates/core` and the
//! edit-and-export path stay network-free (CLAUDE.md "Offline core": *"Only
//! future AI/MCP calls may touch the network"*). The structural proof of that
//! confinement lives in `crates/ffi/tests/offline_guard.rs`.
//!
//! # Phase 31 scope: fixture-proven, no real provider
//!
//! Phase 31 ships the seam and proves it end-to-end against a
//! `FixtureGenProvider` that performs ZERO network I/O — it holds no
//! `reqwest::Client` and opens no socket, the same "always-compiled test
//! double" precedent as `agent_llm::FixtureTransport` (PROVENANCE Entry 7/9).
//! REAL providers arrive in Phase 32+ and only behind the GEN-08 allow-list /
//! licensing gate; no provider credentials, endpoints, or network calls exist
//! in this phase.
//!
//! # Module layout (filled in by later Phase-31 waves)
//!
//! - `provider` — `GenProvider`, `GenRequest`, `SubmitOutcome`, `JobStatus`,
//!   `ModelInfo`, `AssetRef`, `ProviderId`, `GenError` (Wave 2 — landed);
//!   `BackgroundMode` added post-launch (debug session
//!   `canvas-background-leaks-into-agent-vision`, 2026-07-22)
//! - `job` — `JobId`, `JobHandle`, `JobRecord`, `JobRegistry` (Wave 2 — landed)
//! - `fixture` — `FixtureGenProvider`, always compiled (Wave 3 — landed)
//! - `allow_list` — the GEN-08 clean-model gate: `AllowList`,
//!   `phase31_allow_list`, and `submit_checked`, the ONE gate every submit path
//!   goes through (Wave 4 — landed)
//!
//! # Phase 42.1: ONE generation provider, not four
//!
//! Phases 32/33/34 each added a direct provider (`openai`, `veo`, `elevenlabs`).
//! Phase 42.1 collapses image AND video onto a single provider — `runway` — and
//! **DELETES `openai.rs` and `veo.rs` outright** rather than leaving them
//! present-but-unreachable. Deleted code is recoverable from git; a
//! bypassed-but-still-compiled provider is a lie about what the app does, and it
//! keeps two credential paths alive that nothing can reach. `elevenlabs` is
//! untouched: audio is a different provider serving a different modality.
//!
//! Wave 1 was the scaffold; Wave 2 landed the contracts above; Wave 3 landed
//! `fixture` (the phase's one concrete provider); Wave 4 landed `allow_list`.
//! The offline guard was strengthened in the same commit the egress dependency
//! landed (31-RESEARCH.md Pitfall 1).
//!
//! # What this crate deliberately does NOT do
//!
//! No sleeping, no timers, no runtime, no desktop-shell framework. The job POLL
//! LOOP — the part that waits 250ms between provider checks and emits events —
//! lives in the HOST: `crates/app-core/src/generation_host.rs`, which owns
//! `tokio` and the `AppCtx` handle (Phase 54.1 moved it there from the
//! since-retired `src-tauri/src/generation.rs`).
//! That split is what keeps this crate as dependency-light as `agent-llm`
//! (whose `run_turn` is likewise `async fn` with no `tokio` dependency at all).

pub mod allow_list;
pub mod dispatch;
pub mod elevenlabs;
pub mod fixture;
pub mod job;
pub mod provider;
pub mod runway;

// Flat re-exports, mirroring `agent-llm/src/lib.rs`'s shape: consumers write
// `agent_gen::JobRegistry`, not `agent_gen::job::JobRegistry`.
pub use allow_list::{production_allow_list, submit_checked, AllowList, AllowListEntry};
pub use dispatch::ConcreteGenProvider;
pub use elevenlabs::{ElevenLabsProvider, ELEVENLABS_PROVIDER_ID, ELEVENLABS_TTS_MODEL};
pub use fixture::{FixtureGenProvider, FIXTURE_PROVIDER_ID};
pub use job::{JobHandle, JobId, JobRecord, JobRegistry};
pub use provider::{
    AssetRef, BackgroundMode, GenError, GenProvider, GenRequest, JobStatus, ModelInfo, ProviderId,
    ReferenceImage, RequestModality, SubmitOutcome, DEFAULT_POLL_INTERVAL,
};
// Phase 55.1 plan 06 removed four names from this list — the capability enum,
// the intent -> model map, the map's lookup function and the
// expensive-transition fallback const — because plans 01-04 left them with zero
// readers and a dead indirection beside the free-text `model` path is the
// "second source of truth" anti-pattern this phase exists to close. What stays
// is the ADVISORY surface: `RUNWAY_MODELS` + `ModelCaps` + `model_caps` /
// `model_label` feed the spend prompt's price, the disclosure label and
// `catalog()`, and refuse nothing (D-01).
// Phase 56 (GEN-11) adds the `video_to_video` wire surface to this list. The
// two video-input types are re-exported ALONGSIDE their one constructor each
// (`video_data_uri`; `RunwayUploadedAsset` has none by design) so a consumer
// reaching for the type finds the only legal way to build one in the same
// import — the flat re-export is where a caller looks first, and a type without
// its constructor invites someone to add one.
pub use runway::{
    advisory_video_edit_model, build_image_to_video_request, build_submission,
    build_text_to_image_request, build_video_edit_submission, data_uri, model_caps,
    model_label, output_uri_host,
    projected_video_data_uri_len, runway_status_is_terminal, validate_upload_url, video_data_uri,
    video_transport_for_len,
    KeyframePairSupport, ModelCaps, RunwayDataUri, RunwayImageToVideoRequest, RunwayKeyframe,
    RunwayKeyframePosition, RunwayPromptImage, RunwayProvider, RunwayReferenceImage,
    RunwayRequestBody, RunwaySubmission, RunwayTextToImageRequest, RunwayUploadedAsset,
    RunwayUploadRequest, RunwayUploadsResponse, RunwayVideoDataUri, RunwayVideoInput,
    RunwayVideoToVideoRequest, VideoTransport,
    RUNWAY_API_VERSION, RUNWAY_BODY_SCAFFOLD_RESERVE_BYTES,
    RUNWAY_DOCUMENTED_VIDEO_ASSET_CAP_BYTES, RUNWAY_DURATION_SECONDS,
    RUNWAY_ENDPOINT_VIDEO_TO_VIDEO,
    RUNWAY_MAX_REQUEST_BODY_BYTES, RUNWAY_MAX_UPLOAD_BYTES, RUNWAY_MAX_VIDEO_DATA_URI_BYTES,
    RUNWAY_MIN_UPLOAD_BYTES, RUNWAY_MODELS, RUNWAY_POLL_INTERVAL_FLOOR, RUNWAY_PROVIDER_ID,
    RUNWAY_RATIO, RUNWAY_RATIO_IS_UNIVERSAL, RUNWAY_REFERENCE_TAG,
    RUNWAY_TERMINAL_STATUS_NOTICE, RUNWAY_VERSION_HEADER,
    RUNWAY_V2V_INPUT_MAX_FPS, RUNWAY_V2V_INPUT_MAX_SECONDS, RUNWAY_V2V_INPUT_MIN_SECONDS,
    RUNWAY_V2V_MAX_REFERENCES,
};
