// The Timeline's SECOND pipeline, and the first one that samples anything.
//
// WHY A SECOND PIPELINE AT ALL
// ----------------------------
// `quads.wgsl` deliberately runs every flat rectangle through one pipeline, because a lane
// band and a clip body differ only in numbers. A textured quad is not a numbers difference:
// it needs a bind group carrying a texture and a sampler, which is a different bind group
// LAYOUT, which is a different pipeline layout, which is a different pipeline. Folding the
// two together would mean binding the filmstrip atlas for every ruler tick.
//
// COLOUR SPACE — the same decision `quads.wgsl` records, from the other side
// -------------------------------------------------------------------------
// The atlas is `Rgba8Unorm`, NOT `Rgba8UnormSrgb`, so `textureSample` applies no transfer
// function and hands back exactly the bytes `crates/filmstrip` wrote. The surface is also
// non-sRGB. Decoded video frames arrive already sRGB-encoded, so passing them straight
// through is what puts the thumbnail on screen at the brightness the exported file will
// have. Sampling through an sRGB view would decode once and never re-encode, and every
// thumbnail would be visibly darker than its own frame — the classic "looks slightly off
// forever" bug, in the one place a user can directly compare against the Preview region.
//
// ALPHA IS D-08, NOT DECORATION
// -----------------------------
// `crates/filmstrip`'s downsampler fits each frame into its cell PRESERVING ASPECT and
// leaves the leftover pixels at **alpha = 0** (`downsample.rs`'s module header says so from
// its side). Straight-alpha blending here is therefore what makes a 9:16 phone clip show as
// a narrow thumbnail with the clip's own body colour beside it, rather than as a
// centre-cropped lie about the footage. The gap colour is never chosen here: it is simply
// the body quad `quads.rs` already drew underneath, which is a C#-resolved named token
// (CLAUDE.md convention 7 — this crate still holds no colour of its own).

struct Globals {
    // Surface size in physical px. Used to map px -> normalised device coordinates.
    resolution: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VertexIn {
    // The static unit quad, (0,0)..(1,1) — the SAME buffer shape `quads.wgsl` uses.
    @location(0) corner: vec2<f32>,
    // Per-instance.
    @location(1) rect: vec4<f32>,    // x, y, w, h  (physical px, top-left origin)
    @location(2) uv_rect: vec4<f32>, // u0, v0, u1, v1 (normalised over the atlas)
};

struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    // NO AA_PAD here, unlike `quads.wgsl`. That shader inflates its quad so an antialiased
    // SDF edge has somewhere to live; this one has no SDF and a hard edge, and inflating
    // would sample OUTSIDE the tile's uv rect — which with a clamped sampler smears the
    // edge texel and with a neighbouring tile in the same sheet would show a sliver of the
    // wrong frame. Tiles butt edge-to-edge by design (D-05), so any bleed is visible.
    let px = in.rect.xy + in.corner * in.rect.zw;

    let ndc = vec2<f32>(
        px.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - px.y / globals.resolution.y * 2.0,
    );

    var out: VertexOut;
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = in.uv_rect.xy + in.corner * (in.uv_rect.zw - in.uv_rect.xy);
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    // Straight alpha out, matching the pipeline's `ALPHA_BLENDING` state and `quads.wgsl`'s
    // own choice. Transparent letterbox padding therefore lets the body fill through.
    return textureSample(atlas_texture, atlas_sampler, in.uv);
}
