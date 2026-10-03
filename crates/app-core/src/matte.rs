//! The `create_matte` agent tool (Phase 27, LIB-03) — relocated here from
//! `src-tauri/src/lib.rs` by plan 45-09.
//!
//! # What moved, and the closure that says exactly this set
//!
//! `run_create_matte` + `handle_create_matte`, and nothing else: the pair owns
//! no private helper. The both-directions grep, measured before the cut:
//!
//! * **Outward** — `handle_create_matte` is reached only from `run_agent_turn`'s
//!   `"create_matte"` dispatch arm; `run_create_matte` only from that handler and
//!   from `src-tauri`'s `create_matte_gate`, which stays (it drives `place_clip`,
//!   `dispatch_command` and `export_timeline`).
//! * **Inward** — one leaf was NOT resident: `render_scene_frame`, which
//!   `run_generate_image` and `run_generate_video` also call. It moved to
//!   [`crate::compose`] alongside 45-05's five composite leaves rather than being
//!   duplicated here; `src-tauri` reaches it again through a re-export shim. See
//!   that module's header for the reasoning. Everything else was already
//!   resident: [`crate::import`]'s `next_id` / `ID_SEQ` / `poster_cache_dir`
//!   (45-07).
//!
//! **No new [`crate::AppCtx`] method was needed.** Per 45-08's carry-forward the
//! whole region was grepped for `app.path().` in full (not merely for managed
//! state): the single hit is `app_data_dir()`, which the trait has had since
//! 45-06 — and whose baked-in message is EXACTLY the one this call site built
//! inline, so the surfaced error text is byte-identical.
//!
//! # The conversion — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `fn f<R: tauri::Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` | `fn f<C: AppCtx>(ctx: &C, ..)` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first body line |
//! | `app.path().app_data_dir().map_err(..)` | `ctx.app_data_dir()` |
//! | `poster_cache_dir(&TauriAppCtx::new(app, store))` | `poster_cache_dir(ctx)` |
//! | `emit_changed(app, &patch, base_seq, seq)` | `ctx.emit_patch(&patch, base_seq, seq)` |
//!
//! Zero logic changed: the T-27-08 narrowed parse (no `elements` array is ever
//! accepted), the T-27-06 `SceneSpec::resolve()` DoS caps ahead of any encoder or
//! GPU allocation, the T-27-07 server-built `matte-{millis}-{seq}.mp4` filename
//! inside the confined `app_data_dir()/generated`, the `DEFAULT_VIDEO_ENCODER`
//! debug-assert (SC: a matte encodes through the SAME license-safe encoder export
//! uses), the silent-WAV synthesis and its best-effort cleanup, and the
//! orphan-file removal on a failed dispatch are all byte-identical.

use std::sync::atomic::Ordering;

use rudis_core::{Command, MediaBinItem, MediaKind};

use crate::compose::render_scene_frame;
use crate::import::{next_id, poster_cache_dir, ID_SEQ};
use crate::AppCtx;

/// Phase 27 (LIB-03): the `create_matte` interception — render a solid-color or
/// linear-gradient full-canvas background through the PROVEN `generate_video`
/// pipeline (`render_scene_frame` per-tick loop -> [`engine::VideoEncoder`]) into
/// a real, NON-ZERO-duration `MediaKind::Video` asset. This is Pattern 2 (research
/// Pitfall 1): a matte is a TRIVIAL scene spec (background-only, `elements` always
/// empty) driven through the SAME encoder/dispatch path a full generated video
/// uses — never a still image. A still image ALWAYS probes to `duration_us == 0`,
/// and every clip-construction path (`Command::AddClip`, `place_clip`) rejects
/// `out_us <= in_us`, so a still-image matte would be UNPLACEABLE (the confirmed
/// Phase-24 `generate_image` gap). Producing a real Video sidesteps that entirely.
///
/// Steps 4-9 below are byte-identical to `run_generate_video`'s (silent WAV ->
/// VideoEncoder -> per-tick render loop -> finish -> probe -> poster ->
/// AddMediaBinItem dispatch); ONLY steps 1-3 differ (narrower parse: no `elements`
/// array is ever accepted, T-27-08) and the server-built filename is `matte-`
/// rather than `video-`. The matte honors this tool's own `folder` param (the
/// AddMediaBinItem folder-existence check from Plan 27-01 rejects a bad folder,
/// T-27-07). The DoS caps (T-27-06) are enforced by `SceneSpec::resolve()` BEFORE
/// any encoder/GPU resource is allocated — zero new cap logic.
pub fn run_create_matte<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<MediaBinItem, String> {
    // Plan 45-09 (45-06's recipe, unchanged): the `store: &SharedStore`
    // parameter became `ctx.store()`, bound on the first body line, so both
    // `store.lock()` calls below are byte-identical to the pre-move code.
    let store = ctx.store();

    // 1. Project defaults under a SHORT lock (D-07), DROPPED before render work —
    //    matte width/height/fps default to the ACTIVE PROJECT's own settings when
    //    omitted, mirroring run_generate_video.
    let (default_w, default_h, default_fps) = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        let project = guard.snapshot();
        (project.width, project.height, project.fps)
    };

    // 2. Parse ONLY the fields this narrower schema exposes — no `elements` array
    //    is ever accepted (a matte has none, by construction, T-27-08). A missing
    //    durationSeconds/background is a clean Err BEFORE any encode/GPU work.
    let duration_seconds = input
        .get("durationSeconds")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| "durationSeconds is required".to_string())?;
    let background_val = input
        .get("background")
        .ok_or_else(|| "background is required".to_string())?;
    let background: rudis_core::SceneBackground =
        serde_json::from_value(background_val.clone())
            .map_err(|e| format!("invalid background: {e}"))?;
    let folder = input
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let width = input.get("width").and_then(|v| v.as_u64()).map(|w| w as u32);
    let height = input
        .get("height")
        .and_then(|v| v.as_u64())
        .map(|h| h as u32);

    // 3. Build the SAME SceneSpec generate_video consumes — elements ALWAYS empty
    //    (a matte is background-only) — then resolve (require_duration=true; the
    //    MAX_SCENE_DIM / MAX_SCENE_DURATION_SECONDS / MAX_SCENE_TOTAL_RASTER_PIXELS
    //    DoS caps are enforced HERE, before any Frame/encoder allocation, T-27-06).
    let spec = rudis_core::SceneSpec {
        width,
        height,
        fps: None,
        duration_seconds: Some(duration_seconds),
        background,
        elements: vec![],
    };
    let resolved = spec
        .resolve(default_w, default_h, default_fps, /*require_duration*/ true)
        .map_err(|e| e.to_string())?;

    // Confined output dir + SERVER-BUILT filename (`matte-{millis}-{seq}.mp4`; the
    // create_matte schema carries no path arg, so no LLM value reaches the write
    // path — T-27-07).
    // `TauriAppCtx::app_data_dir` bakes in EXACTLY the message this call site
    // built inline (`"resolve app data dir: {e}"`), checked against `ctx.rs`
    // before this conversion -- so the surfaced string is byte-identical.
    let dir = ctx.app_data_dir()?.join("generated");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create generated dir: {e}"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let out_path = dir.join(format!("matte-{millis}-{seq}.mp4"));

    // 4. SILENT AUDIO FIRST (Pitfall 2 / T-24-15): VideoEncoder::new ALWAYS opens
    //    an `-i <audio_wav>` input, but a matte has no audio — synthesize a
    //    zero-filled mono f32 WAV spanning the whole duration.
    let num_samples =
        ((resolved.duration_us as f64 / 1_000_000.0) * engine::AUDIO_SAMPLE_RATE as f64).ceil()
            as usize;
    let silent = vec![0.0f32; num_samples];
    let wav_path = std::env::temp_dir().join(format!(
        "rudis-matte-{millis}-{}-{seq}.wav",
        std::process::id()
    ));
    engine::write_wav_mono_f32(&silent, &wav_path)
        .map_err(|e| format!("write silent audio wav: {e}"))?;

    // 5-6. VideoEncoder::new + the up-to-30s per-tick render+encode loop +
    //   finish() — run under block_in_place on a multi-thread runtime (mirrors
    //   run_generate_video's own guard; inline under the tokio-less MockRuntime).
    //   The matte's per-tick frame is background-only (spec.elements is empty).
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
        .map_err(|e| format!("start matte encoder: {e}"))?;
        // Widened at 60-02 (HWENC-01) for the same reason as
        // `generate_video`'s twin in `export.rs`: export now resolves a
        // preference ladder, so naming `DEFAULT_VIDEO_ENCODER` outright had
        // become an assertion about this machine's GPU. The licensing claim is
        // made directly, and the "same encoder export uses" claim is made by
        // comparing against export's own resolution for this canvas.
        let encoder_name_used = encoder.encoder_name().to_string();
        debug_assert!(
            engine::is_nvenc_video_encoder(&encoder_name_used)
                || engine::is_mf_video_encoder(&encoder_name_used),
            "create_matte must encode via a cleared hardware family (CLAUDE.md \
             rule 6), got {encoder_name_used:?}"
        );
        #[cfg(debug_assertions)]
        if let Ok(bins) = engine::locate() {
            let expected =
                engine::export_encoder_preference(&bins, resolved.width, resolved.height)
                    .unwrap_or(engine::DEFAULT_VIDEO_ENCODER);
            debug_assert_eq!(
                encoder_name_used, expected,
                "create_matte must encode via the SAME license-safe encoder export uses"
            );
        }

        let compositor =
            engine::Compositor::new().map_err(|e| format!("start scene compositor: {e}"))?;
        let mut text_rasterizer = engine::TextRasterizer::new();
        let step_us = engine::frame_step_us(resolved.fps).max(1);
        let mut t = 0i64;
        while t < resolved.duration_us {
            // transparent=false (unchanged): a matte is re-encoded to H.264/HEVC
            // via VideoEncoder, which has no alpha channel at all -- must stay
            // on the locked D-01 opaque-black clear.
            let rgba = render_scene_frame(&compositor, &mut text_rasterizer, &resolved, t, false)?;
            encoder
                .push_frame(&rgba)
                .map_err(|e| format!("push matte frame at {t}us: {e}"))?;
            t += step_us;
        }
        encoder
            .finish()
            .map_err(|e| format!("finish matte encode: {e}"))?;
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
    // Best-effort silent-WAV cleanup regardless of outcome.
    let _ = std::fs::remove_file(&wav_path);
    if let Err(e) = encode_result {
        let _ = std::fs::remove_file(&out_path);
        return Err(e);
    }

    // 7. Probe the JUST-WRITTEN file (every MediaBinItem field measured) + extract
    //    a mid-duration poster (poster failure downgrades to None, never aborts).
    let info = match engine::probe(&out_path) {
        Ok(info) => info,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(format!("probe matte video: {e}"));
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
                eprintln!("create_matte: no poster for {}: {e}", out_path.display());
                None
            }
        }
    };

    // 8. A real, probed Video MediaBinItem (never rotated). This tool's OWN
    //    `folder` param is honored (unlike generate_video's always-"" folder).
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
        folder,
        display_name: None,
        is_image_sequence: false,
        // Phase 60 (OCCL-01): from this asset's OWN probe, through the one
        // narrowing every import surface uses. A matte is written by us but
        // still measured, never assumed.
        reports_alpha: crate::import::probed_alpha(&info),
    };

    // 9. Backend-owned + undoable: the SAME dispatch path generate_video/
    //    import_media use, so the new matte auto-joins the open agent turn and
    //    emits project:changed. A nonexistent `folder` is rejected here by Plan
    //    27-01's AddMediaBinItem::apply folder check (T-27-07). On dispatch
    //    failure the just-written MP4 + poster are orphaned — best-effort remove.
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
            let _ = std::fs::remove_file(&item.path);
            if let Some(pp) = &item.poster_path {
                let _ = std::fs::remove_file(pp);
            }
            return Err(e);
        }
    };
    ctx.emit_patch(&patch, base_seq, seq)?;
    Ok(item)
}

/// Phase 27 (LIB-03): the `create_matte` interception. Never panics: a missing
/// `durationSeconds`/`background`, an invalid background, a rejected folder, or an
/// encode/probe failure becomes an `is_error` text tool_result — never a partial
/// mutation (resolve() + the whole encode run BEFORE the AddMediaBinItem dispatch).
pub fn handle_create_matte<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_create_matte(ctx, input) {
        Ok(item) => result(
            format!(
                "Created a {}x{} matte video asset ({}s, {}), added to the media bin as {}.",
                item.width,
                item.height,
                item.duration_us as f64 / 1_000_000.0,
                item.path,
                item.id
            ),
            None,
        ),
        Err(e) => result(format!("create_matte failed: {e}"), Some(true)),
    }
}
