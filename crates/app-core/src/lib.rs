//! Rudis domain/agent-tool dispatch logic, extracted from the then-live
//! `src-tauri/src/lib.rs` behind the [`AppCtx`] trait (Phase 45, XTRC-01).
//!
//! Shell-agnostic: this crate has ZERO desktop-shell-framework dependency,
//! enforced structurally (not by convention) two ways —
//! `cargo tree -p app-core -i tauri` must print *"did not match any packages"*,
//! and `crates/ffi/tests/offline_guard.rs` names `app-core` in both its
//! network-egress negative list and its own direct-manifest guard.
//!
//! The HOST that implements [`AppCtx`] today is `crates/ffi` — the C ABI cdylib
//! the native C# shell loads (`FfiAppCtx`, `crates/ffi/src/ctx.rs`). The Tauri
//! shell was the first implementation and was retired at Phase 55 (GATE-07,
//! plan 55-10); the trait carried across unchanged, which was the whole point.
//!
//! # State of the extraction
//!
//! Plan 45-03 stood this crate up as a PURE ADDITION (the trait + a test fake);
//! 45-04 relocated the [`session`] types. **Plan 45-05 is the first batch that
//! moved real functions**: the nine zero-`AppHandle` agent-tool leaves in
//! [`inspect`] and [`transcribe`], plus the shared composite leaves in
//! [`compose`] they depend on. **Plan 45-06 is the first batch that actually
//! USES [`AppCtx`]**: [`project_store`] moved wholesale and [`project`]'s three
//! pairs became `<C: AppCtx>(ctx: &C, ..)`, against the ONE concrete
//! `TauriAppCtx` in `src-tauri`. Batches 45-07 .. 45-13 move the rest of the ~45
//! `run_*`/`handle_*` pairs onto [`AppCtx`], leaf-first, behind re-export shims
//! in `src-tauri`. **Plan 45-09 is the first batch that reaches a SECOND external
//! sidecar**: [`tracking`] drives `engine::tracking`'s OpenCV CSRT/KCF subprocess,
//! a process boundary distinct from the FFmpeg engine sidecar. It landed
//! [`matte`] and [`audio_sync`] alongside it and needed NO new [`AppCtx`] method
//! — the first batch since 45-05 for which the trait was already sufficient.
//! **Plan 45-11 added a SECOND trait rather than more [`AppCtx`] methods**:
//! [`GenSubmission`], the external-generation bridge, so [`generation_bridge`]'s
//! three modality handlers could relocate while `src-tauri/src/generation.rs`
//! (10,201 lines, explicitly out of scope) stays byte-untouched and fully
//! `pub(crate)`. It is also the batch that gave this crate its `agent-gen`
//! dependency. **Plan 45-12 moved the central mutation plumbing** —
//! [`dispatch`]'s `dispatch_command_inner` / `undo_inner` / `redo_inner` — and
//! added a THIRD trait, [`SpendPolicy`], on the same
//! forward-through-an-unchanged-`pub(crate)`-function pattern
//! [`GenSubmission`] established, so 45-13's `run_agent_turn` can reach
//! `generation.rs`'s spend gate without that file changing by one line.
//! **Plan 45-13 moved the phase's largest single function** — [`agent_turn`]'s
//! `run_agent_turn` (940 lines) with `build_user_turn`,
//! `apply_option_card_inner` and the ten pure helpers only they use — and added
//! the FOURTH trait, [`AgentVision`], for the one dependency that could not
//! travel with it: the whiteboard/frame vision snapshots, whose ~600-line
//! `src-tauri` cluster is shared with `native_surface.rs` and the GEN-10
//! reference-image seam. After it, every direct `generation::` call outside
//! `src-tauri/src/generation.rs` itself is gone.

/// The Chat agent's TURN — `run_agent_turn` (940 lines, the largest of the ~45
/// functions this phase moves) plus `build_user_turn`, `apply_option_card_inner`
/// and the pure helper ecosystem only they use. Relocated by 45-13, the batch
/// that added the FOURTH trait, [`AgentVision`]. Runs against
/// `AppCtx + GenSubmission + SpendPolicy + AgentVision`.
pub mod agent_turn;
/// The `sync_audio` agent tool (Phase 27, LIB-04), relocated by 45-09. Its whole
/// host surface is one `emit_patch`, so it needed no new [`AppCtx`] method.
pub mod audio_sync;
/// The BYO-key auth trio (`agent_status` / `set_api_key` / `clear_api_key`) +
/// [`AgentStatusView`], relocated by 47-01. Their inner fns were ALREADY
/// tauri-free (`&dyn agent_llm::KeyStore` only), just misplaced in the shell —
/// RESEARCH A1 rows 13, 16, 17. No [`AppCtx`] involvement at all.
pub mod auth;
/// Phase 56 (GEN-11, plans 04/06): the clip-edit SOURCE-RANGE building block —
/// D-01's named window refusal at BOTH edges and D-02's trim-respecting,
/// audio-free range extraction. Pure + engine-sidecar only; no [`AppCtx`], no
/// store, no host. Its bounds are READ from `agent_gen`'s probe-derived consts
/// and never re-declared, which is what keeps a probe correction from having a
/// second place to fail to reach.
pub mod clip_range;
/// Pure `rudis_core`/`engine` composite + decode leaves shared by the inspect,
/// preview, generation and export paths. Relocated by 45-05 because
/// `render_timeline_inspect_frame` calls them and `app-core` cannot depend back
/// on `src-tauri`; `src-tauri` still reaches them through a re-export shim.
/// 45-09 added a sixth, `render_scene_frame`, for the same reason.
pub mod compose;
mod ctx;
/// The central mutation plumbing — `dispatch_command_inner` / `undo_inner` /
/// `redo_inner`, relocated by 45-12. Their three `#[tauri::command]` wrappers
/// stay in `src-tauri` (the macro cannot resolve a `&impl AppCtx` parameter),
/// exactly like `run_export`'s split in 45-10. The batch that added
/// [`AppCtx::agent_session`].
pub mod dispatch;
/// The ONE export path (Phase 7 / 17-04) plus the two ENGINE-based asset
/// generators, relocated by 45-10: `run_export` + `run_export_blocking` +
/// `resolve_export_plan`, `run_export_project`/`handle_export_project` (45-06's
/// deferred pair, D-45-06-01), `run_generate_image`/`handle_generate_image` and
/// `run_generate_video`/`handle_generate_video`. The batch that added
/// [`AppCtx::export_progress_sink`] and [`AppCtx::run_blocking`] — the two
/// capabilities a BORROWED ctx cannot itself provide.
pub mod export;
/// The `send_feedback` agent tool (Phase 17-04), relocated by 45-10. The one
/// function in this crate that reads [`AgentSession`]'s `recent_tool_calls` /
/// `last_error` — which needed no coupling work at all, since 45-04 already
/// moved the type here.
pub mod feedback;
/// The three EXTERNAL-provider generation modality handlers (`generate_ai_image`
/// / `_video` / `_audio`), relocated by 45-11 — distinct from [`export`]'s
/// Claude-authored, engine-composited `generate_image`/`generate_video`. Runs
/// against [`GenSubmission`] as well as [`AppCtx`].
pub mod generation_bridge;
/// Phase 54.1 (SHELL-03, plan 54.1-01): the shell-agnostic generation HOST-GLUE
/// moved out of `src-tauri/src/generation.rs` — provider credential slots, the
/// job-registry/event newtypes, the intent + model resolvers, the ONE spend
/// gate, and the completion→MediaBin landing bridge. Distinct from
/// [`generation_bridge`], which is the agent-tool layer ABOVE it: this module is
/// what a host must own to submit at all. Every item MOVED, never copied — SC-5.
pub mod generation_host;
/// The external-generation bridge trait [`GenSubmission`] + [`GeneratedAsset`],
/// added by 45-11. A SEPARATE trait from [`AppCtx`], modeled on
/// `agent_llm::LlmTransport`: generic-only, native `async fn` under
/// `#[allow(async_fn_in_trait)]`, never `dyn`. It is what lets the three modality
/// handlers move while `src-tauri/src/generation.rs` stays byte-untouched and
/// fully `pub(crate)`.
pub mod gen_submission;
/// Media import: `run_import_media`/`handle_import_media` PLUS the entire
/// import-walk closure they share with the async UI commands `import_media` /
/// `import_media_folder` — relocated whole by 45-07, because `app-core` cannot
/// depend back on `src-tauri` and a half-moved closure would not compile. 47-03
/// finished the job: the UI commands' own command-level logic (the per-file
/// loop and the folder turn bracket) lives here too, as `run_import_media_ui` /
/// `run_import_media_folder_ui`, leaving thin `#[tauri::command]` wrappers.
pub mod import;
/// The agent's `inspect_timeline` / `inspect_media` "eyes" tools (Phase 21),
/// relocated by 45-05. No `AppCtx` implementation involved — the whole surface
/// is `&SharedStore` + `&Mutex<AgentSession>`.
pub mod inspect;
/// Quick 260801-n7q: the ONE MediaBin-item -> conditioning-reference-PNG decode
/// path, hoisted out of BOTH hosts (`src-tauri/src/lib.rs` and
/// `crates/ffi/src/ctx.rs` each carried a full copy) as Phase 54.1's
/// `deferred-items.md` §1 recommended. Carries [`media_reference::clamp_raster_dims`]
/// with it, since the decode derives its bound from it. Pinned single by the
/// SC-5 source scan in `src-tauri/tests/single_generation_path.rs` — a test
/// deleted with that shell at Phase 55 (GATE-07), with no successor, so this
/// single-definition property is now discipline rather than a checked gate.
pub mod media_reference;
/// The `create_matte` agent tool (Phase 27, LIB-03), relocated by 45-09. Renders
/// a background-only [`rudis_core::SceneSpec`] through the SAME
/// `render_scene_frame` + `engine::VideoEncoder` pipeline `generate_video` uses.
pub mod matte;
/// The three overlay agent tools (Phase 29): `get_overlay_library`,
/// `place_overlay` and `export_overlay_asset`, relocated by 45-08 as one
/// contiguous 18-item region. The batch that added [`AppCtx::resolve_resource`]
/// — the bundled overlay catalog lives in the app's read-only RESOURCE root,
/// a third host directory neither `app_data_dir` nor `app_cache_dir` covers.
pub mod overlay;
/// The MediaBin→Timeline placement command (`place_clip`, Phase 5), relocated
/// by 47-01. Needed nothing new from [`AppCtx`] — `store()` + `emit_patch()`
/// were already sufficient, exactly as RESEARCH A1 row 11 predicted.
pub mod place;
/// The `new_project` / `open_project` / `get_projects` agent tools (Phase 26),
/// relocated by 45-06 — the FIRST functions to run against a real [`AppCtx`]
/// (`app_data_dir()`, `active_project_meta()` and `emit_patch()` all in one
/// batch). `run_export_project`/`handle_export_project`, which 45-06's plan
/// grouped in with them, deliberately did NOT move; see this module's doc.
pub mod project;
/// Phase 43 (LAT-08)'s persistent, versioned `ffprobe`-result cache, moved
/// wholesale from `src-tauri/src/probe_cache.rs` by 45-07. Only `cache_path` /
/// `load` / `save` changed: `&tauri::AppHandle<R>` → `&impl AppCtx`.
pub mod probe_cache;
/// Project persistence primitives (Phase 26 LIB-02 + Phase 43 LAT-04's sidecar
/// index), moved wholesale from `src-tauri/src/project_store.rs` by 45-06. Only
/// `projects_dir` changed: `&tauri::AppHandle<R>` → `&impl AppCtx`.
pub mod project_store;
/// The four store-query / instrumentation one-liners (`get_snapshot` /
/// `get_entities` / `get_current_seq` / `debug_mark_interactive`), relocated by
/// 47-01 — the first Phase 47 batch, closing RESEARCH A1's gap so the C ABI
/// wraps `run_*` functions instead of duplicating command bodies. Two of them
/// are Phase 43's LAT-02 apply-instead-of-refetch companions (D-01).
pub mod queries;
/// The Chat agent's conversation state (`AgentSession` + its five companion
/// types), relocated here by plan 45-04 ahead of any function that touches it —
/// `app-core` cannot depend back on `src-tauri`, so the TYPE has to arrive
/// first. See the module doc for the deliberate, temporary `pub`-field widening
/// that 45-14 closes out.
pub mod session;
/// The agent's `get_transcript` / `search_media` tools (Phase 22), relocated by
/// 45-05. Offline `engine::whisper` transcription, TEXT-only tool_results.
pub mod transcribe;
/// The ONE tauri-free `AppCtx` fake every migrated test uses in place of the
/// `tauri::test::MockRuntime` `build_app()`/`build_app_isolated()` helpers
/// scattered through `src-tauri/src/lib.rs`'s `#[cfg(test)]` modules.
#[cfg(test)]
pub mod test_support;
/// The `track_object` agent tool (Phase 30, TRK-01/TRK-02), relocated by 45-09 —
/// the FIRST code in this crate to actually exercise `engine`'s `tracking`
/// feature (the license-clean OpenCV CSRT/KCF sidecar), declared here since
/// 45-03 but until now only compiled, never called.
pub mod tracking;
/// The `transport` command's store half (`run_transport`, Phase 4 / 18.2-05),
/// relocated by 47-03. The host-specific presentation consequences (the
/// lock-free playback-mirror publish + the `playback:changed` emit) stayed in
/// each host on purpose — see [`transport::TransportOutcome`].
pub mod transport;
/// SHELL-09's DELIVERY half (Phase 52, plan 52-05): the bounded background
/// extraction job [`import::import_one_path`] detaches at import time, and the
/// PURE cache read the C ABI's `rudis_get_waveform_peaks` sits on. The read can
/// never start a decode — that property is what makes the roadmap's "never a
/// synchronous call during timeline draw" structural rather than a discipline.
pub mod waveform_job;
/// Phase 53.2's DELIVERY half (plan 53.2-04): [`waveform_job`]'s twin for the
/// Timeline clip's FRAME body. Same three parts, same guarantees — a detached
/// semaphore-bounded job hooked in beside the poster and peaks call sites
/// (D-09), and a pure cache read behind `rudis_get_filmstrip_strip` (D-12) that
/// cannot reach a decoder. One thing is new: D-14's progressive fill means the
/// read has THREE states rather than two, and the extra one crosses the ABI as
/// `completed_tiles` / `total_tiles` rather than as a new envelope shape.
pub mod filmstrip_job;
/// Phase 58's DELIVERY half (plan 58-05, PROXY-02): [`filmstrip_job`]'s twin for
/// the heavy source's PLAYBACK cost. The same three parts — a detached
/// semaphore-bounded job hooked in beside the poster, peaks and filmstrip call
/// sites (D-09), and a pure read behind `rudis_get_proxy_status` (D-29) that
/// cannot reach an encoder. Two things are new: the job is a real subprocess
/// that must be KILLED rather than abandoned (D-11), so it carries
/// `crates/agent-gen`'s cancel-latch registry; and the trigger is
/// PREDICATE-GATED (D-07/D-08), so a light source costs nothing at all.
pub mod proxy_job;
/// Phase 59's DELIVERY half (plan 59-08, CACHE-01/03/04): the render-cache
/// SCHEDULER — the third and thinnest layer of 59-CONTEXT D-39's split.
///
/// ```text
/// the cache LEAF crate           identity, file format, atomic commit, LRU
/// crates/preview                 the D-19 lookup and `render_segment` (writer)
/// app-core/render_cache_job      only WHEN a render happens
/// ```
///
/// The leaf crate is deliberately not NAMED here. Exactly one file in
/// `crates/app-core/src` may name it — the scheduler — and a source scan cannot
/// tell prose from a call, so this list describes it instead of spelling it
/// (`dynres.rs`'s recorded footgun; it has bitten five plans across two phases).
///
/// [`proxy_job`]'s shape almost line for line — a bounded detached job, a
/// registry of epoch-owned rows carrying a cancel latch, and a poll-only read
/// behind the C ABI that cannot start work — with ONE genuinely new obligation:
/// its encoder admission is `proxy_job`'s OWN process-wide semaphore
/// (`proxy_job::try_take_permit`), because a proxy transcode and a segment
/// render drive the same silicon and D-23 forbids them saturating it together.
pub mod render_cache_job;

pub use ctx::AppCtx;
// Plan 45-12. The phase's THIRD trait, and the smallest: two methods, both
// synchronous, both forwarding to an UNCHANGED `pub(crate)` function inside
// `src-tauri/src/generation.rs` — the one file this phase must leave alone
// (`git diff --stat` on it is empty for this batch). `SpendGateDecision` is the
// `app-core` mirror of that module's own two-variant enum. See `ctx.rs` for why
// this is a trait rather than four duplicated helpers, and for why it carries
// TWO of the four methods the plan named.
pub use ctx::{SpendGateDecision, SpendPolicy};
// Plan 45-13. The phase's FOURTH trait: the agent-facing vision surface
// `run_agent_turn` assembles a fresh turn's image block(s) from. A bridge, on
// the identical argument 45-11's `GenSubmission::resolve_reference_image` made
// about the very same `src-tauri` cluster — see `ctx.rs` for why the cluster
// stays put, and for the tested (not assumed) reason a native `async fn`
// compiles here even though these futures DO live inside a
// `#[tauri::command] async fn`'s state machine.
pub use ctx::AgentVision;
pub use gen_submission::{ClipEdge, GenSubmission, GeneratedAsset, ReferenceSource};

// Phase 56 (GEN-11, plan 04): the clip-edit source-range pair, flat-re-exported
// beside the generation surface below because that is the only thing that calls
// them — Plan 06's `edit_clip_pixels` seam runs the window check and then, only
// on `Ok`, the extraction. Kept EXPLICIT for the same reason every list in this
// file is: a glob would hide the day a third name joins the module's contract.
pub use clip_range::{clip_edit_window_check, extract_clip_range_mp4};

// Plan 54.1-01 (Phase 54.1, SHELL-03). The generation host-glue, moved OUT of
// `src-tauri/src/generation.rs` — never copied, so exactly one definition of
// each name exists workspace-wide (SC-5 / the Backlog-999.8 fork rule). The
// list is EXPLICIT rather than `pub use generation_host::*` for the same reason
// every batch since 45-05 has kept it explicit: this list IS the module's
// public contract, and a glob would hide the day a name silently joins it.
//
// `src-tauri/src/generation.rs` re-exports these at their OLD paths so its
// ~7,400 lines of test modules and its `#[tauri::command]` wrappers compile
// unchanged; `src-tauri/src/lib.rs` called `app_core::spend_confirmation_gate`
// and `app_core::resolved_model_for_tool_input` DIRECTLY, so the spend policy
// has one definition and one caller path. (It named a third — the shape/stage
// translation — which Phase 55.1 plan 06 deleted, as Phase 55's cutover deleted
// `src-tauri/` itself. Both sentences are kept in the past tense rather than
// erased, because they record why this list is shaped the way it is.)
//
// Deliberately ABSENT: nothing that has a caller. Phase 55.1 plan 06 removed
// eight names whose last callers 55.1-03 rewired away; see the note above the
// `generation_host` list.
//
// Phase 55.1 (plan 04) adds `cost_signal_for_model` / `cost_signal_for_tool_input`
// / `PRICE_UNKNOWN` — the spend gate's cost dimension — beside
// `spend_confirmation_gate` itself, because every host that calls the gate must
// be able to build its new argument from the SAME place, not from a second
// lookup of its own.
//
// Phase 55.1 (plan 06) REMOVES eight: the two capability consts, the capability
// -> model lookup, the shape-serving predicate, the cheapest-capable default,
// the resolver, the schema translation and the error-vocabulary rewriter. All
// eight lost their last reader when 55.1-03 opened the free-text `model` field,
// and this list is the module's public contract — leaving a name here that
// nothing implements or calls is how the next host re-grows the indirection.
//
// Phase 56 (plan 05) adds `estimated_video_edit_cost_cents` and its two pricing
// consts — the clip-edit path's PER-INPUT-SECOND estimate. It sits beside the
// flat `cost_signal_for_model` rather than replacing it: the roster's 4-second
// figure still answers "what does this model cost", while only this one can
// answer "what will THIS edit cost", which is the question plan 07's
// confirmation actually has to put to the user.
//
// Phase 56 (plan 07) adds the two that turn that estimate into a question:
// `video_edit_cost_signal` (D-56-05-01 — a `$` figure or the literal
// PRICE_UNKNOWN, never neither) and `resolved_video_edit_model_id` (the ONE
// normalization the disclosure, the price and the submission all read, so the
// confirmation and the bill cannot name different models).
pub use generation_host::{
    cost_signal_for_model, cost_signal_for_tool_input, emit_job_event, emit_progress_event,
    estimated_video_edit_cost_cents, finalize_ready_job, gen_submit_error_message,
    generate_elevenlabs_audio_for_agent,
    generate_runway_image_for_agent, generate_runway_video_edit_for_agent,
    generate_runway_video_for_agent, land_generated_asset,
    land_ready_assets, poll_job_until_terminal, resolve_elevenlabs_provider,
    resolve_provenance_flag, resolve_runway_provider, run_clear_provider_key,
    run_set_provider_key,
    resolved_model_for_tool_input, resolved_video_edit_model_id, spend_confirmation_gate,
    start_generation_job,
    submit_generation_job_inner, validate_provider_key_format, validated_ext,
    video_edit_cost_signal, video_edit_reference_check, AgentGeneratedAudio,
    AgentGeneratedImage, AgentGeneratedVideo, GenEventSink, GenHost, GenJobEventPayload,
    GenProgressEventPayload, ManagedAllowList, ManagedAudioGenProvider, ManagedGenJobs,
    ManagedGenProvider, ManagedProviderKeyStore, ManagedVideoGenProvider, ProviderSlot,
    StartedJob, SETTINGS_PROVIDER_ALLOW_LIST,
    AGENT_IMAGE_WAIT_CAP, AGENT_VIDEO_WAIT_CAP, ALLOWED_GEN_EXTS, GEN_JOB_EVENT,
    GEN_PROGRESS_EVENT, KEYCHAIN_SERVICE, MAX_AGENT_AUDIO_PROMPT_CHARS,
    MAX_AGENT_IMAGE_PROMPT_CHARS, MAX_AGENT_VIDEO_PROMPT_CHARS, MAX_GEN_ASSET_BYTES,
    MAX_PROVIDER_KEY_LEN, NO_AUDIO_PROVIDER_CONFIGURED, NO_PROVIDER_CONFIGURED,
    NO_VIDEO_PROVIDER_CONFIGURED, PRICE_UNKNOWN, PROVIDER_KEY_SLOTS,
    RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND, RUNWAY_ALEPH2_MINIMUM_CENTS,
};
// 45-14 (T-45-02, partial close): `InspectTimelineCache` and `TranscriptCache`
// are now `pub(crate)` — the only code that ever named them is `inspect.rs` and
// `transcribe.rs`, both of which reach them by their `crate::session::` path, so
// nothing here needs to re-export them. `PendingGrowth` is still `pub` ONLY
// because `AgentSession::pending_growth` is (a `pub` field of a more-private type
// trips the `private_interfaces` lint); it has zero `src-tauri` name references,
// so `src-tauri` no longer shims it. See `session.rs`'s module doc for the
// per-field measurement and D-45-14-01 for the residue.
pub use session::{AgentSession, PendingGrowth, PendingAskUser, PendingOptionChoice};

// Plan 45-05. Deliberately an EXPLICIT list rather than `pub use inspect::*`:
// these are exactly the names `src-tauri` re-exports at the old locations, so
// this list IS the batch's public contract and 45-14 re-privatizes from it.
// Everything else in those modules (`resolve_inspect_context`,
// `inspect_state_hash`, `cap_matches`, `transcript_json`, the storyboard/cache
// constants) has no caller outside its own module and stays private.
pub use compose::{
    black_rgba, decode_clip_frame, engine_alpha_mode, rasterize_text_layer, render_scene_frame,
    timeline_clip,
};
pub use inspect::{
    handle_inspect_media, handle_inspect_timeline, render_timeline_inspect_frame,
    run_inspect_media, run_inspect_timeline, INSPECT_JPEG_QUALITY, INSPECT_MEDIA_FRAME_MAX_EDGE,
};
pub use transcribe::{
    handle_get_transcript, handle_search_media, run_get_transcript, run_search_media, run_transcribe,
};

// Plan 45-06. Same EXPLICIT-list discipline. `autosave_project` is deliberately
// absent: it is `project`-module-private and has no caller outside it. (45-06's
// note here said the end-of-turn autosave hook (c) lives in `dispatch_command`
// and moves in 45-12. Corrected at 45-12 against the tree: hook (c) is inside
// `run_agent_turn`, not `dispatch_command` — which has no autosave at all. It
// DID arrive with that function in 45-13 and now lives in
// [`agent_turn::run_agent_turn`], still calling
// `project_store::write_project_atomic` directly.)
pub use project::{
    handle_get_projects, handle_new_project, handle_open_project, run_get_projects,
    run_new_project, run_open_project,
};

// Plan 45-07. Same EXPLICIT-list discipline. These are exactly the names
// `src-tauri` still calls after the move — `run_import_media`/
// `handle_import_media` from the agent-tool dispatch, and everything else from
// the three async UI commands that stayed behind as thin `TauriAppCtx`
// delegating wrappers (`import_one_path`, `walk_import_roots`,
// `poster_cache_dir`, the two folder caps) or from an unrelated `src-tauri`
// caller the closure happened to share a leaf with (`next_id`, `ID_SEQ`,
// `map_kind`, `MAX_IMAGE_SEQUENCE_FRAMES`, `detect_image_sequence*`,
// `build_sequence_item`, `split_numbered_basename`, `child_virtual_path`).
// `cached_from_probe`, `poster_at_seconds`, `claim_sequence_members`,
// `walk_import_dir` and `lock_shared` have NO caller outside `import` and stay
// private.
pub use import::{
    build_sequence_item, child_virtual_path, detect_image_sequence, detect_image_sequence_capped,
    handle_import_media, import_one_path, map_kind, next_id, poster_cache_dir, run_import_media,
    split_numbered_basename, walk_import_roots, DetectedSequence, FolderWalk, ImportOutcome,
    ID_SEQ, MAX_FOLDER_IMPORT_DEPTH, MAX_FOLDER_IMPORT_FILES, MAX_IMAGE_SEQUENCE_FRAMES,
};

// Plan 47-03 (Phase 47, FFI-01). The two UI import commands' command-level
// logic, under a `_ui` suffix because `run_import_media` above is ALREADY the
// AGENT TOOL's import path with its own turn/emit contract (RESEARCH A1's
// naming trap): the per-call loop (`run_import_media_ui`, RESEARCH A1 row 8)
// and the folder walk's turn bracket (`run_import_media_folder_ui`, row 9 —
// which absorbed `src-tauri`'s `import_media_folder_capped` whole; the caps
// stay parameters so both DoS clamps remain directly testable). Each is
// called from its one thin `#[tauri::command]` wrapper left in `src-tauri`.
pub use import::{run_import_media_folder_ui, run_import_media_ui};

// Plan 52-05 (Phase 52, SHELL-09). Same EXPLICIT-list discipline, and this one
// is deliberately short: `run_get_waveform_peaks` is the `run_*` the C ABI's
// 24th export wraps (the `run_*` naming every other export uses), `read_peaks`
// is the pure read underneath it that a future in-process host can call
// directly, `PeaksPayload` is that export's return type, and
// `MAX_CONCURRENT_WAVEFORM_JOBS` is the DoS cap — named here because a bound
// nobody outside the module can see is a bound nobody can assert on.
// `waveform_cache_dir` joins `import`'s list above by subject rather than by
// module: it is the poster-cache-dir twin, and its one caller outside
// `import.rs` is a host that wants to know where the peaks live.
// `spawn_extraction` is deliberately ABSENT — the trigger has exactly one call
// site, inside `import_one_path`, and a second one is an architectural
// decision, not a convenience.
pub use import::waveform_cache_dir;
pub use waveform_job::{
    read_peaks, run_get_waveform_peaks, PeaksPayload, MAX_CONCURRENT_WAVEFORM_JOBS,
};

// Plan 53.2-04 (Phase 53.2, D-09/D-12/D-14). The same EXPLICIT-list discipline
// and the same four members as the peaks block directly above, chosen the same
// way: `run_get_filmstrip_strip` is the `run_*` the C ABI's new export wraps,
// `read_strip` is the pure read underneath it that a future in-process host can
// call directly, `StripPayload` is that export's return type, and
// `MAX_CONCURRENT_FILMSTRIP_JOBS` is the DoS cap — named here because a bound
// nobody outside the module can see is a bound nobody can assert on, and this
// one is read by `crates/ffi/tests/contract_media.rs`'s measurement.
// `filmstrip_cache_dir` joins by subject rather than by module, exactly as
// `waveform_cache_dir` does. `spawn_extraction` is deliberately ABSENT — the
// trigger has exactly one call site, inside `import_one_path`, and a second one
// is an architectural decision, not a convenience.
pub use filmstrip_job::{
    read_strip, run_get_filmstrip_strip, StripPayload, FILMSTRIP_CHUNK_BUDGET_BYTES,
    MAX_CONCURRENT_FILMSTRIP_JOBS,
};
pub use import::filmstrip_cache_dir;

// Plan 45-08. Same EXPLICIT-list discipline, and this one is deliberately
// SHORT: of the 18 relocated items only these five have a caller left in
// `src-tauri` — three `handle_*` from `run_agent_turn`'s tool-dispatch match,
// and two `run_*` from the `place_overlay_gate` / `export_overlay_asset_gate`
// `#[cfg(test)]` modules that could not migrate (they drive `#[tauri::command]`s
// that stay in the shell). Everything else the batch moved — `scan_overlay_
// library`, `overlay_component_is_safe`, `overlay_png_dimensions`,
// `resolve_overlay_library_asset`, `parse_overlay_transform`,
// `build_probed_overlay_item`, `run_get_overlay_library`, both outcome structs'
// non-read fields, `ResolvedOverlaySource` and the three constants — has NO
// caller outside `overlay` and stays private there, so 45-14's re-privatization
// worklist grows by five names, not eighteen.
pub use overlay::{
    handle_export_overlay_asset, handle_get_overlay_library, handle_place_overlay,
    run_export_overlay_asset, run_place_overlay,
};

// Plan 45-09. Same EXPLICIT-list discipline. Six names, one per moved function:
// the three `handle_*` from `run_agent_turn`'s tool-dispatch match, and the three
// `run_*` from the `#[cfg(test)]` gates that could NOT migrate (`create_matte_gate`
// and `sync_audio_gate` drive `import_media_blocking` / `place_clip` /
// `dispatch_command` / `get_snapshot` / `export_timeline`; `track_object_gate`
// keeps only its export-composite SC-2 test, for the same reason). Everything
// else the batch moved -- `subsample_position_keyframes`,
// `format_track_object_result`, `TrackObjectOutcome` and `SyncAudioOutcome` --
// has NO caller that NAMES it outside its own module and stays private/unlisted,
// so 45-14's re-privatization worklist grows by six names plus six struct fields
// (`TrackObjectOutcome::{target_clip_id, keyframe_count}` and all four of
// `SyncAudioOutcome`, read off the returned value by the staying gates).
//
// `compose::render_scene_frame` is deliberately on the 45-05 `compose` list
// above, not here: it is a SHARED leaf (`run_generate_image` and
// `run_generate_video` still call it from `src-tauri`), not a `create_matte`
// item.
pub use audio_sync::{handle_sync_audio, run_sync_audio};
pub use matte::{handle_create_matte, run_create_matte};
pub use tracking::{handle_track_object, run_track_object};

// Plan 45-10. Same EXPLICIT-list discipline, and this one is deliberately SHORT
// too: of the FOURTEEN relocated items only these eight have a caller left in
// `src-tauri` -- four `handle_*` from `run_agent_turn`'s tool-dispatch match,
// `run_export` from the `export_timeline` command plus the `export_gate` /
// `export_project_gate` tests that stayed, `EXPORT_PROGRESS_EVENT` from
// `TauriAppCtx::export_progress_sink` and one `export_gate` listener, and
// `run_generate_image` / `run_generate_video` from the four generation-gate
// tests that stayed.
//
// Everything else the batch moved -- `ExportPlan`, `build_export_audio_wav`,
// `resolve_export_plan`, `run_export_blocking`,
// `export_is_single_layer_degenerate`, `run_export_project` and
// `write_feedback_file` -- has NO caller outside its own module and stays
// PRIVATE. That is only true because the two `export_gate` unit tests which
// drove `build_export_audio_wav` and `export_is_single_layer_degenerate`
// directly moved with them; leaving them behind would have forced
// `pub struct ExportPlan` with three `pub` fields across a crate boundary purely
// for a test. So 45-14's re-privatization worklist grows by eight names and ZERO
// struct fields.
pub use export::{
    handle_export_project, handle_generate_image, handle_generate_video, run_export,
    run_generate_image, run_generate_video, EXPORT_PROGRESS_EVENT,
};
pub use feedback::handle_send_feedback;

// Plan 45-11. Same EXPLICIT-list discipline. Six names -- the three `handle_*`
// from `run_agent_turn`'s tool-dispatch match, and the three `run_*` reached
// only through them today but re-exported alongside their partners so the shim
// block in `src-tauri` reads as three complete pairs (the convention every batch
// since 45-05 has followed).
//
// `parse_reference_source`, `parse_destination_source`, `parse_frame_source` and
// `RUNWAY_TRAINING_LICENSE_DISCLOSURE` are deliberately ABSENT: after the two
// parser tests migrated out of `src-tauri`'s `agent_gate`, nothing outside
// `generation_bridge` names any of them, so all four stay PRIVATE and 45-14's
// re-privatization worklist grows by six names plus the two enums below --
// and ZERO struct fields.
//
// `ClipEdge` / `ReferenceSource` are on the `gen_submission` line above, not
// here: they are the BRIDGE's vocabulary (`GenSubmission::resolve_reference_image`
// takes one), and `src-tauri` still names both from `resolve_reference_image`,
// `resolve_clip_edge_reference_png` and five `agent_gate` tests that could not
// move.
// Phase 56 plan 07 (GEN-11 / D-04) makes it eight names: the clip-edit pair
// joins on the SAME "complete pairs" convention. `run_generate_ai_video_edit` is
// reached only through its `handle_*` partner today, exactly as the other three
// `run_*` are.
//
// The two Phase-56 disclosure constants stay PRIVATE alongside
// `RUNWAY_TRAINING_LICENSE_DISCLOSURE`, for the same reason: only the handlers
// in that module append them, and the tests that pin them live in the module
// too. The USER-facing (structural, narration-proof) copy of the same facts is
// `agent_turn::provider_notice_for_modality`, which is public.
pub use generation_bridge::{
    handle_generate_ai_audio, handle_generate_ai_image, handle_generate_ai_video,
    handle_generate_ai_video_edit, run_generate_ai_audio, run_generate_ai_image,
    run_generate_ai_video, run_generate_ai_video_edit,
};

// Plan 45-12. Three names, one per relocated function — each called from
// exactly one thin `#[tauri::command]` wrapper left behind in `src-tauri`
// (`dispatch_command` / `undo` / `redo`), and `undo_inner` additionally from the
// five `library_growth_gate` call sites that could NOT migrate (every one of
// them drives a full `run_agent_turn`, which is still `src-tauri`-resident until
// 45-13). Nothing else moved, so 45-14's re-privatization worklist grows by
// three names, ZERO struct fields, plus the `SpendPolicy` / `SpendGateDecision`
// pair on the `ctx` line above.
pub use dispatch::{dispatch_command_inner, redo_inner, undo_inner};

// Plan 45-13. Same EXPLICIT-list discipline, and this one is deliberately SHORT
// for a batch that moved TWELVE items: of them only these seven have a caller
// left outside `agent_turn`.
//
// * `run_agent_turn` — the `agent_send_message` command, plus 30 `#[cfg(test)]`
//   call sites in `src-tauri/src/lib.rs` and 4 in `src-tauri/src/generation.rs`,
//   none of which can migrate (every one builds a `tauri::test::MockRuntime` app
//   and most need Tauri-managed generation providers a temp-dir `TestAppCtx`
//   cannot represent).
// * `apply_option_card_inner` — the `apply_option_card` command + 3
//   `option_card_gate` tests.
// * `build_user_turn` — 3 `agent_gate` tests plus 3 sites inside larger gates
//   that also drive `run_agent_turn`.
// * `AgentTurnOutcome` — `agent_send_message`'s return type and
//   `spend_gate::drive_turn`'s.
// * `GenerationDisclosure` — read field-by-field by four gates.
// * `provider_notice_for_modality` — FOUR call sites in `src-tauri/src/generation.rs`
//   (`crate::provider_notice_for_modality`), which is why it is `pub` here and
//   why that file needs no edit for it.
// * `is_pre_spend_validation_failure` + `PRE_SPEND_VALIDATION_SUBSTRINGS` — the
//   two `agent_gate` tests that pinned them ALSO named the video capability
//   resolver and its offered-capability list (both `pub(crate)` in the
//   out-of-scope file), so they provably could not move and these two had to be
//   reachable. Historical on both counts: Phase 55.1 plan 06 deleted those two
//   callees, and 55.1-03 re-aimed the needles at the refusals the bridge raises.
//
// Everything else the batch moved — `describe_conditioning_frames`,
// `history_carries_vision`, `is_intercepted_meta_tool`,
// `dispatch_generate_with_cap`, `GENERATE_RETRY_CAP`, `HISTORY_IMAGE_KEEP_COUNT`,
// `SPEND_GATE_NOT_EXECUTED`, `RUNWAY_TRAINING_LICENSE_NOTICE` — has NO caller
// outside `agent_turn` and stays PRIVATE. So 45-14's re-privatization worklist
// grows by eight names (the seven above plus `AgentVision`) and ZERO newly-`pub`
// struct fields: `AgentTurnOutcome`'s and `GenerationDisclosure`'s fields were
// already `pub` before the move, for the frontend's sake.
pub use agent_turn::{
    apply_option_card_inner, build_user_turn, is_pre_spend_validation_failure,
    provider_notice_for_modality, run_agent_turn, AgentTurnOutcome, ExportDisclosure,
    GenerationDisclosure, PRE_SPEND_VALIDATION_SUBSTRINGS,
};

// Plan 47-01 (Phase 47, FFI-01). Four names, one per relocated command body —
// each called from exactly one thin `#[tauri::command]` wrapper left behind in
// `src-tauri` (`get_snapshot` / `get_entities` / `get_current_seq` /
// `debug_mark_interactive`), and each the `run_*` home the Phase 47 C ABI
// (`crates/ffi`) wraps so it stays a pure transport layer (RESEARCH A1 rows
// 1-4). Nothing else in the module is private — these four ARE the module.
pub use queries::{
    run_debug_mark_interactive, run_get_current_seq, run_get_entities, run_get_snapshot,
};

// Plan 47-01, second batch (Phase 47, FFI-01). Four names: the three run_*
// each called from exactly one one-line `#[tauri::command]` wrapper left in
// `src-tauri` (`agent_status` / `set_api_key` / `clear_api_key`), and
// `AgentStatusView` — `agent_status`'s return type, re-exported by `src-tauri`
// at its old path so the `api_key_gate` tests that pin the T-12-11
// boolean-only contract compile unchanged (RESEARCH A1 rows 13, 16, 17).
pub use auth::{
    key_source, key_source_with, run_agent_status, run_clear_api_key, run_set_api_key,
    AgentStatusView, KeySource, ProviderKeyStatus, ANTHROPIC_ENV_FALLBACKS, RUNWAY_ENV_FALLBACKS,
};

// Plan 47-01, third batch (Phase 47, FFI-01). One name: `run_place_clip`,
// called from the one thin `#[tauri::command] place_clip` wrapper left in
// `src-tauri` (RESEARCH A1 row 11 — the one command whose every need,
// `ctx.store()` + `ctx.emit_patch()`, was already on [`AppCtx`]).
pub use place::run_place_clip;

// Plan 47-03 (Phase 47, FFI-01). The `transport` command's store half:
// `run_transport` returns everything host-agnostic (`TransportOutcome`), and
// each host keeps its own presentation consequences — the Tauri wrapper's
// lock-free mirror publish + `playback:changed` emit stayed in `src-tauri`
// verbatim (RESEARCH A1 row 10).
pub use transport::{run_transport, TransportOutcome};

// Re-exported for downstream batches' convenience so a moved function's `use`
// lines change as little as possible; extend as batches land.
pub use rudis_core::{Command, Patch, Project, Store, TransportCmd};

/// The single backend-owned store. Structurally identical to (and kept in sync
/// with) `rudis_app_lib::SharedStore` — `src-tauri` manages exactly this type,
/// so its `AppCtx::store()` impl can hand out a `&SharedStore` with no
/// conversion.
pub type SharedStore = std::sync::Mutex<rudis_core::Store>;
