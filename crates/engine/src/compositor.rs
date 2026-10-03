//! wgpu offscreen compositor.
//!
//! DECISION (Phase 1): preview compositing is verified via a wgpu OFFSCREEN
//! (no-surface) render + pixel readback, so the decode -> GPU -> pixels path
//! is provable headlessly with `cargo test` on macOS (Metal backend).
//!
//! Pipeline per frame:
//!   upload RGBA bytes as a texture
//!     -> render a fullscreen textured quad into an offscreen RGBA target
//!     -> copy the target into a mappable buffer (256-byte row alignment)
//!     -> map + read back tightly-packed RGBA bytes.
//!
//! The on-screen path (creating a `wgpu::Surface` from a native window handle
//! — a child HWND/HWND-window on Windows, an `NSView`/`CAMetalLayer` on macOS
//! — and presenting frames to it) shares this exact pipeline up to the render
//! target. As of Phase 9 Wave 2 it is IMPLEMENTED here: `composite_to_surface`
//! presents a composited `Frame` directly into a `wgpu::Surface`'s swapchain
//! texture with NO CPU readback, reusing the SAME WGSL shader / vertex /
//! bind-group logic as the offscreen `composite_to_rgba` via the shared
//! `encode_frame_pass` helper ("one composite path"). Only the render-target
//! FORMAT differs (surface-negotiated, e.g. `Bgra8UnormSrgb` — Pitfall E — vs
//! the offscreen path's `Rgba8Unorm`), so a second pipeline instance
//! (`surface_pipeline`) is built against the surface format from the SAME
//! shader.
//!
//! SEAM (Phase 4 -> Phase 9): the production Preview attaches HERE — a
//! `wgpu::Surface` created from the video window's native handle replaces the
//! offscreen target. Wave 1's throwaway spike proved the native surface +
//! dual-window compositing shape (see DECISIONS.md ▶ WINDOWS VERDICT: DECIDED
//! on Windows/Vulkan and macOS/Metal). The Phase 4 dev stand-in (decode -> PNG
//! in the app cache -> asset protocol <img>) is superseded by this surface
//! path for live preview.
//!
//! STATUS (Phase 9 Wave 2): `Compositor::new_with_surface()` builds a
//! surface-compatible device + the surface-format pipeline;
//! `composite_to_surface()` presents one composited frame to the swapchain.
//! Compiles and runs on Windows (Vulkan) and macOS (Metal). What remains
//! PENDING beyond this wave: the persistent streaming decode session driving a
//! sustained present loop (Wave 3), live preview-audio + A/V sync (Wave 4),
//! and the automated on-screen-vs-offscreen frame-diff parity gate (Wave 5).
//!
//! SEAM (Phase 4, audio): LIVE audio output + audible A/V sync attaches
//! beside the presenter (WASAPI on Windows via cpal or Media Foundation;
//! CoreAudio/AVAudioEngine on macOS), clocked from the same transport
//! `position_us`. No audio device is opened in the dev path.
//! Live audio + A/V sync is VERIFIED on Windows (Phase 9 Wave 4, cpal/WASAPI —
//! drift 8.7ms ≤ user-confirmed ±20ms, audible sync user-validated). macOS
//! runtime re-validation remains a documented revisit trigger (retired dev
//! machine), not a Windows gap.

use crate::ffmpeg::Frame;
use crate::EngineError;

/// Fullscreen textured quad (triangle strip, 4 vertices generated from the
/// vertex index — no vertex buffer needed) + texture sample fragment shader.
const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Triangle-strip fullscreen quad:
    //   vi=0 -> (-1,-1) uv(0,1)   vi=1 -> (1,-1) uv(1,1)
    //   vi=2 -> (-1, 1) uv(0,0)   vi=3 -> (1, 1) uv(1,0)
    let x = f32(i32(vi & 1u)) * 2.0 - 1.0;
    let y = f32(i32(vi >> 1u)) * 2.0 - 1.0;
    var out: VsOut;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(frame_tex, frame_samp, in.uv);
}

// --- PRESENT-ONLY sharp magnification (quick-260803-g3d) --------------------
// A Catmull-Rom bicubic reconstruction of `frame_tex`, used by exactly ONE
// caller: `blit_texture_to_surface`'s magnification branch, when a smaller-than-
// canvas composite is being stretched up to fill the surface. `fs_main` above is
// byte-untouched and remains the entry point of every other pipeline in this
// file — offscreen, blend, and the 1:1-or-minified present.
//
// Why bicubic here: bilinear reconstructs a magnified image from 2x2 texels, so
// at 2x and beyond it delivers visible mush. Catmull-Rom is an interpolating
// cubic (it passes THROUGH the source texels rather than smoothing them away)
// with a mild sharpening lobe, which is what makes a degraded composite read as
// "soft but legible" instead of "broken".
//
// The 9-tap optimization: the 4x4 texel neighbourhood a bicubic needs would be
// 16 point fetches. Instead each axis' MIDDLE PAIR of weights (w1, w2) is
// collapsed into ONE bilinear fetch positioned at w2/(w1+w2) between the two
// texels, so the sampler's own linear blend reproduces the pair exactly. That
// makes it 3 fetches per axis, 9 total — and it is the reason this pipeline
// REUSES the shared linear `frame_sampler` rather than introducing a sampler of
// its own: the optimization is only exact with linear filtering, and the shared
// sampler is parity-observable and must not be mutated.
@fragment
fn fs_present_sharp(in: VsOut) -> @location(0) vec4<f32> {
    let tex_size = vec2<f32>(textureDimensions(frame_tex));
    let inv_size = vec2<f32>(1.0, 1.0) / tex_size;

    // Where we are in texel space, and the offset from the centre of the texel
    // that contains us (`tex_pos1` is that centre).
    let sample_pos = in.uv * tex_size;
    let tex_pos1 = floor(sample_pos - vec2<f32>(0.5, 0.5)) + vec2<f32>(0.5, 0.5);
    let f = sample_pos - tex_pos1;
    let f2 = f * f;
    let f3 = f2 * f;

    // Catmull-Rom (the a = -0.5 cubic) weights for the taps at -1, 0, +1, +2.
    // They sum to exactly 1 per axis, so a flat region is reproduced flat.
    let w0 = -0.5 * f3 + f2 - 0.5 * f;
    let w1 = 1.5 * f3 - 2.5 * f2 + vec2<f32>(1.0, 1.0);
    let w2 = -1.5 * f3 + 2.0 * f2 + 0.5 * f;
    let w3 = 0.5 * f3 - 0.5 * f2;

    let w12 = w1 + w2;
    let offset12 = w2 / w12;

    let p0 = (tex_pos1 - vec2<f32>(1.0, 1.0)) * inv_size;
    let p3 = (tex_pos1 + vec2<f32>(2.0, 2.0)) * inv_size;
    let p12 = (tex_pos1 + offset12) * inv_size;

    // `textureSampleLevel` (not `textureSample`): explicit LOD 0, so the fetch
    // needs no implicit derivatives and no uniform control flow.
    var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p0.x, p0.y), 0.0) * (w0.x * w0.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p12.x, p0.y), 0.0) * (w12.x * w0.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p3.x, p0.y), 0.0) * (w3.x * w0.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p0.x, p12.y), 0.0) * (w0.x * w12.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p12.x, p12.y), 0.0) * (w12.x * w12.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p3.x, p12.y), 0.0) * (w3.x * w12.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p0.x, p3.y), 0.0) * (w0.x * w3.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p12.x, p3.y), 0.0) * (w12.x * w3.y);
    acc = acc + textureSampleLevel(frame_tex, frame_samp, vec2<f32>(p3.x, p3.y), 0.0) * (w3.x * w3.y);

    // Catmull-Rom's negative lobes overshoot on hard edges (the ringing that
    // buys the sharpness). Clamp per channel: the composite target is opaque, so
    // alpha clamps to the 1.0 it already carries.
    return clamp(acc, vec4<f32>(0.0, 0.0, 0.0, 0.0), vec4<f32>(1.0, 1.0, 1.0, 1.0));
}

// --- Phase 18 multi-layer blend path (composite_layers_to_*) ----------------
// Per-layer params — all values resolved HOST-SIDE in `layer_params_bytes`
// (the ONE geometry spot, so preview and export can never drift):
//   rect:       xy = dest-rect centre (target px), zw = contain-fitted quad
//               HALF extents (target px)
//   rot_target: xy = (cos, sin) of the rotation, zw = target dims (px)
//   crop:       source insets (left, top, right, bottom), each 0-1 of source
//   misc:       x = layer opacity (already clamped to finite [0,1] — T-18-04),
//               yzw = padding
struct LayerParams {
    rect: vec4<f32>,
    rot_target: vec4<f32>,
    crop: vec4<f32>,
    misc: vec4<f32>,
}
@group(1) @binding(0) var<uniform> layer_params: LayerParams;

// Per-layer quad: position/scale define the destination rect, the source is
// contain-fitted inside it (both resolved host-side into `rect`), rotation
// pivots about the rect centre. Rotation happens in aspect-true target-PIXEL
// space (y grows DOWN) — rotating in normalized/NDC space would shear the
// quad on non-square targets.
@vertex
fn vs_layer(@builtin(vertex_index) vi: u32) -> VsOut {
    // Same triangle-strip corner derivation as vs_main, kept as -1/+1 signs.
    let sx = f32(i32(vi & 1u)) * 2.0 - 1.0;
    let sy = f32(i32(vi >> 1u)) * 2.0 - 1.0;
    // Local corner offset (px, y-down), rotated about the quad centre.
    let local = vec2<f32>(sx * layer_params.rect.z, sy * layer_params.rect.w);
    let c = layer_params.rot_target.x;
    let s = layer_params.rot_target.y;
    let rotated = vec2<f32>(local.x * c - local.y * s, local.x * s + local.y * c);
    let px = layer_params.rect.xy + rotated;
    // Target-pixel space -> NDC (y flips).
    let ndc = vec2<f32>(
        px.x / layer_params.rot_target.z * 2.0 - 1.0,
        1.0 - px.y / layer_params.rot_target.w * 2.0,
    );
    var out: VsOut;
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    // sy=-1 is the quad's TOP row in pixel space -> uv.y = 0 (image top).
    // For an identity full-target layer this yields the exact same NDC<->uv
    // mapping as vs_main (bit-identical pass-through — Pitfall 2).
    out.uv = vec2<f32>((sx + 1.0) * 0.5, (sy + 1.0) * 0.5);
    return out;
}

// Premultiply IN-SHADER (locked convention D-01, 18-RESEARCH.md §Q1): the
// (One, OneMinusSrcAlpha) blend state assumes a PREMULTIPLIED source — feeding
// it straight alpha would ignore the alpha entirely (Pitfall 1). For an opaque
// texel at opacity 1.0 this reduces to `return texel` — identical to fs_main.
@fragment
fn fs_layer(in: VsOut) -> @location(0) vec4<f32> {
    // Crop insets remap the sampled UV window to [left, 1-right] x
    // [top, 1-bottom] — the remaining source region fills the fitted quad.
    // Identity crop (all zeros) leaves the UV bit-unchanged (uv * 1.0 + 0.0).
    let cp = layer_params.crop;
    let uv = vec2<f32>(
        cp.x + in.uv.x * (1.0 - cp.x - cp.z),
        cp.y + in.uv.y * (1.0 - cp.y - cp.w),
    );
    let texel = textureSample(frame_tex, frame_samp, uv);
    let opacity = layer_params.misc.x;
    // Phase 28 (OVL-01): misc.y is the per-layer alpha-interpretation flag —
    // 0.0 = Straight (default, the LOCKED Phase-18 convention: premultiply
    // in-shader), 1.0 = Premultiplied (the source already carries rgb *= its
    // own alpha, so skip the texel.a multiply to avoid double-premultiply
    // washout — Pitfall 5). This is the ONLY premultiply authority; no second
    // mechanism is added (one composite path).
    let is_premultiplied = layer_params.misc.y;
    let a = texel.a * opacity;               // effective alpha (unchanged)
    // Straight: scale rgb by texel.a AND opacity (existing byte-identical
    // behavior). Premultiplied: rgb already scaled by its own alpha — scale by
    // opacity ONLY. For any opaque texel (texel.a == 1.0) BOTH branches reduce
    // to `opacity`, so defaulting every layer to Straight is bit-identical to
    // the pre-Phase-28 output (backward compat).
    let rgb_scale = select(a, opacity, is_premultiplied > 0.5);
    return vec4<f32>(texel.rgb * rgb_scale, a);  // PREMULTIPLIED output
}
"#;

/// Phase 57 (PLAY-02) — the NV12-SOURCED LAYER fragment shader.
///
/// This string is never compiled alone: [`Compositor::build_nv12_layer_pipeline`]
/// compiles `SHADER` **concatenated with** this body, so `vs_layer` (the ONE
/// per-layer vertex/geometry stage), the `LayerParams` uniform declaration, and
/// `frame_samp` are reused BY CONSTRUCTION rather than re-typed here — the two
/// halves can never drift (threat T-57-07). `SHADER` itself is byte-untouched,
/// so every pre-existing pipeline still compiles the exact same source it
/// always did (D-06: additive, never a rewrite).
///
/// The fragment body is the two ALREADY-PROVEN halves, combined:
/// - the crop-inset UV remap is copied VERBATIM from `fs_layer` (see the
///   `cp`/`uv` lines there);
/// - the YUV→RGB conversion is copied VERBATIM from `NV12_SHADER`'s
///   `yuv_to_rgb` + its `clamp` (the SPIKE-02 math that frame-diffed at MAD
///   0.0000 against the CLI sidecar). No matrix literal appears here either —
///   the conversion still arrives per frame in `ColorUniform`.
///
/// Two deliberate, stated divergences from `fs_layer`:
/// 1. **NV12 has no alpha plane** (57-RESEARCH.md Pitfall 6). `fs_layer` scales
///    rgb by `texel.a`; a Y/UV pair has no `.a` to sample. Every hardware-decoded
///    video texel is therefore opaque BY CONSTRUCTION (`a = 1.0`) and the only
///    alpha in play is the resolved layer `opacity`. `misc.y` (the
///    straight/premultiplied flag) is consequently IGNORED here: with an
///    implicit `texel.a == 1.0` both of `fs_layer`'s branches already reduce to
///    `opacity`, so honouring the flag could not change a single byte.
/// 2. **Sampled, not `textureLoad`ed.** `NV12_SHADER` fetches exact integer
///    texels because it presents ONE frame 1:1; a LAYER scales to an arbitrary
///    dest rect, so this shader samples with the same filtering sampler
///    `fs_layer` uses. The sample UV is clamped to the display region's last
///    texel centre (per plane — chroma texels are twice as wide in normalized
///    space) so linear filtering can never reach the macroblock-alignment
///    padding that lives beyond the display width/height in the hw frame pool.
#[cfg(all(windows, feature = "hwdecode"))]
const NV12_LAYER_SHADER_BODY: &str = r#"
// --- Phase 57 (PLAY-02): NV12-sourced layer blend --------------------------
// Layout mirrors engine::colorspace::ColorParams (80 bytes) — IDENTICAL to
// NV12_SHADER's ColorUniform, for the identical reason: the conversion is
// CPU-selected per frame from the frame's REAL tags, never hardcoded in WGSL.
struct Nv12ColorUniform {
    mat: mat3x3<f32>,
    range_offset: vec3<f32>,
    range_scale: vec3<f32>,
}

// disp_tex: xy = the frame's DISPLAY dims (px); zw = the WHOLE hw frame pool
// texture's dims (px). The bound plane views cover the macroblock-aligned pool
// texture, so the display region is a sub-rect of it and the CPU must supply
// both — textureDimensions() would report the padded size.
struct Nv12PlaneGeom {
    disp_tex: vec4<f32>,
}

@group(0) @binding(2) var nv12_luma: texture_2d_array<f32>;
@group(0) @binding(3) var nv12_chroma: texture_2d_array<f32>;
@group(0) @binding(4) var<uniform> nv12_color: Nv12ColorUniform;
@group(0) @binding(5) var<uniform> nv12_plane: Nv12PlaneGeom;

// COPIED VERBATIM from NV12_SHADER::yuv_to_rgb (compositor.rs) — range-decode,
// then the matrix, exactly libswscale's own two-stage internal structure.
fn nv12_layer_yuv_to_rgb(y: f32, cb: f32, cr: f32) -> vec3<f32> {
    let yuv = (vec3<f32>(y, cb, cr) - nv12_color.range_offset) * nv12_color.range_scale;
    return nv12_color.mat * yuv;
}

@fragment
fn fs_layer_nv12(in: VsOut) -> @location(0) vec4<f32> {
    // (a) fs_layer's crop-inset UV remap — COPIED VERBATIM.
    let cp = layer_params.crop;
    let uv = vec2<f32>(
        cp.x + in.uv.x * (1.0 - cp.x - cp.z),
        cp.y + in.uv.y * (1.0 - cp.y - cp.w),
    );

    // (b) display-region UV -> whole-pool-texture UV, clamped per plane to the
    // last texel centre inside the DISPLAY region (luma texel = 1/tex,
    // chroma texel = 2/tex in the same normalized space) so the filter can
    // never pull in macroblock padding.
    let disp = nv12_plane.disp_tex.xy;
    let tex = nv12_plane.disp_tex.zw;
    let raw = uv * disp / tex;
    let uv_luma = clamp(raw, vec2<f32>(0.5) / tex, (disp - vec2<f32>(0.5)) / tex);
    let uv_chroma = clamp(raw, vec2<f32>(1.0) / tex, (disp - vec2<f32>(1.0)) / tex);
    // Array index 0 is VIEW-RELATIVE (the D2Array plane views bake the frame's
    // real pool slice into base_array_layer) — the same contract NV12_SHADER
    // documents at its own textureLoad.
    let y = textureSampleLevel(nv12_luma, frame_samp, uv_luma, 0, 0.0).r;
    let cbcr = textureSampleLevel(nv12_chroma, frame_samp, uv_chroma, 0, 0.0).rg;

    // (c) the conversion + clamp, COPIED VERBATIM from NV12_SHADER::fs_main.
    let rgb = clamp(nv12_layer_yuv_to_rgb(y, cbcr.r, cbcr.g), vec3<f32>(0.0), vec3<f32>(1.0));

    // (d) NV12 has NO ALPHA PLANE (Pitfall 6): every hw-decoded video texel is
    // opaque a = 1.0 BY CONSTRUCTION, so the effective alpha is exactly the
    // resolved layer opacity. Output PREMULTIPLIED — the same form fs_layer
    // feeds the SAME premultiplied-over BlendState.
    let a = layer_params.misc.x;
    return vec4<f32>(rgb * a, a);
}
"#;

/// The offscreen render-target format (`composite_to_rgba`). NOT sRGB so RGBA
/// bytes pass through unmodified for exact frame-diff parity.
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// LOCKED CONVENTION (D-01, Phase 18 keystone — 18-RESEARCH.md §Q1):
/// premultiplied-alpha "over", `(One, OneMinusSrcAlpha)` on BOTH color and
/// alpha. Extracted to a `const` in Phase 57 so the NEW `fs_layer_nv12`
/// pipeline binds the IDENTICAL blend state rather than a hand-re-typed copy
/// of it (threat T-57-07: convention drift between two pipelines that must
/// agree). The values are unchanged — `build()` still reads exactly these
/// bytes, and `tests/alpha_convention.rs` still pins the resulting byte 128.
const PREMULTIPLIED_OVER: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

/// Contain-fit letterbox: the `[x, y, w, h]` sub-rect (same units as inputs)
/// that centers `content_w x content_h` inside `target_w x target_h`,
/// preserving aspect ratio ("contain", never crops — the design-handoff
/// Preview rule). ONE formula — reused by `composite_to_surface` (the
/// on-screen video letterbox) and the Phase-13 native pointer-routing spike
/// (screen-px -> normalized frame-content coords), so the two can never
/// silently drift apart. Extracted VERBATIM from `composite_to_surface`'s
/// former inline computation (behavior unchanged, just named + reusable).
pub fn contain_fit_viewport(
    target_w: f32,
    target_h: f32,
    content_w: f32,
    content_h: f32,
) -> [f32; 4] {
    let cw = content_w.max(1.0);
    let ch = content_h.max(1.0);
    let scale = (target_w / cw).min(target_h / ch);
    let vw = cw * scale;
    let vh = ch * scale;
    [(target_w - vw) * 0.5, (target_h - vh) * 0.5, vw, vh]
}

/// Per-layer alpha interpretation (Phase 28, OVL-01) — the ENGINE-side mirror
/// of `core::AlphaMode` (same as `LayerTransform` mirrors `ClipTransform`;
/// the app layer maps between them at the composite call site). Tells `fs_layer`
/// how to treat the decoded frame's alpha:
/// - `Straight` (default): rgb is NOT premultiplied by its own alpha — the
///   shader premultiplies exactly once (`rgb * texel.a * opacity`), the LOCKED
///   Phase-18 convention. Every existing layer (video/text/shapes) is Straight,
///   so this default keeps their output bit-identical.
/// - `Premultiplied`: rgb ALREADY carries `rgb *= its own alpha` (e.g. a
///   pre-premultiplied source); the shader scales by `opacity` ONLY, skipping
///   the `texel.a` multiply to avoid double-premultiply washout (Pitfall 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlphaMode {
    /// Straight (non-premultiplied) alpha — the shader premultiplies in-shader.
    #[default]
    Straight,
    /// Already-premultiplied alpha — the shader skips its own premultiply.
    Premultiplied,
}

/// Per-layer transform — the D-08 vocabulary, adopted verbatim so Phase 19
/// keyframes animate these exact fields. Two confusion-prone conventions,
/// stated explicitly:
/// - `position` is the destination rect's TOP-LEFT corner (NOT its centre),
///   in normalized 0-1 canvas space;
/// - `scale` is the destination rect's normalized WIDTH/HEIGHT (NOT a
///   multiplier) — `(1.0, 1.0)` = the full canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerTransform {
    /// Dest-rect top-left `(x, y)`, normalized 0-1 of the canvas.
    pub position: (f32, f32),
    /// Dest-rect normalized `(width, height)` — 0-1 of the canvas.
    pub scale: (f32, f32),
    /// Rotation in degrees, clockwise on screen (y-down pixel space), about
    /// the destination rect's centre.
    pub rotation_deg: f32,
}

impl Default for LayerTransform {
    /// Identity: top-left origin, full-canvas size, no rotation.
    fn default() -> Self {
        Self {
            position: (0.0, 0.0),
            scale: (1.0, 1.0),
            rotation_deg: 0.0,
        }
    }
}

/// Per-layer crop: four side insets, each 0-1 of the SOURCE frame (D-08). The
/// remaining source window is what gets sampled across the layer's quad.
/// Identity = all zeros.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayerCrop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// One input layer for the multi-layer composite (`composite_layers_to_rgba` /
/// `composite_layers_to_surface`).
pub struct Layer {
    /// Decoded RGBA frame (FFmpeg output; video frames are opaque, a=255).
    pub frame: Frame,
    /// Layer opacity in `[0.0, 1.0]`. Defensively clamped at the blend site
    /// (max-then-min, mirroring `SetClipVolume`) — the engine's last-line
    /// defense (T-18-04); core validation is the primary gate (Plan 03).
    pub opacity: f32,
    /// Position/scale/rotation defining the layer's destination rect (D-08).
    pub transform: LayerTransform,
    /// Source-side crop insets (D-08).
    pub crop: LayerCrop,
    /// How to interpret the frame's alpha (Phase 28, OVL-01). Defaults to
    /// `Straight` — bit-identical to pre-Phase-28 for every opaque/synthetic
    /// layer. Only `Premultiplied` sources (already `rgb *= alpha`) change the
    /// shader path (skip the in-shader premultiply — packed into `misc.y`).
    pub alpha_mode: AlphaMode,
}

impl Layer {
    /// Full-canvas identity layer (identity transform, no crop) at `opacity`.
    /// Defaults `alpha_mode` to `Straight` (the LOCKED Phase-18 convention).
    pub fn new(frame: Frame, opacity: f32) -> Self {
        Self {
            frame,
            opacity,
            transform: LayerTransform::default(),
            crop: LayerCrop::default(),
            alpha_mode: AlphaMode::Straight,
        }
    }
}

/// Host-side per-layer geometry resolution — the ONE place (Pitfall 4) where
/// the D-08 transform vocabulary + crop become GPU uniform bytes, shared
/// verbatim by the offscreen (`composite_layers_to_rgba`) and surface
/// (`composite_layers_to_surface*`) paths so preview and export can never
/// silently drift:
/// - the transform defines the DESTINATION RECT (`position` = top-left,
///   `scale` = normalized dims, `rotation_deg` about the rect centre);
/// - the CROPPED (visible) SOURCE is CONTAIN-FIT (letterboxed, never
///   fit-cropped) inside that rect via the same `contain_fit_viewport` formula
///   the single-frame preview letterbox uses (Open Question 1 resolution:
///   transform defines the dest rect, contain-fit the source inside it).
///   CR-01 FIX (23-REVIEW.md): the fit is sized from the crop-adjusted
///   effective dims (`native * (1 - insets)`), NOT the native frame, so the
///   visible region reframes to FILL the slot (identity crop is byte-for-byte
///   unchanged — factors are exactly 1.0). `contain_fit_viewport` centres the
///   fitted quad, so the fitted-quad centre IS the dest-rect centre — rotation
///   pivots about that shared centre;
/// - `crop` insets are clamped to [0,1], size the effective source dims for
///   the fit above, and remap the sampled UV window in the fragment shader;
/// - opacity gets the T-18-04 last-line clamp (max-then-min; NaN -> 0.0).
///
/// Returns `None` for a draw that can contribute nothing — a degenerate
/// (zero/negative-area) dest rect, non-finite geometry, or a crop that leaves
/// no source — which callers treat as a NO-OP DRAW rather than risking a
/// divide-by-zero or NaN vertex in the shader (threat T-18-01).
fn layer_params_bytes(layer: &Layer, out_w: f32, out_h: f32) -> Option<[u8; 64]> {
    layer_params_bytes_from(
        layer.frame.width,
        layer.frame.height,
        layer.opacity,
        layer.transform,
        layer.crop,
        layer.alpha_mode,
        out_w,
        out_h,
    )
}

/// Phase 57: [`layer_params_bytes`]'s body, taking the SOURCE DIMS directly
/// instead of a CPU `Layer`, so a GPU-resident NV12 layer (which has no
/// `Frame`) resolves its geometry through the SAME ONE spot. Extracted
/// verbatim — `layer_params_bytes` above is now a thin forwarder and its
/// output is byte-for-byte unchanged (pinned by `layer_params_tests`).
///
/// # A SECOND READER OF THIS ARITHMETIC LIVES OUTSIDE THIS CRATE
///
/// Phase 60 (OCCL-01): `preview::occlusion::fills_dest_exactly` MIRRORS the
/// steps below — the degenerate-rect guard, the `native.max(1)` cast, the
/// crop-adjusted effective dims and the [`contain_fit_viewport`] call, in this
/// order — to decide, before anything decodes, whether a layer paints EVERY
/// canvas pixel and therefore whether the layers beneath it can be discarded.
/// It calls the same public `contain_fit_viewport` rather than re-deriving the
/// letterbox, precisely so it cannot form a second opinion about geometry.
///
/// **If the contain-fit rule here changes, that predicate must change with it.**
/// A disagreement of one pixel at one aspect ratio is not a cosmetic drift
/// there: it is a layer culled that should not have been, and a frame whose
/// pixels differ from the un-culled composite — the failure OCCL-01 exists to
/// forbid.
#[allow(clippy::too_many_arguments)]
fn layer_params_bytes_from(
    native_w_px: u32,
    native_h_px: u32,
    layer_opacity: f32,
    t: LayerTransform,
    cr: LayerCrop,
    alpha_mode: AlphaMode,
    out_w: f32,
    out_h: f32,
) -> Option<[u8; 64]> {
    let finite = t.position.0.is_finite()
        && t.position.1.is_finite()
        && t.scale.0.is_finite()
        && t.scale.1.is_finite()
        && t.rotation_deg.is_finite()
        && cr.left.is_finite()
        && cr.top.is_finite()
        && cr.right.is_finite()
        && cr.bottom.is_finite();
    if !finite {
        return None;
    }

    // Destination rect in target pixels. `!(> 0)` also rejects negative area.
    //
    // 18-REVIEW HI-01: the raw inputs above are finite, but these PRODUCTS can
    // still overflow f32 to +/-Infinity (a finite scale ~1e37 times a real
    // canvas width already exceeds f32::MAX) — and `Infinity > 0.0` is TRUE in
    // IEEE-754, so the degenerate-rect guard alone would pass an infinite rect
    // through. Re-check finiteness of every DERIVED value, not just the raw
    // fields, so the documented T-18-01 no-op-draw guarantee actually holds.
    let dest_w = t.scale.0 * out_w;
    let dest_h = t.scale.1 * out_h;
    if !(dest_w > 0.0 && dest_h > 0.0) || !dest_w.is_finite() || !dest_h.is_finite() {
        return None;
    }
    let dest_x = t.position.0 * out_w;
    let dest_y = t.position.1 * out_h;
    if !dest_x.is_finite() || !dest_y.is_finite() {
        return None;
    }

    // Crop insets clamp to [0,1]; a crop that leaves no source is a no-op.
    // NOTE (CR-01, 23-REVIEW.md): the clamp + degenerate-reject are computed
    // HERE — BEFORE the contain-fit — because the fit must be sized from the
    // CROPPED (effective) source dims, which need the clamped insets. This
    // reorder is behavior-preserving for the guards themselves (identical
    // clamp/reject arithmetic, just moved earlier).
    let cl = cr.left.max(0.0).min(1.0);
    let ct = cr.top.max(0.0).min(1.0);
    let crr = cr.right.max(0.0).min(1.0);
    let cb = cr.bottom.max(0.0).min(1.0);
    if cl + crr >= 1.0 || ct + cb >= 1.0 {
        return None;
    }

    // Contain-fit the CROPPED (visible) source inside the dest rect (the ONE
    // letterbox formula). CR-01 FIX (23-REVIEW.md): the destination quad must
    // be sized from the crop-adjusted source dims, NOT the native frame.
    // Previously this fed `frame.width/height` (native) to `contain_fit`, so a
    // clip whose native aspect differed from its slot letterboxed INSIDE the
    // slot even when the crop was chosen to make it COVER (e.g. a 16:9 clip in
    // a tall side_by_side/grid slot) — the crop only remapped UVs, never
    // reshaped the quad. Feeding the cropped dims makes the visible region
    // fill the dest rect: the correct, standard "reframe/zoom-and-fit the
    // visible region into the dest rect" crop semantic for ALL clips, not just
    // layouts. The crop insets (cl/ct/crr/cb) STILL remap UVs in `fs_layer`
    // across this (now correctly-shaped) quad — only WHICH source dims feed the
    // fit changed; `contain_fit_viewport`, the shader, and the UV crop math are
    // untouched.
    //
    // IDENTITY-CROP INVARIANT (byte-for-byte): with crop (0,0,0,0) each factor
    // `(1.0 - inset - inset)` is exactly 1.0, so `effective_w/effective_h` ==
    // the `frame.width/height.max(1) as f32` values previously passed here.
    // `contain_fit_viewport` therefore receives identical arguments, and
    // fit_w/fit_h — and the whole 64-byte uniform — are bit-identical to the
    // pre-fix output. This preserves single-clip and every non-cropped-layer
    // parity across Phases 18-22 (`alpha_convention.rs`,
    // `multilayer_parity.rs` single-identity-layer / parity gates).
    let native_w = native_w_px.max(1) as f32;
    let native_h = native_h_px.max(1) as f32;
    let effective_w = native_w * (1.0 - cl - crr);
    let effective_h = native_h * (1.0 - ct - cb);
    let [_, _, fit_w, fit_h] = contain_fit_viewport(dest_w, dest_h, effective_w, effective_h);
    let cx = dest_x + dest_w * 0.5;
    let cy = dest_y + dest_h * 0.5;
    // HI-01 second half: even with finite dest_x/dest_w, the centre SUM can
    // overflow (dest_x + dest_w/2 > f32::MAX). fit_w/fit_h are checked as
    // belt-and-braces — everything packed into the uniform must be finite
    // (the crop-adjusted effective dims are finite by construction: native
    // dims are finite and each factor is in (0,1], so this guard still holds).
    if !(cx.is_finite() && cy.is_finite() && fit_w.is_finite() && fit_h.is_finite()) {
        return None;
    }

    // T-18-04 last-line defense: clamp to finite [0,1] via max-then-min
    // (mirrors SetClipVolume — NaN.max(0.0) == 0.0, so NaN -> 0.0; ±inf clamp
    // to the range ends). A poisoned opacity can never reach the blend shader.
    let opacity = layer_opacity.max(0.0).min(1.0);

    let (sin, cos) = t.rotation_deg.to_radians().sin_cos();

    // Phase 28 (OVL-01): pack alpha_mode into the previously-unused misc.y slot
    // (vals[13]) as 1.0 = Premultiplied / 0.0 = Straight. A two-variant enum ->
    // a boolean-valued slot: no NaN/Inf path introduced (threat T-28-06). Every
    // existing Straight layer packs 0.0, so the whole 64-byte uniform stays
    // bit-identical to pre-Phase-28 output.
    let is_premultiplied = if matches!(alpha_mode, AlphaMode::Premultiplied) {
        1.0
    } else {
        0.0
    };
    let vals: [f32; 16] = [
        cx, cy, fit_w * 0.5, fit_h * 0.5, // rect
        cos, sin, out_w, out_h, // rot_target
        cl, ct, crr, cb, // crop
        opacity, is_premultiplied, 0.0, 0.0, // misc (x=opacity, y=alpha_mode)
    ];
    let mut bytes = [0u8; 64];
    for (i, v) in vals.iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
    }
    Some(bytes)
}

pub struct Compositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Offscreen pipeline — targets `OFFSCREEN_FORMAT`. Always built.
    pipeline: wgpu::RenderPipeline,
    /// Offscreen multi-layer BLEND pipeline (premultiplied "over") — targets
    /// `OFFSCREEN_FORMAT`, same shader source, `fs_layer` entry point. Always
    /// built. The `blend: None` `pipeline` above stays byte-for-byte untouched
    /// (single-clip parity path — Pitfall 2).
    blend_pipeline: wgpu::RenderPipeline,
    /// Bind group layout for the per-layer uniform (`layer_params`, group 1).
    layer_bind_group_layout: wgpu::BindGroupLayout,
    /// On-screen pipeline — targets the surface's negotiated format (Pitfall
    /// E). `Some` only when built via `new_with_surface`; `None` for the
    /// offscreen-only `new()` path.
    surface_pipeline: Option<wgpu::RenderPipeline>,
    /// On-screen multi-layer BLEND pipeline — the surface-format twin of
    /// `blend_pipeline` (same shader, same blend state, only the target
    /// format differs). Built whenever a surface format is known.
    surface_blend_pipeline: Option<wgpu::RenderPipeline>,
    /// PRESENT-ONLY sharp-magnification twin of `surface_pipeline`
    /// (quick-260803-g3d): identical descriptor, identical `vs_main`, identical
    /// `blend: None`, identical target format — the ONLY difference is the
    /// fragment entry point (`fs_present_sharp`, a Catmull-Rom bicubic).
    ///
    /// Built whenever a surface format is known and referenced from exactly ONE
    /// place: [`Compositor::blit_texture_to_surface`]'s magnification branch. It
    /// is deliberately NOT reachable from any composite or export path — the
    /// offscreen `pipeline`, `blend_pipeline`, the surface composite pipelines,
    /// and the readback twin `blit_texture_to_rgba` all still run `fs_main` /
    /// `fs_layer` and are byte-unchanged, which is what keeps exported pixels
    /// bit-identical to before this field existed.
    present_sharp_pipeline: Option<wgpu::RenderPipeline>,
    /// The surface's negotiated format when built via `new_with_surface`.
    /// Callers reuse this for `SurfaceConfiguration.format`.
    surface_format: Option<wgpu::TextureFormat>,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Phase 48 (plan 48-09): per-target-format cache of the NV12 composite
    /// pipeline + bind group layout. 48-06 built these PER CALL, recording
    /// hot-path caching as 48-09's wiring work — the GPU producer presents at
    /// media cadence, and a per-present shader compile + pipeline build would
    /// dominate the frame budget. At most two entries ever exist (the surface
    /// format and `OFFSCREEN_FORMAT`); a tiny Vec avoids a map dependency.
    /// Lazily filled by [`Compositor::nv12_pipeline`]; lock is held only for
    /// the lookup/insert, never across a render pass.
    #[cfg(all(windows, feature = "hwdecode"))]
    nv12_pipelines: std::sync::Mutex<
        Vec<(
            wgpu::TextureFormat,
            std::sync::Arc<(wgpu::RenderPipeline, wgpu::BindGroupLayout)>,
        )>,
    >,
    /// Phase 57 (PLAY-02): the same per-target-format cache shape, for the NEW
    /// blend-capable NV12 *layer* pipeline (`fs_layer_nv12`). Built at most
    /// once per target format, never per composite.
    #[cfg(all(windows, feature = "hwdecode"))]
    nv12_layer_pipelines: std::sync::Mutex<
        Vec<(
            wgpu::TextureFormat,
            std::sync::Arc<(wgpu::RenderPipeline, wgpu::BindGroupLayout)>,
        )>,
    >,
    /// Phase 57 (PLAY-07 / D-08): the PERSISTENT per-slot GPU resources
    /// `composite_mixed_layers_to_target` reuses every frame — the direct
    /// answer to `encode_layers_pass`'s per-call `create_texture` +
    /// `create_buffer` churn. Slot `i` is keyed by the layer's index in the
    /// mixed list; its texture is reallocated ONLY when that slot's source
    /// dims change, its uniform buffers never.
    ///
    /// Guarded by a `Mutex` held for the whole composite: the intended
    /// consumer is the ring's SINGLE coordinator thread (plan 57-06), and
    /// serializing is what makes a shared slot cache safe if that ever stops
    /// being true.
    mixed_slots: std::sync::Mutex<MixedSlotCache>,
    /// GPU-04's detection funnel, armed at THIS device's birth for
    /// presentation-capable devices only (quick-260829-n96 — see `build`).
    /// `None` on export/offscreen compositors by design, the same scope rule
    /// [`install_uncaptured_error_guard`] enforces (GPU-07). Held so the loss
    /// an armed callback records is observable
    /// ([`Compositor::device_lost_signal`]) rather than only logged.
    #[cfg(all(windows, feature = "hwdecode"))]
    device_lost: Option<crate::device_lost::DeviceLostSignal>,
    /// GPU-04's RECOVERY REQUEST flag (Phase 63, plan 63-01 — TRUST-01), set by
    /// the same device-birth response that runs the degrade, and read by
    /// [`Compositor::device_lost_pending`].
    ///
    /// The response REQUESTS recovery; it must never RUN it. wgpu delivers the
    /// device-lost callback on its own internal thread, and
    /// [`crate::device_lost::RecoveryPlan::recover`] tears down the very device
    /// that callback belongs to — so the recreation runs on whoever polls this
    /// flag (the present loop; [`crate::device_lost::PreviewRecovery`] at the
    /// engine tier), never inside the callback. `None` on export/offscreen
    /// compositors, the same GPU-07 boundary as `device_lost` above.
    #[cfg(all(windows, feature = "hwdecode"))]
    device_lost_pending: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// How THIS compositor was born, so device-lost recovery can rebuild it
    /// through the SAME `build` funnel rather than a hand-rolled twin — which
    /// is what keeps the uncaptured-error guard and GPU-04 detection re-armed
    /// on the recreated device by construction. See [`CompositorBirth`].
    birth: CompositorBirth,
}

/// Which constructor a [`Compositor`] came out of — recorded at `build` so
/// device-lost recovery can rebuild through the SAME funnel (Phase 63, plan
/// 63-01).
///
/// This exists because of the un-losable-placement argument `build` already
/// carries for [`install_uncaptured_error_guard`] and for GPU-04's detection:
/// both are armed at the ONE `request_device` in this crate outside tests, so a
/// recovery that recreated the device any other way would silently hand back a
/// compositor with neither. Recreating through the recorded recipe makes
/// "the recreated device is armed exactly like the original" true by
/// construction instead of by two edits staying in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorBirth {
    /// [`Compositor::new`] / `new_offscreen_on` — the export/readback shape.
    /// NOT presentation-capable: no guard, no detection, nothing to recover
    /// (GPU-07 — export encodes through the CLI sidecar).
    Offscreen,
    /// [`Compositor::new_with_debug_surface_format`] — the documented headless
    /// twin of the live path, carrying its negotiated format.
    DebugSurfaceFormat(wgpu::TextureFormat),
    /// [`Compositor::new_with_surface`] — the LIVE path
    /// (`crates/ffi/src/panel/surface.rs::attach_gpu`). The engine CANNOT
    /// rebuild this by itself: the `wgpu::Surface` belongs to the host's
    /// `SwapChainPanel` and the instance that created it, neither of which the
    /// compositor holds. Recovery therefore requires the host to supply
    /// `RecoveryHostHooks::recreate_device` — the surface choreography plan
    /// 63-02 owns (CONTEXT D-03: "the hard part is the surface").
    HostSurface,
}

/// Replace wgpu's DEFAULT uncaptured-error handler — which PANICS the calling
/// thread — on a PRESENTATION-CAPABLE device.
///
/// # What this is, and why it is here
///
/// This is the panic-containment half of the 48-gpu-oom-4k fix
/// (`.planning/debug/resolved/48-gpu-oom-4k-preview-present-panic.md`),
/// restored at the device-birth site after four phases with no caller
/// (debug session `gpu-oom-guard-lost-in-the-cutover`). The 4K
/// OOM surfaced as an uncaptured `wgpu error: Out of Memory` raised by a
/// per-present uniform-buffer create; the default handler killed the
/// `rudis-preview-present` thread PERMANENTLY while the process stayed alive,
/// so preview could never present again. **Allocation failure on the live
/// device must degrade, never assassinate a thread:**
///
/// - EVERY uncaptured error is LOGGED instead of panicking (an invalid resource
///   from a failed create cascades as further wgpu errors, not process crashes;
///   the present loop's `Result`-guarded surface calls keep logging per frame).
/// - `OutOfMemory` additionally FORCE-ENGAGES the GPU-05 session latch
///   ([`crate::force_hw_latch_engage`]), so every later clip delegation routes
///   to the proven CPU-sidecar software path instead of re-attempting hardware
///   decode against an exhausted adapter — a VRAM OOM treated like the
///   capability failure it effectively is.
///
/// Genuine device loss is NOT handled here: wgpu reports that through the
/// device-lost callback (`crate::device_lost`), which owns the WR-01 recovery
/// funnel.
///
/// # Why PRESENTATION-CAPABLE devices only
///
/// Not caution — semantics. The OOM arm demotes hardware *preview decode*, and
/// only a presenting device drives that path. Export/offscreen compositors
/// ([`Compositor::new`]) encode through the **CLI sidecar** and never open a
/// hardware decoder at all (GPU-07), so an export compositor's OOM demoting
/// preview would be a false signal; and export keeps wgpu's default handler,
/// so nothing about export failure reporting changes. In production
/// `compatible_surface.is_some()` is true for exactly one device — the one
/// `crates/ffi/src/panel/surface.rs::attach_gpu` builds behind the attached
/// `SwapChainPanel`. `debug_surface_format.is_some()` is that same path's
/// documented headless twin ([`Compositor::new_with_debug_surface_format`]),
/// which is what lets the guard be PROVEN to fire without a window
/// (`crates/preview/tests/gpu_oom_guard.rs`).
fn install_uncaptured_error_guard(device: &wgpu::Device) {
    device.on_uncaptured_error(Box::new(|e| {
        let oom = matches!(e, wgpu::Error::OutOfMemory { .. });
        eprintln!(
            "preview: uncaptured wgpu error on the live device (contained, not panicking): {e}"
        );
        if oom {
            // cfg-gated to the arm where a hardware decoder exists to demote;
            // the log above is unconditional, so containment itself is not.
            #[cfg(all(windows, feature = "hwdecode"))]
            crate::force_hw_latch_engage("uncaptured wgpu OutOfMemory on the live preview device");
        }
    }));
}

impl Compositor {
    /// Create an offscreen (surface-less) wgpu device on the default adapter.
    ///
    /// On macOS this resolves to the Metal backend; on Windows the backend is
    /// pinned explicitly to DX12 (SPIKE-05, Phase 44).
    /// Use this for the export/CI/`preview_timeline_at` readback path.
    pub fn new() -> Result<Self, EngineError> {
        Self::new_offscreen(None)
    }

    /// TEST/DEBUG ONLY (headless SC-3 parity gate): build a surface-LESS
    /// compositor whose `surface_pipeline` targets an EXPLICIT `format` instead
    /// of a real negotiated `wgpu::Surface`. This makes the on-screen surface's
    /// rendering path (the surface-format pipeline) exercisable headlessly — a
    /// `cargo test` process has no window, so it cannot create a real surface,
    /// but the ONLY thing that diverges between the offscreen and on-screen
    /// paths is the render-target FORMAT (Pitfall E), which this reproduces
    /// exactly. The offscreen `pipeline` is still built too, so ONE compositor
    /// built this way serves BOTH `composite_to_rgba` and
    /// `composite_to_surface_debug_readback` on the same device.
    ///
    /// Pass the SAME format the production surface negotiates (Windows/Vulkan:
    /// the linear `Bgra8Unorm` the compositor prefers via `!is_srgb()`, per
    /// DECISIONS.md Decision 1). This is NOT a production entry point — the real
    /// present thread builds its compositor via `new_with_surface`.
    pub fn new_with_debug_surface_format(
        format: wgpu::TextureFormat,
    ) -> Result<Self, EngineError> {
        Self::new_offscreen(Some(format))
    }

    /// TEST/PROOF ONLY: [`Compositor::new_with_debug_surface_format`] on a
    /// CALLER-SUPPLIED `wgpu::Instance`.
    ///
    /// The one thing an instance carries that this crate's own offscreen
    /// instance does not is `MemoryBudgetThresholds` — wgpu's own
    /// `QueryVideoMemoryInfo` circuit breaker, which production arms at
    /// `for_resource_creation: Some(95)` (`panel/surface.rs::preview_wgpu_instance`,
    /// "GPU-03 defense-in-depth"). Handing in an instance with a HOSTILE
    /// threshold is what lets `crates/preview/tests/gpu_oom_guard.rs` drive a
    /// REAL `wgpu::Error::OutOfMemory` — through wgpu's real allocation path,
    /// into the real uncaptured-error sink — in seconds instead of by
    /// exhausting a card's VRAM, and it reaches the guard through the SAME
    /// [`Compositor::build`] line that the shipped `new_with_surface` runs.
    ///
    /// Not a production entry point: the live preview compositor is built by
    /// `crates/ffi/src/panel/surface.rs::attach_gpu` via
    /// [`Compositor::new_with_surface`].
    #[doc(hidden)]
    pub fn new_with_debug_surface_format_on(
        instance: &wgpu::Instance,
        format: wgpu::TextureFormat,
    ) -> Result<Self, EngineError> {
        Self::build(instance, None, Some(format), true)
    }

    /// TEST/PROOF ONLY: [`Compositor::new`] (offscreen — the EXPORT shape) on a
    /// CALLER-SUPPLIED `wgpu::Instance`.
    ///
    /// The CONTROL twin of [`Compositor::new_with_debug_surface_format_on`].
    /// Same instance, same `build`, but NOT presentation-capable — so
    /// [`install_uncaptured_error_guard`] is deliberately not applied and wgpu's
    /// default (panicking) handler remains. `crates/preview/tests/gpu_oom_guard.rs`
    /// uses it to show, on the very same hostile budget threshold, that the
    /// allocation genuinely does raise `OutOfMemory` and genuinely would kill
    /// the calling thread — which is what makes the guarded arm's "did not
    /// panic" mean something — and, in the same stroke, that export/offscreen
    /// devices are untouched by the guard.
    #[doc(hidden)]
    pub fn new_offscreen_on(instance: &wgpu::Instance) -> Result<Self, EngineError> {
        Self::build(instance, None, None, true)
    }

    /// The one place the offscreen (surface-less) instance is created — shared by
    /// `new()` and `new_with_debug_surface_format()` so the backend pin and its
    /// fallback can never drift apart between the two.
    ///
    /// SPIKE-05 (Phase 44): the backend is pinned explicitly. Unpinned, wgpu
    /// resolved to Vulkan on Windows; the v7 native-shell path (SwapChainPanel
    /// hosting, D3D11VA interop) requires DX12, and preview and export must run
    /// the SAME backend or "what you preview is what you export" stops being
    /// true. Non-Windows keeps the default set so the Phase-8 macOS dev target
    /// (Metal) is unaffected.
    ///
    /// The pin narrows the adapter search to one backend, so this path — unlike
    /// the surface path, which by definition runs on a machine with a display —
    /// asks for a SOFTWARE fallback adapter (WARP on DX12; it ships with
    /// Windows) before giving up. Without that, a headless/CI/RDP box with no
    /// usable hardware DX12 adapter would lose the export/readback path
    /// entirely, where the pre-pin `Backends::all()` search could still land on
    /// a software Vulkan implementation.
    fn new_offscreen(
        debug_surface_format: Option<wgpu::TextureFormat>,
    ) -> Result<Self, EngineError> {
        let backends = if cfg!(target_os = "windows") {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::all()
        };
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        Self::build(&instance, None, debug_surface_format, true)
    }

    /// Create a surface-compatible wgpu device on an adapter chosen for the
    /// given `surface`, and additionally build `surface_pipeline` against the
    /// surface's negotiated format so `composite_to_surface` can present into
    /// it. The offscreen `pipeline` is ALSO built (a single `Compositor` built
    /// this way still serves `composite_to_rgba`). Wave-1-DECIDED shape.
    ///
    /// CRITICAL: `instance` MUST be the same `wgpu::Instance` that created
    /// `surface` — a surface and the adapter/device used with it must come from
    /// one instance, or wgpu panics with "Surface does not exist" at present.
    pub fn new_with_surface(
        instance: &wgpu::Instance,
        surface: &wgpu::Surface,
    ) -> Result<Self, EngineError> {
        Self::build(instance, Some(surface), None, false)
    }

    /// Shared construction on the given `instance`. When `compatible_surface`
    /// is `Some`, the adapter is chosen compatibly with that surface and a
    /// second pipeline is built for the surface's negotiated format from the
    /// SAME shader/layout. When `compatible_surface` is `None` but
    /// `debug_surface_format` is `Some`, the surface pipeline is still built —
    /// against that explicit format — for the headless SC-3 parity gate (no
    /// real surface required); see `new_with_debug_surface_format`.
    ///
    /// `allow_software_adapter` retries the adapter search with
    /// `force_fallback_adapter: true` when no hardware adapter answers on this
    /// instance's backend(s). Only the offscreen path sets it (see
    /// `new_offscreen`); the surface path leaves it `false` — a real surface
    /// implies a real display, and silently presenting through a software
    /// rasterizer would be a worse failure than an honest error.
    fn build(
        instance: &wgpu::Instance,
        compatible_surface: Option<&wgpu::Surface>,
        debug_surface_format: Option<wgpu::TextureFormat>,
        allow_software_adapter: bool,
    ) -> Result<Self, EngineError> {
        let request = |force_fallback_adapter: bool| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter,
                compatible_surface,
            }))
        };
        let adapter = match request(false) {
            Ok(adapter) => adapter,
            Err(e) if allow_software_adapter => request(true).map_err(|fallback_err| {
                EngineError::Gpu(format!(
                    "no suitable GPU adapter: {e}; software-fallback adapter also \
                     unavailable: {fallback_err}"
                ))
            })?,
            Err(e) => return Err(EngineError::Gpu(format!("no suitable GPU adapter: {e}"))),
        };

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rudis-engine-device"),
            // Phase 48 (GPU-02, plan 48-06 — ADDITIVE): opt into NV12
            // plane-view sampling WHEN the adapter has it, so hardware-decoded
            // frames can be imported onto this same device (`import_frame`
            // requires the feature; 48-05 recorded this wiring for the real
            // compositor device). The request is MASKED by
            // `adapter.features()`: on adapters without NV12 support it
            // requests nothing and device creation behaves exactly as before,
            // so the export/offscreen path can never newly fail here (GPU-07).
            required_features: adapter.features() & wgpu::Features::TEXTURE_FORMAT_NV12,
            ..Default::default()
        }))
        .map_err(|e| EngineError::Gpu(format!("failed to create device: {e}")))?;

        // 48-gpu-oom-4k's panic-containment half, restored at ITS DEVICE-BIRTH
        // SITE (debug session `gpu-oom-guard-lost-in-the-cutover`, 2026-08-22).
        // Presentation-capable devices ONLY — see
        // `install_uncaptured_error_guard` for why that is the semantic
        // boundary and not merely the cautious one. This line, not a caller, is
        // what makes the guard un-losable: there is exactly one
        // `request_device` in this crate outside tests, it is directly above,
        // and every constructor funnels through it.
        if compatible_surface.is_some() || debug_surface_format.is_some() {
            install_uncaptured_error_guard(&device);
        }

        // GPU-04's DETECTION half, restored at ITS device-birth site
        // (quick-260829-n96). The GATE-07 cutover orphaned it exactly as it
        // orphaned the OOM guard above — the caller lived in
        // `src-tauri/src/native_surface.rs` and the WinUI shell never
        // re-installed it (v8.0-MILESTONE-AUDIT § 2a, D-8a: a real driver TDR
        // left preview permanently unable to present). Same un-losable
        // placement argument as the guard: THIS LINE, not a caller — same
        // boundary condition, so the two post-cutover restorations live and die
        // together at the one `request_device` every constructor funnels
        // through.
        //
        // SCOPE (rewritten by plan 63-01 — TRUST-01): DEGRADE **AND REQUEST
        // RECOVERY**. The sentence that stood here until 2026-08-29 —
        // "`RecoveryPlan::recover`'s coordinated recreation is NOT wired here
        // … preview present is dead until restart" — was true and is no longer.
        // The response now ALSO raises `device_lost_pending`, and
        // `crate::device_lost::PreviewRecovery` consumes that flag to run the
        // coordinated six-step recreation THROUGH THIS SAME FUNNEL, so the
        // recreated device comes back with the guard above and the detection
        // below already armed. `crates/engine/tests/device_lost_wiring.rs::
        // removal_then_recovery_presents_a_frame` presents a frame after a real
        // RemoveDevice to prove it.
        //
        // STILL OUT OF SCOPE, and honestly so: the LIVE `SwapChainPanel`
        // surface. A compositor born via `new_with_surface`
        // (`CompositorBirth::HostSurface`) cannot rebuild itself — the surface
        // and its instance belong to the shell — so its host must supply
        // `RecoveryHostHooks::recreate_device`. That present-loop choreography
        // is plan 63-02's (CONTEXT D-03), and until it lands the ENGINE tier is
        // where recovery is provable.
        //
        // REQUEST, NEVER RUN, inside the callback: wgpu delivers this on its
        // own internal thread and `recover()` tears down the very device the
        // callback belongs to. The flag is the whole point of the split.
        //
        // `Destroyed` never reaches the response (`register_with_response`
        // filters it), so app exit and offscreen-compositor drops cannot open
        // spurious grace windows or request spurious recoveries.
        #[cfg(all(windows, feature = "hwdecode"))]
        let (device_lost, device_lost_pending) = if compatible_surface.is_some()
            || debug_surface_format.is_some()
        {
            let signal = crate::device_lost::DeviceLostSignal::new();
            let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let requested = std::sync::Arc::clone(&pending);
            signal.register_with_response(&device, move || {
                eprintln!(
                    "device_lost: live preview device lost — degrading (GPU-04): latch grace \
                     window + pooled D3D11VA device drop, and REQUESTING coordinated recovery \
                     (TRUST-01); the recreation runs on the driver, never on this callback's \
                     thread"
                );
                // The degrade FIRST and byte-unchanged: the grace-window latch
                // behaviour is pinned by
                // `production_construction_path_detects_loss_and_degrades`.
                crate::note_hw_device_reset();
                requested.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            (Some(signal), Some(pending))
        } else {
            (None, None)
        };

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fullscreen-quad"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frame-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compositor-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // Same shader/vertex/bind-group everywhere; ONLY the color-target
        // format differs between offscreen and on-screen pipelines.
        let make_pipeline = |format: wgpu::TextureFormat, label: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };

        // Offscreen pipeline — always built, format unchanged from Phase 1.
        let pipeline = make_pipeline(OFFSCREEN_FORMAT, "compositor-pipeline");

        // The PRESENT-ONLY sharp twin (quick-260803-g3d). Same descriptor shape
        // as `make_pipeline` in every respect — same `pipeline_layout`, so the
        // same group-0 (texture @0, sampler @1) bind group is bound unchanged;
        // same `vs_main`; same `blend: None`; same TriangleStrip — differing in
        // the fragment entry point ALONE. Written out rather than parameterized
        // into `make_pipeline` so that closure, which every pre-existing
        // pipeline in this file is built through, stays byte-untouched.
        let make_present_sharp_pipeline = |format: wgpu::TextureFormat, label: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_present_sharp"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };

        // Per-layer uniform bind group layout (group 1: `layer_params`, 64
        // bytes — rect/rot_target/crop/misc). Visible to BOTH stages: the
        // vertex shader positions the quad, the fragment shader crops/blends.
        let layer_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("layer-params-bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(64),
                    },
                    count: None,
                }],
            });
        let blend_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("compositor-blend-layout"),
                bind_group_layouts: &[&bind_group_layout, &layer_bind_group_layout],
                push_constant_ranges: &[],
            });

        // LOCKED CONVENTION (D-01, Phase 18 keystone — cite 18-RESEARCH.md §Q1):
        // premultiplied-alpha "over", premultiplication performed IN-SHADER
        // (`fs_layer`: rgb *= effective_alpha), blend (One, OneMinusSrcAlpha)
        // on BOTH color and alpha, blended in the existing NON-linearized
        // 8-bit space (no sRGB view/target anywhere — the Phase-9 "avoid sRGB
        // wash" decision stands, Pitfall 2), target cleared to opaque black.
        // Asserted byte: 50%-white over black == 128 exactly
        // (`tests/alpha_convention.rs`, SC-4). A single OPAQUE layer at
        // opacity 1.0 reduces to `One*rgb + (1-1)*dst = rgb` — bit-identical
        // to the `blend: None` pass-through, preserving single-clip MAD-0
        // parity. Changing any part of this convention is a deliberate,
        // parity-gate-breaking decision — not a refactor.
        // Phase 57: the values are now the module-level `PREMULTIPLIED_OVER`
        // const (byte-for-byte the same state this local always held) so the
        // new NV12-layer pipeline can bind THE SAME state object instead of a
        // re-typed copy — one convention, one place it is written (D-01).
        let premultiplied_over = PREMULTIPLIED_OVER;
        // Same discipline as `make_pipeline`: one shader source, one blend
        // state — ONLY the color-target format differs between the offscreen
        // and on-screen multi-layer pipelines. `vs_layer` positions each
        // layer's quad from the per-layer uniform (Plan 02).
        let make_blend_pipeline = |format: wgpu::TextureFormat, label: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&blend_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_layer"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_layer"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(premultiplied_over),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        // Offscreen blend pipeline — unchanged non-sRGB format (Pitfall 2).
        let blend_pipeline =
            make_blend_pipeline(OFFSCREEN_FORMAT, "compositor-blend-pipeline");

        // Surface pipeline — only when a surface was supplied. Prefer a LINEAR
        // (non-sRGB) format: our decoded RGBA bytes are already display-ready, so
        // rendering them to an sRGB target double-encodes them (Pitfall E — the
        // "whitish/washed-out" look). Fall back to formats[0] if no linear one.
        let (surface_pipeline, surface_format) = match compatible_surface {
            Some(surface) => {
                let caps = surface.get_capabilities(&adapter);
                if caps.formats.is_empty() {
                    return Err(EngineError::Gpu(
                        "surface reported no supported formats".to_string(),
                    ));
                }
                let format = caps
                    .formats
                    .iter()
                    .copied()
                    .find(|f| !f.is_srgb())
                    .unwrap_or(caps.formats[0]);
                (
                    Some(make_pipeline(format, "compositor-surface-pipeline")),
                    Some(format),
                )
            }
            // No real surface: build the surface pipeline against an EXPLICIT
            // format ONLY for the headless SC-3 parity gate (debug constructor).
            None => match debug_surface_format {
                Some(format) => (
                    Some(make_pipeline(format, "compositor-surface-pipeline-debug")),
                    Some(format),
                ),
                None => (None, None),
            },
        };

        // Surface-format twin of the blend pipeline — built whenever a
        // surface format is known (real surface OR the headless debug format)
        // so `composite_layers_to_surface` / its parity readback exist
        // alongside the single-frame surface path.
        let surface_blend_pipeline = surface_format
            .map(|format| make_blend_pipeline(format, "compositor-surface-blend-pipeline"));

        // The present-only sharp twin, built on the SAME condition as
        // `surface_pipeline` — keyed off `surface_format`, which is `Some` in
        // BOTH arms above (real negotiated surface AND the headless debug
        // format) and `None` otherwise. Keying off the format rather than
        // repeating the build inside each arm is the convention
        // `surface_blend_pipeline` directly above already uses, and it makes
        // "built exactly when the surface pipeline is built" true by
        // construction instead of by two edits staying in step.
        //
        // CONSTRUCTION TIME ONLY. Nothing in the present path may build a
        // pipeline; the no-readback body scan is the standing proof of that.
        let present_sharp_pipeline = surface_format
            .map(|format| make_present_sharp_pipeline(format, "compositor-present-sharp-pipeline"));

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("frame-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            blend_pipeline,
            layer_bind_group_layout,
            surface_pipeline,
            surface_blend_pipeline,
            present_sharp_pipeline,
            surface_format,
            bind_group_layout,
            sampler,
            // 48-09: lazily filled on the first GpuFrame composite per format.
            #[cfg(all(windows, feature = "hwdecode"))]
            nv12_pipelines: std::sync::Mutex::new(Vec::new()),
            // 57-04: same discipline for the blend-capable NV12 layer pipeline.
            #[cfg(all(windows, feature = "hwdecode"))]
            nv12_layer_pipelines: std::sync::Mutex::new(Vec::new()),
            mixed_slots: std::sync::Mutex::new(MixedSlotCache::default()),
            #[cfg(all(windows, feature = "hwdecode"))]
            device_lost,
            #[cfg(all(windows, feature = "hwdecode"))]
            device_lost_pending,
            // 63-01: the recovery recipe, derived from THIS call's arguments —
            // the same two that decide the guard and the detection above, so
            // "presentation-capable" cannot mean one thing for arming and
            // another for rebuilding.
            birth: match (compatible_surface.is_some(), debug_surface_format) {
                (true, _) => CompositorBirth::HostSurface,
                (false, Some(format)) => CompositorBirth::DebugSurfaceFormat(format),
                (false, None) => CompositorBirth::Offscreen,
            },
        })
    }

    /// How this compositor was born ([`CompositorBirth`]) — the recipe
    /// device-lost recovery rebuilds through.
    pub fn birth(&self) -> CompositorBirth {
        self.birth
    }

    /// Rebuild a compositor equivalent to this one through the SAME
    /// construction funnel (Phase 63, plan 63-01) — recovery step 4's
    /// engine-default "new wgpu device → swapchain/surface → compositor
    /// resources" half.
    ///
    /// Going back through `build` is the point: the recreated device gets
    /// [`install_uncaptured_error_guard`] and GPU-04 detection re-armed at its
    /// own birth, exactly as the original did, with no second arming site to
    /// keep in step.
    ///
    /// [`CompositorBirth::HostSurface`] returns a typed error rather than a
    /// silently-degraded compositor: the live `wgpu::Surface` belongs to the
    /// shell's `SwapChainPanel`, so only the host can rebuild it (supply
    /// `RecoveryHostHooks::recreate_device` — plan 63-02). Rebuilding it here
    /// as a surface-less twin would hand back a compositor that can never
    /// present, which is a WORSE failure than an honest error (CONTEXT D-03:
    /// "a partially-recreated surface is worse than a documented gap").
    ///
    /// [`CompositorBirth::Offscreen`] also refuses: an export/offscreen device
    /// arms no detection and has no preview to recover (GPU-07).
    pub fn rebuild_like_birth(&self) -> Result<Self, EngineError> {
        Self::rebuild_from_birth(self.birth)
    }

    /// [`Compositor::rebuild_like_birth`] from the recipe ALONE — the form the
    /// recovery coordinator needs, because step 3 has already dropped the
    /// original compositor by the time step 4 rebuilds (dropping it is a
    /// MEASURED requirement, not hygiene: while any handle to a removed D3D12
    /// device lives, DXGI hides the hardware adapter and the rebuild lands on
    /// WARP).
    pub fn rebuild_from_birth(birth: CompositorBirth) -> Result<Self, EngineError> {
        match birth {
            CompositorBirth::DebugSurfaceFormat(format) => {
                Self::new_with_debug_surface_format(format)
            }
            CompositorBirth::HostSurface => Err(EngineError::Gpu(
                "cannot rebuild a live-surface compositor from the engine: the wgpu::Surface \
                 and the instance that created it belong to the host's SwapChainPanel. Supply \
                 RecoveryHostHooks::recreate_device (plan 63-02's present-loop choreography)"
                    .to_string(),
            )),
            CompositorBirth::Offscreen => Err(EngineError::Gpu(
                "an offscreen/export compositor arms no device-lost detection and has no \
                 preview to recover (GPU-07) — nothing to rebuild"
                    .to_string(),
            )),
        }
    }

    /// The surface's negotiated format, if this compositor was built with one.
    /// Callers building the `SurfaceConfiguration` reuse this exact value.
    pub fn surface_format(&self) -> Option<wgpu::TextureFormat> {
        self.surface_format
    }

    /// (Re)configure a surface for `width`x`height` against this compositor's
    /// device and the surface's negotiated format. Call once after creating the
    /// surface and again on every window resize before `composite_to_surface`.
    /// No-op-safe: clamps to 1px minimum. Requires a `new_with_surface`
    /// compositor (returns an error otherwise).
    pub fn configure_surface(
        &self,
        surface: &wgpu::Surface,
        width: u32,
        height: u32,
    ) -> Result<(), EngineError> {
        let format = self.surface_format.ok_or_else(|| {
            EngineError::Gpu(
                "configure_surface called on a Compositor with no surface (use new_with_surface)"
                    .to_string(),
            )
        })?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&self.device, &config);
        Ok(())
    }

    /// Shared composite step used by BOTH the offscreen and on-screen paths
    /// ("one composite path"): validate + upload `frame.rgba` as a texture and
    /// encode a fullscreen-quad render pass drawing it into `target_view` with
    /// `pipeline`. Only the render TARGET (offscreen texture vs swapchain view)
    /// and the pipeline's target FORMAT differ between the two callers.
    ///
    /// The `src_texture`/`bind_group` created here are wgpu Arc handles; the
    /// recorded render pass keeps them alive inside `encoder` until submit, so
    /// dropping the locals at function end is safe.
    /// `viewport`, when `Some([x, y, w, h])` (physical px within the target),
    /// restricts the fullscreen quad to that sub-rect — the clear-to-black fills
    /// the rest (contain-fit letterbox for the on-screen path). `None` draws
    /// full-target (offscreen/export path — unchanged).
    fn encode_frame_pass(
        &self,
        frame: &Frame,
        target_view: &wgpu::TextureView,
        pipeline: &wgpu::RenderPipeline,
        encoder: &mut wgpu::CommandEncoder,
        viewport: Option<[f32; 4]>,
    ) -> Result<(), EngineError> {
        let (w, h) = (frame.width, frame.height);
        let expected = w as usize * h as usize * 4;
        if frame.rgba.len() != expected {
            return Err(EngineError::BadOutputSize {
                got: frame.rgba.len(),
                expected,
            });
        }

        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };

        // Upload the frame as an Rgba8Unorm texture (frame bytes are RGBA; the
        // shader samples float — format divergence is handled at the RENDER
        // TARGET via `pipeline`, not the sampled texture).
        let src_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame-src"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &src_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            size,
        );

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &src_texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Letterbox (on-screen path): the clear above already filled the
            // whole target black; restrict the quad to the contain-fit sub-rect
            // so the frame keeps its aspect and the rest stays black.
            if let Some([vx, vy, vw, vh]) = viewport {
                pass.set_viewport(vx, vy, vw, vh, 0.0, 1.0);
            }
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1); // fullscreen quad (triangle strip)
        }

        Ok(())
    }

    /// Composite a decoded frame through the GPU and read the result back as
    /// tightly-packed RGBA bytes (width * height * 4). Offscreen path —
    /// unchanged behavior; the export/CI/`preview_timeline_at` gate.
    ///
    /// ESCAPE HATCH (Phase 48 / ROADMAP reversibility): the CPU RGBA
    /// composite path is the recorded escape hatch for the GPU-resident
    /// preview path — KEEP ALIVE, NEVER DELETE. A vendor-driver-forced
    /// retreat from shared-handle interop re-routes preview through this
    /// function (see 48-CONTEXT.md § VRAM Budget and
    /// `crates/engine/src/import.rs`'s rung-1 note for the full retreat
    /// ladder).
    pub fn composite_to_rgba(&self, frame: &Frame) -> Result<Vec<u8>, EngineError> {
        let (w, h) = (frame.width, frame.height);
        let expected = w as usize * h as usize * 4;

        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };

        // Offscreen render target (COPY_SRC so it can be read back).
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // Readback buffer. copy_texture_to_buffer requires bytes_per_row
        // aligned to COPY_BYTES_PER_ROW_ALIGNMENT (256), so rows are padded in
        // the buffer and the padding is stripped after mapping.
        let unpadded_bytes_per_row = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT; // 256
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let readback_size = padded_bytes_per_row as u64 * h as u64;

        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: readback_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        // Shared upload + fullscreen-quad render pass into the offscreen target.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite"),
            });
        // Offscreen/export path draws full-target — no letterbox.
        self.encode_frame_pass(frame, &target_view, &self.pipeline, &mut encoder, None)?;

        // Offscreen-only: copy target -> readback buffer.
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            size,
        );
        self.queue.submit(Some(encoder.finish()));

        // Map + read back (blocking via poll(Wait), pollster-style sync flow).
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait)
            .map_err(|e| EngineError::Gpu(format!("device poll failed: {e}")))?;
        rx.recv()
            .map_err(|e| EngineError::Gpu(format!("map_async callback dropped: {e}")))?
            .map_err(|e| EngineError::Gpu(format!("buffer map failed: {e}")))?;

        // Strip row padding into a tightly-packed RGBA vec.
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity(expected);
        for row in 0..h as usize {
            let start = row * padded_bytes_per_row as usize;
            out.extend_from_slice(&data[start..start + unpadded_bytes_per_row as usize]);
        }
        drop(data);
        readback.unmap();

        Ok(out)
    }

    /// Shared multi-layer pass encoder used by ALL layer composite entry
    /// points (offscreen `composite_layers_to_rgba`, on-screen
    /// `composite_layers_to_surface`, and the headless parity readback) — ONE
    /// geometry/uniform/paint-order code path so preview and export are
    /// identical by construction (D-02). Per-layer geometry resolves through
    /// `layer_params_bytes` (transform dest rect + contain-fit + crop +
    /// opacity clamp); degenerate layers become no-op draws (T-18-01).
    ///
    /// STACKING CANONICALIZATION (Open Question 2, 18-RESEARCH.md §Q3): the
    /// `layers` slice is TRACK-ORDERED — index-0 = TOP-MOST, matching
    /// `top_video_active_at`'s "first video track wins" — so layers are
    /// painted BACK-TO-FRONT by iterating the slice in REVERSE: the last
    /// element draws first (bottom), index-0 is painted last (top).
    fn encode_layers_pass(
        &self,
        layers: &[Layer],
        target_view: &wgpu::TextureView,
        pipeline: &wgpu::RenderPipeline,
        encoder: &mut wgpu::CommandEncoder,
        out_w: u32,
        out_h: u32,
        clear_color: wgpu::Color,
    ) -> Result<(), EngineError> {
        // Validate every layer's byte size BEFORE creating any GPU resources.
        for layer in layers {
            let need = layer.frame.width as usize * layer.frame.height as usize * 4;
            if layer.frame.rgba.len() != need {
                return Err(EngineError::BadOutputSize {
                    got: layer.frame.rgba.len(),
                    expected: need,
                });
            }
        }

        // Per-layer GPU resources in PAINT order (slice reversed — index-0 =
        // top, painted last): source texture + frame bind group (group 0) and
        // the resolved transform/crop/opacity uniform (group 1). Created up
        // front; the render pass below only records draws. Layers whose
        // geometry resolves to `None` (degenerate/non-finite) are skipped —
        // a no-op draw, never a shader divide-by-zero (T-18-01).
        let mut layer_bind_groups = Vec::with_capacity(layers.len());
        for layer in layers.iter().rev() {
            let Some(params) = layer_params_bytes(layer, out_w as f32, out_h as f32) else {
                continue;
            };
            let lsize = wgpu::Extent3d {
                width: layer.frame.width,
                height: layer.frame.height,
                depth_or_array_layers: 1,
            };
            let src_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("layer-src"),
                size: lsize,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &src_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &layer.frame.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(layer.frame.width * 4),
                    rows_per_image: Some(layer.frame.height),
                },
                lsize,
            );
            let frame_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("layer-frame-bg"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &src_texture.create_view(&wgpu::TextureViewDescriptor::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });

            let uniform = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("layer-params"),
                size: 64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&uniform, 0, &params);
            let params_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("layer-params-bg"),
                layout: &self.layer_bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            layer_bind_groups.push((frame_bg, params_bg));
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite-layers-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Background cleared ONCE for the whole stack. The clear
                        // color is now CALLER-SUPPLIED: every EXISTING caller
                        // passes `wgpu::Color::BLACK` (locked opaque-black D-01,
                        // byte-exact — see tests/alpha_convention.rs); only the
                        // additive OVL-03 sibling `composite_layers_to_rgba_transparent`
                        // passes `wgpu::Color::TRANSPARENT` to preserve per-pixel
                        // alpha for self-contained overlay-asset export.
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline);
            // Back-to-front: bind groups were built slice-reversed, so this
            // draws bottom first; index-0 = top, painted last.
            for (frame_bg, params_bg) in &layer_bind_groups {
                pass.set_bind_group(0, frame_bg, &[]);
                pass.set_bind_group(1, params_bg, &[]);
                pass.draw(0..4, 0..1); // per-layer quad (triangle strip)
            }
        }

        Ok(())
    }

    /// Append a `texture` -> buffer copy to `encoder`, submit, block-map, and
    /// return tightly-packed RGBA bytes (row padding stripped; BGRA channel
    /// order swapped to RGBA when `swap_rb`). Shared by the NEW multi-layer
    /// readback paths; the pre-existing single-frame readbacks are
    /// deliberately untouched (Pitfall 2).
    fn submit_and_read_back(
        &self,
        mut encoder: wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        w: u32,
        h: u32,
        swap_rb: bool,
    ) -> Result<Vec<u8>, EngineError> {
        let unpadded_bytes_per_row = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT; // 256
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let readback_size = padded_bytes_per_row as u64 * h as u64;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("layers-readback"),
            size: readback_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        // Map + read back (same blocking pattern as composite_to_rgba).
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait)
            .map_err(|e| EngineError::Gpu(format!("device poll failed: {e}")))?;
        rx.recv()
            .map_err(|e| EngineError::Gpu(format!("map_async callback dropped: {e}")))?
            .map_err(|e| EngineError::Gpu(format!("buffer map failed: {e}")))?;

        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for row in 0..h as usize {
            let start = row * padded_bytes_per_row as usize;
            let src = &data[start..start + unpadded_bytes_per_row as usize];
            if swap_rb {
                for px in src.chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]); // BGRA -> RGBA
                }
            } else {
                out.extend_from_slice(src);
            }
        }
        drop(data);
        readback.unmap();

        Ok(out)
    }

    /// Composite N layers back-to-front into an `out_w`x`out_h` (PROJECT
    /// resolution — never any layer's own dims) offscreen target and read back
    /// tightly-packed RGBA bytes. THE Phase-18 multi-layer entry point — every
    /// downstream visual plan (transforms, keyframes, text, layouts)
    /// composites through here.
    ///
    /// Semantics (locked convention D-01 — see the blend-site comment in
    /// `build()` and `tests/alpha_convention.rs`):
    /// - the target is cleared ONCE to OPAQUE BLACK `(0,0,0,1)`;
    /// - layers paint BACK-TO-FRONT: the slice is track-ordered, index-0 =
    ///   top, painted last (see `encode_layers_pass`);
    /// - each layer's quad is positioned by its transform (dest rect) with the
    ///   source CONTAIN-FIT inside it, UVs remapped by crop, and its opacity
    ///   clamped to finite `[0.0, 1.0]` via max-then-min (NaN -> 0.0 — the
    ///   `SetClipVolume` discipline, T-18-04) — all in `layer_params_bytes`;
    /// - degenerate/non-finite layers are no-op draws (T-18-01).
    ///
    /// An empty `layers` slice yields the opaque-black canvas.
    pub fn composite_layers_to_rgba(
        &self,
        layers: &[Layer],
        out_w: u32,
        out_h: u32,
    ) -> Result<Vec<u8>, EngineError> {
        let (w, h) = (out_w.max(1), out_h.max(1));
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layers-offscreen-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-layers"),
            });
        self.encode_layers_pass(
            layers,
            &target_view,
            &self.blend_pipeline,
            &mut encoder,
            w,
            h,
            wgpu::Color::BLACK, // D-01 locked opaque-black — byte-exact
        )?;
        self.submit_and_read_back(encoder, &target, w, h, false)
    }

    /// Composite N layers back-to-front onto a fully TRANSPARENT background
    /// (`wgpu::Color::TRANSPARENT`, `a=0`) and read back tightly-packed RGBA
    /// bytes with the source's per-pixel ALPHA PRESERVED — the alpha-preserving
    /// sibling of [`composite_layers_to_rgba`] for OVL-03 self-contained
    /// overlay-asset export (`export_overlay_asset`).
    ///
    /// This is DELIBERATELY SEPARATE from the opaque-black entry point: it uses
    /// the SAME `blend_pipeline` / `fs_layer` shader / per-layer geometry, and
    /// the SAME premultiplied-over blend state whose ALPHA channel also blends
    /// `(One, OneMinusSrcAlpha)` — so accumulating layers over a transparent
    /// base yields correct OUTPUT alpha for free. The ONLY difference from
    /// `composite_layers_to_rgba` is the clear color. NEVER use this for
    /// preview or the frozen timeline export — those keep the locked opaque-black
    /// D-01 convention; a transparent clear there would break WYSIWYG.
    ///
    /// An empty `layers` slice yields a fully-transparent canvas (all `a=0`).
    pub fn composite_layers_to_rgba_transparent(
        &self,
        layers: &[Layer],
        out_w: u32,
        out_h: u32,
    ) -> Result<Vec<u8>, EngineError> {
        let (w, h) = (out_w.max(1), out_h.max(1));
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layers-offscreen-target-transparent"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-layers-transparent"),
            });
        self.encode_layers_pass(
            layers,
            &target_view,
            &self.blend_pipeline,
            &mut encoder,
            w,
            h,
            wgpu::Color::TRANSPARENT, // alpha-preserving clear (a=0) — OVL-03 only
        )?;
        self.submit_and_read_back(encoder, &target, w, h, false)
    }

    /// Composite N layers back-to-front and PRESENT directly into the
    /// surface's swapchain texture — the preview twin of
    /// `composite_layers_to_rgba` (same `encode_layers_pass`, same per-layer
    /// geometry/uniforms; only the render target + its format differ). NO CPU
    /// readback. The target canvas is the surface's own pixel size. Requires a
    /// Compositor built with a surface pipeline (`new_with_surface`).
    pub fn composite_layers_to_surface(
        &self,
        layers: &[Layer],
        surface: &wgpu::Surface,
    ) -> Result<(), EngineError> {
        let pipeline = self.surface_blend_pipeline.as_ref().ok_or_else(|| {
            EngineError::Gpu(
                "composite_layers_to_surface called on a Compositor with no surface \
                 blend pipeline (use new_with_surface)"
                    .to_string(),
            )
        })?;

        let surface_texture = surface
            .get_current_texture()
            .map_err(|e| EngineError::Gpu(format!("get_current_texture failed: {e}")))?;
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let (w, h) = (surface_texture.texture.width(), surface_texture.texture.height());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-layers-to-surface"),
            });
        self.encode_layers_pass(
            layers,
            &view,
            pipeline,
            &mut encoder,
            w,
            h,
            wgpu::Color::BLACK, // D-01 locked opaque-black — byte-exact
        )?;
        self.queue.submit(Some(encoder.finish()));
        surface_texture.present();

        Ok(())
    }

    /// TEST/DEBUG ONLY (multi-layer parity gate): render `layers` through the
    /// SAME surface-format blend pipeline `composite_layers_to_surface`
    /// presents with, but into an OFFSCREEN texture instead of a real
    /// swapchain, then read back as tightly-packed RGBA — directly comparable
    /// to `composite_layers_to_rgba` via a MAD frame-diff. The multi-layer
    /// extension of `composite_to_surface_debug_readback` (Phase 9 SC-3
    /// infra): the ONLY variable under test is the render-target format.
    pub fn composite_layers_to_surface_debug_readback(
        &self,
        layers: &[Layer],
        out_w: u32,
        out_h: u32,
    ) -> Result<Vec<u8>, EngineError> {
        let pipeline = self.surface_blend_pipeline.as_ref().ok_or_else(|| {
            EngineError::Gpu(
                "composite_layers_to_surface_debug_readback needs a surface blend pipeline \
                 (use new_with_surface or new_with_debug_surface_format)"
                    .to_string(),
            )
        })?;
        let format = self
            .surface_format
            .expect("surface_format Some when surface_blend_pipeline Some");
        // Same linear-format constraint as the single-frame debug readback.
        let swap_rb = match format {
            wgpu::TextureFormat::Bgra8Unorm => true,
            wgpu::TextureFormat::Rgba8Unorm => false,
            other => {
                return Err(EngineError::Gpu(format!(
                    "debug readback only supports linear Bgra8Unorm/Rgba8Unorm, got {other:?}"
                )))
            }
        };

        let (w, h) = (out_w.max(1), out_h.max(1));
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layers-surface-debug-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-layers-surface-debug"),
            });
        self.encode_layers_pass(
            layers,
            &target_view,
            pipeline,
            &mut encoder,
            w,
            h,
            wgpu::Color::BLACK, // D-01 locked opaque-black — byte-exact
        )?;
        self.submit_and_read_back(encoder, &target, w, h, swap_rb)
    }

    /// Composite a decoded frame and PRESENT it directly into the surface's
    /// swapchain texture — NO CPU readback (the perf property distinguishing
    /// this from `composite_to_rgba`). Requires a Compositor built via
    /// `new_with_surface` (its `surface_pipeline` targets the surface format).
    pub fn composite_to_surface(
        &self,
        frame: &Frame,
        surface: &wgpu::Surface,
    ) -> Result<(), EngineError> {
        let pipeline = self.surface_pipeline.as_ref().ok_or_else(|| {
            EngineError::Gpu(
                "composite_to_surface called on a Compositor with no surface_pipeline \
                 (use new_with_surface)"
                    .to_string(),
            )
        })?;

        let surface_texture = surface
            .get_current_texture()
            .map_err(|e| EngineError::Gpu(format!("get_current_texture failed: {e}")))?;
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Contain-fit letterbox: centre the frame in the surface preserving its
        // aspect (design-handoff Preview rule: "contain-fit, never crops"). The
        // rest of the surface stays black (the render pass clears to black).
        let sw = surface_texture.texture.width() as f32;
        let sh = surface_texture.texture.height() as f32;
        let fw = (frame.width.max(1)) as f32;
        let fh = (frame.height.max(1)) as f32;
        let viewport = contain_fit_viewport(sw, sh, fw, fh);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-to-surface"),
            });
        self.encode_frame_pass(frame, &view, pipeline, &mut encoder, Some(viewport))?;
        self.queue.submit(Some(encoder.finish()));
        surface_texture.present();

        Ok(())
    }

    /// TEST/DEBUG ONLY (SC-3 headless parity gate): render `frame` through the
    /// SAME `surface_pipeline` + surface FORMAT that `composite_to_surface`
    /// presents with, but into an OFFSCREEN texture (built with the surface
    /// format) instead of a real swapchain, then read it back and CONVERT to
    /// tightly-packed RGBA byte order so the result is directly comparable to
    /// `composite_to_rgba`'s `Rgba8Unorm` output via a MAD frame-diff.
    ///
    /// This is the concrete proof that the on-screen rendering pipeline (which
    /// differs from the offscreen path ONLY in its target format — Pitfall E)
    /// produces pixel-equivalent output; without it, the surface-format pipeline
    /// is only exercisable behind a real window (no `cargo test` coverage).
    ///
    /// Renders FULL-TARGET (no letterbox viewport) at the frame's native dims,
    /// exactly matching `composite_to_rgba`'s layout, so the ONLY variable under
    /// test is the render-target format. Requires a compositor built with a
    /// surface pipeline (`new_with_surface` or `new_with_debug_surface_format`).
    ///
    /// NOT on the production present hot path — the real present thread calls
    /// `composite_to_surface` against a real `wgpu::Surface`.
    pub fn composite_to_surface_debug_readback(
        &self,
        frame: &Frame,
    ) -> Result<Vec<u8>, EngineError> {
        let pipeline = self.surface_pipeline.as_ref().ok_or_else(|| {
            EngineError::Gpu(
                "composite_to_surface_debug_readback needs a surface pipeline \
                 (use new_with_surface or new_with_debug_surface_format)"
                    .to_string(),
            )
        })?;
        let format = self.surface_format.expect("surface_format Some when surface_pipeline Some");

        let (w, h) = (frame.width, frame.height);
        let expected = w as usize * h as usize * 4;
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };

        // Offscreen target in the SURFACE format (not OFFSCREEN_FORMAT) — this
        // is what makes the readback exercise the surface pipeline's format path.
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("surface-debug-target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let unpadded_bytes_per_row = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT; // 256
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let readback_size = padded_bytes_per_row as u64 * h as u64;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("surface-debug-readback"),
            size: readback_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-surface-debug"),
            });
        // Full-target (viewport None) via the SURFACE pipeline — matches
        // composite_to_rgba's layout so only the format differs.
        self.encode_frame_pass(frame, &target_view, pipeline, &mut encoder, None)?;
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            size,
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait)
            .map_err(|e| EngineError::Gpu(format!("device poll failed: {e}")))?;
        rx.recv()
            .map_err(|e| EngineError::Gpu(format!("map_async callback dropped: {e}")))?
            .map_err(|e| EngineError::Gpu(format!("buffer map failed: {e}")))?;

        // Strip row padding, then normalize the channel order to RGBA so the
        // bytes are directly comparable to composite_to_rgba's Rgba8Unorm output.
        // The surface format MUST be LINEAR (non-sRGB) — the compositor picks the
        // linear format via `!is_srgb()`, and the offscreen ground truth is
        // linear Rgba8Unorm; an sRGB target would gamma-encode on store and the
        // MAD diff would (correctly) fail. We handle the two linear 8-bit orders.
        let swap_rb = match format {
            wgpu::TextureFormat::Bgra8Unorm => true,
            wgpu::TextureFormat::Rgba8Unorm => false,
            other => {
                return Err(EngineError::Gpu(format!(
                    "debug readback only supports linear Bgra8Unorm/Rgba8Unorm, got {other:?}"
                )))
            }
        };
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity(expected);
        for row in 0..h as usize {
            let start = row * padded_bytes_per_row as usize;
            let src = &data[start..start + unpadded_bytes_per_row as usize];
            if swap_rb {
                for px in src.chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]); // BGRA -> RGBA
                }
            } else {
                out.extend_from_slice(src);
            }
        }
        drop(data);
        readback.unmap();

        Ok(out)
    }
}

// ===========================================================================
// Phase 48 (GPU-02) — the GPU-resident NV12 composite path.
//
// Plan 48-06's compositor changes are PURE INSERTIONS (zero existing lines
// deleted or modified — T-48-06-02's additive-only guarantee): the CPU/export
// composite path above is byte-preserved, and `composite_to_rgba` stays alive
// as the recorded escape hatch. The new path composites a hardware-decoded
// `GpuFrame` (NV12, GPU-resident — 48-05's zero-copy import) through a
// YUV→RGB shader whose matrix + range constants arrive PER FRAME in a
// CPU-built uniform (`colorspace::color_params`) — never hardcoded in WGSL.
//
// Dual-pipeline discipline mirrors the existing structure: a surface-format
// variant (`composite_gpu_to_surface`, the present route) and an offscreen
// RGBA variant (`composite_gpu_to_rgba`, the VERIFICATION/paused-path route)
// share ONE shader + ONE pass encoder, so plan 48-11 can prove
// surface == offscreen the same way the CPU path does.
// ===========================================================================

#[cfg(all(windows, feature = "hwdecode"))]
use crate::colorspace::color_params;
#[cfg(all(windows, feature = "hwdecode"))]
use crate::hwdecode::{HwDecodeSession, HwFrame};
#[cfg(all(windows, feature = "hwdecode"))]
use crate::import::{import_frame, GpuFrame};

/// WGSL for compositing a hardware-decoded NV12 `GpuFrame`.
///
/// NO matrix literal appears anywhere in this source (GPU-02's prohibition):
/// the conversion arrives per frame in `ColorUniform`, built on the CPU by
/// `colorspace::color_params` from the frame's REAL tags. The only other
/// input is the small `FrameGeom` uniform alongside it (the push-constant-free
/// route for per-draw values).
#[cfg(all(windows, feature = "hwdecode"))]
const NV12_SHADER: &str = r#"
// ColorUniform — layout mirrors engine::colorspace::ColorParams (80 bytes,
// compile-asserted on the Rust side). CPU-selected per frame; the shader is
// a straight matrix multiply.
struct ColorUniform {
    mat: mat3x3<f32>,
    range_offset: vec3<f32>,
    range_scale: vec3<f32>,
}

// The small per-draw uniform alongside ColorUniform:
//   dest: xy = dest-rect origin (target px), zw = dest-rect size (target px)
//   src:  xy = the frame's DISPLAY dims (px) — the bound texture is the WHOLE
//         (macroblock-aligned, multi-slice) hw frame pool, so display dims
//         must come from the CPU, never from textureDimensions().
struct FrameGeom {
    dest: vec4<f32>,
    src: vec4<f32>,
}

@group(0) @binding(0) var luma: texture_2d_array<f32>;
@group(0) @binding(1) var chroma: texture_2d_array<f32>;
@group(0) @binding(2) var<uniform> color: ColorUniform;
@group(0) @binding(3) var<uniform> geom: FrameGeom;

// Range-decode, then the matrix — exactly libswscale's own two-stage internal
// structure (fmt_decode_range -> the colorspace linear op; 48-RESEARCH.md
// Pattern 2, VERIFIED). R8Unorm/Rg8Unorm textureLoad already yields
// normalized [0,1] values, so there is NO extra /255 here.
fn yuv_to_rgb(y: f32, cb: f32, cr: f32) -> vec3<f32> {
    let yuv = (vec3<f32>(y, cb, cr) - color.range_offset) * color.range_scale;
    return color.mat * yuv;
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle: (-1,-1), (3,-1), (-1,3) — clipped/viewported to
    // the dest rect by the pass's set_viewport.
    let x = f32(vi & 1u) * 4.0 - 1.0;
    let y = f32(vi >> 1u) * 4.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    // Target-pixel position -> source texel via textureLoad (exact integer
    // fetch, no sampler — the SPIKE-02 verification route, which keeps the
    // parity ruler free of filtering ambiguity). In the offscreen/parity
    // configuration dest == (0, 0, src.xy), so this is an EXACT 1:1 fetch
    // (pos is the pixel centre x+0.5; floor returns x).
    let local = (pos.xy - geom.dest.xy) / geom.dest.zw;
    let texel = vec2<i32>(local * geom.src.xy);
    // Array index 0 is VIEW-RELATIVE: the D2Array plane views bake the
    // frame's REAL pool slice into base_array_layer (never slice 0 in
    // practice — 48-05 measured slices 19 and 23 of a 24-slice pool), so the
    // SRV itself carries the slice offset and index 0 addresses exactly that
    // slice. Binding shape is texture_2d_array<f32> BY CONSTRUCTION
    // (SPIKE-02 shape finding 2).
    let y = textureLoad(luma, texel, 0, 0).r;
    // Chroma is half-resolution; integer /2 is floor for non-negative
    // coords -> NEAREST (co-sited) chroma siting. libswscale's default 4:2:0
    // upsampling may differ at sharp chroma edges; the fixture-matrix test
    // MEASURES that divergence rather than pre-optimizing it (plan 48-06
    // Task 3's measurement protocol).
    let cbcr = textureLoad(chroma, texel / 2, 0, 0).rg;
    let rgb = clamp(yuv_to_rgb(y, cbcr.r, cbcr.g), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(rgb, 1.0);
}
"#;

#[cfg(all(windows, feature = "hwdecode"))]
impl Compositor {
    /// Import a hardware-decoded frame onto THIS compositor's device/queue
    /// (the zero-copy `import_frame` chain, 48-05). The frame's texture and
    /// plane views then live on the same device the composite methods below
    /// render with — the wiring 48-05 recorded for the real compositor device.
    ///
    /// Requires the device to have `TEXTURE_FORMAT_NV12` (requested in
    /// `build()` when the adapter supports it); `import_frame` fails with a
    /// clear typed error otherwise.
    pub fn import_gpu_frame(
        &self,
        session: &HwDecodeSession,
        frame: HwFrame,
    ) -> Result<GpuFrame, EngineError> {
        import_frame(&self.device, &self.queue, session, frame)
    }

    /// The compositor's own `wgpu::Device` (plan 48-10 — the accessor 48-05
    /// anticipated): needed to register `set_device_lost_callback` on the LIVE
    /// device and to drive the `simulate_device_lost` injection seam. The
    /// device handle is an `Arc`-backed clone target; exposing it adds no
    /// capability `import_frame`'s public signature did not already imply.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The compositor's own `wgpu::Queue` (plan 48-10): the injection seam
    /// must submit its pre-created traffic encoder on the queue the device
    /// actually executes, so wgpu observes the removal and fires the lost
    /// callback (the 48-03 probe's measured chain).
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// This compositor's device-lost funnel (GPU-04), or `None` when detection
    /// was not armed for this device.
    ///
    /// `Some` exactly when `build` took the PRESENTATION-CAPABLE branch — the
    /// same boundary [`install_uncaptured_error_guard`] uses, for the same
    /// reason: only a presenting device drives the preview path whose loss the
    /// degrade responds to. Export/offscreen compositors ([`Compositor::new`])
    /// return `None` and keep wgpu's default device-lost handling (GPU-07).
    ///
    /// Read by `crates/engine/tests/device_lost_wiring.rs` to prove the WIRING
    /// (the arming line in `build`), not merely the mechanism — the distinction
    /// v8.0-MILESTONE-AUDIT § 0 names as this milestone's characteristic defect.
    #[cfg(all(windows, feature = "hwdecode"))]
    pub fn device_lost_signal(&self) -> Option<&crate::device_lost::DeviceLostSignal> {
        self.device_lost.as_ref()
    }

    /// **Has this device's loss been detected and coordinated recovery been
    /// REQUESTED?** (Phase 63, plan 63-01 — TRUST-01.)
    ///
    /// The poll half of the request/run split described on the
    /// `device_lost_pending` field: the device-birth response sets this from
    /// wgpu's own callback thread, and whoever owns the present loop reads it
    /// and drives [`crate::device_lost::PreviewRecovery::recover_after_device_lost`]
    /// on a thread that may legally tear the device down.
    ///
    /// A single relaxed-cost atomic load, safe to call once per presented
    /// frame. Always `false` for export/offscreen compositors, which arm no
    /// detection at all (GPU-07).
    #[cfg(all(windows, feature = "hwdecode"))]
    pub fn device_lost_pending(&self) -> bool {
        self.device_lost_pending
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// The shared recovery-request flag itself, for a driver that wants to
    /// watch it without holding the compositor (plan 63-02's present loop).
    /// `None` exactly when [`Compositor::device_lost_signal`] is `None`.
    #[cfg(all(windows, feature = "hwdecode"))]
    pub fn device_lost_pending_flag(
        &self,
    ) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.device_lost_pending.as_ref().map(std::sync::Arc::clone)
    }

    /// The cached NV12 pipeline + bind group layout for `format` (plan 48-09
    /// — the hot-path caching 48-06 recorded as this plan's wiring work).
    /// Builds via [`Compositor::build_nv12_pipeline`] exactly once per target
    /// format, then answers from `nv12_pipelines` for every later composite:
    /// the live GPU present path runs at media cadence, and a per-present
    /// shader compile would dominate its frame budget.
    fn nv12_pipeline(
        &self,
        format: wgpu::TextureFormat,
        label: &str,
    ) -> std::sync::Arc<(wgpu::RenderPipeline, wgpu::BindGroupLayout)> {
        let mut cache = self
            .nv12_pipelines
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some((_, entry)) = cache.iter().find(|(f, _)| *f == format) {
            return entry.clone();
        }
        let built = std::sync::Arc::new(self.build_nv12_pipeline(format, label));
        cache.push((format, built.clone()));
        built
    }

    /// Build the NV12 pipeline + bind group layout for `format` — the ONE
    /// shader (`NV12_SHADER`) built against either the offscreen or the
    /// surface target format, mirroring `make_pipeline`'s
    /// only-the-format-differs discipline.
    ///
    /// Called ONLY through [`Compositor::nv12_pipeline`]'s per-format cache
    /// (plan 48-09): the build cost is paid once per format, not per present.
    fn build_nv12_pipeline(
        &self,
        format: wgpu::TextureFormat,
        label: &str,
    ) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("nv12-composite"),
                source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
            });
        // Plane views are D2Array BY CONSTRUCTION (SPIKE-02 shape finding 2:
        // wgpu-hal's DX12 backend emits a TEXTURE2DARRAY SRV whenever
        // base_array_layer != 0, so the declared binding shape must match).
        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        };
        let uniform_entry = |binding: u32, size: u64| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(size),
            },
            count: None,
        };
        let bgl = self
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("nv12-frame-bgl"),
                entries: &[
                    texture_entry(0),                 // luma  (Plane0, R8Unorm)
                    texture_entry(1),                 // chroma (Plane1, Rg8Unorm)
                    uniform_entry(2, 80),             // ColorUniform (ColorParams)
                    uniform_entry(3, 32),             // FrameGeom
                ],
            });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("nv12-composite-layout"),
                bind_group_layouts: &[&bgl],
                push_constant_ranges: &[],
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });
        (pipeline, bgl)
    }

    /// Shared pass encoder for BOTH GpuFrame composite entry points (the
    /// same one-composite-path discipline as `encode_frame_pass` /
    /// `encode_layers_pass`): write the per-frame `ColorUniform` from the
    /// frame's REAL tags (GPU-02 / CONTEXT D-05 — the CPU side), write the
    /// `FrameGeom` uniform, and record a fullscreen-triangle pass restricted
    /// to `dest` (`[x, y, w, h]` target px; the clear-to-black fills the rest
    /// — the letterbox for the surface path, full-target offscreen).
    fn encode_gpu_frame_pass(
        &self,
        frame: &GpuFrame,
        target_view: &wgpu::TextureView,
        pipeline: &wgpu::RenderPipeline,
        bgl: &wgpu::BindGroupLayout,
        encoder: &mut wgpu::CommandEncoder,
        dest: [f32; 4],
    ) {
        // The CPU side of GPU-02: per-frame params from the REAL tags. The
        // ONLY route by which conversion constants reach the shader.
        let params = color_params(frame.colorspace, frame.color_range);
        let color_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12-color-uniform"),
            size: std::mem::size_of::<crate::colorspace::ColorParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&color_buf, 0, bytemuck::bytes_of(&params));

        let geom: [f32; 8] = [
            dest[0],
            dest[1],
            dest[2],
            dest[3],
            frame.width.max(1) as f32,
            frame.height.max(1) as f32,
            0.0,
            0.0,
        ];
        let geom_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12-geom-uniform"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&geom_buf, 0, bytemuck::cast_slice(&geom));

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12-frame-bg"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&frame.luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&frame.chroma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: color_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: geom_buf.as_entire_binding(),
                },
            ],
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12-composite-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Restrict the fullscreen triangle to the dest rect; the clear
            // above already filled the rest black (letterbox bars).
            pass.set_viewport(dest[0], dest[1], dest[2], dest[3], 0.0, 1.0);
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1); // fullscreen triangle
        }
    }

    /// Composite a hardware-decoded `GpuFrame` and PRESENT it into the
    /// surface's swapchain — the GpuFrame twin of `composite_to_surface`
    /// (same contain-fit letterbox, surface-format pipeline, no CPU
    /// readback). Requires a Compositor built with a surface format
    /// (`new_with_surface`, or the debug-format constructor for headless
    /// parity work).
    pub fn composite_gpu_to_surface(
        &self,
        frame: &GpuFrame,
        surface: &wgpu::Surface,
    ) -> Result<(), EngineError> {
        let format = self.surface_format.ok_or_else(|| {
            EngineError::Gpu(
                "composite_gpu_to_surface called on a Compositor with no surface format \
                 (use new_with_surface)"
                    .to_string(),
            )
        })?;
        let cached = self.nv12_pipeline(format, "nv12-surface-pipeline");
        let (pipeline, bgl) = (&cached.0, &cached.1);

        let surface_texture = surface
            .get_current_texture()
            .map_err(|e| EngineError::Gpu(format!("get_current_texture failed: {e}")))?;
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Contain-fit letterbox at DISPLAY dims (the design-handoff Preview
        // rule) — the same `contain_fit_viewport` formula as the CPU path.
        let sw = surface_texture.texture.width() as f32;
        let sh = surface_texture.texture.height() as f32;
        let fw = frame.width.max(1) as f32;
        let fh = frame.height.max(1) as f32;
        let viewport = contain_fit_viewport(sw, sh, fw, fh);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-gpu-to-surface"),
            });
        self.encode_gpu_frame_pass(frame, &view, pipeline, bgl, &mut encoder, viewport);
        self.queue.submit(Some(encoder.finish()));
        surface_texture.present();

        Ok(())
    }

    /// Composite a hardware-decoded `GpuFrame` into an offscreen RGBA target
    /// and read back tightly-packed RGBA bytes (display width × height × 4)
    /// — the GpuFrame twin of `composite_to_rgba`, and the
    /// VERIFICATION/paused-path route (never the present hot path).
    ///
    /// The readback RENDERS the plane views to an RGBA target and copies
    /// THAT: NEVER `copy_texture_to_buffer` with a plane aspect — at
    /// wgpu 26.0.1 on DX12 that does not fail, it ABORTS the process
    /// (`calc_subresource_for_copy` hits `unreachable!()` for plane aspects;
    /// source-verified at Phase 44).
    pub fn composite_gpu_to_rgba(&self, frame: &GpuFrame) -> Result<Vec<u8>, EngineError> {
        let (w, h) = (frame.width.max(1), frame.height.max(1));
        let cached = self.nv12_pipeline(OFFSCREEN_FORMAT, "nv12-offscreen-pipeline");
        let (pipeline, bgl) = (&cached.0, &cached.1);

        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-offscreen-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-gpu-to-rgba"),
            });
        // Full-target dest: the EXACT 1:1 texel mapping the parity ruler
        // needs (see the shader's fs_main comment).
        self.encode_gpu_frame_pass(
            frame,
            &target_view,
            pipeline,
            bgl,
            &mut encoder,
            [0.0, 0.0, w as f32, h as f32],
        );
        self.submit_and_read_back(encoder, &target, w, h, false)
    }

    /// TEST/DEBUG ONLY (plan 48-11 — GPU-07's pipeline-parity ruler): render
    /// a hardware-decoded `GpuFrame` through the SURFACE-FORMAT NV12 pipeline
    /// into an offscreen target of that format and read it back as
    /// tightly-packed RGBA — the `GpuFrame` twin of
    /// `composite_to_surface_debug_readback`, for the same reason it exists:
    /// a `cargo test` process has no window, so the surface-format pipeline
    /// is otherwise only exercisable behind a real swapchain.
    ///
    /// Uses the SAME per-format cached pipeline object
    /// `composite_gpu_to_surface` presents with (`nv12_pipeline(format,
    /// "nv12-surface-pipeline")`), and renders FULL-TARGET at display dims —
    /// exactly `composite_gpu_to_rgba`'s layout — so the ONLY variable
    /// between the two readbacks is the render-target format (the sc3
    /// discipline from `surface_present.rs`). NOT a production entry point;
    /// the present hot path stays `composite_gpu_to_surface` and performs no
    /// readback (GPU-06).
    pub fn composite_gpu_to_surface_debug_readback(
        &self,
        frame: &GpuFrame,
    ) -> Result<Vec<u8>, EngineError> {
        let format = self.surface_format.ok_or_else(|| {
            EngineError::Gpu(
                "composite_gpu_to_surface_debug_readback needs a surface format \
                 (use new_with_surface or new_with_debug_surface_format)"
                    .to_string(),
            )
        })?;
        // Same linear-format discipline as the CPU debug readback: normalize
        // the channel order to RGBA so the bytes are directly MAD-comparable
        // to composite_gpu_to_rgba's Rgba8Unorm output.
        let swap_rb = match format {
            wgpu::TextureFormat::Bgra8Unorm => true,
            wgpu::TextureFormat::Rgba8Unorm => false,
            other => {
                return Err(EngineError::Gpu(format!(
                    "debug readback only supports linear Bgra8Unorm/Rgba8Unorm, got {other:?}"
                )))
            }
        };
        let cached = self.nv12_pipeline(format, "nv12-surface-pipeline");
        let (pipeline, bgl) = (&cached.0, &cached.1);

        let (w, h) = (frame.width.max(1), frame.height.max(1));
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-surface-debug-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-gpu-surface-debug"),
            });
        // Full-target dest — matches composite_gpu_to_rgba's exact 1:1 texel
        // mapping so only the target format differs between the two halves.
        self.encode_gpu_frame_pass(
            frame,
            &target_view,
            pipeline,
            bgl,
            &mut encoder,
            [0.0, 0.0, w as f32, h as f32],
        );
        self.submit_and_read_back(encoder, &target, w, h, swap_rb)
    }
}

// ===========================================================================
// Phase 57 (PLAY-02 / PLAY-06 / PLAY-07) — the GPU-RESIDENT MIXED-LAYER
// composite path.
//
// ADDITIVE ONLY (D-06, "extend the compositor, never rewrite it"): `pipeline`,
// `blend_pipeline`, `surface_pipeline`, `surface_blend_pipeline` and the NV12
// single-frame pipelines above are byte-untouched, and every existing entry
// point still runs exactly the code it ran before. What is NEW is one entry
// point that blends a MIXED list of already-GPU-resident (NV12) and CPU (RGBA)
// layers into a PERSISTENT pooled target with:
//   * NO blocking readback anywhere in its body (D-07), and
//   * NO per-frame texture / buffer creation in its steady state (D-08) —
// both machine-checked by `crates/engine/tests/no_readback_pin.rs`, which
// scans this file's source text rather than trusting review.
//
// The capability gap this closes, in 57-RESEARCH.md's own words: "there is no
// shader today that both (a) samples NV12 planes and (b) does the
// transform/crop/opacity/premultiplied-blend math fs_layer does."
// ===========================================================================

/// How many persistent composite targets the pool holds.
///
/// K = 4 rationale: the producer renders into ONE target while up to the ring's
/// lookahead depth of already-composited frames are still in flight toward the
/// presenter. Four covers the ring's in-flight window with a spare, and — being
/// FIXED — makes `checkout` blocking the natural backpressure signal instead of
/// letting the target set grow without bound (threat T-57-08). It is not a
/// tuning knob: raising it trades VRAM for slack the measurements have never
/// asked for.
pub const COMPOSITE_TARGET_POOL_DEPTH: usize = 4;

/// Upper bound on layers in ONE `composite_mixed_layers_to_target` call. Sized
/// well above any realistic visible stack (the multi-layer resolver caps far
/// lower) so the per-slot resource cache is a small fixed array rather than an
/// unbounded map, and so a runaway layer list fails loudly instead of
/// allocating.
pub const MAX_MIXED_LAYERS: usize = 16;

/// One input layer for [`Compositor::composite_mixed_layers_to_target`] — the
/// heterogeneous twin of [`Layer`].
///
/// Both arms BORROW, deliberately: a mixed list is rebuilt every tick from
/// frames the coordinator already owns (a software pool's `advance()` output, a
/// hardware session's HOLD slot), and moving decoded frames into the composite
/// call per tick would be exactly the kind of per-frame copying this path
/// exists to delete.
///
/// **Track order is the caller's contract, and it is the SAME contract
/// `encode_layers_pass` already documents**: index 0 = TOP-MOST, painted LAST.
/// Build the list by walking the authoritative track-ordered stack and asking,
/// per entry, "hardware slot or software frame" — never by concatenating a
/// hardware list and a software list, which is how a mixed set silently loses
/// z-order (57-RESEARCH.md Pitfall 5).
pub enum MixedLayer<'a> {
    /// A CPU-decoded RGBA layer — identical semantics to [`Layer`], rendered
    /// through the EXISTING `fs_layer` blend pipeline.
    Cpu(&'a Layer),
    /// An already-GPU-resident, zero-copy-imported NV12 frame, rendered
    /// through the new `fs_layer_nv12` pipeline with NO upload and NO copy.
    ///
    /// There is no `alpha_mode`: NV12 carries no alpha plane (Pitfall 6), so
    /// the texel is opaque by construction and the flag could not change a
    /// byte. Colorspace/range come from the frame's own REAL tags.
    #[cfg(all(windows, feature = "hwdecode"))]
    Gpu {
        frame: &'a GpuFrame,
        opacity: f32,
        transform: LayerTransform,
        crop: LayerCrop,
    },
}

/// PERSISTENT per-slot GPU resources for [`Compositor::composite_mixed_layers_to_target`].
struct MixedLayerSlot {
    /// 64 B — the `layer_params` uniform (group 1). Written with
    /// `queue.write_buffer` every frame; created ONCE.
    params_buf: wgpu::Buffer,
    /// 80 B — the per-frame `ColorUniform` for the GPU arm. Created ONCE.
    /// (Allocated unconditionally so a slot's resource set does not depend on
    /// which arm happens to land in it first; unused on a non-hwdecode build,
    /// where the GPU arm does not exist.)
    #[cfg_attr(not(all(windows, feature = "hwdecode")), allow(dead_code))]
    color_buf: wgpu::Buffer,
    /// 16 B — the per-frame plane geometry for the GPU arm. Created ONCE.
    #[cfg_attr(not(all(windows, feature = "hwdecode")), allow(dead_code))]
    plane_buf: wgpu::Buffer,
    /// The CPU arm's upload texture + its dims. Allocated on first CPU use in
    /// this slot and reallocated ONLY when this slot's source dims change.
    cpu_texture: Option<(wgpu::Texture, wgpu::TextureView, u32, u32)>,
}

/// The lazily-grown slot table plus the allocation odometer PLAY-07 is scored
/// on. `allocations` counts every `create_texture`/`create_buffer` this cache
/// has ever performed; in steady state it must stop moving.
#[derive(Default)]
struct MixedSlotCache {
    slots: Vec<MixedLayerSlot>,
    allocations: u64,
}

/// Shared state behind a [`CompositeTargetPool`], so a [`PooledTarget`] can
/// return its slot on `Drop` without borrowing the pool.
struct TargetPoolInner {
    /// Indices of currently-free slots. Fixed membership, never grows.
    free: std::sync::Mutex<Vec<usize>>,
    /// Signalled whenever a slot returns — what makes `checkout` block rather
    /// than allocate.
    returned: std::sync::Condvar,
}

impl TargetPoolInner {
    fn release(&self, index: usize) {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        if !free.contains(&index) {
            free.push(index);
        }
        drop(free);
        self.returned.notify_one();
    }
}

/// A FIXED set of [`COMPOSITE_TARGET_POOL_DEPTH`] persistent offscreen
/// composite targets — the D-08 answer to `composite_layers_to_rgba`'s
/// create-a-target-per-call shape.
///
/// [`CompositeTargetPool::ensure_size`] is the ONLY allocation site, and it is
/// rare by construction: dims change on a project-resolution edit or a
/// dynamic-playback-resolution level change (plan 57-08), never per frame.
/// `checkout` hands out an existing target or BLOCKS — it never allocates, so
/// a producer outrunning the presenter is throttled instead of growing VRAM
/// without bound (threat T-57-08).
pub struct CompositeTargetPool {
    device: wgpu::Device,
    slots: Vec<(std::sync::Arc<wgpu::Texture>, std::sync::Arc<wgpu::TextureView>)>,
    inner: std::sync::Arc<TargetPoolInner>,
    width: u32,
    height: u32,
    allocations: u64,
}

impl CompositeTargetPool {
    /// Allocate the pool's `COMPOSITE_TARGET_POOL_DEPTH` targets at
    /// `width`x`height`. `device` MUST be the device of the [`Compositor`]
    /// that will render into these targets (use
    /// [`Compositor::create_composite_target_pool`]).
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let mut pool = Self {
            device: device.clone(),
            slots: Vec::new(),
            inner: std::sync::Arc::new(TargetPoolInner {
                free: std::sync::Mutex::new(Vec::new()),
                returned: std::sync::Condvar::new(),
            }),
            width: 0,
            height: 0,
            allocations: 0,
        };
        pool.reallocate(width.max(1), height.max(1));
        pool
    }

    /// THE ONE allocation site. Rebuilds all `COMPOSITE_TARGET_POOL_DEPTH`
    /// targets on a **fresh** [`TargetPoolInner`].
    ///
    /// # Why a fresh inner, and not a reset free list (Phase 57, plan 57-08)
    ///
    /// Targets already checked out — up to K of them, sitting in the preview
    /// ring as composited frames — keep an `Arc` to the inner they were issued
    /// from, and hand their slot back to THAT inner on `Drop`. Resetting one
    /// shared free list to `0..K` instead would let a returning OLD target push
    /// an index that the same list had already re-issued against the NEW
    /// textures, so two live `PooledTarget`s would share one texture and the
    /// producer would composite into a texture a queued entry is about to
    /// present. Pinned by
    /// `reallocating_under_live_targets_never_reissues_a_live_texture`.
    ///
    /// The old inner dies with the last old target. The old TEXTURES are
    /// `Arc`-backed too, so a dims change transiently holds two generations of
    /// targets — bounded at 2K, and shrinking rather than growing whenever the
    /// change is a dynamic-playback-resolution DROP.
    fn reallocate(&mut self, width: u32, height: u32) {
        self.inner = std::sync::Arc::new(TargetPoolInner {
            free: std::sync::Mutex::new(Vec::new()),
            returned: std::sync::Condvar::new(),
        });
        let mut slots = Vec::with_capacity(COMPOSITE_TARGET_POOL_DEPTH);
        for i in 0..COMPOSITE_TARGET_POOL_DEPTH {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("mixed-composite-target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OFFSCREEN_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let _ = i;
            slots.push((std::sync::Arc::new(texture), std::sync::Arc::new(view)));
            self.allocations += 1;
        }
        self.slots = slots;
        self.width = width;
        self.height = height;
        let mut free = self.inner.free.lock().unwrap_or_else(|p| p.into_inner());
        *free = (0..COMPOSITE_TARGET_POOL_DEPTH).collect();
        drop(free);
        self.inner.returned.notify_all();
    }

    /// Reallocate every target IF AND ONLY IF the dims changed. A no-op (and
    /// allocation-free) at the current size, however often it is called — the
    /// property the pool's whole reason for existing depends on.
    pub fn ensure_size(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if (w, h) == (self.width, self.height) {
            return;
        }
        self.reallocate(w, h);
    }

    /// Check a target out, BLOCKING until one is free. Never allocates.
    pub fn checkout(&self) -> PooledTarget {
        let mut free = self.inner.free.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(index) = free.pop() {
                let (texture, view) = &self.slots[index];
                return PooledTarget {
                    inner: self.inner.clone(),
                    index,
                    texture: texture.clone(),
                    view: view.clone(),
                    width: self.width,
                    height: self.height,
                };
            }
            free = self
                .inner
                .returned
                .wait(free)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Non-blocking [`CompositeTargetPool::checkout`] — `None` when all
    /// `COMPOSITE_TARGET_POOL_DEPTH` targets are in flight.
    pub fn try_checkout(&self) -> Option<PooledTarget> {
        let mut free = self.inner.free.lock().unwrap_or_else(|p| p.into_inner());
        let index = free.pop()?;
        let (texture, view) = &self.slots[index];
        Some(PooledTarget {
            inner: self.inner.clone(),
            index,
            texture: texture.clone(),
            view: view.clone(),
            width: self.width,
            height: self.height,
        })
    }

    /// Current target dims.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// How many textures this pool has EVER created. The PLAY-07 odometer: it
    /// must equal `COMPOSITE_TARGET_POOL_DEPTH` after construction and must not
    /// move again until a real dims change.
    pub fn allocations(&self) -> u64 {
        self.allocations
    }
}

/// A checked-out composite target. Returns itself to its pool on `Drop`.
pub struct PooledTarget {
    inner: std::sync::Arc<TargetPoolInner>,
    index: usize,
    texture: std::sync::Arc<wgpu::Texture>,
    view: std::sync::Arc<wgpu::TextureView>,
    width: u32,
    height: u32,
}

impl PooledTarget {
    /// The persistent target texture (`OFFSCREEN_FORMAT`,
    /// `RENDER_ATTACHMENT | TEXTURE_BINDING`).
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
    /// The target's view — what the presenter blits from.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }
    /// Which pool slot this is (stable for the target's lifetime).
    pub fn index(&self) -> usize {
        self.index
    }
    /// Target dims at checkout time.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for PooledTarget {
    fn drop(&mut self) {
        self.inner.release(self.index);
    }
}

impl Compositor {
    /// Build a [`CompositeTargetPool`] on THIS compositor's device — the only
    /// correct way to obtain one, since the targets must live on the device
    /// that renders into them.
    pub fn create_composite_target_pool(&self, width: u32, height: u32) -> CompositeTargetPool {
        CompositeTargetPool::new(&self.device, width, height)
    }

    /// PLAY-07 odometer for the per-slot CPU-layer resources: total textures +
    /// buffers this compositor's mixed-layer slot cache has ever created. In
    /// steady state (same layer count, same source dims) it must stop moving.
    pub fn mixed_slot_allocations(&self) -> u64 {
        self.mixed_slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .allocations
    }

    /// Grow the slot table to cover `count` layers and make sure slot `i`'s CPU
    /// upload texture matches `(w, h)`. THE ONLY allocation site for mixed-layer
    /// per-slot resources — deliberately OUTSIDE
    /// `composite_mixed_layers_to_target`'s body, and guarded so it does nothing
    /// once a slot is warm.
    fn ensure_mixed_slots(&self, cache: &mut MixedSlotCache, count: usize) {
        while cache.slots.len() < count {
            let mk = |label: &str, size: u64| {
                self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            };
            cache.slots.push(MixedLayerSlot {
                params_buf: mk("mixed-layer-params", 64),
                color_buf: mk("mixed-layer-color", 80),
                plane_buf: mk("mixed-layer-plane", 16),
                cpu_texture: None,
            });
            cache.allocations += 3;
        }
    }

    /// Ensure slot `i` owns an RGBA upload texture of exactly `(w, h)`.
    /// Reallocates ONLY on a dims change for that slot.
    fn ensure_mixed_cpu_texture(&self, cache: &mut MixedSlotCache, i: usize, w: u32, h: u32) {
        let matches = matches!(cache.slots[i].cpu_texture, Some((_, _, tw, th)) if (tw, th) == (w, h));
        if matches {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mixed-layer-cpu-src"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        cache.slots[i].cpu_texture = Some((texture, view, w, h));
        cache.allocations += 1;
    }

    /// Composite a MIXED list of GPU-resident (NV12) and CPU (RGBA) layers,
    /// back-to-front, into an already-allocated pooled `target`.
    ///
    /// This is the phase's one new compositor capability. Semantics are the
    /// LOCKED ones `composite_layers_to_rgba` already documents, unchanged:
    /// the target is cleared ONCE to opaque black; `layers` is TRACK-ORDERED
    /// with index 0 = top, so the slice is iterated in REVERSE and index 0 is
    /// painted LAST; every layer's geometry resolves through the ONE
    /// host-side spot (`layer_params_bytes_from`); degenerate layers are no-op
    /// draws (T-18-01); the blend is the SAME premultiplied-over
    /// [`PREMULTIPLIED_OVER`] state on both arms.
    ///
    /// What is deliberately ABSENT from this body — enforced by
    /// `tests/no_readback_pin.rs`, not by review:
    /// no readback, no `device.poll`, and no texture/buffer creation. GPU
    /// layers bind their plane views DIRECTLY (no upload, no copy at all); CPU
    /// layers upload into the slot cache's PERSISTENT texture.
    ///
    /// An empty `layers` slice yields the opaque-black canvas.
    pub fn composite_mixed_layers_to_target(
        &self,
        layers: &[MixedLayer],
        target: &PooledTarget,
        out_w: u32,
        out_h: u32,
    ) -> Result<(), EngineError> {
        if layers.len() > MAX_MIXED_LAYERS {
            return Err(EngineError::Gpu(format!(
                "composite_mixed_layers_to_target: {} layers exceeds MAX_MIXED_LAYERS ({})",
                layers.len(),
                MAX_MIXED_LAYERS
            )));
        }
        let (w, h) = (out_w.max(1), out_h.max(1));
        // Validate every CPU layer's byte size BEFORE touching any GPU
        // resource — the same up-front check `encode_layers_pass` performs.
        for layer in layers {
            match layer {
                MixedLayer::Cpu(l) => {
                    let need = l.frame.width as usize * l.frame.height as usize * 4;
                    if l.frame.rgba.len() != need {
                        return Err(EngineError::BadOutputSize {
                            got: l.frame.rgba.len(),
                            expected: need,
                        });
                    }
                }
                // A GPU layer carries no CPU bytes to validate: its planes are
                // the decoder's own pool slice.
                #[cfg(all(windows, feature = "hwdecode"))]
                MixedLayer::Gpu { .. } => {}
            }
        }

        let mut cache = self.mixed_slots.lock().unwrap_or_else(|p| p.into_inner());
        self.ensure_mixed_slots(&mut cache, layers.len());

        // Build the per-layer bind groups in PAINT order (slice reversed —
        // index 0 = top, painted last), exactly as `encode_layers_pass` does.
        // `slot` is the layer's index in the ORIGINAL list so a given layer
        // keeps the same cached resources across frames.
        enum Bound {
            Cpu(wgpu::BindGroup, wgpu::BindGroup),
            #[cfg(all(windows, feature = "hwdecode"))]
            Gpu(wgpu::BindGroup, wgpu::BindGroup),
        }
        let mut bound: Vec<Bound> = Vec::with_capacity(layers.len());
        for (slot, layer) in layers.iter().enumerate().rev() {
            match layer {
                MixedLayer::Cpu(l) => {
                    let Some(params) = layer_params_bytes(l, w as f32, h as f32) else {
                        continue;
                    };
                    let (lw, lh) = (l.frame.width.max(1), l.frame.height.max(1));
                    self.ensure_mixed_cpu_texture(&mut cache, slot, lw, lh);
                    let entry = &cache.slots[slot];
                    let (texture, view, _, _) = entry
                        .cpu_texture
                        .as_ref()
                        .expect("cpu texture ensured above");
                    self.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &l.frame.rgba,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(lw * 4),
                            rows_per_image: Some(lh),
                        },
                        wgpu::Extent3d {
                            width: lw,
                            height: lh,
                            depth_or_array_layers: 1,
                        },
                    );
                    self.queue.write_buffer(&entry.params_buf, 0, &params);
                    let frame_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("mixed-cpu-frame-bg"),
                        layout: &self.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                        ],
                    });
                    let params_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("mixed-cpu-params-bg"),
                        layout: &self.layer_bind_group_layout,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: entry.params_buf.as_entire_binding(),
                        }],
                    });
                    bound.push(Bound::Cpu(frame_bg, params_bg));
                }
                #[cfg(all(windows, feature = "hwdecode"))]
                MixedLayer::Gpu {
                    frame,
                    opacity,
                    transform,
                    crop,
                } => {
                    // Same ONE host-side geometry spot as the CPU arm.
                    // `AlphaMode::Straight` packs misc.y = 0.0; `fs_layer_nv12`
                    // ignores it (Pitfall 6 — no alpha plane to interpret).
                    let Some(params) = layer_params_bytes_from(
                        frame.width,
                        frame.height,
                        *opacity,
                        *transform,
                        *crop,
                        AlphaMode::Straight,
                        w as f32,
                        h as f32,
                    ) else {
                        continue;
                    };
                    let cached = self.nv12_layer_pipeline(OFFSCREEN_FORMAT);
                    let entry = &cache.slots[slot];
                    self.queue.write_buffer(&entry.params_buf, 0, &params);
                    // The CPU side of GPU-02, unchanged: per-frame conversion
                    // params from the frame's REAL tags.
                    let color = color_params(frame.colorspace, frame.color_range);
                    self.queue
                        .write_buffer(&entry.color_buf, 0, bytemuck::bytes_of(&color));
                    let plane: [f32; 4] = [
                        frame.width.max(1) as f32,
                        frame.height.max(1) as f32,
                        frame.texture.width().max(1) as f32,
                        frame.texture.height().max(1) as f32,
                    ];
                    self.queue
                        .write_buffer(&entry.plane_buf, 0, bytemuck::cast_slice(&plane));
                    // NO write_texture, NO copy: the plane views ARE the
                    // decoder's own pool slice (48-05's zero-copy import).
                    let frame_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("mixed-nv12-frame-bg"),
                        layout: &cached.1,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(&frame.luma_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(&frame.chroma_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: entry.color_buf.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 5,
                                resource: entry.plane_buf.as_entire_binding(),
                            },
                        ],
                    });
                    let params_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("mixed-nv12-params-bg"),
                        layout: &self.layer_bind_group_layout,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: entry.params_buf.as_entire_binding(),
                        }],
                    });
                    bound.push(Bound::Gpu(frame_bg, params_bg));
                }
            }
        }

        #[cfg(all(windows, feature = "hwdecode"))]
        let nv12_layer = self.nv12_layer_pipeline(OFFSCREEN_FORMAT);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite-mixed-layers"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite-mixed-layers-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // D-01 locked opaque black, cleared ONCE for the stack.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            for b in &bound {
                match b {
                    Bound::Cpu(frame_bg, params_bg) => {
                        pass.set_pipeline(&self.blend_pipeline);
                        pass.set_bind_group(0, frame_bg, &[]);
                        pass.set_bind_group(1, params_bg, &[]);
                    }
                    #[cfg(all(windows, feature = "hwdecode"))]
                    Bound::Gpu(frame_bg, params_bg) => {
                        pass.set_pipeline(&nv12_layer.0);
                        pass.set_bind_group(0, frame_bg, &[]);
                        pass.set_bind_group(1, params_bg, &[]);
                    }
                }
                pass.draw(0..4, 0..1); // per-layer quad (triangle strip)
            }
        }
        self.queue.submit(Some(encoder.finish()));
        Ok(())
    }

    /// PRESENT an already-composited texture into the surface's swapchain,
    /// contain-fit letterboxed at `content_w`x`content_h`.
    ///
    /// This is the presenter's WHOLE job once compositing happens at produce
    /// time (plan 57-06) — a sample-and-blit through the EXISTING `vs_main` /
    /// `fs_main` fullscreen-quad shader, with no knowledge of how many layers
    /// went into the texture. It is also the upscale half of dynamic playback
    /// resolution (plan 57-08): a half- or quarter-resolution composite is
    /// presented by passing its SMALLER dims here and letting the SAME
    /// `contain_fit_viewport` formula scale it up.
    ///
    /// # Sharp magnification (quick-260803-g3d) — present-time, and ONLY here
    ///
    /// When the contain-fit is scaling the composite UP (`fit_w > content_w` —
    /// i.e. a degraded, smaller-than-canvas composite), this draws through
    /// `present_sharp_pipeline`'s Catmull-Rom bicubic instead of the bilinear
    /// `surface_pipeline`. At 1:1 or under minification it draws through
    /// `surface_pipeline` exactly as before, byte-identical: a paused, full-size
    /// picture takes the same code path it always did, and an interpolating
    /// cubic is not applied under minification, where its negative lobes would
    /// only ring.
    ///
    /// Two scope facts, both load-bearing:
    /// * the cost is fragment work at SURFACE resolution — 9 texture fetches per
    ///   output pixel, once per presented frame — which is orders of magnitude
    ///   below the multi-layer composite the degradation exists to cut;
    /// * it can NEVER affect a delivered file. Encoding renders through the
    ///   offscreen pipelines, which do not reference this one, and the shared
    ///   `frame-sampler` is reused unmutated rather than re-specified. Both
    ///   pipelines here draw into a swapchain texture that is presented and
    ///   dropped.
    ///
    /// No readback, no allocation (the no-readback pin scans this body too).
    pub fn blit_texture_to_surface(
        &self,
        view: &wgpu::TextureView,
        content_w: u32,
        content_h: u32,
        surface: &wgpu::Surface,
    ) -> Result<(), EngineError> {
        let pipeline = self.surface_pipeline.as_ref().ok_or_else(|| {
            EngineError::Gpu(
                "blit_texture_to_surface called on a Compositor with no surface pipeline \
                 (use new_with_surface)"
                    .to_string(),
            )
        })?;
        let surface_texture = surface
            .get_current_texture()
            .map_err(|e| EngineError::Gpu(format!("get_current_texture failed: {e}")))?;
        let target_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let viewport = contain_fit_viewport(
            surface_texture.texture.width() as f32,
            surface_texture.texture.height() as f32,
            content_w.max(1) as f32,
            content_h.max(1) as f32,
        );
        // MAGNIFYING? `viewport[2]` is the contain-fitted width in surface
        // pixels; if it exceeds the composite's own width the picture is being
        // stretched up, which is the degraded-playback case the sharp filter
        // exists for. Everything else — a 1:1 present, or a composite being
        // shrunk into a smaller window — keeps the pre-existing pipeline and
        // therefore the pre-existing pixels.
        //
        // Both pipelines are PREBUILT (see `new_offscreen`); this is a pointer
        // choice, not a construction, so the no-readback/no-allocation body scan
        // stays satisfied by construction.
        let pipeline = match self.present_sharp_pipeline.as_ref() {
            Some(sharp) if viewport[2] > content_w as f32 => sharp,
            _ => pipeline,
        };
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit-frame-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("blit-texture-to-surface"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit-texture-to-surface-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_viewport(viewport[0], viewport[1], viewport[2], viewport[3], 0.0, 1.0);
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        surface_texture.present();
        Ok(())
    }

    /// TEST / PARITY READBACK TWIN of [`Compositor::blit_texture_to_surface`]:
    /// sample `view` full-target through the SAME fullscreen-quad shader into
    /// an offscreen RGBA target and read it back tightly packed.
    ///
    /// READBACK IS ALLOWED HERE AND ONLY HERE on the Phase-57 path — this
    /// function exists so the MAD parity pins can see what the pooled composite
    /// target actually holds. It is NEVER on the present path, and it is named
    /// distinctly so `tests/no_readback_pin.rs` can scope its forbidden-call
    /// scan to the steady-state bodies and exempt this one.
    pub fn blit_texture_to_rgba(
        &self,
        view: &wgpu::TextureView,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>, EngineError> {
        let (w, h) = (w.max(1), h.max(1));
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit-readback-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit-readback-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("blit-texture-to-rgba"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit-texture-to-rgba-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        self.submit_and_read_back(encoder, &target, w, h, false)
    }
}

#[cfg(all(windows, feature = "hwdecode"))]
impl Compositor {
    /// The cached NV12-LAYER pipeline + its group-0 bind group layout for
    /// `format` — the blend-capable twin of [`Compositor::nv12_pipeline`].
    /// Built at most once per target format (a shader compile per composite
    /// would dominate the frame budget), then answered from the cache.
    fn nv12_layer_pipeline(
        &self,
        format: wgpu::TextureFormat,
    ) -> std::sync::Arc<(wgpu::RenderPipeline, wgpu::BindGroupLayout)> {
        let mut cache = self
            .nv12_layer_pipelines
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some((_, entry)) = cache.iter().find(|(f, _)| *f == format) {
            return entry.clone();
        }
        let built = std::sync::Arc::new(self.build_nv12_layer_pipeline(format));
        cache.push((format, built.clone()));
        built
    }

    /// Build the `fs_layer_nv12` pipeline for `format`.
    ///
    /// The shader module is `SHADER` CONCATENATED with
    /// [`NV12_LAYER_SHADER_BODY`], so `vs_layer` (the ONE geometry stage) and
    /// the `LayerParams`/`frame_samp` declarations are reused verbatim and
    /// `SHADER` itself stays byte-untouched for every pre-existing pipeline.
    /// The blend state is [`PREMULTIPLIED_OVER`] — literally the same const
    /// `blend_pipeline` binds, not a re-typed copy (threat T-57-07).
    fn build_nv12_layer_pipeline(
        &self,
        format: wgpu::TextureFormat,
    ) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
        let source = format!("{SHADER}{NV12_LAYER_SHADER_BODY}");
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("nv12-layer-composite"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
        // Group 0 for `fs_layer_nv12`: the SHARED sampler at binding 1 (the
        // same declaration `fs_layer` uses), the two NV12 plane views, and the
        // two per-frame uniforms. Binding 0 (`frame_tex`) is declared in the
        // module but unreachable from this entry point, so it is deliberately
        // absent from the layout.
        let plane_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        };
        let uniform_entry = |binding: u32, size: u64| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(size),
            },
            count: None,
        };
        let bgl = self
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("nv12-layer-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    plane_entry(2),       // luma   (Plane0, R8Unorm)
                    plane_entry(3),       // chroma (Plane1, Rg8Unorm)
                    uniform_entry(4, 80), // ColorUniform (ColorParams)
                    uniform_entry(5, 16), // plane geom (display + texture dims)
                ],
            });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("nv12-layer-composite-layout"),
                bind_group_layouts: &[&bgl, &self.layer_bind_group_layout],
                push_constant_ranges: &[],
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("nv12-layer-composite-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    // The EXISTING per-layer vertex stage, byte-for-byte.
                    entry_point: Some("vs_layer"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_layer_nv12"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(PREMULTIPLIED_OVER),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });
        (pipeline, bgl)
    }
}

#[cfg(test)]
mod mixed_layer_tests {
    use std::sync::Mutex;

    use super::{
        AlphaMode, Compositor, CompositeTargetPool, Frame, Layer, LayerCrop, LayerTransform,
        MixedLayer, PooledTarget, COMPOSITE_TARGET_POOL_DEPTH,
    };

    /// Serializes every test in this module that creates a `Compositor`.
    ///
    /// NOT optional, and not cargo-culted: these are the FIRST unit tests in
    /// the `engine` lib binary to build a real wgpu device, and the lib harness
    /// runs them on as many threads as the machine has cores. Building and
    /// tearing down several DX12 devices simultaneously — while the workspace
    /// run has other test binaries doing the same — produced a
    /// `STATUS_HEAP_CORRUPTION` (`0xc0000374`) at process exit under
    /// `cargo test --workspace`, with every individual test reporting `ok`.
    /// Every GPU test FILE in this crate already carries this guard
    /// (`gpu_resident_parity.rs`, `hwdecode_zero_copy.rs`, `vram_usage_probe.rs`,
    /// `mixed_layers_parity.rs`) for the same reason; the unit tests needed it
    /// too.
    static GPU: Mutex<()> = Mutex::new(());

    fn gpu_serial() -> std::sync::MutexGuard<'static, ()> {
        GPU.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// ONE `Compositor` — i.e. ONE DX12 device — for every test in this module.
    ///
    /// Also not cargo-culted. Each `Compositor::new()` builds and later tears
    /// down a real DX12 device, and under `cargo test --workspace` this binary
    /// runs alongside other GPU test binaries (`timeline-render` even holds a
    /// second wgpu MAJOR version, pinned at 29 against this crate's 26). Six
    /// device create/destroy cycles here measurably raised the rate of a
    /// heap-corruption abort at process teardown across the whole run.
    /// Sharing one device cuts that to a single creation which, living in a
    /// `OnceLock` static, is never dropped at all.
    ///
    /// Safe to share because the guard above serializes every user, and because
    /// the only mutable state on `Compositor` — the mixed-layer slot cache — is
    /// re-warmed by each test that measures it.
    fn shared_compositor() -> &'static Compositor {
        static COMPOSITOR: std::sync::OnceLock<Compositor> = std::sync::OnceLock::new();
        COMPOSITOR.get_or_init(|| Compositor::new().expect("offscreen compositor"))
    }

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> Frame {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
        Frame {
            width: w,
            height: h,
            rgba,
        }
    }

    /// Test 1: K distinct persistent targets, handed out and returned; the
    /// (K+1)-th checkout finds nothing until one comes back; and `ensure_size`
    /// allocates EXACTLY once per dims change and never per checkout.
    #[test]
    fn target_pool_hands_out_k_persistent_targets_and_reallocs_only_on_dims_change() {
        let _gpu = gpu_serial();
        let compositor = shared_compositor();
        let mut pool = compositor.create_composite_target_pool(64, 48);
        assert_eq!(pool.size(), (64, 48));
        assert_eq!(
            pool.allocations(),
            COMPOSITE_TARGET_POOL_DEPTH as u64,
            "construction allocates exactly K targets"
        );

        // K distinct targets, all live at once.
        let mut held: Vec<_> = (0..COMPOSITE_TARGET_POOL_DEPTH)
            .map(|i| pool.try_checkout().unwrap_or_else(|| panic!("checkout {i}")))
            .collect();
        let mut indices: Vec<usize> = held.iter().map(|t| t.index()).collect();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(
            indices.len(),
            COMPOSITE_TARGET_POOL_DEPTH,
            "each checkout must hand out a DISTINCT slot"
        );

        // The (K+1)-th finds nothing — the pool never allocates to satisfy it.
        assert!(
            pool.try_checkout().is_none(),
            "a full pool must refuse (blocking, in the checkout() twin) — never grow"
        );
        assert_eq!(
            pool.allocations(),
            COMPOSITE_TARGET_POOL_DEPTH as u64,
            "a refused checkout must not have allocated anything"
        );

        // Return one -> a checkout succeeds again, still with no allocation.
        let returned = held.pop().expect("held a target").index();
        let again = pool.try_checkout().expect("slot returned on Drop");
        assert_eq!(again.index(), returned, "the freed slot is reused");
        assert_eq!(pool.allocations(), COMPOSITE_TARGET_POOL_DEPTH as u64);
        drop(again);
        drop(held);

        // ensure_size at the SAME dims is a no-op, however often it runs.
        for _ in 0..10 {
            pool.ensure_size(64, 48);
        }
        assert_eq!(
            pool.allocations(),
            COMPOSITE_TARGET_POOL_DEPTH as u64,
            "ensure_size at unchanged dims must never allocate"
        );

        // A real dims change reallocates ALL K, exactly once.
        pool.ensure_size(32, 24);
        assert_eq!(pool.size(), (32, 24));
        assert_eq!(
            pool.allocations(),
            2 * COMPOSITE_TARGET_POOL_DEPTH as u64,
            "a dims change reallocates all K targets exactly once"
        );
        pool.ensure_size(32, 24);
        assert_eq!(pool.allocations(), 2 * COMPOSITE_TARGET_POOL_DEPTH as u64);
    }

    /// **Phase 57, plan 57-08.** A dims change while targets are STILL IN
    /// FLIGHT must never re-issue a texture that is already live.
    ///
    /// This is not hypothetical from 57-08 on. Before this plan, `ensure_size`
    /// only ever ran at a project-resolution edit, which cannot happen while the
    /// ring holds composited frames. Dynamic playback resolution changes the
    /// dims mid-playback, with up to `COMPOSITE_TARGET_POOL_DEPTH` composited
    /// entries queued in the ring, each holding a `PooledTarget` by index.
    ///
    /// The defect this pins: `reallocate` used to reset the shared free list to
    /// `0..K` while those old targets were still alive. When one of them was
    /// finally dropped, `release(index)` pushed its index back into the SAME
    /// free list — a list that had already handed that index out again against
    /// the NEW textures. The result is two live `PooledTarget`s sharing one
    /// texture: the producer composites frame N into the very texture a queued
    /// entry is about to present, i.e. a torn or duplicated frame on screen at
    /// every resolution change.
    #[test]
    fn reallocating_under_live_targets_never_reissues_a_live_texture() {
        let _gpu = gpu_serial();
        let compositor = shared_compositor();
        let mut pool = compositor.create_composite_target_pool(64, 48);

        // All K targets in flight — the steady state of a full ring.
        let old: Vec<_> = (0..COMPOSITE_TARGET_POOL_DEPTH)
            .map(|i| pool.try_checkout().unwrap_or_else(|| panic!("checkout {i}")))
            .collect();

        // The level change.
        pool.ensure_size(32, 24);

        // The producer's next composite: one NEW target, while the old ones are
        // still queued.
        let first_new = pool.try_checkout().expect("a fresh pool has free slots");

        // The presenter drains the ring: every OLD target goes away.
        drop(old);

        // Now fill the pool again, as the producer would.
        let mut live: Vec<&PooledTarget> = vec![&first_new];
        let rest: Vec<_> = std::iter::from_fn(|| pool.try_checkout())
            .take(COMPOSITE_TARGET_POOL_DEPTH)
            .collect();
        live.extend(rest.iter());

        // No two LIVE targets may point at the same texture.
        let mut ptrs: Vec<usize> = live
            .iter()
            .map(|t| t.texture() as *const wgpu::Texture as usize)
            .collect();
        let total = ptrs.len();
        ptrs.sort_unstable();
        ptrs.dedup();
        assert_eq!(
            ptrs.len(),
            total,
            "a dims change under live targets re-issued a texture that was \
             already checked out: {total} live targets share only {} distinct \
             textures. The producer would composite into a texture a queued \
             ring entry is about to present.",
            ptrs.len()
        );

        // And the pool still bounds itself: it never hands out more than K at
        // once from the CURRENT generation.
        assert!(
            rest.len() < COMPOSITE_TARGET_POOL_DEPTH,
            "the pool handed out {} targets on top of the one already live — \
             the K-target backpressure (T-57-08) is gone",
            rest.len()
        );
    }

    /// The blocking twin: `checkout()` on a FULL pool parks until a target is
    /// dropped on another thread — the backpressure T-57-08 relies on.
    #[test]
    fn checkout_blocks_until_a_target_returns() {
        let _gpu = gpu_serial();
        let compositor = shared_compositor();
        let pool = std::sync::Arc::new(compositor.create_composite_target_pool(16, 16));
        let held: Vec<_> = (0..COMPOSITE_TARGET_POOL_DEPTH)
            .map(|_| pool.try_checkout().expect("checkout"))
            .collect();
        assert!(pool.try_checkout().is_none(), "pool is full");

        let waiter_pool = pool.clone();
        let waiter = std::thread::spawn(move || {
            let t = waiter_pool.checkout();
            t.index()
        });
        // The waiter cannot possibly have finished: nothing has been returned.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished(), "checkout() must BLOCK on a full pool");

        drop(held);
        let index = waiter.join().expect("waiter thread");
        assert!(index < COMPOSITE_TARGET_POOL_DEPTH);
    }

    /// Test 2: an empty layer list clears the target to the LOCKED opaque
    /// black — the same canvas `composite_layers_to_rgba` yields for `&[]`.
    #[test]
    fn empty_mixed_layer_list_clears_to_opaque_black() {
        let _gpu = gpu_serial();
        let compositor = shared_compositor();
        let pool = compositor.create_composite_target_pool(32, 24);
        let target = pool.try_checkout().expect("checkout");
        compositor
            .composite_mixed_layers_to_target(&[], &target, 32, 24)
            .expect("empty list must succeed");
        let out = compositor
            .blit_texture_to_rgba(target.view(), 32, 24)
            .expect("readback");
        assert_eq!(out.len(), 32 * 24 * 4);
        for (i, px) in out.chunks_exact(4).enumerate() {
            assert_eq!(px, [0, 0, 0, 255], "pixel {i} must be opaque black");
        }
    }

    /// PLAY-06's compositor half that needs NO hardware: the NEW entry point,
    /// fed the SAME layers as CPU arms, must equal the EXISTING, export-ground-
    /// truth `composite_layers_to_rgba` at MAD 0.0000 with ZERO differing
    /// bytes. This is the pin that says the new pass/geometry/blend/pool
    /// machinery did not change the compositor — the GPU arm's own parity is
    /// pinned in `tests/mixed_layers_parity.rs`.
    #[test]
    fn mixed_entry_point_with_cpu_layers_matches_composite_layers_to_rgba() {
        const W: u32 = 96;
        const H: u32 = 64;
        let _gpu = gpu_serial();
        let compositor = shared_compositor();
        let layers = vec![
            Layer {
                frame: solid(32, 16, 0, 0, 255),
                opacity: 0.5,
                transform: LayerTransform {
                    position: (0.25, 0.25),
                    scale: (0.5, 0.5),
                    rotation_deg: 17.0,
                },
                crop: LayerCrop {
                    left: 0.1,
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.2,
                },
                alpha_mode: AlphaMode::Straight,
            },
            Layer {
                frame: solid(48, 32, 0, 255, 0),
                opacity: 0.75,
                transform: LayerTransform {
                    position: (0.0, 0.5),
                    scale: (0.6, 0.5),
                    rotation_deg: 0.0,
                },
                crop: LayerCrop::default(),
                alpha_mode: AlphaMode::Straight,
            },
            Layer::new(solid(W, H, 255, 0, 0), 1.0),
        ];

        let reference = compositor
            .composite_layers_to_rgba(&layers, W, H)
            .expect("existing CPU multi-layer composite (ground truth)");

        let mixed: Vec<MixedLayer> = layers.iter().map(MixedLayer::Cpu).collect();
        let pool = compositor.create_composite_target_pool(W, H);
        let target = pool.try_checkout().expect("checkout");
        compositor
            .composite_mixed_layers_to_target(&mixed, &target, W, H)
            .expect("mixed composite");
        let via_mixed = compositor
            .blit_texture_to_rgba(target.view(), W, H)
            .expect("readback");

        assert_eq!(reference.len(), via_mixed.len());
        let differing = reference
            .iter()
            .zip(via_mixed.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            differing, 0,
            "the mixed entry point must be byte-identical to composite_layers_to_rgba \
             for an all-CPU layer set (D-06: extended, not rewritten)"
        );
    }

    /// PLAY-07/D-08 measured, not asserted: after the first composite warms the
    /// slot cache, repeated composites of the SAME shape allocate NOTHING.
    #[test]
    fn steady_state_mixed_composite_allocates_nothing() {
        const W: u32 = 64;
        const H: u32 = 48;
        let _gpu = gpu_serial();
        let compositor = shared_compositor();
        let layers = [
            Layer::new(solid(32, 24, 0, 0, 255), 0.5),
            Layer::new(solid(W, H, 255, 0, 0), 1.0),
        ];
        let mixed: Vec<MixedLayer> = layers.iter().map(MixedLayer::Cpu).collect();
        let pool = compositor.create_composite_target_pool(W, H);

        {
            let target = pool.try_checkout().expect("checkout");
            compositor
                .composite_mixed_layers_to_target(&mixed, &target, W, H)
                .expect("warm-up composite");
        }
        let warm_slots = compositor.mixed_slot_allocations();
        let warm_targets = pool.allocations();

        for _ in 0..30 {
            let target = pool.try_checkout().expect("checkout");
            compositor
                .composite_mixed_layers_to_target(&mixed, &target, W, H)
                .expect("steady-state composite");
        }
        assert_eq!(
            compositor.mixed_slot_allocations(),
            warm_slots,
            "30 steady-state composites must allocate ZERO per-slot resources"
        );
        assert_eq!(
            pool.allocations(),
            warm_targets,
            "30 steady-state composites must allocate ZERO composite targets"
        );
    }

    /// The `fs_layer_nv12` shader module + pipeline must actually COMPILE and
    /// validate — a WGSL or bind-layout error would otherwise only surface on
    /// a machine with a hardware decode session, i.e. never in the fast suite.
    /// Also pins the per-format cache: a second call builds nothing new.
    #[cfg(all(windows, feature = "hwdecode"))]
    #[test]
    fn nv12_layer_pipeline_compiles_and_caches() {
        let _gpu = gpu_serial();
        let compositor = shared_compositor();
        let first = compositor.nv12_layer_pipeline(super::OFFSCREEN_FORMAT);
        let second = compositor.nv12_layer_pipeline(super::OFFSCREEN_FORMAT);
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "the NV12-layer pipeline must be built at most ONCE per target format"
        );
    }

    /// The pool must be usable from the producer thread the coordinator will
    /// run it on.
    #[test]
    fn pool_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CompositeTargetPool>();
    }
}

#[cfg(test)]
mod layer_params_tests {
    use super::{
        contain_fit_viewport, layer_params_bytes, AlphaMode, Frame, Layer, LayerCrop,
        LayerTransform,
    };

    /// Decode the fitted-quad HALF extents packed at `rect.zw` (uniform floats
    /// #2 and #3 — see `layer_params_bytes`'s `vals` layout).
    fn fit_half_extents(bytes: &[u8; 64]) -> (f32, f32) {
        let hx = f32::from_ne_bytes(bytes[8..12].try_into().unwrap());
        let hy = f32::from_ne_bytes(bytes[12..16].try_into().unwrap());
        (hx, hy)
    }

    /// Decode the `misc` vec4 packed at floats #12..#15 (bytes 48..64):
    /// x = opacity, y = alpha_mode flag (0.0 Straight / 1.0 Premultiplied).
    fn misc_xy(bytes: &[u8; 64]) -> (f32, f32) {
        let x = f32::from_ne_bytes(bytes[48..52].try_into().unwrap());
        let y = f32::from_ne_bytes(bytes[52..56].try_into().unwrap());
        (x, y)
    }

    /// Phase 28 (OVL-01): `Layer.alpha_mode` packs into `misc.y` (vals[13]) as
    /// 0.0 = Straight (default) / 1.0 = Premultiplied — the ONLY new byte in the
    /// 64-byte uniform. Proves the CPU-side wiring the `fs_layer` `select()`
    /// branch reads; the composite-level equivalence/washout proofs live in
    /// `tests/alpha_mode_shader.rs`.
    #[test]
    fn alpha_mode_packs_into_misc_y_and_default_is_straight() {
        let frame = || Frame {
            width: 64,
            height: 48,
            rgba: vec![255; 64 * 48 * 4],
        };
        let base = |alpha_mode: AlphaMode| Layer {
            frame: frame(),
            opacity: 1.0,
            transform: LayerTransform::default(),
            crop: LayerCrop::default(),
            alpha_mode,
        };

        // Default (Layer::new) is Straight -> misc.y == 0.0, misc.x == opacity.
        let default_bytes =
            layer_params_bytes(&Layer::new(frame(), 0.75), 128.0, 96.0).expect("resolves");
        let (opacity, flag) = misc_xy(&default_bytes);
        assert_eq!(flag, 0.0, "Layer::new defaults alpha_mode to Straight (misc.y=0)");
        assert_eq!(opacity, 0.75, "misc.x carries opacity unchanged");

        // Explicit Straight -> 0.0.
        let straight_bytes = layer_params_bytes(&base(AlphaMode::Straight), 128.0, 96.0)
            .expect("straight resolves");
        assert_eq!(misc_xy(&straight_bytes).1, 0.0, "Straight packs misc.y=0.0");

        // Premultiplied -> 1.0 (the skip-flag the shader branches on).
        let premul_bytes = layer_params_bytes(&base(AlphaMode::Premultiplied), 128.0, 96.0)
            .expect("premultiplied resolves");
        assert_eq!(
            misc_xy(&premul_bytes).1,
            1.0,
            "Premultiplied packs misc.y=1.0 (fs_layer skips the premultiply)"
        );

        // BACKWARD-COMPAT: a Straight layer's full 64-byte uniform is
        // BIT-IDENTICAL to the pre-Phase-28 output. misc.y was previously a
        // hardcoded 0.0 padding slot, and Straight packs 0.0 there, so every
        // byte matches — the alpha_convention keystone stays green by
        // construction. (misc.z/misc.w remain 0.0 padding.)
        assert_eq!(&straight_bytes[56..64], &[0u8; 8], "misc.z/misc.w stay zero padding");
    }

    /// CR-01 (23-REVIEW.md): (a) IDENTITY crop keeps the NATIVE contain-fit —
    /// byte-for-byte the pre-fix geometry; (b) a cover crop RESHAPES the fitted
    /// quad to the cropped (visible) region so it fills the slot. Uses a 16:9
    /// source in a tall side_by_side slot (dest rect 320x360 on a 640x360
    /// canvas), the exact "landscape clip in a tall slot" case CR-01 is about.
    #[test]
    fn identity_crop_is_native_fit_byte_identical_and_cover_crop_reshapes_quad() {
        let slot = LayerTransform {
            position: (0.0, 0.0),
            scale: (0.5, 1.0),
            rotation_deg: 0.0,
        };
        let layer = |crop: LayerCrop| Layer {
            frame: Frame {
                width: 160,
                height: 90,
                rgba: vec![255; 160 * 90 * 4],
            },
            opacity: 1.0,
            transform: slot,
            crop,
            alpha_mode: AlphaMode::Straight,
        };

        // (a) Identity crop -> the fit MUST equal the NATIVE contain-fit, i.e.
        // exactly what the pre-fix code produced when it fed native dims to
        // `contain_fit_viewport`. The crop bytes are zero and no other uniform
        // field depends on crop, so matching these half-extents is the complete
        // byte-identity proof for the identity-crop path.
        let id_bytes = layer_params_bytes(&layer(LayerCrop::default()), 640.0, 360.0)
            .expect("identity layer resolves");
        let (id_hx, id_hy) = fit_half_extents(&id_bytes);
        // dest rect is 0.5*640 x 1.0*360 = 320x360; contain_fit(320,360,160,90)
        // -> scale 2.0 -> 320x180 (letterboxed vertically); half-extents 160x90.
        let [_, _, native_fw, native_fh] = contain_fit_viewport(320.0, 360.0, 160.0, 90.0);
        assert_eq!(
            (id_hx, id_hy),
            (native_fw * 0.5, native_fh * 0.5),
            "identity crop must pack the NATIVE contain-fit half-extents \
             (byte-identical to pre-fix geometry)"
        );
        assert_eq!((id_hx, id_hy), (160.0, 90.0), "sanity: native letterbox fit");

        // (b) Cover crop left=right=0.25 -> visible region 80x90 (aspect
        // 0.889 == slot aspect) -> contain_fit(320,360,80,90) -> scale 4.0 ->
        // 320x360 -> half-extents 160x180. The quad now FILLS the slot's full
        // 360 height where identity crop only reached 180 — the CR-01 fix.
        let cov_bytes = layer_params_bytes(
            &layer(LayerCrop {
                left: 0.25,
                right: 0.25,
                top: 0.0,
                bottom: 0.0,
            }),
            640.0,
            360.0,
        )
        .expect("cover layer resolves");
        let (cov_hx, cov_hy) = fit_half_extents(&cov_bytes);
        assert_eq!(
            (cov_hx, cov_hy),
            (160.0, 180.0),
            "cover crop must reshape the fit to FILL the 320x360 slot \
             (half-extents 160x180) — CR-01"
        );
        assert!(
            cov_hy > id_hy,
            "cover crop must make the quad TALLER (fill) than the native \
             letterbox fit (cover {cov_hy} vs native {id_hy})"
        );
    }

    fn red_layer(transform: LayerTransform) -> Layer {
        Layer {
            frame: Frame {
                width: 64,
                height: 48,
                rgba: vec![255; 64 * 48 * 4],
            },
            opacity: 1.0,
            transform,
            crop: LayerCrop::default(),
            alpha_mode: AlphaMode::Straight,
        }
    }

    /// Positive control: an ordinary layer resolves to Some, and every f32 in
    /// the packed uniform is finite (the invariant the shader relies on).
    #[test]
    fn ordinary_layer_resolves_to_finite_uniform_bytes() {
        let bytes = layer_params_bytes(
            &red_layer(LayerTransform {
                position: (0.25, 0.25),
                scale: (0.5, 0.5),
                rotation_deg: 15.0,
            }),
            1920.0,
            1080.0,
        )
        .expect("ordinary layer must resolve");
        for (i, chunk) in bytes.chunks_exact(4).enumerate() {
            let v = f32::from_ne_bytes(chunk.try_into().unwrap());
            assert!(v.is_finite(), "uniform f32 #{i} must be finite, got {v}");
        }
    }

    /// T-18-01 / 18-REVIEW HI-01: FINITE inputs whose RESOLVED pixel-space
    /// geometry overflows f32 during the `* out_w`/`* out_h` multiplication
    /// (or the centre sum) must return None — `Infinity > 0.0` is true in
    /// IEEE-754, so the degenerate-rect guard alone does not catch these,
    /// and Infinity/NaN must never be packed into the GPU uniform.
    #[test]
    fn finite_but_overflowing_resolved_geometry_returns_none() {
        let cases: Vec<(&str, LayerTransform)> = vec![
            (
                "scale 1e37 overflows dest_w/dest_h at 1920x1080",
                LayerTransform {
                    position: (0.0, 0.0),
                    scale: (1e37, 1e37),
                    rotation_deg: 0.0,
                },
            ),
            (
                "position 3e38 overflows dest_x at identity scale",
                LayerTransform {
                    position: (3.0e38, 0.0),
                    scale: (1.0, 1.0),
                    rotation_deg: 0.0,
                },
            ),
            (
                "negative position -3e38 overflows dest_y to -inf",
                LayerTransform {
                    position: (0.0, -3.0e38),
                    scale: (0.5, 0.5),
                    rotation_deg: 0.0,
                },
            ),
            (
                // dest_x and dest_w are each finite here, but the centre
                // cx = dest_x + dest_w * 0.5 overflows.
                "finite dest rect whose centre sum overflows",
                LayerTransform {
                    position: (1.5e35, 0.0),
                    scale: (1.5e35, 0.5),
                    rotation_deg: 0.0,
                },
            ),
        ];
        for (what, transform) in cases {
            assert!(
                layer_params_bytes(&red_layer(transform), 1920.0, 1080.0).is_none(),
                "{what}: must resolve to None (no-op draw), never an \
                 Infinity/NaN uniform"
            );
        }
    }
}

#[cfg(test)]
mod viewport_tests {
    use super::contain_fit_viewport;

    #[test]
    fn viewport_exact_aspect_match_fills_target_no_letterbox() {
        assert_eq!(
            contain_fit_viewport(1920.0, 1080.0, 1920.0, 1080.0),
            [0.0, 0.0, 1920.0, 1080.0]
        );
    }

    #[test]
    fn viewport_square_content_in_wide_target_bars_left_right() {
        let [x, y, w, h] = contain_fit_viewport(1000.0, 500.0, 1000.0, 1000.0);
        assert!(x > 0.0, "wide target: horizontal bars, x > 0 (got {x})");
        assert!(w < 1000.0, "wide target: content narrower than target (got {w})");
        assert_eq!(y, 0.0, "wide target: no vertical bar");
        assert_eq!(h, 500.0, "wide target: full height used");
        // Centered: symmetric bars.
        assert_eq!(x * 2.0 + w, 1000.0, "content must be centered");
    }

    #[test]
    fn viewport_square_content_in_tall_target_bars_top_bottom() {
        let [x, y, w, h] = contain_fit_viewport(500.0, 1000.0, 1000.0, 1000.0);
        assert!(y > 0.0, "tall target: vertical bars, y > 0 (got {y})");
        assert!(h < 1000.0, "tall target: content shorter than target (got {h})");
        assert_eq!(x, 0.0, "tall target: no horizontal bar");
        assert_eq!(w, 500.0, "tall target: full width used");
        assert_eq!(y * 2.0 + h, 1000.0, "content must be centered");
    }

    #[test]
    fn viewport_degenerate_content_dims_clamped_never_div_by_zero() {
        let [x, y, w, h] = contain_fit_viewport(1920.0, 1080.0, 0.0, 0.0);
        assert!(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite());
        assert!(w > 0.0 && h > 0.0);
    }
}
