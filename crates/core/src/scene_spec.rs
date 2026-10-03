//! Declarative scene-spec contract (Phase 24, ASSET-01/02).
//!
//! This is the phase's ONE pure-data contract: a validated, typed shape for a
//! Claude-authored generative asset (a solid/gradient background plus ordered
//! shape/text elements, each with an optional transform/opacity/crop and
//! optional keyframe animation). It is DECLARATIVE-ONLY — every field is a
//! plain number/string/enum/array/object, and `#[serde(deny_unknown_fields)]`
//! on every scene-spec-OWNED node structurally rejects an injected
//! `"code"`/`"script"`/`"eval"` field at deserialize time (SC-3, T-24-01).
//!
//! ZERO I/O: like the rest of `crates/core` (pinned by `tests/offline_guard.rs`
//! to `serde` + `thiserror` only), this module NEVER pulls in a JSON codec — the
//! CALLER (`crates/app-core`, a separate crate that owns the JSON codec) always
//! performs deserialization, then calls [`SceneSpec::resolve`], the ONE entry
//! point (a `grep` for a JSON-codec crate in this file returns zero). All
//! color-parsing, text-payload validation, and ephemeral-`Clip` construction
//! happen INSIDE `resolve()` so the caller never needs any of this crate's
//! `pub(crate)`-only internals (`crate::tools::parse_fill`/`build_keyframe_track`,
//! `crate::command::validate_text_payload`).
//!
//! Animation reuses the codebase's ONE proven sampler ([`crate::model::Clip::sample_at`])
//! via an ephemeral, never-persisted [`crate::model::Clip`] per element — zero
//! new interpolation/arity code.

use serde::{Deserialize, Serialize};

use crate::model::{ClipCrop, ClipTransform, Interpolation, TextAlign};

/// Maximum number of scene elements (DoS cap, T-24-02) — checked BEFORE any
/// allocation-heavy render work exists to reach.
pub const MAX_SCENE_ELEMENTS: usize = 64;
/// Maximum scene width/height in pixels (DoS cap) — matches
/// `set_project_settings`' existing `1..=7680` dimension cap.
pub const MAX_SCENE_DIM: u32 = 7680;
/// Maximum `generate_video` duration in seconds (DoS cap).
pub const MAX_SCENE_DURATION_SECONDS: f64 = 30.0;

/// Per-element raster ceiling in pixels (CR-02). Mirrors
/// `engine::text::MAX_RASTER_PIXELS` (2^24 px ≈ 64 MiB RGBA), the per-`Frame`
/// clamp `engine::scene::clamp_dims` already enforces. Duplicated here (core
/// cannot depend on `engine`) purely to bound the AGGREGATE budget below; the
/// engine remains the single runtime enforcer of the per-frame clamp.
pub const MAX_SCENE_RASTER_PIXELS_PER_ELEMENT: u64 = 1 << 24;

/// Aggregate raster ceiling across a WHOLE scene — background plus every
/// element — in pixels (CR-02). `render_scene_frame` builds every element's
/// `Frame` into one `Vec<Layer>` that is alive simultaneously while the
/// compositor runs, so the DoS surface is the SUM of the per-element buffers,
/// not each one individually. With `MAX_SCENE_ELEMENTS` (64) × the per-element
/// ceiling (~64 MiB) the unbounded worst case is ~4 GiB per frame — repeated for
/// every encoded `generate_video` frame. This budget is `4 ×` the per-element
/// ceiling ≈ 256 MiB RGBA: comfortably above a legitimate multi-element title
/// card (a 4K background plus a dozen shapes/text is well under it) yet ~16×
/// below the 4 GiB worst case, enforced in [`SceneSpec::resolve`] BEFORE any
/// `Frame` is allocated.
pub const MAX_SCENE_TOTAL_RASTER_PIXELS: u64 = 4 * MAX_SCENE_RASTER_PIXELS_PER_ELEMENT;

/// The declarative scene: a background plus ordered elements, with an optional
/// timebase (width/height/fps/durationSeconds) that DEFAULTS to the active
/// project's own settings when omitted (resolved by [`SceneSpec::resolve`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SceneSpec {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// `generate_video` only; ignored by `generate_image`.
    #[serde(default)]
    pub fps: Option<f64>,
    /// `generate_video` only.
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    pub background: SceneBackground,
    #[serde(default)]
    pub elements: Vec<SceneElement>,
}

/// The scene background: a solid color or a 2-stop linear gradient.
// `rename_all_fields` (not just `rename_all`) is what carries camelCase into the
// struct-variant FIELDS (e.g. `angleDeg`) — `rename_all` alone only renames the
// variant identifiers. A scene element's `transform`/`crop` sub-objects use the
// scene-spec-local [`SceneTransform`]/[`SceneCrop`] wrappers (below): they keep
// the shared types' OWN (snake_case) field names but ADD
// `#[serde(deny_unknown_fields)]`, so a stray key inside a `transform`/`crop`
// object is REJECTED at deserialize time — the SAME structural closure every
// other scene-spec-owned node has (WR-03/SC-3), not merely the Anthropic schema
// layer (which `strict:false` leaves unenforced server-side).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SceneBackground {
    Solid {
        color: String,
    },
    Gradient {
        from: String,
        to: String,
        #[serde(default)]
        angle_deg: f64,
    },
}

/// SC-3 (WR-03) wrapper around the SHARED [`ClipTransform`]. Field-for-field
/// identical (same snake_case wire names, same required fields) but adds
/// `#[serde(deny_unknown_fields)]`, so a stray key inside a scene element's
/// `transform` object is REJECTED at deserialize time — the shared
/// `ClipTransform` (reused by already-shipped Phase-18/19 clip tools that must
/// stay forward-compatible) is left UNTOUCHED. `From`-converts into it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneTransform {
    pub position: (f32, f32),
    pub scale: (f32, f32),
    pub rotation_deg: f32,
}

impl From<SceneTransform> for ClipTransform {
    fn from(t: SceneTransform) -> Self {
        ClipTransform {
            position: t.position,
            scale: t.scale,
            rotation_deg: t.rotation_deg,
        }
    }
}

/// SC-3 (WR-03) wrapper around the SHARED [`ClipCrop`] — same rationale as
/// [`SceneTransform`]: identical fields plus `#[serde(deny_unknown_fields)]` so a
/// stray key inside a scene element's `crop` object is rejected at deserialize
/// time, without altering the shared type other tools depend on.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneCrop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

// NOTE: no #[serde(flatten)] anywhere in this file — serde forbids combining
// `deny_unknown_fields` with `flatten` on the same struct. Each enum variant
// below repeats the 4 shared fields (transform/opacity/crop/keyframes) in its
// OWN struct body instead, so every variant stays a fully self-contained,
// closed schema.
/// One scene element — a filled rect, a filled ellipse, or a text overlay.
/// Each carries an optional placement transform, an opacity, an optional crop,
/// and optional per-property keyframe animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SceneElement {
    Rect {
        fill: String,
        #[serde(default)]
        transform: Option<SceneTransform>,
        #[serde(default = "default_opacity_one")]
        opacity: f32,
        #[serde(default)]
        crop: Option<SceneCrop>,
        #[serde(default)]
        keyframes: Option<SceneKeyframeTracks>,
    },
    Ellipse {
        fill: String,
        #[serde(default)]
        transform: Option<SceneTransform>,
        #[serde(default = "default_opacity_one")]
        opacity: f32,
        #[serde(default)]
        crop: Option<SceneCrop>,
        #[serde(default)]
        keyframes: Option<SceneKeyframeTracks>,
    },
    Text {
        content: String,
        #[serde(default)]
        font_family: Option<String>,
        #[serde(default)]
        font_size: Option<f32>,
        #[serde(default)]
        fill: Option<String>,
        #[serde(default)]
        bold: Option<bool>,
        #[serde(default)]
        italic: Option<bool>,
        #[serde(default)]
        align: Option<TextAlign>,
        #[serde(default)]
        wrap_width: Option<f32>,
        #[serde(default)]
        transform: Option<SceneTransform>,
        #[serde(default = "default_opacity_one")]
        opacity: f32,
        #[serde(default)]
        crop: Option<SceneCrop>,
        #[serde(default)]
        keyframes: Option<SceneKeyframeTracks>,
    },
}

fn default_opacity_one() -> f32 {
    1.0
}

/// Per-property keyframe tracks on a scene element — the SAME wire shape
/// `set_keyframes` exposes, minus `volume` (a scene element has no audio).
/// Its own type (not the `set_keyframes` args reused byte-for-byte) so it can
/// carry `deny_unknown_fields` for SC-3; converts to `crate::tools::KeyframeArg`
/// before reusing `crate::tools::build_keyframe_track`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SceneKeyframeTracks {
    #[serde(default)]
    pub position: Vec<SceneKeyframe>,
    #[serde(default)]
    pub scale: Vec<SceneKeyframe>,
    #[serde(default)]
    pub rotation: Vec<SceneKeyframe>,
    #[serde(default)]
    pub opacity: Vec<SceneKeyframe>,
    #[serde(default)]
    pub crop: Vec<SceneKeyframe>,
}

/// One scene-element keyframe: a clip-relative frame in the scene fps timebase,
/// a property-shaped value array (arity validated by `build_keyframe_track`),
/// and an optional interpolation (omitted = `Smooth`, matching the model).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SceneKeyframe {
    pub frame: u32,
    pub value: Vec<f64>,
    #[serde(default)]
    pub interp: Option<Interpolation>,
}

/// Typed failure modes of [`SceneSpec::resolve`] — every one a recoverable
/// `Err` (never a panic, T-24-03), so untrusted LLM-authored input can never
/// crash the resolver.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SceneSpecError {
    #[error("scene width/height must each be 1..={MAX_SCENE_DIM}, got {0}x{1}")]
    InvalidDimensions(u32, u32),
    #[error("scene fps must be finite, > 0, and at most 240, got {0}")]
    InvalidFps(f64),
    #[error("durationSeconds must be > 0 and at most {MAX_SCENE_DURATION_SECONDS}, got {0}")]
    InvalidDuration(f64),
    #[error("elements: at most {MAX_SCENE_ELEMENTS} entries, got {0}")]
    TooManyElements(usize),
    #[error("invalid fill color: {0}")]
    InvalidFill(String),
    #[error("invalid keyframes: {0}")]
    InvalidKeyframes(String),
    #[error("invalid text element: {0}")]
    InvalidText(String),
    #[error("invalid transform/crop: {0}")]
    InvalidGeometry(String),
    #[error(
        "scene raster budget exceeded: {0} px total across background + elements, \
         max {MAX_SCENE_TOTAL_RASTER_PIXELS}"
    )]
    RasterBudgetExceeded(u64),
}

// ---------------------------------------------------------------------------
// Resolved output — what the caller (crates/app-core, a separate crate) receives:
// fully parsed colors + a ready-to-`.sample_at()` ephemeral Clip per element.
// ---------------------------------------------------------------------------

/// A fully resolved scene: dimensions/fps/duration filled from defaults and
/// validated, colors parsed, and every element backed by an ephemeral Clip.
#[derive(Debug, Clone)]
pub struct ResolvedSceneSpec {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// `0` when `require_duration` was false (image — a single tick).
    pub duration_us: i64,
    pub background: ResolvedBackground,
    /// Already ephemeral-Clip-backed, ready to `.sample_at()`.
    pub elements: Vec<ResolvedElement>,
}

/// The background with its color string(s) parsed to RGBA.
#[derive(Debug, Clone)]
pub enum ResolvedBackground {
    Solid { rgba: [u8; 4] },
    Gradient { from: [u8; 4], to: [u8; 4], angle_deg: f64 },
}

/// One scene element, FULLY resolved: `clip` is the ephemeral, never-persisted
/// Clip vehicle (transform/opacity/crop/keyframes baked in, `.text` populated
/// for a Text element) — the caller (a DIFFERENT crate) calls ONLY
/// `clip.sample_at(t, fps)` and matches on `kind` for the shape fill color; it
/// never needs `crate::tools`/`crate::command` internals (pub(crate)-only,
/// invisible outside this crate).
#[derive(Debug, Clone)]
pub struct ResolvedElement {
    pub kind: ResolvedElementKind,
    pub clip: crate::model::Clip,
}

/// The element's shape + parsed fill (Text carries its payload on `clip.text`).
#[derive(Debug, Clone)]
pub enum ResolvedElementKind {
    Rect { fill: [u8; 4] },
    Ellipse { fill: [u8; 4] },
    Text,
}

impl SceneSpec {
    /// Validate + default-fill + fully resolve (colors parsed, ephemeral Clips
    /// built) in ONE pass — the ONLY entry point `crates/app-core` calls.
    /// `default_*` = the ACTIVE PROJECT's own settings (Open Question 1,
    /// DECIDED: default when omitted, mirrors `resolve_export_plan`'s D-07).
    /// `require_duration`: true for `generate_video` (durationSeconds
    /// mandatory), false for `generate_image` (ignored, single tick,
    /// `duration_us = 0`).
    pub fn resolve(
        &self,
        default_width: u32,
        default_height: u32,
        default_fps: f64,
        require_duration: bool,
    ) -> Result<ResolvedSceneSpec, SceneSpecError> {
        // 1. width/height: default when omitted; reject outside 1..=MAX_SCENE_DIM.
        let width = self.width.unwrap_or(default_width);
        let height = self.height.unwrap_or(default_height);
        if !(1..=MAX_SCENE_DIM).contains(&width) || !(1..=MAX_SCENE_DIM).contains(&height) {
            return Err(SceneSpecError::InvalidDimensions(width, height));
        }

        // 2. fps: default when omitted; reject non-finite/<=0/>240 (checked even
        //    for generate_image, where it is otherwise unused, for consistency).
        let fps = self.fps.unwrap_or(default_fps);
        if !fps.is_finite() || fps <= 0.0 || fps > 240.0 {
            return Err(SceneSpecError::InvalidFps(fps));
        }

        // 3. element-count cap.
        if self.elements.len() > MAX_SCENE_ELEMENTS {
            return Err(SceneSpecError::TooManyElements(self.elements.len()));
        }

        // 4. duration: mandatory (and capped) only for generate_video.
        let duration_us: i64 = if require_duration {
            match self.duration_seconds {
                Some(s) if s.is_finite() && s > 0.0 && s <= MAX_SCENE_DURATION_SECONDS => {
                    (s * 1_000_000.0).round() as i64
                }
                other => return Err(SceneSpecError::InvalidDuration(other.unwrap_or(0.0))),
            }
        } else {
            0
        };

        // 5. background -> parsed RGBA (the ONE color grammar, via parse_fill).
        let background = match &self.background {
            SceneBackground::Solid { color } => ResolvedBackground::Solid {
                rgba: parse_scene_fill(color)?,
            },
            SceneBackground::Gradient {
                from,
                to,
                angle_deg,
            } => {
                // WR-02: a JSON number that overflows to +/-inf (e.g. `1e400`)
                // deserializes fine, then `angle_deg.to_radians().cos()/.sin()`
                // become NaN and the whole gradient silently renders all-zero.
                // Reject non-finite here, matching `check_transform_finite`.
                if !angle_deg.is_finite() {
                    return Err(SceneSpecError::InvalidGeometry(
                        "gradient angleDeg must be finite".into(),
                    ));
                }
                ResolvedBackground::Gradient {
                    from: parse_scene_fill(from)?,
                    to: parse_scene_fill(to)?,
                    angle_deg: *angle_deg,
                }
            }
        };

        // 5b. CR-02: AGGREGATE raster-memory gate. `render_scene_frame` holds
        //     every element's `Frame` (plus the full-canvas background) alive at
        //     once, so bound the SUM of their pixel footprints — derived here from
        //     each element's PEAK declared dest dims (transform scale, including
        //     any scale keyframe, × canvas), each clamped to the per-element
        //     ceiling the engine enforces — BEFORE any Frame is ever allocated.
        let mut total_px: u64 =
            (width as u64 * height as u64).min(MAX_SCENE_RASTER_PIXELS_PER_ELEMENT);
        for el in &self.elements {
            total_px = total_px.saturating_add(element_raster_footprint(el, width, height));
            if total_px > MAX_SCENE_TOTAL_RASTER_PIXELS {
                return Err(SceneSpecError::RasterBudgetExceeded(total_px));
            }
        }

        // 6. elements -> fully resolved, ephemeral-Clip-backed.
        let mut elements = Vec::with_capacity(self.elements.len());
        for (i, el) in self.elements.iter().enumerate() {
            elements.push(resolve_element(i, el, duration_us)?);
        }

        // 7. hand back a render-ready spec.
        Ok(ResolvedSceneSpec {
            width,
            height,
            fps,
            duration_us,
            background,
            elements,
        })
    }
}

impl SceneKeyframeTracks {
    /// Cap-check every non-empty track against `MAX_KEYFRAMES_PER_TRACK`, then
    /// convert each via `crate::tools::build_keyframe_track` (mapping each
    /// [`SceneKeyframe`] -> `crate::tools::KeyframeArg`), merging the typed
    /// variants into one [`crate::model::KeyframeTracks`].
    pub(crate) fn to_keyframe_tracks(
        &self,
    ) -> Result<crate::model::KeyframeTracks, SceneSpecError> {
        use crate::model::{KeyframeTrackData, KeyframeTracks, MAX_KEYFRAMES_PER_TRACK};
        use crate::tools::{build_keyframe_track, KeyframeArg};

        let mut out = KeyframeTracks::default();
        // (property name, this element's track) in KeyframeTracks field order —
        // volume is intentionally absent: scene elements never animate volume.
        let named: [(&str, &Vec<SceneKeyframe>); 5] = [
            ("position", &self.position),
            ("scale", &self.scale),
            ("rotation", &self.rotation),
            ("opacity", &self.opacity),
            ("crop", &self.crop),
        ];

        for (property, kfs) in named {
            if kfs.is_empty() {
                continue;
            }
            // T-24-02: cap BEFORE building/allocating the typed track.
            if kfs.len() > MAX_KEYFRAMES_PER_TRACK {
                return Err(SceneSpecError::InvalidKeyframes(format!(
                    "property `{property}` has {} keyframes, exceeds the \
                     {MAX_KEYFRAMES_PER_TRACK} cap",
                    kfs.len()
                )));
            }
            let args: Vec<KeyframeArg> = kfs
                .iter()
                .map(|k| KeyframeArg {
                    frame: k.frame,
                    value: k.value.clone(),
                    interp: k.interp,
                })
                .collect();
            // Reuse the ONE arity-typed converter (zero new interp/arity code).
            let data = build_keyframe_track(property, &args)
                .map_err(|e| SceneSpecError::InvalidKeyframes(e.to_string()))?;
            match data {
                KeyframeTrackData::Position(v) => out.position = v,
                KeyframeTrackData::Scale(v) => out.scale = v,
                KeyframeTrackData::Rotation(v) => out.rotation = v,
                KeyframeTrackData::Opacity(v) => out.opacity = v,
                KeyframeTrackData::Crop(v) => out.crop = v,
                KeyframeTrackData::Volume(_) => {
                    unreachable!("scene elements never request the volume property")
                }
            }
        }
        Ok(out)
    }
}

/// Parse a scene fill color via the ONE shared grammar (`crate::tools::parse_fill`),
/// re-typing any failure as [`SceneSpecError::InvalidFill`] carrying the raw string.
fn parse_scene_fill(s: &str) -> Result<[u8; 4], SceneSpecError> {
    crate::tools::parse_fill(s).map_err(|_| SceneSpecError::InvalidFill(s.to_string()))
}

/// Reject a transform with any non-finite component (defense-in-depth, T-24-04).
fn check_transform_finite(t: &ClipTransform) -> Result<(), SceneSpecError> {
    if !t.position.0.is_finite()
        || !t.position.1.is_finite()
        || !t.scale.0.is_finite()
        || !t.scale.1.is_finite()
        || !t.rotation_deg.is_finite()
    {
        return Err(SceneSpecError::InvalidGeometry(
            "transform components (position/scale/rotationDeg) must be finite".into(),
        ));
    }
    Ok(())
}

/// Reject a non-finite element `opacity` (WR-02) — an overflowing/`inf` JSON
/// number would otherwise flow straight onto `Clip.opacity`.
fn check_opacity_finite(opacity: f32) -> Result<(), SceneSpecError> {
    if !opacity.is_finite() {
        return Err(SceneSpecError::InvalidGeometry(
            "element opacity must be finite".into(),
        ));
    }
    Ok(())
}

/// Convert a scene element's optional `transform` (the SC-3-closed
/// [`SceneTransform`] wrapper) into the shared [`ClipTransform`]: finite-checked
/// when present (defense-in-depth, T-24-04), or `default_when_omitted` when
/// absent — full-canvas identity for a shape, the auto-fit sentinel for text.
fn resolve_transform(
    transform: &Option<SceneTransform>,
    default_when_omitted: ClipTransform,
) -> Result<ClipTransform, SceneSpecError> {
    match transform {
        Some(t) => {
            let ct = ClipTransform::from(*t);
            check_transform_finite(&ct)?;
            Ok(ct)
        }
        None => Ok(default_when_omitted),
    }
}

/// Estimate one element's PEAK simultaneous raster footprint in pixels (CR-02),
/// for the aggregate budget checked in [`SceneSpec::resolve`]. Derived — without
/// any raster work — from the element's declared dest dims: its PEAK scale (the
/// static `transform.scale` and every `scale` keyframe value) × canvas dims,
/// clamped to the per-element ceiling the engine's `clamp_dims` enforces. A
/// shape with an OMITTED transform is full-canvas (scale 1); an omitted TEXT
/// transform auto-fits to its natural (small, individually-capped) glyph size,
/// counted as ~0 here (its footprint is bounded by the engine per-frame clamp,
/// not by canvas × scale). Non-finite scales saturate to the ceiling — never a
/// panic — and are rejected downstream by `check_transform_finite`.
fn element_raster_footprint(el: &SceneElement, canvas_w: u32, canvas_h: u32) -> u64 {
    let (transform, keyframes, is_text) = match el {
        SceneElement::Rect {
            transform,
            keyframes,
            ..
        } => (transform, keyframes, false),
        SceneElement::Ellipse {
            transform,
            keyframes,
            ..
        } => (transform, keyframes, false),
        SceneElement::Text {
            transform,
            keyframes,
            ..
        } => (transform, keyframes, true),
    };
    // Base scale: explicit transform => its scale; omitted shape => full-canvas
    // (1,1); omitted text => auto-fit sentinel (0,0), i.e. ~0 aggregate weight.
    let (mut sx, mut sy) = match transform {
        Some(t) => (t.scale.0.max(0.0) as f64, t.scale.1.max(0.0) as f64),
        None if is_text => (0.0, 0.0),
        None => (1.0, 1.0),
    };
    if let Some(k) = keyframes {
        for kf in &k.scale {
            if kf.value.len() == 2 {
                sx = sx.max(kf.value[0].max(0.0));
                sy = sy.max(kf.value[1].max(0.0));
            }
        }
    }
    // `x as u64` saturates on +inf/overflow and yields 0 on NaN — both safe: the
    // subsequent `.min(ceiling)` bounds the result and no slice/panic is reached.
    let pw = ((canvas_w as f64) * sx).round().max(1.0) as u64;
    let ph = ((canvas_h as f64) * sy).round().max(1.0) as u64;
    pw.saturating_mul(ph)
        .min(MAX_SCENE_RASTER_PIXELS_PER_ELEMENT)
}

/// Resolve an optional crop: `None` -> no crop; `Some` -> finite-checked, then
/// CLAMPED to `[0,1]` per inset (WR-01, mirroring `Command::SetClipCrop`), then
/// rejected if `left+right >= 1` or `top+bottom >= 1` on the CLAMPED values — so
/// `resolve()` never accepts a crop the compositor would silently no-op-draw.
fn resolve_crop(crop: &Option<SceneCrop>) -> Result<ClipCrop, SceneSpecError> {
    match crop {
        None => Ok(ClipCrop::default()),
        Some(c) => {
            if !c.left.is_finite()
                || !c.top.is_finite()
                || !c.right.is_finite()
                || !c.bottom.is_finite()
            {
                return Err(SceneSpecError::InvalidGeometry(
                    "crop insets must be finite".into(),
                ));
            }
            // WR-01: clamp FIRST (the compositor's `layer_params_bytes` clamps
            // each inset to [0,1] before use), THEN check the sum on the clamped
            // values. Checking the raw sum would accept e.g. left=2.0,right=-5.0
            // (raw sum -3.0 < 1) which the compositor clamps to left=1.0,right=0.0
            // => leaves no source => the element silently vanishes with no error.
            let clamp = |v: f32| v.max(0.0).min(1.0);
            let clamped = ClipCrop {
                left: clamp(c.left),
                top: clamp(c.top),
                right: clamp(c.right),
                bottom: clamp(c.bottom),
            };
            if clamped.left + clamped.right >= 1.0 || clamped.top + clamped.bottom >= 1.0 {
                return Err(SceneSpecError::InvalidGeometry(format!(
                    "crop leaves no source: left+right and top+bottom must each \
                     be < 1 after clamping to [0,1], got {}+{} horizontal, \
                     {}+{} vertical",
                    clamped.left, clamped.right, clamped.top, clamped.bottom
                )));
            }
            Ok(clamped)
        }
    }
}

/// Build the ephemeral, never-persisted [`crate::model::Clip`] vehicle for a
/// resolved element — `id`/`media_id` are inert sentinels; only
/// transform/opacity/crop/keyframes(/text) carry real scene-authored data.
#[allow(clippy::too_many_arguments)]
fn build_scene_clip(
    index: usize,
    duration_us: i64,
    transform: ClipTransform,
    opacity: f32,
    crop: ClipCrop,
    keyframes: crate::model::KeyframeTracks,
    text: Option<crate::model::TextPayload>,
) -> crate::model::Clip {
    crate::model::Clip {
        id: format!("scene-el-{index}"),
        media_id: String::new(),
        start_us: 0,
        in_us: 0,
        out_us: duration_us.max(1),
        volume: 1.0,
        audio_detached: false,
        transform,
        opacity,
        crop,
        keyframes,
        text,
        // Scene elements are synthetic straight-alpha layers (Phase 28).
        alpha_mode: crate::model::AlphaMode::default(),
        // A scene element is an ephemeral 1:1 vehicle — scene timing is
        // authored in its own keyframes, never via a clip retime (quick task
        // 260730-x2t). Exhaustive on purpose: a new Clip field must fail the
        // build here rather than silently default-fill.
        retime: None,
    }
}

/// Resolve one element (index `i`) into a [`ResolvedElement`].
fn resolve_element(
    i: usize,
    el: &SceneElement,
    duration_us: i64,
) -> Result<ResolvedElement, SceneSpecError> {
    use crate::model::KeyframeTracks;

    match el {
        SceneElement::Rect {
            fill,
            transform,
            opacity,
            crop,
            keyframes,
        }
        | SceneElement::Ellipse {
            fill,
            transform,
            opacity,
            crop,
            keyframes,
        } => {
            let rgba = parse_scene_fill(fill)?;
            check_opacity_finite(*opacity)?;
            // Rect/Ellipse omitted transform => full-canvas identity.
            let clip_transform = resolve_transform(transform, ClipTransform::default())?;
            let clip_crop = resolve_crop(crop)?;
            let tracks = match keyframes {
                Some(k) => k.to_keyframe_tracks()?,
                None => KeyframeTracks::default(),
            };
            let clip = build_scene_clip(
                i,
                duration_us,
                clip_transform,
                *opacity,
                clip_crop,
                tracks,
                None,
            );
            let kind = match el {
                SceneElement::Ellipse { .. } => ResolvedElementKind::Ellipse { fill: rgba },
                _ => ResolvedElementKind::Rect { fill: rgba },
            };
            Ok(ResolvedElement { kind, clip })
        }
        SceneElement::Text {
            content,
            font_family,
            font_size,
            fill,
            bold,
            italic,
            align,
            wrap_width,
            transform,
            opacity,
            crop,
            keyframes,
        } => {
            check_opacity_finite(*opacity)?;
            // BLOCKER-1: an omitted Text transform defaults to the shipped
            // auto-fit sentinel (scale (0,0)), NOT full-canvas identity — so
            // rasterize_text_layer fits the glyph to its natural size.
            let clip_transform =
                resolve_transform(transform, crate::tools::TEXT_AUTOFIT_TRANSFORM)?;
            let clip_crop = resolve_crop(crop)?;
            let tracks = match keyframes {
                Some(k) => k.to_keyframe_tracks()?,
                None => KeyframeTracks::default(),
            };

            // Build the TextPayload from the flat style fields, defaulting each
            // omitted field to TextStyle::default(), then validate via the ONE
            // shared text validator (bundled font, size/wrap ranges, length cap).
            let mut style = crate::model::TextStyle::default();
            if let Some(f) = font_family {
                style.font_family = f.clone();
            }
            if let Some(s) = font_size {
                style.font_size = *s;
            }
            if let Some(f) = fill {
                style.fill = parse_scene_fill(f)?;
            }
            if let Some(b) = bold {
                style.bold = *b;
            }
            if let Some(it) = italic {
                style.italic = *it;
            }
            if let Some(a) = align {
                style.align = *a;
            }
            if let Some(w) = wrap_width {
                style.wrap_width = Some(*w);
            }
            let payload = crate::model::TextPayload {
                content: content.clone(),
                style,
                caption_group_id: None,
            };
            crate::command::validate_text_payload(&payload)
                .map_err(|e| SceneSpecError::InvalidText(e.to_string()))?;

            let clip = build_scene_clip(
                i,
                duration_us,
                clip_transform,
                *opacity,
                clip_crop,
                tracks,
                Some(payload),
            );
            Ok(ResolvedElement {
                kind: ResolvedElementKind::Text,
                clip,
            })
        }
    }
}
