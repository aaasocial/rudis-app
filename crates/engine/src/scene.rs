//! Scene-spec rasterization primitives (Phase 24, ASSET-01/02).
//!
//! The ONE genuinely-new rendering capability of Phase 24: hand-rolled,
//! dependency-free RGBA fills — a solid color, a linear gradient, a filled
//! rectangle, and a filled ellipse — each producing a [`Frame`] the existing
//! Phase-18 compositor consumes as an ordinary [`crate::Layer`] (zero shader
//! changes, no new compositor entry point).
//!
//! DELIBERATELY primitives-only, mirroring [`crate::annotate`]'s discipline:
//! this module takes PIXEL dimensions and RGBA colors, NEVER `rudis_core`
//! types — `crates/engine` stays a pure pixels crate. Denormalizing a scene
//! element's normalized transform into pixel dimensions is the glue layer's
//! (`crates/app-core`'s) job (see Plan 24-04's `render_scene_frame`).
//!
//! ## STRAIGHT-alpha, color-bled buffer (mirrors [`crate::text`])
//!
//! `compositor.rs`'s `fs_layer` premultiplies IN-SHADER
//! (`return vec4(texel.rgb * ea, ea)` with blend `(One, OneMinusSrcAlpha)`), so
//! it expects a STRAIGHT-alpha input texture. [`rasterize_ellipse`] therefore
//! writes the fill RGB into EVERY pixel (including the fully-transparent ones
//! OUTSIDE the ellipse) and varies only the alpha channel — the same convention
//! `text.rs` documents for glyph coverage, so bilinear filtering (Phase-19
//! keyframe zoom) never pulls RGB toward black at the ellipse edge.
//!
//! ## DoS ceiling (T-24-05, mirrors [`crate::text`])
//!
//! A shape element's raster size is DERIVED from its `transform.scale` × canvas
//! dims, which core does not itself bound to a pixel count. [`clamp_dims`]
//! proportionally clamps `(w, h)` to `text.rs`'s proven
//! [`MAX_RASTER_DIM`]/[`MAX_RASTER_PIXELS`] BEFORE any `Vec` allocation —
//! defense-in-depth alongside Plan 24-01's core-level `MAX_SCENE_DIM` cap.

use crate::ffmpeg::Frame;
use crate::text::{MAX_RASTER_DIM, MAX_RASTER_PIXELS};

/// Proportionally clamp `(w, h)` to the same DoS ceiling `text.rs` enforces —
/// defense-in-depth even though callers should already cap upstream (T-24-05).
/// Each dim is floored at 1 and capped at [`MAX_RASTER_DIM`]; if the AREA still
/// exceeds [`MAX_RASTER_PIXELS`] both dims scale down by `sqrt(cap / area)`
/// (aspect-preserving), mirroring `text.rs`'s own clamp-before-allocate.
fn clamp_dims(w: u32, h: u32) -> (u32, u32) {
    let (mut w, mut h) = (w.max(1).min(MAX_RASTER_DIM), h.max(1).min(MAX_RASTER_DIM));
    if (w as u64) * (h as u64) > MAX_RASTER_PIXELS {
        let scale = (MAX_RASTER_PIXELS as f64 / (w as u64 * h as u64) as f64).sqrt();
        w = ((w as f64) * scale).max(1.0) as u32;
        h = ((h as f64) * scale).max(1.0) as u32;
    }
    (w, h)
}

/// A solid fill: every pixel of the `width`×`height` [`Frame`] is `rgba`.
/// Dimensions are clamped to the DoS ceiling (T-24-05) before allocation.
pub fn rasterize_solid(width: u32, height: u32, rgba: [u8; 4]) -> Frame {
    let (w, h) = clamp_dims(width, height);
    let mut buf = Vec::with_capacity(w as usize * h as usize * 4);
    for _ in 0..(w as usize * h as usize) {
        buf.extend_from_slice(&rgba);
    }
    Frame {
        width: w,
        height: h,
        rgba: buf,
    }
}

/// A rect element IS a solid fill spanning its ENTIRE dest rect. The caller
/// derives this Frame's dims from the element's own `transform.scale` so the
/// compositor's contain-fit fills the slot edge-to-edge with no letterbox
/// (see Plan 24-04). Pixel-identical to [`rasterize_solid`] by construction.
pub fn rasterize_rect(width: u32, height: u32, fill: [u8; 4]) -> Frame {
    rasterize_solid(width, height, fill)
}

/// A linear gradient from `from` (RGBA) to `to` (RGBA) across the buffer along
/// the direction `angle_deg` (0° = left-to-right, growing clockwise in y-down
/// pixel space). The gradient axis is normalized against the buffer's PROJECTED
/// extent so the leftmost/topmost extreme reads exactly `from` and the
/// opposite extreme exactly `to` (within ±1 rounding). Each channel — RGB AND
/// alpha — is linearly interpolated. Dimensions are clamped (T-24-05).
pub fn rasterize_linear_gradient(
    width: u32,
    height: u32,
    from: [u8; 4],
    to: [u8; 4],
    angle_deg: f64,
) -> Frame {
    let (w, h) = clamp_dims(width, height);
    let theta = angle_deg.to_radians();
    let (dx, dy) = (theta.cos(), theta.sin());
    // Project all four corners onto the gradient axis to find its true extent,
    // so t=0 lands on the first-reached corner and t=1 on the last — for any
    // angle, not just axis-aligned ones.
    let corners = [
        (0.0, 0.0),
        (w as f64, 0.0),
        (0.0, h as f64),
        (w as f64, h as f64),
    ];
    let projs: Vec<f64> = corners.iter().map(|(x, y)| x * dx + y * dy).collect();
    let (min_p, max_p) = (
        projs.iter().cloned().fold(f64::INFINITY, f64::min),
        projs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    let span = (max_p - min_p).max(1e-6);
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..w {
            let proj = (x as f64 + 0.5) * dx + (y as f64 + 0.5) * dy;
            let t = ((proj - min_p) / span).clamp(0.0, 1.0);
            let idx = (y as usize * w as usize + x as usize) * 4;
            for c in 0..4 {
                let a = from[c] as f64;
                let b = to[c] as f64;
                buf[idx + c] = (a + (b - a) * t).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Frame {
        width: w,
        height: h,
        rgba: buf,
    }
}

/// A filled ellipse inscribed in the `width`×`height` buffer, STRAIGHT-alpha +
/// color-bled: `rgb = fill` in EVERY pixel, `alpha = fill[3]` INSIDE the
/// ellipse and `0` OUTSIDE — the SAME convention `text.rs` documents for glyph
/// coverage, so bilinear filtering never pulls RGB toward black at the ellipse
/// edge. Dimensions are clamped to the DoS ceiling (T-24-05) before allocation.
pub fn rasterize_ellipse(width: u32, height: u32, fill: [u8; 4]) -> Frame {
    let (w, h) = clamp_dims(width, height);
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    let (rx, ry) = (cx.max(1e-6), cy.max(1e-6));
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..w {
            let nx = (x as f64 + 0.5 - cx) / rx;
            let ny = (y as f64 + 0.5 - cy) / ry;
            let inside = nx * nx + ny * ny <= 1.0;
            let idx = (y as usize * w as usize + x as usize) * 4;
            buf[idx] = fill[0];
            buf[idx + 1] = fill[1];
            buf[idx + 2] = fill[2];
            buf[idx + 3] = if inside { fill[3] } else { 0 };
        }
    }
    Frame {
        width: w,
        height: h,
        rgba: buf,
    }
}
