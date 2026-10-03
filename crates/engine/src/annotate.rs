//! Pixel-primitive annotation compositor (Phase 13, CANV-01 — the IMAGE half
//! of SC-4).
//!
//! Draws canvas annotations (stroke / lasso / arrow / marker) directly onto a
//! decoded [`Frame`]'s RGBA bytes, then PNG-encodes the result to in-memory
//! bytes — the "vision snapshot" the host attaches to a live Chat turn so
//! the agent sees what the user sketched over the video.
//!
//! DELIBERATELY primitives-only: this module takes PIXEL points and RGBA
//! colors, never `rudis_core` types — `crates/engine` stays a pure pixels
//! crate. Denormalizing `Annotation` normalized coords into pixels is the
//! glue layer's (`crates/app-core`'s) job. This is the THIRD consumer of the "one
//! composite path" principle: preview, export, and now the vision snapshot
//! all share this crate's primitives.
//!
//! Label text renders as a position MARKER (a small filled disc), NOT baked
//! glyph text — the structured JSON (agent_state's `AnnotationView.text`)
//! always carries the exact label text, so no font dependency is needed here.
//!
//! Rasterization is a plain hand-rolled DDA line + filled-disc marker + "V"
//! arrowhead — "good enough for vision, not pixel-art" (13-RESEARCH.md,
//! Don't Hand-Roll). Every write goes through the bounds-checked
//! [`set_pixel`], so out-of-frame points are silently dropped, never an
//! out-of-bounds write or panic (threat T-13-05, mitigated).

use crate::{EngineError, Frame};

/// Bounds-checked single-pixel write. Points outside
/// `[0, width) x [0, height)` are silently ignored (T-13-05 mitigation) —
/// EVERY draw function routes through here, never an unchecked index.
fn set_pixel(frame: &mut Frame, x: i32, y: i32, rgba: [u8; 4]) {
    if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 {
        return;
    }
    let idx = (y as usize * frame.width as usize + x as usize) * 4;
    // `Frame.rgba` is tightly packed w*h*4; idx+4 <= len holds whenever the
    // buffer matches its dimensions. Guard anyway so a malformed Frame can
    // never cause a panic here (the PNG encode path reports it properly).
    if idx + 4 > frame.rgba.len() {
        return;
    }
    // Alpha-COMPOSITE the source color over the existing pixel using the src
    // alpha channel (Phase 14.1: the visible-ink overlay fades an annotation by
    // drawing it in progressively lower alpha). Fully-opaque (a == 255) is a
    // byte-identical straight copy — the fast path ALL opaque callers (the
    // vision snapshot, every existing unit test) take, so their behavior is
    // unchanged. Fully-transparent (a == 0) is a no-op. The frame stays opaque
    // (out alpha forced to 255) — it is later composited to the GPU surface.
    let a = rgba[3] as u32;
    if a == 255 {
        frame.rgba[idx..idx + 4].copy_from_slice(&rgba);
    } else if a != 0 {
        let inv = 255 - a;
        for c in 0..3 {
            let src = rgba[c] as u32;
            let dst = frame.rgba[idx + c] as u32;
            frame.rgba[idx + c] = ((src * a + dst * inv) / 255) as u8;
        }
        frame.rgba[idx + 3] = 255;
    }
}

/// Plot a point with a small square neighborhood for visible thickness.
fn set_thick_point(frame: &mut Frame, x: i32, y: i32, rgba: [u8; 4], thickness_px: i32) {
    for dy in -thickness_px..=thickness_px {
        for dx in -thickness_px..=thickness_px {
            set_pixel(frame, x + dx, y + dy, rgba);
        }
    }
}

/// Simple DDA line from `(x0,y0)` to `(x1,y1)`, plotting a
/// `thickness_px`-radius square neighborhood at each step. Unsophisticated
/// but real — sufficient for the vision snapshot's "Claude can see where the
/// user drew" bar.
fn draw_line(
    frame: &mut Frame,
    (x0, y0): (f64, f64),
    (x1, y1): (f64, f64),
    rgba: [u8; 4],
    thickness_px: i32,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).ceil() as i64;
    if steps == 0 {
        set_thick_point(frame, x0.round() as i32, y0.round() as i32, rgba, thickness_px);
        return;
    }
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = x0 + dx * t;
        let y = y0 + dy * t;
        set_thick_point(frame, x.round() as i32, y.round() as i32, rgba, thickness_px);
    }
}

/// DDA line that only PLOTS while inside the "on" portion of a dash cycle,
/// leaving genuine gaps (Phase 14.1, CANV-01 — the existing solid [`draw_line`]
/// has NO dash logic; this is new work). `phase` is the cumulative distance
/// walked so far along the WHOLE path (not just this segment) — the caller
/// carries it across successive segments so a multi-segment stroke/lasso reads
/// as one continuous dashed line, never per-vertex restarts. A pixel is drawn
/// when `(distance mod (dash_on_px + dash_off_px)) < dash_on_px`.
#[allow(clippy::too_many_arguments)]
fn draw_line_dashed(
    frame: &mut Frame,
    (x0, y0): (f64, f64),
    (x1, y1): (f64, f64),
    rgba: [u8; 4],
    thickness_px: i32,
    dash_on_px: f64,
    dash_off_px: f64,
    phase: &mut f64,
) {
    let period = dash_on_px + dash_off_px;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let seg_len = (dx * dx + dy * dy).sqrt();
    let steps = dx.abs().max(dy.abs()).ceil() as i64;
    if steps == 0 {
        // Zero-length segment: plot only if the current phase is in an "on" run.
        if period <= 0.0 || (*phase % period) < dash_on_px {
            set_thick_point(frame, x0.round() as i32, y0.round() as i32, rgba, thickness_px);
        }
        return;
    }
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = x0 + dx * t;
        let y = y0 + dy * t;
        let dist = *phase + seg_len * t;
        if period <= 0.0 || (dist % period) < dash_on_px {
            set_thick_point(frame, x.round() as i32, y.round() as i32, rgba, thickness_px);
        }
    }
    *phase += seg_len;
}

/// Frame-SCALED dash unit (Pitfall 5 — dash sizes must scale with frame
/// resolution, never a flat frame-pixel constant). `dash_on` grows with the
/// frame width (floored so a tiny frame still dashes); `dash_off` is a fixed
/// fraction of it. Returns `(dash_on_px, dash_off_px)`.
fn dash_units(frame: &Frame) -> (f64, f64) {
    // Dash sizes scale with frame resolution AND must stay comfortably larger
    // than the stroke width: a thick line plots a `±STROKE_THICKNESS` square at
    // each step, so it bridges `2*STROKE_THICKNESS` px of any gap from each
    // side. Keeping `dash_off` well above that (floor 10) preserves visible gaps
    // now that the stroke is radius 2 (Phase 14.1 thickness bump merged the old
    // ~4px gaps into a near-solid line).
    let dash_on = (frame.width as f64 / 120.0).max(11.0);
    let dash_off = dash_on * 0.9;
    (dash_on, dash_off)
}

/// Default line thickness radius (px) for all annotation primitives. Radius 2 =
/// a 5px-wide line — bumped from 1 (a thin 3px line read as too faint over live
/// video, Phase 14.1 UAT feedback).
const STROKE_THICKNESS: i32 = 2;
/// Arrowhead barb length in pixels.
const ARROW_HEAD_LEN: f64 = 8.0;
/// Arrowhead half-angle (radians) between the shaft and each barb.
const ARROW_HEAD_SPREAD: f64 = 0.5;
/// Marker (label position) filled-disc radius in pixels.
const MARKER_RADIUS: i32 = 3;

/// Freehand pen path: consecutive `points_px` pairs connected by lines.
/// OPEN path — no closing edge back to the first point (that's `draw_lasso`).
pub fn draw_stroke(frame: &mut Frame, points_px: &[(f64, f64)], rgba: [u8; 4]) {
    for pair in points_px.windows(2) {
        draw_line(frame, pair[0], pair[1], rgba, STROKE_THICKNESS);
    }
    if points_px.len() == 1 {
        let (x, y) = points_px[0];
        set_thick_point(frame, x.round() as i32, y.round() as i32, rgba, STROKE_THICKNESS);
    }
}

/// Closed polygon OUTLINE: consecutive pairs connected, PLUS one closing edge
/// from the LAST point back to the FIRST. Interior is not filled — an outline
/// is sufficient for this phase.
pub fn draw_lasso(frame: &mut Frame, points_px: &[(f64, f64)], rgba: [u8; 4]) {
    draw_stroke(frame, points_px, rgba);
    if points_px.len() >= 2 {
        draw_line(
            frame,
            points_px[points_px.len() - 1],
            points_px[0],
            rgba,
            STROKE_THICKNESS,
        );
    }
}

/// Shaft line from `start_px` to `end_px` plus a standard "V" arrowhead: two
/// short barbs from `end_px` angled back toward `start_px`.
pub fn draw_arrow(frame: &mut Frame, start_px: (f64, f64), end_px: (f64, f64), rgba: [u8; 4]) {
    draw_line(frame, start_px, end_px, rgba, STROKE_THICKNESS);
    // Direction pointing BACK from the tip toward the start.
    let back = (start_px.1 - end_px.1).atan2(start_px.0 - end_px.0);
    for spread in [-ARROW_HEAD_SPREAD, ARROW_HEAD_SPREAD] {
        let bx = end_px.0 + ARROW_HEAD_LEN * (back + spread).cos();
        let by = end_px.1 + ARROW_HEAD_LEN * (back + spread).sin();
        draw_line(frame, end_px, (bx, by), rgba, STROKE_THICKNESS);
    }
}

/// Label position marker: a small filled disc centered at `pos_px`. The label
/// TEXT itself is carried by the structured state (AnnotationView.text) — it
/// is deliberately not rasterized here (no font dependency).
pub fn draw_marker(frame: &mut Frame, pos_px: (f64, f64), rgba: [u8; 4]) {
    let cx = pos_px.0.round() as i32;
    let cy = pos_px.1.round() as i32;
    let r = MARKER_RADIUS;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                set_pixel(frame, cx + dx, cy + dy, rgba);
            }
        }
    }
}

/// Dashed sibling of [`draw_stroke`]: an OPEN freehand path drawn as one
/// CONTINUOUS dashed line (the dash cursor carries across every vertex, so the
/// path is not re-phased per segment). Dash unit derives from the frame width
/// (see [`dash_units`]).
pub fn draw_stroke_dashed(frame: &mut Frame, points_px: &[(f64, f64)], rgba: [u8; 4]) {
    let (on, off) = dash_units(frame);
    let mut phase = 0.0;
    for pair in points_px.windows(2) {
        draw_line_dashed(frame, pair[0], pair[1], rgba, STROKE_THICKNESS, on, off, &mut phase);
    }
    if points_px.len() == 1 {
        let (x, y) = points_px[0];
        set_thick_point(frame, x.round() as i32, y.round() as i32, rgba, STROKE_THICKNESS);
    }
}

/// Dashed sibling of [`draw_lasso`]: a CLOSED polygon outline drawn as one
/// continuous dashed line INCLUDING the closing edge (last -> first). The dash
/// cursor carries through the closing edge too, so the whole outline reads as
/// a single continuous dashed loop.
pub fn draw_lasso_dashed(frame: &mut Frame, points_px: &[(f64, f64)], rgba: [u8; 4]) {
    let (on, off) = dash_units(frame);
    let mut phase = 0.0;
    for pair in points_px.windows(2) {
        draw_line_dashed(frame, pair[0], pair[1], rgba, STROKE_THICKNESS, on, off, &mut phase);
    }
    if points_px.len() >= 2 {
        draw_line_dashed(
            frame,
            points_px[points_px.len() - 1],
            points_px[0],
            rgba,
            STROKE_THICKNESS,
            on,
            off,
            &mut phase,
        );
    } else if points_px.len() == 1 {
        let (x, y) = points_px[0];
        set_thick_point(frame, x.round() as i32, y.round() as i32, rgba, STROKE_THICKNESS);
    }
}

/// Dashed sibling of [`draw_arrow`]: a dashed shaft plus a "V" arrowhead. The
/// shaft carries one dash cursor; each barb restarts its own cursor from the
/// tip so both barbs read as drawn from `end_px` outward.
pub fn draw_arrow_dashed(frame: &mut Frame, start_px: (f64, f64), end_px: (f64, f64), rgba: [u8; 4]) {
    let (on, off) = dash_units(frame);
    let mut phase = 0.0;
    draw_line_dashed(frame, start_px, end_px, rgba, STROKE_THICKNESS, on, off, &mut phase);
    // Direction pointing BACK from the tip toward the start (mirrors draw_arrow).
    let back = (start_px.1 - end_px.1).atan2(start_px.0 - end_px.0);
    for spread in [-ARROW_HEAD_SPREAD, ARROW_HEAD_SPREAD] {
        let bx = end_px.0 + ARROW_HEAD_LEN * (back + spread).cos();
        let by = end_px.1 + ARROW_HEAD_LEN * (back + spread).sin();
        let mut barb_phase = 0.0;
        draw_line_dashed(
            frame,
            end_px,
            (bx, by),
            rgba,
            STROKE_THICKNESS,
            on,
            off,
            &mut barb_phase,
        );
    }
}

/// PNG-encode a [`Frame`] to in-memory bytes — the twin of
/// [`crate::ffmpeg::write_frame_png`], writing to a `Cursor<Vec<u8>>` instead
/// of disk. Same `BadOutputSize` mapping when the byte buffer doesn't match
/// the frame's dimensions.
pub fn encode_png_bytes(frame: &Frame) -> Result<Vec<u8>, EngineError> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone()).ok_or(
        EngineError::BadOutputSize {
            got: frame.rgba.len(),
            expected: frame.width as usize * frame.height as usize * 4,
        },
    )?;
    let mut buf: Vec<u8> = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| EngineError::PngEncode(e.to_string()))?;
    Ok(buf)
}

/// JPEG-encode a [`Frame`] to in-memory bytes, size-clamped to `max_long_edge`
/// pixels on its longest edge (aspect-preserved) and alpha-stripped (JPEG has
/// no alpha channel — `image::codecs::jpeg::JpegEncoder` only accepts
/// `L8`/`Rgb8`, never `Rgba8`, verified against the vendored 0.25.10 source).
/// The twin of [`encode_png_bytes`], for the image-bearing `ToolResult` path
/// (Phase 16, TOOL-06/D-04) — never ships raw/unencoded pixel buffers.
pub fn encode_jpeg_bytes(frame: &Frame, max_long_edge: u32, quality: u8) -> Result<Vec<u8>, EngineError> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone()).ok_or(
        EngineError::BadOutputSize {
            got: frame.rgba.len(),
            expected: frame.width as usize * frame.height as usize * 4,
        },
    )?;
    let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();

    let long_edge = rgb.width().max(rgb.height());
    let clamped = if long_edge > max_long_edge {
        let scale = max_long_edge as f64 / long_edge as f64;
        let new_w = ((rgb.width() as f64) * scale).round().max(1.0) as u32;
        let new_h = ((rgb.height() as f64) * scale).round().max(1.0) as u32;
        image::imageops::resize(&rgb, new_w, new_h, image::imageops::FilterType::Triangle)
    } else {
        rgb
    };

    let mut buf: Vec<u8> = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    encoder
        .encode(
            clamped.as_raw(),
            clamped.width(),
            clamped.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| EngineError::JpegEncode(e.to_string()))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    /// 64x64 all-black opaque frame fixture (no real media needed).
    fn black_frame() -> Frame {
        let mut rgba = Vec::with_capacity(64 * 64 * 4);
        for _ in 0..64 * 64 {
            rgba.extend_from_slice(&BLACK);
        }
        Frame {
            width: 64,
            height: 64,
            rgba,
        }
    }

    fn px(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let idx = (y as usize * frame.width as usize + x as usize) * 4;
        [
            frame.rgba[idx],
            frame.rgba[idx + 1],
            frame.rgba[idx + 2],
            frame.rgba[idx + 3],
        ]
    }

    #[test]
    fn annotate_stroke_draws_on_path_and_leaves_far_pixels_unchanged() {
        let mut frame = black_frame();
        draw_stroke(&mut frame, &[(10.0, 32.0), (54.0, 32.0)], RED);
        // On the line: drawn.
        assert_eq!(px(&frame, 32, 32), RED, "pixel on the stroke path must be colored");
        // Far from the line: untouched.
        assert_eq!(px(&frame, 32, 5), BLACK, "pixel far from the stroke must be unchanged");
    }

    #[test]
    fn annotate_stroke_is_open_no_closing_edge() {
        let mut frame = black_frame();
        // Triangle-shaped OPEN path: (10,10) -> (54,10) -> (32,54).
        // The would-be closing edge (32,54)->(10,10) passes exactly through
        // (21,32); a stroke must NOT draw it.
        draw_stroke(&mut frame, &[(10.0, 10.0), (54.0, 10.0), (32.0, 54.0)], RED);
        assert_eq!(
            px(&frame, 21, 32),
            BLACK,
            "stroke is an open path — the last->first edge must not be drawn"
        );
    }

    #[test]
    fn annotate_lasso_closes_polygon_back_to_first_point() {
        let mut frame = black_frame();
        // Same triangle, as a LASSO: the closing edge (32,54)->(10,10) passes
        // exactly through (21,32) (midpoint of that edge) and must be drawn.
        draw_lasso(&mut frame, &[(10.0, 10.0), (54.0, 10.0), (32.0, 54.0)], RED);
        assert_eq!(
            px(&frame, 21, 32),
            RED,
            "lasso must connect the LAST point back to the FIRST (closed polygon)"
        );
        // Interior stays unfilled (outline only) — centroid-ish point.
        assert_eq!(px(&frame, 32, 24), BLACK, "lasso interior must not be filled");
    }

    #[test]
    fn annotate_arrow_draws_shaft_and_distinct_head_near_end() {
        let mut frame = black_frame();
        // Horizontal shaft (10,32)->(50,32). With thickness radius 2 the plain
        // line alone only colors rows y in [30,34]; the "V" arrowhead barbs
        // angle away from that band near the tip (the 29..=35 exclusion below
        // keeps a margin around it).
        draw_arrow(&mut frame, (10.0, 32.0), (50.0, 32.0), RED);
        // Shaft drawn.
        assert_eq!(px(&frame, 30, 32), RED, "arrow shaft must be drawn");
        // Head: some non-background pixel near the tip OUTSIDE the shaft's
        // thickness band (y <= 29 or y >= 35, within 10px of the end).
        let mut head_pixel_found = false;
        for y in 0..64u32 {
            if (29..=35).contains(&y) {
                continue; // exclude the shaft band + margin
            }
            for x in 40..=52u32 {
                if px(&frame, x, y) == RED {
                    head_pixel_found = true;
                }
            }
        }
        assert!(
            head_pixel_found,
            "arrowhead must produce pixels near end_px beyond the plain shaft line"
        );
    }

    #[test]
    fn annotate_marker_fills_cluster_and_leaves_distant_pixel_unchanged() {
        let mut frame = black_frame();
        draw_marker(&mut frame, (32.0, 32.0), RED);
        // Center + a >=2px-radius cluster around it.
        assert_eq!(px(&frame, 32, 32), RED, "marker center must be colored");
        assert_eq!(px(&frame, 34, 32), RED, "marker must fill a radius >= 2px cluster");
        assert_eq!(px(&frame, 32, 30), RED, "marker must fill vertically too");
        // 20px away: untouched.
        assert_eq!(px(&frame, 52, 32), BLACK, "pixel 20px from the marker must be unchanged");
    }

    #[test]
    fn set_pixel_opaque_overwrites_translucent_blends_transparent_noops() {
        let mut frame = black_frame();
        // Opaque: exact overwrite (the fast path every existing caller/test uses).
        set_pixel(&mut frame, 1, 1, RED);
        assert_eq!(px(&frame, 1, 1), RED, "alpha 255 must overwrite exactly");
        // Half alpha over black: ~half-intensity red, frame stays opaque.
        set_pixel(&mut frame, 2, 2, [255, 0, 0, 128]);
        let blended = px(&frame, 2, 2);
        assert!(
            (120..=136).contains(&blended[0]) && blended[1] == 0 && blended[2] == 0,
            "alpha 128 red over black must blend to ~half red, got {blended:?}"
        );
        assert_eq!(blended[3], 255, "blended pixel keeps the frame opaque");
        // Fully transparent: no change.
        set_pixel(&mut frame, 3, 3, [255, 0, 0, 0]);
        assert_eq!(px(&frame, 3, 3), BLACK, "alpha 0 must leave the pixel untouched");
    }

    #[test]
    fn annotate_out_of_bounds_points_are_dropped_not_panicking() {
        let mut frame = black_frame();
        // Every draw fn with wildly out-of-range points: must not panic, and
        // in-bounds portions of crossing lines are still clipped safely.
        draw_stroke(&mut frame, &[(-100.0, -100.0), (200.0, 200.0)], RED);
        draw_lasso(
            &mut frame,
            &[(-50.0, 10.0), (500.0, 10.0), (32.0, 900.0)],
            RED,
        );
        draw_arrow(&mut frame, (-20.0, -20.0), (100.0, 100.0), RED);
        draw_marker(&mut frame, (-5.0, -5.0), RED);
        draw_marker(&mut frame, (1000.0, 1000.0), RED);
        // The diagonal stroke crosses in-bounds territory and is drawn there.
        assert_eq!(px(&frame, 32, 32), RED, "in-bounds segment of a clipped line still draws");
        // Corner pixel near the fully-out-of-bounds marker stays black.
        assert_eq!(px(&frame, 63, 63), RED); // diagonal passes here too — check a truly far one:
        assert_eq!(px(&frame, 63, 0), BLACK, "untouched corner stays background");
    }

    #[test]
    fn annotate_encode_png_bytes_round_trips_exact_pixels() {
        let mut frame = black_frame();
        draw_marker(&mut frame, (32.0, 32.0), RED);
        let bytes = encode_png_bytes(&frame).expect("png encode succeeds");
        // PNG magic number.
        assert_eq!(&bytes[..4], &[0x89, 0x50, 0x4E, 0x47], "output must start with PNG magic");
        // Round-trip: decode recovers EXACT dimensions + RGBA bytes.
        let decoded = image::load_from_memory(&bytes).expect("bytes decode as an image");
        let rgba = decoded.to_rgba8();
        assert_eq!(rgba.width(), frame.width);
        assert_eq!(rgba.height(), frame.height);
        assert_eq!(rgba.as_raw(), &frame.rgba, "decoded RGBA must be byte-identical");
    }

    // ------------------------------------------------------------------
    // Phase 14.1 (CANV-01): dashed line primitives leave REAL gaps.
    // ------------------------------------------------------------------

    #[test]
    fn dash_line_alternates_drawn_and_undrawn_runs() {
        // A horizontal dashed line across the whole 64px width must produce
        // BOTH colored pixels AND undrawn (black) gaps along its path — i.e.
        // it is genuinely broken, not a solid line.
        let mut frame = black_frame();
        draw_stroke_dashed(&mut frame, &[(0.0, 32.0), (63.0, 32.0)], RED);
        let mut saw_red = false;
        let mut saw_black = false;
        for x in 0..64u32 {
            match px(&frame, x, 32) {
                RED => saw_red = true,
                BLACK => saw_black = true,
                _ => {}
            }
        }
        assert!(saw_red, "a dashed line must draw some colored pixels");
        assert!(
            saw_black,
            "a dashed line must leave some undrawn (black) gaps along the path"
        );
    }

    #[test]
    fn dash_stroke_leaves_gaps_a_solid_stroke_would_not() {
        // Draw the SAME path solid vs dashed; there must exist a path pixel
        // that is colored in the solid render but BLACK in the dashed one.
        let path = [(0.0, 20.0), (63.0, 20.0)];
        let mut solid = black_frame();
        draw_stroke(&mut solid, &path, RED);
        let mut dashed = black_frame();
        draw_stroke_dashed(&mut dashed, &path, RED);
        let mut found_gap = false;
        for x in 0..64u32 {
            if px(&solid, x, 20) == RED && px(&dashed, x, 20) == BLACK {
                found_gap = true;
                break;
            }
        }
        assert!(
            found_gap,
            "a dashed stroke must leave a gap where the solid stroke drew a pixel"
        );
    }

    #[test]
    fn dash_lasso_closes_and_is_dashed() {
        // Triangle lasso: the closing edge (32,54)->(10,10) must be DRAWN
        // (the lasso closes) and the long top edge (10,10)->(54,10) must show
        // both colored pixels and gaps (it is dashed).
        let mut frame = black_frame();
        draw_lasso_dashed(&mut frame, &[(10.0, 10.0), (54.0, 10.0), (32.0, 54.0)], RED);
        // Closing edge is drawn: some sampled point along it is colored.
        let mut edge_red = false;
        for i in 0..=100 {
            let t = i as f64 / 100.0;
            let x = (32.0 - 22.0 * t).round() as u32;
            let y = (54.0 - 44.0 * t).round() as u32;
            if px(&frame, x, y) == RED {
                edge_red = true;
            }
        }
        assert!(edge_red, "dashed lasso must draw the closing edge (last->first)");
        // Top edge is dashed: both colored pixels and gaps present.
        let mut saw_red = false;
        let mut saw_black = false;
        for x in 10..=54u32 {
            match px(&frame, x, 10) {
                RED => saw_red = true,
                BLACK => saw_black = true,
                _ => {}
            }
        }
        assert!(saw_red, "dashed lasso edge must draw colored pixels");
        assert!(saw_black, "dashed lasso edge must show gaps");
    }

    #[test]
    fn dash_out_of_bounds_points_do_not_panic() {
        // Wildly out-of-range points must never panic (reuses the set_pixel
        // bounds guard) and draw nothing out of bounds.
        let mut frame = black_frame();
        draw_stroke_dashed(&mut frame, &[(-100.0, -100.0), (200.0, 200.0)], RED);
        draw_lasso_dashed(&mut frame, &[(-50.0, 10.0), (500.0, 10.0), (32.0, 900.0)], RED);
        draw_arrow_dashed(&mut frame, (-20.0, -20.0), (100.0, 100.0), RED);
        // No panic is the primary assertion; a plain in-bounds read confirms
        // the frame buffer is still intact.
        let _ = px(&frame, 0, 0);
    }

    #[test]
    fn encode_jpeg_bytes_round_trips_a_small_frame_unresized() {
        let frame = black_frame(); // 64x64
        let bytes = encode_jpeg_bytes(&frame, 512, 70).expect("jpeg encode succeeds");
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[0..2], &[0xFF, 0xD8], "must start with the JPEG magic bytes");
        let decoded = image::load_from_memory(&bytes).expect("valid JPEG bytes");
        assert_eq!(decoded.width(), 64);
        assert_eq!(decoded.height(), 64);
    }

    #[test]
    fn encode_jpeg_bytes_clamps_a_large_frame_to_the_long_edge() {
        let mut rgba = Vec::with_capacity(1280 * 720 * 4);
        for _ in 0..(1280 * 720) {
            rgba.extend_from_slice(&[10, 20, 30, 255]);
        }
        let frame = Frame { width: 1280, height: 720, rgba };
        let bytes = encode_jpeg_bytes(&frame, 512, 70).expect("jpeg encode succeeds");
        let decoded = image::load_from_memory(&bytes).expect("valid JPEG bytes");
        assert!(
            decoded.width().max(decoded.height()) <= 512,
            "long edge must be clamped to <=512, got {}x{}",
            decoded.width(),
            decoded.height()
        );
        // Aspect ratio preserved within integer-rounding tolerance (1280:720 == 16:9).
        let ratio = decoded.width() as f64 / decoded.height() as f64;
        assert!((ratio - 16.0 / 9.0).abs() < 0.02, "aspect ratio must be preserved, got {ratio}");
    }

    #[test]
    fn encode_jpeg_bytes_rejects_mismatched_buffer() {
        let frame = Frame { width: 4, height: 4, rgba: vec![0u8; 10] }; // wrong length
        match encode_jpeg_bytes(&frame, 512, 70) {
            Err(EngineError::BadOutputSize { got: 10, expected: 64 }) => {}
            other => panic!("expected BadOutputSize, got {other:?}"),
        }
    }

    #[test]
    fn annotate_encode_png_bytes_rejects_mismatched_buffer() {
        let frame = Frame {
            width: 64,
            height: 64,
            rgba: vec![0u8; 16], // wrong size
        };
        match encode_png_bytes(&frame) {
            Err(EngineError::BadOutputSize { got, expected }) => {
                assert_eq!(got, 16);
                assert_eq!(expected, 64 * 64 * 4);
            }
            other => panic!("expected BadOutputSize, got {other:?}"),
        }
    }
}
