//! Compact, accurate per-turn state serializer (AGENT-04, SC-3).
//!
//! This module produces the agent's VIEW of the timeline — the thing a future
//! rulebook (Phase 12) injects into the model each turn ("the model is only as
//! good as its view of the timeline", baseline.md). Two forms:
//!
//! 1. [`AgentStateView`] — a typed, `serde`-round-trippable struct that is the
//!    source of truth for SC-3's round-trip proof (round-trip the STRUCT via
//!    JSON in the consumer/test crate, using serde's derive — never hand-roll
//!    a parser for the text form).
//! 2. [`render_compact`] — a one-way, human-legible `format!()` rendering (the
//!    token-efficient string a rulebook injects), derived FROM the typed view.
//!
//! ## Two honest domain gaps (never fabricate a frame count)
//!
//! v1's domain model has NO project-level fps, and `Playback.fps` is never
//! populated for Program/timeline mode (only `source_playback.fps` is, via
//! `LoadPreview`). So frame numbers are ALWAYS derived per-clip, from that
//! clip's OWN media fps — and every best-effort `*_frame` field sits beside a
//! raw `*_us` ground-truth field that is ALWAYS populated. A clip whose media
//! is pure audio/image (`MediaBinItem::fps == 0.0`) gets `None` frame fields,
//! never a made-up number; its microsecond fields stay exact. The PROGRAM
//! playhead's frame is resolved via the clip active AT the playhead (same
//! per-clip-fps rule), and is `None` in a gap / empty timeline — but
//! `playhead_us` is ALWAYS the raw `Playback.position_us`.
//!
//! ## No backend "selection"
//!
//! Selection lives only in the frontend today (unlike `Playback`, which the
//! backend owns as session state). [`view`] therefore takes `selection` as an
//! explicit read-only parameter — NOT new `Store`/`Project` state — and passes
//! it through verbatim (opaque ids, no validation against real clips).

use serde::{Deserialize, Serialize};

use crate::model::{frame_step_us, ClipCrop, ClipTransform, Project, TrackKind};

/// The agent's whole per-turn view of the timeline + current selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStateView {
    pub tracks: Vec<TrackView>,
    /// Opaque clip ids the caller has selected — passed through verbatim (see
    /// module docs: no backend selection concept; not validated here).
    pub selection: Vec<String>,
    /// PROGRAM playhead, ALWAYS populated — ground truth, straight from
    /// `Project.playback.position_us`.
    pub playhead_us: i64,
    /// Best-effort frame conversion via the clip active at the playhead's OWN
    /// media fps. `None` when the playhead is in a gap, the timeline is empty,
    /// or that clip's media has no positive fps.
    pub playhead_frame: Option<i64>,
    /// Every current `Project.canvas` annotation with its structured
    /// coordinates, in canvas order (Phase 13 — the STRUCTURED half of SC-4).
    /// `#[serde(default)]` keeps pre-Phase-13 serialized views loadable.
    #[serde(default)]
    pub canvas: Vec<AnnotationView>,
    /// The project's ONE authoritative output frame rate (Phase 18, D-07),
    /// straight from `Project.fps`. NOTE: the per-clip `ClipView.fps` frame
    /// conversions above still use each clip's OWN media fps — reconciling
    /// those media-fps conversions to the project fps is a DEFERRED
    /// follow-up (18-RESEARCH §Q4): today's numbers stay per-media-honest,
    /// and the agent additionally sees the project timebase here.
    /// `#[serde(default = ...)]` keeps pre-Phase-18 serialized views loadable.
    #[serde(default = "default_view_project_fps")]
    pub project_fps: f64,
    /// Project output resolution `(width, height)` from `Project.width/height`.
    #[serde(default = "default_view_project_resolution")]
    pub project_resolution: (u32, u32),
}

/// Serde defaults for pre-Phase-18 serialized views — mirror the model's
/// own `Project` defaults (1920x1080@30).
fn default_view_project_fps() -> f64 {
    30.0
}
fn default_view_project_resolution() -> (u32, u32) {
    (1920, 1080)
}

/// One canvas annotation, flattened for the agent: `points` carries every
/// normalized coordinate pair (Stroke/Lasso = the whole path, Arrow =
/// `[start, end]`, Label = `[position]`); `text` is `Some` only for labels.
///
/// `kind` is a `String` (one of `"stroke"|"lasso"|"arrow"|"label"`, from
/// [`crate::canvas::AnnotationShape::kind_label`]) rather than `&'static str`
/// so the view stays `DeserializeOwned` — the SC-3/SC-4 JSON round-trip
/// (`serde_json::from_value`) cannot deserialize into a `'static` borrow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotationView {
    pub id: String,
    pub kind: String,
    pub points: Vec<(f64, f64)>,
    pub text: Option<String>,
    pub linked_range_us: Option<(i64, i64)>,
    /// Best-effort FRAME conversion of `linked_range_us`, resolved via the
    /// clip active at the range's START (both ends converted with THAT clip's
    /// own media fps — Phase 14, CANV-01 full). `None` when no clip is active
    /// there or its media has non-positive fps — always paired with
    /// `range_clip_id` (both `Some` or both `None`, never mixed).
    /// `#[serde(default)]` keeps pre-Phase-14 serialized views loadable.
    #[serde(default)]
    pub range_frame: Option<(i64, i64)>,
    /// Raw microsecond ground truth — ALWAYS `Some` verbatim whenever
    /// `linked_range_us` is `Some` (never fabricated, never dropped).
    #[serde(default)]
    pub range_us: Option<(i64, i64)>,
    /// The id of the clip active at the range's start — the clip
    /// `range_frame`'s numbers are expressed against. Paired with
    /// `range_frame` (see above).
    #[serde(default)]
    pub range_clip_id: Option<String>,
    /// Phase 14.2: which surface the mark lives on — `"frame_linked"` or
    /// `"whiteboard"` (a `String`, not the enum, to keep the view
    /// `DeserializeOwned`; mirrors the `kind: String` decision). `#[serde(default)]`
    /// keeps pre-14.2 serialized views loadable (empty string) — `annotation_view`
    /// always populates it.
    #[serde(default)]
    pub space: String,
}

/// One track's clips, in track order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackView {
    pub index: usize,
    /// The track's UI gutter label (label-based track addressing): `"v1"` /
    /// `"a2"`, computed by [`crate::tools::track_label`] — video numbered
    /// BOTTOM-UP (v1 = the bottom video track), audio TOP-DOWN (a1 = the
    /// first audio track). This is the ONE way the agent addresses tracks in
    /// tool calls. `#[serde(default)]` keeps pre-label serialized views
    /// loadable.
    #[serde(default)]
    pub label: String,
    pub kind: TrackKind,
    pub clips: Vec<ClipView>,
}

/// One clip: raw microsecond ground truth ALWAYS populated; frame conversions
/// present only when this clip's media has a positive fps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipView {
    pub id: String,
    pub media_id: String,
    /// This clip's OWN media fps (0.0 for pure-audio/image media — no
    /// fabricated value).
    pub fps: f64,
    // Ground truth (ALWAYS populated, exact):
    pub start_us: i64,
    /// The clip's TIMELINE occupancy — its RETIMED length when `speed` is set
    /// or `animated` contains `"speed"`. **Under retime this is NOT
    /// `source_out_us - source_in_us`** (quick task 260730-x2t): a 4 s source
    /// range at 2x occupies 2 s of timeline. Deriving a duration from the
    /// source in/out is the arithmetic that silently breaks for the agent —
    /// read THIS field (or `duration_frames`).
    pub duration_us: i64,
    pub source_in_us: i64,
    pub source_out_us: i64,
    // Best-effort frame conversion (None when fps <= 0.0):
    pub start_frame: Option<i64>,
    /// Timeline length in frames. Under retime
    /// `duration_frames != source_out_frame - source_in_frame` — see
    /// [`ClipView::duration_us`].
    pub duration_frames: Option<i64>,
    pub source_in_frame: Option<i64>,
    pub source_out_frame: Option<i64>,
    pub volume: f32,
    /// Constant playback speed multiplier when the clip carries one (1.0 is
    /// never emitted — it normalizes to "no retime"). `None` for an un-retimed
    /// clip AND for a clip carrying a speed RAMP: a ramp has no single speed,
    /// and it is reported through `animated` containing `"speed"` instead.
    /// Omitted entirely from JSON when absent (palmier's omit-when-default
    /// convention), so pre-retime views stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    pub audio_detached: bool,
    /// Index of the track this clip sits on (Phase 18 — the 18-CONTEXT
    /// "ClipView track-index gap" opportunistic fix: `clip_view`'s callers
    /// already have the index in hand). `#[serde(default)]` keeps
    /// pre-Phase-18 serialized views loadable. Retained as raw backend ground
    /// truth; the AGENT addresses tracks by `track_label` below.
    #[serde(default)]
    pub track_index: usize,
    /// The UI gutter label of the track this clip sits on (`"v1"` / `"a2"` —
    /// label-based track addressing, same convention as [`TrackView::label`]).
    /// This is the value the agent passes to track-addressed tools.
    /// `#[serde(default)]` keeps pre-label serialized views loadable.
    #[serde(default)]
    pub track_label: String,
    /// Per-clip visual state (Phase 18, TOOL-03), serialized verbatim from
    /// the `Clip` — same typed vocabulary as the model, so the agent's
    /// read/delta views can never drift from backend state.
    #[serde(default)]
    pub transform: ClipTransform,
    #[serde(default = "default_view_opacity")]
    pub opacity: f32,
    #[serde(default)]
    pub crop: ClipCrop,
    /// Names of the properties this clip currently ANIMATES via a non-empty
    /// keyframe track (Phase 19-02, COMP-04), e.g. `["position","opacity"]`.
    /// Full keyframe arrays are deliberately NOT exposed here — this is the
    /// write path's truthful "what is animated" signal; the agent re-reads via
    /// get_timeline if it needs the exact keys. Omitted entirely from JSON for
    /// an un-animated clip (`skip_serializing_if`), so pre-19 / static views
    /// stay compact and byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub animated: Vec<String>,
    /// True when this clip is a TEXT clip (`clip.text.is_some()`) — Phase 20,
    /// TEXT-01/D-07. `#[serde(default)]` keeps pre-20 views loadable (false);
    /// a media clip serializes `is_text: false`.
    #[serde(default)]
    pub is_text: bool,
    /// A COMPACT text signal: the first ~40 chars of the content, ellipsized
    /// when longer (Phase 20). The full [`crate::model::TextStyle`] is
    /// deliberately NOT exposed here — the agent re-reads via get_timeline if it
    /// needs exact styling; this keeps the delta serializer compact. `None` for
    /// a non-text clip; omitted from JSON entirely (`skip_serializing_if`) so
    /// pre-20 / media views stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_preview: Option<String>,
}

/// One media bin item, mirroring `MediaBinItem`'s own fields for the
/// agent's read-only view (Phase 26, TOOL-04) -- same "same vocabulary as
/// the model" convention `ClipView` already establishes: a change to
/// `MediaBinItem` deliberately does NOT auto-propagate here (this is a
/// SEPARATE, intentionally decoupled read-model), but every real field
/// mirrors the domain type 1:1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaBinItemView {
    pub id: String,
    pub path: String,
    pub media_kind: crate::model::MediaKind,
    pub duration_us: i64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub is_vfr: bool,
    pub rotation_degrees: u32,
    pub has_audio: bool,
    pub folder: String,
    pub display_name: Option<String>,
}

/// The agent's whole read-only view of the media library (Phase 26,
/// TOOL-04): every `MediaBinItem` plus the known virtual folder paths
/// (Phase 25, LIB-01) -- the exact two `Project` fields `get_media` exposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaLibraryView {
    pub items: Vec<MediaBinItemView>,
    pub folders: Vec<String>,
}

fn media_bin_item_view(item: &crate::model::MediaBinItem) -> MediaBinItemView {
    MediaBinItemView {
        id: item.id.clone(),
        path: item.path.clone(),
        media_kind: item.media_kind,
        duration_us: item.duration_us,
        width: item.width,
        height: item.height,
        fps: item.fps,
        is_vfr: item.is_vfr,
        rotation_degrees: item.rotation_degrees,
        has_audio: item.has_audio,
        folder: item.folder.clone(),
        display_name: item.display_name.clone(),
    }
}

/// Build the agent's read-only media-library view from a real `Project`
/// (Phase 26, TOOL-04). Pure and infallible, mirrors [`view`]'s contract
/// exactly: never mutates, never guesses, always freshly derived.
pub fn media_view(project: &Project) -> MediaLibraryView {
    MediaLibraryView {
        items: project.media_bin.iter().map(media_bin_item_view).collect(),
        folders: project.media_folders.clone(),
    }
}

/// First `MAX` chars of `content`, with a trailing `…` when truncated. Operates
/// on CHARS (never a byte slice) so a multibyte UTF-8 boundary is never split.
///
/// Control characters (raw newlines, tabs, ANSI escapes, …) are stripped
/// BEFORE truncation (L-02): this compact signal is read every turn and may be
/// rendered into a terminal / log line / chat transcript by a downstream
/// consumer with no escaping of its own, so a `\r` or embedded ANSI sequence in
/// agent/user-supplied text must not leak through and corrupt that output. A
/// plain space is kept (it is printable and semantically meaningful).
fn text_preview(content: &str) -> String {
    const MAX: usize = 40;
    let mut chars = content.chars().filter(|c| !c.is_control() || *c == ' ');
    let head: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Serde default for [`ClipView::opacity`] — pre-Phase-18 views are opaque.
fn default_view_opacity() -> f32 {
    1.0
}

/// Convert a microsecond value to a best-effort frame number using `fps`.
/// `None` for non-positive fps (audio/image media) — never fabricates.
fn to_frame(value_us: i64, fps: f64) -> Option<i64> {
    if fps <= 0.0 {
        return None;
    }
    let step = frame_step_us(fps);
    if step <= 0 {
        return None;
    }
    Some((value_us as f64 / step as f64).round() as i64)
}

/// The fps of the media a clip references, or 0.0 when the media is missing
/// (defensive: referential integrity is guaranteed by `Command::apply`, but a
/// pure read-side view must never panic) or is pure audio/image.
fn media_fps(project: &Project, media_id: &str) -> f64 {
    project
        .media_bin
        .iter()
        .find(|m| m.id == media_id)
        .map(|m| m.fps)
        .unwrap_or(0.0)
}

fn clip_view(project: &Project, clip: &crate::model::Clip, track_index: usize) -> ClipView {
    let fps = media_fps(project, &clip.media_id);
    let start_us = clip.start_us;
    let duration_us = clip.timeline_len_us();
    let source_in_us = clip.in_us;
    let source_out_us = clip.out_us;
    let track_label =
        crate::tools::track_label(&project.timeline, track_index).unwrap_or_default();
    ClipView {
        id: clip.id.clone(),
        media_id: clip.media_id.clone(),
        fps,
        start_us,
        duration_us,
        source_in_us,
        source_out_us,
        start_frame: to_frame(start_us, fps),
        duration_frames: to_frame(duration_us, fps),
        source_in_frame: to_frame(source_in_us, fps),
        source_out_frame: to_frame(source_out_us, fps),
        volume: clip.volume,
        speed: match clip.retime.as_ref().map(|r| &r.curve) {
            Some(crate::model::RetimeCurve::Constant(s)) => Some(*s),
            // A RAMP has no single speed — reported via `animated` instead.
            _ => None,
        },
        audio_detached: clip.audio_detached,
        track_index,
        track_label,
        transform: clip.transform,
        opacity: clip.opacity,
        crop: clip.crop,
        animated: {
            let mut names = clip.keyframes.animated_names();
            // `speed` is animated when the clip carries a RAMP (260730-x2t).
            // It is NOT a `KeyframeTracks` field (RT-02), so it is appended
            // here rather than inside `animated_names`.
            if matches!(
                clip.retime.as_ref().map(|r| &r.curve),
                Some(crate::model::RetimeCurve::Ramp(_))
            ) {
                names.push("speed".to_string());
            }
            names
        },
        is_text: clip.text.is_some(),
        text_preview: clip.text.as_ref().map(|t| text_preview(&t.content)),
    }
}

/// Resolve a linked microsecond range against the clip active at the range's
/// START: `Some((clip_id, (frame_start, frame_end)))` with BOTH ends converted
/// via THAT clip's own media fps (even if the end lands on another clip or a
/// gap — the plan's "frame units of the clip active at range start" rule), or
/// `None` when no clip is active at the start or its media has non-positive
/// fps (never fabricate — the module's honesty policy).
fn resolve_range(project: &Project, range: (i64, i64)) -> Option<(String, (i64, i64))> {
    let (start_us, end_us) = range;
    let hit = project.timeline.top_video_active_at(start_us)?;
    let fps = media_fps(project, &hit.media_id);
    Some((hit.clip_id, (to_frame(start_us, fps)?, to_frame(end_us, fps)?)))
}

/// Flatten one canvas [`crate::canvas::Annotation`] into the agent's view
/// shape: Stroke/Lasso -> every path point, Arrow -> `[start, end]`,
/// Label -> `[position]` + `Some(text)`. `linked_range_us` passes through
/// verbatim; `range_frame`/`range_clip_id` are resolved ONCE here (the one
/// place with `&Project` access) so `render_compact` stays a pure,
/// Project-free formatter.
fn annotation_view(project: &Project, a: &crate::canvas::Annotation) -> AnnotationView {
    use crate::canvas::{AnnotationShape, AnnotationSpace};
    let (points, text) = match &a.shape {
        AnnotationShape::Stroke { points } | AnnotationShape::Lasso { points } => {
            (points.iter().map(|p| (p.x, p.y)).collect(), None)
        }
        AnnotationShape::Arrow { start, end } => {
            (vec![(start.x, start.y), (end.x, end.y)], None)
        }
        AnnotationShape::Label { position, text } => {
            (vec![(position.x, position.y)], Some(text.clone()))
        }
    };
    // Resolved as a PAIR: both Some (clip found, positive fps) or both None.
    let (range_clip_id, range_frame) = match a
        .linked_range_us
        .and_then(|range| resolve_range(project, range))
    {
        Some((clip_id, frames)) => (Some(clip_id), Some(frames)),
        None => (None, None),
    };
    AnnotationView {
        id: a.id.clone(),
        kind: a.shape.kind_label().to_string(),
        points,
        text,
        linked_range_us: a.linked_range_us,
        range_frame,
        // Verbatim ground truth — ALWAYS Some when linked_range_us is Some.
        range_us: a.linked_range_us,
        range_clip_id,
        space: match a.space {
            AnnotationSpace::FrameLinked => "frame_linked",
            AnnotationSpace::Whiteboard => "whiteboard",
        }
        .to_string(),
    }
}

/// Compact delta of everything that changed between two `Project` snapshots
/// (Phase 16, TOOL-01/D-03) — the shape every mutating tool's result carries
/// from this phase forward, so the agent can patch its own world-model
/// without a follow-up full-state re-fetch. Re-serializes ONLY touched clips,
/// in `ClipView`'s exact vocabulary (Pattern 2's "same vocabulary as the read
/// tool" requirement) — never a parallel/duplicated clip shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineDelta {
    pub changed_clips: Vec<ClipView>,
    pub shift_rules: Vec<ShiftRule>,
    pub removed_ids: Vec<String>,
    pub created_tracks: Vec<usize>,
    /// Steering text for the caller (e.g. Phase 22's "indices shifted --
    /// re-fetch transcript"). First populated in Phase 18.1: a track-count
    /// DECREASE (a `RemoveTrack`) carries the "track indices shifted" re-fetch
    /// warning, because tracks are addressed positionally and a removal
    /// renumbers every later track. Empty otherwise.
    pub notes: Vec<String>,
    /// True when `project.canvas` differs between the two snapshots (16-review
    /// fix: `RemoveAnnotation`/`ClearCanvas` mutate ONLY the canvas, and a
    /// clip-only diff returned a fully-EMPTY delta for them). Deliberately a
    /// coarse boolean — a truthful "something changed on the canvas" signal,
    /// NOT a full per-annotation diff (out of scope until a phase needs it).
    /// `#[serde(default)]` keeps previously-serialized deltas loadable.
    #[serde(default)]
    pub canvas_changed: bool,
    /// True when `project.media_bin` or `project.media_folders` differs between
    /// the two snapshots (Phase 25, LIB-01). `delta()` otherwise only diffs the
    /// timeline (clips/tracks) and canvas -- a media-bin/folder-only change
    /// (e.g. organize_media touching no clip) would otherwise look like an
    /// EMPTY delta. Deliberately a coarse boolean (mirrors `canvas_changed`'s
    /// exact precedent) -- a full per-item media diff is Phase 26's
    /// `get_media`/`MediaBinItemView` concern, not this phase's.
    /// `#[serde(default)]` keeps previously-serialized deltas loadable.
    #[serde(default)]
    pub media_changed: bool,
    /// Phase 18.1 (TL-06): ground-truth POST-mutation track count (mirrors
    /// project_fps/project_resolution's "always populated ground truth"
    /// pattern) -- lets the agent detect a RemoveTrack-caused renumbering
    /// without needing per-index removal identity (Track has no stable id,
    /// unlike Clip -- see 18.1-RESEARCH.md Open Question 2).
    /// `#[serde(default)]` keeps previously-serialized deltas loadable.
    #[serde(default)]
    pub track_count: usize,
}

/// A uniform ripple: `count` clips on `track`, all starting at-or-after
/// `from_us` (the EARLIEST pre-shift start among them), shifted by the same
/// `by_us` microseconds. Only emitted when `count >= 3` (D-03) — smaller
/// groups stay as individual `TimelineDelta::changed_clips` entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShiftRule {
    pub track: usize,
    pub from_us: i64,
    pub by_us: i64,
    pub count: usize,
}

/// Compute the compact delta between two REAL `Project` snapshots by full
/// before/after comparison (deliberately NOT patch-driven — see 16-RESEARCH.md
/// Pattern 2's note that today's `Patch{kind,ids}` carries no field values; a
/// full diff is the simplest construction that PROVABLY satisfies "fully
/// accounts for an independently-computed diff", TOOL-01 Success Criterion 2).
/// Pure and infallible: never panics, never mutates either input.
pub fn delta(before: &Project, after: &Project) -> TimelineDelta {
    use std::collections::HashMap;

    // id -> (track_index, Clip), for both snapshots.
    let index = |p: &Project| -> HashMap<String, (usize, crate::model::Clip)> {
        p.timeline
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(ti, t)| t.clips.iter().map(move |c| (c.id.clone(), (ti, c.clone()))))
            .collect()
    };
    let before_idx = index(before);
    let after_idx = index(after);

    let mut removed_ids: Vec<String> = before_idx
        .keys()
        .filter(|id| !after_idx.contains_key(*id))
        .cloned()
        .collect();
    removed_ids.sort();

    struct ShiftCandidate {
        id: String,
        track: usize,
        by_us: i64,
        before_start_us: i64,
    }
    let mut shift_candidates: Vec<ShiftCandidate> = Vec::new();
    let mut changed_ids: Vec<String> = Vec::new();

    // Walk AFTER's clips in track/timeline order for determinism.
    for (ti, track) in after.timeline.tracks.iter().enumerate() {
        for c in &track.clips {
            match before_idx.get(&c.id) {
                None => changed_ids.push(c.id.clone()), // new: AddClip/split-right/duplicate/restore
                Some((before_ti, before_clip)) => {
                    if before_clip == c && *before_ti == ti {
                        continue; // byte-identical: not in the delta
                    }
                    let pure_shift = *before_ti == ti
                        && before_clip.media_id == c.media_id
                        && before_clip.in_us == c.in_us
                        && before_clip.out_us == c.out_us
                        && before_clip.volume == c.volume
                        && before_clip.audio_detached == c.audio_detached
                        // Phase 18: the new visual fields must be equal too,
                        // or a moved clip whose transform/opacity/crop ALSO
                        // changed would compress into a ShiftRule and its
                        // visual change would silently vanish from the delta.
                        && before_clip.transform == c.transform
                        && before_clip.opacity == c.opacity
                        && before_clip.crop == c.crop
                        // Phase 19: a keyframe-only change (same static fields,
                        // same position) must NOT compress into a ShiftRule, or
                        // the animation change would silently vanish from the
                        // delta. Unequal tracks => surface as a changed_clip.
                        && before_clip.keyframes == c.keyframes
                        // Phase 20: a text-payload change (same static fields,
                        // same position) must NOT compress into a ShiftRule, or
                        // a moved-AND-retext'd clip's text change would silently
                        // vanish from the delta (mirrors the visual/keyframe
                        // equality guards above).
                        && before_clip.text == c.text
                        && before_clip.start_us != c.start_us;
                    if pure_shift {
                        shift_candidates.push(ShiftCandidate {
                            id: c.id.clone(),
                            track: ti,
                            by_us: c.start_us - before_clip.start_us,
                            before_start_us: before_clip.start_us,
                        });
                    } else {
                        changed_ids.push(c.id.clone());
                    }
                }
            }
        }
    }

    // Group shift candidates by (track, by_us); compress groups of >= 3.
    let mut groups: HashMap<(usize, i64), Vec<&ShiftCandidate>> = HashMap::new();
    for sc in &shift_candidates {
        groups.entry((sc.track, sc.by_us)).or_default().push(sc);
    }
    let mut shift_rules: Vec<ShiftRule> = Vec::new();
    for ((track, by_us), members) in &groups {
        // Contiguity guard (16-review fix): a ShiftRule implies EVERY clip on
        // `track` starting at-or-after `from_us` rippled by `by_us`. An
        // UNCHANGED clip (byte-identical, skipped by the `continue` above)
        // whose original start falls strictly inside the group's pre-shift
        // [min, max] span disproves that uniformity — compressing anyway
        // would silently omit it (the TightenPacing C2 case). Such a group
        // falls back to individual changed_clips entries.
        let spans_unchanged_clip = || {
            let min = members.iter().map(|m| m.before_start_us).min().unwrap();
            let max = members.iter().map(|m| m.before_start_us).max().unwrap();
            before.timeline.tracks.get(*track).is_some_and(|t| {
                t.clips.iter().any(|bc| {
                    bc.start_us > min
                        && bc.start_us < max
                        && after_idx
                            .get(&bc.id)
                            .is_some_and(|(ati, ac)| ati == track && ac == bc)
                })
            })
        };
        if members.len() >= 3 && !spans_unchanged_clip() {
            let from_us = members.iter().map(|m| m.before_start_us).min().unwrap();
            shift_rules.push(ShiftRule {
                track: *track,
                from_us,
                by_us: *by_us,
                count: members.len(),
            });
        } else {
            for m in members {
                changed_ids.push(m.id.clone());
            }
        }
    }
    shift_rules.sort_by_key(|r| (r.track, r.from_us));

    changed_ids.sort();
    changed_ids.dedup();
    let changed_clips: Vec<ClipView> = changed_ids
        .iter()
        .filter_map(|id| after_idx.get(id).map(|(ti, c)| clip_view(after, c, *ti)))
        .collect();

    let created_tracks: Vec<usize> = if after.timeline.tracks.len() > before.timeline.tracks.len() {
        (before.timeline.tracks.len()..after.timeline.tracks.len()).collect()
    } else {
        Vec::new()
    };

    // Label-based track addressing: tracks are addressed by their UI gutter
    // LABEL (v1/a1...), and labels RE-NUMBER when tracks are added/removed --
    // steer the caller to re-fetch before trusting any pre-mutation label
    // (18.1-RESEARCH.md Pitfall 1, re-worded for labels).
    let mut notes: Vec<String> = Vec::new();
    if after.timeline.tracks.len() < before.timeline.tracks.len() {
        notes.push(
            "track labels re-numbered after a track removal -- re-fetch get_timeline \
             before addressing a track by label"
                .to_string(),
        );
    }
    // A track-count INCREASE reports the NEW track's label so the caller knows
    // what to address next (add_track's tool_result carries this): a new VIDEO
    // track inserts on top and takes the HIGHEST v-number (existing video
    // labels are unchanged); a new AUDIO track appends at the bottom as the
    // highest a-number.
    let kind_count = |p: &Project, kind: TrackKind| {
        p.timeline.tracks.iter().filter(|t| t.kind == kind).count()
    };
    let (video_before, video_after) =
        (kind_count(before, TrackKind::Video), kind_count(after, TrackKind::Video));
    for n in (video_before + 1)..=video_after {
        notes.push(format!(
            "added video track on top: its label is v{n} (existing video track labels \
             are unchanged; v1 stays the bottom video track)"
        ));
    }
    let (audio_before, audio_after) =
        (kind_count(before, TrackKind::Audio), kind_count(after, TrackKind::Audio));
    for n in (audio_before + 1)..=audio_after {
        notes.push(format!("added audio track at the bottom: its label is a{n}"));
    }

    TimelineDelta {
        changed_clips,
        shift_rules,
        removed_ids,
        created_tracks,
        notes,
        // Coarse truthful signal (see field docs): canvas-only commands
        // (RemoveAnnotation/ClearCanvas/...) must not yield an empty delta.
        canvas_changed: before.canvas != after.canvas,
        // Coarse truthful signal (see field docs): media-bin/folder-only
        // changes (organize_media touching no clip/canvas) must not yield an
        // empty delta (Phase 25, LIB-01).
        media_changed: before.media_bin != after.media_bin
            || before.media_folders != after.media_folders,
        track_count: after.timeline.tracks.len(),
    }
}

/// Build the agent's state view from a real `Project` plus the caller's opaque
/// selection. Read-only and infallible: it never mutates the project, never
/// dispatches a `Command`, and degrades to `None`/`0.0` rather than guessing.
pub fn view(project: &Project, selection: &[String]) -> AgentStateView {
    let tracks = project
        .timeline
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| TrackView {
            index,
            label: crate::tools::track_label(&project.timeline, index).unwrap_or_default(),
            kind: track.kind,
            clips: track
                .clips
                .iter()
                .map(|c| clip_view(project, c, index))
                .collect(),
        })
        .collect();

    // PROGRAM playhead — ground truth, always populated.
    let playhead_us = project.playback.position_us;
    // Frame is best-effort via the clip active AT the playhead's own fps.
    let playhead_frame = project
        .timeline
        .top_video_active_at(playhead_us)
        .and_then(|hit| to_frame(playhead_us, media_fps(project, &hit.media_id)));

    AgentStateView {
        tracks,
        selection: selection.to_vec(),
        playhead_us,
        playhead_frame,
        canvas: project
            .canvas
            .annotations
            .iter()
            .map(|a| annotation_view(project, a))
            .collect(),
        project_fps: project.fps,
        project_resolution: (project.width, project.height),
    }
}

/// Human-legible label for a track kind (lowercase, matches the wire form).
fn kind_label(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
    }
}

/// Axis-aligned bounding box of a point list: `(min_x, min_y, max_x, max_y)`.
/// Callers only pass non-empty lists (Stroke/Lasso emptiness is rejected by
/// `Command::apply` before an annotation can exist); degrades to zeros if
/// somehow empty rather than panicking (pure read-side view).
fn bbox(points: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    let mut it = points.iter();
    let Some(&(x0, y0)) = it.next() else {
        return (0.0, 0.0, 0.0, 0.0);
    };
    it.fold((x0, y0, x0, y0), |(minx, miny, maxx, maxy), &(x, y)| {
        (minx.min(x), miny.min(y), maxx.max(x), maxy.max(y))
    })
}

/// One-way, token-efficient rendering of an [`AgentStateView`] (NOT
/// round-tripped — round-trip the TYPED struct via JSON instead; see the
/// module docs / research "Don't Hand-Roll").
///
/// One header line per track, one indented line per clip (frame numbers only
/// when they were actually computed — a zero-fps clip shows its microsecond
/// range explicitly, NEVER a fabricated frame), a selection line, and a final
/// playhead line.
pub fn render_compact(view: &AgentStateView) -> String {
    let mut out = String::new();
    for track in &view.tracks {
        // Tracks are named by their UI gutter LABEL (label-based track
        // addressing): `v1` is the BOTTOM video track, `a1` the first audio
        // track — the same labels every track-addressed tool takes.
        out.push_str(&format!(
            "Track {} ({}):\n",
            track.label,
            kind_label(track.kind)
        ));
        for c in &track.clips {
            let detached = if c.audio_detached { " detached" } else { "" };
            match (
                c.start_frame,
                c.duration_frames,
                c.source_in_frame,
                c.source_out_frame,
            ) {
                (Some(start), Some(dur), Some(sin), Some(sout)) => {
                    out.push_str(&format!(
                        "  {} [{}, {}fps] frames {}-{} (source {}-{}) vol={:.2}{}\n",
                        c.id,
                        c.media_id,
                        c.fps,
                        start,
                        start + dur,
                        sin,
                        sout,
                        c.volume,
                        detached,
                    ));
                }
                _ => {
                    out.push_str(&format!(
                        "  {} [{}, no fps] us {}-{} (source {}-{}) vol={:.2}{}\n",
                        c.id,
                        c.media_id,
                        c.start_us,
                        c.start_us + c.duration_us,
                        c.source_in_us,
                        c.source_out_us,
                        c.volume,
                        detached,
                    ));
                }
            }
        }
    }
    // Canvas block — SKIPPED entirely at zero annotations (token economy,
    // mirrors this phase's "skip the image when zero annotations" call).
    if !view.canvas.is_empty() {
        out.push_str(&format!("Canvas annotations ({}):\n", view.canvas.len()));
        for a in &view.canvas {
            // Trailing clause, read straight off the view (resolved upstream
            // in annotation_view — no Project lookup at render time). A
            // whiteboard mark (Phase 14.2) is project-global with NO linked
            // range, so it prints a distinct `(whiteboard idea)` clause rather
            // than any frame-range/us clause; a frame-linked mark prints its
            // clip-resolved frames when available, else the raw us range
            // (never a fabricated frame count), else nothing.
            let range = if a.space == "whiteboard" {
                " (whiteboard idea)".to_string()
            } else if let (Some((fs, fe)), Some(clip_id)) =
                (a.range_frame, a.range_clip_id.as_deref())
            {
                format!(" frames {fs}-{fe} (clip {clip_id})")
            } else if let Some((s, e)) = a.range_us {
                format!(" us {s}-{e}")
            } else {
                String::new()
            };
            match a.kind.as_str() {
                "arrow" => {
                    let (sx, sy) = a.points.first().copied().unwrap_or((0.0, 0.0));
                    let (ex, ey) = a.points.get(1).copied().unwrap_or((0.0, 0.0));
                    out.push_str(&format!(
                        "  {} arrow: ({sx:.2},{sy:.2}) -> ({ex:.2},{ey:.2}){range}\n",
                        a.id
                    ));
                }
                "label" => {
                    let (x, y) = a.points.first().copied().unwrap_or((0.0, 0.0));
                    let text = a.text.as_deref().unwrap_or("");
                    out.push_str(&format!(
                        "  {} label: \"{text}\" at ({x:.2},{y:.2}){range}\n",
                        a.id
                    ));
                }
                kind => {
                    let (minx, miny, maxx, maxy) = bbox(&a.points);
                    out.push_str(&format!(
                        "  {} {kind}: {} pts bbox ({minx:.2},{miny:.2})-({maxx:.2},{maxy:.2}){range}\n",
                        a.id,
                        a.points.len()
                    ));
                }
            }
        }
    }
    out.push_str(&format!("Selection: [{}]\n", view.selection.join(", ")));
    match view.playhead_frame {
        Some(n) => out.push_str(&format!("Playhead: frame {} (us {})\n", n, view.playhead_us)),
        None => out.push_str(&format!("Playhead: us {} (no active clip)\n", view.playhead_us)),
    }
    out
}

#[cfg(test)]
mod tests {
    //! Phase 14 (CANV-01 full): `linked_range_us` surfaced as clip-resolved
    //! frames on `AnnotationView` + in `render_compact`. Fixture helpers
    //! mirror `crates/core/tests/agent_state.rs`'s `video_media()`/`clip()`
    //! style (the established per-file convention).

    use crate::canvas::{Annotation, AnnotationShape, AnnotationSpace, NormPoint};
    use crate::command::Command;
    use crate::model::{Clip, MediaBinItem, MediaKind};
    use crate::store::Store;

    fn video_media(id: &str, duration_us: i64) -> MediaBinItem {
        MediaBinItem {
            id: id.into(),
            path: format!("test-media/{id}.mp4"),
            media_kind: MediaKind::Video,
            duration_us,
            width: 1280,
            height: 720,
            fps: 30.0,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: false,
            poster_path: None,
            folder: String::new(),
            display_name: None,
            is_image_sequence: false,
            reports_alpha: None,
        }
    }

    fn clip(id: &str, media_id: &str, start_us: i64, in_us: i64, out_us: i64) -> Clip {
        Clip {
            id: id.into(),
            media_id: media_id.into(),
            start_us,
            in_us,
            out_us,
            volume: 1.0,
            audio_detached: false,
            transform: crate::model::ClipTransform::default(),
            opacity: 1.0,
            crop: crate::model::ClipCrop::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        }
    }

    /// One 30fps video clip spanning timeline [0, 4s) + one arrow annotation
    /// carrying `range` — built through the real undoable command path.
    fn store_with_range_annotation(range: Option<(i64, i64)>) -> Store {
        let mut store = Store::new();
        store
            .dispatch(Command::AddMediaBinItem(video_media("m-1", 6_000_000)))
            .expect("add media");
        store
            .dispatch(Command::AddClip {
                track: 0,
                clip: clip("clip-1", "m-1", 0, 0, 4_000_000),
            })
            .expect("add clip");
        store
            .dispatch(Command::AddAnnotation(Annotation {
                id: "a1".into(),
                shape: AnnotationShape::Arrow {
                    start: NormPoint { x: 0.10, y: 0.20 },
                    end: NormPoint { x: 0.60, y: 0.70 },
                },
                linked_range_us: range,
                space: AnnotationSpace::FrameLinked,
            }))
            .expect("add annotation");
        store
    }

    #[test]
    fn range_resolves_to_clip_frames_when_range_start_hits_a_clip() {
        // (1s, 2s) at 30fps inside clip-1 -> frames 30 and 60.
        let store = store_with_range_annotation(Some((1_000_000, 2_000_000)));
        let view = super::view(&store.snapshot(), &[]);
        let a = &view.canvas[0];
        assert_eq!(
            a.range_us,
            Some((1_000_000, 2_000_000)),
            "range_us is ALWAYS the verbatim ground truth"
        );
        assert_eq!(
            a.range_frame,
            Some((30, 60)),
            "both ends converted with the fps of the clip active at range START"
        );
        assert_eq!(
            a.range_clip_id,
            Some("clip-1".to_string()),
            "the resolving clip's id travels on the view"
        );
    }

    #[test]
    fn range_is_us_only_when_no_clip_active_at_range_start() {
        // Range start (5s) lands beyond clip-1's end (4s) — a gap: no clip is
        // active there, so frame/clip fields stay None TOGETHER (never
        // fabricate), while the raw us range is still populated.
        let store = store_with_range_annotation(Some((5_000_000, 6_000_000)));
        let view = super::view(&store.snapshot(), &[]);
        let a = &view.canvas[0];
        assert_eq!(a.range_frame, None, "no active clip -> no frame numbers");
        assert_eq!(a.range_clip_id, None, "paired with range_frame");
        assert_eq!(
            a.range_us,
            Some((5_000_000, 6_000_000)),
            "raw us ground truth still populated"
        );
    }

    #[test]
    fn render_compact_prints_frame_range_and_clip_id_for_a_range_annotation() {
        let store = store_with_range_annotation(Some((1_000_000, 2_000_000)));
        let view = super::view(&store.snapshot(), &[]);
        let text = super::render_compact(&view);
        assert!(
            text.contains("frames 30-60"),
            "annotation line must carry the resolved frame range:\n{text}"
        );
        assert!(
            text.contains("(clip clip-1)"),
            "annotation line must name the resolving clip:\n{text}"
        );
    }

    #[test]
    fn render_compact_prints_us_range_when_no_active_clip() {
        let store = store_with_range_annotation(Some((5_000_000, 6_000_000)));
        let view = super::view(&store.snapshot(), &[]);
        let text = super::render_compact(&view);
        let line = text
            .lines()
            .find(|l| l.contains("a1 arrow"))
            .expect("annotation line present");
        assert!(
            line.contains("us 5000000-6000000"),
            "gap case renders the raw us range:\n{text}"
        );
        assert!(
            !line.contains("frames"),
            "never fabricate a frame count for an unresolvable range:\n{text}"
        );
    }

    // --- Phase 14.2: whiteboard-space compact-state clause -------------

    #[test]
    fn render_compact_prints_whiteboard_idea_clause_for_a_whiteboard_mark() {
        let mut store = Store::new();
        store
            .dispatch(Command::AddAnnotation(Annotation {
                id: "w1".into(),
                shape: AnnotationShape::Stroke {
                    points: vec![
                        NormPoint { x: 0.10, y: 0.20 },
                        NormPoint { x: 0.30, y: 0.40 },
                        NormPoint { x: 0.50, y: 0.60 },
                    ],
                },
                // A whiteboard mark is project-global — no linked range.
                linked_range_us: None,
                space: AnnotationSpace::Whiteboard,
            }))
            .expect("add whiteboard mark");
        let view = super::view(&store.snapshot(), &[]);
        assert_eq!(view.canvas[0].space, "whiteboard", "space travels on the view");
        let text = super::render_compact(&view);
        let line = text
            .lines()
            .find(|l| l.contains("w1 "))
            .expect("whiteboard annotation line present");
        assert!(
            line.contains("(whiteboard idea)"),
            "a whiteboard mark must print the distinct idea clause:\n{text}"
        );
        assert!(
            !line.contains("frames "),
            "a whiteboard mark must NOT print a frame-range clause:\n{text}"
        );
        assert!(
            !line.contains(" us "),
            "a whiteboard mark must NOT print a us-range clause:\n{text}"
        );
    }

    #[test]
    fn render_compact_frame_linked_mark_still_prints_frame_range() {
        // Regression: a frame-linked ranged mark keeps its existing clause,
        // unaffected by the whiteboard branch.
        let store = store_with_range_annotation(Some((1_000_000, 2_000_000)));
        let view = super::view(&store.snapshot(), &[]);
        assert_eq!(view.canvas[0].space, "frame_linked");
        let text = super::render_compact(&view);
        assert!(
            text.contains("frames 30-60"),
            "frame-linked clause unchanged:\n{text}"
        );
        assert!(
            !text.contains("whiteboard idea"),
            "frame-linked mark must not carry the whiteboard clause:\n{text}"
        );
    }
}
