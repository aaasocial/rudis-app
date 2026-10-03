//! Undoable commands and change patches.
//!
//! Each [`Command`] variant validates against the current [`Project`], applies
//! its mutation, and returns BOTH a [`Patch`] (what changed — emitted to the
//! renderer over IPC) and its exact inverse [`Command`] (pushed on the undo
//! stack). Inverses are computed at apply time because some need prior state
//! (e.g. the inverse of `MoveClip` must remember the OLD start; the inverse of
//! `RemoveClip` must remember the whole removed clip and its track).

use serde::{Deserialize, Serialize};

use crate::model::{
    track_accepts_media, AlphaMode, Clip, ClipCrop, ClipTransform, CropValue, Keyframe,
    KeyframeTrackData, KeyframeTracks, MediaBinItem, Project, TextPayload, TextStyle,
    TextStylePatch, Track, TrackKind, MAX_KEYFRAMES_PER_TRACK,
};
use crate::CoreError;

/// Every mutation of the project. Phase 2 implements a small REAL set that is
/// enough to prove the model round-trip + undo/redo; later phases add
/// variants (split, ripple, volume, ...).
///
/// Serialized adjacently tagged (`{"type": "add_clip", "data": {...}}`) so the
/// renderer can construct commands as plain typed JSON over IPC.
///
/// (`Eq` dropped in Phase 3: `AddMediaBinItem` carries `MediaBinItem::fps`,
/// an `f64`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Command {
    /// Register a media file in the project bin.
    AddMediaBinItem(MediaBinItem),
    /// Remove a media bin item by id (must not be referenced by any clip).
    /// Exists primarily as the inverse of `AddMediaBinItem` so that command
    /// is undoable; it is also a legitimate user-facing command.
    RemoveMediaBinItem { id: String },
    /// Place a clip on the track at index `track` (appended at the end of
    /// the track's clip list — later clips win on overlap, Phase 5).
    AddClip { track: usize, clip: Clip },
    /// Move an existing clip to a new timeline position.
    MoveClip { id: String, new_start_us: i64 },
    /// Move an existing clip to a DIFFERENT track of the same media-kind
    /// compatibility class (cross-track retarget). Deliberately a SEPARATE
    /// variant from `MoveClip` (same-track reposition only) so every one of
    /// `MoveClip`'s existing callers (agent tools, sync-audio) stays
    /// byte-unchanged.
    ///
    /// `target_index`: `None` for a normal forward (user-driven) move --
    /// the clip is appended at the end of the target track's clip list.
    /// `Some(i)` is used ONLY when this command is itself constructed as
    /// an undo-inverse (see the apply arm below), to restore the clip to
    /// its EXACT prior position-within-track. This makes the inverse of
    /// `MoveClipToTrack` another `MoveClipToTrack` -- self-symmetric, the
    /// same precedent `MoveClip`'s own inverse already sets -- rather than
    /// `RestoreClip`, whose duplicate-id guard assumes the clip is wholly
    /// ABSENT from the project, which is false for a cross-track move (the
    /// clip is still present, just on `target_track`).
    MoveClipToTrack {
        id: String,
        target_track: usize,
        new_start_us: i64,
        #[serde(default)]
        target_index: Option<usize>,
    },
    /// Remove a clip by id.
    RemoveClip { id: String },
    /// Re-insert a clip at an exact position within a track's clip list.
    /// Exists as the inverse of `RemoveClip`: clip ORDER within a track is
    /// semantically meaningful (Phase 5 overlap priority: last wins), so
    /// undoing a removal must restore the clip at its ORIGINAL index, not
    /// append it at the end. Not sent by the renderer.
    RestoreClip {
        track: usize,
        index: usize,
        clip: Clip,
    },

    // ------------------------------------------------------------------
    // Phase 6: edit operations + audio
    // ------------------------------------------------------------------
    /// Trim a clip's source range. LEFT-EDGE semantics: changing `in_us`
    /// moves `start_us` by the SAME delta, so the frames already on the
    /// timeline stay put (non-ripple trim). Right-edge trims change `out_us`
    /// only. Inverse is another `TrimClip` with the prior in/out — the same
    /// delta math restores the prior `start_us` exactly.
    TrimClip {
        id: String,
        new_in_us: i64,
        new_out_us: i64,
        /// Inverse-only exact-restore payload (RestoreClip/RestoreSplit
        /// whole-state-carry precedent). Forward callers (tools, UI, MCP)
        /// leave this `None` — the apply arm derives the content-preserving
        /// keyframe remap (Phase 19, COMP-04) itself. `Some(tracks)` is written
        /// ONLY by apply when constructing the inverse, so undo restores the
        /// exact prior tracks (dropped keys return, synthesized keys vanish)
        /// despite the lossy left-edge remap. `#[serde(default, skip)]` keeps
        /// existing serialized/IPC forms working unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restore_keyframes: Option<KeyframeTracks>,
        /// Inverse-only exact-restore payload for the clip's time remap — the
        /// `restore_keyframes` twin (quick task 260730-x2t). A left-edge trim
        /// remaps the speed curve through
        /// [`crate::model::RetimeCurve::shifted_for_left_cut`], which is exact
        /// for `Hold`/`Linear` but re-eases a `Smooth` segment; carrying the
        /// prior curve makes UNDO lossless regardless.
        ///
        /// `Some(None)` means "the clip had NO retime" and is a real restore
        /// instruction, distinct from the outer `None` ("forward caller —
        /// derive the remap yourself"). Routed through [`validate_retime`] and
        /// cache-rebuilt BEFORE any mutation (threat T-x2t-05: this field
        /// rides the general `dispatch_command` IPC surface, so a hand-crafted
        /// `TrimClip` must not be able to inject an unvalidated curve or a
        /// hostile `timeline_len_us`).
        ///
        /// Serde goes through [`double_option`] so the `Some(None)` /`None`
        /// distinction SURVIVES a JSON round-trip — with plain
        /// `Option<Option<_>>`, a serialized `null` deserializes back to the
        /// OUTER `None` and a "clear the retime" undo would silently become
        /// "derive the remap yourself".
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        restore_retime: Option<Option<crate::model::Retime>>,
    },
    /// Split the clip active at `at_position_us` into two contiguous clips:
    /// the original keeps `(in_us, Sp)` where `Sp = in_us + (at - start)`;
    /// a NEW right clip `(Sp, old_out)` starts at `at_position_us` with the
    /// same media/volume/detached state. The right clip's id is DETERMINISTIC
    /// (`{id}~s{n}`, n = smallest free suffix — a state-derived counter, not
    /// time/random, so tests and redo are stable). Inverse merges.
    SplitClip { id: String, at_position_us: i64 },
    /// Merge a split pair back: `left.out_us = right.out_us`, remove the
    /// right clip. Exists as the inverse of `SplitClip`; also legal from the
    /// wire for adjacent same-media clips produced by a split.
    MergeSplit { left_id: String, right_id: String },
    /// Exact re-split: restore `left.out_us` and re-insert the removed right
    /// clip at its original track/index. Inverse of `MergeSplit` (so redo
    /// after undo-of-split reproduces the EXACT state, same id included).
    /// Not sent by the renderer.
    RestoreSplit {
        left_id: String,
        left_out_us: i64,
        /// The left half's `Option<Retime>` at MERGE time — restored verbatim
        /// (validated + cache-rebuilt) on re-split, so undo of a
        /// split-then-retime is lossless (quick task 260730-x2t). Carried
        /// alongside `left_out_us`, symmetric with it.
        ///
        /// `Option<Option<_>>` via [`double_option`], exactly like
        /// [`Command::TrimClip::restore_retime`] (IN-03). `MergeSplit` always
        /// populates the outer `Some`, so:
        ///
        /// * `Some(Some(r))` — restore this exact retime;
        /// * `Some(None)` — the left half genuinely had NO retime; clear it;
        /// * `None` (the ABSENT field) — "no instruction": LEAVE the left half's
        ///   retime alone.
        ///
        /// The third case is what the flat `Option<Retime>` could not express:
        /// a pre-retime serialized `RestoreSplit`, or a hand-crafted one,
        /// silently WIPED an existing retime — the same `Some(None)` vs `None`
        /// collapse `restore_retime` grew `double_option` to avoid.
        /// `#[serde(default)]` keeps every pre-retime serialized/IPC form
        /// loadable, now with the correct "leave it alone" meaning.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        left_retime: Option<Option<crate::model::Retime>>,
        track: usize,
        index: usize,
        right: Clip,
    },
    /// Duplicate a clip: NEW deterministic id (`{id}~d{n}`), same
    /// media/in/out/volume/detached, placed at the original's timeline end
    /// (appended to the same track — later clips win on overlap). Inverse
    /// removes the duplicate.
    DuplicateClip { id: String },
    /// Set a clip's audio gain (clamped >= 0). Inverse restores the prior
    /// volume.
    SetClipVolume { id: String, volume: f32 },
    /// Detach a VIDEO clip's audio: sets `audio_detached = true` and adds an
    /// independently-editable clip on the first AUDIO track (deterministic id
    /// `{id}~a{n}`, same media/in/out, start = video start, volume = video
    /// volume). Rejected if already detached, if the media has no audio, or
    /// if the clip is not on a video track. Inverse reattaches.
    DetachAudio { clip_id: String },
    /// Remove the detached audio clip and clear the video clip's
    /// `audio_detached` flag. Inverse of `DetachAudio`.
    ReattachAudio { video_id: String, audio_id: String },
    /// Exact re-detach: set the flag and re-insert the removed audio clip at
    /// its original track/index. Inverse of `ReattachAudio` (redo-exactness).
    /// Not sent by the renderer.
    RestoreDetachedAudio {
        video_id: String,
        track: usize,
        index: usize,
        clip: Clip,
    },

    // ------------------------------------------------------------------
    // Phase 13 (CANV-01): canvas annotations. NOT Tool variants — the
    // frozen 12-variant timeline-edit tool spine (SC-5) is untouched; the
    // canvas has no tool-spine wrapper (the agent only READS canvas here).
    // ------------------------------------------------------------------
    /// Append a validated annotation to `project.canvas.annotations`.
    /// Coordinates are CLAMPED into [0,1] and overlong label text is
    /// TRUNCATED before storage (never trust frontend math); structural
    /// violations (empty stroke, sub-3-point lasso, oversized point list)
    /// are REJECTED before any mutation. Inverse removes it.
    AddAnnotation(crate::canvas::Annotation),
    /// Remove an annotation by id. Inverse re-adds the exact original.
    RemoveAnnotation { id: String },
    /// Remove annotations. `space: None` clears ALL (old behavior); `Some(s)`
    /// clears only marks whose `space == s`, leaving the other surface's marks
    /// untouched (Phase 14.2 open-item-1 resolution). Inverse restores the
    /// exact removed subset in original order.
    ClearCanvas {
        #[serde(default)]
        space: Option<crate::canvas::AnnotationSpace>,
    },
    /// Re-insert a whole annotation set verbatim (already-validated data —
    /// no re-clamping, redo-exactness). Exists as the inverse of
    /// `ClearCanvas`; not sent by the renderer.
    RestoreCanvas {
        annotations: Vec<crate::canvas::Annotation>,
        /// The exact scope `ClearCanvas` was dispatched with — echoed back
        /// verbatim as this restore's own inverse, so an EMPTY (or otherwise
        /// non-homogeneous) restore round-trips EXACTLY instead of escalating
        /// to a broader clear on redo.
        #[serde(default)]
        cleared_space: Option<crate::canvas::AnnotationSpace>,
    },

    // ------------------------------------------------------------------
    // Phase 14.3 (CANV-01): annotation reposition / re-edit. The FIRST
    // in-place mutations of an existing annotation (Add/Remove/Clear only
    // ever appended or dropped whole marks before).
    // ------------------------------------------------------------------
    /// Phase 14.3 (D-07/D-08): translate an existing annotation's shape by a
    /// normalized delta. EVERY point-bearing field is translated by (dx,dy) and
    /// CLAMPED into [0,1] (same clamp-don't-reject discipline as AddAnnotation).
    /// The inverse is NOT a negated delta (per-point clamping is non-linear — the
    /// 14.2 CR-01 lesson): it is a SetAnnotationShape carrying the EXACT pre-move
    /// shape captured before any mutation.
    MoveAnnotation { id: String, dx: f64, dy: f64 },
    /// Phase 14.3: replace an existing annotation's shape verbatim (id/space/
    /// linked_range_us untouched). Primarily MoveAnnotation's exact inverse
    /// (the AddMediaBinItem/RemoveMediaBinItem "inverse but also legitimate
    /// user-facing command" precedent); ALSO renderer-dispatched for text
    /// re-edit (D-05). Coordinates re-clamped / text re-truncated via
    /// normalize_and_validate_shape (defense in depth); id must already exist.
    /// Its own inverse is another SetAnnotationShape carrying the prior shape
    /// (self-inverse pair, like SetClipVolume) — undo AND redo stay exact.
    SetAnnotationShape {
        id: String,
        shape: crate::canvas::AnnotationShape,
    },

    // ------------------------------------------------------------------
    // Phase 18 (COMP-01): project timebase.
    // ------------------------------------------------------------------
    /// Set the project's output timebase (fps + resolution). Self-inverse
    /// pair (SetClipVolume pattern): the inverse carries the PRIOR settings.
    /// Validation at apply (T-18-02, all-or-nothing): fps must be finite and
    /// in (0, 240] (18-REVIEW MED-01 ceiling — the fps twin of the dimension
    /// bound); width/height each in 1..=7680 (8K ceiling) — the
    /// untrusted-float gate for Plan 04's `set_project_settings` tool args.
    SetProjectSettings { fps: f64, width: u32, height: u32 },

    // ------------------------------------------------------------------
    // Phase 18 (TOOL-03): per-clip visual properties. Three PER-FIELD
    // commands (18-RESEARCH Open Question 3: per-field over one omnibus —
    // each undo step stays a single legible property change and each field
    // keeps its own validation rule), all self-inverse (SetClipVolume
    // pattern: capture the prior value as the inverse).
    // ------------------------------------------------------------------
    /// Set a clip's placement transform. Rejected if any component is
    /// non-finite (T-18-01, the `ensure_finite_point` precedent). Inverse
    /// restores the prior transform.
    SetClipTransform { id: String, transform: ClipTransform },
    /// Set a clip's layer opacity, clamped into [0,1] (max-then-min, so
    /// NaN -> 0.0 — the SetClipVolume clamp discipline). Inverse restores
    /// the prior opacity.
    SetClipOpacity { id: String, opacity: f32 },
    /// Set a clip's source-crop insets. Each inset is CLAMPED into [0,1]
    /// (max-then-min, NaN -> 0.0); a crop whose clamped `left+right >= 1`
    /// or `top+bottom >= 1` (no source pixels left) is REJECTED before any
    /// mutation. Inverse restores the prior crop.
    SetClipCrop { id: String, crop: ClipCrop },

    // ------------------------------------------------------------------
    // Phase 28 (OVL-01): per-clip alpha interpretation.
    // ------------------------------------------------------------------
    /// Set a clip's [`AlphaMode`] (Straight/Premultiplied). A two-variant
    /// enum needs no validation/clamping. Self-inverse: the inverse restores
    /// the prior mode (the SetClipOpacity template).
    SetClipAlphaMode { id: String, alpha_mode: AlphaMode },

    // ------------------------------------------------------------------
    // Quick task 260730-x2t: time remap (speed / speed ramps). ONE new
    // mutation primitive; BOTH agent halves (`set_clip_properties.speed`
    // for a constant and `set_keyframes property:"speed"` for a ramp)
    // resolve into it (the Tool->Command layer already does N:1).
    // ------------------------------------------------------------------
    /// Set (or clear) a clip's time remap. Follows the codebase's
    /// one-command-per-property pattern (`SetClipTransform`/`SetClipOpacity`/
    /// `SetClipCrop`/`SetClipAlphaMode`/`SetKeyframes`): self-inverse, the
    /// inverse carrying the OLD `Option<Retime>`.
    ///
    /// Validate-then-apply (ZERO mutation on Err, threat T-x2t-01): speed
    /// bounds `[MIN_SPEED, MAX_SPEED]`, finiteness, key cap
    /// (`MAX_RETIME_KEYS`), stable sort + adjacent-duplicate reject — the
    /// `SetKeyframes` pipeline mirrored by [`validate_retime`].
    ///
    /// DERIVED FIELDS ARE ALWAYS REBUILT, never trusted from the wire:
    /// `timeline_len_us` is recomputed from the clip's CURRENT source span
    /// (RT-03 — a hostile or stale cached length is an occupancy bug that
    /// silently misplaces every later clip, threat T-x2t-08), and a
    /// degenerate carried `timebase_fps` (non-finite, `<= 0`, or above the
    /// project fps ceiling) falls back to `project.fps`. Because both are
    /// pure functions of `(curve, fps, span)`, rebuilding them is EXACT for
    /// the legitimate undo path — a round-trip restores the identical `Clip`.
    ///
    /// NORMALIZATION: a `Constant` of exactly 1.0 (and an EMPTY ramp) become
    /// `None`, so an un-retimed clip's JSON stays byte-identical to pre-retime
    /// output. A ramp whose keys all happen to be 1.0 is NOT normalized away —
    /// it is a real (if flat) authored curve.
    SetClipRetime {
        id: String,
        retime: Option<crate::model::Retime>,
    },

    // ------------------------------------------------------------------
    // Phase 19 (COMP-04): keyframe animation — the ONE new mutation
    // primitive of the phase.
    // ------------------------------------------------------------------
    /// FULL-TRACK-REPLACE one property's keyframe track on a clip (D-01):
    /// the `KeyframeTrackData` variant IS the property, so a property/value
    /// -shape mismatch is impossible by construction; an EMPTY array clears
    /// the track. Validate-then-apply (ZERO mutation on Err): length cap
    /// (`MAX_KEYFRAMES_PER_TRACK`, T-19-01), stable sort ascending by frame
    /// + adjacent-duplicate REJECT (D-08, T-19-03), per-property value rules
    /// MIRRORING the static setters (T-19-02): position/scale/rotation
    /// reject non-finite; opacity clamps [0,1] and volume [0,10]
    /// (max-then-min, NaN -> 0.0); crop insets clamp [0,1] then a
    /// no-source-left combination is rejected. Out-of-range frame numbers
    /// are ACCEPTED (the track rides along on trim/move; sampling clamps,
    /// D-05). Self-inverse (SetClipVolume pattern): the inverse carries the
    /// OLD same-property track.
    SetKeyframes {
        id: String,
        track: KeyframeTrackData,
    },

    // ------------------------------------------------------------------
    // Phase 20 (TEXT-01): text overlays as real clips. The ONLY two
    // genuinely-new commands the phase needs — every structural op
    // (trim/split/move/remove/keyframes) reuses the existing commands
    // unchanged (D-01's payoff).
    // ------------------------------------------------------------------
    /// Create a TEXT clip on the track at `track_index`. Validates: track in
    /// range, unique id, `in_us >= 0` & `out_us > in_us`, and the carried
    /// `text` payload (present; content non-empty & length-capped; font_family
    /// in the bundled set; font_size finite & > 0; wrap_width finite if `Some`).
    /// NO media_bin referential check — a text clip's `media_id` is the empty
    /// sentinel (this is exactly why `AddClip` cannot be reused). A negative
    /// `start_us` is CLAMPED to 0 (the AddClip precedent). Inverse = `RemoveClip`.
    AddText { track_index: usize, clip: Clip },
    /// PARTIAL-MERGE style edit of an existing text clip (D-06): only the `Some`
    /// fields of `patch` overwrite; unpassed keys stay. Validate-then-apply
    /// (ZERO mutation on Err): the target must exist AND be a text clip
    /// (`text.is_some()`, else `NotATextClip`); the MERGED style is re-validated
    /// (font_size finite & > 0, wrap_width finite if Some, family in the bundled
    /// set) BEFORE the clip is touched. Inverse = `RestoreText` carrying the FULL
    /// prior [`TextPayload`] (simplest exact restore — round-trips under undo).
    UpdateText {
        clip_id: String,
        patch: TextStylePatch,
    },
    /// Replace a text clip's WHOLE payload verbatim. Primarily `UpdateText`'s
    /// exact inverse (the RestoreClip/RestoreDetachedAudio "inverse but also a
    /// legitimate command" precedent); ALSO the renderer-facing content-edit
    /// path. Self-inverse pair (like SetClipVolume): its own inverse is another
    /// `RestoreText` carrying the prior payload, so undo AND redo stay exact.
    /// The incoming payload is re-validated (defense in depth — it rides the
    /// general dispatch IPC surface; idempotent for a legitimate undo whose
    /// payload was already valid). Target must exist and be a text clip.
    RestoreText {
        clip_id: String,
        text: TextPayload,
    },

    // ------------------------------------------------------------------
    // Phase 18.1 (TL-06): track management.
    // ------------------------------------------------------------------
    /// Add a new, EMPTY track of `kind`. VIDEO inserts on top (index 0);
    /// AUDIO appends at the bottom — lowest-index lane = top compositing layer.
    /// Inverse is `RemoveTrack` at the new track's resulting index.
    AddTrack { kind: TrackKind },
    /// Remove the track at `index`, taking every clip it holds with it
    /// (cascade). Every track AFTER `index` shifts down by one.
    RemoveTrack { index: usize },
    /// Re-insert a whole removed track (kind + its full clip Vec) at its exact
    /// original index. Inverse of `RemoveTrack`. Not sent by the renderer.
    RestoreTrack { index: usize, track: Track },

    // ------------------------------------------------------------------
    // Phase 25 (LIB-01): media library folders.
    // ------------------------------------------------------------------
    /// Register a new, EMPTY virtual folder at `path` (a canonical, non-root,
    /// no-leading/trailing-slash string, e.g. "broll/city"). The IMMEDIATE
    /// PARENT must already be registered (root, or an existing media_folders
    /// entry) -- this Command does NOT mkdir -p; the Tool layer (Plan 25-04)
    /// synthesizes intermediate creates if ever needed. Inverse is
    /// `DeleteMediaFolder` at the same path (legal because nothing has been
    /// added to a just-created folder yet).
    CreateMediaFolder { path: String },
    /// Remove an EMPTY folder at `path` (no items with folder == path, no
    /// registry entry equal to or nested under path). Inverse re-creates it via
    /// `CreateMediaFolder`.
    DeleteMediaFolder { path: String },
    /// Move OR rename a folder: rewrite `old_path` (and every descendant
    /// registry entry / MediaBinItem.folder under it) to `new_path` by prefix
    /// substitution. Self-inverse (mirrors SetClipVolume/SetProjectSettings):
    /// swapping old_path/new_path reverses it exactly. Covers BOTH a same-parent
    /// rename and a cross-parent move -- path-addressing makes them the same
    /// primitive (25-RESEARCH.md Pitfall 4). Cycle-checked at apply time as
    /// defense-in-depth; the Tool layer checks this FIRST, before any scratch
    /// work.
    MoveMediaFolder { old_path: String, new_path: String },
    /// Move a MediaBinItem into `new_folder` ("" = root, or an existing
    /// registered folder). Inverse captures the item's OLD folder at apply time
    /// (mirrors TrimClip's restore-prior-state-at-apply-time pattern).
    MoveMediaItem { id: String, new_folder: String },
    /// Set (or clear, via `None`) a MediaBinItem's display-name override. NEVER
    /// touches the real file at `path` -- the core does zero I/O. Self-inverse:
    /// carries the OLD display_name.
    RenameMediaItem { id: String, display_name: Option<String> },
}

/// Per-annotation size ceilings (threat T-13-01: oversized payloads are a
/// tampering/DoS surface crossing the IPC boundary — validate-then-apply).
const MAX_ANNOTATION_POINTS: usize = 500;
const MAX_LABEL_TEXT_LEN: usize = 500;

/// Cap on RenameMediaItem's display-name length (Phase 25, LIB-01) -- mirrors
/// MAX_LABEL_TEXT_LEN's existing DoS-guard precedent for agent-originated text.
const MAX_MEDIA_NAME_LEN: usize = 255;

/// Reject a non-finite coordinate BEFORE it is clamped/stored. `f64::clamp`
/// passes NaN straight through (NaN is neither `<` nor `>` its bounds), so a
/// NaN/inf coordinate would otherwise be stored verbatim and poison every
/// downstream denormalizer (`p.x * w`). This mirrors `MoveAnnotation`'s
/// finite-delta guard (threat T-14.3-01): reject, don't sanitize.
fn ensure_finite_point(p: &crate::canvas::NormPoint) -> Result<(), CoreError> {
    if !p.x.is_finite() || !p.y.is_finite() {
        return Err(CoreError::InvalidAnnotation(
            "annotation coordinate must be finite".into(),
        ));
    }
    Ok(())
}

/// Validate structural constraints (reject) and clamp coordinate/length
/// constraints (never reject for these — preserve as much of the user's
/// sketch as possible, per "never silently drop an annotation"):
/// empty Stroke/Lasso point lists and Lasso < 3 points are REJECTED;
/// > MAX_ANNOTATION_POINTS points is REJECTED; a non-finite (NaN/inf)
/// coordinate is REJECTED (clamp does NOT sanitize NaN — mirrors the
/// MoveAnnotation finite-delta guard); every finite NormPoint is CLAMPED
/// into [0,1]; Label.text longer than MAX_LABEL_TEXT_LEN is TRUNCATED.
fn normalize_and_validate_shape(
    shape: crate::canvas::AnnotationShape,
) -> Result<crate::canvas::AnnotationShape, CoreError> {
    use crate::canvas::{AnnotationShape, NormPoint};
    match shape {
        AnnotationShape::Stroke { points } => {
            if points.is_empty() {
                return Err(CoreError::InvalidAnnotation(
                    "stroke must have >= 1 point".into(),
                ));
            }
            if points.len() > MAX_ANNOTATION_POINTS {
                return Err(CoreError::InvalidAnnotation(format!(
                    "stroke has {} points, exceeds the {MAX_ANNOTATION_POINTS} cap",
                    points.len()
                )));
            }
            for p in &points {
                ensure_finite_point(p)?;
            }
            Ok(AnnotationShape::Stroke {
                points: points
                    .into_iter()
                    .map(|p| NormPoint::clamped(p.x, p.y))
                    .collect(),
            })
        }
        AnnotationShape::Lasso { points } => {
            if points.len() < 3 {
                return Err(CoreError::InvalidAnnotation(format!(
                    "lasso needs >= 3 points, got {}",
                    points.len()
                )));
            }
            if points.len() > MAX_ANNOTATION_POINTS {
                return Err(CoreError::InvalidAnnotation(format!(
                    "lasso has {} points, exceeds the {MAX_ANNOTATION_POINTS} cap",
                    points.len()
                )));
            }
            for p in &points {
                ensure_finite_point(p)?;
            }
            Ok(AnnotationShape::Lasso {
                points: points
                    .into_iter()
                    .map(|p| NormPoint::clamped(p.x, p.y))
                    .collect(),
            })
        }
        AnnotationShape::Arrow { start, end } => {
            ensure_finite_point(&start)?;
            ensure_finite_point(&end)?;
            Ok(AnnotationShape::Arrow {
                start: NormPoint::clamped(start.x, start.y),
                end: NormPoint::clamped(end.x, end.y),
            })
        }
        AnnotationShape::Label { position, mut text } => {
            ensure_finite_point(&position)?;
            if text.len() > MAX_LABEL_TEXT_LEN {
                text.truncate(MAX_LABEL_TEXT_LEN);
            }
            Ok(AnnotationShape::Label {
                position: NormPoint::clamped(position.x, position.y),
                text,
            })
        }
    }
}

/// Pure per-variant point translation for MoveAnnotation: shifts every
/// point-bearing field by (dx, dy) and CLAMPS each into [0,1] via the SAME
/// `NormPoint::clamped` AddAnnotation uses (never rejects). `Label.text` is
/// carried through untouched — only its position moves. The clamp makes this
/// NON-LINEAR near the [0,1] edges, which is precisely why MoveAnnotation's
/// inverse must carry the exact prior shape rather than a negated delta.
fn translate_shape(
    shape: &crate::canvas::AnnotationShape,
    dx: f64,
    dy: f64,
) -> crate::canvas::AnnotationShape {
    use crate::canvas::{AnnotationShape, NormPoint};
    let t = |p: &NormPoint| NormPoint::clamped(p.x + dx, p.y + dy);
    match shape {
        AnnotationShape::Stroke { points } => AnnotationShape::Stroke {
            points: points.iter().map(t).collect(),
        },
        AnnotationShape::Lasso { points } => AnnotationShape::Lasso {
            points: points.iter().map(t).collect(),
        },
        AnnotationShape::Arrow { start, end } => AnnotationShape::Arrow {
            start: t(start),
            end: t(end),
        },
        AnnotationShape::Label { position, text } => AnnotationShape::Label {
            position: t(position),
            text: text.clone(),
        },
    }
}

/// Deterministic child-clip id: `{base}~{tag}{n}` with the SMALLEST `n >= 1`
/// not already used by any clip. A state-derived counter — never time or
/// random — so ids are stable across test runs and identical on redo.
fn unique_child_id(project: &Project, base: &str, tag: char) -> String {
    let mut n: u64 = 1;
    loop {
        let candidate = format!("{base}~{tag}{n}");
        let taken = project
            .timeline
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .any(|c| c.id == candidate);
        if !taken {
            return candidate;
        }
        n += 1;
    }
}

/// Normalize + validate ONE property's keyframe list (Phase 19, D-08 /
/// T-19-02 / T-19-03), shared by every `KeyframeTrackData` variant:
/// per-key `validate` (clamp or reject, mirroring the property's static
/// setter), then a STABLE sort ascending by `frame` (unsorted input is a
/// lossless canonicalization), then an adjacent-duplicate REJECT (two keys
/// at one frame is a genuine ambiguity — loud failure over silent
/// last-wins, the RemoveTracks duplicate-index precedent). The sorted,
/// duplicate-free result is what makes the sampler's divide denominator
/// provably nonzero.
fn normalize_keys<V: Copy>(
    keys: &[Keyframe<V>],
    validate: impl Fn(V) -> Result<V, CoreError>,
) -> Result<Vec<Keyframe<V>>, CoreError> {
    let mut out: Vec<Keyframe<V>> = Vec::with_capacity(keys.len());
    for k in keys {
        out.push(Keyframe {
            frame: k.frame,
            value: validate(k.value)?,
            interp: k.interp,
        });
    }
    out.sort_by_key(|k| k.frame); // stable: equal frames keep input order
    for pair in out.windows(2) {
        if pair[0].frame == pair[1].frame {
            return Err(CoreError::InvalidSettings(format!(
                "duplicate keyframe frame {}: each frame may carry at most \
                 one key per property",
                pair[0].frame
            )));
        }
    }
    Ok(out)
}

/// Per-variant value validation for `Command::SetKeyframes`, MIRRORING the
/// static setters exactly (T-19-02 — NaN/inf can never enter a track, so
/// interpolation output is always finite):
/// - position/scale components + rotation: REJECT non-finite
///   (`SetClipTransform` discipline);
/// - opacity: clamp `[0,1]` max-then-min, NaN -> 0.0 (`SetClipOpacity`);
/// - volume: clamp `[0,10]` max-then-min, NaN -> 0.0 (`SetClipVolume`);
/// - crop: clamp each inset `[0,1]`, then REJECT `left+right >= 1` or
///   `top+bottom >= 1` per keyframe (`SetClipCrop`).
fn normalize_keyframe_track(track: &KeyframeTrackData) -> Result<KeyframeTrackData, CoreError> {
    let finite_pair = |v: (f32, f32)| -> Result<(f32, f32), CoreError> {
        if !v.0.is_finite() || !v.1.is_finite() {
            return Err(CoreError::InvalidSettings(format!(
                "keyframe components must be finite, got ({}, {})",
                v.0, v.1
            )));
        }
        Ok(v)
    };
    match track {
        KeyframeTrackData::Position(keys) => Ok(KeyframeTrackData::Position(normalize_keys(
            keys,
            finite_pair,
        )?)),
        KeyframeTrackData::Scale(keys) => {
            Ok(KeyframeTrackData::Scale(normalize_keys(keys, finite_pair)?))
        }
        KeyframeTrackData::Rotation(keys) => Ok(KeyframeTrackData::Rotation(normalize_keys(
            keys,
            |v: f32| {
                if !v.is_finite() {
                    return Err(CoreError::InvalidSettings(format!(
                        "keyframe rotation must be finite, got {v}"
                    )));
                }
                Ok(v)
            },
        )?)),
        KeyframeTrackData::Opacity(keys) => Ok(KeyframeTrackData::Opacity(normalize_keys(
            keys,
            // Clamp [0,1] — max-then-min, NOT f32::clamp: f32::max maps a
            // NaN input to 0.0, while clamp would pass NaN through.
            |v: f32| Ok(v.max(0.0).min(1.0)),
        )?)),
        KeyframeTrackData::Volume(keys) => Ok(KeyframeTrackData::Volume(normalize_keys(
            keys,
            // Clamp [0,10] (linear multiplier, 16-review ceiling; NaN -> 0.0).
            |v: f32| Ok(v.max(0.0).min(10.0)),
        )?)),
        KeyframeTrackData::Crop(keys) => Ok(KeyframeTrackData::Crop(normalize_keys(
            keys,
            |v: CropValue| {
                let c = |x: f32| x.max(0.0).min(1.0);
                let clamped = CropValue {
                    left: c(v.left),
                    top: c(v.top),
                    right: c(v.right),
                    bottom: c(v.bottom),
                };
                if clamped.left + clamped.right >= 1.0 || clamped.top + clamped.bottom >= 1.0 {
                    return Err(CoreError::InvalidSettings(format!(
                        "crop keyframe leaves no source: left+right and \
                         top+bottom must each be < 1, got {}+{} horizontal, \
                         {}+{} vertical",
                        clamped.left, clamped.right, clamped.top, clamped.bottom
                    )));
                }
                Ok(clamped)
            },
        )?)),
    }
}

/// Validate a WHOLE `KeyframeTracks` set through the EXACT same per-property
/// pipeline `Command::SetKeyframes` enforces — (a) `MAX_KEYFRAMES_PER_TRACK`
/// cap, then (b) sort + adjacent-duplicate reject and (c) per-property
/// clamp/finite rules (via `normalize_keyframe_track`) — for EACH of the six
/// property tracks (H-01, 19-REVIEW). Used to harden `TrimClip`'s inverse-only
/// `restore_keyframes` path: that field is a plain deserializable part of the
/// wire `Command` enum reachable over the general `dispatch_command` IPC
/// surface, so without this a hand-crafted `TrimClip` could inject
/// NaN/inf/over-cap keyframes straight into `clip.keyframes`, bypassing every
/// `SetKeyframes` invariant. For the LEGITIMATE undo path the carried tracks
/// were already validated when originally set, so this is an idempotent no-op
/// (a normal trim/undo round-trip restores the exact prior track unchanged).
fn validate_keyframe_tracks(tracks: &KeyframeTracks) -> Result<KeyframeTracks, CoreError> {
    // Mirror SetKeyframes' apply order per property: (a) cap FIRST, then the
    // sort/dedup/clamp normalize. `normalize_keyframe_track` returns the SAME
    // variant it is given, so reassembling via `replace` on a default set is
    // total and preserves the property→field mapping.
    fn one(data: KeyframeTrackData) -> Result<KeyframeTrackData, CoreError> {
        if data.len() > MAX_KEYFRAMES_PER_TRACK {
            return Err(CoreError::InvalidSettings(format!(
                "keyframe track '{}' has {} keys, exceeds the \
                 {MAX_KEYFRAMES_PER_TRACK} cap",
                data.name(),
                data.len()
            )));
        }
        normalize_keyframe_track(&data)
    }
    let mut out = KeyframeTracks::default();
    out.replace(one(KeyframeTrackData::Position(tracks.position.clone()))?);
    out.replace(one(KeyframeTrackData::Scale(tracks.scale.clone()))?);
    out.replace(one(KeyframeTrackData::Rotation(tracks.rotation.clone()))?);
    out.replace(one(KeyframeTrackData::Opacity(tracks.opacity.clone()))?);
    out.replace(one(KeyframeTrackData::Crop(tracks.crop.clone()))?);
    out.replace(one(KeyframeTrackData::Volume(tracks.volume.clone()))?);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Quick task 260730-x2t: retime helpers shared by SetClipRetime / TrimClip /
// SplitClip / MergeSplit / RestoreSplit / AddClip. ONE place, so no two apply
// arms can drift on validation or on the derived-cache rule.
// ---------------------------------------------------------------------------

/// Serde adapter that preserves the `Option<Option<T>>` distinction across a
/// JSON round-trip: an explicit `null` deserializes to `Some(None)` ("set the
/// field to nothing"), an ABSENT key to `None` ("leave it alone"). Plain
/// derived serde collapses both to `None`.
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(value: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match value {
            Some(inner) => inner.serialize(s),
            // Unreachable in practice: `skip_serializing_if` drops the key.
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// THE untrusted-`Retime` gate for every command carrier. A thin alias for
/// [`crate::model::sanitized_retime`], which is where the implementation lives
/// because there are TWO untrusted boundaries, not one: this IPC surface and
/// the `.rud` LOAD path (`Project::sanitize_retime_after_load`). A second
/// implementation for the load boundary is exactly how CR-01 happened.
///
/// **EVERY `Command` variant that carries a whole `Clip` — or a bare `Retime` —
/// off the wire MUST route it through here before it reaches the timeline.**
/// The carriers today (keep this list current; `rg -n "clip: Clip|right: Clip"
/// crates/core/src/command.rs` finds them all): `AddClip`, `AddText`,
/// `RestoreClip`, `RestoreDetachedAudio`, `RestoreSplit` (both halves),
/// `TrimClip::restore_retime`, `SetClipRetime`.
fn sanitized_retime(
    carried: Option<&crate::model::Retime>,
    project_fps: f64,
    source_span_us: i64,
) -> Result<Option<crate::model::Retime>, CoreError> {
    crate::model::sanitized_retime(carried, project_fps, source_span_us)
}

/// Rebuild `clip.retime`'s cached occupancy from the clip's CURRENT
/// `in_us`/`out_us`. Call after ANY mutation of the source span or the curve
/// (threat T-x2t-08). A no-op for an un-retimed clip.
fn refresh_retime_cache(clip: &mut Clip) {
    if clip.retime.is_some() {
        clip.retime = clip.recomputed_retime();
    }
}

/// Convert a SOURCE delta into the TIMELINE delta that consumes it under
/// `retime` — the inverse of the speed integral, used by `TrimClip`'s
/// left-edge start shift. `None` (un-retimed) returns `delta` VERBATIM, so
/// every pre-retime trim path is byte-unchanged.
fn timeline_delta_for_source_delta(retime: Option<&crate::model::Retime>, delta: i64) -> i64 {
    match retime {
        None => delta,
        Some(r) => match delta.cmp(&0) {
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => {
                crate::model::retimed_timeline_len_us(&r.curve, r.timebase_fps, delta)
            }
            std::cmp::Ordering::Less => {
                -crate::model::retimed_timeline_len_us(&r.curve, r.timebase_fps, -delta)
            }
        },
    }
}

/// Maximum text-clip content length in BYTES (Phase 20, T-20-03 DoS): text
/// content originates from LLM tool calls / MCP over the general dispatch IPC
/// surface, so bound it BEFORE any allocation-heavy raster work downstream.
/// 5000 bytes is generous for any real title/caption; pathological payloads
/// are rejected loudly at `Command::apply`. `pub` so Plan 03's `add_texts`/
/// `update_text` tools validate against the SAME cap (single source of truth).
pub const MAX_TEXT_CONTENT_LEN: usize = 5000;

/// Whether `name` is a bundled, renderable font family (Phase 20, D-04/D-08).
/// The SINGLE allow-list both `Command::apply` and Plan 03's tools validate
/// against — kept in lock-step with the engine's bundled faces. Only Inter
/// (`engine::BUNDLED_FONT_FAMILY = "Inter"`) actually ships and is the only
/// family the rasterizer shapes against, so it is the ONLY accepted value:
/// accepting a family with no rendering path (e.g. "Roboto") would silently
/// render Inter and desync the declared style from the pixels (H-01 / D-08).
/// This pure crate cannot depend on the engine, so the name is mirrored here —
/// keep the two in sync (add a family here ONLY when its face is bundled AND
/// threaded through `rasterize_text`).
pub fn is_bundled_font(name: &str) -> bool {
    matches!(name, "Inter")
}

/// The maximum accepted normalized `font_size` — a fraction of canvas HEIGHT,
/// so `1.0` is a glyph as tall as the whole canvas. Anything larger is treated
/// as tampering (M-02): reject at the core layer rather than relying on the
/// engine's downstream `MAX_FONT_PX` pixel clamp to absorb it silently.
pub const MAX_TEXT_FONT_SIZE: f32 = 1.0;

/// Validate a [`TextStyle`] (Phase 20, T-20-03): family is the bundled font;
/// font_size finite & in `(0, MAX_TEXT_FONT_SIZE]`; wrap_width in `(0, 1]` when
/// `Some` (both are NORMALIZED fractions of the canvas per `model.rs`). Reject
/// (never clamp) — non-finite/out-of-range values are genuine tampering, not a
/// UI slip, and silently clamping/ignoring them deep in the engine would leave
/// the agent unable to tell an honored value from a discarded one (M-02).
/// Shared by `AddText`/`UpdateText`/`RestoreText` so every text mutation
/// enforces the identical rules.
fn validate_text_style(style: &TextStyle) -> Result<(), CoreError> {
    if !is_bundled_font(&style.font_family) {
        return Err(CoreError::InvalidSettings(format!(
            "font_family '{}' is not the bundled font (Inter)",
            style.font_family
        )));
    }
    if !style.font_size.is_finite() || style.font_size <= 0.0 || style.font_size > MAX_TEXT_FONT_SIZE
    {
        return Err(CoreError::InvalidSettings(format!(
            "font_size must be finite and in (0, {MAX_TEXT_FONT_SIZE}] \
             (a normalized fraction of canvas height), got {}",
            style.font_size
        )));
    }
    if let Some(w) = style.wrap_width {
        if !w.is_finite() || w <= 0.0 || w > 1.0 {
            return Err(CoreError::InvalidSettings(format!(
                "wrap_width must be finite and in (0, 1] \
                 (a normalized fraction of canvas width), got {w}"
            )));
        }
    }
    Ok(())
}

/// Validate a whole [`TextPayload`]: content non-empty & length-capped
/// (`MAX_TEXT_CONTENT_LEN`, T-20-03), then the style via [`validate_text_style`].
pub(crate) fn validate_text_payload(payload: &TextPayload) -> Result<(), CoreError> {
    if payload.content.is_empty() {
        return Err(CoreError::InvalidSettings(
            "text content must be non-empty".into(),
        ));
    }
    if payload.content.len() > MAX_TEXT_CONTENT_LEN {
        return Err(CoreError::InvalidSettings(format!(
            "text content is {} bytes, exceeds the {MAX_TEXT_CONTENT_LEN} cap",
            payload.content.len()
        )));
    }
    validate_text_style(&payload.style)
}

/// Locate a clip by id: `(track_idx, clip_idx)`.
fn find_clip(project: &Project, id: &str) -> Option<(usize, usize)> {
    project.timeline.tracks.iter().enumerate().find_map(|(ti, t)| {
        t.clips
            .iter()
            .position(|c| c.id == id)
            .map(|ci| (ti, ci))
    })
}

impl Command {
    /// Validate against `project`, then apply. Returns the patch describing
    /// the change and the exact inverse command. On `Err`, `project` is
    /// GUARANTEED untouched (all validation happens before any mutation).
    pub(crate) fn apply(&self, project: &mut Project) -> Result<(Patch, Command), CoreError> {
        match self {
            Command::AddMediaBinItem(item) => {
                if project.media_bin.iter().any(|m| m.id == item.id) {
                    return Err(CoreError::DuplicateId(item.id.clone()));
                }
                if item.duration_us < 0 {
                    return Err(CoreError::InvalidTimes(format!(
                        "media duration_us must be >= 0, got {}",
                        item.duration_us
                    )));
                }
                // Phase 27 (LIB-03, 25-REVIEW CR-01 precedent): dispatch_command is a
                // directly-invokable host command surface (the C ABI today), so
                // Tool-layer-only folder
                // validation is bypassable — mirror MoveMediaItem's own check here.
                if !item.folder.is_empty()
                    && !project.media_folders.iter().any(|f| f == &item.folder)
                {
                    return Err(CoreError::FolderNotFound(item.folder.clone()));
                }
                project.media_bin.push(item.clone());
                Ok((
                    Patch::one(PatchKind::MediaBinItemAdded, &item.id),
                    Command::RemoveMediaBinItem {
                        id: item.id.clone(),
                    },
                ))
            }

            Command::RemoveMediaBinItem { id } => {
                let idx = project
                    .media_bin
                    .iter()
                    .position(|m| m.id == *id)
                    .ok_or_else(|| CoreError::MediaBinItemNotFound(id.clone()))?;
                let referencing: Vec<&str> = project
                    .timeline
                    .tracks
                    .iter()
                    .flat_map(|t| t.clips.iter())
                    .filter(|c| c.media_id == *id)
                    .map(|c| c.id.as_str())
                    .collect();
                if !referencing.is_empty() {
                    return Err(CoreError::MediaBinItemInUse(
                        id.clone(),
                        referencing.join(", "),
                    ));
                }
                let removed = project.media_bin.remove(idx);
                Ok((
                    Patch::one(PatchKind::MediaBinItemRemoved, id),
                    Command::AddMediaBinItem(removed),
                ))
            }

            Command::AddClip { track, clip } => {
                let track_count = project.timeline.tracks.len();
                if *track >= track_count {
                    return Err(CoreError::TrackOutOfRange(*track, track_count));
                }
                if project
                    .timeline
                    .tracks
                    .iter()
                    .flat_map(|t| t.clips.iter())
                    .any(|c| c.id == clip.id)
                {
                    return Err(CoreError::DuplicateId(clip.id.clone()));
                }
                if !project.media_bin.iter().any(|m| m.id == clip.media_id) {
                    return Err(CoreError::MediaBinItemNotFound(clip.media_id.clone()));
                }
                if clip.in_us < 0 || clip.out_us <= clip.in_us {
                    return Err(CoreError::InvalidTimes(format!(
                        "clip {}: in_us={} out_us={} (need in>=0, out>in)",
                        clip.id, clip.in_us, clip.out_us
                    )));
                }
                // Phase 5: a NEGATIVE timeline start is CLAMPED to 0, not
                // rejected — drops/drags near the timeline origin land at 0.
                // Still undoable: the inverse is RemoveClip either way.
                let mut clip = clip.clone();
                clip.start_us = clip.start_us.max(0);
                // A whole `Clip` arrives over the general `dispatch_command`
                // IPC surface, so its retime is untrusted: validate the curve
                // and REBUILD the derived occupancy cache from the clip's own
                // span (260730-x2t, threats T-x2t-01/T-x2t-08). A hand-crafted
                // AddClip can otherwise seed a clip whose stored length does
                // not match its curve, silently misplacing every later clip.
                clip.retime = sanitized_retime(
                    clip.retime.as_ref(),
                    project.fps,
                    clip.out_us - clip.in_us,
                )?;
                project.timeline.tracks[*track].clips.push(clip.clone());
                Ok((
                    Patch::one(PatchKind::ClipAdded, &clip.id),
                    Command::RemoveClip {
                        id: clip.id.clone(),
                    },
                ))
            }

            Command::MoveClip { id, new_start_us } => {
                let clip = project
                    .timeline
                    .tracks
                    .iter_mut()
                    .flat_map(|t| t.clips.iter_mut())
                    .find(|c| c.id == *id)
                    .ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let old_start_us = clip.start_us;
                // Phase 5: negative start CLAMPS to 0 (drag past the origin
                // pins the clip at 0). Undo restores the exact old start.
                clip.start_us = (*new_start_us).max(0);
                // Phase 43 (LAT-02): attach the POST-mutation clip so the
                // renderer applies this edit without a follow-up IPC call.
                let patch = Patch::one(PatchKind::ClipMoved, id).with_clip(clip);
                Ok((
                    patch,
                    Command::MoveClip {
                        id: id.clone(),
                        new_start_us: old_start_us,
                    },
                ))
            }

            Command::MoveClipToTrack { id, target_track, new_start_us, target_index } => {
                let (old_track_idx, old_index) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let track_count = project.timeline.tracks.len();
                if *target_track >= track_count {
                    return Err(CoreError::TrackOutOfRange(*target_track, track_count));
                }
                let target_kind = project.timeline.tracks[*target_track].kind;
                let media_id = project.timeline.tracks[old_track_idx].clips[old_index]
                    .media_id
                    .clone();
                let (media_kind, has_audio) = {
                    let item = project
                        .media_bin
                        .iter()
                        .find(|m| m.id == media_id)
                        .ok_or_else(|| CoreError::MediaBinItemNotFound(media_id.clone()))?;
                    (item.media_kind, item.has_audio)
                };
                if !track_accepts_media(target_kind, media_kind, has_audio) {
                    return Err(CoreError::IncompatibleTrackMedia(format!(
                        "{media_kind:?} media onto a {target_kind:?} track"
                    )));
                }

                // Remove first (captures the clip AND its OLD start_us), then
                // insert it into the target track at target_index (or append,
                // for the normal forward/user-driven case where target_index is
                // None -- unwrap_or(len) on the as-yet-unmutated target Vec is
                // exactly the position .push() would use, so this is a strict
                // generalization of the old push-only behavior, not a change to
                // the normal-path result).
                let mut clip = project.timeline.tracks[old_track_idx].clips.remove(old_index);
                let old_start_us = clip.start_us;
                clip.start_us = (*new_start_us).max(0);
                let target_clips = &mut project.timeline.tracks[*target_track].clips;
                let insert_at = target_index.unwrap_or(target_clips.len()).min(target_clips.len());
                target_clips.insert(insert_at, clip);
                // Phase 43 (LAT-02): read the clip back from its NEW home — the
                // local binding was moved into the target track by `insert`, so
                // this is the only way to attach genuinely post-mutation state.
                let patch =
                    Patch::one(PatchKind::ClipMoved, id).with_clip(&target_clips[insert_at]);

                // Self-symmetric inverse -- same precedent as Command::MoveClip's
                // own self-inverse. Deliberately NOT Command::RestoreClip: that
                // command's duplicate-id guard rejects a clip that is already
                // present anywhere in the project, which is exactly the case
                // here (a cross-track MOVE never removes the clip from the
                // project as a whole, only relocates it) -- using RestoreClip as
                // the inverse would make every cross-track undo panic inside
                // Store::undo()'s `.expect("... stored inverse failed to apply")`.
                Ok((
                    patch,
                    Command::MoveClipToTrack {
                        id: id.clone(),
                        target_track: old_track_idx,
                        new_start_us: old_start_us,
                        target_index: Some(old_index),
                    },
                ))
            }

            Command::RemoveClip { id } => {
                for (track_idx, track) in project.timeline.tracks.iter_mut().enumerate() {
                    if let Some(pos) = track.clips.iter().position(|c| c.id == *id) {
                        let removed = track.clips.remove(pos);
                        return Ok((
                            Patch::one(PatchKind::ClipRemoved, id),
                            // Index-preserving inverse: order within a track
                            // is overlap priority (Phase 5), so the clip must
                            // come back exactly where it was.
                            Command::RestoreClip {
                                track: track_idx,
                                index: pos,
                                clip: removed,
                            },
                        ));
                    }
                }
                Err(CoreError::ClipNotFound(id.clone()))
            }

            Command::TrimClip {
                id,
                new_in_us,
                new_out_us,
                restore_keyframes,
                restore_retime,
            } => {
                // Validate FIRST (Err must leave state untouched): times
                // sane, out within the media, and the left-edge start shift
                // keeps the clip at/after the timeline origin.
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                if *new_in_us < 0 || *new_out_us <= *new_in_us {
                    return Err(CoreError::InvalidTimes(format!(
                        "trim {}: new_in_us={new_in_us} new_out_us={new_out_us} \
                         (need in>=0, out>in)",
                        id
                    )));
                }
                let (old_in, old_out, old_start, old_tracks, old_retime, media_id) = {
                    let c = &project.timeline.tracks[ti].clips[ci];
                    (
                        c.in_us,
                        c.out_us,
                        c.start_us,
                        c.keyframes.clone(),
                        c.retime.clone(),
                        c.media_id.clone(),
                    )
                };
                let media_duration = project
                    .media_bin
                    .iter()
                    .find(|m| m.id == media_id)
                    .map(|m| m.duration_us)
                    .unwrap_or(i64::MAX);
                if media_duration > 0 && *new_out_us > media_duration {
                    return Err(CoreError::InvalidTimes(format!(
                        "trim {}: new_out_us={new_out_us} exceeds media duration \
                         {media_duration}",
                        id
                    )));
                }
                // Validate the inverse-only restore payload BEFORE any mutation
                // (validate-then-apply): `restore_keyframes` rides the general
                // `dispatch_command` IPC surface, so a hand-crafted `TrimClip`
                // could otherwise inject NaN/inf/over-cap keys straight into
                // `clip.keyframes`, bypassing every `SetKeyframes` invariant
                // (H-01). Route it through the SAME cap/clamp/finite/dedup
                // pipeline. For a legitimate undo the carried tracks were
                // already validated when originally set, so this is an
                // idempotent no-op (the exact prior track round-trips unchanged).
                let validated_restore = match restore_keyframes {
                    Some(kf) => Some(validate_keyframe_tracks(kf)?),
                    None => None,
                };
                // Read the PROJECT fps (Copy) before the &mut clip borrow — the
                // keyframe remap renumbers in the same timebase the sampler uses.
                let project_fps = project.fps;
                // Same H-01 defense for the retime carrier (threat T-x2t-05).
                // The `Option<Option<_>>` shape matters: the OUTER None means
                // "forward caller, derive the remap"; `Some(None)` is a real
                // instruction to CLEAR the retime. Validated + cache-rebuilt
                // against the POST-trim source span, before any mutation.
                let validated_restore_retime: Option<Option<crate::model::Retime>> =
                    match restore_retime {
                        Some(r) => Some(sanitized_retime(
                            r.as_ref(),
                            project_fps,
                            *new_out_us - *new_in_us,
                        )?),
                        None => None,
                    };
                // Left-edge trim moves start so the frames already on the
                // timeline stay put.
                //
                // `delta` is a SOURCE delta. Pre-retime the timeline↔source map
                // was 1:1 so it doubled as the TIMELINE delta; under retime it
                // does NOT (quick task 260730-x2t). Route it through the
                // INVERSE of the speed integral — trimming 1 s of SOURCE off a
                // 2x clip moves `start_us` by 0.5 s of TIMELINE, not 1 s.
                // `retime: None` returns `delta` verbatim, so every pre-retime
                // trim is byte-unchanged.
                //
                // WHICH CURVE measures the delta matters for undo exactness.
                // The rule: the delta is measured against the curve that is in
                // effect over the source content being REMOVED or RE-ADDED.
                //  - forward trim (no restore payload): the front of the
                //    CURRENT curve is being cut, so measure against it;
                //  - inverse trim (`restore_retime` is `Some`): the content is
                //    being re-added in front of the RESTORED curve, so measure
                //    against THAT. Measuring against the already-remapped
                //    curve instead would land `start_us` a few tens of ms off
                //    and undo would no longer be exact.
                let delta = *new_in_us - old_in;
                let delta_curve: Option<&crate::model::Retime> = match &validated_restore_retime {
                    Some(restored) => restored.as_ref(),
                    None => old_retime.as_ref(),
                };
                let timeline_delta = timeline_delta_for_source_delta(delta_curve, delta);
                let new_start = old_start + timeline_delta;
                if new_start < 0 {
                    return Err(CoreError::InvalidTimes(format!(
                        "trim {}: left-edge trim would move the clip before the \
                         timeline origin (start {old_start} + delta {timeline_delta})",
                        id
                    )));
                }
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                clip.in_us = *new_in_us;
                clip.out_us = *new_out_us;
                clip.start_us = new_start;
                // Keyframe/content lock (Phase 19, COMP-04): a left-edge trim
                // (delta != 0) moves the sampling anchor `t - start_us`, so the
                // animated tracks must be content-preservingly remapped or the
                // same source content would render a different value.
                if let Some(kf) = validated_restore {
                    // Exact-restore path (this is the inverse of a prior trim):
                    // reinstate the precise prior tracks, undoing the lossy
                    // remap (dropped keys return, synthesized keys vanish).
                    clip.keyframes = kf;
                } else if timeline_delta != 0 && !clip.keyframes.is_empty() {
                    // `shifted_for_left_cut` works in clip-relative TIMELINE µs,
                    // so the cut is `timeline_delta` — NOT the source `delta`
                    // (identical when un-retimed; different at every other
                    // speed). 260730-x2t.
                    clip.keyframes = old_tracks.shifted_for_left_cut(timeline_delta, project_fps);
                }
                // else: a right-edge-only trim (delta == 0) leaves tracks
                // untouched — keys past the new end are D-05 out-of-range.
                //
                // The retime curve gets the SAME treatment (260730-x2t): exact
                // restore on the inverse path, else a content-preserving
                // left-cut remap. Then the derived occupancy cache is ALWAYS
                // rebuilt — a right-edge-only trim (timeline_delta == 0) still
                // changes the source span, so the cached length still moves.
                if let Some(restored) = validated_restore_retime {
                    clip.retime = restored;
                } else {
                    if timeline_delta != 0 {
                        if let Some(old) = old_retime.as_ref() {
                            clip.retime = Some(crate::model::Retime {
                                // `old.timebase_fps`, NOT `project_fps`
                                // (260730-x2t WR-03). `shift_track` converts
                                // key frames to µs, compares against the cut,
                                // renumbers, and lerps the continuity key —
                                // every step in the fps it is HANDED. The
                                // resulting curve is later INTERPRETED with
                                // `old.timebase_fps`, so handing it anything
                                // else silently rescales the ramp's frame
                                // numbering by the fps ratio.
                                //
                                // The `keyframes` call above deliberately stays
                                // on `project_fps`: keyframe tracks are
                                // ANCHORED to project fps by definition, so the
                                // two can never diverge there. `Retime`
                                // deliberately carries its OWN frozen timebase
                                // (see `Retime::timebase_fps`'s doc), which is
                                // exactly what makes this site different — a
                                // `SetProjectSettings` fps change after a ramp
                                // is authored is supported and expected.
                                curve: old
                                    .curve
                                    .shifted_for_left_cut(timeline_delta, old.timebase_fps),
                                timeline_len_us: old.timeline_len_us,
                                timebase_fps: old.timebase_fps,
                            });
                        }
                    }
                    refresh_retime_cache(clip);
                }
                // Phase 43 (LAT-02): attach the POST-mutation clip.
                let patch = Patch::one(PatchKind::ClipTrimmed, id).with_clip(clip);
                Ok((
                    patch,
                    // The inverse trim's delta (old_in - new_in) shifts start
                    // back to old_start exactly; its restore_keyframes carries
                    // the EXACT prior tracks (always Some — a no-remap trim
                    // restores an identical clone; cheap and uniform).
                    Command::TrimClip {
                        id: id.clone(),
                        new_in_us: old_in,
                        new_out_us: old_out,
                        restore_keyframes: Some(old_tracks),
                        // ALWAYS Some (even `Some(None)`): the inverse must be
                        // able to say "there was NO retime", which the outer
                        // None cannot express (it means "derive it yourself").
                        restore_retime: Some(old_retime),
                    },
                ))
            }

            Command::SplitClip { id, at_position_us } => {
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let original = project.timeline.tracks[ti].clips[ci].clone();
                // The split point must be STRICTLY inside the clip's timeline
                // occupancy — splitting at an edge would create a 0-length clip.
                if *at_position_us <= original.start_us
                    || *at_position_us >= original.timeline_end_us()
                {
                    return Err(CoreError::InvalidTimes(format!(
                        "split {}: position {at_position_us} not strictly inside \
                         [{}, {})",
                        id,
                        original.start_us,
                        original.timeline_end_us()
                    )));
                }
                // Source split point: contiguous by construction —
                // left=(in,Sp), right=(Sp,out), right.start = at_position_us.
                // Under retime the cut's TIMELINE offset is not its SOURCE
                // offset — route it through the FORWARD speed integral, the
                // same `Clip::source_offset_at` `active_at` resolves through
                // (260730-x2t, RT-04). Identity when un-retimed.
                let sp = original.in_us + original.source_offset_at(*at_position_us - original.start_us);
                // Keyframe/content continuity (Phase 19, COMP-04): the right
                // half's sampling anchor moves to the cut, so its animated
                // tracks are content-preservingly remapped — the SAME operation
                // as a left-edge trim (cut = at_position - start): the right
                // half CONTINUES from the interpolated cut value instead of
                // restarting at its first key (a split Ken-Burns zoom never
                // jumps back). The LEFT half is deliberately UNTOUCHED — its
                // start/in are unchanged so sampling is identical for every
                // interp; keys past the cut become D-05 out-of-range keys,
                // exactly mirroring right-edge trim (and keeping MergeSplit/
                // RestoreSplit undo exact with zero new machinery).
                let cut_rel_us = *at_position_us - original.start_us;
                let right_tracks = original.keyframes.shifted_for_left_cut(cut_rel_us, project.fps);
                // STRUCT-UPDATE inheritance (Phase 18, mirrors DuplicateClip's
                // `..original`): the right half carries EVERY remaining field of
                // the parent forward — media_id, out_us, volume, audio_detached,
                // AND the visual transform/opacity/crop (splitting a PIP clip
                // must yield two visually consistent halves). `keyframes` and
                // `retime` are the TWO fields that must NOT plain-inherit
                // (both are anchored to the clip's own origin, and `retime`
                // additionally carries a derived occupancy cache): `keyframes`
                // is remapped above via shifted_for_left_cut, `retime` just
                // below via the rate-curve twin of the same remap plus a cache
                // rebuild on BOTH halves. Any OTHER future Clip field inherits
                // automatically; never convert the rest back to an exhaustive
                // literal.
                let right_retime = original.retime.as_ref().map(|r| crate::model::Retime {
                    // `r.timebase_fps`, NOT `project.fps` (260730-x2t WR-03):
                    // the remapped curve keeps `r.timebase_fps` two lines down,
                    // so the cut must be measured in the curve's OWN timebase or
                    // the ramp's frame numbering is rescaled by the fps ratio.
                    // The `keyframes` remap above correctly uses `project.fps` —
                    // keyframe tracks are anchored to project fps by definition,
                    // whereas `Retime` deliberately carries a FROZEN timebase
                    // that a later `SetProjectSettings` does not rewarp.
                    curve: r.curve.shifted_for_left_cut(cut_rel_us, r.timebase_fps),
                    // Placeholder — rebuilt from the right half's own
                    // [sp, out_us) span immediately below.
                    timeline_len_us: r.timeline_len_us,
                    timebase_fps: r.timebase_fps,
                });
                let mut right = Clip {
                    id: unique_child_id(project, id, 's'),
                    start_us: *at_position_us,
                    in_us: sp,
                    keyframes: right_tracks,
                    retime: right_retime,
                    ..original
                };
                refresh_retime_cache(&mut right);
                let right_id = right.id.clone();
                project.timeline.tracks[ti].clips[ci].out_us = sp;
                // The LEFT half keeps the ORIGINAL curve (its origin did not
                // move) but its source span shrank, so its cached occupancy
                // must be rebuilt (threat T-x2t-08).
                refresh_retime_cache(&mut project.timeline.tracks[ti].clips[ci]);
                // Insert the right clip immediately AFTER the left so the
                // pair keeps its overlap priority relative to other clips.
                project.timeline.tracks[ti].clips.insert(ci + 1, right);
                Ok((
                    Patch {
                        kind: PatchKind::ClipSplit,
                        ids: vec![id.clone(), right_id.clone()],
                        entities: None,
                    },
                    Command::MergeSplit {
                        left_id: id.clone(),
                        right_id,
                    },
                ))
            }

            Command::MergeSplit { left_id, right_id } => {
                let (lti, lci) = find_clip(project, left_id)
                    .ok_or_else(|| CoreError::ClipNotFound(left_id.clone()))?;
                let (rti, rci) = find_clip(project, right_id)
                    .ok_or_else(|| CoreError::ClipNotFound(right_id.clone()))?;
                let left_out = project.timeline.tracks[lti].clips[lci].out_us;
                // Capture the left half's retime BEFORE the merge so the
                // inverse re-split restores it EXACTLY (260730-x2t) — undo of
                // a split-then-retime must not be lossy.
                let left_retime = project.timeline.tracks[lti].clips[lci].retime.clone();
                let right = project.timeline.tracks[rti].clips.remove(rci);
                // Re-locate the left clip (its index may have shifted if both
                // clips share a track and right preceded it — cannot happen
                // for real split pairs, but stay index-safe regardless).
                let (lti, lci) = find_clip(project, left_id)
                    .expect("left clip still present after removing right");
                project.timeline.tracks[lti].clips[lci].out_us = right.out_us;
                // The merged clip spans BOTH halves' source now, so its cached
                // occupancy is rebuilt. Because the left half kept the ORIGINAL
                // curve through the split, this restores the pre-split length
                // exactly (`timeline_len_us` is a pure function of curve, fps
                // and span).
                refresh_retime_cache(&mut project.timeline.tracks[lti].clips[lci]);
                Ok((
                    Patch {
                        kind: PatchKind::ClipMerged,
                        ids: vec![left_id.clone(), right_id.clone()],
                        entities: None,
                    },
                    Command::RestoreSplit {
                        left_id: left_id.clone(),
                        left_out_us: left_out,
                        // ALWAYS the outer `Some` (even `Some(None)`): the
                        // inverse must be able to say "the left half had NO
                        // retime", which the outer `None` cannot express — it
                        // means "leave it alone" (IN-03).
                        left_retime: Some(left_retime),
                        track: rti,
                        index: rci,
                        right,
                    },
                ))
            }

            Command::RestoreSplit {
                left_id,
                left_out_us,
                left_retime,
                track,
                index,
                right,
            } => {
                let (lti, lci) = find_clip(project, left_id)
                    .ok_or_else(|| CoreError::ClipNotFound(left_id.clone()))?;
                let track_count = project.timeline.tracks.len();
                if *track >= track_count {
                    return Err(CoreError::TrackOutOfRange(*track, track_count));
                }
                if find_clip(project, &right.id).is_some() {
                    return Err(CoreError::DuplicateId(right.id.clone()));
                }
                // Validate-then-apply for BOTH carried retimes (threat
                // T-x2t-05: RestoreSplit rides the general `dispatch_command`
                // IPC surface). Derived caches are rebuilt from the restored
                // spans, so a hostile `timeline_len_us` is structurally
                // impossible — and rebuilding is exact for the legitimate undo.
                let project_fps = project.fps;
                // IN-03: the OUTER `Option` is the instruction. `Some(_)` is a
                // real restore (including `Some(None)` = "clear it"); the
                // ABSENT field means "no instruction" and must LEAVE the left
                // half's retime exactly as it is, never wipe it.
                let left_retime: Option<Option<crate::model::Retime>> = match left_retime {
                    Some(carried) => Some(sanitized_retime(
                        carried.as_ref(),
                        project_fps,
                        *left_out_us - project.timeline.tracks[lti].clips[lci].in_us,
                    )?),
                    None => None,
                };
                let right_retime = sanitized_retime(
                    right.retime.as_ref(),
                    project_fps,
                    right.out_us - right.in_us,
                )?;
                project.timeline.tracks[lti].clips[lci].out_us = *left_out_us;
                if let Some(restored) = left_retime {
                    project.timeline.tracks[lti].clips[lci].retime = restored;
                } else {
                    // No instruction: the left half keeps its curve, but its
                    // SOURCE SPAN just changed (`out_us = left_out_us`), so the
                    // derived occupancy still has to be rebuilt (T-x2t-08).
                    refresh_retime_cache(&mut project.timeline.tracks[lti].clips[lci]);
                }
                let clips = &mut project.timeline.tracks[*track].clips;
                let at = (*index).min(clips.len());
                let mut right = right.clone();
                right.retime = right_retime;
                clips.insert(at, right.clone());
                Ok((
                    Patch {
                        kind: PatchKind::ClipSplit,
                        ids: vec![left_id.clone(), right.id.clone()],
                        entities: None,
                    },
                    Command::MergeSplit {
                        left_id: left_id.clone(),
                        right_id: right.id.clone(),
                    },
                ))
            }

            Command::DuplicateClip { id } => {
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let original = project.timeline.tracks[ti].clips[ci].clone();
                let dup = Clip {
                    id: unique_child_id(project, id, 'd'),
                    start_us: original.timeline_end_us(),
                    ..original
                };
                let dup_id = dup.id.clone();
                project.timeline.tracks[ti].clips.push(dup);
                Ok((
                    Patch::one(PatchKind::ClipAdded, &dup_id),
                    Command::RemoveClip { id: dup_id },
                ))
            }

            Command::SetClipVolume { id, volume } => {
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                let old_volume = clip.volume;
                // Clamp into [0.0, 10.0] (10.0 linear = +20dB — a generous but
                // FINITE ceiling; 16-review fix: an extreme gainDb overflowed
                // f32 to +Infinity, which serialized as JSON null in the
                // delta). Written as max-then-min, NOT f32::clamp: f32::max
                // maps a NaN input to 0.0, while clamp would pass NaN through.
                clip.volume = volume.max(0.0).min(10.0);
                // Phase 43 (LAT-02): attach the POST-mutation clip.
                let patch = Patch::one(PatchKind::ClipVolumeChanged, id).with_clip(clip);
                Ok((
                    patch,
                    Command::SetClipVolume {
                        id: id.clone(),
                        volume: old_volume,
                    },
                ))
            }

            Command::DetachAudio { clip_id } => {
                let (ti, ci) = find_clip(project, clip_id)
                    .ok_or_else(|| CoreError::ClipNotFound(clip_id.clone()))?;
                if project.timeline.tracks[ti].kind != crate::model::TrackKind::Video {
                    return Err(CoreError::InvalidTimes(format!(
                        "detach {clip_id}: only video-track clips have detachable audio"
                    )));
                }
                let video = project.timeline.tracks[ti].clips[ci].clone();
                if video.audio_detached {
                    return Err(CoreError::AlreadyDetached(clip_id.clone()));
                }
                let has_audio = project
                    .media_bin
                    .iter()
                    .find(|m| m.id == video.media_id)
                    .map(|m| m.has_audio)
                    .unwrap_or(false);
                if !has_audio {
                    return Err(CoreError::NoAudioToDetach(clip_id.clone()));
                }
                let audio_track = project
                    .timeline
                    .tracks
                    .iter()
                    .position(|t| t.kind == crate::model::TrackKind::Audio)
                    .ok_or(CoreError::NoAudioTrack)?;
                // STRUCT-UPDATE inheritance (Phase 18, mirrors DuplicateClip's
                // `..original`): the audio clip carries the source clip's
                // media_id/start/in/out/volume AND its visual
                // transform/opacity/crop (inert on an audio track, but never
                // silently zeroed — moving the clip back to a video track or
                // future tooling must see the original values). Only the id
                // and audio_detached are overridden; `audio_detached: false`
                // is guaranteed correct here because an already-detached
                // source was rejected above. Never convert this back to an
                // exhaustive literal.
                let audio_clip = Clip {
                    id: unique_child_id(project, clip_id, 'a'),
                    audio_detached: false,
                    ..video
                };
                let audio_id = audio_clip.id.clone();
                project.timeline.tracks[ti].clips[ci].audio_detached = true;
                project.timeline.tracks[audio_track].clips.push(audio_clip);
                Ok((
                    Patch {
                        kind: PatchKind::AudioDetached,
                        ids: vec![clip_id.clone(), audio_id.clone()],
                        entities: None,
                    },
                    Command::ReattachAudio {
                        video_id: clip_id.clone(),
                        audio_id,
                    },
                ))
            }

            Command::ReattachAudio { video_id, audio_id } => {
                let (vti, vci) = find_clip(project, video_id)
                    .ok_or_else(|| CoreError::ClipNotFound(video_id.clone()))?;
                let (ati, aci) = find_clip(project, audio_id)
                    .ok_or_else(|| CoreError::ClipNotFound(audio_id.clone()))?;
                let audio_clip = project.timeline.tracks[ati].clips.remove(aci);
                // Video/audio clips live on different tracks, so the video
                // indices are unaffected by the removal above.
                project.timeline.tracks[vti].clips[vci].audio_detached = false;
                Ok((
                    Patch {
                        kind: PatchKind::AudioReattached,
                        ids: vec![video_id.clone(), audio_id.clone()],
                        entities: None,
                    },
                    Command::RestoreDetachedAudio {
                        video_id: video_id.clone(),
                        track: ati,
                        index: aci,
                        clip: audio_clip,
                    },
                ))
            }

            Command::RestoreDetachedAudio {
                video_id,
                track,
                index,
                clip,
            } => {
                let (vti, vci) = find_clip(project, video_id)
                    .ok_or_else(|| CoreError::ClipNotFound(video_id.clone()))?;
                let track_count = project.timeline.tracks.len();
                if *track >= track_count {
                    return Err(CoreError::TrackOutOfRange(*track, track_count));
                }
                if find_clip(project, &clip.id).is_some() {
                    return Err(CoreError::DuplicateId(clip.id.clone()));
                }
                // Whole-`Clip` carrier off the general dispatch IPC surface
                // (260730-x2t WR-01): validate + rebuild BEFORE any mutation,
                // so a rejected curve leaves `audio_detached` untouched too
                // (validate-then-apply).
                let mut clip = clip.clone();
                clip.retime =
                    sanitized_retime(clip.retime.as_ref(), project.fps, clip.out_us - clip.in_us)?;
                project.timeline.tracks[vti].clips[vci].audio_detached = true;
                let clips = &mut project.timeline.tracks[*track].clips;
                let at = (*index).min(clips.len());
                clips.insert(at, clip.clone());
                Ok((
                    Patch {
                        kind: PatchKind::AudioDetached,
                        ids: vec![video_id.clone(), clip.id.clone()],
                        entities: None,
                    },
                    Command::ReattachAudio {
                        video_id: video_id.clone(),
                        audio_id: clip.id.clone(),
                    },
                ))
            }

            Command::AddAnnotation(ann) => {
                if project.canvas.annotations.iter().any(|a| a.id == ann.id) {
                    return Err(CoreError::DuplicateId(ann.id.clone()));
                }
                let shape = normalize_and_validate_shape(ann.shape.clone())?;
                let stored = crate::canvas::Annotation {
                    id: ann.id.clone(),
                    shape,
                    // Whiteboard marks are project-global by definition — they
                    // never carry a frame range (canvas.rs's own documented
                    // invariant). Normalize here, alongside every other
                    // validate-then-apply constraint, so a malformed payload or
                    // future agent-facing tool can't store a stale/misleading
                    // range on a Whiteboard mark.
                    linked_range_us: if ann.space == crate::canvas::AnnotationSpace::Whiteboard {
                        None
                    } else {
                        ann.linked_range_us
                    },
                    space: ann.space,
                };
                project.canvas.annotations.push(stored.clone());
                Ok((
                    Patch::one(PatchKind::AnnotationAdded, &stored.id),
                    Command::RemoveAnnotation { id: stored.id },
                ))
            }

            Command::RemoveAnnotation { id } => {
                let idx = project
                    .canvas
                    .annotations
                    .iter()
                    .position(|a| a.id == *id)
                    .ok_or_else(|| CoreError::AnnotationNotFound(id.clone()))?;
                let removed = project.canvas.annotations.remove(idx);
                Ok((
                    Patch::one(PatchKind::AnnotationRemoved, id),
                    Command::AddAnnotation(removed),
                ))
            }

            Command::ClearCanvas { space } => {
                let removed: Vec<crate::canvas::Annotation> = match space {
                    None => std::mem::take(&mut project.canvas.annotations),
                    Some(target) => {
                        let (matched, kept): (Vec<_>, Vec<_>) = project
                            .canvas
                            .annotations
                            .drain(..)
                            .partition(|a| a.space == *target);
                        project.canvas.annotations = kept;
                        matched
                    }
                };
                let ids: Vec<String> = removed.iter().map(|a| a.id.clone()).collect();
                Ok((
                    Patch {
                        kind: PatchKind::CanvasCleared,
                        ids,
                        entities: None,
                    },
                    Command::RestoreCanvas {
                        annotations: removed,
                        cleared_space: *space,
                    },
                ))
            }

            Command::RestoreCanvas {
                annotations,
                cleared_space,
            } => {
                for a in annotations {
                    if project.canvas.annotations.iter().any(|x| x.id == a.id) {
                        return Err(CoreError::DuplicateId(a.id.clone()));
                    }
                }
                let ids: Vec<String> = annotations.iter().map(|a| a.id.clone()).collect();
                project.canvas.annotations.extend(annotations.iter().cloned());
                // Inverse: echo back the EXACT scope the original ClearCanvas
                // carried, so redo re-removes precisely the same subset — an
                // empty/no-op restore round-trips to the same no-op scoped
                // clear instead of escalating to an unscoped full wipe.
                Ok((
                    Patch {
                        kind: PatchKind::CanvasRestored,
                        ids,
                        entities: None,
                    },
                    Command::ClearCanvas {
                        space: *cleared_space,
                    },
                ))
            }

            Command::MoveAnnotation { id, dx, dy } => {
                // Threat T-14.3-01 (Tampering): reject a non-finite delta before
                // it poisons a coordinate — clamp alone does NOT sanitize NaN
                // (NaN + anything is NaN, and f64::clamp with a NaN input is
                // implementation-defined). Reject before any mutation so Err
                // leaves the project untouched.
                if !dx.is_finite() || !dy.is_finite() {
                    return Err(CoreError::InvalidAnnotation(
                        "move delta must be finite".into(),
                    ));
                }
                let ann = project
                    .canvas
                    .annotations
                    .iter_mut()
                    .find(|a| a.id == *id)
                    .ok_or_else(|| CoreError::AnnotationNotFound(id.clone()))?;
                // EXACT prior state, captured BEFORE mutation — the inverse
                // carries this verbatim (never a negated delta; per-point
                // clamping is non-linear, the 14.2 CR-01 lesson).
                let prior_shape = ann.shape.clone();
                ann.shape = translate_shape(&prior_shape, *dx, *dy);
                // INVARIANT (WR-01): reposition/re-edit is a WHITEBOARD-space
                // affordance only — the frontend hit-tests exclusively
                // `space == "whiteboard"` marks (main.ts panelSelectPointerDown /
                // the dblclick re-edit both `continue`/skip other spaces). This is
                // why both AnnotationMoved consumers — native_surface.rs's
                // PROJECT_CHANGED_EVENT overlay allow-list and
                // canvas_overlay::apply_patch_to_overlay — deliberately OMIT this
                // kind: whiteboard marks never reach the FrameLinked Preview
                // overlay, so an AnnotationMoved cannot desync it. If a future
                // phase adds frame-linked moves, add AnnotationMoved to those two
                // consumers (they already refresh from the authoritative list on
                // AnnotationRemoved) OR reject a non-whiteboard move here.
                Ok((
                    Patch::one(PatchKind::AnnotationMoved, id),
                    Command::SetAnnotationShape {
                        id: id.clone(),
                        shape: prior_shape,
                    },
                ))
            }

            Command::SetAnnotationShape { id, shape } => {
                // Validate the incoming shape FIRST (defense in depth for the
                // renderer-facing text-re-edit use — clamp coords, truncate
                // label, reject empty/oversized) so an Err leaves the project
                // untouched; locate the id only after it passes.
                let validated = normalize_and_validate_shape(shape.clone())?;
                let ann = project
                    .canvas
                    .annotations
                    .iter_mut()
                    .find(|a| a.id == *id)
                    .ok_or_else(|| CoreError::AnnotationNotFound(id.clone()))?;
                let prior_shape = ann.shape.clone();
                ann.shape = validated;
                Ok((
                    Patch::one(PatchKind::AnnotationMoved, id),
                    // Self-inverse pair (like SetClipVolume): the inverse is
                    // another SetAnnotationShape carrying the exact prior shape,
                    // so undo AND redo stay exact across repeated moves/edits.
                    Command::SetAnnotationShape {
                        id: id.clone(),
                        shape: prior_shape,
                    },
                ))
            }

            Command::SetProjectSettings { fps, width, height } => {
                // Validate BEFORE mutating (T-18-02: reject non-finite/<=0/
                // above-ceiling fps and 0/oversized resolution — the Phase-16
                // volume-overflow lesson generalized). All-or-nothing: an Err
                // leaves every field untouched.
                //
                // 18-REVIEW MED-01: the fps ceiling is the fps twin of the
                // 7680 width/height bound. Without it, a finite-but-extreme
                // fps (e.g. 1e9) floors frame_step_us at 1 microsecond and
                // turns every per-tick export scan/encode loop into a
                // per-MICROSECOND loop over the whole timeline. 240 is
                // generous for real video (2x the highest common HFR).
                const MAX_FPS: f64 = 240.0; // inclusive ceiling
                if !fps.is_finite() || *fps <= 0.0 || *fps > MAX_FPS {
                    return Err(CoreError::InvalidSettings(format!(
                        "fps must be finite and in (0, {MAX_FPS}], got {fps}"
                    )));
                }
                const MAX_DIMENSION: u32 = 7680; // 8K ceiling, inclusive
                for (name, value) in [("width", *width), ("height", *height)] {
                    if value == 0 || value > MAX_DIMENSION {
                        return Err(CoreError::InvalidSettings(format!(
                            "{name} must be in 1..={MAX_DIMENSION}, got {value}"
                        )));
                    }
                }
                let (old_fps, old_width, old_height) =
                    (project.fps, project.width, project.height);
                project.fps = *fps;
                project.width = *width;
                project.height = *height;
                Ok((
                    Patch::one(PatchKind::ProjectSettingsChanged, "project"),
                    Command::SetProjectSettings {
                        fps: old_fps,
                        width: old_width,
                        height: old_height,
                    },
                ))
            }

            Command::SetClipTransform { id, transform } => {
                // T-18-01: reject any non-finite component BEFORE mutating
                // (the ensure_finite_point precedent — f32::clamp/arithmetic
                // would carry NaN/inf into every downstream denormalizer).
                let components = [
                    transform.position.0,
                    transform.position.1,
                    transform.scale.0,
                    transform.scale.1,
                    transform.rotation_deg,
                ];
                if components.iter().any(|v| !v.is_finite()) {
                    return Err(CoreError::InvalidSettings(format!(
                        "clip transform components must be finite, got \
                         position ({}, {}) scale ({}, {}) rotation {}",
                        transform.position.0,
                        transform.position.1,
                        transform.scale.0,
                        transform.scale.1,
                        transform.rotation_deg
                    )));
                }
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                // M-01: `scale == (0,0)` is the text auto-fit SENTINEL (a
                // synthetic marker the renderer interprets as "omit transform →
                // auto-fit at natural size, top-left"). An EXPLICIT (0,0) on a
                // text clip would collide with that sentinel — a zero-area dest
                // rect renders nothing anyway, so we reject it here so (0,0) is
                // ONLY reachable by OMITTING the transform (the genuine auto-fit
                // intent), never by an explicit request/keyframe/SetClipTransform.
                if clip.text.is_some() && transform.scale == (0.0, 0.0) {
                    return Err(CoreError::InvalidSettings(
                        "explicit scale (0, 0) is reserved as the text auto-fit \
                         sentinel; a zero-size text clip renders nothing — omit \
                         the transform to auto-fit, or use a positive scale"
                            .into(),
                    ));
                }
                let old_transform = clip.transform;
                clip.transform = *transform;
                // Phase 43 (LAT-02): attach the POST-mutation clip.
                let patch = Patch::one(PatchKind::ClipTransformChanged, id).with_clip(clip);
                Ok((
                    patch,
                    Command::SetClipTransform {
                        id: id.clone(),
                        transform: old_transform,
                    },
                ))
            }

            Command::SetClipOpacity { id, opacity } => {
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                let old_opacity = clip.opacity;
                // Clamp into [0,1]. Written as max-then-min, NOT f32::clamp:
                // f32::max maps a NaN input to 0.0, while clamp would pass
                // NaN through (the SetClipVolume discipline, T-18-01).
                clip.opacity = opacity.max(0.0).min(1.0);
                // Phase 43 (LAT-02): attach the POST-mutation clip.
                let patch = Patch::one(PatchKind::ClipOpacityChanged, id).with_clip(clip);
                Ok((
                    patch,
                    Command::SetClipOpacity {
                        id: id.clone(),
                        opacity: old_opacity,
                    },
                ))
            }

            // Phase 28 (OVL-01): mirrors SetClipOpacity EXACTLY — a two-variant
            // enum needs no clamp/validation. Self-inverse via the captured
            // prior mode.
            Command::SetClipAlphaMode { id, alpha_mode } => {
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                let old = clip.alpha_mode;
                clip.alpha_mode = *alpha_mode;
                Ok((
                    Patch::one(PatchKind::ClipAlphaModeChanged, id),
                    Command::SetClipAlphaMode {
                        id: id.clone(),
                        alpha_mode: old,
                    },
                ))
            }

            Command::SetClipCrop { id, crop } => {
                // Clamp each inset into [0,1] (max-then-min: NaN -> 0.0),
                // then REJECT a crop that leaves no source pixels (documented
                // decision: clamp per-inset, reject the no-source combination
                // — silently "fixing" a wipe-out crop would fabricate intent).
                let c = |v: f32| v.max(0.0).min(1.0);
                let clamped = ClipCrop {
                    left: c(crop.left),
                    top: c(crop.top),
                    right: c(crop.right),
                    bottom: c(crop.bottom),
                };
                if clamped.left + clamped.right >= 1.0 || clamped.top + clamped.bottom >= 1.0 {
                    return Err(CoreError::InvalidSettings(format!(
                        "crop leaves no source: left+right and top+bottom must \
                         each be < 1, got {}+{} horizontal, {}+{} vertical",
                        clamped.left, clamped.right, clamped.top, clamped.bottom
                    )));
                }
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                let old_crop = clip.crop;
                clip.crop = clamped;
                // Phase 43 (LAT-02): attach the POST-mutation clip.
                let patch = Patch::one(PatchKind::ClipCropChanged, id).with_clip(clip);
                Ok((
                    patch,
                    Command::SetClipCrop {
                        id: id.clone(),
                        crop: old_crop,
                    },
                ))
            }

            // --------------------------------------------------------------
            // Quick task 260730-x2t: time remap.
            // --------------------------------------------------------------
            Command::SetClipRetime { id, retime } => {
                // Validate-then-apply (T-18-01 discipline, threat T-x2t-01):
                // find the clip FIRST so an unknown id cannot be masked by a
                // validation error, then sanitize the payload — curve bounds,
                // key cap, sort/dedup — and rebuild the derived occupancy from
                // the clip's CURRENT source span. Nothing is mutated until
                // both succeed.
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let project_fps = project.fps;
                let span = {
                    let c = &project.timeline.tracks[ti].clips[ci];
                    c.out_us - c.in_us
                };
                let normalized = sanitized_retime(retime.as_ref(), project_fps, span)?;
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                let old = clip.retime.take();
                clip.retime = normalized;
                // Phase 43 (LAT-02): attach the POST-mutation clip so the
                // renderer's mirror picks up the new occupancy immediately.
                let patch = Patch::one(PatchKind::ClipRetimeChanged, id).with_clip(clip);
                Ok((
                    patch,
                    // Self-inverse carrying the OLD Option<Retime>
                    // (SetClipVolume pattern) — undo restores it exactly.
                    Command::SetClipRetime {
                        id: id.clone(),
                        retime: old,
                    },
                ))
            }

            // --------------------------------------------------------------
            // Phase 19 (COMP-04): keyframe animation.
            // --------------------------------------------------------------
            Command::SetKeyframes { id, track } => {
                // Validate-then-apply (T-18-01 discipline): ALL validation
                // before ANY mutation.
                // (a) Length cap FIRST — bounds LLM/MCP-supplied arrays
                // before any further work (T-19-01).
                if track.len() > MAX_KEYFRAMES_PER_TRACK {
                    return Err(CoreError::InvalidSettings(format!(
                        "keyframe track '{}' has {} keys, exceeds the \
                         {MAX_KEYFRAMES_PER_TRACK} cap",
                        track.name(),
                        track.len()
                    )));
                }
                // (b) sort + duplicate-reject, (c) per-property value
                // clamps/rejects mirroring the static setters.
                let normalized = normalize_keyframe_track(track)?;
                // (d) Find the clip, then replace-and-return-old — an empty
                // Vec == clear (D-01).
                let (ti, ci) =
                    find_clip(project, id).ok_or_else(|| CoreError::ClipNotFound(id.clone()))?;
                let clip = &mut project.timeline.tracks[ti].clips[ci];
                let old = clip.keyframes.replace(normalized);
                // (e) Self-inverse carrying the OLD same-property track
                // (SetClipVolume pattern) — undo restores it exactly.
                Ok((
                    Patch::one(PatchKind::ClipKeyframesChanged, id),
                    Command::SetKeyframes {
                        id: id.clone(),
                        track: old,
                    },
                ))
            }

            // --------------------------------------------------------------
            // Phase 20 (TEXT-01): text overlays as real clips.
            // --------------------------------------------------------------
            Command::AddText { track_index, clip } => {
                // Structural checks mirror AddClip (track range, dup id, times)
                // — but SKIP the media_bin referential check: a text clip's
                // media_id is the empty sentinel. Validate-then-apply: every
                // check runs before the push (Err leaves the project untouched).
                let track_count = project.timeline.tracks.len();
                if *track_index >= track_count {
                    return Err(CoreError::TrackOutOfRange(*track_index, track_count));
                }
                if project
                    .timeline
                    .tracks
                    .iter()
                    .flat_map(|t| t.clips.iter())
                    .any(|c| c.id == clip.id)
                {
                    return Err(CoreError::DuplicateId(clip.id.clone()));
                }
                // A text clip MUST carry a payload (that is what makes it text).
                let payload = clip.text.as_ref().ok_or_else(|| {
                    CoreError::InvalidSettings(format!(
                        "AddText clip {} carries no text payload",
                        clip.id
                    ))
                })?;
                validate_text_payload(payload)?;
                if clip.in_us < 0 || clip.out_us <= clip.in_us {
                    return Err(CoreError::InvalidTimes(format!(
                        "text clip {}: in_us={} out_us={} (need in>=0, out>in)",
                        clip.id, clip.in_us, clip.out_us
                    )));
                }
                // Negative timeline start clamps to 0 (the AddClip precedent);
                // the inverse is RemoveClip either way.
                let mut clip = clip.clone();
                clip.start_us = clip.start_us.max(0);
                // A whole `Clip` off the general dispatch IPC surface: validate
                // the curve and REBUILD the derived occupancy from the clip's
                // own span (260730-x2t WR-01, T-x2t-01/T-x2t-08) — the same
                // treatment AddClip gets. `AddText` is agent-reachable and
                // already validates `text`/`in_us`/`out_us` immediately above,
                // so an unvalidated `retime` was the one hole left in it.
                clip.retime =
                    sanitized_retime(clip.retime.as_ref(), project.fps, clip.out_us - clip.in_us)?;
                project.timeline.tracks[*track_index].clips.push(clip.clone());
                Ok((
                    Patch::one(PatchKind::ClipAdded, &clip.id),
                    Command::RemoveClip {
                        id: clip.id.clone(),
                    },
                ))
            }

            Command::UpdateText { clip_id, patch } => {
                let (ti, ci) = find_clip(project, clip_id)
                    .ok_or_else(|| CoreError::ClipNotFound(clip_id.clone()))?;
                // Reject a non-text target BEFORE mutating.
                let old_payload = match &project.timeline.tracks[ti].clips[ci].text {
                    Some(p) => p.clone(),
                    None => return Err(CoreError::NotATextClip(clip_id.clone())),
                };
                // Validate-then-apply: merge on a CLONE, re-validate the result,
                // and only then commit — an invalid patch leaves the clip exactly
                // as it was (validate-BEFORE-mutate, the SetClipCrop discipline).
                let mut merged = old_payload.clone();
                merged.style.apply_patch(patch);
                validate_text_payload(&merged)?;
                project.timeline.tracks[ti].clips[ci].text = Some(merged);
                Ok((
                    Patch::one(PatchKind::ClipTextChanged, clip_id),
                    // Exact inverse: restore the FULL prior payload.
                    Command::RestoreText {
                        clip_id: clip_id.clone(),
                        text: old_payload,
                    },
                ))
            }

            Command::RestoreText { clip_id, text } => {
                // Re-validate the incoming payload (defense in depth — it rides
                // the general dispatch IPC surface; idempotent no-op for a
                // legitimate undo). Validate BEFORE locating/mutating.
                validate_text_payload(text)?;
                let (ti, ci) = find_clip(project, clip_id)
                    .ok_or_else(|| CoreError::ClipNotFound(clip_id.clone()))?;
                let old_payload = match &project.timeline.tracks[ti].clips[ci].text {
                    Some(p) => p.clone(),
                    None => return Err(CoreError::NotATextClip(clip_id.clone())),
                };
                project.timeline.tracks[ti].clips[ci].text = Some(text.clone());
                Ok((
                    Patch::one(PatchKind::ClipTextChanged, clip_id),
                    // Self-inverse: another RestoreText carrying the prior payload.
                    Command::RestoreText {
                        clip_id: clip_id.clone(),
                        text: old_payload,
                    },
                ))
            }

            // --------------------------------------------------------------
            // Phase 18.1 (TL-06): track management.
            // --------------------------------------------------------------
            Command::AddTrack { kind } => {
                // Infallible: TrackKind is a closed 2-variant enum (malformed
                // kinds are already rejected at the serde boundary).
                //
                // Kind-aware placement (UAT GAP-1): the lowest-index lane is the
                // TOP compositing layer (index-0 = top, per Phase 18). Tracks are
                // laid out video-group-first, audio-group-last.
                //   - VIDEO: a new lane goes ON TOP of the video group — inserted
                //     at index 0 (becomes the new V1, existing lanes renumber
                //     down). The newest video track composites over the others.
                //   - AUDIO: a new lane goes at the BOTTOM of the audio group —
                //     appended at the end (becomes the highest A-number).
                // This preserves the video-prefix / audio-suffix invariant.
                let new_track = Track {
                    kind: *kind,
                    clips: Vec::new(),
                };
                let new_index = match kind {
                    TrackKind::Video => {
                        project.timeline.tracks.insert(0, new_track);
                        0
                    }
                    TrackKind::Audio => {
                        project.timeline.tracks.push(new_track);
                        project.timeline.tracks.len() - 1
                    }
                };
                Ok((
                    Patch::one(PatchKind::TrackAdded, &new_index.to_string()),
                    Command::RemoveTrack { index: new_index },
                ))
            }

            Command::RemoveTrack { index } => {
                // Validate-then-apply (T-18.1-01): on Err the project is
                // byte-unchanged. On success the removal is unconditional and
                // CASCADES — every clip on the track goes with it (mirrors
                // RemoveClip's unconditional-but-undoable removal). The
                // inverse carries the FULL removed track (kind + entire clip
                // Vec) so undo is an EXACT structural restore (T-18.1-04).
                let track_count = project.timeline.tracks.len();
                if *index >= track_count {
                    return Err(CoreError::TrackOutOfRange(*index, track_count));
                }
                let removed = project.timeline.tracks.remove(*index);
                Ok((
                    Patch::one(PatchKind::TrackRemoved, &index.to_string()),
                    Command::RestoreTrack {
                        index: *index,
                        track: removed,
                    },
                ))
            }

            Command::RestoreTrack { index, track } => {
                // Clamped insert (mirrors RestoreClip's clamped-insert
                // exactly — infallible, not sent by the renderer): a
                // corrupted/replayed undo-stack entry can never index out of
                // bounds or panic (T-18.1-02).
                let at = (*index).min(project.timeline.tracks.len());
                project.timeline.tracks.insert(at, track.clone());
                Ok((
                    Patch::one(PatchKind::TrackAdded, &at.to_string()),
                    Command::RemoveTrack { index: at },
                ))
            }

            Command::CreateMediaFolder { path } => {
                // AUTHORITATIVE path-format guard (25-REVIEW CR-01/WR-02/WR-03):
                // `dispatch_command` is a directly-invokable host command surface
                // (the C ABI today), so
                // a hand-crafted Command must NOT bypass the ".."/absolute/drive/
                // UNC/empty-segment/control-char/over-length reject-list the Tool
                // layer runs. Same shared validator, one reject-list.
                if !crate::model::is_valid_folder_path(path, false) {
                    return Err(CoreError::InvalidFolderPath(path.clone()));
                }
                if project.media_folders.iter().any(|f| f == path) {
                    return Err(CoreError::FolderAlreadyExists(path.clone()));
                }
                if let Some((parent, _)) = path.rsplit_once('/') {
                    if !project.media_folders.iter().any(|f| f == parent) {
                        return Err(CoreError::FolderNotFound(parent.to_string()));
                    }
                }
                project.media_folders.push(path.clone());
                Ok((
                    Patch::one(PatchKind::MediaFolderCreated, path),
                    Command::DeleteMediaFolder { path: path.clone() },
                ))
            }

            Command::DeleteMediaFolder { path } => {
                let idx = project
                    .media_folders
                    .iter()
                    .position(|f| f == path)
                    .ok_or_else(|| CoreError::FolderNotFound(path.clone()))?;
                let prefix = format!("{path}/");
                let has_child_folder = project
                    .media_folders
                    .iter()
                    .any(|f| f != path && f.starts_with(&prefix));
                let has_child_item = project
                    .media_bin
                    .iter()
                    .any(|m| m.folder == *path || m.folder.starts_with(&prefix));
                if has_child_folder || has_child_item {
                    return Err(CoreError::FolderNotEmpty(path.clone()));
                }
                project.media_folders.remove(idx);
                Ok((
                    Patch::one(PatchKind::MediaFolderDeleted, path),
                    Command::CreateMediaFolder { path: path.clone() },
                ))
            }

            Command::MoveMediaFolder { old_path, new_path } => {
                // AUTHORITATIVE path-format guard (25-REVIEW CR-01/WR-02/WR-03):
                // re-run the shared reject-list on `new_path` so a direct
                // `dispatch_command` cannot inject a "../"/drive/UNC/over-length
                // path into media_folders, bypassing the Tool layer.
                if !crate::model::is_valid_folder_path(new_path, false) {
                    return Err(CoreError::InvalidFolderPath(new_path.clone()));
                }
                if !project.media_folders.iter().any(|f| f == old_path) {
                    return Err(CoreError::FolderNotFound(old_path.clone()));
                }
                if new_path == old_path || new_path.starts_with(&format!("{old_path}/")) {
                    return Err(CoreError::FolderMoveIntoDescendant(
                        old_path.clone(),
                        new_path.clone(),
                    ));
                }
                if project.media_folders.iter().any(|f| f == new_path) {
                    return Err(CoreError::FolderAlreadyExists(new_path.clone()));
                }
                // WR-01: `new_path`'s immediate parent must already be registered
                // (mirrors CreateMediaFolder's parent-required invariant), so a
                // move can't orphan the entry under an unregistered parent segment.
                // The cycle check above already rejects moving under `old_path`
                // itself, so `parent == old_path` cannot reach here.
                if let Some((parent, _)) = new_path.rsplit_once('/') {
                    if !project.media_folders.iter().any(|f| f == parent) {
                        return Err(CoreError::FolderNotFound(parent.to_string()));
                    }
                }
                let prefix = format!("{old_path}/");
                let new_prefix = format!("{new_path}/");
                for f in project.media_folders.iter_mut() {
                    if f == old_path {
                        *f = new_path.clone();
                    } else if let Some(rest) = f.strip_prefix(&prefix) {
                        *f = format!("{new_prefix}{rest}");
                    }
                }
                for m in project.media_bin.iter_mut() {
                    if m.folder == *old_path {
                        m.folder = new_path.clone();
                    } else if let Some(rest) = m.folder.strip_prefix(&prefix) {
                        m.folder = format!("{new_prefix}{rest}");
                    }
                }
                Ok((
                    Patch {
                        kind: PatchKind::MediaFolderMoved,
                        ids: vec![old_path.clone(), new_path.clone()],
                        entities: None,
                    },
                    Command::MoveMediaFolder {
                        old_path: new_path.clone(),
                        new_path: old_path.clone(),
                    },
                ))
            }

            Command::MoveMediaItem { id, new_folder } => {
                if !new_folder.is_empty()
                    && !project.media_folders.iter().any(|f| f == new_folder)
                {
                    return Err(CoreError::FolderNotFound(new_folder.clone()));
                }
                let item = project
                    .media_bin
                    .iter_mut()
                    .find(|m| m.id == *id)
                    .ok_or_else(|| CoreError::MediaBinItemNotFound(id.clone()))?;
                let old_folder = std::mem::replace(&mut item.folder, new_folder.clone());
                Ok((
                    Patch::one(PatchKind::MediaItemMoved, id),
                    Command::MoveMediaItem {
                        id: id.clone(),
                        new_folder: old_folder,
                    },
                ))
            }

            Command::RenameMediaItem { id, display_name } => {
                if let Some(name) = display_name {
                    if name.is_empty() || name.len() > MAX_MEDIA_NAME_LEN {
                        return Err(CoreError::InvalidMediaName(name.clone()));
                    }
                }
                let item = project
                    .media_bin
                    .iter_mut()
                    .find(|m| m.id == *id)
                    .ok_or_else(|| CoreError::MediaBinItemNotFound(id.clone()))?;
                let old_name = std::mem::replace(&mut item.display_name, display_name.clone());
                Ok((
                    Patch::one(PatchKind::MediaItemRenamed, id),
                    Command::RenameMediaItem {
                        id: id.clone(),
                        display_name: old_name,
                    },
                ))
            }

            Command::RestoreClip { track, index, clip } => {
                let track_count = project.timeline.tracks.len();
                if *track >= track_count {
                    return Err(CoreError::TrackOutOfRange(*track, track_count));
                }
                if project
                    .timeline
                    .tracks
                    .iter()
                    .flat_map(|t| t.clips.iter())
                    .any(|c| c.id == clip.id)
                {
                    return Err(CoreError::DuplicateId(clip.id.clone()));
                }
                if !project.media_bin.iter().any(|m| m.id == clip.media_id) {
                    return Err(CoreError::MediaBinItemNotFound(clip.media_id.clone()));
                }
                // Whole-`Clip` carrier off the general dispatch IPC surface
                // (260730-x2t WR-01): validate the curve and REBUILD the derived
                // occupancy before it lands, exactly as AddClip does. Rebuilding
                // is EXACT for the legitimate undo path (`timeline_len_us` is a
                // pure function of curve/fps/span, and the span rides along on
                // this same clip), so redo-exactness is unaffected.
                let mut clip = clip.clone();
                clip.retime =
                    sanitized_retime(clip.retime.as_ref(), project.fps, clip.out_us - clip.in_us)?;
                let clips = &mut project.timeline.tracks[*track].clips;
                let at = (*index).min(clips.len());
                clips.insert(at, clip.clone());
                Ok((
                    Patch::one(PatchKind::ClipAdded, &clip.id),
                    Command::RemoveClip {
                        id: clip.id.clone(),
                    },
                ))
            }
        }
    }
}

/// Phase 43 (LAT-02): a post-mutation entity value, carried on [`Patch`] so
/// the renderer can apply common edits incrementally instead of calling
/// `get_snapshot`. Internally tagged (`{"type": "clip", ...fields}` /
/// `{"type": "media_bin_item", ...fields}`) — matches [`PatchKind`]'s own
/// `rename_all = "snake_case"` convention.
///
/// (No `Eq`: `Clip`/`MediaBinItem` carry `f32`/`f64` fields — the same
/// reasoning that dropped `Eq` from [`Command`] in Phase 3.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntitySnapshot {
    Clip(Clip),
    MediaBinItem(MediaBinItem),
}

/// What changed, emitted to the renderer as the `project:changed` event
/// payload after every successful dispatch/undo/redo. The renderer treats
/// this as an invalidation hint and refetches (or later, patches) its
/// read-only mirror — it never mutates state itself.
///
/// (`Eq` dropped in Phase 43 for the same reason it was dropped from
/// [`Command`] in Phase 3: `entities` carries `Clip`/`MediaBinItem`, whose
/// `f32`/`f64` fields are not `Eq`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub kind: PatchKind,
    /// Ids of the affected entities (clip ids or media bin item ids).
    pub ids: Vec<String>,
    /// Phase 43 (LAT-02): post-mutation values for the entities `ids` names,
    /// when this kind is one of the ~6 the renderer applies incrementally.
    /// `None` for every other kind (turns, tracks, folders, canvas, bulk
    /// ops) — the renderer's fallback for `None` is `get_entities(ids)`,
    /// NEVER `get_snapshot`. Additive/byte-compatible: `kind`/`ids` are
    /// unchanged; this is a new optional field, `#[serde(default)]` so it
    /// round-trips even if absent and `skip_serializing_if` so an
    /// entity-less patch is byte-identical to the pre-Phase-43 wire shape
    /// (the three non-frontend consumers — `canvas_overlay.rs`'s
    /// `apply_patch_to_overlay` and both `native_surface.rs` listeners —
    /// read only `.kind`/`.ids` and are unaffected either way).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entities: Option<Vec<EntitySnapshot>>,
}

impl Patch {
    fn one(kind: PatchKind, id: &str) -> Self {
        Self {
            kind,
            ids: vec![id.to_string()],
            entities: None,
        }
    }

    /// Phase 43 (LAT-02): attach post-mutation entity values.
    fn with_entities(mut self, entities: Vec<EntitySnapshot>) -> Self {
        self.entities = Some(entities);
        self
    }

    /// Phase 43 (LAT-02): the common single-clip case — attach exactly one
    /// post-mutation [`Clip`].
    fn with_clip(self, clip: &Clip) -> Self {
        self.with_entities(vec![EntitySnapshot::Clip(clip.clone())])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchKind {
    MediaBinItemAdded,
    MediaBinItemRemoved,
    ClipAdded,
    ClipMoved,
    ClipRemoved,
    // Phase 6: edit operations + audio.
    ClipTrimmed,
    /// ids = [left_id, right_id].
    ClipSplit,
    /// ids = [left_id, removed_right_id].
    ClipMerged,
    ClipVolumeChanged,
    /// ids = [video_clip_id, new_audio_clip_id].
    AudioDetached,
    /// ids = [video_clip_id, removed_audio_clip_id].
    AudioReattached,
    // ------------------------------------------------------------------
    // Phase 11 (AGENT-03): one agent turn = one undo entry.
    // ------------------------------------------------------------------
    /// Aggregate kind for a >1-command turn's UNDO — `ids` covers every
    /// touched entity across the whole turn. Never produced by a single
    /// `dispatch()` outside an open turn (that keeps its ORIGINAL specific
    /// kind, e.g. `ClipTrimmed`); only when a turn's undo group has 2+ members.
    /// A notification-only variant — adds NO new mutation primitive (SC-5 is
    /// about `Command`, not `PatchKind`).
    TurnReverted,
    /// Aggregate kind for a >1-command turn's REDO — the forward-direction
    /// mirror of `TurnReverted`. `ids` covers every touched entity re-applied
    /// across the whole turn.
    TurnApplied,
    // ------------------------------------------------------------------
    // Phase 13 (CANV-01): canvas annotations.
    // ------------------------------------------------------------------
    AnnotationAdded,
    AnnotationRemoved,
    /// ids = every removed annotation id, in original order.
    CanvasCleared,
    /// ids = every restored annotation id, in original order.
    CanvasRestored,
    // ------------------------------------------------------------------
    // Phase 14.3 (CANV-01): annotation reposition / re-edit.
    // ------------------------------------------------------------------
    /// ids = [moved/re-edited annotation id]. Reused for the forward
    /// MoveAnnotation/SetAnnotationShape AND their inverse/redo re-application
    /// (mirrors how RestoreClip reuses ClipAdded for a single-entity inverse).
    /// NOTE: the two whiteboard-only PatchKind consumers (the overlay-refresh
    /// allow-list and `apply_patch_to_overlay`, both now in
    /// `crates/ffi/src/panel/overlay.rs`) intentionally omit this kind —
    /// documented in Plan 14.3-02, which owned those files back when they lived
    /// in `src-tauri` (whiteboard marks never reach the Preview overlay).
    AnnotationMoved,
    // ------------------------------------------------------------------
    // Phase 18 (COMP-01): project timebase.
    // ------------------------------------------------------------------
    /// ids = ["project"] (a singleton — there is exactly one settings blob).
    ProjectSettingsChanged,
    // ------------------------------------------------------------------
    // Phase 18 (TOOL-03): per-clip visual properties.
    // ------------------------------------------------------------------
    ClipTransformChanged,
    ClipOpacityChanged,
    ClipCropChanged,
    // ------------------------------------------------------------------
    // Phase 28 (OVL-01): per-clip alpha interpretation.
    // ------------------------------------------------------------------
    /// ids = [the clip whose alpha_mode was toggled].
    ClipAlphaModeChanged,
    // ------------------------------------------------------------------
    // Quick task 260730-x2t: time remap.
    // ------------------------------------------------------------------
    /// ids = [the clip whose `retime` was set/cleared]. Emitted by
    /// `SetClipRetime`. Unlike the other per-property kinds this one changes
    /// the clip's timeline OCCUPANCY, so a consumer that caches clip geometry
    /// must refresh it exactly as it does for `ClipTrimmed`. A
    /// notification-only variant — adds NO new mutation primitive.
    ClipRetimeChanged,
    // ------------------------------------------------------------------
    // Phase 19 (COMP-04): keyframe animation.
    // ------------------------------------------------------------------
    /// ids = [the clip whose keyframe track was replaced/cleared]. NOTE: the
    /// two FrameLinked-only canvas patch consumers (native_surface.rs
    /// overlay allow-list + canvas_overlay.rs::apply_patch_to_overlay)
    /// intentionally omit this kind, like AnnotationMoved — keyframe changes
    /// never touch the ink overlay.
    ClipKeyframesChanged,
    // ------------------------------------------------------------------
    // Phase 20 (TEXT-01): text overlays.
    // ------------------------------------------------------------------
    /// ids = [the text clip whose payload was merged/restored]. Emitted by
    /// UpdateText/RestoreText. A notification-only variant — adds NO new
    /// mutation primitive.
    ClipTextChanged,
    // ------------------------------------------------------------------
    // Phase 18.1 (TL-06): track management.
    // ------------------------------------------------------------------
    /// ids = [appended/restored track index as a string] — a synthetic
    /// singleton id (`Track` has no natural entity id; mirrors
    /// `ProjectSettingsChanged`'s `ids: ["project"]` precedent).
    TrackAdded,
    /// ids = [removed track index as a string].
    TrackRemoved,
    // ------------------------------------------------------------------
    // Phase 25 (LIB-01): media library folders.
    // ------------------------------------------------------------------
    /// ids = [created folder path].
    MediaFolderCreated,
    /// ids = [deleted folder path].
    MediaFolderDeleted,
    /// ids = [old_path, new_path]. Emitted by MoveMediaFolder, which covers
    /// BOTH a same-parent rename and a cross-parent move (path-addressing
    /// makes them the same primitive -- see Plan 25-02).
    MediaFolderMoved,
    /// ids = [media item id].
    MediaItemMoved,
    /// ids = [media item id].
    MediaItemRenamed,
    // ------------------------------------------------------------------
    // Phase 26 (LIB-02) follow-up: project switch (live-UAT MULTIPROJECT-UI).
    // ------------------------------------------------------------------
    /// The ACTIVE project was swapped wholesale (new_project / open_project —
    /// the Pattern-C interceptions that drive `Store::from_project`). ids =
    /// ["project"] (the `ProjectSettingsChanged` singleton precedent). A
    /// notification-only variant — adds NO new mutation primitive and NO undo
    /// entry (the switch itself is deliberately not undoable); its ONLY job is
    /// the `project:changed` emission that makes the renderer rebuild its
    /// mirror from `get_snapshot` and the native preview re-present, so the UI
    /// can never keep showing the PREVIOUS project after a switch.
    ProjectSwitched,
}

#[cfg(test)]
mod canvas_command_tests {
    //! Phase 13 (CANV-01) annotation-command behaviors: validate-then-apply
    //! (clamp coords, reject structural violations, truncate overlong text)
    //! and exact-inverse undo through both `Command::apply` and `Store`.

    use super::*;
    use crate::canvas::{Annotation, AnnotationShape, AnnotationSpace, NormPoint};
    use crate::Store;

    fn p(x: f64, y: f64) -> NormPoint {
        NormPoint { x, y }
    }

    fn stroke(id: &str, points: Vec<NormPoint>) -> Annotation {
        Annotation {
            id: id.into(),
            shape: AnnotationShape::Stroke { points },
            linked_range_us: Some((1_000_000, 1_000_000)),
            space: AnnotationSpace::FrameLinked,
        }
    }

    fn three_point_stroke(id: &str) -> Annotation {
        stroke(id, vec![p(0.1, 0.1), p(0.2, 0.2), p(0.3, 0.3)])
    }

    #[test]
    fn add_annotation_appends_and_inverse_is_remove() {
        let mut project = Project::new();
        let ann = three_point_stroke("a1");
        let (patch, inverse) = Command::AddAnnotation(ann.clone())
            .apply(&mut project)
            .expect("add annotation");
        assert_eq!(project.canvas.annotations, vec![ann]);
        assert_eq!(patch.kind, PatchKind::AnnotationAdded);
        assert_eq!(patch.ids, vec!["a1".to_string()]);
        assert_eq!(inverse, Command::RemoveAnnotation { id: "a1".into() });
    }

    #[test]
    fn add_annotation_duplicate_id_rejected_and_project_unchanged() {
        let mut project = Project::new();
        Command::AddAnnotation(three_point_stroke("a1"))
            .apply(&mut project)
            .expect("first add");
        let before = project.clone();
        let err = Command::AddAnnotation(three_point_stroke("a1"))
            .apply(&mut project)
            .expect_err("duplicate id must be rejected");
        assert!(matches!(err, CoreError::DuplicateId(id) if id == "a1"));
        assert_eq!(project, before, "Err must leave project untouched");
    }

    #[test]
    fn add_annotation_empty_stroke_rejected_and_project_unchanged() {
        let mut project = Project::new();
        let before = project.clone();
        let err = Command::AddAnnotation(stroke("a1", vec![]))
            .apply(&mut project)
            .expect_err("empty stroke must be rejected");
        assert!(matches!(err, CoreError::InvalidAnnotation(_)));
        assert_eq!(project, before);
    }

    #[test]
    fn add_annotation_two_point_lasso_rejected_and_project_unchanged() {
        let mut project = Project::new();
        let before = project.clone();
        let err = Command::AddAnnotation(Annotation {
            id: "a1".into(),
            shape: AnnotationShape::Lasso {
                points: vec![p(0.1, 0.1), p(0.2, 0.2)],
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        })
        .apply(&mut project)
        .expect_err("2-point lasso must be rejected (needs >= 3)");
        assert!(matches!(err, CoreError::InvalidAnnotation(_)));
        assert_eq!(project, before);
    }

    #[test]
    fn add_annotation_oversized_stroke_rejected_and_project_unchanged() {
        let mut project = Project::new();
        let before = project.clone();
        let points: Vec<NormPoint> = (0..=MAX_ANNOTATION_POINTS) // 501 points
            .map(|i| p(i as f64 / 1000.0, 0.5))
            .collect();
        assert_eq!(points.len(), 501);
        let err = Command::AddAnnotation(stroke("a1", points))
            .apply(&mut project)
            .expect_err("501-point stroke must be rejected");
        assert!(matches!(err, CoreError::InvalidAnnotation(_)));
        assert_eq!(project, before);
    }

    #[test]
    fn add_annotation_clamps_out_of_range_coords_instead_of_rejecting() {
        let mut project = Project::new();
        Command::AddAnnotation(stroke("a1", vec![p(1.5, -0.2)]))
            .apply(&mut project)
            .expect("out-of-range coords are clamped, never rejected");
        match &project.canvas.annotations[0].shape {
            AnnotationShape::Stroke { points } => {
                assert_eq!(points, &vec![p(1.0, 0.0)], "x clamps to 1.0, y to 0.0");
            }
            other => panic!("expected stroke, got {other:?}"),
        }
    }

    #[test]
    fn add_annotation_truncates_overlong_label_text_instead_of_rejecting() {
        let mut project = Project::new();
        Command::AddAnnotation(Annotation {
            id: "a1".into(),
            shape: AnnotationShape::Label {
                position: p(0.5, 0.5),
                text: "a".repeat(600),
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        })
        .apply(&mut project)
        .expect("overlong label is truncated, never rejected");
        match &project.canvas.annotations[0].shape {
            AnnotationShape::Label { text, .. } => {
                assert_eq!(text.len(), MAX_LABEL_TEXT_LEN);
                assert_eq!(text, &"a".repeat(500));
            }
            other => panic!("expected label, got {other:?}"),
        }
    }

    #[test]
    fn add_annotation_whiteboard_mark_drops_any_linked_range() {
        // WR-01: a Whiteboard mark is project-global and must never store a
        // linked frame range, even if the payload supplies one.
        let mut project = Project::new();
        Command::AddAnnotation(Annotation {
            id: "w1".into(),
            shape: AnnotationShape::Stroke {
                points: vec![p(0.4, 0.4), p(0.5, 0.5), p(0.6, 0.6)],
            },
            // Deliberately non-None despite Whiteboard space.
            linked_range_us: Some((3_000_000, 3_000_000)),
            space: AnnotationSpace::Whiteboard,
        })
        .apply(&mut project)
        .expect("add whiteboard annotation");
        assert_eq!(
            project.canvas.annotations[0].linked_range_us, None,
            "a Whiteboard mark must be stored with linked_range_us: None"
        );
    }

    #[test]
    fn add_annotation_frame_linked_mark_keeps_its_linked_range() {
        // The normalization must NOT touch FrameLinked marks.
        let mut project = Project::new();
        Command::AddAnnotation(three_point_stroke("f1"))
            .apply(&mut project)
            .expect("add frame-linked annotation");
        assert_eq!(
            project.canvas.annotations[0].linked_range_us,
            Some((1_000_000, 1_000_000)),
            "a FrameLinked mark must keep its supplied range"
        );
    }

    #[test]
    fn remove_annotation_round_trips_exact_original_via_store_undo() {
        let mut store = Store::new();
        let ann = three_point_stroke("a1");
        store
            .dispatch(Command::AddAnnotation(ann.clone()))
            .expect("add");
        let (patch, _, _) = store
            .dispatch(Command::RemoveAnnotation { id: "a1".into() })
            .expect("remove");
        assert_eq!(patch.kind, PatchKind::AnnotationRemoved);
        assert!(store.snapshot().canvas.annotations.is_empty());

        // Undo of the removal re-adds the EXACT original annotation.
        let (undo_patch, _, _) = store.undo().expect("undo the removal");
        assert_eq!(undo_patch.kind, PatchKind::AnnotationAdded);
        assert_eq!(store.snapshot().canvas.annotations, vec![ann]);
    }

    #[test]
    fn remove_annotation_absent_id_errors() {
        let mut project = Project::new();
        let err = Command::RemoveAnnotation { id: "nope".into() }
            .apply(&mut project)
            .expect_err("absent id must error");
        assert!(matches!(err, CoreError::AnnotationNotFound(id) if id == "nope"));
    }

    #[test]
    fn clear_canvas_empties_and_inverse_restores_exact_order() {
        let mut project = Project::new();
        let anns = vec![
            three_point_stroke("a1"),
            Annotation {
                id: "a2".into(),
                shape: AnnotationShape::Arrow {
                    start: p(0.12, 0.30),
                    end: p(0.55, 0.62),
                },
                linked_range_us: None,
                space: AnnotationSpace::FrameLinked,
            },
            Annotation {
                id: "a3".into(),
                shape: AnnotationShape::Label {
                    position: p(0.9, 0.1),
                    text: "here".into(),
                },
                linked_range_us: Some((2_000_000, 2_000_000)),
                space: AnnotationSpace::FrameLinked,
            },
        ];
        for a in &anns {
            Command::AddAnnotation(a.clone())
                .apply(&mut project)
                .expect("add");
        }
        let (patch, inverse) = Command::ClearCanvas { space: None }
            .apply(&mut project)
            .expect("clear");
        assert!(project.canvas.annotations.is_empty());
        assert_eq!(patch.kind, PatchKind::CanvasCleared);
        assert_eq!(patch.ids, vec!["a1", "a2", "a3"]);

        // Applying the inverse restores the EXACT original set, same order.
        let (restore_patch, restore_inverse) =
            inverse.apply(&mut project).expect("restore canvas");
        assert_eq!(project.canvas.annotations, anns);
        assert_eq!(restore_patch.kind, PatchKind::CanvasRestored);
        // The original clear was unscoped (`space: None`), so the restore's
        // inverse echoes that EXACT scope back — a full clear on redo, not an
        // inferred per-space one.
        assert_eq!(
            restore_inverse,
            Command::ClearCanvas { space: None }
        );
    }

    #[test]
    fn store_dispatch_add_annotation_then_undo_restores_prior_canvas_state() {
        let mut store = Store::new();
        store
            .dispatch(Command::AddAnnotation(three_point_stroke("a1")))
            .expect("add a1");
        let before = store.snapshot().canvas.clone();

        store
            .dispatch(Command::AddAnnotation(three_point_stroke("a2")))
            .expect("add a2");
        assert_eq!(store.snapshot().canvas.annotations.len(), 2);

        store.undo().expect("undo add a2");
        assert_eq!(
            store.snapshot().canvas,
            before,
            "undo must restore the exact prior CanvasState"
        );

        // Redo re-applies, then a ClearCanvas + undo round-trips too.
        store.redo().expect("redo add a2");
        let full = store.snapshot().canvas.clone();
        store
            .dispatch(Command::ClearCanvas { space: None })
            .expect("clear");
        assert!(store.snapshot().canvas.annotations.is_empty());
        store.undo().expect("undo clear");
        assert_eq!(store.snapshot().canvas, full);
    }

    // --- Phase 14.2 (open-item-1): space-scoped ClearCanvas ------------

    fn whiteboard_stroke(id: &str) -> Annotation {
        Annotation {
            id: id.into(),
            shape: AnnotationShape::Stroke {
                points: vec![p(0.4, 0.4), p(0.5, 0.5), p(0.6, 0.6)],
            },
            linked_range_us: None,
            space: AnnotationSpace::Whiteboard,
        }
    }

    /// A project seeded with one FrameLinked mark ("f1") + one Whiteboard
    /// mark ("w1"), added through the real command path.
    fn mixed_space_project() -> Project {
        let mut project = Project::new();
        Command::AddAnnotation(three_point_stroke("f1"))
            .apply(&mut project)
            .expect("add frame-linked");
        Command::AddAnnotation(whiteboard_stroke("w1"))
            .apply(&mut project)
            .expect("add whiteboard");
        project
    }

    #[test]
    fn clear_canvas_none_clears_both_spaces_and_inverse_restores_both() {
        let mut project = mixed_space_project();
        let (patch, inverse) = Command::ClearCanvas { space: None }
            .apply(&mut project)
            .expect("clear all");
        assert!(project.canvas.annotations.is_empty());
        assert_eq!(patch.ids, vec!["f1", "w1"]);

        let (_p, _inv) = inverse.apply(&mut project).expect("restore both");
        let ids: Vec<&str> = project
            .canvas
            .annotations
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ids, vec!["f1", "w1"], "both restored in original order");
    }

    #[test]
    fn clear_canvas_some_whiteboard_leaves_frame_linked_and_undo_restores_exact_subset() {
        let mut project = mixed_space_project();
        let (patch, inverse) = Command::ClearCanvas {
            space: Some(AnnotationSpace::Whiteboard),
        }
        .apply(&mut project)
        .expect("clear whiteboard only");
        // Only the whiteboard mark removed; the frame-linked mark stays.
        assert_eq!(patch.ids, vec!["w1"]);
        let remaining: Vec<&str> = project
            .canvas
            .annotations
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(remaining, vec!["f1"], "frame-linked mark untouched");

        // Undo (the derived inverse) restores exactly the one whiteboard mark.
        let (_p, redo) = inverse.apply(&mut project).expect("restore whiteboard");
        let after: Vec<&str> = project
            .canvas
            .annotations
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(after, vec!["f1", "w1"], "exactly the removed subset back");
        // Redo re-removes exactly the whiteboard mark (space-derived inverse).
        assert_eq!(
            redo,
            Command::ClearCanvas {
                space: Some(AnnotationSpace::Whiteboard)
            }
        );
        redo.apply(&mut project).expect("redo clear whiteboard");
        let redone: Vec<&str> = project
            .canvas
            .annotations
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(redone, vec!["f1"], "redo leaves the frame-linked mark");
    }

    #[test]
    fn clear_canvas_scoped_empty_match_undo_redo_preserves_other_space() {
        // Regression (CR-01): a scoped clear that matches ZERO annotations
        // must NOT escalate to a full-canvas wipe on redo. Seed ONLY a
        // frame-linked mark, clear the (empty) whiteboard surface, then
        // undo+redo — the frame-linked mark must survive untouched.
        let mut project = Project::new();
        Command::AddAnnotation(three_point_stroke("f1"))
            .apply(&mut project)
            .expect("add frame-linked");

        // ClearCanvas{Some(Whiteboard)} matches nothing (no whiteboard marks).
        let (clear_patch, undo) = Command::ClearCanvas {
            space: Some(AnnotationSpace::Whiteboard),
        }
        .apply(&mut project)
        .expect("clear empty whiteboard");
        assert!(clear_patch.ids.is_empty(), "nothing matched the scope");
        // The undo carrier echoes the exact original scope, not an inferred one.
        assert_eq!(
            undo,
            Command::RestoreCanvas {
                annotations: vec![],
                cleared_space: Some(AnnotationSpace::Whiteboard),
            }
        );

        // Undo the no-op clear; its own inverse must be the SAME scoped clear,
        // never an unscoped full wipe.
        let (_p, redo) = undo.apply(&mut project).expect("undo the empty clear");
        assert_eq!(
            redo,
            Command::ClearCanvas {
                space: Some(AnnotationSpace::Whiteboard)
            },
            "redo must stay scoped to Whiteboard, not escalate to a full clear"
        );

        // Redo the clear — the frame-linked mark must still be present.
        redo.apply(&mut project).expect("redo the empty clear");
        let ids: Vec<&str> = project
            .canvas
            .annotations
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["f1"],
            "the unrelated frame-linked mark must survive undo+redo of an empty scoped clear"
        );
    }

    #[test]
    fn clear_canvas_some_frame_linked_leaves_whiteboard() {
        let mut project = mixed_space_project();
        let (patch, _inverse) = Command::ClearCanvas {
            space: Some(AnnotationSpace::FrameLinked),
        }
        .apply(&mut project)
        .expect("clear frame-linked only");
        assert_eq!(patch.ids, vec!["f1"]);
        let remaining: Vec<&str> = project
            .canvas
            .annotations
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(remaining, vec!["w1"], "whiteboard mark untouched");
    }

    // --- Phase 14.3 (D-07/D-08): MoveAnnotation + SetAnnotationShape ----

    fn assert_close(pt: &NormPoint, x: f64, y: f64) {
        assert!(
            (pt.x - x).abs() < 1e-9 && (pt.y - y).abs() < 1e-9,
            "expected ~({x}, {y}), got ({}, {})",
            pt.x,
            pt.y
        );
    }

    fn label(id: &str, x: f64, y: f64, text: &str) -> Annotation {
        Annotation {
            id: id.into(),
            shape: AnnotationShape::Label {
                position: p(x, y),
                text: text.into(),
            },
            linked_range_us: None,
            space: AnnotationSpace::Whiteboard,
        }
    }

    #[test]
    fn move_annotation_translates_every_shape_variant() {
        // Interior points (no clamping) so the translation is a clean +delta
        // for each of the four variants — the point is that ALL variants move.
        let (dx, dy) = (0.1, -0.05);

        // Stroke.
        {
            let mut store = Store::new();
            store
                .dispatch(Command::AddAnnotation(stroke(
                    "s1",
                    vec![p(0.20, 0.30), p(0.40, 0.50)],
                )))
                .expect("add stroke");
            store
                .dispatch(Command::MoveAnnotation {
                    id: "s1".into(),
                    dx,
                    dy,
                })
                .expect("move stroke");
            match &store.snapshot().canvas.annotations[0].shape {
                AnnotationShape::Stroke { points } => {
                    assert_close(&points[0], 0.30, 0.25);
                    assert_close(&points[1], 0.50, 0.45);
                }
                other => panic!("expected stroke, got {other:?}"),
            }
        }

        // Lasso (>= 3 points).
        {
            let mut store = Store::new();
            store
                .dispatch(Command::AddAnnotation(Annotation {
                    id: "l1".into(),
                    shape: AnnotationShape::Lasso {
                        points: vec![p(0.20, 0.30), p(0.40, 0.50), p(0.30, 0.60)],
                    },
                    linked_range_us: None,
                    space: AnnotationSpace::Whiteboard,
                }))
                .expect("add lasso");
            store
                .dispatch(Command::MoveAnnotation {
                    id: "l1".into(),
                    dx,
                    dy,
                })
                .expect("move lasso");
            match &store.snapshot().canvas.annotations[0].shape {
                AnnotationShape::Lasso { points } => {
                    assert_close(&points[0], 0.30, 0.25);
                    assert_close(&points[1], 0.50, 0.45);
                    assert_close(&points[2], 0.40, 0.55);
                }
                other => panic!("expected lasso, got {other:?}"),
            }
        }

        // Arrow (start + end).
        {
            let mut store = Store::new();
            store
                .dispatch(Command::AddAnnotation(Annotation {
                    id: "ar1".into(),
                    shape: AnnotationShape::Arrow {
                        start: p(0.20, 0.30),
                        end: p(0.60, 0.70),
                    },
                    linked_range_us: None,
                    space: AnnotationSpace::Whiteboard,
                }))
                .expect("add arrow");
            store
                .dispatch(Command::MoveAnnotation {
                    id: "ar1".into(),
                    dx,
                    dy,
                })
                .expect("move arrow");
            match &store.snapshot().canvas.annotations[0].shape {
                AnnotationShape::Arrow { start, end } => {
                    assert_close(start, 0.30, 0.25);
                    assert_close(end, 0.70, 0.65);
                }
                other => panic!("expected arrow, got {other:?}"),
            }
        }

        // Label (position moves, text unchanged).
        {
            let mut store = Store::new();
            store
                .dispatch(Command::AddAnnotation(label("lb1", 0.20, 0.30, "keep me")))
                .expect("add label");
            store
                .dispatch(Command::MoveAnnotation {
                    id: "lb1".into(),
                    dx,
                    dy,
                })
                .expect("move label");
            match &store.snapshot().canvas.annotations[0].shape {
                AnnotationShape::Label { position, text } => {
                    assert_close(position, 0.30, 0.25);
                    assert_eq!(text, "keep me", "Label.text must never move");
                }
                other => panic!("expected label, got {other:?}"),
            }
        }
    }

    #[test]
    fn move_annotation_undo_restores_exact_prior_shape_after_edge_clamp() {
        // THE Pitfall-1 / CR-01 regression (non-negotiable): a move that pushes
        // one point past the [0,1] edge clamps NON-LINEARLY. A negated-delta
        // inverse would restore that point to the WRONG place (0.90, not 0.98);
        // the exact-prior-shape inverse restores it byte-for-byte.
        let mut store = Store::new();
        let original = AnnotationShape::Stroke {
            points: vec![p(0.98, 0.50), p(0.30, 0.50)],
        };
        store
            .dispatch(Command::AddAnnotation(Annotation {
                id: "s1".into(),
                shape: original.clone(),
                linked_range_us: None,
                space: AnnotationSpace::Whiteboard,
            }))
            .expect("add stroke");

        // dx = +0.10 pushes 0.98 -> 1.0 (clamped, loses 0.08 of delta) while
        // 0.30 -> 0.40 (full delta) — a genuinely non-linear move.
        store
            .dispatch(Command::MoveAnnotation {
                id: "s1".into(),
                dx: 0.10,
                dy: 0.0,
            })
            .expect("move stroke to the edge");
        match &store.snapshot().canvas.annotations[0].shape {
            AnnotationShape::Stroke { points } => {
                assert_close(&points[0], 1.0, 0.50);
                assert_close(&points[1], 0.40, 0.50);
            }
            other => panic!("expected stroke, got {other:?}"),
        }

        // Undo: the shape must be BYTE-FOR-BYTE the pre-move shape. A negated
        // delta (-0.10) applied to the clamped 1.0 would give 0.90 — WRONG.
        store.undo().expect("undo the move");
        assert_eq!(
            store.snapshot().canvas.annotations[0].shape, original,
            "undo must restore the EXACT prior shape (0.98 back, not 0.90)"
        );

        // Redo: the moved (clamped) shape returns.
        store.redo().expect("redo the move");
        match &store.snapshot().canvas.annotations[0].shape {
            AnnotationShape::Stroke { points } => {
                assert_close(&points[0], 1.0, 0.50);
                assert_close(&points[1], 0.40, 0.50);
            }
            other => panic!("expected stroke, got {other:?}"),
        }
    }

    #[test]
    fn move_annotation_rejects_non_finite_delta() {
        let mut project = Project::new();
        Command::AddAnnotation(whiteboard_stroke("w1"))
            .apply(&mut project)
            .expect("add");
        let before = project.clone();
        let err = Command::MoveAnnotation {
            id: "w1".into(),
            dx: f64::NAN,
            dy: 0.0,
        }
        .apply(&mut project)
        .expect_err("NaN delta must be rejected");
        assert!(matches!(err, CoreError::InvalidAnnotation(_)));
        assert_eq!(project, before, "Err must leave the annotation untouched");
    }

    #[test]
    fn move_annotation_missing_id_errors() {
        let mut project = Project::new();
        let err = Command::MoveAnnotation {
            id: "nope".into(),
            dx: 0.1,
            dy: 0.1,
        }
        .apply(&mut project)
        .expect_err("missing id must error");
        assert!(matches!(err, CoreError::AnnotationNotFound(id) if id == "nope"));
    }

    #[test]
    fn normalize_and_validate_shape_rejects_non_finite_coordinate() {
        // WR-02: clamp does NOT sanitize NaN (f64::clamp passes NaN through),
        // so a NaN/inf coordinate submitted via the validate path — shared by
        // AddAnnotation AND SetAnnotationShape — must be REJECTED, mirroring the
        // MoveAnnotation finite-delta guard, and leave the project untouched.

        // AddAnnotation path: a NaN stroke coordinate is rejected.
        {
            let mut project = Project::new();
            let before = project.clone();
            let err = Command::AddAnnotation(stroke("a1", vec![p(f64::NAN, 0.5)]))
                .apply(&mut project)
                .expect_err("NaN coordinate must be rejected");
            assert!(matches!(err, CoreError::InvalidAnnotation(_)));
            assert_eq!(project, before, "Err must leave the project untouched");
        }

        // SetAnnotationShape path: an inf coordinate on re-edit is rejected.
        {
            let mut project = Project::new();
            Command::AddAnnotation(whiteboard_stroke("w1"))
                .apply(&mut project)
                .expect("add");
            let before = project.clone();
            let err = Command::SetAnnotationShape {
                id: "w1".into(),
                shape: AnnotationShape::Stroke {
                    points: vec![p(0.1, 0.1), p(f64::INFINITY, 0.2), p(0.3, 0.3)],
                },
            }
            .apply(&mut project)
            .expect_err("inf coordinate must be rejected");
            assert!(matches!(err, CoreError::InvalidAnnotation(_)));
            assert_eq!(project, before, "Err must leave the annotation untouched");
        }
    }

    #[test]
    fn set_annotation_shape_reedits_label_with_exact_undo_redo() {
        let mut store = Store::new();
        store
            .dispatch(Command::AddAnnotation(label("lb1", 0.4, 0.6, "old")))
            .expect("add label 'old'");

        store
            .dispatch(Command::SetAnnotationShape {
                id: "lb1".into(),
                shape: AnnotationShape::Label {
                    position: p(0.4, 0.6),
                    text: "new".into(),
                },
            })
            .expect("re-edit to 'new'");
        let text_of = |s: &Store| match &s.snapshot().canvas.annotations[0].shape {
            AnnotationShape::Label { text, .. } => text.clone(),
            other => panic!("expected label, got {other:?}"),
        };
        assert_eq!(text_of(&store), "new");

        store.undo().expect("undo the re-edit");
        assert_eq!(text_of(&store), "old", "self-inverse undo restores 'old'");

        store.redo().expect("redo the re-edit");
        assert_eq!(text_of(&store), "new", "self-inverse redo restores 'new'");
    }

    #[test]
    fn set_annotation_shape_wire_round_trips() {
        // The snake_case wire contract the Wave-3 frontend dispatches against.
        let mv: Command = serde_json::from_str(
            r#"{"type":"move_annotation","data":{"id":"a1","dx":0.04,"dy":-0.02}}"#,
        )
        .expect("deserialize move_annotation");
        assert_eq!(
            mv,
            Command::MoveAnnotation {
                id: "a1".into(),
                dx: 0.04,
                dy: -0.02,
            }
        );

        let set: Command = serde_json::from_str(
            r#"{"type":"set_annotation_shape","data":{"id":"a1","shape":{"kind":"label","position":{"x":0.4,"y":0.6},"text":"new text"}}}"#,
        )
        .expect("deserialize set_annotation_shape");
        assert_eq!(
            set,
            Command::SetAnnotationShape {
                id: "a1".into(),
                shape: AnnotationShape::Label {
                    position: p(0.4, 0.6),
                    text: "new text".into(),
                },
            }
        );
    }
}
