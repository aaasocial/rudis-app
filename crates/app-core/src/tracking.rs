//! The `track_object` agent tool (Phase 30, TRK-01/TRK-02) — relocated here
//! from `src-tauri/src/lib.rs` by plan 45-09, together with the two private
//! helpers only it reaches.
//!
//! # What moved, and the closure that says exactly this set
//!
//! One contiguous region: `TrackObjectOutcome`, `subsample_position_keyframes`,
//! `run_track_object`, `handle_track_object` and `format_track_object_result`.
//! Measured in both directions before anything was cut, per this phase's
//! standing rule:
//!
//! * **Outward** — `subsample_position_keyframes` and `format_track_object_result`
//!   have ZERO callers anywhere outside the region; `TrackObjectOutcome` is named
//!   only inside it and by the ONE `src-tauri` test that stayed.
//!   `run_track_object`/`handle_track_object` are reached from `run_agent_turn`'s
//!   `"track_object"` dispatch arm and from that same staying test. Nothing in
//!   `generation.rs`, `native_surface.rs`, `crates/` or any integration test
//!   names any of the five.
//! * **Inward** — everything the region reaches downward was ALREADY resident:
//!   [`crate::compose`]'s `timeline_clip` / `decode_clip_frame` (45-05) and
//!   [`crate::import`]'s `MAX_IMAGE_SEQUENCE_FRAMES` (45-07). Nothing had to be
//!   duplicated and NO new [`crate::AppCtx`] method was needed — the region's
//!   entire host surface is one `emit_changed` call.
//!
//! # This is the first batch to EXERCISE `engine`'s `tracking` feature from here
//!
//! `crates/app-core/Cargo.toml` has declared `engine = { .., features =
//! ["tracking"] }` since 45-03, but nothing in this crate called into it until
//! now. `run_track_object` drives `engine::tracking::track_region` /
//! `smooth_track_path` / `TrackerKind` / `TrackConfidence` — the license-clean
//! OpenCV-contrib-python CSRT/KCF **sidecar subprocess** (PROVENANCE.md Entry
//! 12), a second external-process boundary distinct from the FFmpeg engine
//! sidecar. The whole path compiles and links from `app-core` unchanged.
//!
//! # The conversion — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `fn f<R: tauri::Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` | `fn f<C: AppCtx>(ctx: &C, ..)` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first body line |
//! | `emit_changed(app, &patch, base_seq, seq)` | `ctx.emit_patch(&patch, base_seq, seq)` |
//!
//! Every guard, clamp, cap and validation is byte-identical: the T-30-04 finite/
//! in-bounds bbox checks, the T-30-05 frame-count and duration DoS caps, the
//! T-30-06 live-timeline-only clip resolution, the T-N5T-01 smoothness clamp,
//! the SC-4 truncate-at-first-`Lost` rule and the `MAX_KEYFRAMES_PER_TRACK`
//! subsample. Two of the three `track_object_gate` tests came with them (see the
//! bottom of this file); the third could not — it drives four
//! `#[tauri::command]`s and stays in the shell.

use std::path::PathBuf;

use rudis_core::Command;

use crate::compose::{decode_clip_frame, timeline_clip};
use crate::import::MAX_IMAGE_SEQUENCE_FRAMES;
use crate::AppCtx;

/// What `run_track_object` produced (for the echoed tool_result — RESEARCH Open
/// Question 1: inspectability without a second round trip).
pub struct TrackObjectOutcome {
    pub target_clip_id: String,
    /// Keyframes actually written (post-subsample) via `Command::SetKeyframes`.
    pub keyframe_count: usize,
    /// Tracked frames turned into a usable (non-Lost) path before subsampling.
    tracked_frames: usize,
    /// Clip-relative frame span of the written track (PROJECT fps).
    first_frame: u32,
    last_frame: u32,
    /// The track truncated at a hard `Lost` (SC-4: no path written past a loss).
    truncated_on_loss: bool,
    /// Clip-relative frame index (PROJECT fps) where the track truncated on loss.
    truncated_at_frame: Option<u32>,
    /// Timeline timestamp (seconds) of the truncation frame — for a human report.
    truncated_at_seconds: Option<f64>,
    /// Human-readable reason the track stopped (occlusion/clutter/left-frame).
    truncation_reason: Option<String>,
    /// How many retained keys were flagged `LowConfidence` (drift heuristic).
    low_confidence: usize,
    /// The tracker that ran.
    tracker: &'static str,
}

/// Subsample a per-frame position track to at most `cap` keyframes (Pitfall 4 /
/// T-30-09), so `Command::SetKeyframes` never rejects the WHOLE call on a long
/// window. Fixed-stride decimation with `interp: linear` between retained keys,
/// ALWAYS keeping the final key (the motion-path endpoint). The retained count
/// never exceeds `cap`, and no frame number is duplicated (SetKeyframes rejects
/// duplicate frames).
fn subsample_position_keyframes(
    kfs: Vec<rudis_core::Keyframe<(f32, f32)>>,
    cap: usize,
) -> Vec<rudis_core::Keyframe<(f32, f32)>> {
    let n = kfs.len();
    if n <= cap || cap < 2 {
        return kfs;
    }
    // Budget cap-1 strided samples + the guaranteed final key ⇒ ≤ cap total.
    let budget = cap - 1;
    let stride = n.div_ceil(budget).max(1);
    let last = kfs[n - 1];
    let mut out: Vec<rudis_core::Keyframe<(f32, f32)>> = kfs
        .iter()
        .step_by(stride)
        .filter(|k| k.frame != last.frame)
        .copied()
        .collect();
    out.push(last);
    out
}

/// Run `track_object`: validate agent input BEFORE any decode/track spawn,
/// decode the SOURCE clip per project-fps tick, run the license-clean opencv
/// CSRT/KCF tracker, convert each pixel bbox to the normalized TOP-LEFT position
/// convention (center-tracking), subsample under `MAX_KEYFRAMES_PER_TRACK`, and
/// compose the EXISTING `Command::SetKeyframes(Position)` on the target overlay
/// clip under the ALREADY-open agent turn (one turn = one undo). ZERO new
/// Command variants.
pub fn run_track_object<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<TrackObjectOutcome, String> {
    // Plan 45-09 (45-06's recipe, unchanged): the `store: &SharedStore`
    // parameter became `ctx.store()`, bound on the first body line, so every
    // `store.lock()` below is byte-identical to the pre-move code. At every
    // call site the ctx is built from the SAME `&SharedStore` the old call
    // passed as its second argument.
    let store = ctx.store();

    // 1. Parse + VALIDATE everything BEFORE any decode/track spawn — never trust
    //    agent numbers at the native OpenCV FFI boundary (T-30-04 crash guard).
    let clip_id = input
        .get("clipId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "clipId is required".to_string())?
        .to_string();
    let target_clip_id = input
        .get("targetClipId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "targetClipId is required".to_string())?
        .to_string();

    let bbox = input
        .get("initialBbox")
        .ok_or_else(|| "initialBbox is required".to_string())?;
    let getf = |k: &str| -> Result<f64, String> {
        bbox.get(k)
            .and_then(|v| v.as_f64())
            .ok_or_else(|| format!("initialBbox.{k} is required (a number)"))
    };
    let (nx, ny, nw, nh) = (getf("x")?, getf("y")?, getf("w")?, getf("h")?);
    for (name, v) in [("x", nx), ("y", ny), ("w", nw), ("h", nh)] {
        if !v.is_finite() {
            return Err(format!("initialBbox.{name} must be finite (got {v})"));
        }
    }
    if nw <= 0.0 || nh <= 0.0 {
        return Err(format!(
            "initialBbox w and h must be > 0 (got w={nw}, h={nh})"
        ));
    }
    // Normalized 0-1 in-bounds: the box must lie fully within the source frame.
    if nx < 0.0 || ny < 0.0 || nx + nw > 1.0 || ny + nh > 1.0 {
        return Err(format!(
            "initialBbox must lie within the normalized 0-1 source frame \
             (need 0<=x, 0<=y, x+w<=1, y+h<=1; got x={nx}, y={ny}, w={nw}, h={nh})"
        ));
    }

    let start_frame = input
        .get("startFrame")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "startFrame is required (an integer)".to_string())?;
    let end_frame = input
        .get("endFrame")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "endFrame is required (an integer)".to_string())?;
    if start_frame < 0 {
        return Err(format!("startFrame must be >= 0 (got {start_frame})"));
    }
    if end_frame <= start_frame {
        return Err(format!(
            "endFrame ({end_frame}) must be strictly greater than startFrame ({start_frame})"
        ));
    }
    // Frame-count DoS cap (T-30-05) — fps-independent, so checked HERE before any
    // clip resolution / decode / OpenCV spawn. The finer duration (seconds) cap is
    // applied once the project fps is known (below).
    if (end_frame - start_frame) as usize > MAX_IMAGE_SEQUENCE_FRAMES {
        return Err(format!(
            "tracking window {} frames exceeds the {MAX_IMAGE_SEQUENCE_FRAMES}-frame cap (DoS guard)",
            end_frame - start_frame
        ));
    }
    let (tracker_kind, tracker_name): (engine::tracking::TrackerKind, &'static str) =
        match input.get("tracker").and_then(|v| v.as_str()).unwrap_or("csrt") {
            "csrt" => (engine::tracking::TrackerKind::Csrt, "csrt"),
            "kcf" => (engine::tracking::TrackerKind::Kcf, "kcf"),
            other => {
                return Err(format!("unknown tracker: {other} (expected csrt|kcf)"))
            }
        };
    // Optional moving-average window for the tracked path (default 5). Parsed as
    // u64 and clamped to 0..=31 BEFORE use (T-N5T-01 DoS/panic guard — a wild
    // model value can never trigger unbounded O(n·window) work or an underflow).
    let smoothness = input
        .get("smoothness")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .min(31) as usize;

    // 2. Resolve BOTH clips against the LIVE timeline ONLY (never a raw path —
    //    T-30-06), and read the project fps + source media resolution + the
    //    target overlay's OWN scale (for the center-tracking math), under a
    //    SHORT lock that is then DROPPED before any decode.
    let (fps, src_path, src_rotation, src_is_seq, src_seq_fps, src_in_us, src_out_us, target_scale) = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        let project = guard.snapshot();
        let source = timeline_clip(&project.timeline, &clip_id)
            .ok_or_else(|| format!("source clip not found on the timeline: {clip_id}"))?
            .clone();
        let target = timeline_clip(&project.timeline, &target_clip_id)
            .ok_or_else(|| format!("target clip not found on the timeline: {target_clip_id}"))?
            .clone();
        let media = project
            .media_bin
            .iter()
            .find(|m| m.id == source.media_id)
            .ok_or_else(|| {
                format!(
                    "source clip {clip_id}'s media is missing from the bin: {}",
                    source.media_id
                )
            })?
            .clone();
        (
            project.fps,
            PathBuf::from(&media.path),
            media.rotation_degrees,
            media.is_image_sequence,
            media.fps,
            source.in_us,
            source.out_us,
            target.transform.scale,
        )
    };

    // 3. DoS-cap the REQUESTED analysis window BEFORE any decode/OpenCV spawn
    //    (T-30-05, mirroring export_overlay_asset's pre-spawn cap), then clamp the
    //    window to the source clip's actually-available frames (correctness — so
    //    we never decode past the clip's out point).
    let step_us = engine::frame_step_us(fps).max(1);
    let requested_window = end_frame - start_frame; // > 0 by the check above
    let requested_secs = requested_window as f64 / fps.max(1.0);
    if requested_secs > rudis_core::MAX_SCENE_DURATION_SECONDS {
        return Err(format!(
            "tracking window {requested_secs:.2}s exceeds the {}s cap (DoS guard)",
            rudis_core::MAX_SCENE_DURATION_SECONDS
        ));
    }
    let available = ((src_out_us - src_in_us).max(0)) / step_us; // floor
    let clamped_end = end_frame.min(start_frame + available);
    if clamped_end <= start_frame {
        return Err(format!(
            "the analysis window [{start_frame}, {end_frame}) is entirely past the source clip's \
             available {available} frame(s)"
        ));
    }

    // 4. Decode every project-fps tick in [startFrame, clamped_end) (Pitfall 6 —
    //    decode at PROJECT fps so frame_index maps directly to Keyframe.frame),
    //    then run the tracker on the decoded frames. Both the decode loop and the
    //    subprocess tracker block, so run under block_in_place on a multi-thread
    //    runtime (inline under the tokio-less MockRuntime).
    let analyze = || -> Result<(Vec<engine::tracking::TrackResult>, f32, f32), String> {
        let mut frames: Vec<engine::Frame> = Vec::with_capacity((clamped_end - start_frame) as usize);
        let mut f = start_frame;
        while f < clamped_end {
            let source_us = src_in_us + f * step_us;
            let frame =
                decode_clip_frame(&src_path, source_us, src_rotation, src_is_seq, src_seq_fps)
                    .map_err(|e| format!("decode source clip at frame {f} ({source_us}us): {e}"))?;
            frames.push(frame);
            f += 1;
        }
        if frames.is_empty() {
            return Err("the analysis window decoded no frames".to_string());
        }
        // Convert the normalized bbox to SOURCE pixels using frames[0]'s OWN dims
        // (Pitfall 5 — never the canvas dims).
        let (src_w, src_h) = (frames[0].width as f32, frames[0].height as f32);
        let bbox_px = (
            nx as f32 * src_w,
            ny as f32 * src_h,
            nw as f32 * src_w,
            nh as f32 * src_h,
        );
        let results = engine::tracking::track_region(&frames, bbox_px, tracker_kind)
            .map_err(|e| format!("track_region failed: {e}"))?;
        Ok((results, src_w, src_h))
    };
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let (results, src_w, src_h) = if on_multi_thread {
        tokio::task::block_in_place(analyze)
    } else {
        analyze()
    }?;

    // 4b. Anti-jitter: moving-average the tracked CENTERS in the ENGINE BEFORE
    //     they become keyframes (kills per-frame CSRT center noise, most visible
    //     on slow motion). Smooth only the NON-Lost prefix: track_region truncates
    //     the path at the first hard Lost and that terminal marker must be left
    //     BYTE-untouched so the SC-4 truncation contract (and the loss frame we
    //     report below) is preserved. A linear path is provably identity, so a
    //     legitimate fast/linear track is unchanged.
    let results: Vec<engine::tracking::TrackResult> = if smoothness > 1 {
        let last_is_lost = results
            .last()
            .map(|r| r.confidence == engine::tracking::TrackConfidence::Lost)
            .unwrap_or(false);
        if last_is_lost {
            let split = results.len() - 1;
            let mut smoothed =
                engine::tracking::smooth_track_path(&results[..split], smoothness);
            smoothed.push(results[split]);
            smoothed
        } else {
            engine::tracking::smooth_track_path(&results, smoothness)
        }
    } else {
        results
    };

    // 5. Map each pixel bbox → the overlay's normalized TOP-LEFT position via
    //    CENTER-tracking (RESEARCH Pattern 2): the bbox center drives the
    //    overlay's position, keeping the overlay centered on the moving point.
    //    SC-4: STOP at the first hard Lost (never write a keyframe at/past the
    //    loss — the path would be wrong there). track_region already truncates
    //    the path at the first Lost, so we simply break on it here.
    let mut keyframes: Vec<rudis_core::Keyframe<(f32, f32)>> = Vec::with_capacity(results.len());
    let mut truncated_on_loss = false;
    let mut truncated_at_frame: Option<u32> = None;
    let mut truncated_at_seconds: Option<f64> = None;
    let mut truncation_reason: Option<String> = None;
    let mut low_confidence = 0usize;
    for r in &results {
        match r.confidence {
            engine::tracking::TrackConfidence::Lost => {
                truncated_on_loss = true;
                // Report WHERE the track stopped: clip-relative frame (PROJECT
                // fps) + timeline seconds + a fixed human reason (T-N5T-04: no
                // path/PII leaked). SC-4: we STILL break — nothing past the loss.
                let frame = (start_frame as u32).saturating_add(r.frame_index);
                truncated_at_frame = Some(frame);
                truncated_at_seconds = Some(frame as f64 / fps.max(1.0));
                truncation_reason = Some(
                    "the tracker could no longer follow the subject (occlusion, heavy clutter, \
                     or it left the frame)"
                        .to_string(),
                );
                break;
            }
            engine::tracking::TrackConfidence::LowConfidence => low_confidence += 1,
            engine::tracking::TrackConfidence::Ok => {}
        }
        let (bx, by, bw, bh) = r.bbox_px;
        let center = ((bx + bw / 2.0) / src_w, (by + bh / 2.0) / src_h);
        // center → TOP-LEFT, keeping the overlay's own normalized scale centered.
        let position = (
            center.0 - target_scale.0 / 2.0,
            center.1 - target_scale.1 / 2.0,
        );
        let frame = (start_frame as u32).saturating_add(r.frame_index);
        keyframes.push(rudis_core::Keyframe {
            frame,
            value: position,
            interp: rudis_core::Interpolation::Linear,
        });
    }
    if keyframes.is_empty() {
        return Err(
            "tracking produced no usable keyframes (the subject was lost immediately or the \
             track was degenerate) — no motion path was written"
                .to_string(),
        );
    }
    let tracked_frames = keyframes.len();

    // 6. Subsample under MAX_KEYFRAMES_PER_TRACK BEFORE dispatching (Pitfall 4),
    //    then compose the EXISTING Command::SetKeyframes(Position) under the
    //    ALREADY-open agent turn (one turn = one undo). ZERO new Command variants.
    let keyframes = subsample_position_keyframes(keyframes, rudis_core::MAX_KEYFRAMES_PER_TRACK);
    let first_frame = keyframes.first().map(|k| k.frame).unwrap_or(0);
    let last_frame = keyframes.last().map(|k| k.frame).unwrap_or(0);
    let keyframe_count = keyframes.len();
    let track = rudis_core::KeyframeTrackData::Position(keyframes);

    let (patch, base_seq, seq) = store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .dispatch(Command::SetKeyframes {
            id: target_clip_id.clone(),
            track,
        })
        .map_err(|e| e.to_string())?;
    ctx.emit_patch(&patch, base_seq, seq)?;

    Ok(TrackObjectOutcome {
        target_clip_id,
        keyframe_count,
        tracked_frames,
        first_frame,
        last_frame,
        truncated_on_loss,
        truncated_at_frame,
        truncated_at_seconds,
        truncation_reason,
        low_confidence,
        tracker: tracker_name,
    })
}

/// Phase 30 (TRK-01/TRK-02): the `track_object` interception. Never panics — a
/// bad bbox/frame-range/id, a decode/track failure, or a lost/degenerate track
/// becomes an `is_error` text tool_result, never a partial mutation. On success
/// it ECHOES the emitted keyframe span + any Lost/LowConfidence flag back to the
/// agent (RESEARCH Open Question 1 — inspectability without a second round trip).
pub fn handle_track_object<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_track_object(ctx, input) {
        Ok(o) => {
            let (msg, is_error) = format_track_object_result(&o);
            result(msg, is_error)
        }
        Err(e) => result(format!("track_object failed: {e}"), Some(true)),
    }
}

/// Build the `track_object` success tool_result message (+ optional is_error) from
/// the outcome. Extracted as a pure helper so the truncation-reporting phrasing
/// (frame + timeline seconds + reason) is unit-testable without a live agent.
///
/// On truncation it reports ONE clear sentence naming the seconds (1 decimal) and
/// clip-relative frame, e.g. "Tracking stopped at 3.2s (frame 96): <reason>." — it
/// does NOT also emit the generic truncation sentence (no double-report).
fn format_track_object_result(o: &TrackObjectOutcome) -> (String, Option<bool>) {
    let mut msg = format!(
        "Tracked the object with {} and wrote {} position keyframe(s) on {} (frames {}..={}), \
         tracked from {} analyzed frame(s).",
        o.tracker, o.keyframe_count, o.target_clip_id, o.first_frame, o.last_frame, o.tracked_frames
    );
    match (o.truncated_at_seconds, o.truncated_at_frame) {
        (Some(secs), Some(frame)) => {
            let reason = o
                .truncation_reason
                .as_deref()
                .unwrap_or("the tracker lost the subject");
            msg.push_str(&format!(
                " Tracking stopped at {secs:.1}s (frame {frame}): {reason}."
            ));
        }
        _ if o.truncated_on_loss => {
            // Truncated but no frame captured (defensive) — keep the generic note.
            msg.push_str(
                " The track was TRUNCATED where the subject was lost/occluded (no keyframes \
                 were written past the loss).",
            );
        }
        _ => {}
    }
    if o.low_confidence > 0 {
        msg.push_str(&format!(
            " {} keyframe(s) were flagged low-confidence (possible drift) — inspect the \
             result and re-track a tighter window if needed.",
            o.low_confidence
        ));
    }
    (msg, None)
}

/// Phase 30 (TRK-01/TRK-02): the two `track_object_gate` tests that PIN moved
/// code and drive no `#[tauri::command]`, migrated here with their subject by
/// plan 45-09.
///
/// The module's third test, `track_object_overlay_pinned_on_export` — the SC-2
/// proof that the overlay stays pinned to the analytically-known subject on the
/// RE-DECODED export, and the only one that actually spawns the OpenCV tracker
/// sidecar — did NOT move: it drives `import_media_blocking`, `dispatch_command`,
/// `place_clip` and `export_timeline`, four `#[tauri::command]`s that stay in the
/// shell until 45-10 at the earliest. It still exercises `run_track_object`
/// through the re-export shim, so nothing about the sidecar path is left unpinned;
/// it is simply pinned from `src-tauri` rather than from here (45-05's rule: a
/// test travels with the function it PINS, and its subject is the whole
/// track-and-export pipeline).
#[cfg(test)]
mod track_object_gate {
    use super::*;
    use crate::test_support::TestAppCtx;

    /// The truncation reporting is a pure string transform of the outcome — it
    /// needs no sidecar/decode. A truncated outcome names the seconds (1 decimal)
    /// and the clip-relative frame; a clean outcome never says "stopped at".
    #[test]
    fn format_track_object_result_reports_truncation() {
        let truncated = TrackObjectOutcome {
            target_clip_id: "ov1".into(),
            keyframe_count: 40,
            tracked_frames: 96,
            first_frame: 0,
            last_frame: 95,
            truncated_on_loss: true,
            truncated_at_frame: Some(96),
            truncated_at_seconds: Some(3.2),
            truncation_reason: Some("the tracker could no longer follow the subject".into()),
            low_confidence: 0,
            tracker: "csrt",
        };
        let (msg, is_error) = format_track_object_result(&truncated);
        assert!(is_error.is_none(), "a truncated-but-valid track is not an error");
        assert!(msg.contains("stopped at 3.2s"), "missing seconds phrasing: {msg}");
        assert!(msg.contains("frame 96"), "missing frame index: {msg}");

        let clean = TrackObjectOutcome {
            target_clip_id: "ov1".into(),
            keyframe_count: 40,
            tracked_frames: 120,
            first_frame: 0,
            last_frame: 119,
            truncated_on_loss: false,
            truncated_at_frame: None,
            truncated_at_seconds: None,
            truncation_reason: None,
            low_confidence: 0,
            tracker: "csrt",
        };
        let (clean_msg, _) = format_track_object_result(&clean);
        assert!(!clean_msg.contains("stopped at"), "clean track must not report a stop: {clean_msg}");
    }

    /// Input guards (T-30-04/05/06): missing/degenerate bbox, endFrame<=startFrame,
    /// out-of-bounds bbox, an over-cap window, and an unknown clip are all
    /// rejected with NO keyframes written (no mutation on reject).
    #[test]
    fn track_object_rejects_bad_input_without_mutating() {
        // `build_app_isolated` existed only to hand this test a managed
        // `SharedStore` + a private `app_data_dir`; `TestAppCtx` supplies both
        // with strictly stronger per-INSTANCE isolation and no `tauri`.
        let ctx = TestAppCtx::new();

        for bad in [
            // both ids missing
            serde_json::json!({ "initialBbox": {"x":0.1,"y":0.1,"w":0.2,"h":0.2}, "startFrame":0, "endFrame":10 }),
            // endFrame <= startFrame
            serde_json::json!({ "clipId":"c","targetClipId":"c","initialBbox":{"x":0.1,"y":0.1,"w":0.2,"h":0.2},"startFrame":10,"endFrame":10 }),
            // out-of-bounds bbox (x+w > 1)
            serde_json::json!({ "clipId":"c","targetClipId":"c","initialBbox":{"x":0.9,"y":0.1,"w":0.5,"h":0.2},"startFrame":0,"endFrame":10 }),
            // degenerate bbox (w <= 0)
            serde_json::json!({ "clipId":"c","targetClipId":"c","initialBbox":{"x":0.1,"y":0.1,"w":0.0,"h":0.2},"startFrame":0,"endFrame":10 }),
            // over-cap window (> MAX_SCENE_DURATION_SECONDS * fps frames)
            serde_json::json!({ "clipId":"c","targetClipId":"c","initialBbox":{"x":0.1,"y":0.1,"w":0.2,"h":0.2},"startFrame":0,"endFrame":100000 }),
            // unknown clip (resolves against live timeline only)
            serde_json::json!({ "clipId":"nope","targetClipId":"nope","initialBbox":{"x":0.1,"y":0.1,"w":0.2,"h":0.2},"startFrame":0,"endFrame":10 }),
        ] {
            assert!(
                run_track_object(&ctx, &bad).is_err(),
                "bad input must be rejected: {bad}"
            );
        }
    }
}
