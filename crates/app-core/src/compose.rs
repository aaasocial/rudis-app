//! Pure, shell-agnostic composite/decode leaf helpers, relocated verbatim from
//! `src-tauri/src/lib.rs` by plan 45-05 (Phase 45, XTRC-01).
//!
//! # Why these are here, when the plan named only the inspect/transcribe leaves
//!
//! 45-05's stated closure ("9 functions + their exclusively-owned private
//! helpers") is incomplete as written: `render_timeline_inspect_frame` -- an
//! exclusively-owned helper of `run_inspect_timeline` -- calls FIVE helpers that
//! `src-tauri` shares widely (`timeline_clip` 17 sites, `rasterize_text_layer`
//! 12, `decode_clip_frame` 10, `engine_alpha_mode` 6, `black_rgba` 5, several of
//! them from `native_surface.rs`). `app-core` cannot depend back on `src-tauri`,
//! so those five had to travel with the batch; `src-tauri` reaches them again
//! through a `pub use app_core::{..}` shim and every one of its call sites
//! compiles unchanged. `text_align_to_engine` and `clamp_scale` came along as
//! `rasterize_text_layer`'s own exclusive callees and stay module-private.
//!
//! Everything here is a PURE function over `rudis_core` + `engine` types with no
//! `Store`, no `AppCtx` and no `tauri` -- which is why the move is mechanical.
//! Batches 45-06..45-13 (the export/preview/overlay leaves) call the same five,
//! so this module is where a later batch should look before duplicating one.

use std::path::Path;

/// Phase 28 (OVL-01, T-28-15): the ONE frame-decode entry point shared by EVERY
/// production composite/export site. A normal clip decodes via
/// [`engine::decode_frame_rgba_at`]; an imported numbered image sequence
/// (`is_image_sequence`, whose `path` is a confined `%0Nd` pattern) decodes via
/// [`engine::decode_frame_rgba_at_seq`] at its stored project fps. Keeping the
/// branch in ONE helper means preview, inspect and export can never diverge on
/// how a sequence frame is resolved — WYSIWYG by construction. A corrupt member
/// frame fails cleanly via the sidecar `Result` path (T-28-14), never a panic.
pub fn decode_clip_frame(
    path: &Path,
    source_us: i64,
    rotation: u32,
    is_image_sequence: bool,
    seq_fps: f64,
) -> Result<engine::Frame, engine::EngineError> {
    if is_image_sequence {
        engine::decode_frame_rgba_at_seq(path, source_us, seq_fps)
    } else {
        engine::decode_frame_rgba_at(path, source_us, rotation)
    }
}

/// Locate a clip anywhere on the timeline by id (the export-side twin of
/// core's private `locate()`), for reading its Phase-18 visual properties.
pub fn timeline_clip<'a>(
    timeline: &'a rudis_core::Timeline,
    clip_id: &str,
) -> Option<&'a rudis_core::Clip> {
    timeline
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .find(|c| c.id == clip_id)
}

/// Map core's `TextAlign` to the engine's rasterizer primitive (the two enums
/// are mirrored — core cannot depend on the engine, Plan 02/01).
fn text_align_to_engine(a: rudis_core::TextAlign) -> engine::TextAlign {
    match a {
        rudis_core::TextAlign::Left => engine::TextAlign::Left,
        rudis_core::TextAlign::Center => engine::TextAlign::Center,
        rudis_core::TextAlign::Right => engine::TextAlign::Right,
    }
}

/// Phase 28 (OVL-01): map a clip's core `AlphaMode` to the engine mirror the
/// compositor's `fs_layer` shader reads (via `Layer.alpha_mode` → `misc.y`).
/// A TOTAL match over the two-variant enum (no wildcard) — the compiler forces
/// any future variant to be handled here (threat T-28-08), so a media clip's
/// alpha interpretation can never silently fall back to `Straight`. This is the
/// ONE app-layer core→engine hop that connects the domain toggle (Plan 02) to
/// the shader branch (Plan 03) across the shared preview/export composite path.
pub fn engine_alpha_mode(m: rudis_core::AlphaMode) -> engine::AlphaMode {
    match m {
        rudis_core::AlphaMode::Straight => engine::AlphaMode::Straight,
        rudis_core::AlphaMode::Premultiplied => engine::AlphaMode::Premultiplied,
    }
}

/// Clamp an LLM-authored float to a finite, non-negative value (T-20-07). A
/// non-finite / negative scale collapses to 0.0 (a no-op-ish layer, matching
/// the compositor's own finiteness guard) rather than corrupting the raster.
fn clamp_scale(v: f32) -> f32 {
    if v.is_finite() && v >= 0.0 {
        v
    } else {
        0.0
    }
}

/// THE single shared text-layer builder (Phase 20, TEXT-01). Called from BOTH
/// the export loop AND both preview present paths so a text overlay is
/// composited byte-identically in preview and export (SC-1/SC-2 WYSIWYG).
///
/// Takes the ALREADY-mapped engine `transform`/`opacity`/`crop` (both call
/// sites resolve these from the SAME `Clip::sample_at` and map them
/// field-for-field the identical way — the export loop inline, the preview via
/// `LayerSpec`) so the two sites feed byte-identical inputs and the returned
/// `Layer` is byte-identical ⇒ the composite is identical ⇒ preview == export.
///
/// Rasterizes `text` into a natural-size STRAIGHT-alpha `engine::Frame` (the
/// compositor premultiplies in-shader — never premultiply here) and places it:
///
/// - **Auto-fit sentinel** (Plan 03 `TEXT_AUTOFIT_TRANSFORM`): when the
///   resolved transform's `scale == (0.0, 0.0)` the caller left placement to
///   the renderer → the frame is placed at its NATURAL size (dest rect =
///   `raster_w/project_w × raster_h/project_h`) anchored TOP-LEFT `(0.0, 0.0)`
///   (Open-Q3, Rudis's normalized top-left convention). A content edit re-fits
///   to the new natural size without ever jumping position.
/// - **Explicit transform**: any other scale is honored verbatim (pinned
///   placement / keyframes animate for free via `sample_at`).
///
/// `px = font_size * project_h` (Plan 01: font_size is a fraction of canvas
/// HEIGHT); `wrap_px = wrap_width * project_w` (fraction of canvas WIDTH). The
/// rasterizer clamps absurd `px`/dims itself (T-20-01); the computed auto-fit
/// scale is additionally clamped finite/≥0 here (T-20-07).
pub fn rasterize_text_layer(
    rasterizer: &mut engine::TextRasterizer,
    text: &rudis_core::TextPayload,
    transform: engine::LayerTransform,
    opacity: f32,
    crop: engine::LayerCrop,
    project_w: u32,
    project_h: u32,
) -> engine::Layer {
    let style = &text.style;
    let px = style.font_size * project_h as f32;
    let wrap_px = style.wrap_width.map(|f| f * project_w as f32);
    let align = text_align_to_engine(style.align);
    let frame = rasterizer.rasterize_text(
        &text.content,
        px,
        style.fill,
        style.bold,
        style.italic,
        align,
        wrap_px,
    );

    // Auto-fit sentinel (scale == (0,0)) ⇒ place at natural size, top-left;
    // any other transform ⇒ honor verbatim (identical detection in export and
    // preview because both resolve `transform` via the same `Clip::sample_at`).
    //
    // SENTINEL CONTRACT (M-01): `scale == (0,0)` is UNAMBIGUOUS — it is only ever
    // reachable by OMITTING the transform on a text clip (tools.rs stamps
    // TEXT_AUTOFIT_TRANSFORM). An EXPLICIT (0,0) is rejected upstream at BOTH the
    // add_texts tool layer AND Command::SetClipTransform's text-clip validation
    // (which also gates update_text), so a "shrink to nothing" request can never
    // be silently reinterpreted as "auto-place at natural size" here. A zero-area
    // dest rect renders nothing, so rejecting explicit (0,0) loses no capability.
    let placed = if transform.scale == (0.0, 0.0) {
        let pw = project_w.max(1) as f32;
        let ph = project_h.max(1) as f32;
        engine::LayerTransform {
            position: (0.0, 0.0),
            scale: (
                clamp_scale(frame.width as f32 / pw),
                clamp_scale(frame.height as f32 / ph),
            ),
            rotation_deg: 0.0,
        }
    } else {
        engine::LayerTransform {
            position: transform.position,
            scale: (clamp_scale(transform.scale.0), clamp_scale(transform.scale.1)),
            rotation_deg: transform.rotation_deg,
        }
    };

    engine::Layer {
        frame,
        opacity,
        transform: placed,
        crop,
        // Text layers are straight-alpha by convention (Phase 20: "never
        // premultiply here" — the shader premultiplies once). Straight.
        alpha_mode: engine::AlphaMode::Straight,
    }
}

/// Opaque black RGBA (tightly packed, `w*h*4`) — the export flavor of the
/// gap frame `preview_timeline_at` serves, so a timeline gap exports as an
/// actual black frame rather than aborting or repeating stale content.
pub fn black_rgba(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for px in rgba.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }
    rgba
}

// ---------------------------------------------------------------------------
// Plan 45-09 addition. `render_scene_frame` is the SIXTH shared composite leaf
// to land here, and for exactly 45-05's reason: `matte::run_create_matte` calls
// it, `app-core` cannot depend back on `src-tauri`, and it is NOT exclusively
// owned by the batch -- `run_generate_image` and `run_generate_video` (still in
// `src-tauri`, they move in 45-11) each call it too. So it travels with the
// batch and `src-tauri` reaches it again through a `pub use app_core::` shim,
// with both remaining call sites compiling unchanged. Its own callee
// `rasterize_text_layer` was already right here from 45-05.
// ---------------------------------------------------------------------------

/// Phase 24 (ASSET-01/02): render ONE scene-spec tick into composited RGBA,
/// through the SAME `Compositor::composite_layers_to_rgba`[`_transparent`]
/// pair every other layer consumer uses (never a bespoke flatten routine).
/// Shared by `generate_image` (called once, `t_us = 0`), `generate_video`'s
/// per-tick loop, and `create_matte`'s per-tick loop (research Pattern 3:
/// "generate_image = render ONE tick; generate_video = the SAME per-tick loop
/// already runs").
///
/// Stacking (Pitfall 1): the spec's authoring order lists elements bottom-first
/// AMONG THEMSELVES, but the compositor treats index-0 as TOP-MOST, so elements
/// are pushed in REVERSE; the background is ALWAYS pushed LAST (bottom-most of
/// everything), proven end-to-end by `generate_image_pixels_match_spec`.
///
/// `transparent` (debug session `preview-overlay-generation-opaque-background`,
/// 2026-07-24): selects which of the two D-01/OVL-03 sibling entry points
/// clears the canvas. `false` (the ORIGINAL, unchanged behavior) clears to
/// OPAQUE BLACK via `composite_layers_to_rgba` -- required for `generate_video`
/// and `create_matte`, whose output is re-encoded to H.264/HEVC (no alpha
/// channel exists there at all, and the D-01 opaque-black convention must never
/// change for anything that feeds preview/export WYSIWYG). `true` clears to a
/// genuinely TRANSPARENT canvas via the ALREADY-SHIPPED, ALREADY-TESTED
/// `composite_layers_to_rgba_transparent` (built for OVL-03's
/// `export_overlay_asset`, same premultiplied-over blend, only the clear color
/// differs) -- used ONLY by `run_generate_image`'s still-PNG path, which never
/// touches a video encoder and can freely carry real alpha. For a scene whose
/// background + every element are fully opaque (the overwhelming existing
/// case: solid/gradient title cards, lower-thirds, shape graphics), the two
/// paths are BYTE-IDENTICAL -- the bottom-most, canvas-filling background
/// layer's own alpha=255 fully overwrites whatever the clear color was,
/// regardless of which one it is. The visible difference appears ONLY when the
/// resolved background/element colors are themselves partially or fully
/// transparent (a caller-authored `"#RRGGBBAA"`/`"rgba(...)"` alpha, e.g. an
/// agent reproducing a Canvas/Preview ink sketch as a transparent overlay
/// graphic) -- previously silently discarded by the hardcoded opaque-black
/// clear no matter what alpha the caller asked for.
pub fn render_scene_frame(
    compositor: &engine::Compositor,
    text_rasterizer: &mut engine::TextRasterizer,
    spec: &rudis_core::ResolvedSceneSpec,
    t_us: i64,
    transparent: bool,
) -> Result<Vec<u8>, String> {
    let mut layers: Vec<engine::Layer> = Vec::with_capacity(spec.elements.len() + 1);
    for element in spec.elements.iter().rev() {
        let sampled = element.clip.sample_at(t_us, spec.fps.max(1.0));
        let transform = engine::LayerTransform {
            position: sampled.transform.position,
            scale: sampled.transform.scale,
            rotation_deg: sampled.transform.rotation_deg,
        };
        let crop = engine::LayerCrop {
            left: sampled.crop.left,
            top: sampled.crop.top,
            right: sampled.crop.right,
            bottom: sampled.crop.bottom,
        };
        let layer = match &element.kind {
            rudis_core::ResolvedElementKind::Text => rasterize_text_layer(
                text_rasterizer,
                element
                    .clip
                    .text
                    .as_ref()
                    .expect("Text element always carries clip.text"),
                // For an omitted-transform text element `sampled.transform` is
                // TEXT_AUTOFIT_TRANSFORM (scale (0,0), set by 24-01 resolve());
                // rasterize_text_layer reads that sentinel and auto-fits the
                // glyph to its NATURAL size instead of stretching it full-canvas
                // (BLOCKER-1). An explicitly-transformed text element is honored
                // verbatim. Do NOT override this to full-canvas here.
                transform,
                sampled.opacity,
                crop,
                spec.width,
                spec.height,
            ),
            rudis_core::ResolvedElementKind::Rect { fill }
            | rudis_core::ResolvedElementKind::Ellipse { fill } => {
                // Aspect-ratio pitfall fix: a rect/ellipse has NO intrinsic size,
                // so its raster buffer's aspect ratio MUST derive from the
                // element's OWN dest-rect (`sampled.transform.scale` x canvas
                // size). Otherwise `composite_layers_to_rgba`'s contain-fit would
                // letterbox/distort a non-square shape (a wide banner rasterized
                // square would show black bars on two sides). Deriving the dims
                // this way makes contain-fit a mathematically exact no-letterbox
                // fill regardless of the absolute canvas size.
                let raster_w =
                    ((spec.width as f32) * sampled.transform.scale.0).round().max(1.0) as u32;
                let raster_h =
                    ((spec.height as f32) * sampled.transform.scale.1).round().max(1.0) as u32;
                let frame = match &element.kind {
                    rudis_core::ResolvedElementKind::Rect { .. } => {
                        engine::rasterize_rect(raster_w, raster_h, *fill)
                    }
                    _ => engine::rasterize_ellipse(raster_w, raster_h, *fill),
                };
                engine::Layer {
                    frame,
                    opacity: sampled.opacity,
                    transform,
                    crop,
                    // Rasterized shapes are straight-alpha (mirrors text). Straight.
                    alpha_mode: engine::AlphaMode::Straight,
                }
            }
        };
        layers.push(layer);
    }
    // Background ALWAYS last = bottom-most (identity transform, full opacity, its
    // dest rect IS the full canvas by definition).
    let bg_frame = match spec.background {
        rudis_core::ResolvedBackground::Solid { rgba } => {
            engine::rasterize_solid(spec.width, spec.height, rgba)
        }
        rudis_core::ResolvedBackground::Gradient {
            from,
            to,
            angle_deg,
        } => engine::rasterize_linear_gradient(spec.width, spec.height, from, to, angle_deg),
    };
    layers.push(engine::Layer::new(bg_frame, 1.0));
    if transparent {
        compositor
            .composite_layers_to_rgba_transparent(&layers, spec.width, spec.height)
            .map_err(|e| format!("composite scene frame at {t_us}us: {e}"))
    } else {
        compositor
            .composite_layers_to_rgba(&layers, spec.width, spec.height)
            .map_err(|e| format!("composite scene frame at {t_us}us: {e}"))
    }
}
