//! The `sync_audio` agent tool (Phase 27, LIB-04) — relocated here from
//! `src-tauri/src/lib.rs` by plan 45-09.
//!
//! # What moved, and the closure that says exactly this set
//!
//! `SyncAudioOutcome`, `run_sync_audio` and `handle_sync_audio`. The
//! both-directions grep, measured before the cut:
//!
//! * **Outward** — `handle_sync_audio` is reached from `run_agent_turn`'s
//!   `"sync_audio"` dispatch arm and from `src-tauri`'s `sync_audio_gate`;
//!   `run_sync_audio` from that handler and the same gate. `SyncAudioOutcome` is
//!   never named outside the pair (the gate reads its fields off the returned
//!   value), which is why the struct is `pub` here but is NOT on `app_core`'s
//!   re-export list.
//! * **Inward** — the only non-`engine`, non-`rudis_core` leaf it reaches is
//!   [`crate::compose`]'s `timeline_clip`, resident since 45-05. Nothing had to
//!   be duplicated.
//!
//! **No new [`crate::AppCtx`] method was needed** — grepped for `app.path().` in
//! full per 45-08's carry-forward and there is not one hit in the region. Its
//! entire host surface is a single `emit_changed` call.
//!
//! # The conversion — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `fn f<R: tauri::Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` | `fn f<C: AppCtx>(ctx: &C, ..)` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first body line |
//! | `emit_changed(app, &patch, base_seq, seq)` | `ctx.emit_patch(&patch, base_seq, seq)` |
//!
//! Zero logic changed: the T-27-09 `check_sync_window_cap` DoS guard on BOTH
//! clips ahead of either `render_audio_pcm` call, the always-1.0 correlation
//! gain, the T-27-11 below-`SYNC_CONFIDENCE_FLOOR` DECLINE that dispatches
//! nothing, `best_lag_us`'s sign convention, and WR-02's read-the-position-back
//! post-dispatch snapshot are byte-identical.
//!
//! All three `sync_audio_gate` tests stayed in `src-tauri`: every one of them
//! reaches this code through `import_media_blocking` / `place_clip` /
//! `get_snapshot` / `export_timeline`, four `#[tauri::command]`s that do not move
//! in this batch (45-05's rule — a test travels with the function it PINS, and
//! their subject is the whole import-place-sync-export path). They reach
//! `run_sync_audio` / `handle_sync_audio` through the re-export shim.

use rudis_core::Command;

use crate::compose::timeline_clip;
use crate::AppCtx;

/// The outcome of a [`run_sync_audio`] call: whether a real `Command::MoveClip`
/// was dispatched (`moved`), the measured `lag_us` (positive => the target's
/// content lags the reference; per `engine::best_lag_us`'s sign convention) and
/// its correlation `score`, plus the target's resulting `new_start_us` (its
/// UNCHANGED `start_us` when the confidence floor was not met).
pub struct SyncAudioOutcome {
    pub moved: bool,
    pub lag_us: i64,
    pub score: f64,
    pub new_start_us: i64,
}

/// Phase 27 (LIB-04): the `sync_audio` interception body — render each of two
/// clips' CURRENT on-timeline audio via [`engine::render_audio_pcm`] (audio-only
/// `-vn` extraction, VFR-agnostic by construction — Pitfall 4), find the lag that
/// maximizes normalized cross-correlation via Plan 27-01's [`engine::best_lag_us`],
/// and — ONLY above [`engine::SYNC_CONFIDENCE_FLOOR`] — dispatch a real, undoable
/// `Command::MoveClip` on the TARGET clip (the reference never moves). Below the
/// floor it DECLINES: dispatches nothing, the target's `start_us` is untouched,
/// and it reports the measured `score` (DaVinci Resolve's "Based on Waveform"
/// precedent — never force a wrong alignment).
///
/// SIGN CONVENTION (from `best_lag_us`'s own doc): a POSITIVE `lag_us` means the
/// target's content LAGS the reference (the same event appears LATER in the
/// target), so the target moves EARLIER: `new_start_us = target.start_us - lag_us`.
///
/// The T-27-09 DoS window cap ([`engine::check_sync_window_cap`]) is enforced on
/// BOTH clips' `[in_us, out_us)` windows BEFORE either `render_audio_pcm` call, so
/// a pathologically long clip window is rejected before any PCM extraction.
pub fn run_sync_audio<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<SyncAudioOutcome, String> {
    // Plan 45-09 (45-06's recipe, unchanged): the `store: &SharedStore`
    // parameter became `ctx.store()`, bound on the first body line, so all
    // three `store.lock()` calls below are byte-identical to the pre-move code.
    let store = ctx.store();

    let reference_clip_id = input
        .get("referenceClipId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "referenceClipId is required".to_string())?
        .to_string();
    let target_clip_id = input
        .get("targetClipId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "targetClipId is required".to_string())?
        .to_string();

    // Short-locked snapshot (D-07), guard dropped before the PCM/correlate work.
    let project = store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .snapshot();

    let ref_clip = timeline_clip(&project.timeline, &reference_clip_id)
        .cloned()
        .ok_or_else(|| format!("reference clip not found: {reference_clip_id}"))?;
    let target_clip = timeline_clip(&project.timeline, &target_clip_id)
        .cloned()
        .ok_or_else(|| format!("target clip not found: {target_clip_id}"))?;
    let ref_media = project
        .media_bin
        .iter()
        .find(|m| m.id == ref_clip.media_id)
        .ok_or_else(|| format!("reference clip's media not found: {}", ref_clip.media_id))?;
    let target_media = project
        .media_bin
        .iter()
        .find(|m| m.id == target_clip.media_id)
        .ok_or_else(|| format!("target clip's media not found: {}", target_clip.media_id))?;

    // DoS cap BEFORE any render_audio_pcm call (T-27-09).
    engine::check_sync_window_cap(ref_clip.in_us, ref_clip.out_us).map_err(|e| e.to_string())?;
    engine::check_sync_window_cap(target_clip.in_us, target_clip.out_us)
        .map_err(|e| e.to_string())?;

    let ref_path = std::path::PathBuf::from(&ref_media.path);
    let target_path = std::path::PathBuf::from(&target_media.path);

    // Volume ALWAYS 1.0 for correlation -- a clip's authored gain is irrelevant to
    // alignment (the SAME speech event correlates regardless of playback gain).
    let ref_pcm = engine::render_audio_pcm(&ref_path, ref_clip.in_us, ref_clip.out_us, 1.0)
        .map_err(|e| format!("render reference audio: {e}"))?;
    let target_pcm =
        engine::render_audio_pcm(&target_path, target_clip.in_us, target_clip.out_us, 1.0)
            .map_err(|e| format!("render target audio: {e}"))?;
    if ref_pcm.is_empty() || target_pcm.is_empty() {
        return Err("one or both clips have no audio to sync (silent source)".to_string());
    }

    let (lag_us, score) = engine::best_lag_us(&ref_pcm, &target_pcm, engine::AUDIO_SAMPLE_RATE);

    if score < engine::SYNC_CONFIDENCE_FLOOR {
        // DECLINE: no dispatch, the target's start_us is untouched (T-27-11).
        return Ok(SyncAudioOutcome {
            moved: false,
            lag_us,
            score,
            new_start_us: target_clip.start_us,
        });
    }

    let requested_start_us = target_clip.start_us - lag_us;
    let (patch, base_seq, seq) = store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())
        .and_then(|mut g| {
            g.dispatch(Command::MoveClip {
                id: target_clip_id.clone(),
                new_start_us: requested_start_us,
            })
            .map_err(|e| e.to_string())
        })?;
    ctx.emit_patch(&patch, base_seq, seq)?;

    // WR-02: report the clip's ACTUAL post-move start_us (ground truth), NOT the
    // pre-clamp `requested_start_us`. `Command::MoveClip::apply` clamps a negative
    // start to 0 (`clip.start_us = new_start_us.max(0)`), so when the measured lag
    // exceeds the target's current start the requested value goes negative but the
    // clip really lands at 0. Reading the position back from the post-dispatch
    // snapshot keeps the reported outcome (and the agent-facing tool_result text)
    // in lockstep with the real timeline state, regardless of any current or future
    // clamping/validation inside MoveClip.
    let new_start_us = {
        let snap = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?
            .snapshot();
        timeline_clip(&snap.timeline, &target_clip_id)
            .map(|c| c.start_us)
            .unwrap_or(requested_start_us.max(0))
    };

    Ok(SyncAudioOutcome {
        moved: true,
        lag_us,
        score,
        new_start_us,
    })
}

/// Phase 27 (LIB-04): the `sync_audio` interception. Never panics: an unknown
/// clip id, an over-cap window, a silent source, or a render failure becomes an
/// `is_error` text tool_result. A below-floor DECLINE is NOT an error — it is a
/// legitimate, informative outcome (Resolve's own precedent), so it mints a plain
/// (non-error) tool_result reporting the confidence.
pub fn handle_sync_audio<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_sync_audio(ctx, input) {
        Ok(outcome) if outcome.moved => result(
            format!(
                "Synced: moved the target clip to {}us (measured lag {}us, confidence {:.2}).",
                outcome.new_start_us, outcome.lag_us, outcome.score
            ),
            None,
        ),
        Ok(outcome) => result(
            format!(
                "Declined to sync: confidence {:.2} is below the {:.2} floor -- the audio does not \
                 correlate strongly enough to trust an automatic move. Check these clips manually.",
                outcome.score, engine::SYNC_CONFIDENCE_FLOOR
            ),
            None,
        ),
        Err(e) => result(format!("sync_audio failed: {e}"), Some(true)),
    }
}
