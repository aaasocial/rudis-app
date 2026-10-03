//! The ONE export path plus the two Claude-authored/engine-composited asset
//! generators — relocated here from `src-tauri/src/lib.rs` by plan 45-10.
//!
//! # What moved
//!
//! | Item | Visibility here | Why |
//! |---|---|---|
//! | `EXPORT_PROGRESS_EVENT` | `pub` | `TauriAppCtx::export_progress_sink` + `export_gate` still name it |
//! | `ExportPlan` | private | no caller outside this module once its two builders moved |
//! | `build_export_audio_wav` | private | same |
//! | `run_export` | `pub` | `export_timeline`, `export_gate`, `export_project_gate` |
//! | `resolve_export_plan` | private | both callers moved |
//! | `run_export_blocking` | private | both callers moved |
//! | `export_is_single_layer_degenerate` | private | its only test moved with it |
//! | `run_export_project` | private | reached only through `handle_export_project` |
//! | `handle_export_project` | `pub` | `run_agent_turn`'s dispatch arm |
//! | `run_generate_image` / `handle_generate_image` | `pub` | dispatch arm + the 3 gates that stayed |
//! | `run_generate_video` / `handle_generate_video` | `pub` | dispatch arm + the 1 gate that stayed |
//!
//! **`run_export_project`/`handle_export_project` are 45-06's deferred pair.**
//! Deferred item **D-45-06-01** measured that they are neither project
//! management nor leaves — `run_export_project` reaches `resolve_export_plan`,
//! `ExportPlan` and `run_export_blocking`, all three shared with the UI export
//! command `run_export` — and assigned them to this batch, which owns that
//! cluster. They moved here, and the debt is discharged. (45-10's own plan text
//! wrongly asserted they had already moved at 45-06 and that `export_project_gate`
//! covered relocated code; D-45-06-01 records the correction, and the gate was
//! still in `src-tauri` when this batch started.)
//!
//! # The two blockers D-45-06-01 named, and how each was solved
//!
//! 1. **`run_export_blocking` needs an owned `Send + 'static` progress emitter
//!    a borrowed `&impl AppCtx` cannot produce.** Solved by
//!    [`crate::AppCtx::export_progress_sink`], which HANDS OUT the owned sink.
//!    `run_export_blocking` therefore takes `on_progress: engine::ProgressFn`
//!    as a parameter and names no host type at all — it is now pure over
//!    `engine` + `rudis_core`. The closure the Tauri impl returns is literally
//!    the one that stood inside this function before the move.
//! 2. **`run_export` invokes it inside `spawn_blocking`, which needs a `'static`
//!    ctx.** Solved by NOT crossing the thread boundary with a ctx at all:
//!    [`crate::AppCtx::run_blocking`] takes the closure and awaits it, so the
//!    only things that cross are the already-owned `ExportPlan`, the `PathBuf`,
//!    the numbers and the sink. `TauriAppCtx` stays borrowing; no owning variant
//!    was needed.
//!
//! `AppCtx::run_blocking` deliberately keeps `tauri::async_runtime::
//! spawn_blocking` on the HOST side rather than substituting
//! `tokio::task::spawn_blocking` (45-07's D-45-07-01 substitution). That swap
//! would NOT have been behavior-preserving here: Tauri's wrapper carries its own
//! process-global runtime, while tokio's resolves against the ambient one and
//! PANICS when there is none — and `src-tauri`'s `export_project_gate` drives
//! `run_export` under `pollster::block_on`, which enters no runtime.
//!
//! # The both-directions reachability grep, measured before the cut
//!
//! * **Outward** — ZERO code hits outside `src-tauri/src/lib.rs` for any of the
//!   twelve relocated items. Every workspace hit is prose: `crates/app-core/src/
//!   {inspect,project,matte,compose,import}.rs` doc comments, `crates/engine/src/
//!   audio.rs`, `crates/core/src/scene_spec.rs`, `crates/agent-gen/src/
//!   allow_list.rs`, `crates/agent-llm/tests/agent_eval_live.rs`,
//!   `src-tauri/src/{generation,native_surface}.rs` — none is code.
//! * **Inward** — everything was already resident: [`crate::compose`]'s
//!   `timeline_clip` / `rasterize_text_layer` / `decode_clip_frame` /
//!   `engine_alpha_mode` / `black_rgba` (45-05) and `render_scene_frame` (45-09),
//!   and [`crate::import`]'s `next_id` / `ID_SEQ` / `poster_cache_dir` (45-07).
//!   `render_scene_frame` is the leaf 45-09 predicted this batch would need; it
//!   is REUSED from `compose`, never duplicated.
//! * **`include_str!`** (45-09's carry-forward, a second invisible reachability
//!   direction) — `crates/engine/tests/overlay_asset_export.rs` reads
//!   `src-tauri/src/lib.rs` textually to pin the frozen `VideoEncoder::new(`
//!   count. This batch carries BOTH of `src-tauri`'s two remaining sites into
//!   this file, so `APP_LAYER` was EXTENDED (`src-tauri` 2 -> 0, this file 0 ->
//!   2) and the total is still asserted to be exactly 3. The count was never
//!   lowered: it is CLAUDE.md's business-critical no-GPL-encoder rule made
//!   re-runnable.
//!
//! # The host-surface grep (45-08's carry-forward), run in full
//!
//! | Region | `app.path().` | `.state::<` | `AgentSession` | `generation::` |
//! |---|---|---|---|---|
//! | `export_project` (2657-2741) | 1 (`app_data_dir`) | 0 | 0 | 0 |
//! | export block (3914-4006, 4053-4648) | 0 | 0 | 0 | 0 |
//! | `generate_image` (4665-4845) | 1 (`app_data_dir`) | 0 | 0 | 0 |
//! | `generate_video` (5385-5635) | 1 (`app_data_dir`) | 0 | 0 | 0 |
//!
//! All three resolve to `AppCtx::app_data_dir`, whose baked-in message
//! (`"resolve app data dir: {e}"`) is byte-identical to the string each call
//! site built inline. The two NEW trait methods
//! ([`crate::AppCtx::export_progress_sink`], [`crate::AppCtx::run_blocking`])
//! exist for the emitter and the blocking pool, not for a directory.
//!
//! # The conversion — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `fn f<R: tauri::Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` | `fn f<C: AppCtx>(ctx: &C, ..)` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first body line |
//! | `app.path().app_data_dir().map_err(..)` | `ctx.app_data_dir()?` |
//! | `poster_cache_dir(&TauriAppCtx::new(app, store))` | `poster_cache_dir(ctx)` |
//! | `emit_changed(app, &patch, base_seq, seq)` | `ctx.emit_patch(&patch, base_seq, seq)` |
//! | `app.emit(EXPORT_PROGRESS_EVENT, pct)` | a sink from `ctx.export_progress_sink()` |
//! | `tauri::async_runtime::spawn_blocking(f).await` | `ctx.run_blocking(f).await` |
//!
//! Zero logic changed. Not one line of the D-07 defaulting, the resolve-then-
//! drop-the-lock discipline, the COMP-02 single-layer degenerate detection, the
//! streaming fast-path, the multi-layer composite loop, the keyframe sampling,
//! the audio sum-mix envelope, the T-24-10/T-24-17 server-built filenames, the
//! T-24-11 closed-`SceneSpec` parse, the silent-WAV synthesis, the IN-01
//! orphan-file cleanups or the `AddMediaBinItem` dispatch differs.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use rudis_core::{Command, MediaBinItem, MediaKind};

use crate::compose::{
    black_rgba, decode_clip_frame, engine_alpha_mode, rasterize_text_layer, render_scene_frame,
    timeline_clip,
};
use crate::import::{next_id, poster_cache_dir, ID_SEQ};
use crate::{AppCtx, SharedStore};

/// Event emitted repeatedly during `export_timeline` with a percentage in
/// `[0.0, 100.0]` — monotonically increasing, ending at (approximately) 100.
pub const EXPORT_PROGRESS_EVENT: &str = "export:progress";

/// Snapshot of everything `export_timeline`'s blocking work needs, resolved
/// under the store lock and then carried across the `spawn_blocking` thread
/// boundary WITHOUT holding the lock (mirrors the `preview_timeline_at` /
/// `clip_audio_rms` pattern: resolve-then-drop-the-lock before slow I/O).
struct ExportPlan {
    timeline: rudis_core::Timeline,
    /// media_id -> (path, rotation_degrees, is_image_sequence, fps),
    /// pre-resolved for every media item the timeline could reference (avoids
    /// re-locking the store per frame from the blocking thread). Phase 28: the
    /// `is_image_sequence`/`fps` fields let the decode loop branch a numbered
    /// image sequence to `decode_frame_rgba_at_seq` at its project fps.
    media: std::collections::HashMap<String, (PathBuf, u32, bool, f64)>,
    /// PROJECT fps (COMP-01 timebase) — the keyframe sampler's timebase (O-1),
    /// carried SEPARATELY from the encode `fps` (which a caller may override,
    /// D-07). Keyframe `frame` numbers are authored in the project timebase, so
    /// `sample_at` MUST use this value regardless of the chosen encode fps, or
    /// an fps-overridden export would retime the animation (SC-4 drift).
    project_fps: f64,
}

/// SOURCE µs of LEAD-OUT the export mix must add beyond `window`'s own range,
/// so the returned PCM is never SHORTER than the window's expected sample count
/// and the caller can TRUNCATE rather than pad (RT-05). `multi_window` is true
/// when the contributor was staircased into more than one window.
///
/// There are TWO independent reasons a window needs it, and the predicate must
/// cover BOTH (quick task 260730-x2t, WR-02):
///
/// * **an interior seam** — a compressed stream decodes in whole codec frames
///   (1024 samples ≈ 21.3 ms for AAC), so a 100 ms window returns 4064 of the
///   4800 samples asked for and every boundary would hold a silent hole.
///   RE-CONFIRMED on the shipped `N-125907` sidecar 2026-08-08: `-ss 1.0 -t 0.1`
///   returns exactly 4064 of 4800, and at tempo 2.0 a 50 ms window returns 2032
///   of 2400. An input seek rounds the start FORWARD to the next AAC frame
///   boundary, so the loss is position-dependent (zero at `-ss 0.5` and
///   `-ss 2.0`, 736 samples at `-ss 1.0`) and cannot be computed away;
/// * **any time-stretch at all** — a lone CONSTANT-speed window still starts at
///   an arbitrary `in_us`, so it takes the frame-granularity loss above, and
///   `atempo`'s output length only APPROXIMATES the speed integral the expected
///   count is derived from (+0.12% at 2.0x, −0.36% at 0.25x). Measured: a lone
///   tempo-2.0 window over `[1.0 s, 3.0 s)` returns 47088 of 48000.
///
///   **CORRECTION 2026-08-08** (debug session `waveform-aac-priming-trim-short`):
///   this bullet used to read "`atempo`'s own ~36-40 ms head latency, measured on
///   the bundled build (`-t 1.0` at tempo 2.0 returned 46 064 of 48 000
///   samples)". `atempo` has no such latency. That measurement was taken at
///   window start ZERO, where `engine::render_audio_pcm` used to emit a
///   `-ss 0.000000` that discarded a whole 1024-sample AAC frame off the head;
///   at a nonzero start on a frame boundary the same call returns 48000 of 48000.
///   **The predicate does NOT change** — a stretched lone window still needs the
///   lead-out for the two reasons now stated — but the quantity is
///   position-dependent rather than a fixed startup cost, and 260730-x2t's WR-02
///   finding rests on the first bullet rather than the second.
///
/// The original predicate was `windows.len() > 1` alone, which conflates "one
/// window" with "tempo 1.0". `retime_audio_windows` returns exactly ONE window
/// for a `RetimeCurve::Constant`, so every CONSTANT-speed clip's exported audio
/// was ~40 ms short at its tail — audible as a clipped word at a cut, and a
/// hole rather than a tail when another clip follows. Only tempo 1.0 (at the
/// window's own 1:1 length) makes the render call byte-identical to the
/// pre-retime one; that is exactly the LIVE MIXER's predicate
/// (`engine/src/audio.rs`: `tempo == 1.0 && out_len_us == out_us - in_us`), and
/// reusing it here is what stops preview and export stretching the same audio
/// differently.
///
/// Deliberately a NAMED function rather than an inline expression: this exact
/// decision was wrong once, and a named function can be unit-tested
/// deterministically without a decoder in the loop.
fn window_lead_out_us(window: &rudis_core::AudioWindow, multi_window: bool) -> i64 {
    // Exact sentinel comparison, NOT approximate equality: the un-retimed path
    // writes literal `1.0f32` (`retime_audio_windows`'s `one()`), and an
    // epsilon here would let a nearly-1.0 ramp window skip the lead-out and
    // re-open the silent-hole bug.
    #[allow(clippy::float_cmp)]
    let byte_identical_to_pre_retime = !multi_window
        && window.tempo == 1.0
        && window.timeline_len_us == window.out_us - window.in_us;
    if byte_identical_to_pre_retime {
        0
    } else {
        engine::audio_lead_out_us(window.tempo)
    }
}

/// Build the full-duration composited audio mix (Phase 7 CONTEXT: sum-mix
/// every [`rudis_core::AudioContributor`] at its timeline position) and write
/// it as a temp wav. Runs on the calling (blocking) thread — real sidecar
/// audio renders per contributor.
fn build_export_audio_wav(
    plan: &ExportPlan,
    duration_us: i64,
    wav_path: &Path,
) -> Result<(), String> {
    let total_samples = ((duration_us.max(0) as f64 / 1_000_000.0)
        * engine::AUDIO_SAMPLE_RATE as f64)
        .ceil() as usize;
    let mut mix = vec![0.0f32; total_samples];

    for contributor in plan.timeline.audio_contributors() {
        let Some((path, _rotation, _is_seq, _fps)) = plan.media.get(&contributor.media_id) else {
            continue; // dangling media reference: contributes silence, not a hard failure
        };
        // Phase 19 (COMP-04): a NON-EMPTY volume track OVERRIDES the static
        // gain with a per-sample envelope (D-06, exactly as `sample_at`
        // overrides the visual static fields) — render at UNITY, then multiply
        // each sample by the sampler's value at that sample's clip-relative
        // time. An EMPTY track leaves the pre-19 path BYTE-UNCHANGED: render at
        // the static volume and plain sum-mix.
        let animated = !contributor.volume_keyframes.is_empty();
        let render_gain = if animated { 1.0 } else { contributor.volume };
        // Retime (quick task 260730-x2t, RT-05): the contributor is split into
        // CONSTANT-TEMPO windows by the ONE core segmenter both export and
        // preview call. An un-retimed (or constant-speed) contributor yields
        // exactly ONE window covering `[in_us, out_us)` at its own tempo, so
        // the un-retimed path below is byte-identical to pre-retime.
        let windows = rudis_core::retime_audio_windows(&contributor);
        let windowed = windows.len() > 1;
        for window in &windows {
            let lead = window_lead_out_us(window, windowed);
            let pcm = engine::render_audio_pcm_retimed(
                path,
                window.in_us,
                window.out_us + lead,
                render_gain,
                window.tempo,
            )
            .map_err(|e| {
                format!(
                    "render audio for clip {} ({}): {e}",
                    contributor.clip_id,
                    path.display()
                )
            })?;
            if pcm.is_empty() {
                continue; // no audio stream: silence, not an error (twin of clip_audio_rms)
            }
            let start_sample = ((window.timeline_start_us.max(0) as f64 / 1_000_000.0)
                * engine::AUDIO_SAMPLE_RATE as f64)
                .round() as usize;
            // TRUNCATE to the window's EXACT expected sample count (RT-05):
            // `atempo`'s output length is only approximate (+0.12% at 2.0x,
            // −0.36% at 0.25x), so trusting `pcm.len()` would let every window's
            // error accumulate into audible drift against the picture. The
            // expected count comes from the speed INTEGRAL, via
            // `window.timeline_len_us`. Short renders are NOT padded here —
            // a shortfall only happens at a true clip tail, where the mix
            // buffer is already zero.
            let expect = ((window.timeline_len_us.max(0) as f64 / 1_000_000.0)
                * engine::AUDIO_SAMPLE_RATE as f64)
                .round() as usize;
            let take = pcm.len().min(expect);
            for (i, &s) in pcm.iter().take(take).enumerate() {
                let idx = start_sample + i;
                if idx >= mix.len() {
                    break;
                }
                if animated {
                    // The volume envelope samples at CLIP-relative time. Under
                    // retime the clip-relative time of sample `i` within THIS
                    // window is measured from the WINDOW's timeline offset —
                    // never from the raw sample index, which was only valid
                    // because `window.timeline_start_us == contributor.start_us`
                    // in the 1:1 case. An un-retimed contributor yields one
                    // window whose offset is 0, so this collapses to exactly
                    // today's `i * 1e6 / rate`.
                    //
                    // Sample with the ONE shared core sampler at the PROJECT
                    // fps (O-1) — same interpolation as the visual export path,
                    // never a second implementation. The `unwrap_or` is
                    // unreachable given a non-empty track, but keeps the D-06
                    // static fallback literal.
                    let clip_rel_us = (window.timeline_start_us - contributor.start_us)
                        + i as i64 * 1_000_000 / engine::AUDIO_SAMPLE_RATE as i64;
                    let g = rudis_core::sample_scalar_track(
                        &contributor.volume_keyframes,
                        clip_rel_us,
                        plan.project_fps,
                    )
                    .unwrap_or(contributor.volume);
                    mix[idx] += s * g;
                } else {
                    mix[idx] += s;
                }
            }
        }
    }

    engine::write_wav_mono_f32(&mix, wav_path).map_err(|e| e.to_string())
}

/// The ONE export path (Phase 17-04, SC-4): validate params, resolve the
/// [`ExportPlan`] under a short store lock, then run the decode/encode loop on a
/// blocking thread and emit the terminal `export:progress`. BOTH the
/// `export_timeline` command (UI export) and the agent's `export_project`
/// interception call this — there is no second export path, so what you preview
/// is what you export regardless of who triggered it.
///
/// Takes `store: &SharedStore` (not `State`) so the in-app agent interception —
/// which holds `&SharedStore`, not a Tauri `State` — can call it directly.
///
/// `width`/`height`/`fps` are `Option`s (Phase 18, D-07): `None` means "use
/// the project's own settings" (`Project.width/height/fps`, the COMP-01
/// timebase); `Some` is an explicit caller override. Resolution happens in
/// [`resolve_export_plan`] under the same short store lock that snapshots the
/// timeline, so the settings and the timeline are read atomically.
pub async fn run_export<C: AppCtx>(
    ctx: &C,
    out_path: PathBuf,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
) -> Result<String, String> {
    let (plan, duration_us, width, height, fps) = resolve_export_plan(ctx.store(), width, height, fps)?;

    // Plan 45-10: `AppCtx::run_blocking` IS `tauri::async_runtime::
    // spawn_blocking` on the Tauri host — the primitive stays host-side (45-07's
    // `block_on` trick) because `tokio::task::spawn_blocking` panics without an
    // ambient runtime and `export_project_gate` drives this under
    // `pollster::block_on`. The `{e}` text is wrapped HERE, so the surfaced
    // message is byte-identical to the pre-move `tauri::Error` rendering.
    let on_progress = ctx.export_progress_sink();
    let mut final_progress = ctx.export_progress_sink();
    let final_path = ctx
        .run_blocking(move || {
            run_export_blocking(on_progress, plan, duration_us, out_path, width, height, fps)
        })
        .await
        .map_err(|e| format!("export task panicked: {e}"))??;

    // Final progress guarantee: ensure a >=100% event is observed even if the
    // encoder's `-progress` stream under-reports on a very short export.
    final_progress(100.0);
    Ok(final_path.to_string_lossy().into_owned())
}

/// Validate the export params and resolve the owned [`ExportPlan`] (+ its
/// duration) under a SHORT store lock, dropping the guard before any slow work —
/// the shared front half of the ONE export path, called by BOTH `run_export`
/// (async, `spawn_blocking`) and the synchronous `export_project` interception.
///
/// Phase 18 (COMP-01, D-07): `width`/`height`/`fps` DEFAULT to the project's
/// own settings (`Project.width/height/fps`) when `None` — the project
/// timebase is the authoritative output shape; explicit `Some` args override
/// it. Returns the RESOLVED `(width, height, fps)` alongside the plan so every
/// caller encodes with exactly the values the plan was built against.
fn resolve_export_plan(
    store: &SharedStore,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
) -> Result<(ExportPlan, i64, u32, u32, f64), String> {
    // Resolve everything the blocking work needs, then DROP the lock before
    // the (comparatively very slow) decode/encode loop — same pattern as
    // preview_timeline_at / clip_audio_rms.
    let (plan, width, height, fps) = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        let project = guard.snapshot();
        // D-07 defaulting: absent args resolve to the project settings read
        // under the SAME lock as the timeline snapshot (atomic pair).
        let width = width.unwrap_or(project.width);
        let height = height.unwrap_or(project.height);
        let fps = fps.unwrap_or(project.fps);
        let timeline = guard.timeline().clone();
        let media_ids: std::collections::HashSet<String> = timeline
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(|c| c.media_id.clone())
            .collect();
        let mut media = std::collections::HashMap::new();
        for id in media_ids {
            if let Some(item) = guard.media_item(&id) {
                media.insert(
                    id,
                    (
                        PathBuf::from(&item.path),
                        item.rotation_degrees,
                        item.is_image_sequence,
                        item.fps,
                    ),
                );
            }
        }
        (
            ExportPlan {
                timeline,
                media,
                // PROJECT timebase for the keyframe sampler (O-1) — always the
                // project's own fps, NOT the (possibly overridden) encode fps.
                project_fps: project.fps,
            },
            width,
            height,
            fps,
        )
    };

    if width == 0 || height == 0 {
        return Err(format!("invalid export resolution {width}x{height}"));
    }
    if !fps.is_finite() || fps <= 0.0 {
        return Err(format!("invalid export fps {fps}"));
    }

    let duration_us = plan.timeline.duration_us();
    if duration_us <= 0 {
        return Err("timeline is empty: nothing to export".to_string());
    }
    Ok((plan, duration_us, width, height, fps))
}

/// Phase 17-04 (SC-4): a FULLY SYNCHRONOUS export from an already-resolved,
/// owned [`ExportPlan`] — the ONE encode path. `run_export` runs this on a
/// blocking thread (`spawn_blocking`) so the UI stays responsive; the agent's
/// `export_project` interception calls it DIRECTLY (it already runs inside the
/// turn's `apply_round` closure, a synchronous context that cannot `.await`, and
/// an agent export is a deliberate foreground action). BOTH paths run this exact
/// code, so an agent export is byte-identical to a UI export from the same
/// timeline. Emits `export:progress` via the encoder's callback; never touches
/// the store (the plan is already owned).
fn run_export_blocking(
    on_progress: engine::ProgressFn,
    plan: ExportPlan,
    duration_us: i64,
    out_path: PathBuf,
    width: u32,
    height: u32,
    fps: f64,
) -> Result<PathBuf, String> {
    // Temp mixed-audio wav in the OS TEMP dir (NOT next to the output, so it
    // never appears beside the user's export); cleaned up unconditionally
    // below. Unique per output stem + pid so concurrent exports don't clash.
    let wav_path = {
        let stem = out_path.file_stem().and_then(|s| s.to_str()).unwrap_or("rudis");
        std::env::temp_dir().join(format!("{stem}-{}.export-audio.wav", std::process::id()))
    };
    build_export_audio_wav(&plan, duration_us, &wav_path)?;

    let cleanup_wav = |wav: &Path| {
        let _ = std::fs::remove_file(wav);
    };

    // Plan 45-10: `on_progress` is now a PARAMETER. It is the identical
    // closure — `AppCtx::export_progress_sink`'s Tauri impl is literally the
    // `app.clone()` + `app.emit(EXPORT_PROGRESS_EVENT, pct)` body that stood
    // here — built by the caller instead, because a borrowed `&impl AppCtx`
    // cannot produce an owned `Send + 'static` emitter (D-45-06-01).
    let mut encoder = match engine::VideoEncoder::new(
        &out_path,
        width,
        height,
        fps,
        &wav_path,
        duration_us,
        Some(on_progress),
    ) {
        Ok(e) => e,
        Err(e) => {
            cleanup_wav(&wav_path);
            return Err(format!("failed to start export encoder: {e}"));
        }
    };

    // Phase 18 (COMP-02, D-02): ONE composite rule for export. Detect the
    // single-layer DEGENERATE case (at every output frame time there is at
    // most one active video layer, and every active layer has identity
    // transform / opacity 1.0 / no crop) and keep the existing
    // `ExportRunDecoder` streaming fast-path for it UNCHANGED — that path's
    // `scale=W:H` stretch and the compositor's contain-fit render the same
    // pixels ONLY in the degenerate case, and it is the throughput +
    // regression-proof path every pre-Phase-18 timeline takes. Anything else
    // (stacked layers, or any non-identity visual property) routes EVERY
    // output frame through `composite_layers_to_rgba` — the SAME multi-layer
    // compositor preview uses, so what you preview is what you export by
    // construction, not by coincidence.
    let step_us = engine::frame_step_us(fps).max(1);
    if export_is_single_layer_degenerate(&plan.timeline, &plan.media, duration_us, step_us) {
        // ------------------------------------------------------------------
        // SINGLE-LAYER FAST-PATH (pre-Phase-18 behavior, byte-unchanged).
        // ------------------------------------------------------------------
        // Build contiguous RUNS across the output timeline — a maximal span of
        // frames whose active clip is the same id (or a gap) — then decode each
        // in ONE streaming ffmpeg pass. Replaces the Phase-7 spawn-ffmpeg-PER-
        // OUTPUT-FRAME path (thousands of process spawns → minutes/hours) with one
        // spawn per clip run. Frame selection is preserved: accurate `-ss` seek +
        // the `fps` filter emit frames at the SAME cadence the per-frame loop
        // seeked to, so export stays WYSIWYG with preview.
        let mut run_bounds: Vec<(i64, i64)> = Vec::new();
        {
            let mut t = 0i64;
            let mut run_key = plan.timeline.top_video_active_at(0).map(|h| h.clip_id);
            let mut run_start = 0i64;
            while t < duration_us {
                let key = plan.timeline.top_video_active_at(t).map(|h| h.clip_id);
                if key != run_key {
                    run_bounds.push((run_start, t));
                    run_key = key;
                    run_start = t;
                }
                t += step_us;
            }
            run_bounds.push((run_start, duration_us));
        }

        // A gap (or missing media) run: push exactly its duration's worth of black
        // frames so the video length still matches the mixed audio.
        let push_black_run =
            |encoder: &mut engine::VideoEncoder, dur_us: i64| -> Result<(), String> {
                let n = ((dur_us.max(0) as f64 / 1_000_000.0) * fps).round() as i64;
                let black = black_rgba(width, height);
                for _ in 0..n {
                    encoder
                        .push_frame(&black)
                        .map_err(|e| format!("push black frame: {e}"))?;
                }
                Ok(())
            };

        for (t0, t1) in run_bounds {
            let dur_us = t1 - t0;
            if dur_us <= 0 {
                continue;
            }
            let run_result = match plan.timeline.top_video_active_at(t0) {
                Some(h) => match plan.media.get(&h.media_id) {
                    // Sequences are never degenerate (see the guard above), so a
                    // clip reaching this streaming fast-path is always ordinary
                    // single-file media.
                    Some((path, rotation, _is_seq, _seq_fps)) => (|| -> Result<(), String> {
                        let decoder = engine::ExportRunDecoder::start(
                            path, h.source_us, dur_us, *rotation, width, height, fps,
                        )
                        .map_err(|e| format!("export decode start {}: {e}", path.display()))?;
                        while let Some(frame) = decoder.next_frame() {
                            encoder
                                .push_frame(&frame.rgba)
                                .map_err(|e| format!("push frame: {e}"))?;
                        }
                        Ok(())
                    })(),
                    None => push_black_run(&mut encoder, dur_us),
                },
                None => push_black_run(&mut encoder, dur_us),
            };
            if let Err(e) = run_result {
                cleanup_wav(&wav_path);
                return Err(e);
            }
        }
    } else {
        // ------------------------------------------------------------------
        // MULTI-LAYER COMPOSITE BRANCH (Phase 18, COMP-02 keystone).
        // ------------------------------------------------------------------
        // For every output frame time t (stepping the PROJECT-fps cadence,
        // Pitfall 5): gather all active video layers (`active_layers_at`,
        // already TRACK-ORDERED index-0 = top, exactly the compositor's
        // contract — never pre-reversed), decode each layer's frame at its
        // resolved source_us (PTS-accurate, rotation-upright, NATIVE source
        // resolution — the compositor contain-fits each source inside its
        // dest rect, NEVER ExportRunDecoder's `scale=W:H` stretch, Pitfall 4),
        // composite through `composite_layers_to_rgba` at the output
        // resolution, and push the RGBA to the unchanged encoder.
        //
        // Throughput (D-09, T-18-04): decoded-frame REUSE. The naive path
        // decoded each layer with `decode_frame_rgba_at` PER FRAME — two
        // process spawns (probe + ffmpeg) per layer per output frame, a spawn
        // storm that measured 1.49 fps on the 3-layer 1080p fixture (the
        // single-layer fast-path hits 45.92 fps precisely by NOT re-spawning).
        // So each active clip keeps ONE persistent sequential decoder
        // (`ExportRunDecoder` at the clip's NATIVE upright resolution — the
        // `scale` is an identity no-op, so the compositor still contain-fits,
        // never a W:H stretch, Pitfall 4) and layers are pulled in output-tick
        // order. The single-layer WYSIWYG gate proves an `ExportRunDecoder`'s
        // frames are byte-WYSIWYG with per-frame seeks, so pixel output is
        // unchanged (SC-2/SC-3 stay green). Concurrent decoders are bounded by
        // the active-layer count (the registered T-18-04 mitigation: bounded +
        // measured, cache wired because the floor wasn't met).
        if let Err(e) = (|| -> Result<(), String> {
            use std::collections::{HashMap, HashSet};
            let compositor = engine::Compositor::new()
                .map_err(|e| format!("start export compositor: {e}"))?;

            /// One clip's open sequential decoder + the source_us its next
            /// `next_frame()` will land on (advances one output step per pull).
            struct SeqLayer {
                decoder: engine::ExportRunDecoder,
                next_source_us: i64,
            }
            // Native UPRIGHT dims per media, probed at most once.
            let mut dims: HashMap<String, (u32, u32)> = HashMap::new();
            let mut seq: HashMap<String, SeqLayer> = HashMap::new();
            // ONE text rasterizer per export run (Phase 20, TEXT-01): shaping +
            // glyph caches are hot, and text content is stable across frames, so
            // the same rasterizer serves every text clip at every tick (a
            // per-clip raster cache is unnecessary here). Constructed even when
            // no text clip is present — `TextRasterizer::new` only builds the
            // bundled-Inter FontSystem (no OS scan), cheap and offline.
            let mut text_rasterizer = engine::TextRasterizer::new();

            let mut t = 0i64;
            while t < duration_us {
                let hits = plan.timeline.active_layers_at(t);
                // Drop decoders for clips not active this tick — frees the
                // ffmpeg child; a re-activation after a gap restarts cleanly.
                let active_ids: HashSet<&String> = hits.iter().map(|h| &h.clip_id).collect();
                seq.retain(|id, _| active_ids.contains(id));

                let mut layers: Vec<engine::Layer> = Vec::with_capacity(hits.len());
                for hit in &hits {
                    let Some(clip) = timeline_clip(&plan.timeline, &hit.clip_id) else {
                        continue; // unreachable: the hit came from this timeline
                    };

                    // Phase 20 (TEXT-01): a TEXT clip (media_id is the empty
                    // sentinel) rasterizes its payload into a straight-alpha
                    // layer via the ONE shared helper — the SAME helper both
                    // preview present paths call, so preview == export — and
                    // skips the media `dims`/`seq`/`ExportRunDecoder` machinery
                    // entirely. `sample_at` still runs so keyframed titles
                    // animate identically to the preview.
                    if let Some(text) = &clip.text {
                        let sampled = clip.sample_at(t - clip.start_us, plan.project_fps);
                        // Map sampled → engine types the SAME field-for-field
                        // way the media branch and the preview `LayerSpec` do,
                        // so the shared helper receives byte-identical inputs.
                        layers.push(rasterize_text_layer(
                            &mut text_rasterizer,
                            text,
                            engine::LayerTransform {
                                position: sampled.transform.position,
                                scale: sampled.transform.scale,
                                rotation_deg: sampled.transform.rotation_deg,
                            },
                            sampled.opacity,
                            engine::LayerCrop {
                                left: sampled.crop.left,
                                top: sampled.crop.top,
                                right: sampled.crop.right,
                                bottom: sampled.crop.bottom,
                            },
                            width,
                            height,
                        ));
                        continue;
                    }

                    // Missing media (imported file since removed from the bin
                    // map) contributes nothing — black shows through, the
                    // multi-layer twin of the fast-path's black run.
                    let Some((path, rotation, is_seq, seq_fps)) = plan.media.get(&hit.media_id)
                    else {
                        continue;
                    };

                    // Native upright dims (cached probe) → identity `scale`.
                    let (nw, nh) = match dims.get(&hit.media_id) {
                        Some(d) => *d,
                        None => {
                            let info = engine::probe(path)
                                .map_err(|e| format!("probe layer {}: {e}", path.display()))?;
                            let d = match rotation % 360 {
                                90 | 270 => (info.height, info.width),
                                _ => (info.width, info.height),
                            };
                            dims.insert(hit.media_id.clone(), d);
                            d
                        }
                    };

                    // Phase 28 (OVL-01, SC-4): a numbered image sequence never
                    // streams (image2 `%0Nd` needs `-framerate` + per-frame
                    // timing) — decode the exact frame at project-fps timing via
                    // the ONE shared `decode_clip_frame` helper (the SAME the
                    // preview/inspect path uses → WYSIWYG). The compositor
                    // contain-fits its native resolution below, exactly like any
                    // other layer.
                    let frame = if *is_seq {
                        decode_clip_frame(path, hit.source_us, *rotation, true, *seq_fps).map_err(
                            |e| {
                                format!(
                                    "export decode sequence layer {} ({}) at {}us: {e}",
                                    hit.clip_id,
                                    path.display(),
                                    hit.source_us
                                )
                            },
                        )?
                    } else {
                        // Retime cadence (quick task 260730-x2t).
                        //
                        // A CONSTANT-speed clip streams perfectly well: the
                        // decoder already takes an `out_fps` it applies as an
                        // `fps={out_fps}` filter (ffmpeg.rs), so asking for
                        // `fps * speed` makes the sidecar emit source frames at
                        // exactly the retimed cadence — the retime lever for
                        // FREE, with ZERO new ffmpeg args.
                        //
                        // A RAMP has no constant cadence, so no single `fps=`
                        // value can express it. Those clips take the per-frame
                        // `decode_frame_rgba_at` path below — the SAME pattern
                        // numbered image sequences already use. It decodes real
                        // frames at the exact retimed timestamps (correct by
                        // construction, not a stub) and it is SLOW: one probe +
                        // one ffmpeg spawn per output frame, the 1.49 fps class
                        // of cost documented at the top of this branch. The
                        // measured number is recorded in this task's
                        // `260730-x2t-throughput.md` artifact and PINNED by a
                        // spawn-count assertion, so a regression is loud. The
                        // restart-per-sub-window optimisation is deliberately
                        // NOT attempted here — measure first, optimise with a
                        // number in hand.
                        let ramped = matches!(
                            clip.retime.as_ref().map(|r| &r.curve),
                            Some(rudis_core::RetimeCurve::Ramp(_))
                        );
                        if ramped {
                            engine::decode_frame_rgba_at(path, hit.source_us, *rotation).map_err(
                                |e| {
                                    format!(
                                        "export decode ramped layer {} ({}) at {}us: {e}",
                                        hit.clip_id,
                                        path.display(),
                                        hit.source_us
                                    )
                                },
                            )?
                        } else {
                            // The SOURCE advance this output step represents.
                            // Un-retimed => exactly `step_us`, so the whole
                            // block below is byte-identical to pre-retime.
                            // At 2x it is `2 * step_us`, which against the old
                            // hard-coded `step_us / 2` tolerance would have
                            // judged EVERY tick non-sequential and spawned a
                            // fresh ffmpeg per layer per output frame (SR-3).
                            let rel = t - clip.start_us;
                            let src_step = clip
                                .source_offset_at(rel + step_us)
                                - clip.source_offset_at(rel);
                            // Reuse the clip's decoder when this tick continues
                            // it in order; otherwise (first activation or a
                            // non-sequential jump) (re)start it at this
                            // source_us for the remaining span.
                            let sequential = seq
                                .get(&hit.clip_id)
                                .map(|s| {
                                    (s.next_source_us - hit.source_us).abs()
                                        <= (src_step.abs() / 2).max(1)
                                })
                                .unwrap_or(false);
                            if !sequential {
                                let dur = (clip.out_us - hit.source_us).max(step_us);
                                // Constant speed rides the decoder's OWN
                                // `out_fps` filter — but DIVIDED by the speed,
                                // not multiplied.
                                //
                                // `ExportRunDecoder` applies `fps=N` as a
                                // FILTER over the source timeline and bounds
                                // the run with a SOURCE-domain `-t`. So `N` is
                                // "frames emitted per second of SOURCE", and
                                // consecutive emitted frames are `1/N` seconds
                                // of source apart. Each output tick must
                                // advance source by `step_us * speed`, so
                                // `N = fps / speed`: at 2x, 15 fps of source
                                // sampling, i.e. frames 66.6 ms of source
                                // apart, pulled once per 33.3 ms output frame.
                                //
                                // MEASURED CONSEQUENCE of getting this
                                // backwards (caught by the export gate, not by
                                // a duration check): `fps * speed` produced a
                                // file of exactly the right LENGTH whose frames
                                // were the UN-RETIMED ones — the export gate's
                                // pixel comparison matched the un-retimed
                                // timestamps better than the retimed ones at
                                // every sampled tick. A duration-only assertion
                                // would have shipped it green.
                                let decode_fps = match clip.retime.as_ref().map(|r| &r.curve) {
                                    Some(rudis_core::RetimeCurve::Constant(s)) => {
                                        (fps / (*s as f64).max(f64::MIN_POSITIVE)).max(1e-3)
                                    }
                                    _ => fps,
                                };
                                let decoder = engine::ExportRunDecoder::start(
                                    path,
                                    hit.source_us,
                                    dur,
                                    *rotation,
                                    nw,
                                    nh,
                                    decode_fps,
                                )
                                .map_err(|e| {
                                    format!(
                                        "export layer decoder {} ({}) at {}us: {e}",
                                        hit.clip_id,
                                        path.display(),
                                        hit.source_us
                                    )
                                })?;
                                seq.insert(
                                    hit.clip_id.clone(),
                                    SeqLayer {
                                        decoder,
                                        next_source_us: hit.source_us,
                                    },
                                );
                            }

                            // Pull the next sequential frame; on stream
                            // exhaustion (a clip tail past the last PTS) fall
                            // back to a precise single-frame decode so output
                            // stays exact.
                            let pulled = seq.get_mut(&hit.clip_id).and_then(|s| {
                                let f = s.decoder.next_frame();
                                if f.is_some() {
                                    s.next_source_us = hit.source_us + src_step;
                                }
                                f
                            });
                            match pulled {
                                Some(f) => f,
                                None => {
                                    seq.remove(&hit.clip_id);
                                    engine::decode_frame_rgba_at(path, hit.source_us, *rotation)
                                        .map_err(|e| {
                                            format!(
                                                "export decode layer {} ({}) at {}us: {e}",
                                                hit.clip_id,
                                                path.display(),
                                                hit.source_us
                                            )
                                        })?
                                }
                            }
                        }
                    };

                    // Phase 19 (COMP-04): resolve this clip's CONCRETE visual
                    // properties at this timeline tick through the ONE shared
                    // sampler, passing the PROJECT fps (O-1) — the SAME value
                    // the preview path (`resolve_multilayer`) passes, so a
                    // keyframed export frame equals the preview frame at the
                    // identical timestamp by construction (SC-4). An un-animated
                    // clip samples its static fields verbatim (byte-unchanged
                    // from pre-19). `sampled.volume` is intentionally unused
                    // here — audio is Plan 04; do NOT stub anything.
                    let sampled = clip.sample_at(t - clip.start_us, plan.project_fps);
                    // Field-for-field map: core's ClipTransform/ClipCrop and
                    // engine's LayerTransform/LayerCrop share shape AND
                    // semantics by construction (Plan 18-03).
                    layers.push(engine::Layer {
                        frame,
                        opacity: sampled.opacity,
                        transform: engine::LayerTransform {
                            position: sampled.transform.position,
                            scale: sampled.transform.scale,
                            rotation_deg: sampled.transform.rotation_deg,
                        },
                        crop: engine::LayerCrop {
                            left: sampled.crop.left,
                            top: sampled.crop.top,
                            right: sampled.crop.right,
                            bottom: sampled.crop.bottom,
                        },
                        // Phase 28 (OVL-01): thread THIS clip's alpha
                        // interpretation (core→engine) into the export composite
                        // — the shared shader premultiplies straight sources once
                        // and SKIPS the premultiply for already-premultiplied
                        // ones. A synthetic/opaque clip's default Straight keeps
                        // this bit-identical to pre-Phase-28.
                        alpha_mode: engine_alpha_mode(clip.alpha_mode),
                    });
                }
                let rgba = if layers.is_empty() {
                    // Gap frame: identical bytes to the compositor's
                    // opaque-black empty canvas, without a GPU round-trip.
                    black_rgba(width, height)
                } else {
                    compositor
                        .composite_layers_to_rgba(&layers, width, height)
                        .map_err(|e| format!("composite export frame at {t}us: {e}"))?
                };
                encoder
                    .push_frame(&rgba)
                    .map_err(|e| format!("push composited frame: {e}"))?;
                t += step_us;
            }
            Ok(())
        })() {
            cleanup_wav(&wav_path);
            return Err(e);
        }
    }

    if let Err(e) = encoder.finish() {
        cleanup_wav(&wav_path);
        return Err(format!("export encode failed: {e}"));
    }
    cleanup_wav(&wav_path);
    Ok(out_path)
}

/// Phase 18 (COMP-02): is this whole export the single-layer DEGENERATE case?
///
/// Scans the timeline at every output frame time (the same `step_us` cadence
/// the export loop steps) and returns `true` iff at EVERY time there is at
/// most ONE active video layer AND every active layer's clip has identity
/// visuals (identity transform, opacity 1.0, no crop) AND no keyframe tracks
/// (Phase 19: an animated clip must composite so `sample_at` runs, even when
/// its STATIC visuals are identity — otherwise the streaming fast-path would
/// silently render the un-animated frame). In exactly that case
/// the pre-Phase-18 `ExportRunDecoder` streaming fast-path and the multi-layer
/// compositor render the same content, so the fast-path (one ffmpeg spawn per
/// clip run — the throughput path, and the regression proof that pre-Phase-18
/// timelines export byte-identically) is kept. Any stacked layer or
/// non-identity visual property routes the export through
/// `composite_layers_to_rgba` instead.
///
/// Visual-identity checks are cached per clip id — clips don't change
/// mid-export (the plan owns an immutable snapshot), so each clip is inspected
/// once regardless of how many frames it spans.
fn export_is_single_layer_degenerate(
    timeline: &rudis_core::Timeline,
    media: &std::collections::HashMap<String, (PathBuf, u32, bool, f64)>,
    duration_us: i64,
    step_us: i64,
) -> bool {
    let identity_transform = rudis_core::ClipTransform::default();
    let identity_crop = rudis_core::ClipCrop::default();
    let mut known_identity: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut t = 0i64;
    while t < duration_us {
        let layers = timeline.active_layers_at(t);
        if layers.len() > 1 {
            return false; // genuinely stacked layers: must composite
        }
        if let Some(hit) = layers.first() {
            // Phase 28 (OVL-01): a numbered image sequence must ALWAYS composite
            // — the streaming `ExportRunDecoder` fast-path can't decode a `%0Nd`
            // pattern at project-fps timing, and it would push native-resolution
            // frames straight to the encoder (no contain-fit). Routing it through
            // the composite branch decodes via `decode_frame_rgba_at_seq` and
            // contain-fits it exactly as the preview/inspect path does.
            if media.get(&hit.media_id).map(|m| m.2).unwrap_or(false) {
                return false;
            }
            if !known_identity.contains(&hit.clip_id) {
                match timeline_clip(timeline, &hit.clip_id) {
                    Some(clip)
                        if clip.transform == identity_transform
                            && clip.opacity == 1.0
                            && clip.crop == identity_crop
                            // Phase 19 (COMP-04, T-19-04): an animated clip must
                            // composite so `sample_at` runs — a static-identity
                            // clip carrying a keyframe track is NOT degenerate.
                            && clip.keyframes.is_empty()
                            // Phase 20 (TEXT-01): a TEXT clip must ALWAYS
                            // composite — the streaming `ExportRunDecoder`
                            // fast-path would try to decode its empty-sentinel
                            // media_id. Never degenerate, even with an explicit
                            // identity transform.
                            && clip.text.is_none()
                            // Retime (quick task 260730-x2t, SR-1): a retimed
                            // clip must ALWAYS composite. The streaming
                            // `ExportRunDecoder` fast-path streams source
                            // frames at `fps=out_fps` and pushes them straight
                            // to the encoder — a 1:1 time assumption retime
                            // breaks SILENTLY, producing a wrong-length,
                            // wrong-paced file with NO error at all. Routing
                            // retimed clips through the composite branch is
                            // correct by construction: that branch resolves
                            // every tick through `active_layers_at`, which
                            // goes through `Clip::source_offset_at`.
                            && clip.retime.is_none() =>
                    {
                        known_identity.insert(hit.clip_id.clone());
                    }
                    // Non-identity visuals (or an unresolvable clip id —
                    // impossible, but composite is the safe answer): the
                    // fast-path's stretch/opaque decode would render it wrong.
                    _ => return false,
                }
            }
        }
        t += step_us;
    }
    true
}

/// Phase 17-04 (D-06, TOOL-07, SC-4): the `export_project` interception — export
/// the current timeline to a real file via the SAME encode path the UI export
/// uses, then mint its tool_result. Never panics: any failure becomes an
/// `is_error` tool_result so a failed export never aborts the agent turn.
pub fn handle_export_project<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    // Debug session `export-no-file-written` (2026-08-01): the turn's
    // structural export-outcome channel. The tool_result string below reaches
    // only the MODEL, and nothing forces it to relay the destination verbatim
    // (T-p3q-01) — this Vec is what the Chat panel renders deterministically.
    disclosures: &mut Vec<crate::ExportDisclosure>,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_export_project(ctx) {
        Ok(path) => {
            disclosures.push(crate::ExportDisclosure {
                path: Some(path.display().to_string()),
                error: None,
            });
            result(format!("Exported the project to {}.", path.display()), None)
        }
        Err(e) => {
            disclosures.push(crate::ExportDisclosure {
                path: None,
                error: Some(e.clone()),
            });
            result(format!("Export failed: {e}"), Some(true))
        }
    }
}

/// The synchronous export the `export_project` interception runs (it lives in
/// the turn's SYNC `apply_round` closure, which cannot `.await` — and an agent
/// export is a deliberate foreground action). Reuses the ONE export path
/// (`resolve_export_plan` + `run_export_blocking`), so the produced file is
/// byte-identical to a UI `export_timeline` from the same timeline (SC-4). The
/// output path is SERVER-DERIVED and confined (threat T-17-11): a FIXED
/// `app_data_dir/exports` dir + a server-built `project-{millis}.mp4` name — the
/// `export_project` schema exposes NO path arg, so no LLM value ever reaches the
/// path. Resolution/fps default to the PROJECT's own settings (Phase 18 /
/// COMP-01, D-07: `Project.fps/width/height` — 1920x1080@30 for any project
/// that never changed them, so the pre-Phase-18 behavior is preserved exactly).
fn run_export_project<C: AppCtx>(ctx: &C) -> Result<PathBuf, String> {
    // Plan 45-10 (45-06's recipe): the `store: &SharedStore` parameter became
    // `ctx.store()`, bound on the first body line, so the `resolve_export_plan`
    // call below is byte-identical to the pre-move code.
    let store = ctx.store();
    let dir = ctx.app_data_dir()?.join("exports");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create exports dir: {e}"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // SERVER-BUILT filename ONLY — no LLM input reaches the path (T-17-11).
    let out_path = dir.join(format!("project-{millis}.mp4"));

    let (plan, duration_us, width, height, fps) = resolve_export_plan(store, None, None, None)?;
    // The EXACT encode the UI export runs. This helper is SYNCHRONOUS (the
    // apply_round closure cannot `.await`), but the encode is a potentially
    // multi-minute FFmpeg pass — running it inline would block whichever tokio
    // worker is currently driving the agent-turn future, stalling other async
    // commands (HI-04). `tokio::task::block_in_place` hands the blocking call off
    // to the runtime's blocking pool while staying synchronous from the closure's
    // point of view — the same intent as `run_export`'s `spawn_blocking`.
    //
    // `block_in_place` PANICS unless it runs on a MULTI-THREAD tokio runtime.
    // Tauri's default `async_runtime` (which drives `#[tauri::command] async fn`)
    // is exactly that, so production always takes the offloading branch. The guard
    // falls back to an inline call when there is no such runtime (e.g. the
    // `pollster::block_on`-driven tests, which stand up no tokio runtime at all) —
    // identical output, only the worker-yielding optimization is skipped.
    // Plan 45-10: the owned `Send + 'static` progress emitter a BORROWED
    // `&impl AppCtx` cannot produce — the exact blocker D-45-06-01 named. The
    // sink IS the closure this function used to hand `run_export_blocking` an
    // `AppHandle` to build; only who constructs it changed.
    let on_progress = ctx.export_progress_sink();
    let mut final_progress = ctx.export_progress_sink();
    let run_blocking = move || {
        run_export_blocking(
            on_progress,
            plan,
            duration_us,
            out_path,
            width,
            height,
            fps,
        )
    };
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let written = if on_multi_thread {
        tokio::task::block_in_place(run_blocking)?
    } else {
        run_blocking()?
    };
    final_progress(100.0);
    Ok(written)
}

/// The synchronous `generate_image` interception (Phase 24, ASSET-01): parse the
/// untrusted scene spec, `resolve()`-validate it BEFORE any GPU/file work
/// (T-24-11), render ONE composited frame through [`render_scene_frame`], write
/// it as a real PNG to a SERVER-DERIVED confined path (`app_data_dir/generated`,
/// T-24-10), probe the JUST-WRITTEN file (every MediaBinItem field measured, never
/// guessed), and add it via the SAME undoable `AddMediaBinItem` dispatch every
/// import uses — so the new asset auto-joins the open agent turn (one-turn undo).
pub fn run_generate_image<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<MediaBinItem, String> {
    // Plan 45-10 (45-06's recipe): `store` is bound on the first body line, so
    // every `store.lock()` below stays byte-identical to the pre-move code.
    let store = ctx.store();
    // 1. Project defaults under a SHORT lock (D-07), DROPPED before render work.
    let (default_w, default_h, default_fps) = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        let project = guard.snapshot();
        (project.width, project.height, project.fps)
    };

    // 2. Parse the untrusted JSON into the CLOSED SceneSpec (deny_unknown_fields
    //    rejects any injected `code`/`script`/`eval` field, T-24-11) ...
    let spec: rudis_core::SceneSpec = serde_json::from_value(input.clone())
        .map_err(|e| format!("invalid scene spec: {e}"))?;
    // 3. ... then validate + default-fill + fully resolve (colors parsed, DoS
    //    caps enforced) BEFORE any Compositor/GPU/file resource is allocated.
    let resolved = spec
        .resolve(default_w, default_h, default_fps, /*require_duration*/ false)
        .map_err(|e| e.to_string())?;

    // Confined output dir + SERVER-BUILT filename (the generate_image schema
    // carries no path arg, so no LLM value ever reaches the path — T-24-10).
    let dir = ctx.app_data_dir()?.join("generated");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create generated dir: {e}"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let out_path = dir.join(format!("image-{millis}-{seq}.png"));

    // 4-6. Compositor round-trip + PNG write. HI-04 / WARNING-3: this handler
    //   runs inside run_agent_turn's SYNC apply_round closure on a tokio worker;
    //   wrap the blocking GPU/IO work in the SAME block_in_place guard
    //   run_export_project uses (block_in_place PANICS off a multi-thread runtime,
    //   so fall back to an inline call under the tokio-less MockRuntime tests —
    //   identical output, only the worker-yield optimization is skipped).
    let render_and_write = || -> Result<(), String> {
        let compositor =
            engine::Compositor::new().map_err(|e| format!("start scene compositor: {e}"))?;
        let mut text_rasterizer = engine::TextRasterizer::new();
        // transparent=true: a standalone still PNG never touches a video
        // encoder, so it can freely carry real alpha (debug session
        // preview-overlay-generation-opaque-background, 2026-07-24).
        let rgba = render_scene_frame(&compositor, &mut text_rasterizer, &resolved, 0, true)?;
        let frame = engine::Frame {
            width: resolved.width,
            height: resolved.height,
            rgba,
        };
        engine::write_frame_png(&frame, &out_path).map_err(|e| format!("write generated PNG: {e}"))
    };
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if on_multi_thread {
        tokio::task::block_in_place(render_and_write)?;
    } else {
        render_and_write()?;
    }

    // 7. Probe the JUST-WRITTEN file (measured, never hand-guessed) + extract a
    //    poster (image kind, at t=0 — mirrors import_media's Image branch; a
    //    poster failure downgrades to None, never aborts). IN-01: on a
    //    post-write failure, best-effort remove the orphaned PNG so it never
    //    becomes permanently-untracked disk usage (nothing references it).
    let info = match engine::probe(&out_path) {
        Ok(info) => info,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(format!("probe generated image: {e}"));
        }
    };
    let id = next_id("media");
    // MEDIABIN-STILL-THUMBNAIL: the poster goes to the asset-protocol-
    // allowlisted poster cache dir ($APPCACHE/posters/**) — NOT next to the
    // asset in the un-scoped `generated/` dir, where convertFileSrc URLs are
    // blocked and the MediaBin <img> never loads. Only the poster is served;
    // the asset itself stays in `generated/`. A dir/poster failure downgrades
    // to None, never aborts.
    let poster_path = {
        let written = poster_cache_dir(ctx).and_then(|poster_dir| {
            let poster_out = poster_dir.join(format!("{id}.png"));
            engine::generate_poster(&out_path, &poster_out, 0.0)
                .map(|()| poster_out)
                .map_err(|e| e.to_string())
        });
        match written {
            Ok(poster_out) => Some(poster_out.to_string_lossy().into_owned()),
            Err(e) => {
                eprintln!("generate_image: no poster for {}: {e}", out_path.display());
                None
            }
        }
    };

    // 8. A real, probed MediaBinItem — every dimension/kind field measured from
    //    the actual written file. Pitfall 5: a generated asset is never rotated.
    let item = MediaBinItem {
        id,
        path: out_path.to_string_lossy().into_owned(),
        media_kind: MediaKind::Image,
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
        // Phase 60 (OCCL-01): from the written PNG's OWN probe — a generated
        // image genuinely CAN be RGBA, so this is measured, never assumed.
        reports_alpha: crate::import::probed_alpha(&info),
    };

    // 9. Backend-owned + undoable: the SAME dispatch path import_media uses, so
    //    the new asset auto-joins the open agent turn and emits project:changed.
    //    IN-01: if the dispatch fails (e.g. poisoned mutex) the just-written
    //    PNG + poster are orphaned (no MediaBinItem references them), so
    //    best-effort remove both before propagating the error.
    let dispatched = store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())
        .and_then(|mut g| {
            g.dispatch(Command::AddMediaBinItem(item.clone()))
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

/// Phase 24 (ASSET-01): the `generate_image` interception. Never panics: a parse
/// / resolve / render / write failure becomes an `is_error` text tool_result
/// (T-24-11), never a partially-written file (resolve() runs before any write).
pub fn handle_generate_image<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_generate_image(ctx, input) {
        Ok(item) => result(
            format!(
                "Generated a {}x{} image asset ({}), added to the media bin as {}.",
                item.width, item.height, item.path, item.id
            ),
            None,
        ),
        Err(e) => result(format!("generate_image failed: {e}"), Some(true)),
    }
}

/// The synchronous `generate_video` interception (Phase 24, ASSET-02): parse the
/// untrusted scene spec, `resolve()`-validate it (with `require_duration=true` —
/// video needs a length) BEFORE any GPU/encoder/file work, then render N frames
/// through the SHARED [`render_scene_frame`] (the SAME helper `generate_image`
/// uses, called ONCE per tick) and push each through [`engine::VideoEncoder`] —
/// the IDENTICAL hardware/license-safe encoder ([`engine::DEFAULT_VIDEO_ENCODER`])
/// `export_timeline`/`export_project` use (SC-2b, true by construction: this path
/// NEVER sets the dev-only encoder override env var). The finished MP4 is probed,
/// posterized, and added via the SAME undoable `AddMediaBinItem` dispatch every
/// import uses, so the generated video is indistinguishable from any other
/// MediaBinItem to the rest of the app (SC-4).
///
/// Pitfall 2 (research): [`engine::VideoEncoder::new`] UNCONDITIONALLY muxes an
/// audio input (`-i <audio_wav>`), but a scene-spec render has no audio at all —
/// so a zero-filled silent WAV is synthesized via [`engine::write_wav_mono_f32`]
/// and MUST exist on disk BEFORE the encoder is constructed (there is no
/// "audio-less" encoder mode). Regression-proven by
/// `generate_video_silent_audio_does_not_error`.
pub fn run_generate_video<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<MediaBinItem, String> {
    // Plan 45-10 (45-06's recipe): `store` is bound on the first body line, so
    // every `store.lock()` below stays byte-identical to the pre-move code.
    let store = ctx.store();
    // 1. Project defaults under a SHORT lock (D-07), DROPPED before render work —
    //    generated-asset width/height/fps default to the ACTIVE PROJECT's own
    //    settings when omitted (Open Question 1, DECIDED).
    let (default_w, default_h, default_fps) = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        let project = guard.snapshot();
        (project.width, project.height, project.fps)
    };

    // 2. Parse the untrusted JSON into the CLOSED SceneSpec (deny_unknown_fields
    //    rejects any injected `code`/`script`/`eval` field, T-24-01) ...
    let spec: rudis_core::SceneSpec = serde_json::from_value(input.clone())
        .map_err(|e| format!("invalid scene spec: {e}"))?;
    // 3. ... then validate + default-fill + fully resolve BEFORE any
    //    Compositor/GPU/encoder/file resource is allocated. `require_duration` is
    //    TRUE: `durationSeconds` is mandatory for video (a missing/invalid value
    //    is rejected here, per Plan 24-01), and the DoS cap
    //    (MAX_SCENE_DURATION_SECONDS = 30) bounds the per-tick loop below.
    let resolved = spec
        .resolve(default_w, default_h, default_fps, /*require_duration*/ true)
        .map_err(|e| e.to_string())?;

    // Confined output dir + SERVER-BUILT filename (the generate_video schema
    // carries no path arg, so no LLM value ever reaches the path — T-24-17).
    let dir = ctx.app_data_dir()?.join("generated");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create generated dir: {e}"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let out_path = dir.join(format!("video-{millis}-{seq}.mp4"));

    // 4. SILENT AUDIO FIRST (Pitfall 2 / T-24-15): synthesize a zero-filled mono
    //    f32 WAV spanning the whole duration and write it to the OS temp dir
    //    (NOT beside the output) — it MUST exist before VideoEncoder::new, which
    //    always opens an `-i <audio_wav>` input. Unique per pid + seq so
    //    concurrent generations never clash.
    let num_samples =
        ((resolved.duration_us as f64 / 1_000_000.0) * engine::AUDIO_SAMPLE_RATE as f64).ceil()
            as usize;
    let silent = vec![0.0f32; num_samples];
    let wav_path = std::env::temp_dir().join(format!(
        "rudis-genvideo-{millis}-{}-{seq}.wav",
        std::process::id()
    ));
    engine::write_wav_mono_f32(&silent, &wav_path)
        .map_err(|e| format!("write silent audio wav: {e}"))?;

    // 5-6. HI-04 / WARNING-3: VideoEncoder::new + the up-to-30s per-tick
    //   render+encode loop + finish() are a potentially multi-minute BLOCKING
    //   pass running inside run_agent_turn's SYNCHRONOUS apply_round closure on a
    //   tokio worker — run them under the SAME `tokio::task::block_in_place` guard
    //   run_export_project uses (block_in_place PANICS off a multi-thread runtime,
    //   so fall back to an inline call under the tokio-less MockRuntime tests —
    //   identical output, only the worker-yield optimization is skipped). The
    //   encoder name actually used is captured INSIDE the closure (before
    //   finish() consumes the encoder) and returned out of it.
    let render_and_encode = || -> Result<String, String> {
        let mut encoder = engine::VideoEncoder::new(
            &out_path,
            resolved.width,
            resolved.height,
            resolved.fps,
            &wav_path,
            resolved.duration_us,
            None,
        )
        .map_err(|e| format!("start generated-video encoder: {e}"))?;
        // Capture NOW — finish() consumes self. This is ALWAYS a license-safe
        // cleared encoder (SC-2b): run_generate_video never sets the dev-only
        // encoder override, so VideoEncoder::new resolves through the SAME
        // three-rung chokepoint export does.
        //
        // **The assertion was widened at 60-02 (HWENC-01), not weakened.** It
        // used to name `DEFAULT_VIDEO_ENCODER` outright; export now resolves a
        // preference ladder, so that spelling had become an assertion about
        // which GPU the machine has rather than about licensing. The claim it
        // was written to make — "the SAME license-safe encoder export uses" —
        // is now made literally, by comparing against export's own resolution
        // for this canvas.
        let encoder_name_used = encoder.encoder_name().to_string();
        debug_assert!(
            engine::is_nvenc_video_encoder(&encoder_name_used)
                || engine::is_mf_video_encoder(&encoder_name_used),
            "generate_video must encode via a cleared hardware family (CLAUDE.md \
             rule 6), got {encoder_name_used:?}"
        );
        #[cfg(debug_assertions)]
        if let Ok(bins) = engine::locate() {
            let expected =
                engine::export_encoder_preference(&bins, resolved.width, resolved.height)
                    .unwrap_or(engine::DEFAULT_VIDEO_ENCODER);
            debug_assert_eq!(
                encoder_name_used, expected,
                "generate_video must encode via the SAME license-safe encoder export uses"
            );
        }

        let compositor =
            engine::Compositor::new().map_err(|e| format!("start scene compositor: {e}"))?;
        let mut text_rasterizer = engine::TextRasterizer::new();
        let step_us = engine::frame_step_us(resolved.fps).max(1);
        let mut t = 0i64;
        while t < resolved.duration_us {
            // transparent=false (unchanged): generated video is re-encoded to
            // H.264/HEVC via VideoEncoder, which has no alpha channel at all --
            // must stay on the locked D-01 opaque-black clear.
            let rgba = render_scene_frame(&compositor, &mut text_rasterizer, &resolved, t, false)?;
            encoder
                .push_frame(&rgba)
                .map_err(|e| format!("push generated frame at {t}us: {e}"))?;
            t += step_us;
        }
        encoder
            .finish()
            .map_err(|e| format!("finish generated video encode: {e}"))?;
        Ok(encoder_name_used)
    };
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let encode_result = if on_multi_thread {
        tokio::task::block_in_place(render_and_encode)
    } else {
        render_and_encode()
    };
    // Best-effort silent-WAV cleanup (mirrors run_export_blocking's cleanup_wav) —
    // regardless of success/failure, the temp WAV never lingers.
    let _ = std::fs::remove_file(&wav_path);
    // IN-01: a mid-loop encode failure can leave a partially-written .mp4 at
    // out_path — best-effort remove it before propagating the error.
    if let Err(e) = encode_result {
        let _ = std::fs::remove_file(&out_path);
        return Err(e);
    }

    // 7. Probe the JUST-WRITTEN file (every MediaBinItem field measured, never
    //    guessed) + extract a mid-duration poster (mirrors import_media's Video
    //    branch; a poster failure downgrades to None, never aborts). `has_audio`
    //    is TRUSTED from the probe (Open Question 2, DECIDED: do NOT force false).
    //    IN-01: on a probe failure, best-effort remove the orphaned MP4.
    let info = match engine::probe(&out_path) {
        Ok(info) => info,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(format!("probe generated video: {e}"));
        }
    };
    let id = next_id("media");
    // MEDIABIN-STILL-THUMBNAIL: poster into the asset-protocol-allowlisted
    // poster cache dir ($APPCACHE/posters/**), never the un-scoped
    // `generated/` dir — see poster_cache_dir.
    let poster_path = {
        let at_seconds = resolved.duration_us as f64 / 2_000_000.0;
        let written = poster_cache_dir(ctx).and_then(|poster_dir| {
            let poster_out = poster_dir.join(format!("{id}.png"));
            engine::generate_poster(&out_path, &poster_out, at_seconds)
                .map(|()| poster_out)
                .map_err(|e| e.to_string())
        });
        match written {
            Ok(poster_out) => Some(poster_out.to_string_lossy().into_owned()),
            Err(e) => {
                eprintln!("generate_video: no poster for {}: {e}", out_path.display());
                None
            }
        }
    };

    // 8. A real, probed MediaBinItem — Video kind, every dimension/kind field
    //    measured from the actual written file. Pitfall 5: a generated asset is
    //    never rotated.
    let item = MediaBinItem {
        id,
        path: out_path.to_string_lossy().into_owned(),
        media_kind: MediaKind::Video,
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
        // Phase 60 (OCCL-01): from the written MP4's OWN probe.
        reports_alpha: crate::import::probed_alpha(&info),
    };

    // 9. Backend-owned + undoable: the SAME dispatch path import_media uses, so
    //    the new asset auto-joins the open agent turn and emits project:changed.
    //    IN-01: if the dispatch fails (e.g. poisoned mutex) the just-written
    //    MP4 + poster are orphaned (no MediaBinItem references them), so
    //    best-effort remove both before propagating the error.
    let dispatched = store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())
        .and_then(|mut g| {
            g.dispatch(Command::AddMediaBinItem(item.clone()))
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

/// Phase 24 (ASSET-02): the `generate_video` interception. Never panics: a parse
/// / resolve / render / encode failure becomes an `is_error` text tool_result,
/// never a partially-registered asset (resolve() + the whole encode run before
/// the AddMediaBinItem dispatch).
pub fn handle_generate_video<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_generate_video(ctx, input) {
        Ok(item) => result(
            format!(
                "Generated a {}x{} video asset ({}), added to the media bin as {}.",
                item.width, item.height, item.path, item.id
            ),
            None,
        ),
        Err(e) => result(format!("generate_video failed: {e}"), Some(true)),
    }
}

// ---------------------------------------------------------------------------
// Migrated `#[cfg(test)]` gates (plan 45-10).
//
// A test travels with the function it PINS, unless it drives a
// `#[tauri::command]` or `run_agent_turn` that stays in the shell (45-05's
// rule, applied at 45-06, 45-08 and 45-09). Of the four gate modules that
// cover this batch's code, NINE tests moved and NINE stayed:
//
// | Test | Verdict | Why |
// |---|---|---|
// | `export_gate` (44 others) | STAYED | every one drives `export_timeline` / `place_clip` / `dispatch_command` / `import_media_blocking` |
// | `export_gate::animated_identity_clip_is_not_export_degenerate` | MOVED | a pure unit test over `export_is_single_layer_degenerate`; builds a `Timeline` by hand, no app, no command |
// | `export_gate::keyframe_volume_envelope` | MOVED | drives `build_export_audio_wav` directly over a hand-built `ExportPlan`; no app, no command |
// | `export_project_gate` (2) | STAYED | both drive `run_agent_turn` with a scripted `FixtureTransport` |
// | `generate_image_gate` 4 of 7 | MOVED | pure pixel assertions over `run_generate_image` |
// | `generate_image_gate::generate_image_adds_real_media_bin_item` | STAYED | drives `get_snapshot` |
// | `generate_image_gate::generate_image_rejects_invalid_payload_cleanly` | STAYED | drives `get_snapshot` |
// | `generate_image_gate::generate_image_poster_lands_in_allowlisted_poster_cache_dir` | STAYED | pins the TAURI integration: `$APPCACHE/posters/**` is `tauri.conf.json`'s `assetProtocol.scope`, which a temp-dir `TestAppCtx` cannot represent |
// | `generate_video_gate` 3 of 4 | MOVED | pure decoded-pixel / probed-codec assertions |
// | `generated_asset_place_trim_export_roundtrip` | STAYED | drives `place_clip`, `dispatch_command`, `export_timeline` |
//
// Moving `animated_identity_clip_is_not_export_degenerate` and
// `keyframe_volume_envelope` is what lets `ExportPlan` (and its three fields),
// `build_export_audio_wav` and `export_is_single_layer_degenerate` stay PRIVATE
// here: keeping them in `src-tauri` would have forced a `pub struct` with three
// `pub` fields across a crate boundary purely for a test — exactly the
// widening 45-14 has to undo.
//
// The `build_app_isolated`/`MockRuntime` construction became `TestAppCtx`,
// whose per-INSTANCE temp `app_data_dir` is strictly stronger isolation than a
// per-identifier `%APPDATA%\app.rudis.test.*` directory (and immune to
// D-45-05-01's orphaned-file trap, which is exactly that shape).
// ---------------------------------------------------------------------------

/// Phase 19 Plan 04 (COMP-04) + Phase 18 (COMP-02): the two `export_gate` tests
/// that pin THIS module's private helpers rather than the `export_timeline`
/// command — relocated by plan 45-10.
#[cfg(test)]
mod export_gate {
    use super::*;
    use crate::test_support::fixture;

    /// Carried from `src-tauri`'s `export_gate` (which still uses its own copy
    /// for the six tests that stayed): the relative RMS tolerance the volume
    /// envelope asserts against. One line, deliberately duplicated rather than
    /// made `pub` — a test constant, not a production leaf.
    const RMS_REL_TOL: f64 = 0.15;

    /// The local twin of `src-tauri`'s `export_gate::tmp_export_path` (still
    /// used there by 30 tests). Same shape, same pid-scoped directory name.
    fn tmp_export_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rudis-export-gate-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create export tmp dir");
        dir.join(name)
    }

    /// Task 1 (unit, T-19-04): `export_is_single_layer_degenerate` returns
    /// FALSE for a lone clip whose STATIC visuals are all identity but which
    /// carries a keyframe track — it MUST composite so `sample_at` runs — and
    /// TRUE once the track is removed (the pre-19 streaming fast-path preserved
    /// for genuinely static clips). Cheap: no decode/export.
    #[test]
    fn animated_identity_clip_is_not_export_degenerate() {
        use rudis_core::{
            Clip, ClipCrop, ClipTransform, Interpolation, Keyframe, KeyframeTracks, Timeline,
            Track, TrackKind,
        };
        let make = |keyframes: KeyframeTracks| -> Timeline {
            Timeline {
                tracks: vec![
                    Track {
                        kind: TrackKind::Video,
                        clips: vec![Clip {
                            id: "c".into(),
                            media_id: "m".into(),
                            start_us: 0,
                            in_us: 0,
                            out_us: 2_000_000,
                            volume: 1.0,
                            audio_detached: false,
                            transform: ClipTransform::default(),
                            opacity: 1.0,
                            crop: ClipCrop::default(),
                            keyframes,
                            text: None,
                            alpha_mode: Default::default(),
                            retime: None,
                        }],
                    },
                    Track {
                        kind: TrackKind::Audio,
                        clips: vec![],
                    },
                ],
            }
        };
        let step = engine::frame_step_us(30.0).max(1);
        let animated = KeyframeTracks {
            position: vec![
                Keyframe {
                    frame: 0,
                    value: (0.0, 0.0),
                    interp: Interpolation::Smooth,
                },
                Keyframe {
                    frame: 60,
                    value: (0.5, 0.0),
                    interp: Interpolation::Smooth,
                },
            ],
            ..Default::default()
        };
        let no_media = std::collections::HashMap::new();
        let tl_anim = make(animated);
        assert!(
            !export_is_single_layer_degenerate(&tl_anim, &no_media, tl_anim.duration_us(), step),
            "an identity-static but ANIMATED clip must NOT be degenerate — it has to \
             composite so sample_at runs (the SC-4 fast-path trap)"
        );
        let tl_static = make(KeyframeTracks::default());
        assert!(
            export_is_single_layer_degenerate(&tl_static, &no_media, tl_static.duration_us(), step),
            "a genuinely static identity clip (no keyframes) must still take the streaming \
             fast-path (pre-19 regression preserved)"
        );
    }

    /// Task 1 (COMP-04): `build_export_audio_wav` applies a NON-EMPTY volume
    /// track as a per-sample gain envelope (a hold-step 1.0 -> 0.25 quarters
    /// the decoded RMS across the step), and an EMPTY track is BYTE-IDENTICAL
    /// to the pre-19 static-volume sum-mix. Tests the WAV/PCM stage directly
    /// (cheaper than a full A/V export; the decoded-file proof is Task 2).
    #[test]
    fn keyframe_volume_envelope() {
        use rudis_core::{
            Clip, ClipCrop, ClipTransform, Interpolation, Keyframe, KeyframeTracks, Timeline,
            Track, TrackKind,
        };
        let tone = fixture("tone.m4a");
        let media_id = "m-tone".to_string();
        let mut media = std::collections::HashMap::new();
        media.insert(media_id.clone(), (PathBuf::from(&tone), 0u32, false, 0.0));

        let base_clip = |keyframes: KeyframeTracks, volume: f32| Clip {
            id: "c".into(),
            media_id: media_id.clone(),
            start_us: 0,
            in_us: 0,
            out_us: 2_000_000,
            volume,
            audio_detached: false,
            transform: ClipTransform::default(),
            opacity: 1.0,
            crop: ClipCrop::default(),
            keyframes,
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        };
        let make_plan = |clip: Clip| ExportPlan {
            timeline: Timeline {
                tracks: vec![
                    Track { kind: TrackKind::Video, clips: vec![clip] },
                    Track { kind: TrackKind::Audio, clips: vec![] },
                ],
            },
            media: media.clone(),
            project_fps: 30.0,
        };

        // --- Envelope half: hold-step 1.0 -> 0.25 at frame 30 (= 1.0s @30fps).
        let animated = KeyframeTracks {
            volume: vec![
                Keyframe { frame: 0, value: 1.0, interp: Interpolation::Hold },
                Keyframe { frame: 30, value: 0.25, interp: Interpolation::Hold },
            ],
            ..Default::default()
        };
        let plan = make_plan(base_clip(animated, 1.0));
        let wav = tmp_export_path("kf_vol_env.wav");
        build_export_audio_wav(&plan, 2_000_000, &wav).expect("build envelope wav");
        // Windows straddle the hold step (at 1.0s) with margin, avoiding the
        // exact transition sample.
        let before = engine::render_audio_pcm(&wav, 0, 900_000, 1.0).expect("decode [0,0.9s)");
        let after =
            engine::render_audio_pcm(&wav, 1_100_000, 2_000_000, 1.0).expect("decode [1.1s,2s)");
        let (rb, ra) = (engine::rms(&before), engine::rms(&after));
        let ratio = rb / ra.max(1e-12);
        println!("keyframe_volume_envelope: before={rb:.6} after={ra:.6} ratio={ratio:.4}");
        assert!(rb > 0.01, "pre-step window must be audible (RMS {rb})");
        assert!(ra > 0.001, "quarter-gain window must still sound (RMS {ra})");
        assert!(
            (ratio - 4.0).abs() < 4.0 * RMS_REL_TOL,
            "the hold-step 1.0->0.25 envelope must quarter the decoded RMS (~4x, got {ratio:.4})"
        );
        let _ = std::fs::remove_file(&wav);

        // --- Regression half: EMPTY volume track @ static 0.5 must be
        // BYTE-IDENTICAL to the pre-19 render-at-volume + plain sum-mix path.
        let plan_static = make_plan(base_clip(KeyframeTracks::default(), 0.5));
        let wav_static = tmp_export_path("kf_vol_static.wav");
        build_export_audio_wav(&plan_static, 2_000_000, &wav_static).expect("build static wav");

        // Reconstruct the EXACT pre-19 path inline: render at the static volume,
        // sum-mix into a zeroed full-duration buffer, write via the same writer.
        let total =
            ((2_000_000f64 / 1_000_000.0) * engine::AUDIO_SAMPLE_RATE as f64).ceil() as usize;
        let mut expected = vec![0.0f32; total];
        let pcm = engine::render_audio_pcm(Path::new(&tone), 0, 2_000_000, 0.5)
            .expect("pre-19 render @ static 0.5");
        for (i, &s) in pcm.iter().enumerate() {
            if i >= expected.len() {
                break;
            }
            expected[i] += s;
        }
        let ref_wav = tmp_export_path("kf_vol_ref.wav");
        engine::write_wav_mono_f32(&expected, &ref_wav).expect("write pre-19 reference wav");

        let got = std::fs::read(&wav_static).expect("read static wav");
        let want = std::fs::read(&ref_wav).expect("read reference wav");
        assert_eq!(
            got, want,
            "an empty-volume-track export must be byte-identical to the pre-19 \
             static-volume sum-mix (no regression for un-animated clips)"
        );
        let _ = std::fs::remove_file(&wav_static);
        let _ = std::fs::remove_file(&ref_wav);
    }

    // -----------------------------------------------------------------------
    // Quick task 260730-x2t, Task 4: retimed video through the export
    // composite path, and the two silent-ship risks that live here.
    // -----------------------------------------------------------------------

    /// Build a one-video-track timeline holding exactly `clip`.
    fn one_clip_timeline(clip: rudis_core::Clip) -> rudis_core::Timeline {
        use rudis_core::{Timeline, Track, TrackKind};
        Timeline {
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip],
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![],
                },
            ],
        }
    }

    /// A plain identity-visuals media clip.
    fn plain_clip(media_id: &str, in_us: i64, out_us: i64) -> rudis_core::Clip {
        rudis_core::Clip {
            id: "c".into(),
            media_id: media_id.into(),
            start_us: 0,
            in_us,
            out_us,
            volume: 1.0,
            audio_detached: false,
            transform: rudis_core::ClipTransform::default(),
            opacity: 1.0,
            crop: rudis_core::ClipCrop::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        }
    }

    /// Attach a retime with a correctly derived cached occupancy.
    fn retimed(mut clip: rudis_core::Clip, curve: rudis_core::RetimeCurve, fps: f64) -> rudis_core::Clip {
        let span = clip.out_us - clip.in_us;
        clip.retime = Some(rudis_core::Retime {
            timeline_len_us: rudis_core::retimed_timeline_len_us(&curve, fps, span),
            curve,
            timebase_fps: fps,
        });
        clip
    }

    /// **SR-1, BOTH DIRECTIONS.** A retimed clip must never reach the streaming
    /// fast path (which streams source frames at `fps=out_fps` and would emit a
    /// wrong-length, wrong-paced file with NO error), and an un-retimed one must
    /// still take it (the pre-retime behaviour, byte-unchanged).
    #[test]
    fn retimed_clip_is_never_export_degenerate() {
        let step = engine::frame_step_us(30.0).max(1);
        let no_media = std::collections::HashMap::new();

        let plain = one_clip_timeline(plain_clip("m", 0, 2_000_000));
        assert!(
            export_is_single_layer_degenerate(&plain, &no_media, plain.duration_us(), step),
            "an un-retimed identity clip must STILL take the streaming fast path \
             — the pre-retime behaviour is byte-unchanged"
        );

        for curve in [
            rudis_core::RetimeCurve::Constant(2.0),
            rudis_core::RetimeCurve::Constant(0.5),
            rudis_core::RetimeCurve::Ramp(vec![
                rudis_core::Keyframe {
                    frame: 0,
                    value: 1.0,
                    interp: rudis_core::Interpolation::Smooth,
                },
                rudis_core::Keyframe {
                    frame: 30,
                    value: 0.4,
                    interp: rudis_core::Interpolation::Smooth,
                },
            ]),
        ] {
            let tl = one_clip_timeline(retimed(
                plain_clip("m", 0, 2_000_000),
                curve.clone(),
                30.0,
            ));
            assert!(
                !export_is_single_layer_degenerate(&tl, &no_media, tl.duration_us(), step),
                "a clip carrying {curve:?} MUST composite — the streaming \
                 fast-path's 1:1 time assumption would silently produce a \
                 wrong-length file (SR-1)"
            );
        }
    }

    /// **WR-02: the constant-speed export audio lead-out.**
    ///
    /// `retime_audio_windows` returns exactly ONE window for a
    /// `RetimeCurve::Constant`, so the old `windows.len() > 1` predicate judged
    /// a 2x clip "not windowed" and gave it NO lead-out — yet its tempo is 2.0
    /// and it goes through the full `atempo` chain, whose measured ~36-40 ms
    /// head latency then left the exported tail short (an audibly clipped word
    /// at a cut, and a HOLE rather than a tail when another clip follows).
    ///
    /// Meanwhile the LIVE preview mixer used the correct predicate, so preview
    /// and export were stretching the same audio differently — exactly the
    /// divergence this feature exists to prevent.
    ///
    /// **The deterministic half — this is the one that fails on the pre-fix
    /// predicate on ANY machine.** `window_lead_out_us` is exercised over
    /// windows produced by the real `retime_audio_windows` segmenter, so it
    /// pins the DECISION, not a restatement of it, and needs no decoder.
    #[test]
    fn every_time_stretched_audio_window_asks_for_a_lead_out() {
        use rudis_core::{AudioContributor, Retime, RetimeCurve};

        let contributor = |retime: Option<Retime>| AudioContributor {
            clip_id: "c".into(),
            media_id: "m".into(),
            start_us: 0,
            in_us: 0,
            out_us: 4_000_000,
            volume: 1.0,
            volume_keyframes: Vec::new(),
            retime,
        };
        let with = |curve: RetimeCurve| {
            let span = 4_000_000;
            contributor(Some(Retime {
                timeline_len_us: rudis_core::retimed_timeline_len_us(&curve, 30.0, span),
                curve,
                timebase_fps: 30.0,
            }))
        };

        // 1. UN-RETIMED: one window, tempo exactly 1.0, 1:1 length. NO lead-out
        //    — this is what keeps the pre-retime render call byte-identical.
        let plain = rudis_core::retime_audio_windows(&contributor(None));
        assert_eq!(plain.len(), 1);
        assert_eq!(
            window_lead_out_us(&plain[0], false),
            0,
            "an un-retimed contributor must still ask for exactly its own range"
        );

        // 2. CONSTANT speed: ONE window (so the old `windows.len() > 1` said
        //    "not windowed") that nonetheless goes through the full atempo
        //    chain. THIS is WR-02.
        for speed in [2.0f32, 0.5, 4.0, 0.25] {
            let w = rudis_core::retime_audio_windows(&with(RetimeCurve::Constant(speed)));
            assert_eq!(
                w.len(),
                1,
                "a Constant curve yields exactly ONE window at speed {speed} — \
                 which is precisely why a window-COUNT test cannot detect that \
                 it still stretches"
            );
            assert_eq!(w[0].tempo, speed);
            assert_eq!(
                window_lead_out_us(&w[0], false),
                engine::audio_lead_out_us(speed),
                "WR-02: a constant-{speed}x clip is a SINGLE window and still \
                 time-stretches, so it must get the same lead-out the live \
                 preview mixer gives it — without it the exported tail is \
                 ~40 ms short at every such clip"
            );
        }

        // 3. A RAMP: multi-window, every window gets it (interior seams).
        let ramped = with(RetimeCurve::Ramp(vec![
            rudis_core::Keyframe {
                frame: 0,
                value: 1.0,
                interp: rudis_core::Interpolation::Linear,
            },
            rudis_core::Keyframe {
                frame: 60,
                value: 2.0,
                interp: rudis_core::Interpolation::Linear,
            },
        ]));
        let windows = rudis_core::retime_audio_windows(&ramped);
        assert!(windows.len() > 1, "a ramp must staircase into many windows");
        for w in &windows {
            assert!(
                window_lead_out_us(w, true) > 0,
                "every window of a ramp needs the lead-out (codec-frame \
                 granularity at each seam)"
            );
        }
    }

    /// **The end-to-end half.** Real media, the real `build_export_audio_wav`,
    /// real PCM read back off disk: a constant-speed retimed clip's exported
    /// audio must run all the way to its retimed end.
    ///
    /// NOTE on discriminating power: the head loss is a property of the RESOLVED
    /// ffmpeg build, so this test's separating power varies by machine while
    /// `every_time_stretched_audio_window_asks_for_a_lead_out` above is the
    /// binary-independent gate. This one proves the whole pipeline actually lands
    /// the samples.
    ///
    /// **CORRECTION 2026-08-08** (debug session `waveform-aac-priming-trim-short`):
    /// this note used to attribute the loss to "`atempo`'s head loss", citing
    /// "the bundled/shipped binary exhibits it (measured: `-t 1.0` at tempo 2.0
    /// returns 46 064 of 48 000 samples, and 47 387 for `tone.m4a`)". `atempo` has
    /// no head loss. Those figures were taken at window start ZERO, where
    /// `engine::render_audio_pcm` used to emit a `-ss 0.000000` that discarded a
    /// whole 1024-sample AAC frame off the head on the shipped sidecar; at a
    /// nonzero start on a frame boundary the same call returns 48000 of 48000. The
    /// real, still-present loss is AAC frame granularity at a NONZERO input seek
    /// (measured: 4064 of 4800 for `-ss 1.0 -t 0.1`), which is what
    /// `RETIME_AUDIO_LEAD_OUT_US` covers. Nothing about this test's assertions
    /// changes — only what the note claims causes the shortfall it tolerates.
    #[test]
    fn constant_speed_export_audio_reaches_its_retimed_tail() {
        use rudis_core::{Timeline, Track, TrackKind};

        /// One AAC frame. The review's tolerance: the non-silent audio must
        /// reach within ONE codec frame of the window's exact expected end.
        const AAC_FRAME: usize = 1024;
        /// |sample| at or below this counts as silence.
        const SILENCE: f32 = 1e-4;

        let tone = fixture("tone.m4a");
        let media_id = "m-tone".to_string();
        let mut media = std::collections::HashMap::new();
        media.insert(media_id.clone(), (PathBuf::from(&tone), 0u32, false, 0.0f64));

        // 2 s of source at a CONSTANT 2.0x = exactly 1 s of timeline.
        let mut clip = plain_clip(&media_id, 0, 2_000_000);
        clip.id = "a".into();
        let clip = retimed(clip, rudis_core::RetimeCurve::Constant(2.0), 30.0);
        let timeline = Timeline {
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![],
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![clip],
                },
            ],
        };

        // The shape that made the old predicate wrong, asserted rather than
        // assumed: ONE window, and its tempo is NOT 1.0.
        let contributors = timeline.audio_contributors();
        assert_eq!(contributors.len(), 1);
        let windows = rudis_core::retime_audio_windows(&contributors[0]);
        assert_eq!(
            windows.len(),
            1,
            "a Constant curve yields exactly ONE window — which is why \
             `windows.len() > 1` could never detect that it still stretches"
        );
        assert_eq!(windows[0].tempo, 2.0, "...and that one window's tempo is 2.0");
        assert_eq!(windows[0].timeline_len_us, 1_000_000);

        let wav = tmp_export_path("x2t-const-2x-tail.wav");
        let _ = std::fs::remove_file(&wav);
        build_export_audio_wav(
            &ExportPlan {
                timeline,
                media,
                project_fps: 30.0,
            },
            1_000_000,
            &wav,
        )
        .expect("build the constant-2x export mix");

        let pcm = engine::render_audio_pcm(&wav, 0, 1_000_000, 1.0).expect("read the mix back");
        assert!(
            pcm.len() > 4 * AAC_FRAME,
            "the mix must be a real 1 s buffer, got {} samples",
            pcm.len()
        );

        let head = engine::rms(&pcm[..AAC_FRAME]);
        let tail = engine::rms(&pcm[pcm.len() - AAC_FRAME..]);
        assert!(
            head > 0.01,
            "the fixture must be audible at its head (RMS {head}) or this test \
             proves nothing"
        );
        assert!(
            tail > 0.25 * head,
            "WR-02: the exported tail of a CONSTANT-speed retimed clip is \
             missing. head RMS {head:.6}, final-frame RMS {tail:.6} — atempo's \
             head latency ate the last ~40 ms because the single-window \
             constant-speed render asked for no lead-out"
        );

        let silent_tail = pcm.iter().rev().take_while(|s| s.abs() <= SILENCE).count();
        assert!(
            silent_tail < AAC_FRAME,
            "WR-02: {silent_tail} trailing silent samples ({:.1} ms) — the \
             non-silent audio must reach within ONE codec frame ({AAC_FRAME} \
             samples) of the window's expected end",
            silent_tail as f64 * 1000.0 / engine::AUDIO_SAMPLE_RATE as f64
        );

        let _ = std::fs::remove_file(&wav);
    }

    /// **A/V SYNC, on real exported output.** An untrimmed AAC clip at timeline
    /// zero must land its audio where the picture is — not 42.67 ms early.
    ///
    /// # Why this gate exists (debug session `waveform-aac-priming-trim-short`)
    ///
    /// `engine::render_audio_pcm` emitted `-ss 0.000000` unconditionally, and on
    /// the shipped sidecar an input seek on AAC-in-MP4 discards a whole
    /// 1024-sample frame off the HEAD at position 0. That surfaced as a Timeline
    /// waveform ending 40 ms short, but the waveform was the harmless symptom: the
    /// SAME call renders the export mix here, from `window.in_us == 0` for every
    /// untrimmed clip, so every AAC clip's exported audio ran 42.67 ms ahead of
    /// its own picture. The engine-level gates
    /// (`engine/tests/audio_render.rs::a_render_from_zero_*`) pin the render; this
    /// pins the WHOLE pipeline through `build_export_audio_wav` onto a real file on
    /// disk, which is the claim a listener would actually notice (CLAUDE.md rule 3).
    ///
    /// `speech_delayed_700ms.mp4` is the instrument: its silent lead-in makes the
    /// speech onset an ABSOLUTE landmark needing no reference render. Measured
    /// across all three of this tree's ffmpeg builds, the correct onset is block 89
    /// (890 ms); the defect moves it to block 85 (850 ms).
    ///
    /// Tolerance ±2 blocks (±20 ms) — half the 4-block error the defect produces.
    #[test]
    fn an_untrimmed_aac_clip_exports_its_audio_in_sync_not_40ms_early() {
        use rudis_core::{Timeline, Track, TrackKind};

        const BLOCK: usize = engine::AUDIO_SAMPLE_RATE as usize / 100; // 10 ms
        const FLOOR: f64 = 0.005;
        const EXPECTED_ONSET_BLOCK: i64 = 89; // 890 ms
        const TOL_BLOCKS: i64 = 2;

        let src = fixture("speech_delayed_700ms.mp4");
        let media_id = "m-delayed".to_string();
        let mut media = std::collections::HashMap::new();
        media.insert(
            media_id.clone(),
            (PathBuf::from(&src), 0u32, false, 30.0f64),
        );

        // The whole clip, untrimmed, at timeline zero — `window.in_us == 0`, which
        // is the position the defect lived at.
        let mut clip = plain_clip(&media_id, 0, 4_455_000);
        clip.id = "a".into();
        let timeline = Timeline {
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![],
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![clip],
                },
            ],
        };

        // The shape asserted rather than assumed: ONE window, at tempo 1.0,
        // starting at source zero and at timeline zero.
        let contributors = timeline.audio_contributors();
        assert_eq!(contributors.len(), 1);
        let windows = rudis_core::retime_audio_windows(&contributors[0]);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].in_us, 0, "the window starts at the SOURCE head");
        assert_eq!(windows[0].timeline_start_us, 0);

        let wav = tmp_export_path("aac-zero-offset-sync.wav");
        let _ = std::fs::remove_file(&wav);
        build_export_audio_wav(
            &ExportPlan {
                timeline,
                media,
                project_fps: 30.0,
            },
            4_455_000,
            &wav,
        )
        .expect("build the untrimmed-AAC export mix");

        // Read the REAL file back off disk. It is a WAV, so this read cannot
        // reintroduce the AAC behaviour under test.
        let pcm = engine::render_audio_pcm(&wav, 0, 4_455_000, 1.0).expect("read the mix back");
        let blocks: Vec<f64> = pcm
            .chunks_exact(BLOCK)
            .map(|b| {
                (b.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / b.len() as f64).sqrt()
            })
            .collect();
        let loudest = blocks.iter().copied().fold(0.0f64, f64::max);
        // NON-VACUITY: a silent mix would report onset 0 and assert nothing.
        assert!(
            loudest > FLOOR * 4.0,
            "the exported mix must be audible for an onset to exist: peak block \
             RMS {loudest:.4} against a {FLOOR} floor"
        );
        let onset = blocks
            .iter()
            .position(|&r| r > FLOOR)
            .expect("a loud block exists") as i64;

        println!(
            "EXPORT-SYNC speech_delayed_700ms.mp4 @ timeline 0: onset block {onset} \
             = {} ms (expected {EXPECTED_ONSET_BLOCK} = {} ms), peak block RMS {loudest:.4}",
            onset * 10,
            EXPECTED_ONSET_BLOCK * 10
        );
        assert!(
            (onset - EXPECTED_ONSET_BLOCK).abs() <= TOL_BLOCKS,
            "the exported audio's speech onset is at {} ms instead of ~{} ms. An \
             onset ~40 ms EARLY means the zero-position input seek is back and \
             every untrimmed AAC clip's exported audio leads its picture",
            onset * 10,
            EXPECTED_ONSET_BLOCK * 10
        );

        let _ = std::fs::remove_file(&wav);
    }

    // -----------------------------------------------------------------------
    // SR-3's spawn budget: measured PER THREAD, not process-globally
    // -----------------------------------------------------------------------
    // The two tests below assert a TIGHT ffmpeg-spawn budget around a real
    // export. They originally read `engine::STREAM_SPAWN_COUNT` /
    // `PROBE_SPAWN_COUNT`, which are PROCESS-GLOBAL — and this crate's test
    // binary runs with cargo's default parallelism alongside
    // `generate_image_gate` / `generate_video_gate`, which also spawn
    // ffmpeg/ffprobe. The deltas were therefore polluted by whatever happened
    // to be running, and `cargo test -p app-core` failed DETERMINISTICALLY,
    // every run, regression or not. A check that fails unconditionally cannot
    // distinguish a regression from noise: it is worse than no check, because
    // the next engineer to hit it deletes it.
    //
    // The fix is a per-test-scoped MEASUREMENT, not serialization: the engine
    // now mirrors both counters into thread-locals (`engine::thread_spawn_counts`),
    // and `run_export_blocking` issues every one of its child spawns on the
    // calling thread. The delta is then exactly this export's cost, on any
    // number of threads, with no `--test-threads=1`, no crate-local mutex and
    // no new third-party dependency.
    //
    // The BUDGETS ARE UNCHANGED. Nothing here is loosened to make the tests
    // pass — the point is that they must still FAIL on a real regression.

    /// The mechanism SR-3's budgets now rest on, pinned directly: the
    /// thread-local spawn counters must be genuinely per-thread. If they ever
    /// became process-global again, the two budget assertions below would
    /// silently go back to being unusable under the parallel harness — and they
    /// would still PASS here, which is exactly how this gap shipped the first
    /// time.
    #[test]
    fn thread_spawn_counts_are_not_polluted_by_other_threads() {
        let src = fixture("testsrc_720p30_5s.mp4");

        let (s0, p0) = engine::thread_spawn_counts();
        // A real probe on THIS thread moves this thread's counter.
        engine::probe(Path::new(&src)).expect("probe the fixture");
        let (s1, p1) = engine::thread_spawn_counts();
        assert_eq!(p1, p0 + 1, "a probe on this thread must count here");
        assert_eq!(s1, s0, "a probe is not a stream spawn");

        // Probes on ANOTHER thread must not. This is the whole property: it is
        // what makes a tight budget assertable inside a parallel test binary.
        let global_before = engine::PROBE_SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst);
        let other = src.clone();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let _ = engine::probe(Path::new(&other));
            }
            // The child thread sees ITS OWN count, starting from zero.
            let (_, own) = engine::thread_spawn_counts();
            assert_eq!(own, 4, "a fresh thread starts its own counters at 0");
        })
        .join()
        .expect("the probing thread must not panic");

        let (s2, p2) = engine::thread_spawn_counts();
        assert_eq!(
            (s2, p2),
            (s1, p1),
            "SR-3: 4 probes on ANOTHER thread must leave this thread's counters \
             untouched — that is the property the spawn budgets depend on"
        );
        // ...while the process-global counter DID move, proving the sibling
        // work really happened and this is not a vacuous comparison.
        assert!(
            engine::PROBE_SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst) >= global_before + 4,
            "the negative control failed: the other thread's probes must still \
             be visible in the process-global counter"
        );
    }

    /// **SR-3, closed by an assertion rather than a comment.** Exporting a
    /// CONSTANT-speed clip through the composite branch must consume a BOUNDED
    /// number of ffmpeg stream spawns — one decoder reused across the run, not
    /// one spawn per layer per output frame.
    ///
    /// Without the retime-aware sequentiality fix, consecutive ticks advance
    /// SOURCE by `2 * step_us` against the old hard-coded `step_us / 2`
    /// tolerance, so EVERY tick is judged non-sequential and respawns: the
    /// 1.49 fps spawn storm documented at the top of the composite branch.
    /// Real media, real encode, real file on disk.
    #[test]
    fn constant_speed_composite_export_reuses_one_decoder() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let src = fixture("testsrc_720p30_5s.mp4");
        let media_id = "m-testsrc".to_string();
        let mut media = std::collections::HashMap::new();
        media.insert(
            media_id.clone(),
            (PathBuf::from(&src), 0u32, false, 0.0f64),
        );
        let clip = retimed(
            plain_clip(&media_id, 0, 5_000_000),
            rudis_core::RetimeCurve::Constant(2.0),
            30.0,
        );
        let timeline = one_clip_timeline(clip);
        let duration_us = timeline.duration_us();
        assert_eq!(duration_us, 2_500_000, "5 s of source at 2x is 2.5 s");

        let out = tmp_export_path("x2t-const-2x.mp4");
        let _ = std::fs::remove_file(&out);
        // PER-THREAD counters, NOT the process-global atomics (see
        // `spawn_budget` above): `run_export_blocking` issues every one of its
        // ffmpeg/ffprobe spawns on THIS thread, so this delta is exactly this
        // export's cost and is immune to the parallel suite around it.
        //
        // The probe count is the sharper per-frame-decode detector: the
        // per-frame fallback (`decode_frame_rgba_at`) calls `probe()` every
        // time, and probe is the ONLY one of the two child processes that is
        // instrumented. A spawn storm therefore shows up here as ~1 probe per
        // output frame even though the stream count would stay low.
        let (before, before_probes) = engine::thread_spawn_counts();
        let written = run_export_blocking(
            Box::new(|_| {}),
            ExportPlan {
                timeline,
                media,
                project_fps: 30.0,
            },
            duration_us,
            out.clone(),
            640,
            360,
            30.0,
        )
        .expect("a 2x constant-speed export must succeed");
        let (after, after_probes) = engine::thread_spawn_counts();
        let (spawns, probes) = (after - before, after_probes - before_probes);
        let out_frames = duration_us / engine::frame_step_us(30.0).max(1);

        assert!(written.exists(), "the export must write a real file");
        let bytes = std::fs::metadata(&written).expect("stat export").len();
        assert!(bytes > 1024, "the export must have real content, got {bytes} bytes");
        // ~75 output frames. A spawn storm would be ~75 of each; the
        // reused-decoder path is a handful (one decoder + the encoder + the
        // audio render + the cached dims probe).
        assert!(
            spawns <= 8,
            "SR-3: a constant-speed composite export must REUSE its decoder. \
             {out_frames} output frames consumed {spawns} ffmpeg stream spawns \
             (budget 8) — that is the spawn storm the retime-aware sequentiality \
             check exists to prevent"
        );
        assert!(
            probes <= 8,
            "SR-3: a constant-speed composite export must not probe per frame. \
             {out_frames} output frames consumed {probes} ffprobe spawns (budget 8)"
        );
        let _ = std::fs::remove_file(&written);
    }

    /// A RAMPED clip exports through the per-frame decode fallback (no single
    /// `fps=` value can express a varying cadence). That path is CORRECT by
    /// construction and SLOW by construction; this pins the cost so a
    /// regression is loud and the honest number stays on the record.
    ///
    /// Kept to a SHORT span deliberately — the point is the spawn ratio, not
    /// the wall clock. The full-length measurement lives in Task 8's artifacts.
    #[test]
    fn ramped_clip_export_spawn_count_is_pinned_to_per_frame_decode() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let src = fixture("testsrc_720p30_5s.mp4");
        let media_id = "m-testsrc".to_string();
        let mut media = std::collections::HashMap::new();
        media.insert(
            media_id.clone(),
            (PathBuf::from(&src), 0u32, false, 0.0f64),
        );
        // A 1 s source span with a 1.0 -> 2.0 hold-then-linear ramp.
        let clip = retimed(
            plain_clip(&media_id, 0, 1_000_000),
            rudis_core::RetimeCurve::Ramp(vec![
                rudis_core::Keyframe {
                    frame: 0,
                    value: 1.0,
                    interp: rudis_core::Interpolation::Linear,
                },
                rudis_core::Keyframe {
                    frame: 30,
                    value: 2.0,
                    interp: rudis_core::Interpolation::Linear,
                },
            ]),
            30.0,
        );
        let timeline = one_clip_timeline(clip);
        let duration_us = timeline.duration_us();
        let step = engine::frame_step_us(30.0).max(1);
        let out_frames = (duration_us + step - 1) / step;
        // A ramped clip also staircases its AUDIO into constant-tempo windows,
        // each of which is one `render_audio_pcm_retimed` (and therefore one
        // probe). Derive that count from the SAME core segmenter the export
        // path uses so the pin below is the design's real cost, not a magic
        // number that drifts.
        let audio_windows: usize = timeline
            .audio_contributors()
            .iter()
            .map(|c| rudis_core::retime_audio_windows(c).len())
            .sum();
        assert!(
            audio_windows > 1,
            "the ramped fixture must actually exercise the windowed audio path"
        );

        let out = tmp_export_path("x2t-ramp.mp4");
        let _ = std::fs::remove_file(&out);
        // PER-THREAD counters (see `spawn_budget` above): this test asserts a
        // tight budget AND a lower bound, so a sibling test's spawns landing in
        // the window would break it in BOTH directions.
        let (before, before_probes) = engine::thread_spawn_counts();
        let written = run_export_blocking(
            Box::new(|_| {}),
            ExportPlan {
                timeline,
                media,
                project_fps: 30.0,
            },
            duration_us,
            out.clone(),
            640,
            360,
            30.0,
        )
        .expect("a ramped export must succeed");
        let (after, after_probes) = engine::thread_spawn_counts();
        let (spawns, probes) = (after - before, after_probes - before_probes);

        assert!(written.exists());
        // PINNED, not "asserted fast". `decode_frame_rgba_at` spawns TWO child
        // processes per call (ffprobe + ffmpeg) but only the ffprobe half is
        // instrumented, so PROBE_SPAWN_COUNT is the per-frame counter and
        // STREAM_SPAWN_COUNT stays near zero — which is itself the proof that
        // this clip never took the streaming path.
        //
        // If someone later lands the restart-per-sub-window optimisation this
        // test FAILS loudly and gets re-pinned to the better number. That is
        // the point: the cost is measured, not hidden.
        assert!(
            spawns <= 4,
            "a ramped clip must NOT open a streaming decoder (no single fps= \
             value expresses a varying cadence): measured {spawns} stream spawns"
        );
        assert!(
            probes >= out_frames as usize,
            "the ramp path is per-frame decode: {out_frames} output frames should \
             cost at least that many ffprobe spawns, measured {probes}"
        );
        let budget = out_frames as usize + audio_windows + 8;
        assert!(
            probes <= budget,
            "the ramp path must cost ONE video decode per output frame \
             ({out_frames}) plus ONE audio render per constant-tempo window \
             ({audio_windows}) plus a small fixed overhead: budget {budget}, \
             measured {probes} probes"
        );
        let _ = std::fs::remove_file(&written);
    }
}

/// Phase 24 (ASSET-01, SC-1): the four `generate_image_gate` tests that assert
/// only on the DECODED pixels of the written PNG — relocated by plan 45-10.
/// Real-data only: every assertion reads back the actual file
/// `engine::write_frame_png` wrote.
#[cfg(test)]
mod generate_image_gate {
    use super::*;
    use crate::test_support::TestAppCtx;

    const W: u32 = 320;
    const H: u32 = 240;
    /// The dark-navy solid background used across the gate (#101840).
    const NAVY: [u8; 4] = [16, 24, 64, 255];

    /// Read the written PNG back to `(width, height, rgba)` via the SAME crate
    /// `engine::write_frame_png` used — a perfect round-trip of the written bytes.
    fn read_png_rgba(path: &str) -> (u32, u32, Vec<u8>) {
        let img = image::open(path)
            .unwrap_or_else(|e| panic!("written PNG {path} must open: {e}"))
            .to_rgba8();
        (img.width(), img.height(), img.into_raw())
    }

    /// Sample pixel `(x, y)` from a `width`-wide row-major RGBA buffer.
    fn px(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    /// Per-channel closeness within `tol` (0..255).
    fn close(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
        (0..4).all(|k| (a[k] as i32 - b[k] as i32).abs() <= tol)
    }

    /// Red-dominant like the compositor renders a #ff0000 fill (matches the
    /// engine's own stacking regression: r>200 && g<60 && b<60).
    fn is_red(p: [u8; 4]) -> bool {
        p[0] > 200 && p[1] < 60 && p[2] < 60
    }

    /// SC-1: a solid background + a placed rect + a placed text produce a PNG
    /// whose decoded pixels show each element at ITS coordinates (background did
    /// NOT occlude either — the Pitfall-1 stacking rule at the FULL pipeline).
    #[test]
    fn generate_image_pixels_match_spec() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = TestAppCtx::new();

        let input = serde_json::json!({
            "width": W, "height": H,
            "background": { "type": "solid", "color": "#101840" },
            "elements": [
                { "type": "rect", "fill": "#ff0000",
                  "transform": { "position": [0.05, 0.05], "scale": [0.35, 0.35], "rotation_deg": 0.0 } },
                { "type": "text", "content": "HELLO", "fill": "#00ff00", "fontSize": 0.15,
                  "transform": { "position": [0.1, 0.65], "scale": [0.8, 0.25], "rotation_deg": 0.0 } }
            ]
        });

        let item = run_generate_image(&ctx, &input)
            .expect("generate_image succeeds");
        let (pw, ph, rgba) = read_png_rgba(&item.path);
        assert_eq!((pw, ph), (W, H), "the PNG dims equal the spec");

        // Rect center (norm 0.225,0.225) sits inside [0.05,0.40]^2 -> its fill red.
        let (rx, ry) = ((0.225 * W as f32) as u32, (0.225 * H as f32) as u32);
        let rp = px(&rgba, W, rx, ry);
        assert!(is_red(rp), "the rect center must be its fill red, got {rp:?}");

        // Canvas center (0.5,0.5) is in neither element -> the background navy.
        let cp = px(&rgba, W, W / 2, H / 2);
        assert!(
            close(cp, NAVY, 8),
            "the canvas center (neither element) must be the background navy, got {cp:?}"
        );

        // Text region [0.1,0.9]x[0.65,0.90]: at least some GREEN (its fill) pixels
        // — proving the text element was painted OVER the background there.
        let (x0, x1) = ((0.1 * W as f32) as u32, (0.9 * W as f32) as u32);
        let (y0, y1) = ((0.65 * H as f32) as u32, (0.90 * H as f32) as u32);
        let mut green_px = 0u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = px(&rgba, W, x, y);
                if p[1] as i32 > p[0] as i32 + 40 && p[1] as i32 > p[2] as i32 + 40 {
                    green_px += 1;
                }
            }
        }
        assert!(
            green_px > 30,
            "the green text must be visible in its region (found {green_px} green px) — \
             the background must not occlude it"
        );
    }

    /// Debug session `preview-overlay-generation-opaque-background` (2026-07-24):
    /// a caller-authored alpha-bearing background (`"#RRGGBBAA"` with A=0, the
    /// tool's OWN documented color grammar) must land REAL transparent pixels in
    /// the written PNG, not the pre-fix silently-flattened opaque-black canvas
    /// (`composite_layers_to_rgba`'s locked D-01 opaque-black clear, shared with
    /// preview/export, discarded ANY background alpha unconditionally). An
    /// opaque element painted on top must stay fully opaque and its own color —
    /// proving the transparent-clear path composites normal opaque content
    /// exactly like before, and only the UNCOVERED canvas gains real alpha.
    #[test]
    fn generate_image_honors_a_transparent_background_alpha() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = TestAppCtx::new();

        let input = serde_json::json!({
            "width": W, "height": H,
            "background": { "type": "solid", "color": "#00000000" },
            "elements": [
                { "type": "rect", "fill": "#ff0000ff",
                  "transform": { "position": [0.05, 0.05], "scale": [0.35, 0.35], "rotation_deg": 0.0 } }
            ]
        });

        let item = run_generate_image(&ctx, &input)
            .expect("generate_image succeeds");
        let (pw, ph, rgba) = read_png_rgba(&item.path);
        assert_eq!((pw, ph), (W, H), "the PNG dims equal the spec");

        // Canvas center (0.5,0.5) is outside the rect -> bare background ->
        // must be GENUINELY transparent (alpha near 0), not opaque black.
        let cp = px(&rgba, W, W / 2, H / 2);
        assert!(
            cp[3] < 16,
            "the transparent background must land real alpha (near 0), got {cp:?} \
             (a near-255 alpha here is exactly the pre-fix opaque-black-clear bug)"
        );

        // The opaque red rect's own region must stay fully opaque and red —
        // proving normal (fully-opaque) content is unaffected by the transparent
        // clear color.
        let (rx, ry) = ((0.225 * W as f32) as u32, (0.225 * H as f32) as u32);
        let rp = px(&rgba, W, rx, ry);
        assert!(
            is_red(rp) && rp[3] > 240,
            "the opaque rect must stay fully opaque red, got {rp:?}"
        );
    }

    /// The aspect-ratio pitfall regression at the full-pipeline level: a wide,
    /// short banner rect fills its whole region edge-to-edge — no letterbox bars
    /// from contain-fit (raster dims derived from the element's own scale).
    #[test]
    fn generate_image_wide_banner_rect_has_no_letterbox() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = TestAppCtx::new();

        let input = serde_json::json!({
            "width": W, "height": H,
            "background": { "type": "solid", "color": "#101840" },
            "elements": [
                { "type": "rect", "fill": "#ff0000",
                  "transform": { "position": [0.2, 0.46], "scale": [0.6, 0.08], "rotation_deg": 0.0 } }
            ]
        });

        let item = run_generate_image(&ctx, &input)
            .expect("generate_image succeeds");
        let (pw, _ph, rgba) = read_png_rgba(&item.path);
        assert_eq!(pw, W);

        // Banner dest rect: x [0.2W, 0.8W] = [64,256], y [0.46H, 0.54H] = [110,130].
        // Sample the FOUR interior corners — all RED = filled edge-to-edge. If the
        // raster were a fixed square, contain-fit would letterbox two sides here.
        let x0 = (0.2 * W as f32) as u32 + 4;
        let x1 = (0.8 * W as f32) as u32 - 4;
        let y0 = (0.46 * H as f32) as u32 + 2;
        let y1 = (0.54 * H as f32) as u32 - 2;
        for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
            let p = px(&rgba, W, x, y);
            assert!(
                is_red(p),
                "banner corner ({x},{y}) must be the fill red edge-to-edge (no letterbox), got {p:?}"
            );
        }
    }

    /// BLOCKER-1 at the full-pipeline level (the flagship ASSET-01 title-card):
    /// a text element with an OMITTED transform renders at NATURAL (auto-fit)
    /// size — NOT stretched full-canvas.
    #[test]
    fn generate_image_text_omitted_transform_is_natural_size_not_full_canvas() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = TestAppCtx::new();

        let input = serde_json::json!({
            "width": W, "height": H,
            "background": { "type": "solid", "color": "#101840" },
            "elements": [
                { "type": "text", "content": "Hi", "fill": "#ffffff", "fontSize": 0.1 }
            ]
        });

        let item = run_generate_image(&ctx, &input)
            .expect("generate_image succeeds");
        let (pw, ph, rgba) = read_png_rgba(&item.path);
        assert_eq!((pw, ph), (W, H));

        // Count non-background (glyph) pixels. A naturally-sized "Hi" covers only a
        // small fraction; a full-canvas stretch (the bug) would smear the glyph
        // raster across the frame at a MUCH larger coverage.
        let mut non_bg = 0u32;
        for chunk in rgba.chunks_exact(4) {
            if !close([chunk[0], chunk[1], chunk[2], chunk[3]], NAVY, 24) {
                non_bg += 1;
            }
        }
        let coverage = non_bg as f64 / (W * H) as f64;
        assert!(non_bg > 0, "the text must actually render SOMETHING (non-vacuous)");
        assert!(
            coverage < 0.40,
            "an omitted-transform title-card text must render at NATURAL size (auto-fit), \
             not stretched full-canvas — coverage {coverage:.3} must be well under 0.40"
        );

        // The top-left canvas corner must still be background (a full-canvas
        // stretch would map the glyph raster's origin across the whole frame).
        let corner = px(&rgba, W, 0, 0);
        assert!(
            close(corner, NAVY, 16),
            "the top-left corner must still be the background, got {corner:?}"
        );
    }
}

/// Phase 24 (ASSET-02, SC-2/SC-2b): the three `generate_video_gate` tests that
/// assert only on the written MP4 (decoded pixels + probed codec) — relocated by
/// plan 45-10. `generate_video_uses_export_encoder` is half of the SC-4
/// licensing story; the other half is `crates/engine/tests/
/// overlay_asset_export.rs::frozen_export_encoder_untouched`, whose `APP_LAYER`
/// this batch extended to scan THIS file.
#[cfg(test)]
mod generate_video_gate {
    use super::*;
    use crate::test_support::TestAppCtx;

    /// Sample pixel `(x, y)` from a `width`-wide row-major RGBA buffer.
    fn px(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    /// Decode the written MP4 at `us` via the REAL engine video-decode path.
    /// (`src-tauri`'s `generate_video_gate` keeps its own copy for the SC-4
    /// roundtrip test that stayed.)
    fn decode_at(path: &str, us: i64) -> (u32, u32, Vec<u8>) {
        let f = engine::decode_frame_rgba_at(std::path::Path::new(path), us, 0)
            .unwrap_or_else(|e| panic!("decode generated video {path} at {us}us: {e}"));
        (f.width, f.height, f.rgba)
    }

    /// SC-2: an opacity-keyframed scene (a full-canvas WHITE rect fading in 0->1
    /// over a solid BLACK background) produces an MP4 whose decoded frames at two
    /// sampled timestamps show CLEARLY different pixels consistent with the fade
    /// curve — early tick ~background (dark), late tick ~fill (bright).
    #[test]
    fn generate_video_keyframed_frames_match_spec() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = TestAppCtx::new();

        // 2s @ 10fps = 20 frames (0..19). Opacity keyframed 0 (frame 0) -> 1
        // (frame 19, linear): a monotonic fade-in across the whole clip.
        let input = serde_json::json!({
            "width": 320, "height": 240, "fps": 10, "durationSeconds": 2,
            "background": { "type": "solid", "color": "#000000" },
            "elements": [
                { "type": "rect", "fill": "#ffffff",
                  "keyframes": { "opacity": [
                      { "frame": 0, "value": [0.0] },
                      { "frame": 19, "value": [1.0], "interp": "linear" }
                  ] } }
            ]
        });

        let item = run_generate_video(&ctx, &input)
            .expect("generate_video succeeds");
        assert_eq!(item.media_kind, MediaKind::Video, "kind measured as Video");

        // Sample the SAME center coordinate at an EARLY tick (opacity ~0.1 ->
        // near-black) and a LATE tick (opacity ~0.9 -> near-white).
        let (w, _h, early) = decode_at(&item.path, 200_000); // ~frame 2
        let (w2, _h2, late) = decode_at(&item.path, 1_700_000); // ~frame 17
        assert_eq!(w, w2, "both decoded frames share the asset width");
        let (cx, cy) = (w / 2, 120);
        let ep = px(&early, w, cx, cy);
        let lp = px(&late, w, cx, cy);
        let e_lum = ep[0] as i32 + ep[1] as i32 + ep[2] as i32;
        let l_lum = lp[0] as i32 + lp[1] as i32 + lp[2] as i32;
        println!(
            "SC-2 keyframed fade: early lum {e_lum} (px {ep:?}) vs late lum {l_lum} (px {lp:?})"
        );

        // The two sampled pixels at the SAME coordinate CLEARLY differ, each on
        // the correct side of the fade.
        assert!(
            l_lum > e_lum + 250,
            "opacity fade-in: the late frame must be far brighter than the early \
             (early lum {e_lum}, late lum {l_lum})"
        );
        assert!(
            e_lum < 360,
            "the early tick (opacity ~0.1) must read close to the BLACK background, got lum {e_lum}"
        );
        assert!(
            l_lum > 420,
            "the late tick (opacity ~0.9) must read close to the WHITE fill, got lum {l_lum}"
        );
    }

    /// SC-2b: the generated video is encoded via the SAME license-safe encoder
    /// export uses (`engine::DEFAULT_VIDEO_ENCODER`), NEVER a dev override.
    /// Proven by construction: the harness runs WITHOUT the dev override env set,
    /// so `VideoEncoder::new` selects the hardware default, and the call succeeds
    /// end-to-end (only possible if that real hardware encoder path is reachable
    /// — the very same path `export_timeline`/`export_project` already exercise);
    /// cross-checked against the written file's REAL probed video codec.
    #[test]
    fn generate_video_uses_export_encoder() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        assert!(
            std::env::var(engine::DEV_ENCODER_OVERRIDE_ENV).is_err(),
            "the encoder-identity test must run WITHOUT the dev encoder override \
             ({}) set — otherwise SC-2b's 'same encoder as export' claim is moot",
            engine::DEV_ENCODER_OVERRIDE_ENV
        );
        assert!(
            engine::DEFAULT_VIDEO_ENCODER.starts_with("h264"),
            "the license-safe default export encoder is an H.264 encoder, got {:?}",
            engine::DEFAULT_VIDEO_ENCODER
        );

        let ctx = TestAppCtx::new();
        let input = serde_json::json!({
            "width": 320, "height": 240, "fps": 10, "durationSeconds": 1,
            "background": { "type": "solid", "color": "#204080" },
            "elements": []
        });

        // Succeeds ONLY via the REAL hardware encoder path (the one export uses).
        let item = run_generate_video(&ctx, &input)
            .expect("generate_video succeeds via the real license-safe hardware encoder");

        // Real-output cross-check: the written stream's video codec is H.264, the
        // family DEFAULT_VIDEO_ENCODER (h264_mf / h264_videotoolbox) produces.
        let info = engine::probe(std::path::Path::new(&item.path)).expect("probe generated video");
        let vcodec = info.vcodec.as_deref().unwrap_or("");
        println!("SC-2b encoder: DEFAULT={} probed vcodec={vcodec}", engine::DEFAULT_VIDEO_ENCODER);
        assert!(
            vcodec.contains("h264") || vcodec.contains("avc"),
            "the generated video's probed codec must be H.264 (the export encoder's \
             family), got {vcodec:?}"
        );
    }

    /// Pitfall 2 regression: a minimal scene (solid background, NO elements) with
    /// a duration encodes cleanly even though the scene has NO audio — the silent
    /// WAV synthesized before `VideoEncoder::new` (which always muxes an audio
    /// input) satisfies the encoder. `has_audio` is TRUSTED from the probe (Open
    /// Question 2, DECIDED — NOT force-set to false).
    #[test]
    fn generate_video_silent_audio_does_not_error() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = TestAppCtx::new();
        let input = serde_json::json!({
            "width": 320, "height": 240, "fps": 10, "durationSeconds": 1,
            "background": { "type": "solid", "color": "#88aa44" },
            "elements": []
        });

        let item = run_generate_video(&ctx, &input)
            .expect("generate_video with no audio elements must NOT error (Pitfall 2)");

        assert_eq!(item.media_kind, MediaKind::Video, "kind measured as Video");
        assert!(
            std::path::Path::new(&item.path).is_file(),
            "the generated MP4 exists on disk: {}",
            item.path
        );
        // has_audio is whatever the probe measured — real value, not forced.
        let info = engine::probe(std::path::Path::new(&item.path)).expect("probe generated video");
        assert_eq!(
            item.has_audio, info.has_audio,
            "has_audio is trusted from the probe, not overridden"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 55 (plan 55-03, GATE-01): the PARITY TWINS.
//
// GATE-07 deletes `src-tauri`, and with it the 193-test `#[cfg(test)]` module
// that has always carried this project's full-timeline preview==export parity
// claim. `.planning/phases/55-integrated-parity-gate-cutover/artifacts/
// 55-03-parity-census.md` dispositions every one of those tests; the SEVEN
// rows it marks `PORT` are the class representatives, and they are below.
//
// TauriAppCtx+MockRuntime -> TestAppCtx is the ONLY change class. Concretely:
//
// | src-tauri original                                   | twin here                                   |
// |------------------------------------------------------|---------------------------------------------|
// | `build_app()` (`tauri::test::mock_builder`)           | `TestAppCtx::new()`                          |
// | `import_media_blocking(handle, app.state(), paths)`   | `crate::run_import_media_ui(ctx, paths)`     |
// | `place_clip(handle, app.state(), ..)`                 | `crate::run_place_clip(ctx, ..)`             |
// | `dispatch_command(handle, app.state(), cmd)`          | `crate::dispatch_command_inner(ctx, cmd)`    |
// | `export_timeline(handle, app.state(), p, w, h, fps)`  | `run_export(ctx, p, Some(w), .., Some(fps))` |
// | `get_snapshot(handle, app.state())`                   | `ctx.store().lock()..snapshot()`             |
// | `app.listen_any(EXPORT_PROGRESS_EVENT, ..)`           | `ctx.export_progress()`                      |
// | `TauriPreviewHost::new(handle)`                       | [`TestPreviewHost`]                          |
//
// Each of those `src-tauri` names is ALREADY a thin ctx-construction shim over
// the `app-core` function beside it — `export_timeline` is six lines over
// `run_export` (`src-tauri/src/lib.rs:2344`), `place_clip` one line over
// `run_place_clip` (`:2101`) — so the twins call what the originals called,
// one layer down.
//
// NO THRESHOLD IS LOOSENED. Every MAD bar, every control floor and every
// fixture is byte-identical to its original; the two MAD-0.0 assertions are
// still `assert_eq!(d, 0.0)`. The `mad()` in use is
// `test_support::mad`, "carried verbatim" per 48-VALIDATION.md's reuse rule
// (the same lift `crates/engine/tests/{surface_present,gpu_resident_parity,
// hw_decode_fallback}.rs` each carry, each with its own "do not invent a new
// metric" comment) — so these numbers stay comparable to every historical one.
//
// TWO DELIBERATE DELTAS, both recorded in the census and the plan SUMMARY:
//
//   1. `preview_timeline_at` — the PNG-over-asset-protocol preview command —
//      has NO successor (zero hits in `crates/`, no `rudis_*` export; the C#
//      shell reads the GPU present path). One original,
//      `export_timeline_is_frame_accurate_and_wysiwyg`, called it and then
//      explicitly DISCARDED the pixels (`let _ = preview_rgba;`), asserting
//      only that the PNG was 1280x720. That one resolution assertion is the
//      only thing the twin drops; every MAD assertion it made was against
//      `decode_frame_rgba_at_scaled` references, and all of those are kept.
//   2. `ffprobe` is resolved through `engine::locate()` (which finds the
//      vendored LGPL sidecar relocated to `runtime/binaries` by plan 55-01)
//      instead of the bare `"ffprobe"` PATH lookup the original used. Strictly
//      stronger: it pins the SHIPPED binary rather than whatever is on PATH.
// ---------------------------------------------------------------------------

/// The seven `PORT`-disposition parity twins (plan 55-03 Task 2).
#[cfg(test)]
mod parity_twins {
    use super::*;
    use crate::test_support::{fixture, mad, TestAppCtx};
    use std::process::{Command as Proc, Stdio};
    use std::sync::Arc;

    // ---- the originals' constants, carried byte-identically ----------------

    /// Lossy-encode tolerance for "the exported frame IS the right content"
    /// (H.264 introduces real but small per-pixel error vs. the lossless
    /// preview PNG path). Carried from `src-tauri`'s `export_gate:11414`.
    const EXPORT_MATCH_MAD: f64 = 12.0;
    /// Wrong-content floor: must be comfortably above `EXPORT_MATCH_MAD` so the
    /// tolerance is discriminating, not vacuous. `export_gate:11417`.
    const EXPORT_WRONG_MAD: f64 = 40.0;
    /// Relative RMS tolerance for the audio-mix assertion. `export_gate:11418`
    /// (and already duplicated once, at `export_gate::RMS_REL_TOL` above).
    const RMS_REL_TOL: f64 = 0.15;

    // ---- harness ----------------------------------------------------------

    /// Twin of `src-tauri`'s `export_gate::tmp_export_path` — same shape, same
    /// pid-scoped directory name. (Deliberately a second copy rather than a
    /// `pub(super)` widening: the sibling `export_gate` module above made the
    /// same call for the same reason.)
    fn tmp_export_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rudis-parity-twins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create export tmp dir");
        dir.join(name)
    }

    /// `build_app()`'s replacement. `Arc` because [`TestPreviewHost`] owns one
    /// (the ring producer thread needs an OWNED `Arc<dyn PreviewHost>` that
    /// outlives the call), and both must see the SAME store.
    fn ctx() -> Arc<TestAppCtx> {
        Arc::new(TestAppCtx::new())
    }

    /// `import_media_blocking(handle, app.state(), paths)`'s replacement.
    fn import(ctx: &TestAppCtx, paths: &[String]) -> Vec<rudis_core::MediaBinItem> {
        ctx.block_on(crate::run_import_media_ui(ctx, paths.to_vec()))
            .expect("import the fixtures")
    }

    /// `place_clip(handle, app.state(), ..)`'s replacement.
    fn place(ctx: &TestAppCtx, media_id: &str, track: usize, start_us: i64) -> rudis_core::Clip {
        crate::run_place_clip(ctx, media_id.to_string(), track, start_us)
            .expect("place the clip on the timeline")
    }

    /// `dispatch_command(handle, app.state(), cmd_from_json(..))`'s
    /// replacement — including the originals' `cmd_from_json` step, so the
    /// frontend JSON wire shape is still what gets deserialized.
    fn dispatch(ctx: &TestAppCtx, v: serde_json::Value) -> rudis_core::Patch {
        let cmd: rudis_core::Command =
            serde_json::from_value(v).expect("frontend JSON must map to a typed Command");
        crate::dispatch_command_inner(ctx, cmd).expect("dispatch the command")
    }

    /// `export_timeline(handle, app.state(), out, w, h, fps)`'s replacement:
    /// the SAME `run_export` that command is a six-line shim over, with the
    /// same explicit `Some(..)` overrides it passes.
    fn export(ctx: &TestAppCtx, out: &Path, w: u32, h: u32, fps: f64) -> String {
        ctx.block_on(run_export(
            ctx,
            out.to_path_buf(),
            Some(w),
            Some(h),
            Some(fps),
        ))
        .expect("the export must succeed")
    }

    /// `get_snapshot(handle, app.state())`'s replacement.
    fn snapshot(ctx: &TestAppCtx) -> rudis_core::Project {
        ctx.store().lock().expect("lock store").snapshot()
    }

    /// Decode the exported mp4 at `position_us` via an INDEPENDENT ffmpeg seek.
    /// Carried verbatim from `src-tauri`'s `export_gate::decode_export_at`.
    fn decode_export_at(path: &Path, position_us: i64) -> (u32, u32, Vec<u8>) {
        let probe = engine::probe(path).expect("probe exported file");
        let frame =
            engine::decode_frame_rgba_at(path, position_us, 0).expect("decode exported frame");
        assert_eq!((frame.width, frame.height), (probe.width, probe.height));
        (frame.width, frame.height, frame.rgba)
    }

    /// `export_gate::ffprobe_json`, with the ONE recorded improvement: the
    /// binary is resolved through `engine::locate()` (the vendored LGPL sidecar
    /// under `runtime/binaries` since plan 55-01) rather than a bare PATH
    /// lookup for `"ffprobe"`.
    fn ffprobe_json(path: &Path) -> serde_json::Value {
        let bins = engine::locate().expect("the vendored ffprobe sidecar must resolve");
        let out = Proc::new(&bins.ffprobe)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_streams",
                "-show_format",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .expect("spawn ffprobe");
        assert!(
            out.status.success(),
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("ffprobe JSON")
    }

    /// The tauri-free [`preview::PreviewHost`] — the twin of `src-tauri`'s
    /// `preview_host::TauriPreviewHost`, method for method.
    ///
    /// Only two methods carry behavior, and they are the two the ported gates
    /// exercise:
    ///
    /// * `store` — the SAME store the [`TestAppCtx`] mutates (an `Arc` field,
    ///   not a snapshot copy), so the producer thread reads the live project
    ///   exactly as `TauriPreviewHost` read the managed one.
    /// * `rasterize_text` — a PASS-THROUGH to
    ///   [`crate::compose::rasterize_text_layer`], the ONE shared helper the
    ///   export loop also calls. `TauriPreviewHost` is a pass-through to that
    ///   very same function (`src-tauri/src/preview_host.rs:218-238`), which is
    ///   precisely why `text_preview_parity_mad_zero` can assert MAD 0.
    ///
    /// The rest return the same inert values `TauriPreviewHost` returns under a
    /// MockRuntime (no overlay mirror, no gesture, no window): empty vectors,
    /// the ink token, and no-ops.
    struct TestPreviewHost {
        ctx: Arc<TestAppCtx>,
        /// `TauriPreviewHost` falls back to a private mirror whenever one is
        /// not managed — which, under `build_app()`, is always except where a
        /// test managed one explicitly. Its documented reading is "not playing,
        /// position 0, **Program mode**", which is exactly what a fresh
        /// `PlaybackMirror` reads, so the ring twin needs no `app.manage` step.
        mirror: preview::PlaybackMirror,
    }

    impl TestPreviewHost {
        fn new(ctx: Arc<TestAppCtx>) -> Self {
            Self {
                ctx,
                mirror: preview::PlaybackMirror::new(),
            }
        }
    }

    impl preview::PreviewHost for TestPreviewHost {
        fn store(&self) -> Option<std::sync::MutexGuard<'_, rudis_core::Store>> {
            self.ctx.store().lock().ok()
        }
        fn playback_mirror(&self) -> &preview::PlaybackMirror {
            &self.mirror
        }
        fn resolve_overlay(&self) -> Vec<(rudis_core::Annotation, f32)> {
            Vec::new()
        }
        fn live_gesture(&self) -> Vec<(f32, f32)> {
            Vec::new()
        }
        fn overlay_ink(&self) -> [u8; 4] {
            // `src-tauri`'s `OVERLAY_INK` design token. Unreachable from these
            // gates (none draws ink), carried for completeness.
            [0x4F, 0x8A, 0xFF, 0xFF]
        }
        fn draw_ink(
            &self,
            _frame: &mut engine::Frame,
            _annotations: &[rudis_core::Annotation],
            _ink: [u8; 4],
            _dashed: bool,
        ) {
        }
        fn rasterize_text(
            &self,
            text: &rudis_core::TextPayload,
            transform: engine::LayerTransform,
            opacity: f32,
            crop: engine::LayerCrop,
            project_w: u32,
            project_h: u32,
        ) -> engine::Layer {
            // The ONE shared helper — never a duplicate. This is the whole
            // reason preview and export composite text byte-identically.
            let mut rasterizer = engine::TextRasterizer::new();
            crate::compose::rasterize_text_layer(
                &mut rasterizer,
                text,
                transform,
                opacity,
                crop,
                project_w,
                project_h,
            )
        }
        fn emit_canvas_viewport(&self, _w: u32, _h: u32, _fw: u32, _fh: u32) {}
        // NOTE: no `present_gpu` here. That method belongs to
        // `preview::PresentSink` (the GPU OUTPUT port), not to `PreviewHost`
        // (the shell-SERVICES port) — and none of these twins presents to a
        // live surface, so no `PresentSink` impl is needed at all. A first draft
        // of this impl carried one behind
        // `#[cfg(all(windows, feature = "hwdecode"))]`, which was doubly wrong:
        // that predicate reads THIS crate's feature namespace, where no
        // `hwdecode` feature exists, so the method would never have compiled
        // even if the trait had wanted it. `cargo test -p app-core`'s
        // `unexpected_cfgs` warning is what caught it.
    }

    /// A `Clip` literal with EVERY field explicit (the 18-03 fixture rule the
    /// originals follow).
    fn make_clip(
        id: &str,
        media_id: &str,
        start_us: i64,
        out_us: i64,
        transform: rudis_core::ClipTransform,
        opacity: f32,
        crop: rudis_core::ClipCrop,
    ) -> rudis_core::Clip {
        rudis_core::Clip {
            id: id.to_string(),
            media_id: media_id.to_string(),
            start_us,
            in_us: 0,
            out_us,
            volume: 1.0,
            audio_detached: false,
            transform,
            opacity,
            crop,
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        }
    }

    /// Build an `engine::Layer` for the ground-truth composite from a clip +
    /// its media item at `t_us` — the originals' `layer_for` closure.
    fn layer_for(
        clip: &rudis_core::Clip,
        item: &rudis_core::MediaBinItem,
        t_us: i64,
    ) -> engine::Layer {
        let source_us = clip.in_us + (t_us - clip.start_us);
        let frame =
            engine::decode_frame_rgba_at(Path::new(&item.path), source_us, item.rotation_degrees)
                .expect("decode ground-truth layer source");
        engine::Layer {
            frame,
            opacity: clip.opacity,
            transform: engine::LayerTransform {
                position: clip.transform.position,
                scale: clip.transform.scale,
                rotation_deg: clip.transform.rotation_deg,
            },
            crop: engine::LayerCrop {
                left: clip.crop.left,
                top: clip.crop.top,
                right: clip.crop.right,
                bottom: clip.crop.bottom,
            },
            alpha_mode: engine::AlphaMode::Straight,
        }
    }

    // =======================================================================
    // 1/7 — still-image export
    // =======================================================================

    /// Ported from src-tauri/src/lib.rs:11467 (Phase 55 plan 55-03, GATE-01) —
    /// TauriAppCtx→TestAppCtx is the ONLY change class.
    ///
    /// Regression (still-image export, Phase 32): a timeline whose ONLY clip is
    /// a 5s STILL must export a 5s video with a real, decodable frame at 1s.
    /// The bug was a missing `-loop 1`, which emitted exactly ONE frame and made
    /// the decode at 1s return `BadOutputSize { got: 0 }`. Proof is on REAL
    /// output, with a black-frame control proving the match is discriminating.
    #[test]
    fn still_image_exports_full_duration_decodable_at_one_second() {
        let ctx = ctx();
        let still = fixture("still.png");

        let imported = import(&ctx, &[still]);
        let item = imported[0].clone();
        assert_eq!(
            item.media_kind,
            MediaKind::Image,
            "fixture must probe as a still image"
        );
        let (w, h) = (item.width, item.height);
        assert!(w > 0 && h > 0, "real probed dims: {w}x{h}");

        // Place at t=0 → a still gets the DEFAULT_STILL_DURATION_US (5s).
        let clip = place(&ctx, &item.id, 0, 0);
        assert_eq!(
            clip.out_us,
            rudis_core::tools::DEFAULT_STILL_DURATION_US,
            "a placed still spans the 5s default duration"
        );

        let out_path = tmp_export_path("still_full_duration.mp4");
        export(&ctx, &out_path, w, h, 30.0);

        // The exported container must actually be ~5s long (the bug produced a
        // ~1/30s file).
        let meta = ffprobe_json(&out_path);
        let dur_s: f64 = meta["format"]["duration"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .expect("exported container reports a duration");
        assert!(
            dur_s >= 4.5,
            "a 5s still must export a ~5s video, got {dur_s:.3}s (the pre-fix 1-frame bug yields ~0.03s)"
        );

        let exported = engine::decode_frame_rgba_at(&out_path, 1_000_000, 0)
            .expect("decode the exported frame at 1s (pre-fix: BadOutputSize { got: 0 })");
        assert_eq!((exported.width, exported.height), (w, h));

        // MATCH: the exported pixels are the still's own pixels, within H.264
        // tolerance. The source PNG is decoded through the SAME engine entry
        // point the original's `png_rgba` helper shelled out for.
        let source_png = engine::decode_frame_rgba_at(Path::new(&item.path), 0, 0)
            .expect("decode the source still");
        assert_eq!((source_png.width, source_png.height), (w, h));
        let d_right = mad(&exported.rgba, &source_png.rgba);
        // CONTROL: a black frame — the visual signature of the bug's missing
        // frame (non-vacuous for a saturated still).
        let black: Vec<u8> = (0..(w as usize * h as usize))
            .flat_map(|_| [0u8, 0, 0, 255])
            .collect();
        let d_wrong = mad(&exported.rgba, &black);
        println!(
            "still-export regression: container={dur_s:.3}s | MAD(exported@1s vs source png)={d_right:.3} (<= {EXPORT_MATCH_MAD}) | MAD(vs black control)={d_wrong:.3} (>= {EXPORT_WRONG_MAD})"
        );
        assert!(
            d_right <= EXPORT_MATCH_MAD,
            "the exported 1s frame must be the still's pixels, MAD {d_right:.3} > {EXPORT_MATCH_MAD}"
        );
        assert!(
            d_wrong >= EXPORT_WRONG_MAD,
            "control must be discriminating: exported still vs black MAD {d_wrong:.3} < {EXPORT_WRONG_MAD}"
        );

        let _ = std::fs::remove_file(&out_path);
    }

    // =======================================================================
    // 2/7 — single-clip edited-timeline export (WYSIWYG). THE core-value test.
    // =======================================================================

    /// Ported from src-tauri/src/lib.rs:11551 (Phase 55 plan 55-03, GATE-01) —
    /// TauriAppCtx→TestAppCtx is the ONLY change class.
    ///
    /// Every edit kind in ONE project — trim, split, per-clip volume, rearrange
    /// (move), audio-detach — exported through the real export path, then
    /// verified on the REAL decoded file: h264/640x360/aac, duration within a
    /// frame, three sampled frames MAD-matched against independent scaled
    /// source decodes (each with a wrong-content control), the detached
    /// volume-halved audio region at ~half the RMS of an independent
    /// full-volume placement of the SAME source, and monotone progress to 100%.
    ///
    /// Delta (recorded): the original also called `preview_timeline_at` at each
    /// sample and asserted the returned PNG was 1280x720, while explicitly
    /// discarding its pixels (`let _ = preview_rgba;`). That command has no
    /// successor past the cutover, so the twin drops that one resolution
    /// assertion. Every MAD assertion the original made is kept, unchanged.
    #[test]
    fn export_timeline_is_frame_accurate_and_wysiwyg() {
        let ctx = ctx();
        let bars = fixture("bars_720p30_5s.mp4");
        let tsrc = fixture("testsrc_720p30_5s.mp4");

        // media1 = testsrc (trimmed/split piece); media2 = bars (rearranged
        // piece — HAS audio, needed for detach).
        let imported = import(&ctx, &[tsrc.clone(), bars.clone()]);
        let (media1, media2) = (imported[0].id.clone(), imported[1].id.clone());

        // media1 whole @0 -> trim to [1s,4s).
        let clip1 = place(&ctx, &media1, 0, 0);
        dispatch(
            &ctx,
            serde_json::json!({
                "type": "trim_clip",
                "data": { "id": clip1.id, "new_in_us": 1_000_000, "new_out_us": 4_000_000 }
            }),
        );

        // Split at 2.5s -> left [1s,2.5s), right [2.5s,4s).
        let split_patch = dispatch(
            &ctx,
            serde_json::json!({
                "type": "split_clip",
                "data": { "id": clip1.id, "at_position_us": 2_500_000 }
            }),
        );
        let right_id = split_patch.ids[1].clone();

        // Volume=0.5 on the right split clip.
        dispatch(
            &ctx,
            serde_json::json!({
                "type": "set_clip_volume", "data": { "id": right_id, "volume": 0.5 }
            }),
        );

        // media2 whole @4s, then REARRANGED to @6s.
        let clip2 = place(&ctx, &media2, 0, 4_000_000);
        dispatch(
            &ctx,
            serde_json::json!({
                "type": "move_clip", "data": { "id": clip2.id, "new_start_us": 6_000_000 }
            }),
        );

        // Detach media2's audio, then halve the DETACHED clip's volume.
        let detach_patch = dispatch(
            &ctx,
            serde_json::json!({
                "type": "detach_audio", "data": { "clip_id": clip2.id }
            }),
        );
        let detached_audio_clip_id = detach_patch.ids[1].clone();
        dispatch(
            &ctx,
            serde_json::json!({
                "type": "set_clip_volume",
                "data": { "id": detached_audio_clip_id, "volume": 0.5 }
            }),
        );

        // An independent FULL-volume audio-track placement of the SAME source
        // elsewhere — the reference region for the RMS ratio check.
        place(&ctx, &media2, 1, 20_000_000);

        let snap = snapshot(&ctx);
        assert_eq!(
            snap.timeline.tracks[0].clips.len(),
            3,
            "left+right split + media2"
        );
        assert_eq!(
            snap.timeline.tracks[1].clips.len(),
            2,
            "detached audio clip + full-vol reference"
        );
        let expected_duration_us = snap.timeline.duration_us();
        assert_eq!(
            expected_duration_us, 25_000_000,
            "the full-volume reference clip @20s + 5s now sets the timeline's extent"
        );

        // --- Export. ---
        let out_path = tmp_export_path("export_gate_core_value.mp4");
        let out_path_str = out_path.to_string_lossy().into_owned();
        let returned_path = export(&ctx, &out_path, 640, 360, 30.0);
        assert_eq!(returned_path, out_path_str);
        assert!(out_path.is_file(), "exported mp4 must exist on disk");
        assert!(
            std::fs::metadata(&out_path).unwrap().len() > 0,
            "exported mp4 must be non-empty"
        );

        // --- ffprobe: h264, 640x360, expected duration (+-1 frame), aac. ---
        let probe = ffprobe_json(&out_path);
        let streams = probe["streams"].as_array().expect("streams array");
        let vstream = streams
            .iter()
            .find(|s| s["codec_type"] == "video")
            .expect("a video stream");
        assert_eq!(vstream["codec_name"], "h264", "video codec must be h264");
        assert_eq!(vstream["width"], 640);
        assert_eq!(vstream["height"], 360);
        let astream = streams
            .iter()
            .find(|s| s["codec_type"] == "audio")
            .expect("an audio stream");
        assert_eq!(astream["codec_name"], "aac", "audio codec must be aac");

        let duration_s: f64 = probe["format"]["duration"]
            .as_str()
            .expect("format.duration")
            .parse()
            .expect("duration parses as f64");
        let expected_s = expected_duration_us as f64 / 1_000_000.0;
        let frame_s = 1.0 / 30.0;
        println!("exported duration = {duration_s:.4}s; expected = {expected_s:.4}s");
        assert!(
            (duration_s - expected_s).abs() <= frame_s * 2.0,
            "exported duration must match the timeline duration within ~1 frame \
             (got {duration_s:.4}s, expected {expected_s:.4}s)"
        );

        // --- WYSIWYG samples. ---
        let sample = |label: &str, t_us: i64, right_ref: &[u8], wrong_ref: &[u8]| {
            let (w, h, exported_rgba) = decode_export_at(&out_path, t_us);
            assert_eq!((w, h), (640, 360));
            let d_right = mad(&exported_rgba, right_ref);
            let d_wrong = mad(&exported_rgba, wrong_ref);
            println!(
                "export@{label} ({t_us}us): vs correct-content MAD = {d_right:.4}; \
                 vs wrong-content MAD = {d_wrong:.4}"
            );
            assert!(
                d_right <= EXPORT_MATCH_MAD,
                "export@{label} must match the right content within lossy tolerance \
                 (MAD {d_right:.4} > {EXPORT_MATCH_MAD})"
            );
            assert!(
                d_wrong > EXPORT_WRONG_MAD,
                "export@{label} must clearly NOT match the wrong content \
                 (MAD {d_wrong:.4} <= {EXPORT_WRONG_MAD})"
            );
        };

        // T=1.5s: inside the LEFT split piece -> testsrc source 1.5s.
        let right_ref = engine::decode_frame_rgba_at_scaled(Path::new(&tsrc), 1_500_000, 0, 640, 360)
            .expect("scaled reference testsrc@1.5s")
            .rgba;
        let wrong_ref = engine::decode_frame_rgba_at_scaled(Path::new(&bars), 1_500_000, 0, 640, 360)
            .expect("scaled reference bars@1.5s")
            .rgba;
        sample("inside trimmed+split A", 1_500_000, &right_ref, &wrong_ref);

        // T=2.6s: just after the split (right piece, volume=0.5 region).
        let right_ref = engine::decode_frame_rgba_at_scaled(Path::new(&tsrc), 2_600_000, 0, 640, 360)
            .expect("scaled reference testsrc@2.6s")
            .rgba;
        let wrong_ref = engine::decode_frame_rgba_at_scaled(Path::new(&bars), 2_600_000, 0, 640, 360)
            .expect("scaled reference bars@2.6s")
            .rgba;
        sample("just after split", 2_600_000, &right_ref, &wrong_ref);

        // T=7s: inside B (bars, rearranged to start@6s) -> bars source 1s.
        let right_ref = engine::decode_frame_rgba_at_scaled(Path::new(&bars), 1_000_000, 0, 640, 360)
            .expect("scaled reference bars@1s")
            .rgba;
        let wrong_ref = engine::decode_frame_rgba_at_scaled(Path::new(&tsrc), 1_000_000, 0, 640, 360)
            .expect("scaled reference testsrc@1s")
            .rgba;
        sample("inside B (rearranged)", 7_000_000, &right_ref, &wrong_ref);

        // --- Audio: the volume-halved DETACHED region ~= half the independent
        // full-volume reference region (SAME source, two placements). ---
        let full_pcm = engine::render_audio_pcm(&out_path, 20_000_000, 21_500_000, 1.0)
            .expect("render exported audio [20s,21.5s) (full-volume reference)");
        let half_pcm = engine::render_audio_pcm(&out_path, 6_500_000, 8_000_000, 1.0)
            .expect("render exported audio [6.5s,8s) (detached, volume=0.5)");
        let full_rms = engine::rms(&full_pcm);
        let half_rms = engine::rms(&half_pcm);
        let ratio = full_rms / half_rms;
        println!(
            "exported audio RMS: full-volume reference region = {full_rms:.6}; \
             detached volume=0.5 region = {half_rms:.6}; ratio = {ratio:.4}"
        );
        assert!(
            full_rms > 0.01,
            "full-volume reference region must be audible, got RMS {full_rms}"
        );
        assert!(
            half_rms > 0.01,
            "the detached (volume-halved) clip must still be audible, got RMS {half_rms}"
        );
        assert!(
            (ratio - 2.0).abs() < 2.0 * RMS_REL_TOL,
            "exported RMS ratio (full-volume / detached-half-volume) must be ~2:1, got {ratio:.4}"
        );

        // --- Progress: monotonically increasing, ending ~100%. ---
        // `app.listen_any(EXPORT_PROGRESS_EVENT, ..)` -> `ctx.export_progress()`,
        // the in-memory twin of that event bus.
        let log = ctx.export_progress();
        println!("export:progress events ({} total): {log:?}", log.len());
        assert!(!log.is_empty(), "export:progress must fire at least once");
        for pair in log.windows(2) {
            assert!(
                pair[1] + 1e-6 >= pair[0],
                "export:progress must never decrease: {log:?}"
            );
        }
        assert!(
            *log.last().unwrap() >= 99.0,
            "export:progress must end at ~100%, got {:?}",
            log.last()
        );

        let _ = std::fs::remove_file(&out_path);
    }

    // =======================================================================
    // 3/7 — multi-layer composite export. AUTHORITATIVE (research A1).
    // =======================================================================

    /// Ported from src-tauri/src/lib.rs:12551 (Phase 55 plan 55-03, GATE-01) —
    /// TauriAppCtx→TestAppCtx is the ONLY change class.
    ///
    /// Phase 18 (COMP-02, SC-2): FULL-PATH multi-layer WYSIWYG. A 3-layer
    /// timeline (three distinct sources, each with a different
    /// transform/opacity/crop) is exported through the REAL export path, and the
    /// DECODED EXPORTED frames are frame-diffed against the offscreen
    /// `composite_layers_to_rgba` output at the SAME timestamps — export and
    /// preview share ONE composite by measurement, not by coincidence. Two
    /// non-vacuous controls per sample: a wrong-content full-frame source, and
    /// the pre-Phase-18 fast path (the TOP clip alone, stretched full-frame).
    #[test]
    fn multilayer_export_matches_offscreen_composite() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = ctx();
        let rot = fixture("rotated_90.mp4");
        let bars = fixture("bars_720p30_5s.mp4");
        let tsrc = fixture("testsrc_720p30_5s.mp4");

        let imported = import(&ctx, &[rot.clone(), bars.clone(), tsrc.clone()]);
        let (rot_item, bars_item, tsrc_item) = (
            imported[0].clone(),
            imported[1].clone(),
            imported[2].clone(),
        );

        // TOP (track 0): testsrc as a bottom-right PIP, cropped from the left.
        let top = make_clip(
            "L-top",
            &tsrc_item.id,
            0,
            1_200_000,
            rudis_core::ClipTransform {
                position: (0.55, 0.55),
                scale: (0.4, 0.4),
                rotation_deg: 0.0,
            },
            1.0,
            rudis_core::ClipCrop {
                left: 0.25,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            },
        );
        // MIDDLE: bars as a translucent, slightly rotated upper-left panel.
        let mid = make_clip(
            "L-mid",
            &bars_item.id,
            0,
            1_200_000,
            rudis_core::ClipTransform {
                position: (0.05, 0.08),
                scale: (0.45, 0.45),
                rotation_deg: 10.0,
            },
            0.5,
            rudis_core::ClipCrop::default(),
        );
        // BASE: the rotated (portrait) source, full-canvas identity.
        let base = make_clip(
            "L-base",
            &rot_item.id,
            0,
            1_200_000,
            rudis_core::ClipTransform::default(),
            1.0,
            rudis_core::ClipCrop::default(),
        );

        {
            let mut guard = ctx.store().lock().expect("lock store");
            let mut project = guard.snapshot();
            // tracks = [Video, Audio]; `active_layers_at` scans video tracks in
            // `tracks` order (index-0 = TOP), so track 0 stays the top layer and
            // the appended tracks stack beneath it.
            project.timeline.tracks[0].clips.push(top.clone());
            project.timeline.tracks.push(rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: vec![mid.clone()],
            });
            project.timeline.tracks.push(rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: vec![base.clone()],
            });
            *guard = rudis_core::Store::from_project(project);
        }

        let out_path = tmp_export_path("multilayer_export_sc2.mp4");
        let out_path_str = out_path.to_string_lossy().into_owned();
        let returned = export(&ctx, &out_path, 640, 360, 30.0);
        assert_eq!(returned, out_path_str);
        assert!(out_path.is_file(), "exported mp4 must exist on disk");

        let compositor = engine::Compositor::new().expect("offscreen compositor");
        for (label, t_us) in [("early", 500_000i64), ("late", 1_000_000i64)] {
            // Track order = compositor order: index-0 = top (never reversed).
            let layers = vec![
                layer_for(&top, &tsrc_item, t_us),
                layer_for(&mid, &bars_item, t_us),
                layer_for(&base, &rot_item, t_us),
            ];
            let truth = compositor
                .composite_layers_to_rgba(&layers, 640, 360)
                .expect("offscreen ground-truth composite");

            let (w, h, exported) = decode_export_at(&out_path, t_us);
            assert_eq!((w, h), (640, 360));

            let d_right = mad(&exported, &truth);
            let wrong = engine::decode_frame_rgba_at_scaled(Path::new(&bars), t_us, 0, 640, 360)
                .expect("wrong-content reference")
                .rgba;
            let d_wrong = mad(&exported, &wrong);
            let old_path = engine::decode_frame_rgba_at_scaled(Path::new(&tsrc), t_us, 0, 640, 360)
                .expect("old-fast-path reference")
                .rgba;
            let d_old = mad(&exported, &old_path);
            println!(
                "multilayer export@{label} ({t_us}us): vs offscreen composite MAD = \
                 {d_right:.4}; vs wrong-content MAD = {d_wrong:.4}; vs old-fast-path \
                 MAD = {d_old:.4}"
            );
            assert!(
                d_right <= EXPORT_MATCH_MAD,
                "exported frame@{label} must match the offscreen composite within lossy \
                 tolerance (MAD {d_right:.4} > {EXPORT_MATCH_MAD}) — SC-2 full-path WYSIWYG"
            );
            assert!(
                d_wrong > EXPORT_WRONG_MAD,
                "exported frame@{label} must clearly NOT match wrong content \
                 (MAD {d_wrong:.4} <= {EXPORT_WRONG_MAD})"
            );
            assert!(
                d_old > EXPORT_WRONG_MAD,
                "exported frame@{label} must clearly NOT be the old single-layer \
                 fast-path output (MAD {d_old:.4} <= {EXPORT_WRONG_MAD}) — the \
                 multi-layer composite branch must actually have run"
            );
        }

        let _ = std::fs::remove_file(&out_path);
    }

    // =======================================================================
    // 4/7 — multi-layer PREVIEW pool composite
    // =======================================================================

    /// Ported from src-tauri/src/lib.rs:12770 (Phase 55 plan 55-03, GATE-01) —
    /// TauriAppCtx→TestAppCtx is the ONLY change class (plus
    /// TauriPreviewHost→TestPreviewHost, the same substitution one layer down).
    ///
    /// Phase 18.2 (PERF-01, SC-3): HEADLESS preview/export composite PARITY. The
    /// pool-sourced multi-layer PREVIEW composite — built through the REAL glue
    /// (`preview::resolve_multilayer` → the persistent `engine::LayerDecoderPool`
    /// via `preview::compose_multilayer_from_pool`) — is frame-diffed against an
    /// INDEPENDENT offscreen ground truth whose layers come from per-layer
    /// `decode_frame_rgba_at` (NO pool). This is the COMP-02 D-02
    /// one-composite-path guard. A SECOND sequential tick covers the pool's
    /// decoder-REUSE path, not just the first pull.
    #[test]
    fn multilayer_preview_matches_offscreen_composite() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = ctx();
        let rot = fixture("rotated_90.mp4");
        let bars = fixture("bars_720p30_5s.mp4");
        let tsrc = fixture("testsrc_720p30_5s.mp4");

        // Two DISTINCT sources placed; bars is decoded ONLY as a wrong-content
        // control (never placed) so it can never alias either layer.
        let imported = import(&ctx, &[rot.clone(), tsrc.clone()]);
        let (rot_item, tsrc_item) = (imported[0].clone(), imported[1].clone());

        // TOP (track 0): testsrc as a cropped, scaled centre PIP.
        let top = make_clip(
            "L-top",
            &tsrc_item.id,
            0,
            3_000_000,
            rudis_core::ClipTransform {
                position: (0.30, 0.30),
                scale: (0.4, 0.4),
                rotation_deg: 0.0,
            },
            1.0,
            rudis_core::ClipCrop {
                left: 0.2,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            },
        );
        // BASE: the rotated (portrait) source, full-canvas identity.
        let base = make_clip(
            "L-base",
            &rot_item.id,
            0,
            3_000_000,
            rudis_core::ClipTransform::default(),
            1.0,
            rudis_core::ClipCrop::default(),
        );

        {
            let mut guard = ctx.store().lock().expect("lock store");
            let mut project = guard.snapshot();
            project.width = 640;
            project.height = 360;
            project.fps = 30.0;
            project.timeline.tracks[0].clips.push(top);
            project.timeline.tracks.push(rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: vec![base],
            });
            *guard = rudis_core::Store::from_project(project);
        }

        let host = TestPreviewHost::new(Arc::clone(&ctx));
        // ONE pool advanced across BOTH ticks so the second tick lands on the
        // decoder-REUSE (sequential source_us) path.
        let mut pool = engine::LayerDecoderPool::new(30.0, std::time::Duration::from_secs(5));
        let step_us = engine::frame_step_us(30.0);
        let compositor = engine::Compositor::new().expect("offscreen compositor");
        let pos0 = 500_000i64;

        for (label, position_us) in [("early", pos0), ("reuse", pos0 + step_us)] {
            let stack = preview::resolve_multilayer(&host, position_us)
                .expect("position must sit inside the multi-layer range");
            assert_eq!((stack.width, stack.height), (640, 360));
            assert_eq!(stack.layers.len(), 2, "two video layers must resolve");

            let pool_layers = preview::compose_multilayer_from_pool(&host, &mut pool, &stack);
            assert_eq!(
                pool_layers.len(),
                2,
                "the pool must source BOTH layers this tick"
            );
            let pool_rgba = compositor
                .composite_layers_to_rgba(&pool_layers, stack.width, stack.height)
                .expect("pool-sourced composite");

            let truth_layers: Vec<engine::Layer> = stack
                .layers
                .iter()
                .map(|spec| {
                    let frame =
                        engine::decode_frame_rgba_at(&spec.path, spec.source_us, spec.rotation)
                            .expect("independent ground-truth decode");
                    engine::Layer {
                        frame,
                        opacity: spec.opacity,
                        transform: spec.transform,
                        crop: spec.crop,
                        alpha_mode: engine::AlphaMode::Straight,
                    }
                })
                .collect();
            let truth_rgba = compositor
                .composite_layers_to_rgba(&truth_layers, stack.width, stack.height)
                .expect("offscreen ground-truth composite");

            let d_right = mad(&pool_rgba, &truth_rgba);
            let wrong =
                engine::decode_frame_rgba_at_scaled(Path::new(&bars), position_us, 0, 640, 360)
                    .expect("wrong-content reference")
                    .rgba;
            let d_wrong = mad(&pool_rgba, &wrong);
            let top_only =
                engine::decode_frame_rgba_at_scaled(Path::new(&tsrc), position_us, 0, 640, 360)
                    .expect("top-only reference")
                    .rgba;
            let d_top = mad(&pool_rgba, &top_only);

            println!(
                "multilayer preview parity@{label} ({position_us}us): vs offscreen composite \
                 MAD = {d_right:.4}; vs wrong-content MAD = {d_wrong:.4}; vs top-only \
                 MAD = {d_top:.4}"
            );
            assert!(
                d_right <= EXPORT_MATCH_MAD,
                "pool-sourced preview composite@{label} must match the INDEPENDENT offscreen \
                 ground truth within lossy tolerance (MAD {d_right:.4} > {EXPORT_MATCH_MAD}) — \
                 SC-3 one-composite-path parity"
            );
            assert!(
                d_wrong > EXPORT_WRONG_MAD,
                "preview composite@{label} must clearly NOT match wrong content \
                 (MAD {d_wrong:.4} <= {EXPORT_WRONG_MAD})"
            );
            assert!(
                d_top > EXPORT_WRONG_MAD,
                "preview composite@{label} must clearly NOT be the top layer alone \
                 (MAD {d_top:.4} <= {EXPORT_WRONG_MAD}) — the multi-layer composite branch \
                 must actually have run"
            );
        }
    }

    // =======================================================================
    // 5/7 — ring / multi-frame. AUTHORITATIVE (research A1). MAD 0.0.
    // =======================================================================

    /// 18.3-04's shared harness, ported alongside its test: a REAL 2-track
    /// overlapping timeline built via REAL commands only (never a
    /// `Store::from_project` bypass) — project pinned to 640x360@30, BASE =
    /// bars full-canvas on video track 0 spanning [0, 2.4s), TOP = testsrc as a
    /// PIP on a REAL added video track spanning [0.8s, 1.6s). The multi-layer
    /// overlap is therefore exactly [800_000, 1_600_000).
    fn build_overlap_timeline(ctx: &TestAppCtx) {
        let bars = fixture("bars_720p30_5s.mp4");
        let tsrc = fixture("testsrc_720p30_5s.mp4");
        let imported = import(ctx, &[bars, tsrc]);
        let (bars_item, tsrc_item) = (imported[0].clone(), imported[1].clone());

        dispatch(
            ctx,
            serde_json::json!({
                "type": "set_project_settings",
                "data": { "fps": 30.0, "width": 640, "height": 360 }
            }),
        );

        // BASE on the default video track 0: bars, full-canvas identity.
        dispatch(
            ctx,
            serde_json::json!({
                "type": "add_clip",
                "data": { "track": 0, "clip": {
                    "id": "L-base", "media_id": bars_item.id.clone(),
                    "start_us": 0, "in_us": 0, "out_us": 2_400_000i64,
                    "volume": 1.0, "audio_detached": false
                } }
            }),
        );

        // The SECOND video track via the REAL AddTrack command (video inserts
        // ON TOP: the new track lands at index 0, BASE shifts to 1).
        dispatch(
            ctx,
            serde_json::json!({ "type": "add_track", "data": { "kind": "video" } }),
        );

        // TOP (PIP) on the new top track, spanning [0.8s, 1.6s) — the overlap.
        dispatch(
            ctx,
            serde_json::json!({
                "type": "add_clip",
                "data": { "track": 0, "clip": {
                    "id": "L-pip", "media_id": tsrc_item.id.clone(),
                    "start_us": 800_000i64, "in_us": 0, "out_us": 800_000i64,
                    "volume": 1.0, "audio_detached": false
                } }
            }),
        );
        dispatch(
            ctx,
            serde_json::json!({
                "type": "set_clip_transform",
                "data": { "id": "L-pip", "transform": {
                    "position": [0.55, 0.55], "scale": [0.4, 0.4], "rotation_deg": 0.0
                } }
            }),
        );
    }

    /// Ported from src-tauri/src/lib.rs:13050 (Phase 55 plan 55-03, GATE-01) —
    /// TauriAppCtx→TestAppCtx is the ONLY change class (plus
    /// TauriPreviewHost→TestPreviewHost).
    ///
    /// 18.3-04 Task 1 (PERF-01, SC-R2): ring-produced multi-layer PARITY, MAD
    /// 0.0. The REAL background producer (`preview::spawn_producer`, production
    /// code — not a reconstruction) is started INSIDE the overlap of a REAL
    /// 2-track timeline; the first multi-layer entry it emits is byte-compared
    /// against a DIRECT composite of the SAME position. One composite path held:
    /// the ring relocates the CALL SITE of `composite_layers_to_rgba` to the
    /// producer thread, never what is computed.
    #[test]
    fn ring_multi_frame_parity_mad_zero() {
        // ONE GPU device at a time (D-1a). This test builds TWO compositors — the
        // producer's and a fresh ground-truth one — under this single lease; see
        // `test_support::GPU` for why the lease cannot live at the construction site.
        let _gpu = crate::test_support::gpu_lease();
        use std::sync::atomic::Ordering;

        let ctx = ctx();
        build_overlap_timeline(&ctx);

        // The producer thread outlives this call, so the host must be OWNED —
        // the same `Arc<dyn PreviewHost>` port the shell hands it (D-46-06-01).
        let host: Arc<dyn preview::PreviewHost> =
            Arc::new(TestPreviewHost::new(Arc::clone(&ctx)));

        let ctl = preview::RingCtl::new();
        // Root production INSIDE the overlap before the thread spawns
        // (restart_pos is stored BEFORE the gen bump — race-free).
        ctl.flush(1_000_000);
        let producer = preview::spawn_producer(
            Arc::clone(&host),
            ctl.clone(),
            Arc::new(engine::Compositor::new().expect("producer compositor")),
        );
        let cur_gen = ctl.gen.load(Ordering::SeqCst);
        let step = engine::frame_step_us(30.0);

        // Pop with an increasing target CAPPED inside the overlap: a cold
        // lockstep tick pushes NOTHING and advances, so the demand walks
        // forward — but never past the unmerge.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut target = 1_000_000i64;
        let entry = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the REAL producer must emit a multi-layer overlap entry within 30s"
            );
            if let Some(e) = ctl.pop_for_target(cur_gen, target) {
                break e;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            target = (target + step).min(1_599_999);
        };
        let p = entry.timeline_us;
        assert!(
            (1_000_000..1_600_000).contains(&p),
            "the popped entry ({p}µs) must sit inside the overlap [1.0s, 1.6s)"
        );
        assert_eq!(entry.gen, cur_gen, "entry stamped the current gen");
        let entry_frame = entry
            .frame()
            .expect("multi-layer ring entries are CPU composites");

        // DIRECT composite at the SAME position P.
        let stack =
            preview::resolve_multilayer(&*host, p).expect("P sits inside the multi-layer overlap");
        assert_eq!(stack.layers.len(), 2, "two video layers resolve at P");
        assert_eq!(
            (entry_frame.width, entry_frame.height),
            (stack.width, stack.height),
            "ring multi entries are PROJECT-res composites"
        );
        let truth_layers: Vec<engine::Layer> = stack
            .layers
            .iter()
            .map(|spec| {
                let frame = engine::decode_frame_rgba_at(&spec.path, spec.source_us, spec.rotation)
                    .expect("independent ground-truth decode");
                engine::Layer {
                    frame,
                    opacity: spec.opacity,
                    transform: spec.transform,
                    crop: spec.crop,
                    alpha_mode: engine::AlphaMode::Straight,
                }
            })
            .collect();
        let fresh = engine::Compositor::new().expect("fresh ground-truth compositor");
        let truth = fresh
            .composite_layers_to_rgba(&truth_layers, stack.width, stack.height)
            .expect("direct offscreen composite");

        let d_right = mad(&entry_frame.rgba, &truth);
        // Non-vacuity control: the TOP clip alone, stretched full-frame.
        let tsrc = fixture("testsrc_720p30_5s.mp4");
        let top_only =
            engine::decode_frame_rgba_at_scaled(Path::new(&tsrc), p - 800_000, 0, 640, 360)
                .expect("top-only control decode")
                .rgba;
        let d_top = mad(&entry_frame.rgba, &top_only);
        println!(
            "SC-R2 ring parity@{p}us: vs direct composite MAD = {d_right:.4}; \
             vs top-only control MAD = {d_top:.4}"
        );
        assert_eq!(
            d_right, 0.0,
            "SC-R2: a ring-produced multi-layer frame must be BYTE-IDENTICAL \
             (MAD 0.0, got {d_right:.4}) to the direct composite_layers_to_rgba \
             composite at the same position — one composite path"
        );
        assert!(
            d_top > EXPORT_WRONG_MAD,
            "parity must be non-vacuous: the composite must clearly differ from \
             the top layer alone (MAD {d_top:.4} <= {EXPORT_WRONG_MAD})"
        );

        ctl.request_stop();
        let reap = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !producer.is_finished() {
            assert!(
                std::time::Instant::now() < reap,
                "producer must finish within 10s of request_stop"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        producer.join().expect("producer thread joins cleanly");
    }

    // =======================================================================
    // 6/7 — keyframe / seek. AUTHORITATIVE (research A1).
    // =======================================================================

    /// SC-1/SC-4 shared fixture, ported alongside its test: `bars` placed WHOLE
    /// at 0 with a STATIC half-canvas scale (0.5,0.5) and a 2-key horizontal
    /// POSITION animation (frame 0 → x=0.0, frame 60 → x=0.5; smooth). Static
    /// scale is set BEFORE the keyframes so the transform-bundle D-06 clear
    /// cannot wipe the fresh track. Keys are authored via the REAL
    /// `set_keyframes` Command (never struct poking).
    fn seed_scaled_animated_position_clip(ctx: &TestAppCtx, bars: &str) -> String {
        let imported = import(ctx, &[bars.to_string()]);
        let media = imported[0].id.clone();
        let clip = place(ctx, &media, 0, 0);
        dispatch(
            ctx,
            serde_json::json!({
                "type": "set_clip_transform",
                "data": { "id": clip.id, "transform": {
                    "position": [0.0, 0.0], "scale": [0.5, 0.5], "rotation_deg": 0.0
                } }
            }),
        );
        dispatch(
            ctx,
            serde_json::json!({
                "type": "set_keyframes",
                "data": { "id": clip.id, "track": { "position": [
                    {"frame": 0, "value": [0.0, 0.0], "interp": "smooth"},
                    {"frame": 60, "value": [0.5, 0.0], "interp": "smooth"}
                ] } }
            }),
        );
        clip.id
    }

    /// Ported from src-tauri/src/lib.rs:14974 (Phase 55 plan 55-03, GATE-01) —
    /// TauriAppCtx→TestAppCtx is the ONLY change class (plus
    /// TauriPreviewHost→TestPreviewHost).
    ///
    /// SC-4: preview == export at the identical timestamp. The animated SC-1
    /// fixture is exported; at t=1.0s the decoded export frame is compared to
    /// the PREVIEW path's frame for the SAME timestamp — driven through the REAL
    /// `preview::resolve_multilayer` (so the comparison covers the preview's own
    /// fps read and `sample_at` call), then composited offscreen through the
    /// SAME `composite_layers_to_rgba` the surface path uses. Parity MAD ≤ 4.0
    /// (18-05 measured ~2.2); wrong-frame guard: the same export frame vs the
    /// preview composite at t=0 is ≥ 40.
    #[test]
    fn keyframe_preview_export_parity() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = ctx();
        let bars = fixture("bars_720p30_5s.mp4");
        let _clip = seed_scaled_animated_position_clip(&ctx, &bars);

        let out_path = tmp_export_path("kf_parity.mp4");
        export(&ctx, &out_path, 640, 360, 30.0);

        let host = TestPreviewHost::new(Arc::clone(&ctx));
        // Composited at the EXPORT resolution (640x360), not the project canvas
        // — position/scale/crop are FRACTIONAL, so the same layers rendered at
        // the export res are the apples-to-apples twin of the decoded export
        // frame (18-05 composited its twin at the export res identically).
        // ONE compositor, hoisted out of the closure. The `src-tauri` original
        // built a fresh `Compositor` (i.e. a fresh wgpu device) on each of its
        // two calls; that was harmless in a 193-test binary where it was the
        // only GPU tenant of the moment, and it is needless churn here, where
        // seven GPU-heavy twins share one test binary under the parallel
        // harness. Strictly fewer devices, identical pixels: the compositor is
        // stateless across `composite_layers_to_rgba` calls, which is exactly
        // why the ring producer is allowed to share one `Arc<Compositor>` with
        // the presenter in production.
        let compositor = engine::Compositor::new().expect("offscreen compositor");
        let preview_composite = |position_us: i64| -> Vec<u8> {
            let stack = preview::resolve_multilayer(&host, position_us)
                .expect("the animated clip resolves to the composite path");
            let mut layers: Vec<engine::Layer> = Vec::with_capacity(stack.layers.len());
            for spec in &stack.layers {
                let frame = engine::decode_frame_rgba_at(&spec.path, spec.source_us, spec.rotation)
                    .expect("decode preview layer source");
                layers.push(engine::Layer {
                    frame,
                    opacity: spec.opacity,
                    transform: spec.transform,
                    crop: spec.crop,
                    alpha_mode: engine::AlphaMode::Straight,
                });
            }
            compositor
                .composite_layers_to_rgba(&layers, 640, 360)
                .expect("offscreen preview composite")
        };

        let (w, h, export_1s) = decode_export_at(&out_path, 1_000_000);
        assert_eq!((w, h), (640, 360));
        let preview_1s = preview_composite(1_000_000);
        let d_parity = mad(&export_1s, &preview_1s);
        // Wrong-frame control: preview at t=0 (content un-shifted to x=0).
        let preview_0s = preview_composite(0);
        let d_wrong = mad(&export_1s, &preview_0s);
        println!(
            "SC-4 parity@1.0s: export vs preview MAD = {d_parity:.4}; wrong-frame \
             (export@1s vs preview@0s) MAD = {d_wrong:.4}"
        );
        assert!(
            d_parity <= 4.0,
            "preview and export must agree at the identical timestamp (MAD {d_parity:.4} > 4.0) \
             — same sampler, same project fps, SC-4"
        );
        assert!(
            d_wrong >= 40.0,
            "non-vacuous: the export frame must clearly DIFFER from the preview at a \
             different animation time (MAD {d_wrong:.4} < 40.0)"
        );
        let _ = std::fs::remove_file(&out_path);
    }

    // =======================================================================
    // 7/7 — overlay / text. MAD 0.0.
    // =======================================================================

    /// The Plan-03 auto-fit sentinel: `scale == (0,0)` ⇒ the renderer fits the
    /// text at its natural size (matches `rudis_core::tools::TEXT_AUTOFIT_TRANSFORM`).
    fn text_autofit_sentinel() -> rudis_core::ClipTransform {
        rudis_core::ClipTransform {
            position: (0.0, 0.0),
            scale: (0.0, 0.0),
            rotation_deg: 0.0,
        }
    }

    fn white_style(font_size: f32, wrap_width: Option<f32>) -> rudis_core::TextStyle {
        rudis_core::TextStyle {
            font_family: "Inter".to_string(),
            font_size,
            fill: [255, 255, 255, 255],
            bold: false,
            italic: false,
            align: rudis_core::TextAlign::Left,
            wrap_width,
        }
    }

    /// A TEXT clip literal (empty-sentinel media_id), ready for `Command::AddText`.
    fn make_text_clip(
        id: &str,
        content: &str,
        start_us: i64,
        dur_us: i64,
        transform: rudis_core::ClipTransform,
        style: rudis_core::TextStyle,
    ) -> rudis_core::Clip {
        rudis_core::Clip {
            id: id.to_string(),
            media_id: String::new(),
            start_us,
            in_us: 0,
            out_us: dur_us,
            volume: 1.0,
            audio_detached: false,
            transform,
            opacity: 1.0,
            crop: rudis_core::ClipCrop::default(),
            keyframes: Default::default(),
            text: Some(rudis_core::TextPayload {
                content: content.to_string(),
                style,
                caption_group_id: None,
            }),
            alpha_mode: Default::default(),
            retime: None,
        }
    }

    /// Ported from src-tauri/src/lib.rs:15698 (Phase 55 plan 55-03, GATE-01) —
    /// TauriAppCtx→TestAppCtx is the ONLY change class (plus
    /// TauriPreviewHost→TestPreviewHost).
    ///
    /// Task 2 (SC-1/SC-2 WYSIWYG): the preview play-path glue
    /// (`preview::resolve_multilayer` → `preview::compose_multilayer_from_pool`,
    /// which rasterizes text inline via the shared helper and NEVER sends it to
    /// the decoder pool) composites a text clip BYTE-IDENTICALLY to the export
    /// loop's own text branch at the same timestamp — MAD exactly 0.
    ///
    /// The twin's [`TestPreviewHost::rasterize_text`] is a pass-through to
    /// `crate::compose::rasterize_text_layer`, exactly as `TauriPreviewHost`'s
    /// is — which is why this MAD-0 assertion still holds after the substitution.
    #[test]
    fn text_preview_parity_mad_zero() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let ctx = ctx();
        let clip = make_text_clip(
            "txt",
            "Parity!",
            0,
            1_000_000,
            text_autofit_sentinel(),
            white_style(0.2, None),
        );
        crate::dispatch_command_inner(
            &*ctx,
            rudis_core::Command::AddText {
                track_index: 0,
                clip,
            },
        )
        .expect("AddText must dispatch");

        let t = 500_000i64;
        let host = TestPreviewHost::new(Arc::clone(&ctx));
        let stack = preview::resolve_multilayer(&host, t)
            .expect("a text clip must resolve to the composite path (never degenerate)");
        assert_eq!(stack.layers.len(), 1, "one text layer resolved");
        assert!(
            stack.layers[0].text.is_some(),
            "the resolved spec must be a TEXT spec"
        );
        let mut pool =
            engine::LayerDecoderPool::new(stack.fps, std::time::Duration::from_millis(100));
        let preview_layers = preview::compose_multilayer_from_pool(&host, &mut pool, &stack);
        assert_eq!(preview_layers.len(), 1, "the text layer survived pool-compose");
        let compositor = engine::Compositor::new().expect("offscreen compositor");
        let preview_rgba = compositor
            .composite_layers_to_rgba(&preview_layers, stack.width, stack.height)
            .expect("preview composite");

        // Export side: the export loop's text branch (same helper, same dims).
        let snap = snapshot(&ctx);
        let clip = snap.timeline.tracks[0]
            .clips
            .iter()
            .find(|c| c.id == "txt")
            .expect("text clip present")
            .clone();
        let text = clip.text.as_ref().expect("text payload");
        let mut rasterizer = engine::TextRasterizer::new();
        let export_layer = crate::compose::rasterize_text_layer(
            &mut rasterizer,
            text,
            engine::LayerTransform {
                position: clip.transform.position,
                scale: clip.transform.scale,
                rotation_deg: clip.transform.rotation_deg,
            },
            clip.opacity,
            engine::LayerCrop {
                left: clip.crop.left,
                top: clip.crop.top,
                right: clip.crop.right,
                bottom: clip.crop.bottom,
            },
            stack.width,
            stack.height,
        );
        let export_rgba = compositor
            .composite_layers_to_rgba(&[export_layer], stack.width, stack.height)
            .expect("export composite");

        let d = mad(&preview_rgba, &export_rgba);
        println!("text preview==export MAD = {d:.6}");
        assert_eq!(
            d, 0.0,
            "preview and export must be byte-identical through the ONE shared helper (MAD {d})"
        );
        // Non-vacuous: the text actually composited (not two black frames).
        let bright = preview_rgba.chunks_exact(4).filter(|p| p[0] > 180).count();
        assert!(
            bright > 20,
            "the text must be visibly composited ({bright} bright px)"
        );
    }
}

// ---------------------------------------------------------------------------
// V-10 (Phase 58, plan 58-08) — PROXY-04 / 58-CONTEXT D-20's STRUCTURAL half.
//
// Phase 58 gave playback a second thing it may decode: an all-intra proxy,
// chosen by `preview::decode_source::resolve_decode_source`. D-20 says export
// never consumes that seam, and says it is "proven, not asserted". This module
// is one of the two proofs — the static one. The other is the real-bytes one,
// `crates/ffi/tests/export_proxy_isolation.rs`: two exported FILES, one with a
// warm proxy cache and one with none, frame-diffed at MAD 0.0000. Neither is
// sufficient alone and the plan says so: a name scan cannot see a shared
// global, and a byte diff cannot see a door that is merely closed today.
// ---------------------------------------------------------------------------

/// V-10's committed source scan: export's PRODUCTION code cannot name the seam.
#[cfg(test)]
mod proxy_seam_scan {
    /// The seam's vocabulary. Every way this file could reach a proxy has to
    /// spell at least one of these:
    ///
    /// * `preview::` — the crate the resolver lives in (a real `[dependencies]`
    ///   edge of `app-core` since plan 58-05, which is exactly why a *text*
    ///   scan is now needed where a crate-graph wall used to do the work — see
    ///   `deferred-items.md § D-14` and this plan's D-14 substitution);
    /// * `decode_source` / `DecodeSourceKind` — the module and the answer type;
    /// * `proxy::` / `proxy_job` — the cache crate and the background job that
    ///   fills it, both of which `app-core` also depends on for real.
    ///
    /// Absent from the production half at HEAD, all five, with no
    /// comment-stripping needed (measured, not assumed — see the doc comment on
    /// the test below).
    ///
    /// (Deliberately NOT an intra-doc link to that test: this plan's acceptance
    /// gate counts occurrences of its identifier in this file and expects
    /// exactly one, so a rustdoc reference — the most natural thing to write
    /// here — would blunt the check. `deferred-items.md § D-10`, sixth
    /// occurrence in this phase and the reason that item exists.)
    const PROXY_SEAM_NEEDLES: [&str; 5] = [
        "preview::",
        "decode_source",
        "DecodeSourceKind",
        "proxy::",
        "proxy_job",
    ];

    /// Anchors proving the scan is reading the export code it claims to audit.
    /// 57-09's discipline (`EXPORT_SOURCES` in
    /// `crates/ffi/tests/export_parity.rs`): a scan whose "absent" findings are
    /// taken on trust is a scan whose findings are about the scanner.
    const PRODUCTION_ANCHORS: [&str; 3] = [
        "pub async fn run_export<C: AppCtx>(",
        "fn run_export_blocking(",
        "fn run_export_project<C: AppCtx>(",
    ];

    /// **V-10 (PROXY-04, 58-CONTEXT D-20).** The production half of this file —
    /// everything ABOVE the first `#[cfg(test)]` module — must never name the
    /// decode-source seam or the proxy cache. Export renders from ORIGINALS,
    /// and the structural reason it cannot do otherwise is that it cannot say
    /// the words.
    ///
    /// # Why the boundary is found by LINE, not by substring
    ///
    /// `58-RESEARCH.md` § RQ6 specifies the durable scan as
    /// `awk '/^#\[cfg\(test\)\]/{exit}'` — **line-anchored**. The convenience
    /// form (`src.split("#[cfg(test)]").next()`) is NOT equivalent in this
    /// file, and the difference was measured rather than reasoned about: this
    /// file carries the literal text `#[cfg(test)]` inside a **prose comment**
    /// 36 lines above the real module attribute, so a substring split stops
    /// early, at a comment, and would silently stop scanning any production
    /// code a later edit placed between the two. Measured at this HEAD (LF-
    /// normalised, bytes): line-anchored **82142**, substring **79440** — a
    /// **2702-byte** blind spot that no one would ever notice going wrong. The
    /// line-anchored form is the one whose name is true, and the printed
    /// `production=` byte count below is what makes the difference auditable
    /// rather than asserted.
    ///
    /// # Durability
    ///
    /// Line numbers drift; this does not. `include_str!` binds at COMPILE
    /// time, so renaming or moving this file is a build error rather than a
    /// silent pass, and the boundary is a syntactic landmark rather than a
    /// count. The vacuity guards below exist because eight of this test's
    /// assertions are of the form "this substring appears zero times", which is
    /// exactly the shape that also passes when the scan is pointed at an empty
    /// string (58-06's recorded lesson, one plan old).
    ///
    /// # If a future doc comment in the production half trips this
    ///
    /// Blank the comments before counting — `deferred-items.md § D-10`'s
    /// recorded fix, already applied twice in this phase
    /// (`crates/proxy/tests/cache.rs::code_lines`,
    /// `crates/preview/src/decode_source.rs`'s `code_only`) — rather than
    /// dropping the needle. Not done here because it is not yet needed: all
    /// five needles measure ZERO against the raw production text at HEAD, and
    /// an unused blanker is one more thing that can be silently wrong.
    #[test]
    fn export_production_code_never_references_the_proxy_seam() {
        // Compile-time bind: a renamed/moved file fails the BUILD.
        let src = include_str!("export.rs").replace("\r\n", "\n");

        // RQ6's boundary: the first line that STARTS with the attribute — not a
        // line that merely contains its text somewhere.
        let mut offset = 0usize;
        let mut boundary: Option<usize> = None;
        for line in src.split_inclusive('\n') {
            if line.starts_with("#[cfg(test)]") {
                boundary = Some(offset);
                break;
            }
            offset += line.len();
        }
        let boundary = boundary.expect(
            "export.rs has no line-initial `#[cfg(test)]` — the boundary technique no longer \
             matches this file's shape, and without a boundary this scan would either audit \
             the test code too (a permanent false red) or nothing at all",
        );
        let production = &src[..boundary];
        let tests = &src[boundary..];

        println!(
            "V10-SCAN export.rs production={} bytes tests={} bytes boundary_at_byte={boundary}",
            production.len(),
            tests.len()
        );

        // ---- Vacuity guards. -------------------------------------------
        assert!(
            production.len() > 10_000,
            "the production region is only {} bytes — the boundary split collapsed, so every \
             'absent' finding below would be a statement about an empty string",
            production.len()
        );
        for anchor in PRODUCTION_ANCHORS {
            assert!(
                production.contains(anchor),
                "`{anchor}` is not in the scanned production region — the scan is not reading \
                 the export code it names, so its findings are vacuous"
            );
        }

        // ---- POSITIVE CONTROL: the scanner CAN find what it forbids. ----
        // `preview::` genuinely occurs in this file's test half (the 55-03
        // parity twins import the headless preview composite as their ground
        // truth). If the needle were misspelled, or the boundary were placed
        // at the end of the file, this would be zero — and the whole scan
        // would be proving something about itself instead of about the code.
        // The `device.poll` lesson, 57-09: a needle the technique cannot find
        // is a needle it cannot forbid.
        let control_hits = tests.matches("preview::").count();
        println!("V10-SCAN control (test half): `preview::` x{control_hits}");
        assert!(
            control_hits > 0,
            "`preview::` does not appear in this file's TEST half either. Either the boundary \
             swallowed the whole file or the needle is misspelled; either way the zero-hit \
             findings below prove nothing about the production code"
        );

        // ---- The claim. -------------------------------------------------
        for needle in PROXY_SEAM_NEEDLES {
            let hits = production.matches(needle).count();
            println!("V10-SCAN production: `{needle}` x{hits}");
            assert_eq!(
                hits, 0,
                "`{needle}` appears in export.rs's PRODUCTION code — PROXY-04 structural \
                 breach. Export renders from ORIGINALS (58-CONTEXT D-20); the proxy is a \
                 PLAYBACK substitution and export must not be able to name the seam that \
                 chooses one. If this is a prose mention rather than a call, blank the \
                 comments before counting (deferred-items D-10) — do not drop the needle."
            );
        }
    }
}
