//! The three EXTERNAL-provider generation modality handlers —
//! `generate_ai_image` (Phase 32, GEN-01), `generate_ai_video` (Phase 33,
//! GEN-02) and `generate_ai_audio` (Phase 34, GEN-03) — relocated here from
//! `src-tauri/src/lib.rs` by plan 45-11.
//!
//! Distinct from [`crate::export`]'s `run_generate_image`/`run_generate_video`,
//! which 45-10 moved: those are the Claude-authored, ENGINE-composited still and
//! clip generators (they call `render_scene_frame` and build a
//! `VideoEncoder`). These three call an external paid provider through the ONE
//! governed submit path and land whatever comes back.
//!
//! # What moved
//!
//! | Item | Visibility here | Why |
//! |---|---|---|
//! | `run_generate_ai_image` / `handle_generate_ai_image` | `pub` | dispatch arm + `generation.rs`'s `agent_tool` gates |
//! | `run_generate_ai_video` / `handle_generate_ai_video` | `pub` | same |
//! | `run_generate_ai_audio` / `handle_generate_ai_audio` | `pub` | same |
//! | `parse_reference_source` / `parse_destination_source` / `parse_frame_source` | private | their two tests moved with them, so nothing outside this module names them |
//! | `RUNWAY_TRAINING_LICENSE_DISCLOSURE` | private | read only by the three handlers above it |
//!
//! Phase 56 plan 07 (GEN-11 / D-04) adds a FOURTH modality handler here —
//! `run_generate_ai_video_edit` / `handle_generate_ai_video_edit` (`pub`, the
//! dispatch arm) — plus its private `optional_model_from_input`,
//! `parse_reference_item`, `parse_reference_list` and
//! `RUNWAY_SOURCE_CLIP_TRAINING_DISCLOSURE`. It is the same Pattern-C shape as
//! the other three and the first one on which the user's OWN recorded footage
//! leaves the machine, which is why the training-license constant above grew a
//! third enumerated input.
//!
//! [`ClipEdge`] and [`ReferenceSource`] moved too, but to
//! [`crate::gen_submission`] rather than here: they are the vocabulary of
//! [`GenSubmission::resolve_reference_image`], and `src-tauri` still names both
//! (its `resolve_reference_image`, `resolve_clip_edge_reference_png` and five
//! `agent_gate` tests), so they are `pub` and re-exported at the old location.
//!
//! # The coupling, and how the bridge resolves it
//!
//! Everything these three functions reach that could NOT come with them is a
//! [`GenSubmission`] method, implemented in `src-tauri` — see that module's doc
//! for the full table and for why `generation.rs` needed ZERO visibility
//! widening (threat T-45-07). In this module the conversion is visible as five
//! call shapes:
//!
//! | Before (in `src-tauri`) | After |
//! |---|---|
//! | `generation::generate_runway_image_for_agent(app.clone(), ..)` | `ctx.submit_image(..)` |
//! | `generation::generate_runway_video_for_agent(app.clone(), ..)` | `ctx.submit_video(..)` |
//! | `generation::generate_elevenlabs_audio_for_agent(app.clone(), ..)` | `ctx.submit_audio(..)` |
//! | `resolve_reference_image(app, store, src).await` | `ctx.resolve_reference_image(src).await` |
//! | `parse_video_shape_stage(input)` | *(RETIRED by Phase 55.1 — the video handler reads the caller's `model` field instead; the trait method survives with no caller until plan 06 deletes it)* |
//!
//! plus the two mechanical ones every batch since 45-06 has done:
//! `<R: tauri::Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` becomes
//! `<C: AppCtx + GenSubmission>(ctx: &C, ..)` with `let store = ctx.store();`,
//! and `tauri::async_runtime::block_on` becomes
//! [`AppCtx::block_on`](crate::AppCtx::block_on) — the host primitive 45-07 put
//! on the trait precisely so a moved function could keep entering Tauri's own
//! process-global runtime rather than a second one.
//!
//! **The `block_in_place` guard is UNTOUCHED.** All three functions keep the
//! `tokio::runtime::Handle::try_current()` / `RuntimeFlavor::MultiThread` probe
//! and both branches verbatim: `block_in_place` panics off a multi-thread
//! runtime, and the tokio-less `MockRuntime` tests in `generation.rs` take the
//! inline branch. `tokio::task::block_in_place` is called by its own path here,
//! exactly as it already is in [`crate::export`] — it is the SAME crate and the
//! SAME function, not 45-07's `spawn_blocking` substitution (D-45-07-01), which
//! this batch does not make anywhere.
//!
//! # Ordering is load-bearing and is preserved exactly
//!
//! Every parse happens BEFORE any resolution and before any provider work:
//! `parse_reference_source`, then `parse_destination_source`, then the
//! shape/stage translation, and only then — inside the async block — the
//! reference/destination rasters and the submit. A typo in any of those fields
//! is therefore still a clean `Err` before a single frame is decoded, let alone
//! billed (T-34.1-01 / T-33-19). Folding parse-and-resolve into one bridge call
//! would have been simpler and would have broken exactly that property, so the
//! trait carries the parsed [`ReferenceSource`] rather than the raw JSON.
//!
//! # Zero logic changes
//!
//! Nothing else differs: not the hardcoded `BackgroundMode::Transparent` (Phase
//! 42.3 (D)), not the media-bin lookup under a SHORT store lock, not the
//! preview-decode downgrade-to-`None` policy, not the GEN-09 watermark note, not
//! [`RUNWAY_TRAINING_LICENSE_DISCLOSURE`]'s unconditional Phase-42.1 sign-off
//! text, not PROMPT-03's prompt echo, and not the never-panic `is_error`
//! tool_result on every failure path.

use rudis_core::MediaBinItem;

use crate::gen_submission::{ClipEdge, GenSubmission, ReferenceSource};
use crate::inspect::{INSPECT_JPEG_QUALITY, INSPECT_MEDIA_FRAME_MAX_EDGE};
use crate::AppCtx;

/// Parse the tool-call JSON's `referenceSource` (+ `referenceMediaId` when it is
/// `"media"`) into a closed [`ReferenceSource`] (ASVS V5 input validation,
/// T-34.1-01). `None` (no `referenceSource` key) means an unconditioned call —
/// the pre-34.1 behavior. `"media"` without a `referenceMediaId` is an honest
/// `Err`; any string outside `sketch|frame|media` is an honest `Err`.
fn parse_reference_source(
    input: &serde_json::Value,
) -> Result<Option<ReferenceSource>, String> {
    parse_frame_source(input, "reference")
}

/// Quick 260726-t5z: the DESTINATION (last-frame) twin of
/// [`parse_reference_source`] — `destinationSource` + `destinationMediaId` /
/// `destinationClipId`. Deliberately the SAME closed-enum validation, not a
/// second hand-rolled match: the two slots must accept exactly the same
/// vocabulary and reject unknown values identically, or the destination side
/// would quietly become the loose one.
fn parse_destination_source(
    input: &serde_json::Value,
) -> Result<Option<ReferenceSource>, String> {
    parse_frame_source(input, "destination")
}

/// The shared body of both parsers, keyed by field PREFIX (`"reference"` /
/// `"destination"`) so every error message names the field the caller actually
/// passed.
///
/// Accepts the full source vocabulary on BOTH slots even though the tool schema
/// advertises a narrower, guided pairing per slot (`clipEnd` for the outgoing
/// clip, `clipStart` for the incoming one). That asymmetry is prompt GUIDANCE —
/// the pairing the model should reach for — while the backend simply refuses to
/// invent an error for a coherent request: "end this shot on the last frame of
/// clip X" is a real intent, and the schema is not `strict`, so rejecting it
/// would be a gratuitous failure rather than a safety property. What IS enforced
/// here is what matters: a CLOSED value set (T-34.1-01, no free string reaching
/// resolution) and each keyword reading only its OWN id field.
fn parse_frame_source(
    input: &serde_json::Value,
    prefix: &str,
) -> Result<Option<ReferenceSource>, String> {
    let source_key = format!("{prefix}Source");
    // One accessor for both id fields — `{prefix}MediaId` / `{prefix}ClipId`.
    let required_id = |suffix: &str, value: &str| -> Result<String, String> {
        let key = format!("{prefix}{suffix}");
        input
            .get(&key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("{source_key} \"{value}\" requires {key}"))
    };
    match input.get(&source_key).and_then(|v| v.as_str()) {
        None => Ok(None),
        Some("sketch") => Ok(Some(ReferenceSource::Sketch)),
        Some("frame") => Ok(Some(ReferenceSource::Frame)),
        Some("media") => Ok(Some(ReferenceSource::Media(required_id(
            "MediaId", "media",
        )?))),
        Some("clipEnd") => Ok(Some(ReferenceSource::Clip(
            required_id("ClipId", "clipEnd")?,
            ClipEdge::End,
        ))),
        Some("clipStart") => Ok(Some(ReferenceSource::Clip(
            required_id("ClipId", "clipStart")?,
            ClipEdge::Start,
        ))),
        Some(other) => Err(format!(
            "unrecognized {source_key} '{other}' (expected \"sketch\", \"frame\", \"media\", \
             \"clipEnd\", or \"clipStart\")"
        )),
    }
}

/// Phase 55.1 (D-10): read the REQUIRED free-text `model` id out of a
/// `generate_ai_image` / `generate_ai_video` tool input.
///
/// # What this deliberately does NOT do
///
/// It does not check the id against [`agent_gen::RUNWAY_MODELS`], against
/// `catalog()`, or against any local table at all. D-01 is explicit — *"fully
/// open free-text model ids, no roster validation"* — because every local check
/// re-introduces the staleness the phase exists to remove: a model Runway shipped
/// this morning would be refused until Rudis recompiled. Runway's `model` field
/// IS a server-side closed enum, so an id it does not know comes back as its own
/// `{error, docUrl, issues}` 400, surfaced as the vendor's text rather than
/// re-authored as a Rudis rejection (D-05).
///
/// What it DOES enforce is the two things that are not roster questions:
///
/// 1. **Presence.** The field is required, and a missing one is a `$0.00` local
///    refusal that never reaches a provider. Guessing a default here would be
///    the deleted `gen4_image` capability hardcode returning under a new name,
///    and would bill the user for a model nobody chose.
/// 2. **The retired vocabulary is refused LOUDLY.** `shape`, `stage` and the
///    older `intent` are gone (D-11). A model still typing one is running on
///    stale context; answering it with a refusal that names `model` is what
///    lets its next attempt succeed. Silently ignoring the key would bill for a
///    model the caller did not pick — the exact failure the whole capability
///    indirection was built to prevent.
///
/// Both messages are matched by [`crate::PRE_SPEND_VALIDATION_SUBSTRINGS`]
/// (`"requires a model"` / `"replaced by model"`), so neither consumes one of
/// the turn's two paid retry slots.
fn model_from_input(input: &serde_json::Value, tool_name: &str) -> Result<String, String> {
    if input.get("shape").is_some() || input.get("stage").is_some() || input.get("intent").is_some()
    {
        return Err("the shape/stage fields were replaced by model -- name the Runway model id \
                    directly in the model field (the rulebook carries the cost table)"
            .to_string());
    }
    input
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "{tool_name} requires a model -- name the Runway model id to run (pick from \
                 the rulebook's cost table, or the model the user asked for)"
            )
        })
}

/// Phase 56 (GEN-11 / D-04): read the OPTIONAL free-text `model` id out of a
/// `generate_ai_video_edit` tool input.
///
/// The sibling reader is [`model_from_input`], and the difference between them
/// is the whole of 56-05's and 56-06's contract. There the field is REQUIRED and
/// its absence is a refusal, because neither sibling has a default anyone chose
/// — 55.1-03 deleted the `generate_ai_image` arm that always claimed
/// `gen4_image`, precisely because a disclosure naming a model no call submitted
/// is a fabricated claim. Here the capability HAS a real, derived, priced
/// default (`agent_gen::advisory_video_edit_model`), so an omitted model is a
/// legitimate call rather than a malformed one.
///
/// **It never defaults, and that is the point.** The substitution belongs to the
/// governed host entry (`generate_runway_video_edit_for_agent`), which is the
/// same place `resolved_model_for_tool_input`'s disclosure fallback and
/// `estimated_video_edit_cost_cents`'s price both read it from. If the bridge
/// substituted here, the host's fallback would be unreachable and its
/// disclosure would be describing a decision made somewhere else.
///
/// A BLANK id normalizes to ABSENT rather than to a refusal — deliberately
/// divergent from the sibling seam, where `model` is required and blank is
/// malformed. Here blank means UNNAMED, and 56-06's seam normalizes it
/// identically, so one string has one meaning at every reader (T-42.3-11).
fn optional_model_from_input(input: &serde_json::Value) -> Option<String> {
    input
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Phase 56 (GEN-11): parse ONE `references[]` item into a closed
/// [`ReferenceSource`] (T-56-INJ-05).
///
/// The same closed vocabulary [`parse_frame_source`] enforces on the flat
/// sibling fields, expressed per array item: a source WORD plus the one id field
/// that word reads. An unknown word is an honest `Err` NAMING it, and the index
/// travels with the message because a five-item array's third bad entry is
/// otherwise a guessing game for whoever has to fix it.
///
/// `clipStart` is deliberately absent from the accepted set: a clip edit has no
/// destination endpoint, so admitting it would be inventing a concept the seam
/// cannot carry.
fn parse_reference_item(item: &serde_json::Value, index: usize) -> Result<ReferenceSource, String> {
    let required_id = |key: &str, value: &str| -> Result<String, String> {
        item.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("references[{index}] source \"{value}\" requires {key}"))
    };
    match item.get("source").and_then(|v| v.as_str()) {
        None => Err(format!(
            "references[{index}] requires a source (\"frame\", \"media\", \"sketch\", or \
             \"clipEnd\")"
        )),
        Some("sketch") => Ok(ReferenceSource::Sketch),
        Some("frame") => Ok(ReferenceSource::Frame),
        Some("media") => Ok(ReferenceSource::Media(required_id("mediaId", "media")?)),
        Some("clipEnd") => Ok(ReferenceSource::Clip(
            required_id("clipId", "clipEnd")?,
            ClipEdge::End,
        )),
        Some(other) => Err(format!(
            "unrecognized reference source '{other}' at references[{index}] (expected \
             \"frame\", \"media\", \"sketch\", or \"clipEnd\")"
        )),
    }
}

/// Phase 56 (GEN-11): the whole OPTIONAL `references` array, or an empty vec.
///
/// **Items are parsed BEFORE the count gate fires**, which departs from the
/// plan's written order, and the reason is that the two layers have different
/// costs. In the FFI host, `video_edit_reference_check` runs first because the
/// work behind it is a store lock, an ffmpeg decode and a whiteboard raster —
/// there, refusing on a number before touching any of that is a spend property
/// (T-56-SPEND-06). Here the work behind it is `serde_json` string matching over
/// at most a handful of items, so ordering buys nothing, and parsing first buys
/// something real: a caller who sent SIX items one of which is misspelled learns
/// about the misspelling, instead of fixing the count and then discovering the
/// typo on the next round trip.
///
/// The count gate itself is [`crate::video_edit_reference_check`] — the ONE
/// site, one threshold, one message, called rather than restated (56-04's rule).
fn parse_reference_list(input: &serde_json::Value) -> Result<Vec<ReferenceSource>, String> {
    let Some(raw) = input.get("references") else {
        return Ok(Vec::new());
    };
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let items = raw.as_array().ok_or_else(|| {
        "references must be an array of {source, mediaId?, clipId?} objects".to_string()
    })?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_reference_item(item, index))
        .collect()
}

/// Phase 56 (GEN-11 / D-04): drive the `generate_ai_video_edit` seam — the
/// clip-edit sibling of [`run_generate_ai_video`], and the FIRST tool on which
/// the user's OWN recorded footage leaves the machine.
///
/// Everything expensive happens on the far side of
/// [`GenSubmission::submit_video_edit`], backend-side: the clip lookup, 56-04's
/// window check, the trim-respecting range extraction and the ONE governed
/// submit. This half does what the other two `run_generate_ai_*` do — parse
/// every caller value BEFORE any resolution (the module's ordering discipline),
/// hand them to the seam under the same block guard, look the landed asset up
/// under a SHORT store lock, and decode a frame the agent can see.
///
/// The bridge deliberately carries **no** clip-id validation, no window check
/// and no path resolution: those need the store and the sidecar, they already
/// exist at exactly one site each (56-04/56-06), and a second copy here would be
/// a second message about the same state.
pub fn run_generate_ai_video_edit<C: AppCtx + GenSubmission>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<(MediaBinItem, Option<Vec<u8>>, bool), String> {
    let store = ctx.store();
    // 1a. The prompt — what to CHANGE. Reaches only `GenRequest.prompt`
    //     downstream, never a filename and never an endpoint (the endpoint
    //     follows the FRAMES: `GenRequest.source_video`'s presence is what
    //     selects `/v1/video_to_video`, 56-03).
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "generate_ai_video_edit requires a prompt string".to_string())?
        .to_string();

    // 1b. The clip whose CURRENT trimmed range is edited (D-02). Validated as a
    //     non-empty string here and as a real timeline id backend-side — a blank
    //     one is a $0.00 local refusal rather than a store lookup for "".
    let clip_id = input
        .get("clipId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "generate_ai_video_edit requires a clipId string naming the timeline clip to edit"
                .to_string()
        })?
        .to_string();

    // 1c. The OPTIONAL model, forwarded VERBATIM. NOTHING here defaults it —
    //     no `unwrap_or`, no `or_else`, no roster lookup — because the
    //     substitution is the governed host entry's, and the disclosure and the
    //     price both read it from there. See `optional_model_from_input`.
    //     (The one `unwrap_or` below is the tokio runtime-flavor probe.)
    let model = optional_model_from_input(input);

    // 1d. The OPTIONAL reference set: closed vocabulary per item, then the ONE
    //     shared count gate. Both run before ANY resolution or provider work.
    let references = parse_reference_list(input)?;
    crate::video_edit_reference_check(references.len())?;

    // 2. The seam, under the SAME block guard the other two use: block_in_place
    //    PANICS off a multi-thread runtime, so fall back to an inline block_on
    //    under the tokio-less MockRuntime tests. Nothing is resolved on this
    //    side of it — the clip id, the window and the frames are all the host's,
    //    so no footage crosses the renderer/IPC boundary (T-34.1-09, and
    //    56-06 proved the bytes never cross the C ABI either).
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let generated = if on_multi_thread {
        tokio::task::block_in_place(|| {
            ctx.block_on(async { ctx.submit_video_edit(prompt, clip_id, model, references).await })
        })
    } else {
        ctx.block_on(async { ctx.submit_video_edit(prompt, clip_id, model, references).await })
    }?;

    // 3. Look up the landed id's MediaBinItem under a SHORT store lock.
    let id = generated
        .media_item_ids
        .first()
        .cloned()
        .ok_or_else(|| "generation reported success but landed no asset id".to_string())?;
    let item = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        guard
            .media_item(&id)
            .cloned()
            .ok_or_else(|| format!("landed asset {id} is missing from the media bin (unreachable)"))?
    };

    // 4. A preview frame the agent can SEE, decoded from the file the landing
    //    bridge already probed — never from provider bytes. Downgrade to None on
    //    failure: the asset IS landed and undoable, so failing the tool over a
    //    missing preview would lie about what happened.
    let preview = match engine::decode_frame_rgba_at(std::path::Path::new(&item.path), 0, 0) {
        Ok(frame) => match engine::encode_jpeg_bytes(
            &frame,
            INSPECT_MEDIA_FRAME_MAX_EDGE,
            INSPECT_JPEG_QUALITY,
        ) {
            Ok(jpeg) => Some(jpeg),
            Err(e) => {
                eprintln!(
                    "generate_ai_video_edit: no preview JPEG for {}: {e}",
                    item.path
                );
                None
            }
        },
        Err(e) => {
            eprintln!(
                "generate_ai_video_edit: could not decode {} for preview: {e}",
                item.path
            );
            None
        }
    };

    Ok((item, preview, generated.carries_provenance_watermark))
}

/// Phase 56 (GEN-11 / D-04): the `generate_ai_video_edit` interception (the
/// never-panic run/handle split). Any failure — a refused reference set, an
/// out-of-window clip, an unknown clip id, no key, a gate rejection, a provider
/// error — is a single `is_error` text tool_result, never a panic and never a
/// silent success.
///
/// # The success text is doing four jobs at once
///
/// 1. **D-06:** it says a NEW asset landed and the original is untouched, so the
///    agent does not go looking for a replace-in-place that does not exist.
/// 2. **D-07:** it teaches detachAudio-before-covering at the exact moment the
///    audio is about to be lost. A generated clip is always picture-only; a
///    relit talking head placed over its source goes silent unless the source's
///    audio was detached first. The skill body teaches this too, but a tool
///    result the agent is already reading is the last place it can still land.
/// 3. **GEN-09 + GEN-08:** the watermark note, then the shared training-license
///    disclosure, then the clip-edit-specific sentence naming the user's OWN
///    uploaded footage — the 2026-08-09 signature's binding condition.
/// 4. **PROMPT-03:** the prompt echo, verbatim from the tool's own input.
pub fn handle_generate_ai_video_edit<C: AppCtx + GenSubmission>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    match run_generate_ai_video_edit(ctx, input) {
        Ok((item, preview, carries_watermark)) => {
            let duration_s = ((item.duration_us as f64) / 1_000_000.0).round() as i64;
            let source_clip = input
                .get("clipId")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let mut text = format!(
                "Edited clip {source_clip} via Runway: a NEW {duration_s}s {}x{} asset landed \
                 in the media bin as {} ({}). The original clip is untouched and still on the \
                 timeline. Place the new asset yourself -- and if you place it OVER the source \
                 clip, FIRST call detachAudio on the source so its audio survives, because the \
                 edited clip is picture only.",
                item.width, item.height, item.id, item.path
            );
            if carries_watermark {
                text.push_str(
                    " Note: this AI-edited clip carries an invisible AI-provenance watermark \
                     (C2PA/SynthID-class) that persists into exported files.",
                );
            }
            text.push_str(RUNWAY_TRAINING_LICENSE_DISCLOSURE);
            text.push_str(RUNWAY_SOURCE_CLIP_TRAINING_DISCLOSURE);
            // PROMPT-03 (Phase 40): the actual prompt sent to the provider, from
            // the tool's OWN caller-supplied args (never credential material).
            if let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) {
                text.push_str(&format!(" Prompt sent to the provider: \"{prompt}\""));
            }
            let content = match preview {
                Some(jpeg) => agent_llm::vision::image_tool_result(text, &jpeg),
                None => agent_llm::vision::text_tool_result(text),
            };
            agent_llm::ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content,
                is_error: None,
            }
        }
        Err(e) => agent_llm::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: agent_llm::vision::text_tool_result(format!(
                "generate_ai_video_edit failed: {e}"
            )),
            is_error: Some(true),
        },
    }
}

/// Phase 32 (GEN-01): drive the `generate_ai_image` seam. Routes the prompt
/// through [`GenSubmission::submit_image`] to the still-`src-tauri`
/// `generation::generate_runway_image_for_agent` (the ONE governed submit path —
/// GEN-08 gate INSIDE it), then decodes the JUST-LANDED file to a JPEG the
/// agent can SEE. Returns the landed `MediaBinItem`, an optional preview JPEG (a
/// decode/encode failure downgrades to `None` — the asset IS landed + undoable,
/// so failing the tool over a preview would lie), and the GEN-09 provenance flag.
///
/// Never a new submit path. **Phase 55.1 (D-10) changed what the model id is**:
/// it is no longer a server-side const derived from a pinned capability word, it
/// is the caller's own REQUIRED `model` field, read here and carried through
/// verbatim. The fields read are therefore `prompt`, `model` and the
/// `referenceSource` trio.
pub fn run_generate_ai_image<C: AppCtx + GenSubmission>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<(MediaBinItem, Option<Vec<u8>>, bool), String> {
    // Plan 45-11: the `store: &SharedStore` parameter this function used to
    // take is now `ctx.store()` -- provably the SAME value, because every call
    // site builds its `TauriAppCtx` from exactly the `&SharedStore` it used to
    // pass here.
    let store = ctx.store();
    // 1. The prompt reaches only GenRequest.prompt downstream — never a
    //    filename or an endpoint (T-32-17). It is no longer true that no caller
    //    value reaches the MODEL: `model` (read at 1d) is now exactly that, by
    //    owner decision D-01/D-10, and the residual injection risk that opens is
    //    recorded in PROVENANCE.md Entry 17's 2026-08-01 amendment.
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "generate_ai_image requires a prompt string".to_string())?
        .to_string();

    // 1b. Phase 42.3 (D): the `background` SCHEMA FIELD is deleted. Runway's
    //     image API has no background parameter at any nesting level (42.1-03
    //     established this; 42.1-04's live probe confirmed Runway silently
    //     STRIPS unknown keys), and the field's own description had degraded to
    //     "OMIT THIS -- it currently has NO effect on the output" — prose that
    //     spends model attention to say nothing. Plan 42.3-02 removed the
    //     property and added `background` to this tool's walk ban list; this is
    //     the src-tauri half: no parse, no caller value, no error path.
    //
    //     `Transparent` is HARDCODED rather than dropped so the downstream
    //     `GenRequest.background` bytes are byte-identical to the shipped 34.1
    //     default. `agent_gen::BackgroundMode` and `GenRequest.background` are
    //     deliberately NOT deleted — they are shared plumbing all three
    //     modalities construct (agent-gen `dispatch.rs` / `fixture.rs`), and
    //     removing them would be a cross-crate change this phase does not own.
    //
    //     A stray `background` key in the tool input is now simply IGNORED
    //     (the handler reads only what it needs; `additionalProperties: false`
    //     is the schema-side guard), never an error.
    let background = agent_gen::BackgroundMode::Transparent;

    // 1c. Phase 34.1 (GEN-10): the OPTIONAL conditioning reference source
    //     (sketch / annotated frame / media item) the agent named. Parsed to a
    //     closed enum here (T-34.1-01, honest Err on an unrecognized value BEFORE
    //     any provider work); the actual PNG bytes are resolved backend-side
    //     inside the async block below, at tool-call time on a fresh snapshot.
    let reference_source = parse_reference_source(input)?;

    // 1d. Phase 55.1 (D-10): the REQUIRED model id — the caller's own free-text
    //     choice, replacing the capability-word -> `gen4_image` const the seam
    //     used to hardcode. Read here, with the other caller values, so a
    //     model-less call is a $0.00 refusal before a frame is resolved or a
    //     paid call is reachable. Trimmed and non-empty-checked ONLY: whether
    //     the id exists is Runway's server-side enum to answer (D-01), not
    //     ours, and any local plausibility check would rebuild the staleness
    //     this phase exists to remove.
    let model = model_from_input(input, "generate_ai_image")?;

    // 2. Call the seam entry under the SAME block guard run_generate_image uses:
    //    block_in_place PANICS off a multi-thread runtime, so fall back to an
    //    inline block_on under the tokio-less MockRuntime tests (the
    //    list_generation_models_inner precedent). The reference is resolved
    //    INSIDE the same async block, backend-side, BEFORE the seam entry — no
    //    reference bytes ever cross the renderer/IPC boundary (T-34.1-09).
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let generated = if on_multi_thread {
        tokio::task::block_in_place(|| {
            ctx.block_on(async {
                let reference = match reference_source {
                    None => None,
                    Some(src) => Some(ctx.resolve_reference_image(src).await?),
                };
                ctx.submit_image(prompt, model, background, reference).await
            })
        })
    } else {
        ctx.block_on(async {
            let reference = match reference_source {
                None => None,
                Some(src) => Some(ctx.resolve_reference_image(src).await?),
            };
            ctx.submit_image(prompt, model, background, reference).await
        })
    }?;

    // 3. Look up the first landed id's MediaBinItem under a SHORT store lock.
    let id = generated
        .media_item_ids
        .first()
        .cloned()
        .ok_or_else(|| "generation reported success but landed no asset id".to_string())?;
    let item = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        guard
            .media_item(&id)
            .cloned()
            .ok_or_else(|| format!("landed asset {id} is missing from the media bin (unreachable)"))?
    };

    // 4. Decode the JUST-LANDED file to a JPEG the agent can SEE (the same
    //    inspect_media recipe — decodes only the file the landing bridge already
    //    probed, never provider bytes directly). Downgrade to None on failure.
    let preview = match engine::decode_frame_rgba_at(std::path::Path::new(&item.path), 0, 0) {
        Ok(frame) => match engine::encode_jpeg_bytes(
            &frame,
            INSPECT_MEDIA_FRAME_MAX_EDGE,
            INSPECT_JPEG_QUALITY,
        ) {
            Ok(jpeg) => Some(jpeg),
            Err(e) => {
                eprintln!("generate_ai_image: no preview JPEG for {}: {e}", item.path);
                None
            }
        },
        Err(e) => {
            eprintln!(
                "generate_ai_image: could not decode {} for preview: {e}",
                item.path
            );
            None
        }
    };

    Ok((item, preview, generated.carries_provenance_watermark))
}

/// Phase 32 (GEN-01): the `generate_ai_image` interception (the never-panic
/// run/handle split). Success returns an IMAGE-bearing ToolResult — a Text block
/// (media id + GEN-09 provenance disclosure) THEN the generated image so the
/// agent can see what it made; a preview downgrade drops to text-only. Any
/// failure (no key, gate rejection, provider error, invalid prompt) is a single
/// `is_error` text tool_result — never a panic, never a silent success.
/// **The binding condition of the Phase-42.1 GEN-08 sign-off, discharged.**
///
/// Runway's terms reserve a license to use INPUTS and OUTPUTS for model
/// training, content moderation and product improvement on every non-Enterprise
/// tier (42.1-RESEARCH § (d)). The sign-off that cleared the five `("runway", …)`
/// allow-list rows is conditional on that fact being surfaced to the user
/// in-app, in the same manner the AI-provenance watermark already is — so it is
/// appended here, to the same tool-result text, right after the watermark note.
///
/// # Why it is UNCONDITIONAL, unlike the watermark note
///
/// `carries_provenance_watermark` is a per-MODEL fact the provider catalog
/// knows. The training license is a per-PROVIDER, per-ACCOUNT-TIER fact that
/// Rudis structurally CANNOT observe: the user's plan tier is not visible
/// through a BYO API key, and asking for it would be a worse privacy trade than
/// the disclosure itself. So this is stated for every Runway generation.
/// Over-disclosing to an Enterprise customer costs a sentence; under-disclosing
/// to a Free-tier one is a real harm, and the asymmetry decides it — the same
/// conservative-default reasoning as T-31-19's "an uncatalogued model discloses
/// a watermark anyway".
///
/// Audio (ElevenLabs) deliberately does NOT carry this text: it is a different
/// provider under a different sign-off, and copying a competitor's ToS onto it
/// would be a false statement about ElevenLabs.
/// # Phase 56 (GEN-11): the enumeration was EXTENDED, and that was a condition
/// of a signature rather than a tidy-up
///
/// Until 2026-08-09 this string read *"what is sent (prompts and any reference
/// frames)"*. It **enumerates**, and a user's own recorded source clip is
/// neither of those two things — so the moment `video_to_video` shipped, the
/// disclosure would have been describing a strictly smaller set of inputs than
/// the ones actually leaving the machine.
///
/// The Phase 56 GEN-08 sign-off caught exactly that. The owner's first answer
/// was *"existing text sufficient"*; it was checked against this constant before
/// being recorded, the check refuted it, and the answer was re-decided to
/// **`Disclosure wording: NEW WORDING REQUIRED`** — recorded in the instrument
/// as a **CONDITION OF THE SIGNATURE, not a follow-up**: *"the endpoint does not
/// ship on the current wording."*
///
/// So the enumeration grew a third member naming the user's own source video.
/// It was **extended, never narrowed**: `generate_ai_image` and
/// `generate_ai_video` still see prompts and reference frames named, and the new
/// clause is conditional (*"on a clip edit"*), which is a true statement about
/// Runway's TERMS on every path — the terms are the same whether or not a given
/// call happens to send footage. `the_training_license_disclosure_names_the_
/// users_own_source_video` pins both halves, and counts the append sites so a
/// fourth Runway modality has to join rather than fork.
///
/// The clip-edit handler additionally appends
/// [`RUNWAY_SOURCE_CLIP_TRAINING_DISCLOSURE`], because a conditional clause in a
/// shared paragraph reads as boilerplate, and the one fact the owner conditioned
/// their signature on is that the user is told *their own footage* left the
/// machine.
const RUNWAY_TRAINING_LICENSE_DISCLOSURE: &str =
    " Note: unless the user's Runway account is on an Enterprise plan, Runway's terms let \
     Runway use both what is sent -- prompts, any reference frames, and, on a clip edit, \
     the user's own source video itself (the footage they recorded and put on their own \
     timeline, uploaded to Runway) -- and what comes back, to train and improve their \
     models. Tell the user this.";

/// Phase 56 (GEN-11): the clip-edit path's own half of the training-license
/// disclosure — appended ONLY by [`handle_generate_ai_video_edit`], right after
/// the shared constant above.
///
/// It exists because the shared string can only ever state the clip-edit case
/// CONDITIONALLY (it is appended by three handlers, two of which never send
/// footage), and "unless... on a clip edit... may be used" is the kind of clause
/// a reader — or a model paraphrasing for a reader — skims past. This one is
/// about the call that just happened, in the indicative.
///
/// It is the tool-result (agent-facing) copy. The structural, narration-proof
/// half the USER reads lives in [`crate::agent_turn`]'s
/// `provider_notice_for_modality`, for the reason T-p3q-01 established: nothing
/// forces Claude to relay tool text verbatim, and live UAT proved it
/// paraphrases. A legal disclosure conditional on the model choosing to repeat
/// it is not a disclosure.
const RUNWAY_SOURCE_CLIP_TRAINING_DISCLOSURE: &str =
    " That clause is not hypothetical on this call: THIS edit uploaded the user's own \
     source clip -- their recorded footage, not merely a prompt or a reference frame. \
     Say so plainly rather than summarising the note away.";

pub fn handle_generate_ai_image<C: AppCtx + GenSubmission>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    match run_generate_ai_image(ctx, input) {
        Ok((item, preview, carries_watermark)) => {
            let mut text = format!(
                "Generated a {}x{} AI image via Runway, added to the media bin \
                 as {} ({}). Place it with placeClip.",
                item.width, item.height, item.id, item.path
            );
            if carries_watermark {
                text.push_str(
                    " Note: this AI-generated image carries an AI-provenance watermark \
                     (C2PA/SynthID-class) that persists into exported files.",
                );
            }
            text.push_str(RUNWAY_TRAINING_LICENSE_DISCLOSURE);
            // PROMPT-03 (Phase 40): surface the actual prompt sent to the provider in
            // the Chat transcript -- extends the existing GEN-09 disclosure-text
            // transparency mechanism to also carry prompt content. `input` is the
            // tool's OWN caller-supplied args (already scoped away from any
            // credential/keyring material) -- never a secret (see this plan's
            // threat_model, T-40-01).
            if let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) {
                text.push_str(&format!(" Prompt sent to the provider: \"{prompt}\""));
            }
            let content = match preview {
                Some(jpeg) => agent_llm::vision::image_tool_result(text, &jpeg),
                None => agent_llm::vision::text_tool_result(text),
            };
            agent_llm::ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content,
                is_error: None,
            }
        }
        Err(e) => agent_llm::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: agent_llm::vision::text_tool_result(format!("generate_ai_image failed: {e}")),
            is_error: Some(true),
        },
    }
}

/// Phase 33 (GEN-02): the `generate_ai_video` run half — the async sibling of
/// [`run_generate_ai_image`]. Extracts the caller values, drives the seam entry
/// under the SAME block guard, looks up the landed asset, and decodes its FIRST
/// frame for a preview the agent can see.
///
/// **Phase 55.1 (D-10/D-11):** `prompt` is no longer the sole free-text caller
/// value — `model` is now a REQUIRED one, and it reaches `GenRequest.model_id`.
/// T-33-19's ban is retired for that one field ON PURPOSE, with the reason and
/// the residual risk recorded in PROVENANCE.md Entry 17's 2026-08-01 amendment;
/// the prompt still reaches only `GenRequest.prompt`, and no caller value has
/// become a filename or an endpoint.
pub fn run_generate_ai_video<C: AppCtx + GenSubmission>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<(MediaBinItem, Option<Vec<u8>>, bool), String> {
    // Plan 45-11: the `store: &SharedStore` parameter this function used to
    // take is now `ctx.store()` -- provably the SAME value, because every call
    // site builds its `TauriAppCtx` from exactly the `&SharedStore` it used to
    // pass here.
    let store = ctx.store();
    // 1. The prompt. No size/duration/resolution/background field exists on this
    //    tool's schema and none may be added — those nine substrings are still
    //    banned by the schema walk. `model` is the ONE deliberate exception
    //    (Phase 55.1 D-06), read at 1d.
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "generate_ai_video requires a prompt string".to_string())?
        .to_string();

    // 1b. Phase 34.1 (GEN-10): the OPTIONAL first-frame conditioning reference
    //     (sketch / annotated frame / media item) the agent named — parsed to a
    //     closed enum (T-34.1-01) here, resolved to PNG bytes backend-side inside
    //     the async block below. Video has no background parameter to touch.
    let reference_source = parse_reference_source(input)?;
    // 1c. Quick 260726-t5z: the OPTIONAL destination (LAST frame). Parsed with the
    //     SAME closed-enum validation, and BOTH are parsed BEFORE any resolution
    //     or provider work — so a typo in either field is a clean Err before a
    //     single frame is decoded or a paid call is reachable.
    let destination_source = parse_destination_source(input)?;
    // 1d. Phase 55.1 (D-10/D-11): the REQUIRED model id, which REPLACED Phase
    //     42.3's two-axis `shape`/`stage` capability vocabulary outright. The
    //     agent now names the Runway model itself, guided by the rulebook's
    //     cost/capability table or by the user's own words in chat, and the
    //     string reaches `GenRequest.model_id` verbatim (D-01: no local roster
    //     check — Runway's own server-side enum is the validator).
    //
    //     A lingering `shape`/`stage`/`intent` key is a LOUD refusal naming the
    //     field that replaced it, exactly as the retired `intent` key already
    //     was: a model running on stale context must be corrected, never
    //     silently billed for a model it did not choose (T-33-19's residual
    //     concern). Both refusals are $0.00 and raised before ANY provider
    //     work, which is why both are `PRE_SPEND_VALIDATION_SUBSTRINGS` entries.
    let model = model_from_input(input, "generate_ai_video")?;

    // 2. Call the seam entry under the SAME block guard run_generate_ai_image
    //    uses: block_in_place PANICS off a multi-thread runtime, so fall back to
    //    an inline block_on under the tokio-less MockRuntime tests. The reference
    //    is resolved INSIDE the same async block, backend-side, BEFORE the seam
    //    entry — no reference bytes ever cross the renderer/IPC boundary (T-34.1-09).
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let generated = if on_multi_thread {
        tokio::task::block_in_place(|| {
            ctx.block_on(async {
                let reference = match reference_source {
                    None => None,
                    Some(src) => Some(ctx.resolve_reference_image(src).await?),
                };
                let destination = match destination_source {
                    None => None,
                    Some(src) => Some(ctx.resolve_reference_image(src).await?),
                };
                ctx.submit_video(prompt, model, reference, destination)
                    .await
            })
        })
    } else {
        ctx.block_on(async {
            let reference = match reference_source {
                None => None,
                Some(src) => Some(ctx.resolve_reference_image(src).await?),
            };
            let destination = match destination_source {
                None => None,
                Some(src) => Some(ctx.resolve_reference_image(src).await?),
            };
            ctx.submit_video(prompt, model, reference, destination)
                .await
        })
    }?;

    // 3. Look up the first landed id's MediaBinItem under a SHORT store lock.
    let id = generated
        .media_item_ids
        .first()
        .cloned()
        .ok_or_else(|| "generation reported success but landed no asset id".to_string())?;
    let item = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        guard
            .media_item(&id)
            .cloned()
            .ok_or_else(|| format!("landed asset {id} is missing from the media bin (unreachable)"))?
    };

    // 4. Decode the JUST-LANDED file's first frame to a JPEG the agent can SEE
    //    (the same recipe as run_generate_ai_image — decodes only the file the
    //    landing bridge already probed, never provider bytes). Downgrade to None
    //    on failure: the asset IS landed and undoable, so a missing preview must
    //    not fail the tool.
    let preview = match engine::decode_frame_rgba_at(std::path::Path::new(&item.path), 0, 0) {
        Ok(frame) => match engine::encode_jpeg_bytes(
            &frame,
            INSPECT_MEDIA_FRAME_MAX_EDGE,
            INSPECT_JPEG_QUALITY,
        ) {
            Ok(jpeg) => Some(jpeg),
            Err(e) => {
                eprintln!("generate_ai_video: no preview JPEG for {}: {e}", item.path);
                None
            }
        },
        Err(e) => {
            eprintln!(
                "generate_ai_video: could not decode {} for preview: {e}",
                item.path
            );
            None
        }
    };

    Ok((item, preview, generated.carries_provenance_watermark))
}

/// Phase 33 (GEN-02): the `generate_ai_video` interception (the never-panic
/// run/handle split, mirroring [`handle_generate_ai_image`]). Success returns an
/// IMAGE-bearing ToolResult — a Text block (media id + GEN-09 provenance
/// disclosure) THEN a decoded frame; a preview downgrade drops to text-only. Any
/// failure (no key, gate rejection, provider error, timeout, invalid prompt) is
/// a single `is_error` text tool_result — never a panic, never a silent success.
pub fn handle_generate_ai_video<C: AppCtx + GenSubmission>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    match run_generate_ai_video(ctx, input) {
        Ok((item, preview, carries_watermark)) => {
            let duration_s = ((item.duration_us as f64) / 1_000_000.0).round() as i64;
            let mut text = format!(
                "Generated a {}s {}x{} AI video clip via Runway, \
                 added to the media bin as {} ({}). Place it with placeClip.",
                duration_s, item.width, item.height, item.id, item.path
            );
            if carries_watermark {
                text.push_str(
                    " Note: this AI-generated clip carries an invisible AI-provenance watermark \
                     (C2PA/SynthID-class) that persists into exported files.",
                );
            }
            text.push_str(RUNWAY_TRAINING_LICENSE_DISCLOSURE);
            // PROMPT-03 (Phase 40): surface the actual prompt sent to the provider in
            // the Chat transcript -- extends the existing GEN-09 disclosure-text
            // transparency mechanism to also carry prompt content. `input` is the
            // tool's OWN caller-supplied args (already scoped away from any
            // credential/keyring material) -- never a secret (see this plan's
            // threat_model, T-40-01).
            if let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) {
                text.push_str(&format!(" Prompt sent to the provider: \"{prompt}\""));
            }
            let content = match preview {
                Some(jpeg) => agent_llm::vision::image_tool_result(text, &jpeg),
                None => agent_llm::vision::text_tool_result(text),
            };
            agent_llm::ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content,
                is_error: None,
            }
        }
        Err(e) => agent_llm::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: agent_llm::vision::text_tool_result(format!("generate_ai_video failed: {e}")),
            is_error: Some(true),
        },
    }
}

/// Phase 34 (GEN-03): the `generate_ai_audio` run half — the SYNC sibling of
/// [`run_generate_ai_image`], MINUS all frame-decode/image machinery (audio has
/// no visual preview). Extracts the SOLE caller value (`prompt`, which IS the
/// literal text to speak and reaches only `GenRequest.prompt` — never a
/// filename/endpoint/model/voice, T-34-11), drives the seam entry under the SAME
/// block guard, and looks up the landed audio asset. Returns the item + the
/// provenance flag; there is NO preview tuple element.
pub fn run_generate_ai_audio<C: AppCtx + GenSubmission>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<(MediaBinItem, bool), String> {
    // Plan 45-11: the `store: &SharedStore` parameter this function used to
    // take is now `ctx.store()` -- provably the SAME value, because every call
    // site builds its `TauriAppCtx` from exactly the `&SharedStore` it used to
    // pass here.
    let store = ctx.store();
    // 1. The prompt is the ONLY caller input — no voice/model/format field exists
    //    on this tool's schema (Task-2 schema-walk test). It IS the text to speak.
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "generate_ai_audio requires a prompt string".to_string())?
        .to_string();

    // 2. Call the seam entry under the SAME block guard run_generate_ai_image
    //    uses: block_in_place PANICS off a multi-thread runtime, so fall back to
    //    an inline block_on under the tokio-less MockRuntime tests.
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let generated = if on_multi_thread {
        tokio::task::block_in_place(|| {
            ctx.block_on(ctx.submit_audio(prompt))
        })
    } else {
        ctx.block_on(ctx.submit_audio(prompt))
    }?;

    // 3. Look up the first landed id's MediaBinItem under a SHORT store lock.
    let id = generated
        .media_item_ids
        .first()
        .cloned()
        .ok_or_else(|| "generation reported success but landed no asset id".to_string())?;
    let item = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        guard
            .media_item(&id)
            .cloned()
            .ok_or_else(|| format!("landed asset {id} is missing from the media bin (unreachable)"))?
    };

    // NO preview decode — audio has no meaningful visual frame (the text-only pin).
    Ok((item, generated.carries_provenance_watermark))
}

/// Phase 34 (GEN-03): the `generate_ai_audio` interception (the never-panic
/// run/handle split, mirroring [`handle_generate_ai_image`]). Success returns a
/// TEXT-ONLY ToolResult — the new media id + the probed duration + an explicit
/// steer to place on audio track "a1" via placeClip + the GEN-09 provenance
/// disclosure. There is deliberately NO image block: audio has no visual preview.
/// Any failure (no key, gate rejection, provider error, invalid prompt) is a
/// single `is_error` text tool_result — never a panic, never a silent success.
pub fn handle_generate_ai_audio<C: AppCtx + GenSubmission>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    match run_generate_ai_audio(ctx, input) {
        Ok((item, carries_watermark)) => {
            let duration_s = ((item.duration_us as f64) / 1_000_000.0).round() as i64;
            let mut text = format!(
                "Generated a {duration_s}s AI speech clip via ElevenLabs eleven_multilingual_v2, \
                 added to the media bin as {} ({}). This is an audio asset — place it on audio \
                 track \"a1\" with placeClip.",
                item.id, item.path
            );
            if carries_watermark {
                text.push_str(
                    " Note: this AI-generated audio carries an inaudible AI-provenance watermark \
                     (C2PA/SynthID-class) that persists into exported files.",
                );
            }
            // PROMPT-03 (Phase 40): surface the actual prompt sent to the provider in
            // the Chat transcript -- extends the existing GEN-09 disclosure-text
            // transparency mechanism to also carry prompt content. `input` is the
            // tool's OWN caller-supplied args (already scoped away from any
            // credential/keyring material) -- never a secret (see this plan's
            // threat_model, T-40-01).
            if let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) {
                text.push_str(&format!(" Prompt sent to the provider: \"{prompt}\""));
            }
            agent_llm::ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: agent_llm::vision::text_tool_result(text),
                is_error: None,
            }
        }
        Err(e) => agent_llm::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: agent_llm::vision::text_tool_result(format!("generate_ai_audio failed: {e}")),
            is_error: Some(true),
        },
    }
}

/// The two tests that PIN the relocated parsers, moved out of `src-tauri`'s
/// `agent_gate` (plan 45-11).
///
/// `agent_gate` SPLIT — the second module in this phase to do so, after 45-09's
/// `track_object_gate`. Its other ~60 tests all drive `run_agent_turn`,
/// `resolve_reference_image` or a `#[tauri::command]` and stayed; these two
/// drive nothing but a `serde_json::Value` and the closed-enum parsers, share no
/// helper with the stayers, and travel with the functions they pin.
///
/// **Moving them is what keeps `parse_reference_source` /
/// `parse_destination_source` / `parse_frame_source` PRIVATE.** Leaving them
/// behind would have forced three `pub fn`s and a re-export across a crate
/// boundary purely for a test — the T-45-02-class widening 45-14 has to undo.
///
/// The remaining `src-tauri` coverage of this module is stronger than these two
/// and did not move: ~27 `generation.rs` `agent_tool` gates drive
/// `handle_generate_ai_image` / `_video` / `_audio` through REAL fixture and
/// live providers, a real GEN-08 allow-list gate and the real landing bridge.
/// Every one of them needs Tauri managed state (`ManagedGenProvider`,
/// `ManagedVideoGenProvider`, `ManagedAudioGenProvider`), which a `TestAppCtx`
/// cannot represent, so they stayed and were converted in place.
#[cfg(test)]
mod generate_ai_parse_gate {
    use super::*;
    use serde_json::json;

    /// The destination side parses with the SAME closed-enum discipline as the
    /// reference side: each keyword reads only its own id field, a missing id is
    /// an honest `Err` naming the field to supply, and an unrecognized value is
    /// rejected before any resolution work.
    #[test]
    fn parse_destination_source_is_closed_and_names_its_own_fields() {
        // Absent => None (an unconditioned call), not an error.
        assert_eq!(
            parse_destination_source(&json!({ "prompt": "x" })).expect("absent is Ok"),
            None
        );
        // clipStart + its id.
        assert_eq!(
            parse_destination_source(&json!({
                "destinationSource": "clipStart",
                "destinationClipId": "clip-9"
            }))
            .expect("clipStart parses"),
            Some(ReferenceSource::Clip("clip-9".to_string(), ClipEdge::Start))
        );
        // media + its id.
        assert_eq!(
            parse_destination_source(&json!({
                "destinationSource": "media",
                "destinationMediaId": "media-3"
            }))
            .expect("media parses"),
            Some(ReferenceSource::Media("media-3".to_string()))
        );
        // A clip id supplied to the REFERENCE side must not satisfy the
        // destination side — the two slots read different keys.
        let err = parse_destination_source(&json!({
            "destinationSource": "clipStart",
            "referenceClipId": "clip-9"
        }))
        .expect_err("the destination slot must not read the reference slot's id");
        assert!(
            err.contains("destinationClipId"),
            "the error names the field the caller must supply: {err}"
        );
        // An unrecognized value is rejected, and the message names the field the
        // caller actually passed (not the reference one).
        let err = parse_destination_source(&json!({ "destinationSource": "bogus" }))
            .expect_err("unrecognized value is rejected");
        assert!(
            err.contains("unrecognized destinationSource") && err.contains("bogus"),
            "the error names destinationSource specifically: {err}"
        );
        // And the reference side keeps its own vocabulary + gains clipEnd.
        assert_eq!(
            parse_reference_source(&json!({
                "referenceSource": "clipEnd",
                "referenceClipId": "clip-1"
            }))
            .expect("clipEnd parses on the reference side"),
            Some(ReferenceSource::Clip("clip-1".to_string(), ClipEdge::End))
        );
    }

    /// Phase 34.1 (T-34.1-01): `parse_reference_source` — `None` when absent,
    /// `Err` for `"media"` without an id, `Err` for any string outside the closed
    /// set, and the three valid mappings.
    #[test]
    fn parse_reference_source_rejects_unrecognized_strings_and_missing_media_id() {
        // Absent => None (unconditioned, pre-34.1 behavior).
        assert_eq!(
            parse_reference_source(&json!({ "prompt": "a cat" })).unwrap(),
            None
        );
        // Valid mappings.
        assert_eq!(
            parse_reference_source(&json!({ "referenceSource": "sketch" })).unwrap(),
            Some(ReferenceSource::Sketch)
        );
        assert_eq!(
            parse_reference_source(&json!({ "referenceSource": "frame" })).unwrap(),
            Some(ReferenceSource::Frame)
        );
        assert_eq!(
            parse_reference_source(
                &json!({ "referenceSource": "media", "referenceMediaId": "m-7" })
            )
            .unwrap(),
            Some(ReferenceSource::Media("m-7".into()))
        );
        // media without an id => Err.
        let err = parse_reference_source(&json!({ "referenceSource": "media" }))
            .expect_err("media without an id is rejected");
        assert!(err.contains("requires referenceMediaId"), "{err}");
        // Any string outside the closed set => Err.
        let err = parse_reference_source(&json!({ "referenceSource": "http://evil" }))
            .expect_err("an unrecognized source is rejected");
        assert!(err.contains("unrecognized referenceSource"), "{err}");
    }
}

/// Phase 55.1 (plan 03, D-10/D-11): the `model` field is a REQUIRED caller value
/// on BOTH generation tools, and the string the caller typed is what the seam
/// submits — verbatim, with no roster lookup, no intent indirection and no
/// server-side substitution.
///
/// # What this module can and cannot prove
///
/// It pins the BRIDGE half: extraction, refusal and hand-off. The double records
/// what [`GenSubmission::submit_video`] / [`GenSubmission::submit_image`]
/// actually received and then returns `Err`, so nothing downstream runs — that is
/// deliberate, because everything downstream (provider slots, the GEN-08 gate,
/// the landing bridge) needs a [`GenHost`](crate::generation_host::GenHost) with
/// real managed provider state, which a [`TestAppCtx`](crate::TestAppCtx) cannot
/// represent. The other half — that the recorded string becomes
/// `GenRequest.model_id` on the wire — is pinned one crate over in
/// `crates/ffi/tests/contract_generation.rs`, where a real fixture provider and a
/// real allow list exist and the gate itself reports the model id it was handed.
///
/// The two refusals below are ALSO the reason two new needles joined
/// [`crate::PRE_SPEND_VALIDATION_SUBSTRINGS`]: both are `$0.00` local rejections
/// raised before any provider work, so neither may consume one of the two paid
/// retry slots (WR-02 discipline).
#[cfg(test)]
mod model_field_gate {
    use super::*;
    use crate::test_support::TestAppCtx;
    use crate::GeneratedAsset;
    use serde_json::json;
    use std::sync::Mutex;

    /// A ctx that IS a [`TestAppCtx`] for every `AppCtx` capability and, for
    /// [`GenSubmission`], records what each `submit_*` was handed and then
    /// refuses.
    ///
    /// Refusing rather than succeeding is what keeps the assertions honest: a
    /// successful return would send `run_generate_ai_*` on to the media-bin
    /// lookup and an `engine::decode_frame_rgba_at` of a file that does not
    /// exist, which would fail for reasons that have nothing to do with the
    /// model field.
    struct RecordingCtx {
        inner: TestAppCtx,
        seen: Mutex<Vec<String>>,
    }

    impl RecordingCtx {
        fn new() -> Self {
            Self {
                inner: TestAppCtx::new(),
                seen: Mutex::new(Vec::new()),
            }
        }

        /// Everything the double was handed, in order, as `"{modality} {model}"`.
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }

        fn record(&self, line: String) -> Result<GeneratedAsset, String> {
            self.seen
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(line);
            Err("the recording double never lands an asset".to_string())
        }
    }

    // Pure delegation — this double changes NOTHING about the host surface, only
    // about what `GenSubmission` does with what it is given.
    impl AppCtx for RecordingCtx {
        fn store(&self) -> &crate::SharedStore {
            self.inner.store()
        }
        fn app_data_dir(&self) -> Result<std::path::PathBuf, String> {
            self.inner.app_data_dir()
        }
        fn app_cache_dir(&self) -> Result<std::path::PathBuf, String> {
            self.inner.app_cache_dir()
        }
        fn resolve_resource(&self, path: &str) -> Result<std::path::PathBuf, String> {
            self.inner.resolve_resource(path)
        }
        fn emit_patch(
            &self,
            patch: &rudis_core::Patch,
            base_seq: u64,
            seq: u64,
        ) -> Result<(), String> {
            self.inner.emit_patch(patch, base_seq, seq)
        }
        fn active_project_meta(&self) -> &crate::project_store::ActiveProjectMeta {
            self.inner.active_project_meta()
        }
        fn agent_session(&self) -> &Mutex<crate::AgentSession> {
            self.inner.agent_session()
        }
        fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
            self.inner.block_on(fut)
        }
        fn export_progress_sink(&self) -> engine::ProgressFn {
            self.inner.export_progress_sink()
        }
        fn gen_event_sink(&self) -> crate::generation_host::GenEventSink {
            self.inner.gen_event_sink()
        }
        fn run_blocking<T, F>(
            &self,
            f: F,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send>>
        where
            F: FnOnce() -> T + Send + 'static,
            T: Send + 'static,
        {
            self.inner.run_blocking(f)
        }
    }

    impl GenSubmission for RecordingCtx {
        async fn submit_image(
            &self,
            _prompt: String,
            model: String,
            _background: agent_gen::BackgroundMode,
            _reference: Option<agent_gen::ReferenceImage>,
        ) -> Result<GeneratedAsset, String> {
            self.record(format!("image {model}"))
        }

        async fn submit_video(
            &self,
            _prompt: String,
            model: String,
            _reference: Option<agent_gen::ReferenceImage>,
            _destination: Option<agent_gen::ReferenceImage>,
        ) -> Result<GeneratedAsset, String> {
            self.record(format!("video {model}"))
        }

        // Phase 56 plan 06 (GEN-11): the clip-edit seam. The double records the
        // model string the same way the other two do — including which side
        // produced it, because `None` here is the case where the HOST substitutes
        // `advisory_video_edit_model()` and the disclosure arm's fallback depends
        // on that substitution really happening (56-05's contract).
        async fn submit_video_edit(
            &self,
            _prompt: String,
            source_clip_id: String,
            model: Option<String>,
            _references: Vec<ReferenceSource>,
        ) -> Result<GeneratedAsset, String> {
            self.record(format!(
                "video_edit {} clip={source_clip_id}",
                model.as_deref().unwrap_or("<none: host defaults>")
            ))
        }

        async fn submit_audio(&self, _prompt: String) -> Result<GeneratedAsset, String> {
            self.record("audio (no model field on this tool)".to_string())
        }

        async fn resolve_reference_image(
            &self,
            _source: ReferenceSource,
        ) -> Result<agent_gen::ReferenceImage, String> {
            Err("this double resolves no references".to_string())
        }

        // The double's fifth method — the shape/stage translation — went with the
        // trait method itself (Phase 55.1 plan 06). It forwarded to app-core's
        // real implementation, which is now deleted; nothing in this module ever
        // called it, because `model_from_input` is what refuses the retired keys.
    }

    /// D-01/D-02: an OFF-ROSTER model id the agent (or the user, through the
    /// agent) named reaches the submit seam byte-for-byte. `kling3.0_pro` is real
    /// on Runway's side and has never appeared in any Rudis table, so nothing
    /// local could have produced this string — it can only be the caller's.
    #[test]
    fn the_video_bridge_hands_the_seam_the_callers_model_string_verbatim() {
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_video(&ctx, &json!({ "prompt": "p", "model": "kling3.0_pro" }))
            .expect_err("the recording double refuses after recording");
        assert!(
            err.contains("never lands an asset"),
            "the refusal must come from the DOUBLE (i.e. the bridge got that far): {err}"
        );
        assert_eq!(ctx.seen(), vec!["video kling3.0_pro".to_string()]);
    }

    /// The same for the image tool, which had NO caller model channel at all
    /// before this phase (`gen4_image` was hardcoded behind a pinned capability
    /// word). `gen4_image_turbo` is on the roster but was unreachable — D-10's
    /// whole point.
    #[test]
    fn the_image_bridge_hands_the_seam_the_callers_model_string_verbatim() {
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_image(&ctx, &json!({ "prompt": "p", "model": "gen4_image_turbo" }))
            .expect_err("the recording double refuses after recording");
        assert!(err.contains("never lands an asset"), "{err}");
        assert_eq!(ctx.seen(), vec!["image gen4_image_turbo".to_string()]);
    }

    /// The field is REQUIRED, and its absence is a local `$0.00` refusal raised
    /// BEFORE any provider work — proven by the double never being reached.
    #[test]
    fn a_missing_model_is_refused_before_any_provider_work() {
        for (label, result) in [
            (
                "video",
                run_generate_ai_video(&RecordingCtx::new(), &json!({ "prompt": "p" })),
            ),
            (
                "image",
                run_generate_ai_image(&RecordingCtx::new(), &json!({ "prompt": "p" })),
            ),
        ] {
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{label}: a model-less call must be refused"));
            assert!(
                err.contains("requires a model"),
                "{label}: the refusal must name the missing field: {err}"
            );
        }
        // ...and nothing was submitted on either path.
        let ctx = RecordingCtx::new();
        let _ = run_generate_ai_video(&ctx, &json!({ "prompt": "p" }));
        let _ = run_generate_ai_image(&ctx, &json!({ "prompt": "p" }));
        assert!(
            ctx.seen().is_empty(),
            "a model-less call must never reach submit: {:?}",
            ctx.seen()
        );
    }

    /// A blank/whitespace model is the same refusal, not an empty string handed
    /// to Runway.
    #[test]
    fn a_blank_model_is_the_same_refusal_not_an_empty_id_on_the_wire() {
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_video(&ctx, &json!({ "prompt": "p", "model": "   " }))
            .expect_err("a blank model is refused");
        assert!(err.contains("requires a model"), "{err}");
        assert!(ctx.seen().is_empty(), "{:?}", ctx.seen());
    }

    /// D-11: `shape`/`stage` are DELETED, not deprecated. A model running on
    /// stale context that still types one gets a LOUD refusal naming the field
    /// that replaced them — the same discipline the retired `intent` key already
    /// had, and never a silent ignore that would bill for a model the caller did
    /// not choose.
    #[test]
    fn a_lingering_shape_stage_or_intent_key_is_a_loud_refusal_naming_model() {
        for stale in [
            json!({ "prompt": "p", "model": "gen4_turbo", "shape": "transition" }),
            json!({ "prompt": "p", "model": "gen4_turbo", "stage": "draft" }),
            json!({ "prompt": "p", "model": "gen4_turbo", "intent": "cheap-draft" }),
        ] {
            let ctx = RecordingCtx::new();
            let err = run_generate_ai_video(&ctx, &stale)
                .expect_err("a retired selection key is refused");
            assert!(
                err.contains("replaced by model"),
                "the refusal must point at the field that replaced it: {err}"
            );
            assert!(
                ctx.seen().is_empty(),
                "a stale-vocabulary call must never reach submit: {:?}",
                ctx.seen()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Phase 56 plan 07 (GEN-11 / D-04): the clip-edit bridge
    // -----------------------------------------------------------------------

    /// The `model` field is OPTIONAL on this tool and reaches the seam VERBATIM
    /// — including when it is absent, which is the case
    /// `resolved_model_for_tool_input`'s advisory fallback (56-05) and
    /// `generate_runway_video_edit_for_agent`'s substitution (56-06) both stand
    /// on. **No `unwrap_or` in the bridge**: if the bridge defaulted here, the
    /// host's fallback would become unreachable and its disclosure would be
    /// describing a substitution that had already happened somewhere else.
    #[test]
    fn video_edit_parse_passes_the_model_field_through_verbatim() {
        // Named, and off-roster: nothing local could have produced this string.
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_video_edit(
            &ctx,
            &json!({ "prompt": "relight her face", "clipId": "c1", "model": "brand-new-model-2027" }),
        )
        .err()
        .expect("the recording double refuses after recording");
        assert!(err.contains("never lands an asset"), "{err}");
        assert_eq!(
            ctx.seen(),
            vec!["video_edit brand-new-model-2027 clip=c1".to_string()]
        );

        // Absent -> `None` reaches the seam, NOT a bridge-side default.
        let ctx = RecordingCtx::new();
        let _ = run_generate_ai_video_edit(&ctx, &json!({ "prompt": "p", "clipId": "c9" }));
        assert_eq!(
            ctx.seen(),
            vec!["video_edit <none: host defaults> clip=c9".to_string()],
            "an omitted model must arrive as None so the HOST substitutes the advisory \
             default (56-05's contract, 56-06's seam)"
        );

        // Blank -> ABSENT, not an empty id on the wire. The seam normalizes a
        // blank the same way (56-06), so one string has one meaning everywhere.
        let ctx = RecordingCtx::new();
        let _ = run_generate_ai_video_edit(
            &ctx,
            &json!({ "prompt": "p", "clipId": "c9", "model": "   " }),
        );
        assert_eq!(
            ctx.seen(),
            vec!["video_edit <none: host defaults> clip=c9".to_string()]
        );
    }

    /// T-56-INJ-05: the per-item source vocabulary is CLOSED, and an unknown
    /// word is an honest `Err` NAMING it, raised before any resolution — the
    /// same discipline `parse_frame_source` has carried since 34.1.
    ///
    /// `"path"` is the probe value on purpose: it is one of the nine substrings
    /// the schema walk bans as a FIELD name, so if it ever became a legal
    /// SOURCE word the ban would have been routed around rather than removed.
    #[test]
    fn video_edit_parse_rejects_unknown_source_words_before_any_work() {
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_video_edit(
            &ctx,
            &json!({
                "prompt": "p",
                "clipId": "c1",
                "references": [{ "source": "path" }]
            }),
        )
        .err()
        .expect("an unknown source word is refused");
        assert!(
            err.contains("unrecognized reference source 'path'"),
            "the refusal must NAME the rejected word: {err}"
        );
        assert!(
            ctx.seen().is_empty(),
            "an unparseable reference must never reach submit: {:?}",
            ctx.seen()
        );

        // ...and an id-bearing source without its id is the same class of
        // honest refusal, naming the field it wanted.
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_video_edit(
            &ctx,
            &json!({ "prompt": "p", "clipId": "c1", "references": [{ "source": "media" }] }),
        )
        .err()
        .expect("a media reference without mediaId is refused");
        assert!(err.contains("requires mediaId"), "{err}");
        assert!(ctx.seen().is_empty(), "{:?}", ctx.seen());
    }

    /// The over-cap refusal names the MEASURED ceiling (probe F-1b's
    /// `{"code":"too_big","maximum":5,"inclusive":true}`), and it comes from
    /// `app_core::video_edit_reference_check` — the ONE gate — rather than from
    /// a second threshold written here. Asserted by EQUALITY against that
    /// function so a copy-paste of its wording would fail.
    #[test]
    fn video_edit_parse_rejects_more_than_five_references() {
        let six: Vec<serde_json::Value> = (0..6).map(|_| json!({ "source": "sketch" })).collect();
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_video_edit(
            &ctx,
            &json!({ "prompt": "p", "clipId": "c1", "references": six }),
        )
        .err()
        .expect("six references are refused");
        assert_eq!(
            err,
            crate::video_edit_reference_check(6)
                .err()
                .expect("six is over the cap"),
            "the bridge must raise the SHARED refusal verbatim, never its own wording"
        );
        assert!(
            err.contains(&agent_gen::RUNWAY_V2V_MAX_REFERENCES.to_string()),
            "the refusal must name the cap: {err}"
        );
        assert!(ctx.seen().is_empty(), "{:?}", ctx.seen());

        // And 1..=5 is refused too, by the OTHER arm of the same gate, naming
        // 56-09 rather than silently dropping what the caller asked for
        // (42.1-04 caught the alternative live: HTTP 200, plausible, unconditioned).
        let ctx = RecordingCtx::new();
        let err = run_generate_ai_video_edit(
            &ctx,
            &json!({ "prompt": "p", "clipId": "c1", "references": [{ "source": "frame" }] }),
        )
        .err()
        .expect("one reference is refused today");
        assert_eq!(
            err,
            crate::video_edit_reference_check(1)
                .err()
                .expect("any non-empty set is refused today")
        );
        assert!(err.contains("56-09"), "{err}");
        assert!(ctx.seen().is_empty(), "{:?}", ctx.seen());
    }

    /// A ctx whose clip edit SUCCEEDS and whose store already holds the landed
    /// item — the only way to reach the success-text assembly, which
    /// [`RecordingCtx`] deliberately cannot.
    struct LandingCtx {
        inner: TestAppCtx,
    }

    impl LandingCtx {
        /// A ctx holding one media-bin item under `id`, so the post-submit
        /// lookup finds something real. The path does NOT exist, so the preview
        /// decode downgrades to `None` — which is itself the documented
        /// behaviour (a missing preview must never fail a landed, undoable
        /// asset).
        fn with_landed(id: &str) -> Self {
            let inner = TestAppCtx::new();
            let item = rudis_core::MediaBinItem {
                id: id.to_string(),
                path: format!("C:/nonexistent/{id}.mp4"),
                media_kind: rudis_core::MediaKind::Video,
                duration_us: 3_000_000,
                width: 1280,
                height: 720,
                fps: 30.0,
                is_vfr: false,
                rotation_degrees: 0,
                has_audio: false,
                poster_path: None,
                folder: String::new(),
                display_name: None,
                is_image_sequence: false,
                reports_alpha: None,
            };
            inner
                .store()
                .lock()
                .expect("store")
                .dispatch(rudis_core::Command::AddMediaBinItem(item))
                .expect("the landed item joins the bin");
            Self { inner }
        }
    }

    impl AppCtx for LandingCtx {
        fn store(&self) -> &crate::SharedStore {
            self.inner.store()
        }
        fn app_data_dir(&self) -> Result<std::path::PathBuf, String> {
            self.inner.app_data_dir()
        }
        fn app_cache_dir(&self) -> Result<std::path::PathBuf, String> {
            self.inner.app_cache_dir()
        }
        fn resolve_resource(&self, path: &str) -> Result<std::path::PathBuf, String> {
            self.inner.resolve_resource(path)
        }
        fn emit_patch(
            &self,
            patch: &rudis_core::Patch,
            base_seq: u64,
            seq: u64,
        ) -> Result<(), String> {
            self.inner.emit_patch(patch, base_seq, seq)
        }
        fn active_project_meta(&self) -> &crate::project_store::ActiveProjectMeta {
            self.inner.active_project_meta()
        }
        fn agent_session(&self) -> &Mutex<crate::AgentSession> {
            self.inner.agent_session()
        }
        fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
            self.inner.block_on(fut)
        }
        fn export_progress_sink(&self) -> engine::ProgressFn {
            self.inner.export_progress_sink()
        }
        fn gen_event_sink(&self) -> crate::generation_host::GenEventSink {
            self.inner.gen_event_sink()
        }
        fn run_blocking<T, F>(
            &self,
            f: F,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send>>
        where
            F: FnOnce() -> T + Send + 'static,
            T: Send + 'static,
        {
            self.inner.run_blocking(f)
        }
    }

    impl GenSubmission for LandingCtx {
        async fn submit_image(
            &self,
            _p: String,
            _m: String,
            _b: agent_gen::BackgroundMode,
            _r: Option<agent_gen::ReferenceImage>,
        ) -> Result<GeneratedAsset, String> {
            Err("not this double's job".to_string())
        }
        async fn submit_video(
            &self,
            _p: String,
            _m: String,
            _r: Option<agent_gen::ReferenceImage>,
            _d: Option<agent_gen::ReferenceImage>,
        ) -> Result<GeneratedAsset, String> {
            Err("not this double's job".to_string())
        }
        async fn submit_video_edit(
            &self,
            _prompt: String,
            _source_clip_id: String,
            _model: Option<String>,
            _references: Vec<ReferenceSource>,
        ) -> Result<GeneratedAsset, String> {
            Ok(GeneratedAsset {
                media_item_ids: vec!["m-edited".to_string()],
                carries_provenance_watermark: true,
            })
        }
        async fn submit_audio(&self, _p: String) -> Result<GeneratedAsset, String> {
            Err("not this double's job".to_string())
        }
        async fn resolve_reference_image(
            &self,
            _source: ReferenceSource,
        ) -> Result<agent_gen::ReferenceImage, String> {
            Err("this double resolves no references".to_string())
        }
    }

    /// **The GEN-08 binding condition, discharged in the result text itself.**
    ///
    /// The 2026-08-09 signature records `Disclosure wording: NEW WORDING
    /// REQUIRED` as a CONDITION OF THE SIGNATURE, not a follow-up, because the
    /// shipped constant ENUMERATED what it covered — *"prompts and any
    /// reference frames"* — and a user's own recorded clip is neither.
    /// `video_to_video` does not ship on the old wording, so this test is the
    /// gate on it shipping at all.
    ///
    /// It also pins the rest of the set in the same breath: the watermark note,
    /// the prompt echo (PROMPT-03) and the D-06/D-07 placement teaching, whose
    /// detachAudio half is the sentence standing between a relit talking head
    /// and a silent one.
    #[test]
    fn video_edit_result_carries_the_full_disclosure_set() {
        let ctx = LandingCtx::with_landed("m-edited");
        let block = handle_generate_ai_video_edit(
            &ctx,
            "tu-1",
            &json!({ "prompt": "relight her face, warmer key", "clipId": "c-source" }),
        );
        let agent_llm::ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        else {
            panic!("the handler must mint a ToolResult");
        };
        assert!(
            !matches!(is_error, Some(true)),
            "a landed edit is not an error result"
        );
        let text = content
            .iter()
            .find_map(|b| match b {
                agent_llm::ToolResultBlock::Text { text } => Some(text.clone()),
                agent_llm::ToolResultBlock::Image { .. } => None,
            })
            .expect("a text block");

        // The landed asset, and the ORIGINAL left alone (D-06).
        assert!(text.contains("m-edited"), "{text}");
        assert!(text.contains("c-source"), "{text}");
        assert!(text.contains("untouched"), "{text}");
        // D-07, at the exact moment it matters.
        assert!(
            text.contains("detachAudio") && text.contains("picture only"),
            "the placement guidance must teach detachAudio-before-covering: {text}"
        );
        // GEN-09.
        assert!(text.contains("watermark"), "{text}");
        // The training license — the WHOLE shared constant, byte-for-byte.
        assert!(
            text.contains(RUNWAY_TRAINING_LICENSE_DISCLOSURE),
            "the shared training-license disclosure must be appended verbatim: {text}"
        );
        // ...and the source-clip half of it, stated for THIS call rather than
        // left as a clause in a shared paragraph.
        assert!(
            text.contains(RUNWAY_SOURCE_CLIP_TRAINING_DISCLOSURE),
            "the clip-edit result must state that the user's OWN source clip was \
             uploaded — the 2026-08-09 signature's binding condition: {text}"
        );
        // PROMPT-03.
        assert!(
            text.contains("relight her face, warmer key"),
            "the prompt echo must survive: {text}"
        );
    }

    /// The binding condition, checked on the CONSTANT rather than only on one
    /// call's rendered text — so a future edit cannot satisfy the test above by
    /// appending a sentence in one handler while the shared string goes back to
    /// enumerating only prompts and reference frames.
    ///
    /// The second half is just as load-bearing: **the image and video paths'
    /// disclosure must stay TRUE.** The enumeration is extended, never
    /// narrowed — those two still name prompts and reference frames — and all
    /// three handlers append the identical string, so no path can end up with a
    /// weaker story than another.
    #[test]
    fn the_training_license_disclosure_names_the_users_own_source_video() {
        let d = RUNWAY_TRAINING_LICENSE_DISCLOSURE;
        // The Phase 56 addition: the user's OWN footage, named.
        for needle in ["source video", "recorded", "timeline"] {
            assert!(
                d.contains(needle),
                "the training-license disclosure must name the user's own source video \
                 (GEN-08 2026-08-09, `Disclosure wording: NEW WORDING REQUIRED`): {d}"
            );
        }
        // The pre-56 enumeration survives, so the two older paths are still
        // covered by exactly what they were covered by before.
        for needle in ["prompt", "reference frames", "Enterprise", "train and improve"] {
            assert!(
                d.contains(needle),
                "the pre-Phase-56 enumeration must not be narrowed by extending it: {d}"
            );
        }

        // One string, three handlers — asserted on the shipped source rather
        // than on intention, because "we remembered to append it" is exactly
        // the kind of claim that decays one handler at a time.
        let src = include_str!("generation_bridge.rs");
        // Assembled at runtime so this test's OWN source does not contain the
        // needle and inflate its own count — the literal appears here only as
        // the two fragments below.
        let needle = format!("text.push_str({});", "RUNWAY_TRAINING_LICENSE_DISCLOSURE");
        assert_eq!(
            src.matches(needle.as_str()).count(),
            3,
            "all three Runway handlers (image, video, video_edit) must append the SAME \
             disclosure — a fourth Runway modality must join them, never fork"
        );
    }

    /// **Derive, never transcribe** (56-05's rule, applied across a crate
    /// boundary). `agent-llm`'s `schema_strict_guard` bans `maxItems` from every
    /// tool schema — its own words: *"enforce the bound in crates/core code, not
    /// the schema"* — so the tool's cap survives only as PROSE, which nothing
    /// would otherwise hold to the number the endpoint actually measured.
    ///
    /// This crate is the one that can see both the authored schema (through
    /// `agent_llm::tool_defs`) and `agent_gen::RUNWAY_V2V_MAX_REFERENCES`, so
    /// the pin lives here.
    #[test]
    fn the_tool_schemas_reference_cap_agrees_with_the_measured_ceiling() {
        let defs = agent_llm::tool_defs();
        let def = defs
            .iter()
            .find(|d| d.name == "generate_ai_video_edit")
            .expect("the clip-edit tool is authored");
        let refs = &def.input_schema["properties"]["references"];
        assert!(
            refs.get("maxItems").is_none(),
            "no JSON-schema constraint keyword may ship (agent-llm's schema_strict_guard)"
        );
        let desc = refs["description"].as_str().expect("a description");
        let cap = agent_gen::RUNWAY_V2V_MAX_REFERENCES;
        assert!(
            desc.contains(&format!("at most {cap}")),
            "the prose cap must be the endpoint's MEASURED ceiling ({cap}, probe F-1b), \
             derived here rather than trusted: {desc}"
        );
        // And the code that actually enforces it agrees, at the boundary.
        assert!(crate::video_edit_reference_check(cap + 1).is_err());
    }
}
