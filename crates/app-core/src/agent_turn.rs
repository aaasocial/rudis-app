//! The Chat agent's TURN: `run_agent_turn` — at 940 lines the single largest
//! function of the ~45 this phase relocates, and per `45-RESEARCH.md` the
//! "central plumbing… highest fan-in… moved last" (plan 45-13).
//!
//! Everything here arrived from `src-tauri/src/lib.rs` VERBATIM apart from the
//! conversions tabled below. The two `#[tauri::command]`s that drive it —
//! `agent_send_message` and `apply_option_card` — stay in the shell for the
//! structural reason 45-10's `run_export` split established: Tauri's command
//! macro resolves every parameter from the invoke message or from managed
//! state, and it cannot resolve a `&impl AppCtx`.
//!
//! # What moved with it, and why each item had to
//!
//! | Item | Why it could not stay behind |
//! |---|---|
//! | [`AgentTurnOutcome`] | `run_agent_turn`'s return type |
//! | [`GenerationDisclosure`] | the `Vec` that type carries; built inside the meta loop |
//! | `RUNWAY_TRAINING_LICENSE_NOTICE` / [`provider_notice_for_modality`] | the disclosure's `provider_notice` field |
//! | `describe_conditioning_frames` | the disclosure's `frames` field |
//! | `HISTORY_IMAGE_KEEP_COUNT` | the turn-start `prune_stale_images` bound |
//! | `SPEND_GATE_NOT_EXECUTED` | [`build_user_turn`]'s spend-gate resume marker |
//! | `history_carries_vision` | the CR-01 pre-round-0 vision scan |
//! | [`build_user_turn`] | composes the turn's user message from the session |
//! | `GENERATE_RETRY_CAP` / [`PRE_SPEND_VALIDATION_SUBSTRINGS`] / [`is_pre_spend_validation_failure`] / `dispatch_generate_with_cap` | the CHECK-03 per-turn paid-call cap the three `generate_ai_*` arms wrap |
//! | `is_intercepted_meta_tool` | the `apply_round` closure's Pattern-C pre-scan AND its meta-block filter (one predicate so the two can never drift) |
//! | [`apply_option_card_inner`] | the CANV-02 sibling; same session→store lock order, same `emit_patch` |
//!
//! # The conversion table
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `<T: LlmTransport, R: tauri::Runtime>(app: &AppHandle<R>, store, session, ..)` | `<T: LlmTransport, C: AppCtx + GenSubmission + SpendPolicy + AgentVision>(ctx: &C, session, ..)` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first body line |
//! | `emit_changed(&app_for_events, ..)` | `ctx.emit_patch(..)` — whose Tauri impl DELEGATES to the unchanged `emit_changed`, so the `#[serde(flatten)]`ed LAT-02 envelope is byte-identical |
//! | 14 × `&TauriAppCtx::new(&app_for_events, store)` in the tool-dispatch match | `ctx` |
//! | `generation::resolved_model_for_tool_input(..)` ×2 | `ctx.resolved_model_for_tool_input(..)` ([`SpendPolicy`], 45-12) |
//! | `generation::spend_confirmation_gate(..)` | `ctx.spend_confirmation_gate(..)` ([`SpendPolicy`], 45-12) |
//! | `build_vision_snapshot_block(&project)` | `ctx.vision_snapshot_block(&project)` ([`AgentVision`]) |
//! | `build_whiteboard_snapshot_block(&project, rw, rh)` | `ctx.whiteboard_snapshot_block(&project, rw, rh)` ([`AgentVision`]) |
//! | `app.try_state::<Mutex<WhiteboardAspect>>()` + `clamp_raster_dims(..)` | `ctx.whiteboard_raster_dims()` ([`AgentVision`]) |
//! | `app.try_state::<ActiveProjectMeta>()` (autosave hook c) | `ctx.active_project_meta()` |
//! | `next_id("ex")` / `project_store::write_project_atomic` | `crate::next_id` / `crate::project_store::write_project_atomic` (45-07 / 45-06) |
//!
//! **`generation::` is now unreachable from this crate and that is the point:**
//! the two calls above were the LAST direct coupling to `src-tauri/src/generation.rs`
//! anywhere outside that file, and both go through [`SpendPolicy`], which 45-12
//! landed for exactly this batch. `generation.rs`'s production code changes by
//! zero lines (threat T-45-07).
//!
//! # ⚠ `session` STAYS AN EXPLICIT PARAMETER — and that is a measurement, not a preference
//!
//! The plan prescribed dropping it in favour of [`AppCtx::agent_session`]
//! (45-12). Measured against the tree first, as 45-12's own carry-forward
//! demanded: **all 34 non-production call sites pass a STACK-LOCAL
//! `Mutex<AgentSession>`** (30 in `src-tauri/src/lib.rs`, 4 in
//! `src-tauri/src/generation.rs`), and **17 of them seed it before the call or
//! read it after** — `generation::confirmed_session()`, a pre-set
//! `pending_option_choice`/`pending_ask_user`, a seeded `history`, or an
//! assertion on `spend_approved_turn`/`pending_growth`/`last_error`.
//! `ctx.agent_session()` resolves the app's MANAGED session, which none of
//! those tests can see; converting would have redirected every one of them
//! silently — the exact green-to-red 45-12 caught on `undo_inner`'s five sites,
//! at seven times the scale. Keeping the parameter is also the shape
//! [`crate::inspect::handle_inspect_timeline`] and
//! [`crate::feedback::handle_send_feedback`] already have here, and it leaves
//! production byte-identical: `agent_send_message` still passes the very
//! `State<'_, Mutex<AgentSession>>` it always did.
//!
//! # ⚠ The vision snapshots are a BRIDGE, not a mover — [`AgentVision`]
//!
//! `build_vision_snapshot_block` / `build_whiteboard_snapshot_block` (and the
//! `WhiteboardAspect` mirror behind their raster dims) stay in `src-tauri`,
//! byte-untouched, reached through the phase's FOURTH trait. Same justification
//! 45-11 gave for `GenSubmission::resolve_reference_image`, which awaits those
//! very builders: they sit on a ~600-line cluster (`resolve_active_snapshot_frame`,
//! `draw_annotations_onto_styled`, `clamp_raster_dims`, `OVERLAY_INK`,
//! `WHITEBOARD_BG`, `blank_whiteboard_frame`, the two `_png`/`_jpeg` byte
//! producers) that is shared with `native_surface.rs`'s live-preview ink overlay
//! and with the GEN-10 reference-image seam, and pinned by ~20 byte-identity
//! tests. Dragging all of it in would have made the phase's largest batch larger
//! still, against its own SC-2 rule that a batch which cannot keep the suite
//! green is too large.
//!
//! Native `async fn` under `#[allow(async_fn_in_trait)]` was NOT assumed safe
//! here — 45-10's `Pin<Box<dyn Future + Send>>` verdict applies to futures that
//! live inside a `#[tauri::command] async fn`'s state machine, which these
//! provably do (`agent_send_message` awaits `run_agent_turn` awaits
//! `ctx.vision_snapshot_block(..)`). It was tested with a throwaway probe of
//! exactly that shape before any code moved, and it COMPILES: `Send` stays
//! satisfiable because the obligation is discharged at the concrete
//! instantiation, not higher-ranked over `&self` the way `AppCtx::run_blocking`'s
//! was. See [`AgentVision`]'s own doc.

use crate::{
    handle_create_matte, handle_export_overlay_asset, handle_export_project,
    handle_generate_ai_audio, handle_generate_ai_image, handle_generate_ai_video,
    handle_generate_ai_video_edit,
    handle_generate_image, handle_generate_video, handle_get_overlay_library,
    handle_get_projects, handle_get_transcript, handle_import_media, handle_inspect_media,
    handle_inspect_timeline, handle_new_project, handle_open_project, handle_place_overlay,
    handle_search_media, handle_send_feedback, handle_sync_audio, handle_track_object,
    AgentSession, AgentVision, AppCtx, GenSubmission, Patch, PendingAskUser, PendingGrowth,
    PendingOptionChoice, SpendGateDecision, SpendPolicy,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// PROMPT-03 gap fix (40-HUMAN-UAT.md item 3): one successful `generate_ai_*`
/// call's disclosure, independent of Claude's own narration. The existing
/// GEN-09 disclosure text ("Prompt sent to the provider: \"...\"") reaches
/// Claude's tool_result, but nothing forces Claude to relay it verbatim in
/// what it shows the user -- live UAT proved Claude's narration paraphrases
/// instead of quoting. This struct is the STRUCTURAL fix: the frontend
/// renders one always-visible Chat bubble per entry, never derived from
/// narration text (T-p3q-01).
#[derive(Debug, Clone, serde::Serialize)]
pub struct GenerationDisclosure {
    /// Which generate_ai_* tool made the call: "image" | "video" | "audio".
    pub modality: String,
    /// The literal `prompt` argument Claude sent -- re-read verbatim from the
    /// tool's own `input`, never reformatted (T-p3q-01: the SAME value the
    /// existing GEN-09 disclosure text already reads, never a secret).
    pub prompt: String,
    /// Quick 260726-ufz: WHICH conditioning frames the call carried, in the
    /// caller's own vocabulary (e.g. `first frame: clipEnd(clip-3) | last frame:
    /// clipStart(clip-7)`), or the explicit `text only (no reference frames)`.
    ///
    /// Exists because a generation that silently ignored the frames is
    /// INDISTINGUISHABLE from one that used them: live UAT (2026-07-26) produced
    /// a Veo clip that landed nowhere near the plate it was told to end on, and
    /// there was no way — in the UI or in any log — to tell whether the frames
    /// had been sent at all. Absence is reported as loudly as presence for
    /// exactly that reason.
    ///
    /// Built by re-reading the tool's OWN `input` verbatim, the same discipline
    /// as `prompt` (T-p3q-01) — never re-derived from narration, and never the
    /// resolved bytes (which must not cross IPC, CLAUDE.md rule 4). `None` for
    /// modalities with no frame concept (audio), so the frontend renders nothing
    /// rather than a misleading "text only".
    pub frames: Option<String>,
    /// Phase 42.1: a standing, provider-level notice the user must see for this
    /// modality — today, Runway's model-training license on non-Enterprise
    /// plans. `None` for modalities whose provider carries no such notice
    /// (audio/ElevenLabs), so the frontend renders nothing rather than a claim
    /// about the wrong vendor.
    ///
    /// This rides the STRUCTURAL disclosure channel and not only the tool-result
    /// text for the same reason `prompt` does: T-p3q-01 established that nothing
    /// forces Claude to relay tool text verbatim, and live UAT proved it
    /// paraphrases. A legal disclosure that is conditional on the model choosing
    /// to repeat it is not a disclosure. The tool-result copy in
    /// [`RUNWAY_TRAINING_LICENSE_DISCLOSURE`] stays as well — it is what makes
    /// the AGENT aware of the constraint when it answers questions about it.
    pub provider_notice: Option<String>,
    /// Phase 42.1-03: WHICH MODEL actually ran, as a human label plus its id
    /// (e.g. `Google Veo 3.1 Fast (via Runway) (veo3.1_fast)`).
    ///
    /// Exists because Plan 03 made model choice a real, varying decision: an
    /// `intent` the agent picked — or the shape-derived default when it picked
    /// none — now selects between models that differ ~2.4x in price. The user
    /// paid for that choice and cannot see it anywhere else: the model id is
    /// server-side by design (T-33-19), the agent is never told it, and the tool
    /// result does not name it. Reporting it is the honesty half of keeping the
    /// choice server-side.
    ///
    /// Built by re-running the SAME resolvers the seam ran, over the tool's OWN
    /// `input` — the identical verbatim-from-backend discipline as `prompt` and
    /// `frames` (T-p3q-01), never narration, never a guess. `None` for a
    /// modality with no model choice to report, so the frontend renders nothing
    /// rather than a wrong name.
    pub model_resolved: Option<String>,
}

/// One `export_project` completion this turn — the STRUCTURAL record the Chat
/// UI renders so the export's real fate is never hostage to the model's
/// narration (the same T-p3q-01 rationale as [`GenerationDisclosure`]: nothing
/// forces Claude to relay tool text verbatim, and live UAT proved it
/// paraphrases).
///
/// Exists because of debug session `export-no-file-written` (2026-08-01): the
/// agent's `export_project` writes to a SERVER-DERIVED path under
/// `app_data_dir/exports` that the user cannot see anywhere else in the UI, so
/// three successful exports read as "export completes but no file appears".
/// The DESTINATION (or the failure) must ride a structural channel the
/// frontend renders deterministically.
///
/// Exactly one of the two fields is `Some`: `path` on success, `error` on
/// failure. Two `Option`s rather than an enum so the C# presenter's total,
/// never-throwing `OptionalString` reads apply unchanged (the same wire shape
/// discipline as [`GenerationDisclosure`]'s optional fields).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExportDisclosure {
    /// The written file's absolute path, on success.
    pub path: Option<String>,
    /// The failure reason, verbatim from the export path, on failure.
    pub error: Option<String>,
}

/// The user-facing wording of [`RUNWAY_TRAINING_LICENSE_DISCLOSURE`], addressed
/// to the USER rather than to the agent (the tool-result copy ends "Tell the
/// user this", which would be nonsense in a Chat bubble the user is reading).
///
/// Unconditional for Runway modalities — Rudis cannot see which plan tier a BYO
/// key belongs to, and under-disclosing to a Free-tier user is the failure that
/// matters. See the const above for the full reasoning.
/// **Phase 56 (GEN-11) extended the enumeration here too, and this is the copy
/// that actually reaches the user.**
///
/// The tool-result constant is addressed to the AGENT, and T-p3q-01 established
/// that nothing forces Claude to relay tool text verbatim — live UAT proved it
/// paraphrases. This string rides the STRUCTURAL disclosure channel the Chat
/// region renders deterministically, so discharging the GEN-08 condition only on
/// the agent-facing copy would have discharged it on the weaker of the two
/// channels. Extended in the same shape and for the same reason: the third
/// enumerated input is the user's own recorded clip, the older two are
/// untouched, and the new clause is conditional because this one string is read
/// on every Runway modality.
const RUNWAY_TRAINING_LICENSE_NOTICE: &str =
    "Runway may use what you send (your prompt, any reference frames, and -- when you \
     edit an existing clip -- that clip's own video, the footage you recorded) and what \
     it generates to train and improve its models, unless your Runway account is on an \
     Enterprise plan.";

/// Phase 56 (GEN-11): the clip-edit modality's own added sentence — the
/// indicative half of the notice above.
///
/// The shared notice can only ever put the clip-edit case conditionally, because
/// it is rendered for image and video generations that never send footage. A
/// conditional clause in a standing notice is the thing a reader skims; the one
/// fact the 2026-08-09 signature was conditioned on is that the user learns
/// *their own footage* was uploaded, so on this modality it is said outright.
const RUNWAY_SOURCE_CLIP_NOTICE: &str =
    " This edit uploaded your own source clip -- the footage on your timeline, not just a \
     prompt -- to Runway.";

/// The standing provider notice for one `generate_ai_*` modality, or `None`.
///
/// Keyed on MODALITY because that is what determines the provider: image, video
/// and clip edits go to Runway, audio to ElevenLabs. A future provider change
/// updates this one function.
pub fn provider_notice_for_modality(modality: &str) -> Option<String> {
    match modality {
        "image" | "video" => Some(RUNWAY_TRAINING_LICENSE_NOTICE.to_string()),
        // Phase 56 (GEN-11): same provider, same terms, plus the sentence only
        // this modality can honestly say in the indicative.
        "video edit" => Some(format!(
            "{RUNWAY_TRAINING_LICENSE_NOTICE}{RUNWAY_SOURCE_CLIP_NOTICE}"
        )),
        // ElevenLabs is a different provider under a different GEN-08 sign-off;
        // asserting Runway's terms about it would be a false statement.
        _ => None,
    }
}

/// Quick 260726-ufz: render the conditioning-frame summary for ONE `generate_ai_*`
/// tool input, or `None` when frames are not a concept for that modality.
///
/// Reads ONLY the already-present schema fields, verbatim (T-p3q-01). Note it
/// reports what Claude ASKED FOR, which is the question that actually needed
/// answering — the model omitting `destinationSource` and the backend failing to
/// resolve it are different bugs, and this distinguishes them. The `[E-01] veo
/// request:` stderr line reports the other half (what survived resolution).
fn describe_conditioning_frames(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    // Phase 56 (GEN-11 / T-56-FOOTAGE): the clip edit has no first/last frame
    // pair — its "conditioning frames" ARE the user's own footage, and WHICH
    // clip left the machine is the single most disclosure-worthy fact on this
    // path. It rides the structural channel for exactly the reason the notice
    // does: the tool result says it too, and nothing forces the model to relay
    // that. Reference images are refused today, so there is no second half to
    // report and none is invented.
    if tool_name == "generate_ai_video_edit" {
        let clip = input.get("clipId").and_then(|v| v.as_str())?;
        return Some(format!(
            "source clip {clip} (its current trimmed range -- your own footage, uploaded)"
        ));
    }
    if tool_name != "generate_ai_video" && tool_name != "generate_ai_image" {
        return None;
    }
    // `sketch`/`frame` name no id; `media`/`clipEnd`/`clipStart` do. Showing the
    // id is what makes the line verifiable against the timeline.
    let describe = |source_key: &str, media_key: &str, clip_key: &str| -> Option<String> {
        let source = input.get(source_key)?.as_str()?;
        let id = input
            .get(media_key)
            .and_then(|v| v.as_str())
            .or_else(|| input.get(clip_key).and_then(|v| v.as_str()));
        Some(match id {
            Some(id) => format!("{source}({id})"),
            None => source.to_string(),
        })
    };
    let first = describe(
        "referenceSource",
        "referenceMediaId",
        "referenceClipId",
    );
    let last = describe(
        "destinationSource",
        "destinationMediaId",
        "destinationClipId",
    );
    Some(match (first, last) {
        (None, None) => "text only (no reference frames)".to_string(),
        (Some(f), None) => format!("first frame: {f} | last frame: none"),
        (None, Some(l)) => format!("first frame: none | last frame: {l}"),
        (Some(f), Some(l)) => format!("first frame: {f} | last frame: {l}"),
    })
}

/// The whole Chat-turn result surfaced to the frontend: `agent_llm::TurnOutcome`
/// (narration/clarifying_question/options, UNCHANGED and still owned by the
/// model-agnostic `agent-llm` crate) plus this turn's `generation_disclosures`
/// -- an APP-LAYER-only concept, since `generate_ai_*` interception is a
/// Pattern-C mechanic living exclusively in this crate (plan 45-13 relocated
/// it from `src-tauri`; `is_intercepted_meta_tool`'s own doc comment: these
/// tools "must never enter agent-llm"). Field names
/// mirror `agent_llm::TurnOutcome`'s exactly (plus one additive field) so
/// every existing `.narration`/`.clarifying_question`/`.options` read on a
/// `run_agent_turn` result keeps compiling unchanged.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentTurnOutcome {
    pub narration: Option<String>,
    pub clarifying_question: Option<String>,
    pub options: Option<Vec<agent_llm::OptionCard>>,
    /// [bug/agent-turn-no-response-max-tokens] forwarded VERBATIM from
    /// `agent_llm::TurnOutcome::truncated` -- the turn ran out of output budget
    /// mid-write, so what came back is partial. src-tauri adds no interpretation
    /// (the Chat panel owns the user-facing wording); it only refuses to drop
    /// the fact on the floor.
    pub truncated: bool,
    pub generation_disclosures: Vec<GenerationDisclosure>,
    /// Debug session `export-no-file-written` (2026-08-01): every
    /// `export_project` completion this turn, success or failure, so the Chat
    /// panel can surface WHERE the file landed (or why it did not) without
    /// depending on the model narrating it. Additive — serialized as `[]` when
    /// no export ran, so every existing consumer parses unchanged.
    pub export_disclosures: Vec<ExportDisclosure>,
}

/// Compose the next user `MessageParam` for a Chat turn, resolving the Pitfall-5
/// resumption case: on a RESUMED `askUser` answer the message is PURELY the
/// pending tool_results (tool_results FIRST, per Anthropic's ordering rule) with
/// the answer appended as the `askUser` id's tool_result and NO extra `Text`
/// block; on a FRESH turn the CURRENT compact `agent_state` view is injected so
/// Claude always reasons from live state, not a stale session-start snapshot.
///
/// Phase 13/14.2 (SC-4 image half + D-04): on a FRESH turn, `image_blocks` —
/// the ordered vision snapshots assembled by `run_agent_turn` via
/// `spawn_blocking` (frame snapshot first, then the whiteboard snapshot; each
/// omitted when its space is empty or its snapshot could not be built) — are
/// attached as the FIRST content block(s), BEFORE the text block, per
/// Anthropic's documented vision ordering guidance. An empty collection yields
/// today's exact text-only shape. The resumed-`askUser`/option-choice branches
/// NEVER attach an image (snapshots are a fresh-turn-start concern only, A6).
/// Phase 42 (CR-01): true iff `history` — the FULL message list about to be
/// sent on round 0 (this turn's own freshly-attached whiteboard/frame snapshot,
/// PLUS every prior turn's accumulated content that survived
/// [bug/agent-history-413]'s `prune_stale_images` turn-start call) — carries real
/// vision content anywhere, either as a top-level `Image` block (a fresh or
/// earlier snapshot) or nested inside a `ToolResult` (an earlier
/// `inspect_timeline`/`inspect_media` result). `classify()` only ever reads the
/// CURRENT message's TEXT and structurally cannot see either, so the caller uses
/// this as a distinct pre-round-0 check that forces the Default tier whenever the
/// outgoing request already contains an image — the fail-closed guarantee that a
/// cheap-tier round's REQUEST never carries vision content.
/// [bug/agent-history-413] how many real image blocks `run_agent_turn` keeps
/// (newest-first) in `AgentSession.history` at the start of every turn, via
/// `agent_llm::prune_stale_images`. At LEAST `2` is required regardless of
/// product preference: a SINGLE turn can attach BOTH a frame snapshot AND a
/// whiteboard snapshot at once (`image_blocks` in `run_agent_turn`,
/// `image_count` up to 2) — pruning runs AFTER this turn's own fresh image(s)
/// are pushed, so they are the newest by construction, and a keep count below
/// the max simultaneous per-turn image count would stub one of THIS turn's own
/// just-attached snapshots before the model ever saw it.
///
/// Set to `3`, one above that floor (human decision, 2026-07-26, see the
/// `agent-history-413` debug session's Evidence log): `2` gave the agent ZERO
/// cross-turn visual memory — every turn it could see only its own
/// just-attached pair, never anything from an earlier turn, which breaks a
/// workflow like "make it more like the sketch I drew earlier". The 3rd slot
/// buys ONE image from the immediately preceding turn — by prune order that is
/// always the tail of that turn's own `image_blocks` push (frame first, board
/// second — see `run_agent_turn`), i.e. the prior turn's WHITEBOARD snapshot,
/// not its frame. This is deliberately a HALF prior-turn (one image, not the
/// full frame+whiteboard pair) — a full pair would need `4`. The human was
/// told this and chose `3` anyway; do not "round up" to `4` without asking.
const HISTORY_IMAGE_KEEP_COUNT: usize = 3;

/// Phase 42.3 gap closure (plan 42.3-06; 42.3-HUMAN-UAT.md tests 1b/2, live
/// 2026-07-28): what the RESUMED history tells the model about the HALTING
/// `generate_ai_*` call.
///
/// The halting call NEVER executed — the pre-spend gate halted the turn before
/// any provider work. Relaying the user's answer as this call's SUCCESSFUL
/// tool_result (the askUser idiom, correct only when the answer genuinely IS
/// the tool's result) made the model believe the call had run: it reported an
/// image that did not exist and never re-issued the call. This marker is the
/// structural fix, extending [`agent_llm::HALT_SKIP_ACK`]'s own doctrine
/// ("`is_error: Some(true)` on purpose: the call genuinely did NOT happen") to
/// the halting id itself. The user's verbatim reply rides separately as a Text
/// block after the tool_results — user bytes never live inside a synthesized
/// tool_result (T-42.3-15).
///
/// NOT a [`PRE_SPEND_VALIDATION_SUBSTRINGS`] entry, deliberately: this is minted
/// into a USER message at resume, and `dispatch_generate_with_cap`'s accounting
/// only ever inspects dispatcher-round tool_results, so the cap never reads it.
const SPEND_GATE_NOT_EXECUTED: &str =
    "not executed: this paid call was paused by the automatic pre-spend \
     confirmation gate and never ran -- nothing was generated, nothing was \
     billed. The user's reply to the confirmation follows as text in this \
     message. If they approved, re-issue this exact call now; it will run this \
     time. If they declined, do not re-issue it, and do not describe any \
     generation as started or done.";

fn history_carries_vision(history: &[agent_llm::MessageParam]) -> bool {
    history.iter().any(|m| {
        m.content.iter().any(|b| match b {
            agent_llm::ContentBlock::Image { .. } => true,
            agent_llm::ContentBlock::ToolResult { content, .. } => content
                .iter()
                .any(|t| matches!(t, agent_llm::ToolResultBlock::Image { .. })),
            _ => false,
        })
    })
}

pub fn build_user_turn(
    session: &mut AgentSession,
    project: &rudis_core::Project,
    selection: &[String],
    message: String,
    image_blocks: Vec<agent_llm::ContentBlock>,
) -> agent_llm::MessageParam {
    if let Some(pending) = session.pending_ask_user.take() {
        if pending.spend_confirmation {
            // Phase 42.3 (F): the ONE place the per-turn spend one-shot is
            // granted. The halt being resumed was synthesized by the pre-spend
            // gate, so the user has now seen the tool name + the resolved model
            // and replied. Granting on ANY answer stays deliberate (plan
            // 42.3-04): there is NO backend NLP on free text — this flag says
            // "the user was asked and answered", not "the user said yes".
            // Nothing spends unless the MODEL chooses to call the tool again,
            // and both the marker below and rulebook_v3's decline rule tell it
            // not to after a "no".
            session.spend_approved_turn = true;
            // Gap closure 42.3-06 (42.3-HUMAN-UAT.md tests 1b/2, live
            // 2026-07-28): the halting `generate_ai_*` call NEVER RAN, so its
            // tool_result must SAY SO — an error-typed fixed marker — instead of
            // relaying the user's words as a successful return. That relay is
            // what made the model believe the call had already executed: it
            // reported an image that did not exist, never re-issued the call,
            // and on a decline narrated "I've kicked off the AI image
            // generation". The reply rides as a Text block AFTER the
            // tool_results — the exact shape the proposeOptions resume below has
            // always shipped (Anthropic's ordering rule), which also keeps
            // user-controlled bytes out of a synthesized tool_result
            // (T-42.3-15) while still recording consent verbatim (T-42.3-16).
            let mut content = pending.prior_tool_results;
            content.push(agent_llm::ContentBlock::ToolResult {
                tool_use_id: pending.tool_use_id,
                content: agent_llm::vision::text_tool_result(SPEND_GATE_NOT_EXECUTED),
                is_error: Some(true),
            });
            content.push(agent_llm::ContentBlock::Text {
                text: format!("Reply to the pre-spend confirmation: {message}"),
            });
            return agent_llm::MessageParam {
                role: agent_llm::Role::User,
                content,
            };
        }
        // Resumed REAL-askUser answer — UNCHANGED Pitfall-5 branch, and it must
        // stay that way: here the answer genuinely IS `askUser`'s own result, so
        // relaying it verbatim with `is_error: None` and no Text block is
        // CORRECT. Only the gate-synthesized halt above (a result minted against
        // a DIFFERENT tool that never executed) needed the new shape.
        let mut content = pending.prior_tool_results;
        content.push(agent_llm::ContentBlock::ToolResult {
            tool_use_id: pending.tool_use_id,
            content: agent_llm::vision::text_tool_result(message),
            is_error: None,
        });
        agent_llm::MessageParam {
            role: agent_llm::Role::User,
            content,
        }
    } else if let Some(choice) = session.pending_option_choice.take() {
        // Phase 14 (CANV-02) THIRD branch: a prior turn halted on
        // `proposeOptions`. The halted tool_use ALWAYS gets a tool_result —
        // the server-held resolution summary when a card was applied, a
        // synthesized fallback when the user ignored the cards (T-14-16:
        // the every-tool_use-needs-a-tool_result invariant is unbreakable
        // by this mechanism). Unlike askUser, the user's new message is a
        // NEW request, so the fresh compact state + message text rides
        // along AFTER the tool_result (tool_results FIRST, per Anthropic's
        // ordering rule). Never an image (resumed turns skip the snapshot).
        let resolved = choice
            .resolved_summary
            .unwrap_or_else(|| "User did not apply an option card.".to_string());
        let view = rudis_core::agent_state::view(project, selection);
        let compact = rudis_core::agent_state::render_compact(&view);
        let text = format!("Current timeline state:\n{compact}\n\nUser request: {message}");
        // Debug session `agent-turn-dangling-tool-result-on-halt`: the halting
        // round's OTHER tool_results lead the message (Anthropic's ordering
        // rule), then the proposeOptions call's own answer, then the text. Any
        // id left out here is unanswerable forever.
        let mut content = choice.prior_tool_results;
        content.push(agent_llm::ContentBlock::ToolResult {
            tool_use_id: choice.tool_use_id,
            content: agent_llm::vision::text_tool_result(resolved),
            is_error: None,
        });
        content.push(agent_llm::ContentBlock::Text { text });
        agent_llm::MessageParam {
            role: agent_llm::Role::User,
            content,
        }
    } else {
        // Fresh turn — UNCHANGED.
        let view = rudis_core::agent_state::view(project, selection);
        let compact = rudis_core::agent_state::render_compact(&view);
        let text = format!("Current timeline state:\n{compact}\n\nUser request: {message}");
        // Image(s) strictly FIRST, in the caller's deterministic order (frame
        // snapshot then whiteboard snapshot); text-only (today's exact shape)
        // when the collection is empty — the structured coords in `text` stay
        // in every case (SC-4 needs BOTH halves in the same request). Phase
        // 14.2 (D-04): a turn may now carry TWO images (frame + whiteboard).
        let content: Vec<agent_llm::ContentBlock> = image_blocks
            .into_iter()
            .chain(std::iter::once(agent_llm::ContentBlock::Text { text }))
            .collect();
        agent_llm::MessageParam {
            role: agent_llm::Role::User,
            content,
        }
    }
}

/// The transport-generic helper BOTH the real `agent_send_message` command and
/// the deterministic `mod agent_gate` wiring test drive — the genericity is
/// exactly what makes the whole lock/event/undo discipline testable with NO
/// live key (a `FixtureTransport` stands in for `AnthropicTransport`).
///
/// Lock discipline (mirrors `export_timeline`): `SharedStore` is locked ONLY in
/// short synchronous spans — snapshotting the project, `begin_turn()`, each
/// synchronous `apply_response` inside the `apply_round` closure, and
/// `end_turn()` — and the guard is ALWAYS dropped before control reaches
/// EITHER `.await` (the vision-snapshot `spawn_blocking` handle or
/// `run_turn(...)`). No guard ever crosses an async boundary.
pub async fn run_agent_turn<
    T: agent_llm::LlmTransport,
    C: AppCtx + GenSubmission + SpendPolicy + AgentVision,
>(
    ctx: &C,
    session: &Mutex<AgentSession>,
    transport: &T,
    message: String,
    selection: Vec<String>,
    library_dir: &std::path::Path,
) -> Result<AgentTurnOutcome, String> {
    // Plan 45-13: the `store: &SharedStore` parameter became `ctx.store()`,
    // bound HERE on the first body line (the convention `import`, `export`,
    // `audio_sync` and `generation_bridge` already follow), so every
    // `store.lock()` span below -- including the four inside the `apply_round`
    // closure -- is BYTE-IDENTICAL to the pre-move code and provably reads the
    // same value: each call site builds its ctx from exactly the
    // `&SharedStore` it used to pass as this parameter.
    //
    // `session` deliberately STAYS an explicit parameter rather than becoming
    // `ctx.agent_session()`. All 34 non-production call sites pass a
    // STACK-LOCAL `Mutex<AgentSession>` and 17 of them seed or read it around
    // the call; the accessor resolves the app's MANAGED session, which those
    // tests provably cannot see. See this module's doc.
    let store = ctx.store();
    // [E-01] wall-clock start for the WHOLE turn (D-10 latency probe): the total
    // is logged just before returning, separating this turn's local snapshot
    // cost (timed individually above) from the model round-trip(s) (timed
    // per-round in agent_llm::turn). Diagnostic-only; logs durations/counts, no
    // message content (threat T-14.3-05).
    let turn_started = std::time::Instant::now();
    // Phase 14 (MOAT-03/MOAT-04, DECISIONS.md A5): retrieval + skill matching
    // key off the turn's RAW request text ONLY (never canvas/annotation
    // content), assembled into ONE dynamic-context string that becomes the
    // second, UNCACHED system block. `library_dir` is backend-computed by the
    // caller (`app_data_dir()/agent_library` in production, a temp dir in
    // tests) — never derived from IPC input (T-14-15). Assembled here, before
    // `message` moves into build_user_turn below.
    let mut library = agent_llm::seed_library();
    library.extend(agent_llm::load_library(library_dir));
    let examples = agent_llm::select_top_k(&message, &library, 5); // k=5, DECISIONS.md A6
    let playbooks = agent_llm::matching_playbooks(&message);
    let mut dynamic = String::new();
    if !examples.is_empty() {
        dynamic.push_str(&agent_llm::render_examples_block(&examples));
    }
    if !playbooks.is_empty() {
        dynamic.push_str(&agent_llm::render_playbooks_block(&playbooks));
    }
    // D-07 (Phase 17-03): the one-line skill index rides the UNCACHED dynamic
    // block on EVERY turn so the agent always knows which skill ids exist for
    // read_skill — appended unconditionally, so `dynamic` is never empty and
    // `dynamic_context` below is never None. Phase 42.3 (A): the app now runs
    // RULEBOOK_V3 (v2 minus the two extracted section bodies, plus the compact
    // `# Generation` section); V1/V2 remain byte-stable frozen artifacts, and
    // the never-None invariant above is unchanged — which is exactly why the
    // extraction is safe, since a trigger MISS still leaves both playbook ids
    // visible in this index for `read_skill`.
    if !dynamic.is_empty() {
        dynamic.push('\n');
    }
    dynamic.push_str("# Available skills (load the full body with read_skill):\n");
    dynamic.push_str(&agent_llm::render_skill_index());

    // Short sync span: snapshot the live project (an OWNED clone — the guard
    // drops immediately; the vision-snapshot resolution below re-reads NOTHING
    // from the store).
    let project = store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .snapshot();

    // The raw request text, cloned BEFORE `message` moves into
    // build_user_turn below — it becomes the appended `LibraryExample.request`
    // if this turn qualifies for library growth (DECISIONS.md A1).
    let request_text = message.clone();

    // Peek (short sync span): a RESUMED turn (a pending askUser answer OR a
    // pending proposeOptions choice) never attaches an image (fresh-turn-start
    // only, Assumption A6 / T-13-15) — skip the blocking decode entirely
    // rather than building a block the resumed branches drop.
    let resumed = {
        let mut sess = session
            .lock()
            .map_err(|_| "agent session poisoned".to_string())?;
        // DECISIONS.md A1: a NEW turn starting FINALIZES (silently accepts)
        // any prior turn's still-pending library growth — take-and-drop the
        // tracking record, NEVER deleting the file. Only the immediately-next
        // `undo` could have retracted it, and this isn't that.
        sess.pending_growth = None;
        let resumed = sess.pending_ask_user.is_some() || sess.pending_option_choice.is_some();
        // ME-01: a FRESH turn starts with a CLEAN diagnostic error — clear the
        // session's `last_error` so a later `send_feedback` reports only THIS
        // turn's failure(s), never a stale error carried over from a chat turn
        // that failed many turns ago. A RESUMED turn (answering a pending
        // askUser / proposeOptions) is a continuation of the same logical turn,
        // so it KEEPS whatever error the in-flight turn already recorded.
        if !resumed {
            sess.last_error = None;
        }
        // Phase 42.3 (F): the spend one-shot is cleared at the start of EVERY
        // turn — including a resumed one — and re-granted a few lines below by
        // `build_user_turn` IF (and only if) this turn is resuming the gate's own
        // confirmation halt. Deliberately NOT scoped like `last_error` above:
        // that field is diagnostic, this one authorizes real money, and the
        // continuation of a turn that halted on some OTHER question is not
        // evidence that the user re-approved spending. Fail-closed (T-42.3-02).
        // The sanctioned CHECK-02/03 silent retry is unaffected: it happens
        // inside the SAME `run_agent_turn` call, after the grant.
        sess.spend_approved_turn = false;
        resumed
    };

    // Phase 13/14.2 (SC-4 + D-04): assemble the turn's snapshot image(s) off the
    // async runtime thread, in DETERMINISTIC order — the frame vision snapshot
    // FIRST, the whiteboard snapshot SECOND (so any future request-shape/golden
    // test stays stable). Each is skipped when its space is empty or its build
    // fails (graceful text-only degradation); a RESUMED turn attaches neither
    // (snapshots are a fresh-turn-start concern only, A6). Both `.await`s run off
    // the store lock — the guard was dropped when `project` was snapshotted.
    //
    // Phase 14.3 (D-03): raster the whiteboard PNG at the live panel aspect the
    // frontend last reported via `canvas-surface-resize` (mirrored into managed
    // `Mutex<WhiteboardAspect>`), clamped by `clamp_raster_dims`. Read in a short
    // sync span (the guard drops before either `.await`); falls back to the
    // retired board's 1280x720 aspect when the mirror is absent (no report yet,
    // or the mock-runtime tests, which never manage it).
    let (rw, rh) = ctx.whiteboard_raster_dims();
    let image_blocks: Vec<agent_llm::ContentBlock> = if resumed {
        Vec::new()
    } else {
        let mut blocks = Vec::new();
        // [E-01] time the frame vision-snapshot builder (decode+annotate+encode).
        let t_frame = std::time::Instant::now();
        let frame = ctx.vision_snapshot_block(&project).await;
        eprintln!(
            "[E-01] frame snapshot {:?} present={}",
            t_frame.elapsed(),
            frame.is_some()
        );
        if let Some(frame) = frame {
            blocks.push(frame);
        }
        // [E-01] time the whiteboard-snapshot builder (raster+encode).
        let t_board = std::time::Instant::now();
        let board = ctx.whiteboard_snapshot_block(&project, rw, rh).await;
        eprintln!(
            "[E-01] whiteboard snapshot {:?} present={}",
            t_board.elapsed(),
            board.is_some()
        );
        if let Some(board) = board {
            blocks.push(board);
        }
        blocks
    };
    // [E-01] how many images this turn carries (1 = frame XOR board, 2 = the
    // double-image turn RESEARCH §5 flags as a latency amplifier) — captured
    // before `image_blocks` moves into build_user_turn below.
    let image_count = image_blocks.len();

    // Short sync span: compose the user's turn.
    let (mut history, user_turn) = {
        let mut sess = session
            .lock()
            .map_err(|_| "agent session poisoned".to_string())?;
        let turn = build_user_turn(&mut sess, &project, &selection, message, image_blocks);
        (std::mem::take(&mut sess.history), turn)
    };
    history.push(user_turn);

    // [bug/agent-history-413] turn-start image pruning — Layer 2 of the fix,
    // and the ACTUAL bound: Layer 1 (the JPEG-not-PNG chat vision snapshot,
    // `build_vision_snapshot_jpeg`) only buys headroom per turn, but
    // `AgentSession.history` was otherwise never pruned by turn count, so an
    // uncapped run of canvas/vision turns still grew the persisted,
    // every-request-resent history without bound until the Anthropic Messages
    // API's 32MB HTTP request-body cap rejected EVERY future request with an
    // unrecoverable `413 request_too_large` — the codebase's own prior comment
    // (still true one line below until this fix) named the exact mechanism.
    // Keep the newest `HISTORY_IMAGE_KEEP_COUNT` real image blocks (BOTH
    // top-level `ContentBlock::Image` snapshots and nested
    // `ToolResultBlock::Image` inspect_* results) and replace every older one
    // with a short text stub — a stale snapshot is stale STATE the model
    // should not reason from anyway. `HISTORY_IMAGE_KEEP_COUNT` must be AT
    // LEAST 2 (a single turn can attach BOTH a frame AND a whiteboard snapshot
    // at once — `image_blocks` above, `image_count` up to 2 — and pruning runs
    // AFTER this turn's own fresh image(s) are already pushed into `history`,
    // so they are by construction the NEWEST and always survive); it is set to
    // `3` (human decision, 2026-07-26) specifically to ALSO keep one image from
    // the immediately preceding turn, so a "make it more like the sketch I
    // drew earlier" request has something to reference — `2` gave the agent
    // zero cross-turn visual memory. That 3rd slot is deliberately a HALF
    // prior turn: it lands on whichever of that turn's images was pushed LAST
    // (frame first then whiteboard, per `image_blocks` above), i.e. the prior
    // turn's whiteboard snapshot, not a full frame+whiteboard pair (a full
    // pair would need `4` — see `HISTORY_IMAGE_KEEP_COUNT`'s own doc comment).
    // Mutates `history` in place BEFORE `history_carries_vision` below (Layer
    // 2 fully precedes CR-01's classify-blindness check) so that scan — and
    // the request actually built and sent — see IDENTICAL, already-pruned
    // content; pruning can only ever REMOVE vision content, never add it, so
    // this ordering cannot cause a real image to slip onto the cheap tier.
    let stubbed = agent_llm::prune_stale_images(&mut history, HISTORY_IMAGE_KEEP_COUNT);
    if stubbed > 0 {
        eprintln!(
            "[E-01] turn start: pruned {stubbed} stale image block(s) from history \
             (keeping the newest {HISTORY_IMAGE_KEEP_COUNT})"
        );
    }

    // Short sync span: open the ONE undo group for the whole turn, drop the
    // guard immediately (AGENT-03/AGENT-05).
    store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .begin_turn();

    // An EMPTY assembly means no second block at all: `None` produces a
    // request byte-shape-identical to the pre-Phase-14 `run_turn` shape.
    let dynamic_context = if dynamic.is_empty() {
        None
    } else {
        Some(dynamic.as_str())
    };

    let mut captured_ask: Option<PendingAskUser> = None;
    let mut captured_options: Option<PendingOptionChoice> = None;
    // Library-growth accumulators (DECISIONS.md A1), filled across EVERY
    // round via the same closure-capture pattern `captured_ask`/
    // `captured_options` use: the successfully dispatched `{tool,args}`
    // calls and every entity id the turn's own patches touched.
    let mut dispatched_calls: Vec<serde_json::Value> = Vec::new();
    let mut touched_ids: Vec<String> = Vec::new();
    // PROMPT-03 gap fix (T-p3q-01): a structural, narration-independent
    // record of every successful generate_ai_* dispatch this turn -- filled
    // inside the SAME apply_round closure below, exactly like
    // `dispatched_calls`/`touched_ids` above, and read again AFTER the
    // `.await` resolves (same proven pattern those two accumulators use).
    let mut generation_disclosures: Vec<GenerationDisclosure> = Vec::new();
    // Debug session `export-no-file-written`: the same accumulator pattern, for
    // `export_project` completions — filled inside the apply_round closure's
    // meta-tool arm and read again AFTER the `.await` resolves.
    let mut export_disclosures: Vec<ExportDisclosure> = Vec::new();
    // CHECK-03 (Phase 39): bounded per-turn, per-tool-name counter -- caps a
    // self-check-triggered retry of a PAID generation call at ONE automatic
    // attempt beyond the first. Independent of MAX_ROUNDS (which caps model
    // round-trips, not provider spend). Resets every new turn (a fresh local,
    // never persisted) -- 39-RESEARCH.md "Where the bounded retry counter lives".
    let mut generate_call_counts: std::collections::HashMap<&'static str, u32> =
        std::collections::HashMap::new();

    // Phase 42 (ROUTE-01/02): the routing decision for round 0, made from the
    // raw incoming user message ONLY (never turn length, Pitfall D1) --
    // classify() is a pure function in agent_llm::routing. `escalated` is a
    // STICKY flag shared with the apply_round closure below (the EXISTING
    // Pattern-C meta-tool pre-scan, which already inspects every round's raw
    // tool_use blocks) -- std::sync::Arc<AtomicBool> (not Rc<Cell<bool>>)
    // because this function is the body of a #[tauri::command] async fn,
    // whose future Tauri's async runtime may require to be Send.
    // CR-01: classify() only ever reads the CURRENT message's TEXT — by design
    // it is blind to the vision content this round's REQUEST actually carries.
    // Two such sources exist and neither routing layer can otherwise see them:
    //   (a) this turn's freshly-attached whiteboard/frame snapshot (pushed into
    //       `history` above via `user_turn`, gated only on `Project.canvas`
    //       being non-empty, NOT on the message text), and
    //   (b) any earlier turn's image still riding forward in `AgentSession.history`
    //       (a top-level `Image` block or a nested `ToolResultBlock::Image` from
    //       a prior inspect_* call) that survived the `prune_stale_images` call
    //       above -- [bug/agent-history-413] history is no longer UNBOUNDED, but
    //       it can still carry up to `HISTORY_IMAGE_KEEP_COUNT` real images, and
    //       classify() is just as blind to those as it ever was to the old
    //       never-pruned backlog.
    // Because `history` at THIS point already contains round 0's own outgoing
    // content (already pruned), a single scan over it covers both (a) and (b). If any image is
    // present, force the INITIAL tier to Default so round 0's real image is never
    // dispatched on the cheap (Haiku) tier — the fail-closed direction ROUTE-02's
    // "regardless of apparent triviality" requires. (Conservative/coarse: once a
    // session has carried any image, it stays on the default tier — acceptable
    // per the review; a last-K-messages refinement is a possible later scope.)
    let request_carries_vision = history_carries_vision(&history);
    let initial_tier = if request_carries_vision {
        agent_llm::routing::RouteTier::Default
    } else {
        agent_llm::routing::classify(&request_text)
    };
    if request_carries_vision {
        eprintln!(
            "[ROUTE] round 0 forced to Default tier: outgoing request carries vision content \
             (canvas/frame snapshot or accumulated-history image) classify() cannot see"
        );
    }
    let escalated = std::sync::Arc::new(AtomicBool::new(false));
    // Phase 42 (WR-01): tools are re-resolved EVERY round via a closure (mirroring
    // `model_for_round`), so a mid-turn escalation widens the schema from the cheap
    // subset to the full `tool_defs()` set — not just the model. Both the cheap and
    // full defs are built once up front and cloned per round.
    let escalated_for_tools = escalated.clone();
    let cheap_tier_tools = agent_llm::routing::cheap_tier_tool_defs();
    let full_tools = agent_llm::tool_defs();
    let tools_for_round = move |_round: u32| -> Vec<agent_llm::ToolDef> {
        if escalated_for_tools.load(Ordering::Relaxed) {
            return full_tools.clone();
        }
        match initial_tier {
            agent_llm::routing::RouteTier::Cheap => cheap_tier_tools.clone(),
            agent_llm::routing::RouteTier::Default => full_tools.clone(),
        }
    };
    let escalated_for_model = escalated.clone();
    let model_for_round = move |round: u32| -> String {
        let env_override = std::env::var("RUDIS_AGENT_MODEL").ok();
        let model = agent_llm::routing::resolve_model(
            initial_tier,
            escalated_for_model.load(Ordering::Relaxed),
            env_override.as_deref(),
        );
        // [ROUTE] diagnostic-only, stdout/stderr never returned to the
        // frontend (ROUTE-03) -- mirrors the existing [E-01] logging style.
        eprintln!(
            "[ROUTE] round {round}: tier={initial_tier:?} escalated={} model={model}",
            escalated_for_model.load(Ordering::Relaxed)
        );
        model
    };

    let outcome = agent_llm::run_turn_with_context_and_model(
        transport,
        // Phase 42.3 (A): the ONE production rulebook call site. V3 is v2 with
        // the Transitions and Prompt-expansion bodies extracted into
        // trigger-loaded skill playbooks and a compact `# Generation` section
        // added — 15,640 bytes against v2's 18,761, on every single turn.
        agent_llm::RULEBOOK_V3,
        dynamic_context,
        tools_for_round,
        &mut history,
        model_for_round,
        |response| {
            // Phase 17-04 (Pattern C / Pitfall 4): send_feedback (and, in Task 2,
            // export_project) are META tools that need `app_data_dir` + the engine
            // — both of which live ONLY in src-tauri and must NEVER be pulled into
            // agent-llm. Pre-scan this round's response for those NON-EDIT meta
            // tool_use blocks, STRIP them from what `apply_response` sees (so it
            // never mints a duplicate/spurious `is_error` tool_result for an
            // unknown name), handle them here at the src-tauri layer, and merge
            // their minted tool_results back in content order.
            let meta: Vec<(String, String, serde_json::Value)> = response
                .content
                .iter()
                .filter_map(|b| match b {
                    agent_llm::ContentBlock::ToolUse { id, name, input }
                        if is_intercepted_meta_tool(name) =>
                    {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();
            // Phase 42 (ROUTE-02): sticky escalation -- the instant this
            // round's raw response contains an escalation-trigger tool_use,
            // force every SUBSEQUENT round onto the Default tier
            // (42-RESEARCH.md §2). Reuses the SAME `meta` pre-scan above --
            // no second scan.
            if agent_llm::routing::should_escalate(meta.iter().map(|(_, name, _)| name.as_str())) {
                escalated.store(true, Ordering::Relaxed);
            }
            // The response `apply_response` actually sees: meta blocks removed.
            // When there are none, pass the original by reference (zero-copy, the
            // pre-17-04 byte-for-byte path).
            let filtered = if meta.is_empty() {
                None
            } else {
                let mut r = response.clone();
                r.content.retain(|b| {
                    !matches!(b, agent_llm::ContentBlock::ToolUse { name, .. } if is_intercepted_meta_tool(name))
                });
                Some(r)
            };
            let response_ref = filtered.as_ref().unwrap_or(response);

            // Synchronous-only critical section: lock, apply, DROP the guard
            // before emitting or capturing anything. No lock is held past here.
            let mut guard = store.lock().expect("backend store mutex poisoned");
            let mut round = agent_llm::apply_response(&mut guard, response_ref, &selection);
            drop(guard);
            for (patch, base_seq, seq) in &round.patches {
                // Phase 43 (LAT-02): routed through emit_changed so this
                // mid-turn emission carries the SAME (base_seq, seq) envelope
                // as every other site — each pair was captured atomically by
                // the `Store::dispatch` that produced its patch, so an N-tool
                // turn emits N seq-correct thin patches (D-15/D-16: the
                // renderer must NOT resync once per tool call).
                let _ = ctx.emit_patch(patch, *base_seq, *seq); // AGENT-05: live, mid-turn
                touched_ids.extend(patch.ids.iter().cloned());
            }
            dispatched_calls.extend(round.dispatched_calls.iter().cloned());

            // Phase 17-04 (D-08): roll THIS round's diagnostics into the session
            // buffer that send_feedback reads — every successfully dispatched
            // `{tool,args}` (bounded cap 15, palmier Pattern 9) plus the last real
            // error text. The store guard is already dropped; the session lock is a
            // short span here (store IS NOT re-locked while it is held, so there is
            // no deadlock against apply_option_card_inner's session->store order).
            {
                let mut sess = session
                    .lock()
                    .expect("agent session poisoned");
                for call in &round.dispatched_calls {
                    sess.recent_tool_calls.push_back(call.clone());
                    while sess.recent_tool_calls.len() > 15 {
                        sess.recent_tool_calls.pop_front();
                    }
                }
            }

            // CR-01: `apply_response` enforces "stop at the first control block,
            // do NOTHING after it" for every ordinary edit tool — but the two
            // intercepted meta tools bypass `apply_response` entirely, so that
            // invariant must be mirrored HERE. `send_feedback` writes a real file
            // and `export_project` runs a real FFmpeg encode; neither may fire on
            // a round that also halted the turn on `askUser`/`proposeOptions`
            // (e.g. a hallucinated response emitting `askUser` AND `export_project`
            // together) — the user must get to answer BEFORE any consequential,
            // side-effecting action runs.
            let mut halted = round.asked.is_some() || round.options.is_some();

            // Phase 42.3 (F): bookkeeping for a halt synthesized by the pre-spend
            // confirmation gate below. `unacked_meta_from` is the index of the
            // FIRST `meta` entry that did NOT run (everything before it already
            // pushed a real tool_result); `gate_halt_id` is the halting
            // `generate_ai_*` tool_use id. Both are read by the CR-01 ack loop.
            // Left at `(0, None)` when no gate halt occurs, which reproduces the
            // pre-42.3 behavior exactly for a real askUser/proposeOptions halt.
            let mut unacked_meta_from = 0usize;
            let mut gate_halt_id: Option<String> = None;

            // On a NON-halted round, handle each intercepted meta tool_use in
            // content order, merging its minted tool_result into the round (which
            // the loop feeds back to the model as the next user turn). Each handler
            // NEVER panics — a failure mints an `is_error` tool_result.
            if !halted {
                for (index, (id, name, input)) in meta.iter().enumerate() {
                    // Phase 42.3 (F) / SC-6: the PRE-SPEND confirmation gate. The
                    // FIRST unconfirmed paid `generate_ai_*` call in a turn does
                    // not run at all — the turn halts and asks. Placed here, ahead
                    // of the `match` below, so it covers all three modalities
                    // (ElevenLabs audio is a paid external call too) with ONE
                    // decision site and leaves `dispatch_generate_with_cap`
                    // completely untouched: the cap is the POST-spend guard, this
                    // is the PRE-spend one, and they compose.
                    if matches!(
                        name.as_str(),
                        "generate_ai_image"
                            | "generate_ai_video"
                            | "generate_ai_audio"
                            // Phase 56 (GEN-11 / D-04): the clip edit is a paid
                            // Runway call like the other three, and the DEAREST
                            // of them — it is metered by the length of input,
                            // so one call reaches $8.40 at the window maximum
                            // against $1.12 for a 4-second text-to-video.
                            | "generate_ai_video_edit"
                    ) {
                        // Short sync span (the established discipline in this
                        // closure): read the one-shot and DROP the guard before
                        // any dispatch — several meta handlers below take this
                        // very lock.
                        let approved = session
                            .lock()
                            .expect("agent session poisoned")
                            .spend_approved_turn;
                        // Re-run the SAME resolver the disclosure uses, over the
                        // tool's OWN input, so the question names the model that
                        // would actually be billed (T-42.3-13).
                        let resolved = ctx.resolved_model_for_tool_input(name, input);
                        // Phase 55.1 (D-01): and its PRICE, read from the SAME
                        // `input["model"]` — the roster's own line, or the
                        // literal "price unknown" for an id it cannot price.
                        // With the roster open this is the only cost control
                        // left, so an unnamed spend is not an option (T-55.1-08).
                        //
                        // Phase 56 (D-56-05-01): the clip edit's price is not a
                        // property of the model alone — it is metered per
                        // SECOND OF INPUT, so it is a property of the model AND
                        // the range the user chose. `cost_signal_for_tool_input`
                        // returns `&'static str` and has no arm for this tool,
                        // which would have handed the gate `(Some(model), None)`
                        // — the branch that prints the model with NO price and
                        // NO "price unknown". Silence, not honest absence, on
                        // the one path that reaches $8.40 in a single call. So
                        // the cost is built HERE, where the store is reachable,
                        // and `video_edit_cost_signal` guarantees it is either a
                        // real figure or the SAME `PRICE_UNKNOWN` literal
                        // 55.1-04 already ships.
                        let per_call_cost: Option<String> = (name
                            == "generate_ai_video_edit")
                            .then(|| {
                                // The model this call WILL submit — resolved by
                                // the one normalization the host itself uses, so
                                // the question and the bill cannot disagree.
                                let model = crate::generation_host::resolved_video_edit_model_id(
                                    input.get("model").and_then(|v| v.as_str()),
                                );
                                // The clip's VISIBLE (trim-respecting) length —
                                // the range `extract_clip_range_mp4` will
                                // actually send, so the estimate meters what the
                                // vendor will bill for. SHORT lock, dropped
                                // before the gate call. An unknown clip id or a
                                // poisoned mutex yields `None`, which prices as
                                // "unknown" and is followed moments later by the
                                // handler's own clean Err naming the id.
                                let visible_us =
                                    input.get("clipId").and_then(|v| v.as_str()).and_then(
                                        |clip_id| {
                                            let guard = store.lock().ok()?;
                                            crate::compose::timeline_clip(guard.timeline(), clip_id)
                                                .map(|c| c.out_us - c.in_us)
                                        },
                                    );
                                crate::generation_host::video_edit_cost_signal(
                                    model.as_deref(),
                                    visible_us,
                                )
                            });
                        let cost: Option<&str> = match per_call_cost.as_deref() {
                            Some(per_call) => Some(per_call),
                            None => crate::generation_host::cost_signal_for_tool_input(name, input),
                        };
                        if let SpendGateDecision::NeedsConfirmation { question } =
                            ctx.spend_confirmation_gate(name, resolved.as_deref(), cost, approved)
                        {
                            // 1. `dispatch_generate_with_cap` is NOT invoked: no
                            //    handler, no provider work, no file, no cap
                            //    increment. The gate halt is pre-spend by
                            //    construction.
                            //
                            //    Note this call produces NO tool_result here at
                            //    all (it is not a refusal result — its result is
                            //    minted at RESUME by `build_user_turn`), so the
                            //    cap never sees it and
                            //    `PRE_SPEND_VALIDATION_SUBSTRINGS` is deliberately
                            //    NOT extended by this gate.
                            //
                            // 2. The halt is SYNTHESIZED into `round.asked` — the
                            //    exact structure a real `askUser` halt populates —
                            //    so `run_turn_inner` returns it as the turn's
                            //    `clarifying_question` through the existing
                            //    channel, the capture below builds a
                            //    `PendingAskUser`, and the whole 2026-07-26
                            //    ack/reconcile machinery applies unchanged. A
                            //    bespoke break/early-return here would re-open
                            //    debug session
                            //    `agent-turn-dangling-tool-result-on-halt` on day
                            //    one.
                            round.asked = Some(agent_llm::AskUser {
                                tool_use_id: id.clone(),
                                question,
                            });
                            gate_halt_id = Some(id.clone());
                            // 3. Stop processing FURTHER meta tools this round;
                            //    this index and everything after it is unrun and
                            //    routes through the CR-01 ack loop below.
                            unacked_meta_from = index;
                            halted = true;
                            // 4. At most ONE gate halt per round, by construction.
                            break;
                        }
                    }
                    // Plan 45-13: every arm below used to build its own
                    // `&TauriAppCtx::new(&app_for_events, store)` inline. Now
                    // that THIS function is generic over the ctx, they all pass
                    // `ctx` -- the same value, one construction earlier. The
                    // per-arm "built here from the exact `AppHandle` +
                    // `&SharedStore`" notes below describe that pre-45-13 shape
                    // and are kept verbatim as the history of each move.
                    let result = match name.as_str() {
                        // Phase 45 (45-10): both now live in `app-core`, driven
                        // through the ONE `TauriAppCtx`, built here from the exact
                        // `AppHandle` + `&SharedStore` these arms already passed --
                        // so `ctx.store()` IS the former `store` arg.
                        "send_feedback" => handle_send_feedback(
                            ctx,
                            session,
                            id,
                            input,
                        ),
                        "export_project" => handle_export_project(
                            ctx,
                            id,
                            &mut export_disclosures,
                        ),
                        "inspect_timeline" => handle_inspect_timeline(store, session, id, input),
                        "inspect_media" => handle_inspect_media(store, id, input),
                        "get_transcript" => handle_get_transcript(store, session, id, input),
                        "search_media" => handle_search_media(store, session, id, input),
                        // Phase 45 (45-10): the two ENGINE-based generators
                        // (contrast the external-provider `generate_ai_*` arms
                        // further down, which 45-11 moves).
                        "generate_image" => handle_generate_image(
                            ctx,
                            id,
                            input,
                        ),
                        "generate_video" => handle_generate_video(
                            ctx,
                            id,
                            input,
                        ),
                        // Phase 26 (LIB-02): the project-switch interceptions.
                        // Phase 45 (45-06): now in `app_core::project`, driven
                        // through the ONE `TauriAppCtx`, built here from the
                        // exact `AppHandle` + `&SharedStore` these arms already
                        // passed — so `ctx.store()` IS the former `store` arg.
                        "new_project" => handle_new_project(
                            ctx,
                            id,
                            input,
                        ),
                        "open_project" => handle_open_project(
                            ctx,
                            id,
                            input,
                        ),
                        // Phase 26 (TOOL-04): read-only registry listing. Takes
                        // no `input` — only the ctx + the tool_use id.
                        "get_projects" => {
                            handle_get_projects(ctx, id)
                        }
                        // Phase 27 (LIB-03): import ONE real external file. Needs
                        // engine::probe + app_cache_dir (poster), which live ONLY
                        // here — the SAME Pattern-C interception generate_image uses.
                        // Phase 45 (45-07): now `app_core::import`, driven
                        // through the ONE `TauriAppCtx` — built here from the
                        // exact `AppHandle` + `&SharedStore` this arm already
                        // passed, so `ctx.store()` IS the former `store` arg.
                        "import_media" => {
                            handle_import_media(ctx, id, input)
                        }
                        // Phase 27 (LIB-03): render a solid/gradient matte through
                        // render_scene_frame + engine::VideoEncoder (both live ONLY
                        // here) — the SAME Pattern-C interception generate_video uses.
                        // Phase 45 (45-09): now `app_core::matte`, driven
                        // through the ONE `TauriAppCtx` — built here from the
                        // exact `AppHandle` + `&SharedStore` this arm already
                        // passed, so `ctx.store()` IS the former `store` arg.
                        "create_matte" => handle_create_matte(
                            ctx,
                            id,
                            input,
                        ),
                        // Phase 27 (LIB-04): correlate two clips' CURRENT audio via
                        // engine::render_audio_pcm + engine::best_lag_us (both live
                        // ONLY here) and, above the confidence floor, dispatch a real
                        // Command::MoveClip on the target — the SAME Pattern-C shape.
                        // Phase 45 (45-09): now `app_core::audio_sync`, same
                        // ctx shape.
                        "sync_audio" => handle_sync_audio(
                            ctx,
                            id,
                            input,
                        ),
                        // Phase 29 (OVL-03): list the reusable overlay-asset catalog
                        // (bundled resource dir + app_data_dir/overlay-library). Needs
                        // resource_dir/app_data_dir (Tauri-only, live ONLY here) — the
                        // SAME Pattern-C read shape get_projects uses. Takes no input.
                        // Phase 45 (45-08): now `app_core::overlay`, driven through
                        // the ONE `TauriAppCtx` — the resource-dir half is
                        // `AppCtx::resolve_resource`, the trait's 7th method.
                        "get_overlay_library" => {
                            handle_get_overlay_library(ctx, id)
                        }
                        // Phase 29 (OVL-02): import-if-needed + place + style a
                        // reusable overlay asset (or an already-imported media
                        // item) as ONE composited layer. Needs engine::probe +
                        // resource_dir/app_data_dir (live ONLY here) and dispatches
                        // existing Commands under the open turn — the SAME
                        // Pattern-C interception create_matte uses. ZERO new
                        // Command variants; one-turn-one-undo is free.
                        // Phase 45 (45-08): now `app_core::overlay`; the ctx is
                        // built from the exact `AppHandle` + `&SharedStore` this
                        // arm already passed, so `ctx.store()` IS the former arg.
                        "place_overlay" => handle_place_overlay(
                            ctx,
                            id,
                            input,
                        ),
                        // Phase 29 (OVL-03): render ONE overlay clip/media through
                        // the transparent-clear compositor + license-clean png/
                        // prores_ks encoders (engine, live ONLY here), write a
                        // confined server-built file, and re-import it — the SAME
                        // Pattern-C interception create_matte uses. NEVER touches
                        // the frozen H.264/MF export path (SC-4).
                        // Phase 45 (45-08): now `app_core::overlay`, same ctx shape.
                        "export_overlay_asset" => handle_export_overlay_asset(
                            ctx,
                            id,
                            input,
                        ),
                        // Phase 30 (TRK-01/TRK-02): decode the source clip + run
                        // the license-clean opencv CSRT/KCF tracker (engine, live
                        // ONLY here) and compose the EXISTING SetKeyframes(Position)
                        // on the target overlay clip under the open turn — the SAME
                        // Pattern-C interception. ZERO new Command variants.
                        // Phase 45 (45-09): now `app_core::tracking`, the
                        // first `app-core` code to reach `engine::tracking`.
                        "track_object" => handle_track_object(
                            ctx,
                            id,
                            input,
                        ),
                        // Phase 32 (GEN-01): generate_ai_image routes a prompt
                        // through the OpenAI generation seam (ManagedGenProvider +
                        // the GEN-08 gate + landing bridge, all living ONLY here)
                        // and hands the agent back the landed media id + the image
                        // itself — the SAME Pattern-C interception generate_image
                        // uses, but calling the REAL external provider.
                        "generate_ai_image" => dispatch_generate_with_cap(
                            "generate_ai_image",
                            id,
                            &mut generate_call_counts,
                            || {
                                handle_generate_ai_image(
                                    ctx,
                                    id,
                                    input,
                                )
                            },
                        ),
                        // Phase 33 (GEN-02): generate_ai_video routes a prompt
                        // through the async Veo generation seam (the modality-
                        // scoped ManagedVideoGenProvider + the GEN-08 gate + the
                        // landing bridge, all living ONLY here), awaits the poll
                        // loop, and hands the agent back the landed media id + a
                        // decoded frame — the SAME Pattern-C interception
                        // generate_ai_image uses, but for real external video.
                        "generate_ai_video" => dispatch_generate_with_cap(
                            "generate_ai_video",
                            id,
                            &mut generate_call_counts,
                            || {
                                handle_generate_ai_video(
                                    ctx,
                                    id,
                                    input,
                                )
                            },
                        ),
                        // Phase 34 (GEN-03): generate_ai_audio routes the literal
                        // text through the SYNC ElevenLabs seam (the modality-
                        // scoped ManagedAudioGenProvider + the GEN-08 gate + the
                        // landing bridge, all living ONLY here) and hands the agent
                        // back a TEXT-ONLY result (media id + the a1 placement
                        // steer) — the SAME Pattern-C interception, for real
                        // external text-to-speech.
                        "generate_ai_audio" => dispatch_generate_with_cap(
                            "generate_ai_audio",
                            id,
                            &mut generate_call_counts,
                            || {
                                handle_generate_ai_audio(
                                    ctx,
                                    id,
                                    input,
                                )
                            },
                        ),
                        // Phase 56 (GEN-11 / D-04): generate_ai_video_edit sends
                        // a timeline clip's own CURRENT trimmed frames through
                        // the Runway `/v1/video_to_video` seam (the modality-
                        // scoped video provider + the GEN-08 gate + 56-04's
                        // window check and range extraction + the landing
                        // bridge, all living ONLY here) and hands the agent back
                        // the landed media id + a decoded frame. Wrapped in the
                        // SAME cap as its three siblings — this is the dearest
                        // paid call in the product, so it least of all may retry
                        // itself unbounded.
                        "generate_ai_video_edit" => dispatch_generate_with_cap(
                            "generate_ai_video_edit",
                            id,
                            &mut generate_call_counts,
                            || {
                                handle_generate_ai_video_edit(
                                    ctx,
                                    id,
                                    input,
                                )
                            },
                        ),
                        other => unreachable!("unhandled intercepted meta tool: {other}"),
                    };
                    // PROMPT-03 gap fix (40-HUMAN-UAT.md item 3, T-p3q-01):
                    // capture a STRUCTURAL record of a successful generate_ai_*
                    // dispatch -- `is_error` is `Some(true)` for EVERY
                    // rejection path (dispatch_generate_with_cap's own cap
                    // denial, a GEN-08 gate rejection, a provider error, a
                    // missing-prompt validation failure) and `None` ONLY for a
                    // genuine dispatch that actually reached the provider, so
                    // this single check excludes every rejection alike.
                    if let agent_llm::ContentBlock::ToolResult { is_error, .. } = &result {
                        if !matches!(is_error, Some(true)) {
                            let modality = match name.as_str() {
                                "generate_ai_image" => Some("image"),
                                "generate_ai_video" => Some("video"),
                                "generate_ai_audio" => Some("audio"),
                                // Phase 56 (GEN-11): a DISTINCT modality rather
                                // than folding into "video", because this is the
                                // one whose standing provider notice has to say
                                // something the other two must not — that the
                                // user's OWN footage was uploaded. The C# Chat
                                // presenter interpolates this string and
                                // switches on nothing, so a new value renders
                                // without a shell change.
                                "generate_ai_video_edit" => Some("video edit"),
                                _ => None,
                            };
                            if let Some(modality) = modality {
                                if let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) {
                                    generation_disclosures.push(GenerationDisclosure {
                                        modality: modality.to_string(),
                                        prompt: prompt.to_string(),
                                        // Quick 260726-ufz: read from the SAME
                                        // `input` as `prompt`, one line up.
                                        frames: describe_conditioning_frames(name, input),
                                        // Phase 42.1 GEN-08 sign-off condition:
                                        // the standing provider notice for this
                                        // modality, on the structural channel so
                                        // it does not depend on narration.
                                        provider_notice: provider_notice_for_modality(modality),
                                        // Phase 42.1-03: which model the intent
                                        // actually resolved to. Re-run from the
                                        // SAME `input`, by the SAME resolver the
                                        // seam used, so the line cannot name one
                                        // model while another was billed.
                                        model_resolved:
                                            ctx.resolved_model_for_tool_input(name, input),
                                    });
                                }
                            }
                        }
                    }
                    round.tool_results.push(result);
                }
            }

            // HI-01 (+ ME-01): capture THIS round's last real (`is_error`)
            // tool_result into the session's `last_error` — scanned AFTER the meta
            // loop, so a failed `export_project`/`send_feedback` (the very failure
            // a follow-up `send_feedback` report exists to carry) is included, not
            // structurally excluded by running the scan too early. `last_error` was
            // cleared at fresh-turn start (ME-01), so it only ever reflects the
            // current turn. On a HALTED round the meta tools did NOT run, so this
            // sees only genuine pre-halt edit errors (never a "skipped" ack, which
            // is appended below, after this scan).
            {
                let mut sess = session
                    .lock()
                    .expect("agent session poisoned");
                for tr in &round.tool_results {
                    if let agent_llm::ContentBlock::ToolResult {
                        content,
                        is_error: Some(true),
                        ..
                    } = tr
                    {
                        if let Some(text) = content.iter().find_map(|b| match b {
                            agent_llm::ToolResultBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        }) {
                            sess.last_error = Some(text);
                        }
                    }
                }
            }

            // CR-01: on a HALTED round, still acknowledge each intercepted meta
            // tool_use with a NON-executing `is_error` tool_result — the assistant
            // message recorded in history carries the meta tool_use blocks, so the
            // API requires a matching tool_result for each. Appended AFTER the
            // `last_error` scan so a "skipped" ack never masquerades as a real
            // failure. These flow into the pending state's `prior_tool_results` and
            // are replayed when the user answers, so the model can retry the call.
            //
            // Debug session `agent-turn-dangling-tool-result-on-halt`: this loop
            // covers ONLY `meta`, which is exactly why ordinary edit tools emitted
            // alongside the control block used to dangle. `apply_response` now
            // acks those itself (agent_llm::HALT_SKIP_ACK, same wording). The two
            // are disjoint by construction — meta blocks are STRIPPED from the
            // response `apply_response` sees above — so no id is acked twice.
            //
            // Phase 42.3 (F): the slice start skips the meta tools that ALREADY
            // ran (and pushed real results) before a spend-confirmation gate halt
            // — acking those would answer them twice as well. `unacked_meta_from`
            // is 0 for every non-gate halt, so that case is byte-identical to
            // before.
            if halted {
                for (id, _name, _input) in &meta[unacked_meta_from..] {
                    // Phase 42.3 (F) — THE DOUBLE-ACK TRAP, debug session
                    // `agent-turn-dangling-tool-result-on-halt`. The HALTING
                    // tool_use id is EXCLUDED here on purpose.
                    //
                    // A real `askUser`/`proposeOptions` block is a CONTROL block:
                    // it is never in `meta`, so it structurally cannot reach this
                    // loop, and its tool_result is minted once at resume by
                    // `build_user_turn`. The gate-synthesized halt breaks that
                    // symmetry — it puts a REAL meta id (the `generate_ai_*`
                    // call) into `round.asked` — so without this skip the halting
                    // id would get a `HALT_SKIP_ACK` here AND a tool_result at
                    // resume. Two tool_results for one tool_use id is the SAME
                    // permanently-bricked-session class the 2026-07-26 fix closed,
                    // approached from the opposite direction: an HTTP 400 at a
                    // fixed `messages.N` on every later request, forever, because
                    // the assistant message is replayed verbatim.
                    if Some(id.as_str()) == gate_halt_id.as_deref() {
                        continue;
                    }
                    round.tool_results.push(agent_llm::ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: agent_llm::vision::text_tool_result(
                            agent_llm::HALT_SKIP_ACK,
                        ),
                        is_error: Some(true),
                    });
                }
            }

            if let Some(ask) = &round.asked {
                captured_ask = Some(PendingAskUser {
                    tool_use_id: ask.tool_use_id.clone(),
                    prior_tool_results: round.tool_results.clone(),
                    // Phase 42.3 (F): true ONLY when THIS halt is the one the
                    // spend gate synthesized above — a real model `askUser` in the
                    // same turn never grants a spend approval on resume.
                    spend_confirmation: gate_halt_id.as_deref()
                        == Some(ask.tool_use_id.as_str()),
                });
            }
            // CANV-02: a proposeOptions halt captures the offered cards as
            // server-held pending state (mutually exclusive with `asked` by
            // construction — first-control-block-wins, T-14-10).
            if let Some((tool_use_id, cards)) = &round.options {
                captured_options = Some(PendingOptionChoice {
                    tool_use_id: tool_use_id.clone(),
                    cards: cards.clone(),
                    resolved_summary: None,
                    // Same capture the askUser branch above does — without it
                    // every OTHER tool_use id in this round's assistant message
                    // goes unanswered forever.
                    prior_tool_results: round.tool_results.clone(),
                });
            }
            round
        },
    )
    .await; // <-- no lock held across this await (nor the snapshot one above)

    // Short sync span: ALWAYS close the undo group, even if run_turn errored
    // above (T-12-14 — an errored/dropped turn never leaves an open group).
    store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .end_turn();

    // Phase 26 (LIB-02, autosave hook c): persist the active project's real
    // post-turn state so an agent mutation is durable without waiting for the
    // next switch. Plan 45-13: the `app.try_state::<ActiveProjectMeta>()` this
    // used is now `ctx.active_project_meta()` -- the ONE managed value
    // `configure_with_key_store` registers for EVERY app, real or mock, so the
    // `Option` it used to guard against is uninhabited in this tree (grepped:
    // every app builder in `src-tauri` and `generation.rs` routes through it).
    // A no-active-project (`None` path — pre-Phase-26 in-memory-only behavior) is
    // a silent no-op, never an error that fails the turn. The write is atomic
    // (write_project_atomic = temp+rename), matching hooks (a)/(b) (T-26-06).
    {
        let meta = ctx.active_project_meta();
        if let Ok(Some(path)) = meta.0.lock().map(|g| g.clone()) {
            if let Ok(guard) = store.lock() {
                let snapshot = guard.snapshot();
                drop(guard);
                // WR-03: don't swallow a failed persist -- a disk-full/locked/
                // permission failure here silently loses the whole turn's edits.
                // Log it (matching the sibling library-growth failure path) but
                // keep control flow: end_turn already ran, the turn still stands.
                if let Err(e) = crate::project_store::write_project_atomic(&path, &snapshot) {
                    eprintln!("[T-26] end-of-turn autosave failed for {path:?}: {e}");
                }
            }
        }
    }

    // Persist the conversation + any pending clarifying-question bookkeeping.
    let mut sess = session
        .lock()
        .map_err(|_| "agent session poisoned".to_string())?;
    sess.history = history;
    sess.pending_ask_user = captured_ask;
    sess.pending_option_choice = captured_options;

    // Library growth (MOAT-03, DECISIONS.md A1): a COMPLETED turn — it did
    // NOT halt on askUser/proposeOptions — that dispatched >=1 real edit is
    // an accepted interaction, appended to the growable library IMMEDIATELY.
    // A failed append never fails the turn (the vision-snapshot
    // graceful-degradation precedent): log and skip the tracking.
    if let Ok(out) = &outcome {
        if out.clarifying_question.is_none()
            && out.options.is_none()
            && !dispatched_calls.is_empty()
        {
            let entry = agent_llm::LibraryExample {
                id: crate::next_id("ex"),
                request: request_text,
                tool_calls: std::mem::take(&mut dispatched_calls),
                canvas_summary: None,
            };
            match agent_llm::append_library_entry(&entry, library_dir) {
                Ok(path) => {
                    sess.pending_growth = Some(PendingGrowth {
                        path,
                        ids: touched_ids,
                    });
                }
                Err(e) => {
                    eprintln!("library-growth append failed (turn kept, growth skipped): {e}");
                }
            }
        }
    }
    drop(sess);

    // [E-01] total wall time for the whole turn + how many images it carried +
    // whether it was a resumed (image-free) turn — the top-level number D-10
    // reads against the per-round transport timings to attribute the >2min cost.
    eprintln!(
        "[E-01] agent turn total={:?} images={} resumed={}",
        turn_started.elapsed(),
        image_count,
        resumed
    );

    outcome
        .map(|turn| AgentTurnOutcome {
            narration: turn.narration,
            clarifying_question: turn.clarifying_question,
            options: turn.options,
            truncated: turn.truncated,
            generation_disclosures,
            export_disclosures,
        })
        .map_err(|e| e.to_string())
}

/// Phase 39 (CHECK-03): the hard cap on automatic, self-check-triggered
/// retries of a PAID `generate_ai_*` call within ONE turn. `2` admits the
/// original call PLUS exactly one automatic retry per tool NAME (not
/// combined across all three generate_ai_* tools) -- Assumption A2 of
/// 39-RESEARCH.md. Defense-in-depth alongside the rulebook's own "#
/// Self-check" retry-cap rule (Pitfall A1: a prompt-only rule is not a
/// spend guarantee).
const GENERATE_RETRY_CAP: u32 = 2;

/// Phase 39 (WR-01 review fix): literal substrings that appear ONLY in a
/// `generate_ai_*` failure message produced BEFORE `start_generation_job`
/// ever reaches a real provider call -- i.e. a caller-input validation
/// failure (empty/too-long prompt), missing/unmanaged Tauri-managed state, an
/// unconfigured provider (no key), or a GEN-08 clean-model allow-list gate
/// rejection. `submit_checked` (agent-gen's `allow_list.rs`) runs the GEN-08
/// gate as `start_generation_job`'s FIRST act -- "BEFORE the job-registry
/// insert, before any event emit, and before any file work" (its own doc
/// comment) -- so a gate rejection is provably pre-spend, exactly like the
/// earlier caller-input checks in `generate_runway_image_for_agent` /
/// `generate_runway_video_for_agent` / `generate_elevenlabs_audio_for_agent`
/// (`generation.rs`). Each entry is matched with `str::contains` against the
/// tool's `{tool_name} failed: {message}` text, so it is insensitive to which
/// of the three `generate_ai_*` tools produced it (image/video/audio share
/// near-identical wording for these specific rejections).
///
/// Phase 39 (WR-01b review fix): also covers the `run_generate_ai_*`
/// (`lib.rs`)-level checks that run even earlier than `generation.rs`'s own
/// checks -- the missing/non-string `prompt` field guard ("requires a
/// prompt string", `lib.rs:5785/5941/6084`) and `run_generate_ai_image`'s
/// synchronous pre-provider parse `parse_reference_source`
/// ("unrecognized referenceSource '" / "requires referenceMediaId",
/// `lib.rs:804-824`). All of these run before `run_generate_ai_*` ever calls
/// into the async provider-call block, so they are exactly as pre-spend as the
/// `generation.rs`-level checks above.
///
/// **Phase 55.1 (plan 03) REPLACED the whole Phase-42.3 (G) block with two
/// entries.** The six shape/stage/intent needles that lived here
/// (`"unrecognized shape '"`, `"unrecognized stage '"`,
/// `"pass shape OR stage, not both"`, `"was replaced by shape/stage"`,
/// `"cannot generate from "`, `"can serve a request with"`) each pinned a
/// refusal raised by the schema translation or the capability resolver — both
/// DELETED outright by plan 06. D-11 deleted both schema fields and the dispatch
/// path stopped calling either resolver, so **no code path can produce any of
/// those six strings any more** —
/// the same reasoning that removed `"unrecognized background '"` in Phase 42.3
/// (D). Keeping them would be decoration that reads like coverage.
///
/// The two that replace them, each produced ONLY by
/// [`crate::generation_bridge`]'s `model_from_input` (and by the seam's own
/// emptiness backstop) before any provider work:
///
/// * `"requires a model"` -- the REQUIRED `model` field is missing or blank.
/// * `"replaced by model"` -- a lingering `shape`/`stage`/`intent` key from a
///   model running on stale context. A loud refusal naming the field that
///   replaced them, never a silent ignore that would bill for a model the
///   caller did not choose.
///
/// Deliberately does NOT match anything downstream of a passed gate (a real
/// provider/network error, a poll timeout, a landing failure, an
/// async-when-sync-expected error, "no asset landed") -- those all occur
/// AFTER `submit_checked` accepted the request, meaning a provider call was
/// actually attempted (spend occurred or was in flight), so they must still
/// count against the retry cap.
pub const PRE_SPEND_VALIDATION_SUBSTRINGS: &[&str] = &[
    "requires a non-empty prompt",
    "requires a prompt string",
    "prompt is too long (",
    "provider state is not managed",
    "jobs state is not managed",
    "allow-list state is not managed",
    "generation provider configured",
    "is not on the clean-model allow-list (GEN-08)",
    "unrecognized referenceSource '",
    "requires referenceMediaId",
    // Phase 55.1 (plan 03): the free-text `model` field's two rejections, which
    // replaced Phase 42.3 (G)'s six shape/stage/intent ones.
    "requires a model",
    "replaced by model",
    // -----------------------------------------------------------------------
    // Phase 56 plan 07 (GEN-11 / D-04): the clip-edit path's PRE-spend
    // refusals. Every one of them is raised at $0.00 before a byte reaches
    // Runway, and every one of them is a refusal the agent can act on — a
    // clip that is too long can be split, a reference set can be dropped, a
    // clip id can be corrected. Letting them consume one of the two paid
    // retry slots would spend the turn's budget on refusals that cost
    // nothing, which is the exact WR-01 defect these needles exist to
    // prevent. `video_edit_pre_spend_refusals_never_burn_the_cap` asserts
    // each one against the SHIPPED message rather than a re-typed copy.
    // -----------------------------------------------------------------------
    // 56-04's window check, BOTH edges (over the ceiling and under the floor).
    // Pure, local, and raised before any decode or extraction.
    "clip-edit model",
    // 56-06's host: the clip id names nothing on the timeline, or its media
    // is gone. Raised under the first store lock, before the extraction.
    "on the timeline to edit",
    "which is not in the media bin",
    // The ONE reference gate's two arms (`video_edit_reference_check`), and
    // the bridge's own clipId / per-item parse refusals.
    "a clip edit accepts at most",
    "reference images cannot be sent with a clip edit",
    "requires a clipId",
    "references[",
    "references must be an array",
    // -----------------------------------------------------------------------
    // Debug session `v2v-413-on-4k-source-unprobed-upload-window` (2026-08-13):
    // the TRANSPORT refusals. Both are raised by `agent_gen`'s
    // `video_transport_for_len` / `check_video_data_uri_size`, and both are
    // provably PRE-EGRESS despite living inside `RunwayProvider::submit`:
    // `resolve_video_input` runs the router as `submit`'s first act, before any
    // `.send()`, and `build_video_edit_submission` runs the per-asset check
    // before the POST is built. A clip too big for either transport therefore
    // costs $0.00 and nothing is in flight — exactly the WR-01 shape these
    // needles exist for. Pinned against the shipped strings on the agent-gen
    // side by `the_transport_refusals_carry_the_pre_spend_needles`, because a
    // grep across crate boundaries is not something this crate does (see
    // `the_quoted_clip_edit_refusal_fragments_are_still_the_shipped_ones`).
    // -----------------------------------------------------------------------
    "maximum Runway's upload endpoint accepts",
    "belongs on the upload transport",
    "fits neither transport",
];

/// Phase 39 (WR-01 review fix): true only for an `is_error` tool_result whose
/// text matches a known pre-spend rejection (see
/// [`PRE_SPEND_VALIDATION_SUBSTRINGS`]). A successful dispatch is never
/// pre-spend by construction and returns `false` -- `dispatch_generate_with_cap`
/// counts every outcome EXCEPT a pre-spend rejection, so a genuine provider
/// failure (which did reach/attempt the provider) still consumes a retry slot.
pub fn is_pre_spend_validation_failure(result: &agent_llm::ContentBlock) -> bool {
    let agent_llm::ContentBlock::ToolResult {
        content,
        is_error: Some(true),
        ..
    } = result
    else {
        return false;
    };
    content.iter().any(|block| match block {
        agent_llm::ToolResultBlock::Text { text } => PRE_SPEND_VALIDATION_SUBSTRINGS
            .iter()
            .any(|needle| text.contains(needle)),
        agent_llm::ToolResultBlock::Image { .. } => false,
    })
}

/// Shared cap-and-dispatch wrapper for the three `generate_ai_*` meta tools.
/// Calls `call()` (the REAL handler, which attempts a provider spend) while
/// under the cap, then increments `counts[tool_name]` UNLESS the outcome was a
/// pre-spend validation/config/gate rejection (WR-01 review fix -- see
/// [`is_pre_spend_validation_failure`]): counting those identically to a
/// genuine dispatch could exhaust the 2-call budget on validation noise alone,
/// before any real self-check retry ever happened. Once `tool_name` has been
/// counted `GENERATE_RETRY_CAP` times THIS turn, `call()` is NEVER invoked
/// again -- no provider call, no file write, no media-bin entry -- and an
/// `is_error` tool_result steers the model toward `askUser` instead. `counts`
/// is a per-turn local (`generate_call_counts` in `run_agent_turn`), so the
/// cap always resets on the next turn.
fn dispatch_generate_with_cap(
    tool_name: &'static str,
    tool_use_id: &str,
    counts: &mut std::collections::HashMap<&'static str, u32>,
    call: impl FnOnce() -> agent_llm::ContentBlock,
) -> agent_llm::ContentBlock {
    let count = counts.entry(tool_name).or_insert(0);
    if *count >= GENERATE_RETRY_CAP {
        agent_llm::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: agent_llm::vision::text_tool_result(format!(
                "{tool_name} has reached its {GENERATE_RETRY_CAP}-calls-per-turn generation \
                 cap (this turn already made {GENERATE_RETRY_CAP} calls to it, whether those \
                 were self-check retries or separate requests). This is a PAID generation \
                 call — do not call it again silently. Call askUser to get the user's explicit \
                 confirmation before trying again."
            )),
            is_error: Some(true),
        }
    } else {
        let result = call();
        if !is_pre_spend_validation_failure(&result) {
            *count += 1;
        }
        result
    }
}

/// Phase 17-04: the META tools intercepted at the src-tauri layer inside
/// `run_agent_turn`'s `apply_round` closure (research Pattern C / Pitfall 4:
/// they need `app_data_dir` and/or the FFmpeg engine, which live ONLY here and
/// must never enter `agent-llm`). Their schemas are authored ONCE in
/// `agent-tools` (D-09); this is the in-app handling half. Kept as a single
/// predicate so the closure's pre-scan and its meta-block filter can never drift
/// apart.
fn is_intercepted_meta_tool(name: &str) -> bool {
    name == "send_feedback"
        || name == "export_project"
        // Phase 21 (EYES-01/02): the agent-eyes read tools, routed through the
        // SAME Pattern-C interception (they need the engine, which lives here).
        || name == "inspect_timeline" || name == "inspect_media"
        // Phase 22 (TEXT-02/EYES-03): the engine-needing transcript READ tools,
        // routed through the SAME Pattern-C interception (they call
        // engine::whisper, which lives here and must never enter agent-llm).
        || name == "get_transcript" || name == "search_media"
        // Phase 24 (ASSET-01): generate_image needs app_data_dir + the FFmpeg
        // engine (write_frame_png/probe/generate_poster), which live ONLY here —
        // so it is a Pattern-C interception, NOT a core Tool. Phase 24 (ASSET-02):
        // generate_video is the SAME interception, additionally driving
        // engine::VideoEncoder (the license-safe export encoder).
        || name == "generate_image"
        || name == "generate_video"
        // Phase 26 (LIB-02): new_project/open_project — the project-switch
        // mechanic. Both need app_data_dir + ActiveProjectMeta (which live ONLY
        // here) and drive Store::from_project — so they are Pattern-C
        // interceptions, NOT core Command/Tool variants (no undo entry).
        // open_project resolves REGISTRY-ONLY (never a raw caller path).
        || name == "new_project"
        || name == "open_project"
        // Phase 26 (TOOL-04): get_projects — the registry READ. Needs
        // app_data_dir + ActiveProjectMeta (both live ONLY here), so it is the
        // SAME Pattern-C interception. Read-only, no mutation, no undo entry.
        || name == "get_projects"
        // Phase 27 (LIB-03): import_media needs engine::probe + app_cache_dir
        // (poster extraction), which live ONLY here — the SAME Pattern-C
        // interception generate_image/generate_video already use.
        || name == "import_media"
        // Phase 27 (LIB-03): create_matte drives render_scene_frame +
        // engine::VideoEncoder (both live ONLY here) — the SAME Pattern-C
        // interception generate_video uses, producing a real placeable Video asset.
        || name == "create_matte"
        // Phase 27 (LIB-04): sync_audio renders each clip's CURRENT audio via
        // engine::render_audio_pcm and correlates via engine::best_lag_us (both
        // live ONLY here) — the SAME Pattern-C interception, dispatching a real
        // Command::MoveClip on the target only above the confidence floor.
        || name == "sync_audio"
        // Phase 29 (OVL-03): get_overlay_library scans the bundled resource dir
        // (app.path().resolve(.., BaseDirectory::Resource)) + app_data_dir()/
        // overlay-library — both Tauri-only concepts that live ONLY here, so it
        // is the SAME Pattern-C read interception get_projects uses. Read-only,
        // no mutation, no undo entry; assets resolve by opaque server-built id.
        || name == "get_overlay_library"
        // Phase 29 (OVL-02): place_overlay imports-if-needed (engine::probe +
        // resource_dir/app_data_dir, all Tauri/engine-only and living ONLY here)
        // then dispatches existing AddMediaBinItem/CreateMediaFolder/AddClip
        // Commands under the ALREADY-open agent turn — the SAME Pattern-C
        // interception create_matte/import_media use. Composes existing Commands
        // only (ZERO new Command variants); one-turn-one-undo is FREE from the
        // whole-turn begin_turn/end_turn bracket.
        || name == "place_overlay"
        // Phase 29 (OVL-03): export_overlay_asset renders ONE overlay clip/media
        // through the transparent-clear compositor + license-clean png/prores_ks
        // encoders (engine, live ONLY here) and re-imports the produced asset —
        // the SAME Pattern-C interception create_matte uses. Output path is
        // SERVER-BUILT under app_data_dir()/overlay-exports (no agent path arg);
        // NEVER touches the frozen H.264/MF export encoder (SC-4).
        || name == "export_overlay_asset"
        // Phase 30 (TRK-01/TRK-02): track_object decodes a source clip
        // (decode_clip_frame) + runs engine::tracking::track_region (the bundled
        // opencv CSRT/KCF sidecar) — both live ONLY here — then composes the
        // EXISTING Command::SetKeyframes(Position) on a target overlay clip under
        // the ALREADY-open agent turn. The SAME Pattern-C interception
        // create_matte/export_overlay_asset use. ZERO new Command variants; one
        // turn = one undo. NEVER touches the frozen H.264/MF export encoder.
        || name == "track_object"
        // Phase 32 (GEN-01): generate_ai_image needs the generation seam's
        // managed state (ManagedGenProvider/ManagedGenJobs/ManagedAllowList) +
        // app_data_dir, all of which live ONLY here — so it is a Pattern-C
        // interception, NOT a core Tool. It is the ONLY network-touching tool
        // (the allow-list-gated, GEN-08-signed-off OpenAI endpoint), routed
        // through the ONE governed submit path; one-turn-one-undo is FREE from
        // the whole-turn begin_turn/end_turn bracket + the landing bridge's
        // auto-joining nested turn.
        || name == "generate_ai_image"
        // Phase 33 (GEN-02): generate_ai_video needs the generation seam's
        // managed state (the modality-scoped ManagedVideoGenProvider +
        // ManagedGenJobs/ManagedAllowList) + app_data_dir, all of which live
        // ONLY here — so it is a Pattern-C interception, NOT a core Tool. Its
        // egress is the allow-list-gated, GEN-08-signed-off Google Veo endpoint,
        // routed through the ONE governed submit path; one-turn-one-undo is FREE
        // from the whole-turn begin_turn/end_turn bracket + the landing bridge's
        // auto-joining nested turn.
        || name == "generate_ai_video"
        // Phase 34 (GEN-03): generate_ai_audio needs the generation seam's managed
        // state (the modality-scoped ManagedAudioGenProvider + ManagedGenJobs/
        // ManagedAllowList) + app_data_dir, all of which live ONLY here — so it is
        // a Pattern-C interception, NOT a core Tool. Its egress WOULD be the
        // ElevenLabs endpoint, routed through the ONE governed submit path — but
        // fail-closed this wave (no allow-list row until the 34-03 sign-off).
        // one-turn-one-undo is FREE from the whole-turn begin_turn/end_turn bracket
        // + the landing bridge's auto-joining nested turn.
        || name == "generate_ai_audio"
        // Phase 56 (GEN-11 / D-04): generate_ai_video_edit needs the generation
        // seam's managed state AND three more things that live only here — the
        // backend Store (to resolve the clip id to a media path and its CURRENT
        // trim), 56-04's window check, and the ffmpeg range extraction. It is
        // therefore the SAME Pattern-C interception, NOT a core Tool: it lands a
        // NEW MediaBin asset (D-06) rather than mutating the timeline, so ZERO
        // new Command variants and one-turn-one-undo is free from the existing
        // begin_turn/end_turn bracket. Its egress is Runway's
        // `/v1/video_to_video`, cleared by the Phase 56 GEN-08 sign-off and
        // routed through the ONE governed submit path.
        || name == "generate_ai_video_edit"
}

/// The command-level helper BOTH the real `apply_option_card` command and the
/// deterministic `mod option_card_gate` wiring test drive (the exact
/// `run_agent_turn` genericity pattern): look the chosen card up by id in the
/// SERVER-HELD pending state — never trusting a frontend `{tool,args}` pair
/// (T-14-13) — apply it against the live backend Store as ONE undo step via
/// `agent_llm::cards_apply_option_card` (the shared `dispatch_edit` path every
/// edit tool_use already uses), emit `project:changed` per patch, and record
/// the resolution summary for the NEXT turn's tool_result.
///
/// Lock order is session → store, matching `run_agent_turn`'s only sequential
/// use of the two (neither path ever locks store → session, so no deadlock).
pub fn apply_option_card_inner<C: AppCtx>(
    ctx: &C,
    session: &Mutex<AgentSession>,
    card_id: &str,
    // Phase 43 (LAT-02): each patch keeps the (base_seq, seq) pair captured
    // atomically with its own dispatch, so a multi-command card emits a
    // correctly chained run of envelopes.
) -> Result<Vec<(Patch, u64, u64)>, String> {
    let mut sess = session
        .lock()
        .map_err(|_| "agent session poisoned".to_string())?;
    let choice = sess
        .pending_option_choice
        .as_mut()
        .ok_or_else(|| "no option cards are pending".to_string())?; // T-14-14
    let card = choice
        .cards
        .iter()
        .find(|c| c.id == card_id)
        .ok_or_else(|| format!("unknown option card id: {card_id}"))? // T-14-13
        .clone();

    // Short sync span on the store: the whole apply is one begin/end_turn
    // bracket inside cards_apply_option_card; a failed dispatch mutates
    // nothing and pushes no undo entry (proven in agent-llm's own tests).
    let mut guard = ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?;
    let (summary, patches) = agent_llm::cards_apply_option_card(&mut guard, &card)?;
    drop(guard);

    for (p, base_seq, seq) in &patches {
        ctx.emit_patch(p, *base_seq, *seq)?;
    }
    choice.resolved_summary = Some(format!(
        "User applied option '{}' ({}): {summary}",
        card.label, card.id
    ));
    Ok(patches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Quick 260726-ufz: the conditioning-frame disclosure. The absence case is
    /// the one that matters — a text-only generation must SAY it is text-only,
    /// because a generation that ignored the frames was previously
    /// indistinguishable from one that used them (the whole reason this exists).
    ///
    /// Migrated from `src-tauri`'s `mod agent_gate` by plan 45-13, verbatim: it
    /// is pure `serde_json` in / `Option<String>` out, needs no app of any kind,
    /// and travelling with `describe_conditioning_frames` is exactly what keeps
    /// that function PRIVATE here (45-11's precedent with the three closed-enum
    /// reference parsers).
    #[test]
    fn conditioning_frame_disclosure_states_presence_and_absence() {
        // Both frames: a real A→B transition, with ids so the line is checkable
        // against the timeline.
        assert_eq!(
            describe_conditioning_frames(
                "generate_ai_video",
                &json!({
                    "prompt": "drone rises",
                    "referenceSource": "clipEnd",
                    "referenceClipId": "clip-1",
                    "destinationSource": "clipStart",
                    "destinationClipId": "clip-7"
                })
            )
            .as_deref(),
            Some("first frame: clipEnd(clip-1) | last frame: clipStart(clip-7)")
        );
        // NEITHER frame — must be loud, not empty.
        assert_eq!(
            describe_conditioning_frames("generate_ai_video", &json!({ "prompt": "a cat" }))
                .as_deref(),
            Some("text only (no reference frames)")
        );
        // First only — names the missing half explicitly, so a half-wired
        // transition reads as wrong at a glance.
        assert_eq!(
            describe_conditioning_frames(
                "generate_ai_video",
                &json!({
                    "prompt": "x",
                    "referenceSource": "clipEnd",
                    "referenceClipId": "clip-1"
                })
            )
            .as_deref(),
            Some("first frame: clipEnd(clip-1) | last frame: none")
        );
        // A source that carries no id (sketch/frame) renders without parentheses.
        assert_eq!(
            describe_conditioning_frames(
                "generate_ai_image",
                &json!({ "prompt": "x", "referenceSource": "sketch" })
            )
            .as_deref(),
            Some("first frame: sketch | last frame: none")
        );
        // Audio has no frame concept — `None`, so the UI renders nothing rather
        // than a misleading "text only".
        assert_eq!(
            describe_conditioning_frames("generate_ai_audio", &json!({ "prompt": "hello" })),
            None
        );
    }

    /// Phase 55.1 (plan 03), WR-02 discipline: the TWO refusals the required
    /// `model` field introduces are `$0.00` local rejections raised before any
    /// provider work, so neither may consume one of the two paid retry slots
    /// under [`GENERATE_RETRY_CAP`].
    ///
    /// The needles are asserted against the LITERAL text
    /// `crate::generation_bridge` produces, not against the const's own entries,
    /// so a reworded refusal that stopped matching would fail here rather than
    /// silently start burning paid slots.
    #[test]
    fn the_model_field_refusals_are_recognized_as_zero_dollar_pre_spend() {
        let as_error = |text: &str| agent_llm::ContentBlock::ToolResult {
            tool_use_id: "tu-1".to_string(),
            content: agent_llm::vision::text_tool_result(format!(
                "generate_ai_video failed: {text}"
            )),
            is_error: Some(true),
        };

        for text in [
            "generate_ai_video requires a model -- name the Runway model id to run \
             (pick from the rulebook's cost table, or the model the user asked for)",
            "generate_ai_image requires a model -- name the Runway model id to run \
             (pick from the rulebook's cost table, or the model the user asked for)",
            "the shape/stage fields were replaced by model -- name the Runway model id \
             directly in the model field (the rulebook carries the cost table)",
        ] {
            assert!(
                is_pre_spend_validation_failure(&as_error(text)),
                "a $0.00 model-field refusal must not consume a paid retry slot: {text}"
            );
        }

        // The retired shape/stage vocabulary is NOT re-listed: nothing on the
        // dispatch path can produce it any more (plan 03 replaced the one caller
        // of the schema translation and of the capability resolver; plan 06
        // deleted both functions), so an entry for it would be decorative.
        for retired in [
            "unrecognized shape '",
            "unrecognized stage '",
            "pass shape OR stage, not both",
            "was replaced by shape/stage",
            "cannot generate from ",
            "can serve a request with",
        ] {
            assert!(
                !PRE_SPEND_VALIDATION_SUBSTRINGS.contains(&retired),
                "`{retired}` pins a refusal no code path can raise any more"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Phase 56 plan 07 (GEN-11 / D-04): the clip-edit dispatch arm
    // -----------------------------------------------------------------------

    /// The clip-edit tool's `is_error` result, shaped exactly as the dispatch
    /// produces it (`"{tool} failed: {message}"`).
    fn edit_error(text: &str) -> agent_llm::ContentBlock {
        agent_llm::ContentBlock::ToolResult {
            tool_use_id: "tu-edit".to_string(),
            content: agent_llm::vision::text_tool_result(format!(
                "generate_ai_video_edit failed: {text}"
            )),
            is_error: Some(true),
        }
    }

    /// **Every clip-edit refusal that costs $0.00 must stay free.**
    ///
    /// The clip edit refuses more often than its siblings — an out-of-window
    /// range, an unknown clip id, a reference set the endpoint cannot carry —
    /// and every one of those is a refusal the agent can act on (split the clip,
    /// fix the id, drop the references). Letting them consume one of the two
    /// paid retry slots would exhaust the turn's budget on refusals that spent
    /// nothing, which is precisely the WR-01 defect the classification exists to
    /// prevent.
    ///
    /// **Every needle is the SHIPPED message**, obtained by CALLING the function
    /// that raises it rather than by re-typing it, so a rewording that stopped
    /// matching reddens here instead of silently starting to burn paid slots.
    #[test]
    fn video_edit_pre_spend_refusals_never_burn_the_cap() {
        let min_us = i64::from(agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS) * 1_000_000;
        let max_us = i64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_SECONDS) * 1_000_000;

        // 56-04's window check, BOTH edges — asked for by calling it.
        let over = crate::clip_edit_window_check(max_us + 1).expect_err("over the ceiling");
        let under = crate::clip_edit_window_check(min_us - 1).expect_err("under the floor");
        // 56-06's ONE reference gate, BOTH arms.
        let cap = crate::video_edit_reference_check(agent_gen::RUNWAY_V2V_MAX_REFERENCES + 1)
            .expect_err("over the reference cap");
        let blocked = crate::video_edit_reference_check(1).expect_err("any non-empty set");

        for shipped in [&over, &under, &cap, &blocked] {
            assert!(
                is_pre_spend_validation_failure(&edit_error(shipped)),
                "a $0.00 clip-edit refusal must not consume a paid retry slot: {shipped}"
            );
        }

        // The host's and the bridge's own refusals. These are raised inside
        // functions that need a store or a real tool input, so their
        // DISTINCTIVE fragments are quoted here and pinned by
        // `the_quoted_clip_edit_refusal_fragments_are_still_the_shipped_ones`
        // below, which greps the sources that raise them.
        for fragment in [
            "no clip with id 'c-gone' on the timeline to edit",
            "clip 'c1' references media 'm9', which is not in the media bin",
            "generate_ai_video_edit requires a clipId string naming the timeline clip to edit",
            "unrecognized reference source 'path' at references[0]",
            "references[2] source \"media\" requires mediaId",
            "references must be an array of {source, mediaId?, clipId?} objects",
        ] {
            assert!(
                is_pre_spend_validation_failure(&edit_error(fragment)),
                "a $0.00 clip-edit refusal must not consume a paid retry slot: {fragment}"
            );
        }

        // NON-VACUITY, and it is the half that matters: everything DOWNSTREAM of
        // an accepted request still burns a slot, because a provider call was
        // attempted and money may already have moved.
        for post_spend in [
            "provider returned 500 Internal Server Error",
            "generation timed out waiting for the provider",
            "generation reported success but landed no asset id",
        ] {
            assert!(
                !is_pre_spend_validation_failure(&edit_error(post_spend)),
                "a failure AFTER the provider was reached must still cost a retry slot: \
                 {post_spend}"
            );
        }
    }

    /// The fragments quoted above are the ones the code actually raises — greped
    /// from the two files that raise them, so the test above cannot drift into
    /// asserting about strings nothing produces (the failure mode 55.1-03's own
    /// "retired vocabulary" check was written to catch).
    #[test]
    fn the_quoted_clip_edit_refusal_fragments_are_still_the_shipped_ones() {
        let bridge = include_str!("generation_bridge.rs");
        for needle in [
            "requires a clipId string naming the timeline clip to edit",
            "unrecognized reference source '",
            "references must be an array of {source, mediaId?, clipId?} objects",
        ] {
            assert!(
                bridge.contains(needle),
                "`{needle}` is no longer raised by generation_bridge.rs — update the \
                 PRE_SPEND_VALIDATION_SUBSTRINGS entry that pins it"
            );
        }
        // The host-side pair lives one crate over (`crates/ffi/src/ctx.rs`), so
        // it is pinned by the entries themselves rather than by a grep this
        // crate cannot do.
        for entry in ["on the timeline to edit", "which is not in the media bin"] {
            assert!(
                PRE_SPEND_VALIDATION_SUBSTRINGS.contains(&entry),
                "`{entry}` must stay listed — it is the FFI host's clip-lookup refusal"
            );
        }
    }

    /// CHECK-03, extended to the dearest paid call in the product. The cap is
    /// PER TOOL NAME, so a turn that has already made two clip edits cannot make
    /// a third even if it has never called `generate_ai_video`.
    ///
    /// The claim is not "the third call errors" — it is that the third call
    /// never RUNS. Proven by a counter the closure owns: it reads 2 after three
    /// dispatches, so the handler (and therefore the provider) was never entered
    /// the third time.
    #[test]
    fn video_edit_is_capped_at_two_calls_per_turn() {
        let mut counts: std::collections::HashMap<&'static str, u32> =
            std::collections::HashMap::new();
        let entered = std::cell::Cell::new(0u32);

        let ok_result = |id: &str| agent_llm::ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: agent_llm::vision::text_tool_result("edited".to_string()),
            is_error: None,
        };

        for _ in 0..GENERATE_RETRY_CAP {
            let r = dispatch_generate_with_cap("generate_ai_video_edit", "tu", &mut counts, || {
                entered.set(entered.get() + 1);
                ok_result("tu")
            });
            assert!(
                matches!(
                    r,
                    agent_llm::ContentBlock::ToolResult { is_error: None, .. }
                ),
                "calls under the cap run normally"
            );
        }

        let third = dispatch_generate_with_cap("generate_ai_video_edit", "tu", &mut counts, || {
            entered.set(entered.get() + 1);
            ok_result("tu")
        });
        assert_eq!(
            entered.get(),
            GENERATE_RETRY_CAP,
            "the third dispatch must NEVER enter the handler — no provider call, no spend"
        );
        let agent_llm::ContentBlock::ToolResult {
            content,
            is_error: Some(true),
            ..
        } = third
        else {
            panic!("the capped call must be an is_error result steering toward askUser");
        };
        let text = content
            .iter()
            .find_map(|b| match b {
                agent_llm::ToolResultBlock::Text { text } => Some(text.clone()),
                agent_llm::ToolResultBlock::Image { .. } => None,
            })
            .expect("a text block");
        assert!(text.contains("generate_ai_video_edit"), "{text}");
        assert!(text.contains("cap"), "{text}");
        assert!(text.contains("askUser"), "{text}");

        // The cap is per NAME: a sibling tool still has its full budget.
        let sibling = dispatch_generate_with_cap("generate_ai_video", "tu2", &mut counts, || {
            entered.set(entered.get() + 1);
            ok_result("tu2")
        });
        assert!(
            matches!(
                sibling,
                agent_llm::ContentBlock::ToolResult { is_error: None, .. }
            ),
            "the cap counts per tool name, not across all paid tools"
        );

        // ...and a PRE-SPEND refusal does not increment it, which is what keeps
        // an honest refusal free (the composition
        // `video_edit_pre_spend_refusals_never_burn_the_cap` proves one half of).
        let mut free_counts: std::collections::HashMap<&'static str, u32> =
            std::collections::HashMap::new();
        let refusal = crate::video_edit_reference_check(1).expect_err("refused today");
        for _ in 0..5 {
            let _ = dispatch_generate_with_cap(
                "generate_ai_video_edit",
                "tu",
                &mut free_counts,
                || edit_error(&refusal),
            );
        }
        assert_eq!(
            free_counts.get("generate_ai_video_edit").copied(),
            Some(0),
            "five $0.00 refusals must leave the paid budget untouched"
        );
    }

    /// T-56-FOOTAGE: the STRUCTURAL disclosure names WHICH of the user's clips
    /// was uploaded, and the standing provider notice says outright that their
    /// own footage went to Runway.
    ///
    /// Both ride the narration-independent channel for the reason T-p3q-01
    /// established: nothing forces Claude to relay tool text verbatim, and live
    /// UAT proved it paraphrases. The tool result says these things too, but a
    /// legal disclosure conditional on the model choosing to repeat it is not a
    /// disclosure — and this is the path on which the material being disclosed
    /// is the user's own recorded footage.
    #[test]
    fn the_clip_edit_disclosure_names_the_uploaded_clip_and_the_training_license() {
        assert_eq!(
            describe_conditioning_frames(
                "generate_ai_video_edit",
                &json!({ "prompt": "relight", "clipId": "clip-42" })
            )
            .as_deref(),
            Some("source clip clip-42 (its current trimmed range -- your own footage, uploaded)")
        );

        let notice = provider_notice_for_modality("video edit")
            .expect("the clip edit carries Runway's standing notice");
        for needle in [
            // the shared Runway terms, unchanged in substance...
            "train and improve its models",
            "Enterprise plan",
            // ...plus the sentence only this modality can say in the indicative.
            "uploaded your own source clip",
        ] {
            assert!(notice.contains(needle), "{needle} missing from: {notice}");
        }

        // The two older modalities are byte-unchanged apart from the extended
        // enumeration they share, and they do NOT claim a source clip was sent.
        for older in ["image", "video"] {
            let n = provider_notice_for_modality(older).expect("Runway modality");
            assert!(
                !n.contains("uploaded your own source clip"),
                "{older} must not claim to have uploaded footage: {n}"
            );
            assert!(
                n.contains("train and improve its models"),
                "{older} keeps the training-license notice: {n}"
            );
        }
        assert_eq!(
            provider_notice_for_modality("audio"),
            None,
            "ElevenLabs is a different provider under a different sign-off"
        );
    }

    /// D-04/D-18: the clip-edit tool is an INTERCEPTED meta tool (it needs the
    /// store, the sidecar and the generation seam, all of which live only here),
    /// so it must never be routed to `parse_edit_tool`/`Store::dispatch` — and
    /// the pre-scan and the meta-block filter read this one predicate, so they
    /// cannot drift apart.
    #[test]
    fn the_clip_edit_tool_is_an_intercepted_meta_tool() {
        assert!(is_intercepted_meta_tool("generate_ai_video_edit"));
        // ...and the three siblings still are, so this was an addition.
        for sibling in [
            "generate_ai_image",
            "generate_ai_video",
            "generate_ai_audio",
        ] {
            assert!(is_intercepted_meta_tool(sibling));
        }
    }
}
