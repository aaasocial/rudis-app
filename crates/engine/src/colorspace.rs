//! Per-frame YUV→RGB conversion parameters (Phase 48, GPU-02).
//!
//! The CPU builds the 3×3 conversion matrix and range offsets from each
//! frame's REAL `AVFrame.colorspace` / `AVFrame.color_range` tags and pushes
//! them to the composite shader as a uniform; the shader is a straight matrix
//! multiply. The branchy selection logic lives HERE, in testable Rust — never
//! in WGSL, and NEVER as a hardcoded matrix (GPU-02's explicit prohibition).
//!
//! ## The untagged-content rule (AMENDED, empirically confirmed)
//!
//! Untagged colorspace → BT.601-family coefficients; untagged range →
//! limited ("tv"). **Unconditionally — there is no dimension branch.** The
//! rule is dimension-independent BY API SHAPE: [`color_params`] takes only
//! the two tag enums, so a dimension branch is unrepresentable.
//!
//! Grounding (cite chain, strongest last):
//! - 48-CONTEXT.md § Colorspace, the **AMENDED 2026-07-29** decision: research
//!   read libswscale's source and found no dimension check anywhere —
//!   `swscale.h` pins `SWS_CS_DEFAULT` to `SWS_CS_ITU601` unconditionally, and
//!   `format.c`'s `AVCOL_SPC_UNSPECIFIED` branch takes the BT.470BG
//!   coefficients unconditionally; `fmt_encode_range` defaults unspecified
//!   range to limited.
//! - `artifacts/48-02-untagged-rule-recheck.md`: the rule CONFIRMED on the
//!   vendored production `ffmpeg.exe` itself — untagged content decodes
//!   byte-identically (full-frame RGBA md5) to explicitly-tagged
//!   BT.601-limited siblings on BOTH SD (640×480) and HD (1280×720), and the
//!   720p fixture is exactly the one a dimension branch would have flipped to
//!   BT.709. `VERDICT: untagged → BT.601-family + limited on BOTH SD and HD
//!   (no dimension branch)`.
//!
//! ## Constant provenance (PROVENANCE.md Entry 22)
//!
//! The Kr/Kg/Kb triples are ITU-R BT.601-7 / BT.709-6 public standard values;
//! the matrix is DERIVED from them in code by the standard algebra (see
//! [`color_params`]). libswscale (LGPL) was READ to confirm FFmpeg applies
//! these same values; no libswscale source was copied.

use crate::import::{FrameColorRange, FrameColorspace};

/// Per-frame YUV→RGB conversion parameters, laid out exactly as the WGSL
/// `ColorUniform` uniform expects (std140-style uniform rules):
///
/// ```wgsl
/// struct ColorUniform {
///     mat: mat3x3<f32>,       // 3 columns, each vec3 padded to 16 bytes
///     range_offset: vec3<f32>,
///     range_scale: vec3<f32>,
/// }
/// ```
///
/// `mat3x3<f32>` in a WGSL uniform is COLUMN-major with a 16-byte column
/// stride (each vec3 column padded to a vec4 slot — 48 bytes total), and each
/// `vec3<f32>` member is 16-byte aligned, so the whole struct is 80 bytes.
/// The compile assert below pins that.
///
/// Column semantics (`m * v` in WGSL = `v.x*col0 + v.y*col1 + v.z*col2`):
/// - `mat[0]` — the Y′ weights `(1, 1, 1)`;
/// - `mat[1]` — the Cb′ weights `(0, m11, m21)`;
/// - `mat[2]` — the Cr′ weights `(m02, m12, 0)`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ColorParams {
    /// Column-major 3×3 matrix, each column padded to 16 bytes (WGSL
    /// `mat3x3<f32>` uniform layout). Padding lanes are always 0.0.
    pub mat: [[f32; 4]; 3],
    /// Per-component offsets subtracted from the raw normalized texel values:
    /// `[Y_off, C_off, C_off]`.
    pub range_offset: [f32; 3],
    /// WGSL `vec3<f32>` alignment padding — always 0.0.
    pub _pad0: f32,
    /// Per-component scales applied after the offset: `[Y_scale, C_scale,
    /// C_scale]`.
    pub range_scale: [f32; 3],
    /// WGSL struct-size padding — always 0.0.
    pub _pad1: f32,
}

/// WGSL-layout pin: `mat3x3<f32>` (48) + `vec3<f32>`+pad (16) + `vec3<f32>`
/// +pad (16) = 80 bytes. If this ever fails, the uniform the shader reads and
/// the bytes the CPU writes have silently diverged.
const _: () = assert!(std::mem::size_of::<ColorParams>() == 80);

impl ColorParams {
    /// The R←Cr′ coefficient (`2·(1−Kr)`): 1.4020 (BT.601-family) / 1.5748
    /// (BT.709). The single most 601-vs-709-distinguishing cell.
    pub fn m02(&self) -> f32 {
        self.mat[2][0]
    }

    /// The G←Cb′ coefficient (`−(Kb/Kg)·m21`): −0.3441 / −0.1873.
    pub fn m11(&self) -> f32 {
        self.mat[1][1]
    }

    /// The G←Cr′ coefficient (`−(Kr/Kg)·m02`): −0.7141 / −0.4681.
    pub fn m12(&self) -> f32 {
        self.mat[2][1]
    }

    /// The B←Cb′ coefficient (`2·(1−Kb)`): 1.7720 / 1.8556.
    pub fn m21(&self) -> f32 {
        self.mat[1][2]
    }
}

/// Build the per-frame conversion parameters from the frame's REAL tags.
///
/// Deliberately takes ONLY the two tag enums — the untagged mapping is
/// dimension-INDEPENDENT (the AMENDED 48-CONTEXT.md rule, confirmed on the
/// vendored production binary by `artifacts/48-02-untagged-rule-recheck.md`),
/// so a dimension branch is unrepresentable in this API.
///
/// Selection (matches libswscale's unconditional defaults, cited above):
/// - `Bt709` → Kr/Kg/Kb = 0.2126/0.7152/0.0722 (ITU-R BT.709-6);
/// - `Bt601` **and `Unspecified`** → Kr/Kg/Kb = 0.299/0.587/0.114
///   (ITU-R BT.601-7 / BT.470BG / SMPTE 170M — numerically identical);
/// - `Full` → identity range decode (offsets/scales for "pc" range);
/// - `Limited` **and `Unspecified`** → limited/"tv" range decode.
pub fn color_params(cs: FrameColorspace, range: FrameColorRange) -> ColorParams {
    let kr_kg_kb = match cs {
        FrameColorspace::Bt709 => BT709_KR_KG_KB,
        // The AMENDED rule: Unspecified takes the BT.601-family coefficients
        // UNCONDITIONALLY (48-CONTEXT.md § Colorspace, AMENDED 2026-07-29;
        // confirmed on the vendored binary in 48-02's artifact).
        FrameColorspace::Bt601 | FrameColorspace::Unspecified => BT601_KR_KG_KB,
    };
    let (y_off, y_scale, c_scale) = match range {
        // Full ("pc"): identity Y decode; chroma re-centred, not rescaled.
        FrameColorRange::Full => (0.0, 1.0, 1.0),
        // Limited ("tv") — ALSO the Unspecified default (libswscale's
        // `fmt_encode_range` falls into the limited branch for untagged
        // range; 48-RESEARCH.md § Pattern 2, VERIFIED). The constants are
        // written as the defining fractions, never as rounded decimals.
        FrameColorRange::Limited | FrameColorRange::Unspecified => {
            (16.0 / 255.0, 255.0 / 219.0, 255.0 / 224.0)
        }
    };
    // Chroma is centred at 128 in BOTH ranges.
    let c_off = 128.0 / 255.0;
    ColorParams {
        mat: matrix_from_luma_coefficients(kr_kg_kb),
        range_offset: [y_off, c_off, c_off],
        _pad0: 0.0,
        range_scale: [y_scale, c_scale, c_scale],
        _pad1: 0.0,
    }
}

/// ITU-R BT.601-7 luma coefficients `[Kr, Kg, Kb]` — numerically identical
/// for `AVCOL_SPC_BT470BG` and `AVCOL_SPC_SMPTE170M`, and the untagged
/// default (libavutil/csp.c's `luma_coefficients` table carries these exact
/// values; cited in 48-RESEARCH.md § Pattern 2).
const BT601_KR_KG_KB: [f64; 3] = [0.299, 0.587, 0.114];

/// ITU-R BT.709-6 luma coefficients `[Kr, Kg, Kb]` (`AVCOL_SPC_BT709`).
const BT709_KR_KG_KB: [f64; 3] = [0.2126, 0.7152, 0.0722];

/// Derive the column-major conversion matrix from `[Kr, Kg, Kb]` — the
/// standard algebra (48-RESEARCH.md § Pattern 2, VERIFIED against
/// `libswscale/format.c`'s own matrix construction):
///
/// ```text
/// m02 (R←Cr) = 2·(1−Kr)      m21 (B←Cb) = 2·(1−Kb)
/// m11 (G←Cb) = −(Kb/Kg)·m21  m12 (G←Cr) = −(Kr/Kg)·m02
/// R = Y′ + m02·Cr′    G = Y′ + m11·Cb′ + m12·Cr′    B = Y′ + m21·Cb′
/// ```
///
/// Computed in `f64` and narrowed once at the end, so the two Kr/Kg/Kb
/// triples above are the ONLY numeric inputs — no rounded matrix literal
/// exists anywhere in this crate outside the unit-test reference table.
fn matrix_from_luma_coefficients(kr_kg_kb: [f64; 3]) -> [[f32; 4]; 3] {
    let [kr, kg, kb] = kr_kg_kb;
    let m02 = 2.0 * (1.0 - kr);
    let m21 = 2.0 * (1.0 - kb);
    let m11 = -(kb / kg) * m21;
    let m12 = -(kr / kg) * m02;
    // Column-major with zeroed WGSL padding lanes:
    //   col0 = Y' weights, col1 = Cb' weights, col2 = Cr' weights.
    [
        [1.0, 1.0, 1.0, 0.0],
        [0.0, m11 as f32, m21 as f32, 0.0],
        [m02 as f32, m12 as f32, 0.0, 0.0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table tolerance — the reference cells are rounded to 4 decimals
    /// (48-RESEARCH.md § Pattern 2's table), so 1e-4 is the honest bound.
    const TOL: f32 = 1e-4;

    /// Assert all four non-trivial matrix cells against the verified table.
    fn assert_matrix_cells(p: &ColorParams, m02: f32, m21: f32, m11: f32, m12: f32, who: &str) {
        for (got, want, name) in [
            (p.m02(), m02, "m02 (R<-Cr)"),
            (p.m21(), m21, "m21 (B<-Cb)"),
            (p.m11(), m11, "m11 (G<-Cb)"),
            (p.m12(), m12, "m12 (G<-Cr)"),
        ] {
            assert!(
                (got - want).abs() < TOL,
                "{who}: {name} = {got}, want {want} (tolerance {TOL})"
            );
        }
        // The structural cells: the Y' column is all ones, and the two
        // structurally-zero cells really are zero (R takes no Cb, B takes no
        // Cr). These are exact, not toleranced.
        assert_eq!(p.mat[0][0], 1.0, "{who}: R<-Y' must be exactly 1.0");
        assert_eq!(p.mat[0][1], 1.0, "{who}: G<-Y' must be exactly 1.0");
        assert_eq!(p.mat[0][2], 1.0, "{who}: B<-Y' must be exactly 1.0");
        assert_eq!(p.mat[1][0], 0.0, "{who}: R<-Cb' must be exactly 0.0");
        assert_eq!(p.mat[2][2], 0.0, "{who}: B<-Cr' must be exactly 0.0");
        // WGSL column-padding lanes must be zero — anything else would be
        // uninitialized garbage shipped to the GPU.
        for (c, col) in p.mat.iter().enumerate() {
            assert_eq!(col[3], 0.0, "{who}: mat column {c} padding lane must be 0.0");
        }
        assert_eq!(p._pad0, 0.0, "{who}: _pad0 must be 0.0");
        assert_eq!(p._pad1, 0.0, "{who}: _pad1 must be 0.0");
    }

    /// The limited ("tv") range constants, written as the SAME fractions the
    /// implementation must use — asserted EXACTLY equal, not toleranced.
    fn assert_limited_range(p: &ColorParams, who: &str) {
        assert_eq!(p.range_offset[0], 16.0 / 255.0, "{who}: Y_off must be 16/255");
        assert_eq!(p.range_offset[1], 128.0 / 255.0, "{who}: Cb_off must be 128/255");
        assert_eq!(p.range_offset[2], 128.0 / 255.0, "{who}: Cr_off must be 128/255");
        assert_eq!(p.range_scale[0], 255.0 / 219.0, "{who}: Y_scale must be 255/219");
        assert_eq!(p.range_scale[1], 255.0 / 224.0, "{who}: Cb_scale must be 255/224");
        assert_eq!(p.range_scale[2], 255.0 / 224.0, "{who}: Cr_scale must be 255/224");
    }

    /// The full ("pc") range constants: identity Y decode, chroma re-centred
    /// but not rescaled.
    fn assert_full_range(p: &ColorParams, who: &str) {
        assert_eq!(p.range_offset[0], 0.0, "{who}: full-range Y_off must be 0");
        assert_eq!(p.range_offset[1], 128.0 / 255.0, "{who}: Cb_off must be 128/255");
        assert_eq!(p.range_offset[2], 128.0 / 255.0, "{who}: Cr_off must be 128/255");
        assert_eq!(p.range_scale[0], 1.0, "{who}: full-range Y_scale must be 1.0");
        assert_eq!(p.range_scale[1], 1.0, "{who}: full-range Cb_scale must be 1.0");
        assert_eq!(p.range_scale[2], 1.0, "{who}: full-range Cr_scale must be 1.0");
    }

    /// 48-RESEARCH.md § Pattern 2's verified BT.709 row + limited range.
    #[test]
    fn bt709_limited_matches_verified_table() {
        let p = color_params(FrameColorspace::Bt709, FrameColorRange::Limited);
        assert_matrix_cells(&p, 1.5748, 1.8556, -0.1873, -0.4681, "bt709/limited");
        assert_limited_range(&p, "bt709/limited");
    }

    /// 48-RESEARCH.md § Pattern 2's verified BT.601-family row + full range.
    #[test]
    fn bt601_full_matches_verified_table() {
        let p = color_params(FrameColorspace::Bt601, FrameColorRange::Full);
        assert_matrix_cells(&p, 1.4020, 1.7720, -0.3441, -0.7141, "bt601/full");
        assert_full_range(&p, "bt601/full");
    }

    /// The remaining two tagged corners of the fixture matrix, so all 8 table
    /// cells are pinned through both range paths.
    #[test]
    fn remaining_tagged_corners_match_verified_table() {
        let p = color_params(FrameColorspace::Bt601, FrameColorRange::Limited);
        assert_matrix_cells(&p, 1.4020, 1.7720, -0.3441, -0.7141, "bt601/limited");
        assert_limited_range(&p, "bt601/limited");

        let p = color_params(FrameColorspace::Bt709, FrameColorRange::Full);
        assert_matrix_cells(&p, 1.5748, 1.8556, -0.1873, -0.4681, "bt709/full");
        assert_full_range(&p, "bt709/full");
    }

    /// THE AMENDED RULE: fully untagged content takes BT.601-family + limited
    /// — exactly equal (whole struct) to the explicit Bt601/Limited params.
    /// Empirical grounding: artifacts/48-02-untagged-rule-recheck.md (VERDICT
    /// confirmed on the vendored production binary, both SD and HD).
    #[test]
    fn untagged_equals_bt601_limited_the_amended_rule() {
        assert_eq!(
            color_params(FrameColorspace::Unspecified, FrameColorRange::Unspecified),
            color_params(FrameColorspace::Bt601, FrameColorRange::Limited),
            "untagged content must decode with BT.601-family + limited params, \
             unconditionally (the AMENDED 48-CONTEXT.md rule)"
        );
    }

    /// Each Unspecified tag falls back independently: untagged range alone →
    /// limited; untagged colorspace alone → BT.601-family.
    #[test]
    fn each_unspecified_tag_falls_back_independently() {
        assert_eq!(
            color_params(FrameColorspace::Bt709, FrameColorRange::Unspecified),
            color_params(FrameColorspace::Bt709, FrameColorRange::Limited),
            "untagged range alone must decode as limited"
        );
        assert_eq!(
            color_params(FrameColorspace::Unspecified, FrameColorRange::Full),
            color_params(FrameColorspace::Bt601, FrameColorRange::Full),
            "untagged colorspace alone must decode as BT.601-family"
        );
    }

    /// The untagged mapping is dimension-INDEPENDENT by API SHAPE: the
    /// function takes ONLY the two tag enums, so a dimension branch is
    /// unrepresentable. This is a compile-shape assertion — if anyone adds a
    /// parameter, this line stops compiling and the AMENDED-rule rationale
    /// must be revisited explicitly.
    #[test]
    fn api_shape_makes_a_dimension_branch_unrepresentable() {
        let f: fn(FrameColorspace, FrameColorRange) -> ColorParams = color_params;
        // Use it through the narrowed pointer so the assertion is live code.
        let p = f(FrameColorspace::Unspecified, FrameColorRange::Unspecified);
        assert!((p.m02() - 1.4020).abs() < TOL, "untagged routes to the 601 family");
    }

    /// The GPU-bound byte image: exactly 80 bytes (the WGSL ColorUniform
    /// layout the compile assert pins), and the bytes round-trip.
    #[test]
    fn pod_bytes_are_the_wgsl_uniform_layout() {
        let p = color_params(FrameColorspace::Bt709, FrameColorRange::Limited);
        let bytes = bytemuck::bytes_of(&p);
        assert_eq!(bytes.len(), 80, "ColorParams must serialize to exactly 80 bytes");
        let back: ColorParams = *bytemuck::from_bytes(bytes);
        assert_eq!(back, p, "Pod bytes must round-trip losslessly");
    }
}
