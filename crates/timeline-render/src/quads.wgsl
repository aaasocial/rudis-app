// The Timeline's ONE geometry shader: every flat rectangle, band, tick, handle, guide and
// playhead the region draws is an instance through here.
//
// WHY THIS IS HAND-ROLLED, AND WHERE THAT STOPS
// ---------------------------------------------
// 52-RESEARCH's "Don't Hand-Roll" table draws a line and states the test for which side of
// it a problem is on: does getting this subtly wrong produce a correctness bug users cannot
// see coming (font metrics, audio decode, unbounded memory), or does it just look slightly
// off and is trivially fixable (a rectangle's corner radius)? Flat coloured quads are
// squarely the second, so they are ours. Font shaping, glyph rasterisation and atlas
// packing are squarely the first, so they are glyphon's (see `text.rs`). No vector-graphics
// library is taken for this: Vello is self-declared alpha, and Lyon solves arbitrary paths
// the Timeline does not have.
//
// ONE PIPELINE, NOT THREE
// -----------------------
// A lane band, a clip body and a 1px ruler tick differ only in their radius and border
// width, so they are the SAME instance format through the SAME pipeline with different
// numbers, not three pipelines with three bind groups and three state switches. The
// rounded-rect signed-distance field below is what makes that possible: `radius_px = 2`
// gives the handoff's `radius-clip` (README:229), `radius_px = 0` gives a hard rectangle,
// and `border_px = 0` gives a plain fill.
//
// COLOUR SPACE AND BLENDING — stated explicitly, because getting this wrong is the classic
// "looks slightly off forever" bug nobody files
// ------------------------------------------------------------------------------------
// The surface format is `Bgra8Unorm` — NON-sRGB (see `surface.rs::PREFERRED_FORMAT`, whose
// comment says the same thing from the other side). A non-sRGB target applies no transfer
// function on write, so what is written is what is displayed. The `u32 0xAARRGGBB` colours
// that arrive across the ABI are sRGB-encoded design-token values, and they are therefore
// passed through as `byte / 255` with NO linearisation. Linearising them here and writing
// to a non-sRGB target would darken every colour in the region.
//
// A consequence, taken deliberately: alpha blending then happens in sRGB space rather than
// in linear light. That is "physically wrong" and it is also exactly what CSS, WPF and
// WinUI all do — and the design handoff's own source is an HTML mock rendered by a browser
// (`design_handoff_rudis_editor/Rudis Editor - Your Layout.html`). Matching the artefact
// the colours were authored against beats theoretical correctness for a UI port. `text.rs`
// makes the same choice explicitly by taking glyphon's `ColorMode::Web`.
//
// Blending is STRAIGHT alpha (`src_alpha, one_minus_src_alpha`), not premultiplied: the
// fragment stage returns un-premultiplied rgb with coverage in `a`.

struct Globals {
    // Surface size in physical px. Used to map px -> normalised device coordinates.
    resolution: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;

struct VertexIn {
    // The static unit quad, (0,0)..(1,1). One buffer, uploaded once, never touched again.
    @location(0) corner: vec2<f32>,
    // Per-instance, from the dynamic instance buffer.
    @location(1) rect: vec4<f32>,   // x, y, w, h  (physical px, top-left origin)
    @location(2) fill: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) params: vec4<f32>, // radius_px, border_px, unused, unused
};

struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    // Position relative to the rect's CENTRE, in px — the SDF's input.
    @location(0) local: vec2<f32>,
    // Half the rect's size, in px.
    @location(1) half_size: vec2<f32>,
    @location(2) fill: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) params: vec4<f32>,
};

// The drawn quad is inflated by this many px on every side so the antialiased falloff at
// the shape's edge has somewhere to live. Without it the outermost half-pixel of every
// rectangle is clipped away and edges read as harsh — which looks like "no antialiasing"
// rather than like a bug, and so never gets investigated.
const AA_PAD: f32 = 1.0;

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    let size = max(in.rect.zw, vec2<f32>(0.0, 0.0));
    let half_size = size * 0.5;
    let centre = in.rect.xy + half_size;

    // Expand the unit quad across the inflated rect.
    let inflated = size + vec2<f32>(AA_PAD * 2.0, AA_PAD * 2.0);
    let offset = (in.corner - vec2<f32>(0.5, 0.5)) * inflated;
    let px = centre + offset;

    // px (top-left origin, y down) -> NDC (centre origin, y up).
    let ndc = vec2<f32>(
        px.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - px.y / globals.resolution.y * 2.0,
    );

    var out: VertexOut;
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.local = offset;
    out.half_size = half_size;
    out.fill = in.fill;
    out.border = in.border;
    out.params = in.params;
    return out;
}

// Signed distance to a rounded box centred at the origin. Negative inside, positive
// outside, and — the property the whole approach rests on — its magnitude is a real
// distance in px, so a half-pixel smoothstep across it is a correct coverage estimate.
fn sd_round_box(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    // Clamp the radius so it can never exceed half the smaller side; an over-large radius
    // would otherwise invert the shape rather than saturate into a stadium.
    let r = clamp(radius, 0.0, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let radius = in.params.x;
    let border_px = in.params.y;

    let d = sd_round_box(in.local, in.half_size, radius);

    // Coverage of the shape itself: 1 well inside, 0 well outside, a one-pixel ramp across
    // the boundary.
    let outer = 1.0 - smoothstep(-0.5, 0.5, d);

    // Coverage of the INTERIOR, i.e. inside the border stroke. `border_px` is an INNER
    // stroke by construction — it eats into the rect rather than growing it — so a 1px
    // border on a 2px-wide tick still fits inside the tick.
    var inner: f32 = 1.0;
    if (border_px > 0.0) {
        inner = 1.0 - smoothstep(-0.5, 0.5, d + border_px);
    }

    let colour = mix(in.border, in.fill, inner);

    // Straight alpha out; coverage multiplies the colour's own alpha.
    return vec4<f32>(colour.rgb, colour.a * outer);
}
