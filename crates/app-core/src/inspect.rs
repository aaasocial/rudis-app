//! The agent's two READ-ONLY "eyes" tools -- `inspect_timeline` (Phase 21,
//! EYES-01) and `inspect_media` (EYES-02) -- relocated verbatim from
//! `src-tauri/src/lib.rs` by plan 45-05 (Phase 45, XTRC-01), the phase's FIRST
//! real extraction batch.
//!
//! Neither takes `AppHandle`/`State`, so nothing here needs an [`crate::AppCtx`]
//! implementation: the whole surface is `&SharedStore` + `&Mutex<AgentSession>`,
//! both of which 45-03/45-04 had already relocated. That is what made this the
//! lowest-blast-radius batch to prove the move/shim/gate mechanic on.
//!
//! `src-tauri` keeps its old call sites through a `pub use app_core::{..}` shim,
//! so `run_agent_turn`'s tool-dispatch match is byte-identical to before.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::compose::{
    black_rgba, decode_clip_frame, engine_alpha_mode, rasterize_text_layer, timeline_clip,
};
use crate::session::{AgentSession, InspectTimelineCache};
use crate::SharedStore;

/// Phase 21 (EYES-01): render ONE timeline timestamp through the SAME
/// multi-layer composite sequence `run_export_blocking`'s per-tick branch
/// uses (`Timeline::active_layers_at` -> per-hit text/media `Layer` build ->
/// `Compositor::composite_layers_to_rgba`) -- the "same composite path as
/// export" contract. A ONE-SHOT render: unlike the export loop, no
/// `ExportRunDecoder` streaming reuse is needed (that amortizes ffmpeg
/// spawns across MANY output ticks; an `inspect_timeline` call renders
/// exactly one).
///
/// Returns `(rgba, visible_clip_ids)`: tightly-packed RGBA bytes
/// (`width*height*4`) and the visible clip ids TOP-FIRST -- verbatim
/// `active_layers_at`'s own order (never re-sorted/reversed here, per its
/// ORDERING CONTRACT doc comment, crates/core/src/model.rs:182-193).
pub fn render_timeline_inspect_frame(
    timeline: &rudis_core::Timeline,
    media: &std::collections::HashMap<String, (PathBuf, u32, bool, f64)>,
    project_fps: f64,
    width: u32,
    height: u32,
    position_us: i64,
) -> Result<(Vec<u8>, Vec<String>), String> {
    let t = position_us.max(0);
    let hits = timeline.active_layers_at(t);
    let visible_clip_ids: Vec<String> = hits.iter().map(|h| h.clip_id.clone()).collect();

    let mut text_rasterizer = engine::TextRasterizer::new();
    let mut layers: Vec<engine::Layer> = Vec::with_capacity(hits.len());
    for hit in &hits {
        let Some(clip) = timeline_clip(timeline, &hit.clip_id) else {
            continue; // unreachable: the hit came from this timeline
        };
        let sampled = clip.sample_at(t - clip.start_us, project_fps);
        if let Some(text) = &clip.text {
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
        let Some((path, rotation, is_seq, seq_fps)) = media.get(&hit.media_id) else {
            continue; // dangling/missing media: contributes nothing, black shows through
        };
        // Phase 28 (OVL-01, SC-4): decode via the ONE shared helper — a numbered
        // image sequence resolves through `decode_frame_rgba_at_seq` at project
        // fps, the SAME path the export loop takes (inspect == export == preview).
        let frame = decode_clip_frame(path, hit.source_us, *rotation, *is_seq, *seq_fps).map_err(|e| {
            format!(
                "inspect decode layer {} ({}) at {}us: {e}",
                hit.clip_id,
                path.display(),
                hit.source_us
            )
        })?;
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
            // Phase 28 (OVL-01): thread this media clip's alpha interpretation
            // through the inspect composite path too, so an inspected overlay
            // frame matches the export/preview composite (one composite path).
            alpha_mode: engine_alpha_mode(clip.alpha_mode),
        });
    }
    let rgba = if layers.is_empty() {
        black_rgba(width, height)
    } else {
        let compositor = engine::Compositor::new().map_err(|e| format!("start inspect compositor: {e}"))?;
        compositor
            .composite_layers_to_rgba(&layers, width, height)
            .map_err(|e| format!("composite inspect frame at {t}us: {e}"))?
    };
    Ok((rgba, visible_clip_ids))
}

// ---------------------------------------------------------------------------
// Phase 21 (EYES-01/EYES-02): the agent-eyes interception handlers. Both are
// routed through the SAME Pattern-C mechanism send_feedback/export_project use
// (is_intercepted_meta_tool + the apply_round meta match). Per 21-RESEARCH.md's
// simplification note, NEITHER needs `AppHandle<R>` (unlike the feedback/export
// handlers): inspect_timeline needs only `&SharedStore` + `&Mutex<AgentSession>`
// (for its one-slot cache), inspect_media needs only `&SharedStore`.
// ---------------------------------------------------------------------------

/// Phase 21 (Pitfall 3): hash the WHOLE Timeline + project width/height/fps
/// as one unit -- OVER-invalidates on any unrelated edit (acceptable; SC-3
/// only requires never UNDER-invalidating). Deliberately excludes
/// canvas/playback/source_playback/preview_mode -- none of those affect
/// what active_layers_at/composite_layers_to_rgba render.
fn inspect_state_hash(timeline: &rudis_core::Timeline, width: u32, height: u32, fps: f64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(bytes) = serde_json::to_vec(timeline) {
        bytes.hash(&mut hasher);
    }
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    fps.to_bits().hash(&mut hasher);
    hasher.finish()
}

/// Phase 21: resolve `(project, media_map)` for an inspect_* call under a
/// SHORT store lock, then DROP the guard before any decode/composite work
/// -- the same resolve-then-drop discipline resolve_export_plan/
/// preview_timeline_at already established. Resolves EVERY media_id any
/// clip references (not just those active at one timestamp), mirroring
/// resolve_export_plan's own media map build.
fn resolve_inspect_context(
    store: &SharedStore,
) -> Result<(rudis_core::Project, std::collections::HashMap<String, (PathBuf, u32, bool, f64)>), String> {
    let guard = store.lock().map_err(|_| "backend store mutex poisoned".to_string())?;
    let project = guard.snapshot();
    let media_ids: std::collections::HashSet<String> = project
        .timeline
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
    Ok((project, media))
}

const INSPECT_TIMELINE_JPEG_MAX_EDGE: u32 = 512;
pub const INSPECT_MEDIA_FRAME_MAX_EDGE: u32 = 512;
const INSPECT_STORYBOARD_MAX_EDGE: u32 = 384;
pub const INSPECT_JPEG_QUALITY: u8 = 70;
const INSPECT_STORYBOARD_DEFAULT_COUNT: usize = 6;
const INSPECT_STORYBOARD_MAX_COUNT: usize = 12; // T-21-01: hard ceiling, never agent-controlled beyond this

/// Phase 21 (EYES-01): render the current timeline at the agent-requested (or
/// playhead-default) position, size-clamp-encode it to JPEG, cache it, and
/// return `(text, jpeg)`. The agent's `positionUs` is clamped server-side into
/// `[0, duration_us]` before any decode (T-21-02); the one-slot cache hits only
/// when BOTH the whole-timeline state hash AND the exact position match.
pub fn run_inspect_timeline(
    store: &SharedStore,
    session: &Mutex<AgentSession>,
    input: &serde_json::Value,
) -> Result<(String, Vec<u8>), String> {
    let (project, media) = resolve_inspect_context(store)?;
    let duration_us = project.timeline.duration_us();
    let requested = input.get("positionUs").and_then(|v| v.as_i64());
    // T-21-02: clamp server-side, never trust an agent value raw into decode.
    let position_us = requested
        .unwrap_or(project.playback.position_us)
        .clamp(0, duration_us.max(0));

    let state_hash = inspect_state_hash(&project.timeline, project.width, project.height, project.fps);
    {
        let sess = session.lock().map_err(|_| "agent session poisoned".to_string())?;
        if let Some(cached) = &sess.inspect_timeline_cache {
            if cached.state_hash == state_hash && cached.position_us == position_us {
                return Ok((cached.text.clone(), cached.jpeg.clone()));
            }
        }
    }

    let (rgba, visible_clip_ids) = render_timeline_inspect_frame(
        &project.timeline, &media, project.fps, project.width, project.height, position_us,
    )?;
    let frame = engine::Frame { width: project.width, height: project.height, rgba };
    let jpeg = engine::encode_jpeg_bytes(&frame, INSPECT_TIMELINE_JPEG_MAX_EDGE, INSPECT_JPEG_QUALITY)
        .map_err(|e| format!("encode inspect_timeline JPEG: {e}"))?;
    let ids_json = serde_json::to_string(&visible_clip_ids).unwrap_or_default();
    let text = format!("t={position_us}us. visible clip ids (top-first): {ids_json}");

    {
        let mut sess = session.lock().map_err(|_| "agent session poisoned".to_string())?;
        sess.inspect_timeline_cache = Some(InspectTimelineCache {
            state_hash, position_us, text: text.clone(), jpeg: jpeg.clone(),
        });
    }
    Ok((text, jpeg))
}

/// Phase 21 (EYES-01): the `inspect_timeline` interception. Never panics: any
/// failure becomes an `is_error` text tool_result. Success is TEXT-then-IMAGE
/// (a single base64 JPEG) via `image_tool_result` (SC-5).
pub fn handle_inspect_timeline(
    store: &SharedStore,
    session: &Mutex<AgentSession>,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content, is_error| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(), content, is_error,
    };
    match run_inspect_timeline(store, session, input) {
        Ok((text, jpeg)) => result(agent_llm::vision::image_tool_result(text, &jpeg), None),
        Err(e) => result(agent_llm::vision::text_tool_result(e), Some(true)),
    }
}

/// Phase 21 (EYES-02): decode a MediaBin SOURCE asset (never the timeline, never
/// a compositor) by id in one of two modes -- single "frame" (one JPEG at a
/// clamped `timestampUs`) or "storyboard" (N evenly-spaced JPEGs, N clamped to a
/// hard ceiling of `INSPECT_STORYBOARD_MAX_COUNT` regardless of the request,
/// T-21-01). Returns `(text, jpegs)`; an unknown `mediaId` returns `Err`.
pub fn run_inspect_media(store: &SharedStore, input: &serde_json::Value) -> Result<(String, Vec<Vec<u8>>), String> {
    let media_id = input
        .get("mediaId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "inspect_media requires mediaId".to_string())?
        .to_string();
    let item = {
        let guard = store.lock().map_err(|_| "backend store mutex poisoned".to_string())?;
        guard
            .media_item(&media_id)
            .cloned()
            .ok_or_else(|| format!("no media bin item with id {media_id}"))?
    };
    let path = PathBuf::from(&item.path);
    let mode = input.get("mode").and_then(|v| v.as_str()).unwrap_or("frame");

    if mode == "storyboard" {
        let requested = input.get("frameCount").and_then(|v| v.as_u64()).unwrap_or(INSPECT_STORYBOARD_DEFAULT_COUNT as u64);
        let n = (requested.max(1) as usize).min(INSPECT_STORYBOARD_MAX_COUNT); // T-21-01 clamp
        let mut jpegs = Vec::with_capacity(n);
        let mut timestamps = Vec::with_capacity(n);
        for i in 0..n {
            let t = if n <= 1 { 0 } else { item.duration_us.max(0) * i as i64 / (n as i64 - 1) };
            let frame = engine::decode_frame_rgba_at(&path, t, item.rotation_degrees)
                .map_err(|e| format!("inspect_media storyboard decode at {t}us: {e}"))?;
            let jpeg = engine::encode_jpeg_bytes(&frame, INSPECT_STORYBOARD_MAX_EDGE, INSPECT_JPEG_QUALITY)
                .map_err(|e| format!("encode storyboard JPEG: {e}"))?;
            jpegs.push(jpeg);
            timestamps.push(t);
        }
        let text = format!(
            "media {media_id} ({}x{}, {}us): storyboard of {n} frames at {timestamps:?}us",
            item.width, item.height, item.duration_us
        );
        Ok((text, jpegs))
    } else {
        let requested = input.get("timestampUs").and_then(|v| v.as_i64()).unwrap_or(0);
        let t = requested.clamp(0, item.duration_us.max(0)); // T-21-02 clamp
        let frame = engine::decode_frame_rgba_at(&path, t, item.rotation_degrees)
            .map_err(|e| format!("inspect_media decode at {t}us: {e}"))?;
        let jpeg = engine::encode_jpeg_bytes(&frame, INSPECT_MEDIA_FRAME_MAX_EDGE, INSPECT_JPEG_QUALITY)
            .map_err(|e| format!("encode inspect_media JPEG: {e}"))?;
        let text = format!("media {media_id} ({}x{}, {}us) at t={t}us", item.width, item.height, item.duration_us);
        Ok((text, vec![jpeg]))
    }
}

/// Phase 21 (EYES-02): the `inspect_media` interception. Never panics: any
/// failure becomes an `is_error` text tool_result. Success is TEXT-then-IMAGE(S)
/// (one base64 JPEG in frame mode, N in storyboard mode) via `images_tool_result`.
pub fn handle_inspect_media(store: &SharedStore, tool_use_id: &str, input: &serde_json::Value) -> agent_llm::ContentBlock {
    let result = |content, is_error| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(), content, is_error,
    };
    match run_inspect_media(store, input) {
        Ok((text, jpegs)) => result(agent_llm::vision::images_tool_result(text, &jpegs), None),
        Err(e) => result(agent_llm::vision::text_tool_result(e), Some(true)),
    }
}

/// Phase 21 (EYES-01) gate: `render_timeline_inspect_frame` is proven to render
/// ONE timeline timestamp through the EXACT SAME multi-layer composite path
/// export's per-tick branch uses — a PURE function, no Store/App/AppHandle. Each
/// test builds a `rudis_core::Timeline` directly + a pre-resolved media map (the
/// same already-resolved inputs `run_export_blocking`'s loop body operates on)
/// and frame-diffs against an INDEPENDENTLY-built export-path composite. MAD 0.0
/// (EXACT, pre-JPEG-encode) is the "same composite path" contract; the returned
/// clip-id order is `active_layers_at`'s own top-first order verbatim.
#[cfg(test)]
mod inspect_render_gate {
    use crate::test_support::*;
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Build a single fully-explicit `rudis_core::Clip` (the 18-03 fixture rule:
    /// never rely on struct defaults for the fields under test) spanning
    /// [0, 1_200_000)us with no keyframes and no text — a plain media clip.
    fn inspect_clip(
        id: &str,
        media_id: &str,
        transform: rudis_core::ClipTransform,
        opacity: f32,
        crop: rudis_core::ClipCrop,
    ) -> rudis_core::Clip {
        rudis_core::Clip {
            id: id.to_string(),
            media_id: media_id.to_string(),
            start_us: 0,
            in_us: 0,
            out_us: 1_200_000,
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

    /// SC-1 (pre-encode exact parity) + SC-2 (clip-id top-first ordering): a
    /// 2-video-track stack (a transformed PIP on top of a full-canvas base)
    /// renders byte-identically to an independently-built export-path composite.
    #[test]
    fn render_timeline_inspect_frame_matches_an_independent_offscreen_composite() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let tsrc = fixture("testsrc_720p30_5s.mp4");
        let bars = fixture("bars_720p30_5s.mp4");

        // TOP: testsrc as a non-identity PIP; BASE: bars full-canvas identity.
        let top = inspect_clip(
            "L-top",
            "m-top",
            rudis_core::ClipTransform {
                position: (0.1, 0.1),
                scale: (0.4, 0.4),
                rotation_deg: 0.0,
            },
            1.0,
            rudis_core::ClipCrop::default(),
        );
        let base = inspect_clip(
            "L-base",
            "m-base",
            rudis_core::ClipTransform::default(),
            1.0,
            rudis_core::ClipCrop::default(),
        );
        // Two VIDEO tracks: track 0 = TOP (active_layers_at scans in `tracks`
        // order, index-0 = top-most), track 1 stacks beneath it.
        let timeline = rudis_core::Timeline {
            tracks: vec![
                rudis_core::Track {
                    kind: rudis_core::TrackKind::Video,
                    clips: vec![top.clone()],
                },
                rudis_core::Track {
                    kind: rudis_core::TrackKind::Video,
                    clips: vec![base.clone()],
                },
            ],
        };
        let mut media: HashMap<String, (PathBuf, u32, bool, f64)> = HashMap::new();
        media.insert("m-top".to_string(), (PathBuf::from(&tsrc), 0, false, 0.0));
        media.insert("m-base".to_string(), (PathBuf::from(&bars), 0, false, 0.0));

        let t = 500_000i64;
        let (rgba, visible_clip_ids) =
            render_timeline_inspect_frame(&timeline, &media, 30.0, 640, 360, t)
                .expect("inspect render must succeed");

        // SC-2: verbatim active_layers_at order — TOP first, never reversed.
        assert_eq!(
            visible_clip_ids,
            vec!["L-top".to_string(), "L-base".to_string()],
            "visible clip ids must be active_layers_at's own top-first order"
        );

        // Independent export-path ground truth: decode + map each layer the
        // IDENTICAL field-for-field way, composite via a freshly-built
        // Compositor's SAME composite_layers_to_rgba call the function uses.
        let layer_for = |clip: &rudis_core::Clip, path: &str| -> engine::Layer {
            let source_us = clip.in_us + (t - clip.start_us);
            let frame = engine::decode_frame_rgba_at(std::path::Path::new(path), source_us, 0)
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
        };
        let layers = vec![layer_for(&top, &tsrc), layer_for(&base, &bars)];
        let compositor = engine::Compositor::new().expect("offscreen compositor");
        let truth = compositor
            .composite_layers_to_rgba(&layers, 640, 360)
            .expect("offscreen ground-truth composite");

        // SC-1 pre-JPEG-encode half: EXACT parity (== 0.0), not a bounded
        // tolerance (21-RESEARCH.md Pitfall 1 — Plan 03 proves the lossy half).
        assert_eq!(
            mad(&rgba, &truth),
            0.0,
            "inspect render must be byte-identical to the independent export-path \
             composite (MAD 0.0) — the same-composite-path contract, pre-encode"
        );
    }

    /// A timestamp with zero active video layers renders the SAME opaque-black
    /// canvas export's gap frames use, with an empty clip-id list.
    #[test]
    fn render_timeline_inspect_frame_at_an_empty_moment_is_black_with_no_visible_clips() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let timeline = rudis_core::Timeline { tracks: vec![] };
        let media: HashMap<String, (PathBuf, u32, bool, f64)> = HashMap::new();
        let (rgba, visible_clip_ids) =
            render_timeline_inspect_frame(&timeline, &media, 30.0, 640, 360, 500_000)
                .expect("empty-moment inspect render must succeed");
        assert_eq!(
            rgba,
            black_rgba(640, 360),
            "an empty moment must render the same opaque-black gap frame export uses"
        );
        assert!(
            visible_clip_ids.is_empty(),
            "an empty moment must report no visible clips"
        );
    }

    /// A SINGLE identity clip (no stacking) renders at MAD 0.0 vs a plain
    /// `decode_frame_rgba_at` of its source at the resolved `source_us` — the
    /// function ALWAYS composites (no "degenerate" special-case), matching
    /// run_export_blocking's own no-special-case design. Rendered at the
    /// source's own native dims (1280x720) so there is zero contain-fit
    /// letterbox ambiguity: an identity full-canvas layer reduces exactly to
    /// the decoded frame.
    #[test]
    fn render_timeline_inspect_frame_single_identity_layer_matches_plain_decode() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        let bars = fixture("bars_720p30_5s.mp4");
        let clip = inspect_clip(
            "L-only",
            "m-only",
            rudis_core::ClipTransform::default(),
            1.0,
            rudis_core::ClipCrop::default(),
        );
        let timeline = rudis_core::Timeline {
            tracks: vec![rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: vec![clip.clone()],
            }],
        };
        let mut media: HashMap<String, (PathBuf, u32, bool, f64)> = HashMap::new();
        media.insert("m-only".to_string(), (PathBuf::from(&bars), 0, false, 0.0));

        let t = 500_000i64;
        let (rgba, visible_clip_ids) =
            render_timeline_inspect_frame(&timeline, &media, 30.0, 1280, 720, t)
                .expect("single-layer inspect render must succeed");
        assert_eq!(visible_clip_ids, vec!["L-only".to_string()]);

        // Plain decode of the SAME source at the resolved source_us.
        let source_us = clip.in_us + (t - clip.start_us);
        let plain = engine::decode_frame_rgba_at(std::path::Path::new(&bars), source_us, 0)
            .expect("plain decode reference");
        assert_eq!(
            mad(&rgba, &plain.rgba),
            0.0,
            "a single identity layer at native dims must reduce EXACTLY to the \
             decoded frame (MAD 0.0) — the function always composites, no special-case"
        );
    }

    /// A visible TEXT clip composites through the SAME `rasterize_text_layer`
    /// helper export's text branch uses — its glyphs are reachable in the
    /// output (not silently dropped) and it appears in the visible clip ids.
    /// Uses the clip's DEFAULT transform (scale (1.0,1.0), an EXPLICIT
    /// full-canvas placement — NOT the auto-fit `scale == (0,0)` sentinel).
    #[test]
    fn render_timeline_inspect_frame_includes_a_visible_text_clip() {
        let _gpu = crate::test_support::gpu_lease(); // ONE GPU device at a time (D-1a)
        // A text clip: media_id is the empty-string sentinel; text clips never
        // look up `media`, so the map stays empty.
        let clip = rudis_core::Clip {
            id: "L-text".to_string(),
            media_id: String::new(),
            start_us: 0,
            in_us: 0,
            out_us: 1_000_000,
            volume: 1.0,
            audio_detached: false,
            transform: rudis_core::ClipTransform::default(),
            opacity: 1.0,
            crop: rudis_core::ClipCrop::default(),
            keyframes: Default::default(),
            text: Some(rudis_core::TextPayload::new("HELLO")),
            alpha_mode: Default::default(),
            retime: None,
        };
        let timeline = rudis_core::Timeline {
            tracks: vec![rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: vec![clip.clone()],
            }],
        };
        let media: HashMap<String, (PathBuf, u32, bool, f64)> = HashMap::new();

        let (rgba, visible_clip_ids) =
            render_timeline_inspect_frame(&timeline, &media, 30.0, 640, 360, 500_000)
                .expect("text-clip inspect render must succeed");
        assert_eq!(
            visible_clip_ids,
            vec!["L-text".to_string()],
            "the text clip must be reported visible"
        );
        // The glyphs actually rasterized into the composite: at least one pixel
        // is NOT the opaque-black background the empty canvas would be.
        assert!(
            rgba.chunks_exact(4).any(|px| px != [0, 0, 0, 255]),
            "a visible text clip must contribute non-black pixels — the text \
             branch must genuinely composite, not be skipped"
        );
    }
}

/// Phase 21 (EYES-01/EYES-02) gate: the REAL Pattern-C handlers
/// (`handle_inspect_timeline`/`handle_inspect_media`) exercised end-to-end
/// against a bare `SharedStore` (`Mutex<Store>`, NO Tauri App/MockRuntime) —
/// proving SC-1's bounded-lossy post-JPEG-encode MAD (structurally distinct
/// from Plan 02's exact pre-encode `== 0.0`), SC-2's clip-id text end-to-end
/// through the real handler, and SC-5's wire-shape guarantee (every content
/// block is `text` or a base64 `image`, never a raw pixel buffer). Project dims
/// are 512x288 so `encode_jpeg_bytes(_, 512, _)` performs NO resize — the
/// re-decoded JPEG dims equal the pre-encode composite dims exactly, so the MAD
/// isolates ONLY the lossy JPEG compression, not a resize.
#[cfg(test)]
pub(crate) mod inspect_wiring_gate {
    use crate::test_support::*;
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use std::path::Path;

    const W: u32 = 512;
    const H: u32 = 288;
    const T: i64 = 500_000;

    /// A plain media clip spanning [0, 1_200_000)us, every field explicit.
    pub(super) fn wclip(
        id: &str,
        media_id: &str,
        transform: rudis_core::ClipTransform,
        opacity: f32,
        crop: rudis_core::ClipCrop,
    ) -> rudis_core::Clip {
        rudis_core::Clip {
            id: id.to_string(),
            media_id: media_id.to_string(),
            start_us: 0,
            in_us: 0,
            out_us: 1_200_000,
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

    /// A real MediaBin SOURCE item pointing at a fixture (720p30 5s video).
    pub(super) fn witem(id: &str, path: &str) -> rudis_core::MediaBinItem {
        rudis_core::MediaBinItem {
            id: id.to_string(),
            path: path.to_string(),
            media_kind: rudis_core::MediaKind::Video,
            duration_us: 5_000_000,
            width: 1280,
            height: 720,
            fps: 30.0,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: true,
            poster_path: None,
            folder: String::new(),
            display_name: None,
            is_image_sequence: false,
            reports_alpha: None,
        }
    }

    /// Build a bare `SharedStore` (no Tauri App) around a 2-video-track stack —
    /// L-top testsrc PIP over L-base bars full-canvas — plus a bare
    /// `Mutex<AgentSession>`. Returns the fixture paths for ground-truth builds.
    /// The SAME 2-video-track stack (L-top testsrc PIP over L-base bars
    /// full-canvas) as [`stacked_store`], but with the two source media paths
    /// supplied by the caller — so a test can point the media bin at TEMP
    /// copies it is free to delete (the cache-hit test in
    /// `inspect_cache_and_media_gate` does exactly that). The ONE store builder;
    /// `stacked_store` is a thin fixture-path wrapper over it.
    pub(crate) fn stacked_store_from_paths(
        top_path: &str,
        base_path: &str,
    ) -> (SharedStore, Mutex<AgentSession>) {
        let top = wclip(
            "L-top",
            "m-top",
            rudis_core::ClipTransform {
                position: (0.1, 0.1),
                scale: (0.4, 0.4),
                rotation_deg: 0.0,
            },
            1.0,
            rudis_core::ClipCrop::default(),
        );
        let base = wclip(
            "L-base",
            "m-base",
            rudis_core::ClipTransform::default(),
            1.0,
            rudis_core::ClipCrop::default(),
        );

        // Deserialize a minimal project skeleton (handles every serde default),
        // then install the explicit timeline + media_bin + dims.
        let mut project: rudis_core::Project = serde_json::from_value(serde_json::json!({
            "media_bin": [],
            "timeline": { "tracks": [] },
            "fps": 30.0,
            "width": W,
            "height": H
        }))
        .expect("project skeleton deserializes");
        project.timeline = rudis_core::Timeline {
            tracks: vec![
                rudis_core::Track {
                    kind: rudis_core::TrackKind::Video,
                    clips: vec![top],
                },
                rudis_core::Track {
                    kind: rudis_core::TrackKind::Video,
                    clips: vec![base],
                },
            ],
        };
        project.media_bin = vec![witem("m-top", top_path), witem("m-base", base_path)];

        let store: SharedStore = Mutex::new(rudis_core::Store::from_project(project));
        let session = Mutex::new(AgentSession::default());
        (store, session)
    }

    fn stacked_store() -> (SharedStore, Mutex<AgentSession>, String, String) {
        let tsrc = fixture("testsrc_720p30_5s.mp4");
        let bars = fixture("bars_720p30_5s.mp4");
        let (store, session) = stacked_store_from_paths(&tsrc, &bars);
        (store, session, tsrc, bars)
    }

    /// Build the INDEPENDENT pre-encode ground-truth composite at (W, H) the
    /// SAME way Plan 02's inspect_render_gate does: decode each source at its
    /// resolved source_us, map field-for-field, composite via a fresh
    /// Compositor's SAME `composite_layers_to_rgba`.
    fn ground_truth_rgba(tsrc: &str, bars: &str) -> Vec<u8> {
        let layer = |path: &str, position: (f32, f32), scale: (f32, f32)| -> engine::Layer {
            // in_us=0, start_us=0 => source_us == T.
            let frame = engine::decode_frame_rgba_at(Path::new(path), T, 0)
                .expect("decode ground-truth layer source");
            engine::Layer {
                frame,
                opacity: 1.0,
                transform: engine::LayerTransform {
                    position,
                    scale,
                    rotation_deg: 0.0,
                },
                crop: engine::LayerCrop::default(),
                alpha_mode: engine::AlphaMode::Straight,
            }
        };
        let layers = vec![
            layer(tsrc, (0.1, 0.1), (0.4, 0.4)),
            layer(bars, (0.0, 0.0), (1.0, 1.0)),
        ];
        let compositor = engine::Compositor::new().expect("offscreen compositor");
        compositor
            .composite_layers_to_rgba(&layers, W, H)
            .expect("offscreen ground-truth composite")
    }

    /// Decode a base64 JPEG payload back to tightly-packed RGBA bytes.
    pub(crate) fn jpeg_b64_to_rgba(data: &str) -> (u32, u32, Vec<u8>) {
        let bytes = STANDARD.decode(data).expect("valid base64");
        let img = image::load_from_memory(&bytes).expect("decodes as a valid JPEG");
        let rgba = img.to_rgba8();
        (rgba.width(), rgba.height(), rgba.into_raw())
    }

    /// SC-1 (bounded lossy MAD) + SC-2 (clip-id text) through the REAL handler.
    #[test]
    fn inspect_timeline_returns_text_then_image_within_bounded_lossy_tolerance() {
        // ONE GPU device at a time (D-1a). Two paths reach a device from here —
        // `handle_inspect_timeline` and `ground_truth_rgba` — under this one lease.
        let _gpu = crate::test_support::gpu_lease();
        let (store, session, tsrc, bars) = stacked_store();
        let block = handle_inspect_timeline(
            &store,
            &session,
            "tu-1",
            &serde_json::json!({ "positionUs": T }),
        );

        let (content, is_error) = match block {
            agent_llm::ContentBlock::ToolResult { content, is_error, .. } => (content, is_error),
            other => panic!("expected ToolResult, got {other:?}"),
        };
        assert!(is_error.is_none(), "a real render must not be an error");
        assert_eq!(content.len(), 2, "text block then ONE image block");

        // SC-2: the text block names BOTH visible clip ids, top-first.
        let text = match &content[0] {
            agent_llm::ToolResultBlock::Text { text } => text.clone(),
            other => panic!("content[0] must be Text, got {other:?}"),
        };
        assert!(text.contains("L-top"), "text must name L-top: {text}");
        assert!(text.contains("L-base"), "text must name L-base: {text}");
        let top_at = text.find("L-top").unwrap();
        let base_at = text.find("L-base").unwrap();
        assert!(top_at < base_at, "clip ids must be top-first in the text: {text}");

        // SC-1 lossy half: re-decode the JPEG and MAD-compare against the
        // independent pre-encode composite.
        let (jw, jh, jpeg_rgba) = match &content[1] {
            agent_llm::ToolResultBlock::Image { source } => {
                assert_eq!(source.media_type, "image/jpeg", "must be a JPEG");
                jpeg_b64_to_rgba(&source.data)
            }
            other => panic!("content[1] must be Image, got {other:?}"),
        };
        // No resize happened (512x288 long edge == 512), so dims match exactly.
        assert_eq!((jw, jh), (W, H), "no resize: JPEG dims equal project dims");

        let truth = ground_truth_rgba(&tsrc, &bars);
        let measured = mad(&jpeg_rgba, &truth);
        println!("inspect_timeline post-JPEG MAD = {measured}");
        assert!(
            measured > 0.0,
            "MAD must be > 0.0 — genuine lossy compression, not an accidental no-op"
        );
        assert!(
            measured <= 8.0,
            "MAD must be bounded (<= 8.0) — JPEG quality 70 is only mildly lossy; got {measured}"
        );
    }

    /// SC-5 wiring: single-frame mode returns exactly [Text, Image].
    #[test]
    fn inspect_media_frame_mode_returns_one_text_and_one_image() {
        let (store, _session, _tsrc, _bars) = stacked_store();
        let block = handle_inspect_media(&store, "tu-2", &serde_json::json!({ "mediaId": "m-top" }));
        let content = match block {
            agent_llm::ContentBlock::ToolResult { content, is_error, .. } => {
                assert!(is_error.is_none(), "frame-mode decode must not error");
                content
            }
            other => panic!("expected ToolResult, got {other:?}"),
        };
        assert_eq!(content.len(), 2, "one text + one image");
        match &content[1] {
            agent_llm::ToolResultBlock::Image { source } => {
                let bytes = STANDARD.decode(&source.data).expect("valid base64");
                assert!(
                    image::load_from_memory(&bytes).is_ok(),
                    "the image must decode to valid, non-empty bytes"
                );
            }
            other => panic!("content[1] must be Image, got {other:?}"),
        }
    }

    /// SC-5 + T-21-01: storyboard mode returns ONE text then N images, and a
    /// requested frameCount of 4 yields exactly 4 images (5 blocks total).
    #[test]
    fn inspect_media_storyboard_mode_returns_n_images_after_one_text() {
        let (store, _session, _tsrc, _bars) = stacked_store();
        let block = handle_inspect_media(
            &store,
            "tu-3",
            &serde_json::json!({ "mediaId": "m-top", "mode": "storyboard", "frameCount": 4 }),
        );
        let content = match block {
            agent_llm::ContentBlock::ToolResult { content, is_error, .. } => {
                assert!(is_error.is_none(), "storyboard decode must not error");
                content
            }
            other => panic!("expected ToolResult, got {other:?}"),
        };
        assert_eq!(content.len(), 5, "1 text + 4 images for frameCount:4");
        assert!(
            matches!(content[0], agent_llm::ToolResultBlock::Text { .. }),
            "content[0] must be the ONE text block"
        );
        for (i, blk) in content[1..].iter().enumerate() {
            match blk {
                agent_llm::ToolResultBlock::Image { source } => {
                    let bytes = STANDARD.decode(&source.data).expect("valid base64");
                    assert!(
                        image::load_from_memory(&bytes).is_ok(),
                        "storyboard image {i} must decode to valid bytes"
                    );
                }
                other => panic!("content[{}] must be Image, got {other:?}", i + 1),
            }
        }
    }

    /// SC-5 (structural): for EVERY inspect ToolResult, serialize to JSON and
    /// assert the ONLY content-block shapes are `text` or base64 `image` —
    /// structurally, a raw/unencoded pixel buffer can never appear on the wire.
    #[test]
    fn inspect_results_never_carry_a_raw_pixel_buffer_on_the_wire() {
        let (store, session, _tsrc, _bars) = stacked_store();
        let results = vec![
            handle_inspect_timeline(&store, &session, "tu-a", &serde_json::json!({ "positionUs": T })),
            handle_inspect_media(&store, "tu-b", &serde_json::json!({ "mediaId": "m-top" })),
            handle_inspect_media(
                &store,
                "tu-c",
                &serde_json::json!({ "mediaId": "m-top", "mode": "storyboard", "frameCount": 4 }),
            ),
        ];
        for block in &results {
            let v = serde_json::to_value(block).expect("ToolResult serializes");
            let content = v["content"].as_array().expect("content is an array, never a bare buffer");
            assert!(!content.is_empty(), "every result carries at least a text block");
            for entry in content {
                let ty = entry["type"].as_str().expect("every block has a type");
                assert!(
                    ty == "text" || ty == "image",
                    "the ONLY wire shapes are text|image, never a raw buffer; got {ty}"
                );
                if ty == "image" {
                    assert_eq!(entry["source"]["type"], "base64", "images are base64 only");
                    let mt = entry["source"]["media_type"].as_str().expect("image media_type");
                    assert!(mt.starts_with("image/"), "image media_type must be image/*: {mt}");
                }
            }
        }
    }
}

/// Phase 21 Plan 04 (EYES-01/EYES-02) gate: the two correctness properties Plan
/// 03's one-slot cache design must NEVER violate — SC-3 (never a stale frame
/// after a real mutation; a genuine cache hit when nothing changed) and SC-4
/// (`inspect_media` genuinely decodes the SOURCE asset at the requested
/// timestamp, discriminated against wrong-timestamp/wrong-source controls). Each
/// test reuses `inspect_wiring_gate`'s ONE store builder (`stacked_store_from_paths`)
/// and its JPEG-round-trip helper — no forked fixture, no duplicated compositor.
#[cfg(test)]
mod inspect_cache_and_media_gate {
    use super::inspect_wiring_gate::{jpeg_b64_to_rgba, stacked_store_from_paths};
    use crate::test_support::*; // fixture, mad
    use super::*;
    use std::path::Path;

    /// Extract the single base64-JPEG payload from a NON-error inspect ToolResult
    /// (panics if it errored or carries no image block — an errored second call in
    /// the cache-hit test would surface here as a loud failure, exactly as SC-3
    /// wants).
    fn image_b64(block: &agent_llm::ContentBlock) -> String {
        match block {
            agent_llm::ContentBlock::ToolResult { content, is_error, .. } => {
                assert!(is_error.is_none(), "inspect must not be an error: {content:?}");
                content
                    .iter()
                    .find_map(|c| match c {
                        agent_llm::ToolResultBlock::Image { source } => Some(source.data.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("no image block in {content:?}"))
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// SC-3 (never stale): a REAL `SetClipOpacity` mutation between two
    /// `inspect_timeline` calls at the SAME `positionUs` MUST yield DIFFERENT
    /// frames — the exact regression 21-RESEARCH.md Pitfall 3 says the cache
    /// design must never reproduce (byte-identical bytes after a mutation).
    #[test]
    fn inspect_timeline_cache_invalidates_after_a_real_mutation() {
        let (store, session) = stacked_store_from_paths(
            &fixture("testsrc_720p30_5s.mp4"),
            &fixture("bars_720p30_5s.mp4"),
        );
        let before = image_b64(&handle_inspect_timeline(
            &store,
            &session,
            "tu-a",
            &serde_json::json!({ "positionUs": 500_000 }),
        ));

        // A REAL mutation: drop the top PIP clip's opacity 1.0 -> 0.3. This
        // changes composited pixels in the PIP region AND changes the whole-
        // timeline state hash, so the one-slot cache MUST invalidate.
        store
            .lock()
            .unwrap()
            .dispatch(rudis_core::Command::SetClipOpacity {
                id: "L-top".to_string(),
                opacity: 0.3,
            })
            .expect("dispatch opacity change");

        let after = image_b64(&handle_inspect_timeline(
            &store,
            &session,
            "tu-b",
            &serde_json::json!({ "positionUs": 500_000 }),
        ));

        assert_ne!(
            before, after,
            "SC-3: after a real mutation, re-inspecting the SAME position must \
             return a DIFFERENT frame — never the stale cached bytes"
        );
    }

    /// SC-3 (genuine hit): with NO mutation between two calls at the SAME
    /// position, the second call must return byte-identical bytes FROM THE CACHE.
    /// Proven non-vacuously by making a fresh recompute IMPOSSIBLE — the media is
    /// deleted after the first call, so a real decode would `Err` (21-RESEARCH.md
    /// Pitfall 3's loud, non-timing-based cache-is-real proof).
    #[test]
    fn inspect_timeline_cache_hits_when_nothing_changed() {
        // Per-test-unique temp copies so we can DELETE the media without ever
        // touching the shared test-media/ fixtures other parallel tests depend on.
        let pid = std::process::id();
        let top = std::env::temp_dir().join(format!("rudis-inspect-cache-{pid}-top.mp4"));
        let base = std::env::temp_dir().join(format!("rudis-inspect-cache-{pid}-base.mp4"));
        std::fs::copy(fixture("testsrc_720p30_5s.mp4"), &top).expect("copy top fixture");
        std::fs::copy(fixture("bars_720p30_5s.mp4"), &base).expect("copy base fixture");

        let (store, session) =
            stacked_store_from_paths(&top.to_string_lossy(), &base.to_string_lossy());

        // First call: a genuine render -> populates the one-slot cache.
        let first = image_b64(&handle_inspect_timeline(
            &store,
            &session,
            "hit-1",
            &serde_json::json!({ "positionUs": 500_000 }),
        ));

        // Make a fresh recompute IMPOSSIBLE: delete the underlying media.
        std::fs::remove_file(&top).expect("remove top temp copy");
        std::fs::remove_file(&base).expect("remove base temp copy");

        // Second call: SAME position, NO mutation -> must be a cache hit. If the
        // cache regressed, run_inspect_timeline would try to decode the deleted
        // file and return an is_error result -> image_b64 panics.
        let second = image_b64(&handle_inspect_timeline(
            &store,
            &session,
            "hit-2",
            &serde_json::json!({ "positionUs": 500_000 }),
        ));

        assert_eq!(
            first, second,
            "an unchanged re-inspect with the media deleted must return \
             byte-identical cached bytes — a genuine cache hit, not a lucky recompute"
        );
    }

    /// SC-4: `inspect_media`'s decoded, size-clamped JPEG genuinely matches the
    /// real SOURCE asset at the requested timestamp (bounded lossy+resize MAD),
    /// and is CLEARLY discriminated (much higher MAD) against a wrong-timestamp
    /// control (same source, different time) AND a wrong-source control
    /// (different source, same time). testsrc is used as the media because it
    /// animates over time, so the wrong-timestamp control is non-vacuous.
    #[test]
    fn inspect_media_frame_mode_matches_the_real_source_and_is_discriminating() {
        let tsrc = fixture("testsrc_720p30_5s.mp4");
        let bars = fixture("bars_720p30_5s.mp4");
        let (store, _session) = stacked_store_from_paths(&tsrc, &bars);

        // inspect_media decodes the SOURCE asset (m-top = testsrc) at 1.0s.
        let block = handle_inspect_media(
            &store,
            "tu-m",
            &serde_json::json!({ "mediaId": "m-top", "timestampUs": 1_000_000 }),
        );
        let (jw, jh, decoded) = jpeg_b64_to_rgba(&image_b64(&block));

        // Independent decodes, each scaled to the JPEG's exact dims so `mad`'s
        // equal-length invariant holds and only content differs.
        let scaled = |path: &str, t: i64| -> Vec<u8> {
            engine::decode_frame_rgba_at_scaled(Path::new(path), t, 0, jw, jh)
                .unwrap_or_else(|e| panic!("scaled decode {path} @{t}us: {e}"))
                .rgba
        };
        let d_right = mad(&decoded, &scaled(&tsrc, 1_000_000)); // right source, right time
        let d_wrong_time = mad(&decoded, &scaled(&tsrc, 4_000_000)); // right source, WRONG time
        let d_wrong_src = mad(&decoded, &scaled(&bars, 1_000_000)); // WRONG source, right time
        println!(
            "inspect_media SC-4 MADs: right={d_right:.4} wrong_time={d_wrong_time:.4} wrong_src={d_wrong_src:.4}"
        );

        // Mirror export_gate's MATCH/WRONG floor METHOD (not its exact numbers —
        // JPEG q70 @512 + an ffmpeg-vs-image resampler delta is a different lossy
        // budget than an H.264 export). Measured on this fixture set: right=1.23,
        // wrong_time=21.44, wrong_src=121.75 — a floor of 15.0 sits comfortably
        // above the ~1.2 match ceiling AND below the smallest wrong control.
        const INSPECT_MEDIA_MATCH_MAD: f64 = 8.0;
        const INSPECT_MEDIA_WRONG_MAD: f64 = 15.0;
        assert!(
            d_right <= INSPECT_MEDIA_MATCH_MAD,
            "the decoded frame matches the real source (MAD {d_right:.4})"
        );
        assert!(
            d_wrong_time >= INSPECT_MEDIA_WRONG_MAD,
            "a wrong TIMESTAMP is clearly discriminated (MAD {d_wrong_time:.4})"
        );
        assert!(
            d_wrong_src >= INSPECT_MEDIA_WRONG_MAD,
            "a wrong SOURCE is clearly discriminated (MAD {d_wrong_src:.4})"
        );
        assert!(
            d_right < d_wrong_time && d_right < d_wrong_src,
            "the right source+time must be the closest match of the three"
        );
    }
}
