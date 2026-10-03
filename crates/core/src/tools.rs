//! Intent-shaped agent tool schema (Phase 11, AGENT-02 / SC-5).
//!
//! This module is the CONTRACT layer between an agent (a live model in Phase
//! 12, a deterministic fixture in this phase) and the existing undoable
//! [`Command`] engine. Each [`Tool`] variant's [`Tool::resolve`] absorbs the
//! domain arithmetic a caller should never have to re-derive —
//! frame<->microsecond conversion (via each referenced clip's OWN media fps,
//! falling back to the PROJECT fps for audio-only media, which carries no fps
//! of its own — live-UAT AUDIO-ONLY-PLACEMENT-BLOCKED fix), dB<->linear-gain
//! conversion, and the
//! left-edge trim delta math — and returns ONLY [`Command`] values that
//! already exist in [`crate::command`].
//!
//! **SC-5 (no new mutation primitive):** every `resolve()` match arm
//! constructs and returns existing `Command` variants exclusively. Adding a
//! genuinely new mutation would require editing the closed `Command` enum in
//! `command.rs` — a reviewable diff no tool wrapper can silently produce. The
//! `sc5_command_enum_variant_count_is_unchanged` test + the plan's grep gate
//! are the canaries that prove the `Command` enum gained zero variants this
//! phase.
//!
//! **Zero new runtime deps:** only `serde` (already an allowed core dep). The
//! JSON stringification / schema derivation lives in the consumer crate
//! (`crates/agent-mcp`), keeping `crates/core`'s `offline_guard.rs` pin
//! (`serde` + `thiserror`) untouched.

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::model::{
    frame_step_us, Clip, ClipCrop, ClipTransform, CropValue, Interpolation, Keyframe,
    KeyframeTrackData, Project, TextAlign, TextPayload, TextStyle, TextStylePatch, Timeline,
    TrackKind,
};
use crate::CoreError;

/// Upper bound on how many entries one `add_texts` / `update_text` call may
/// carry (T-20-04 DoS guard). A batch above this resolves `Err` before any
/// scratch work — the all-or-none contract's cheap first gate.
pub const MAX_TEXT_BATCH: usize = 500;

/// Upper bound on one organize_media call's operation count (Phase 25,
/// LIB-01) -- mirrors MAX_TEXT_BATCH's existing DoS-guard precedent.
pub const MAX_ORGANIZE_MEDIA_BATCH: usize = 500;

/// Default timeline length for a STILL-IMAGE clip: 5 seconds, the standard
/// NLE still default (Premiere/Resolve/FCP all default stills to ~4-5s).
/// Live-UAT GENERATE-IMAGE-UNPLACEABLE fix (backlog 999.3): a still image
/// (`MediaKind::Image`) always probes to `duration_us == 0`, and
/// `Command::AddClip` rightly rejects `out_us <= in_us` — so every placement
/// arm supplies THIS real default length instead of a zero-length clip
/// (the AddClip invariant is never weakened). A placed still stays trimmable
/// to ANY positive length afterward: `Command::TrimClip`'s media-duration cap
/// is gated on `media_duration > 0`, so a frozen frame can be stretched
/// freely. Media with a REAL probed duration (video/audio) is never touched
/// by this default.
pub const DEFAULT_STILL_DURATION_US: i64 = 5_000_000;

/// Auto-fit sentinel transform for a text clip whose caller supplied NO
/// explicit `transform`: `scale = (0.0, 0.0)` signals "Plan 04's renderer
/// auto-fits the text to its natural rendered size, and does not jump when the
/// content changes". A zero-area dest rect is degenerate/invisible, so no
/// legitimate explicit user transform ever collides with this sentinel — and
/// this is now ENFORCED (M-01), not merely conventional: an explicit `scale ==
/// (0.0, 0.0)` is rejected by both the `add_texts` tool and
/// `Command::SetClipTransform`'s text-clip validation (which also gates
/// `update_text`), so (0,0) is reachable ONLY by omitting the transform here.
///
/// **Plan 04 MUST match this exact convention** (documented in 20-03-SUMMARY):
/// `Clip.transform.scale == (0.0, 0.0)` ⇒ auto-fit; anything else ⇒ the caller
/// pinned an explicit transform, so honor it verbatim (no re-fit on edit).
pub const TEXT_AUTOFIT_TRANSFORM: ClipTransform = ClipTransform {
    position: (0.0, 0.0),
    scale: (0.0, 0.0),
    rotation_deg: 0.0,
};

/// Errors a tool can raise BEFORE constructing a `Command` — the first
/// validation layer for agent-originated input (defense-in-depth ahead of
/// `Command::apply`'s own validate-then-apply guarantee).
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("clip not found: {0}")]
    ClipNotFound(String),
    #[error("media bin item not found: {0}")]
    MediaNotFound(String),
    #[error(
        "unknown track label `{label}`: tracks are addressed by the labels shown in \
         get_timeline — video tracks are v1..vN (v1 is the BOTTOM video track, higher \
         numbers sit higher), audio tracks are a1..aN (a1 is the first audio track). \
         Re-fetch get_timeline after adding/removing tracks, since labels re-number"
    )]
    UnknownTrackLabel { label: String },
    #[error("clip {0}'s media has no positive fps; cannot convert frames")]
    NoFps(String),
    #[error("no clip is active at the current playhead to resolve an implicit target")]
    NoActiveClip,
    #[error("invalid frame range: fromFrame must resolve to a position before toFrame")]
    InvalidFrameRange,
    #[error("empty batch: {0}")]
    EmptyBatch(&'static str),
    #[error(
        "duplicate track label `{0}` in remove_tracks call (two labels resolving to \
         the same track count as duplicates)"
    )]
    DuplicateTrackLabel(String),
    #[error("invalid set_keyframes call: {0}")]
    InvalidKeyframes(String),
    #[error("text batch too large: {0} entries exceeds the MAX_TEXT_BATCH cap")]
    TextBatchTooLarge(usize),
    #[error(
        "mixed-track add_texts batch: either EVERY entry sets trackIndex or none do \
         (mixed is rejected to avoid index-shift bugs)"
    )]
    MixedTrackBatch,
    #[error("no video track exists to place text on; add a track first")]
    NoVideoTrack,
    #[error(
        "track {track} already has a clip overlapping frames \
         [{start_frame}, {end_frame}) — placing text there would cover that \
         footage in preview AND export (live-UAT TEXT-EVICTS-CLIP content-loss \
         guard); omit track to auto-place the text on an overlay track \
         above, or call add_track (kind \"video\") and target the new track"
    )]
    TextTrackOccupied {
        /// The offending track's LABEL (label-based track addressing, e.g. "v1").
        track: String,
        start_frame: i64,
        end_frame: i64,
    },
    #[error(
        "explicit text transform scale (0, 0) is reserved as the auto-fit sentinel; \
         a zero-size text clip renders nothing — omit the transform to auto-fit, or \
         use a positive scale"
    )]
    TextAutofitScaleReserved,
    #[error(
        "invalid fill color `{0}`: expected hex `#RRGGBB`/`#RRGGBBAA` or `rgb(r,g,b)`/`rgba(r,g,b,a)`"
    )]
    InvalidFill(String),
    #[error(
        "word range [{from_us}, {to_us}) is outside clip `{clip_id}`'s current source \
         window; re-fetch the transcript after any trim/split/move before cutting"
    )]
    WordRangeOutOfClip {
        clip_id: String,
        from_us: i64,
        to_us: i64,
    },
    #[error(
        "update_text targets caption group `{0}`, but no text clip carries that \
         caption_group_id (nothing to restyle); the group may have been removed"
    )]
    CaptionGroupNotFound(String),
    #[error(
        "update_text target names neither a clipId nor a groupId; supply exactly one"
    )]
    NoTextTarget,
    #[error("unknown layout template `{0}`")]
    UnknownLayoutTemplate(String),
    #[error("layout template `{template}` has no slot named `{slot}`")]
    UnknownLayoutSlot { template: String, slot: String },
    #[error("duplicate slot `{0}` in one apply_layout call")]
    DuplicateLayoutSlot(String),
    #[error("duplicate clip id `{0}` in one apply_layout call")]
    DuplicateLayoutClip(String),
    #[error("invalid media folder path or name: {0}")]
    InvalidFolderPath(String),
    #[error("cannot move/rename folder {0} into its own descendant {1}")]
    FolderMoveCycle(String, String),
    #[error("duplicate organize_media target `{0}` in one batch")]
    DuplicateOrganizeTarget(String),
    #[error("organize_media batch too large: {0} entries exceeds the MAX_ORGANIZE_MEDIA_BATCH cap")]
    OrganizeMediaBatchTooLarge(usize),
    #[error("underlying command rejected: {0}")]
    Command(#[from] CoreError),
}

/// One agent-callable tool. `resolve()` returns 1+ EXISTING `Command` values —
/// NEVER a new mutation primitive (SC-5). Wire shape: `{"tool":"trimClip","args":{...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tool", content = "args", rename_all = "camelCase")]
pub enum Tool {
    PlaceClip(PlaceClipArgs),
    TrimClip(TrimClipArgs),
    SplitClip(SplitClipArgs),
    RemoveClip(RemoveClipArgs),
    RemoveSection(RemoveSectionArgs),
    DuplicateClip(DuplicateClipArgs),
    MoveClip(MoveClipArgs),
    SetClipVolume(SetClipVolumeArgs),
    SetClipMuted(SetClipMutedArgs),
    DetachAudio(DetachAudioArgs),
    ReattachAudio(ReattachAudioArgs),
    TightenPacing(TightenPacingArgs),
    // ---- canvas delete (Phase 14.1 — wrap EXISTING Command variants) --------
    // These add ZERO new mutation primitives (Command::RemoveAnnotation /
    // Command::ClearCanvas already exist from Phase 13), so SC-5 holds. They
    // inherit one-turn-one-undo for free via the generic dispatch_edit path.
    RemoveAnnotation(RemoveAnnotationArgs),
    ClearCanvas(ClearCanvasArgs),
    // ---- batch clip tools (Phase 17-01, TOOL-02) ----------------------------
    // Each resolves to a Vec of EXISTING Commands via resolve_on_scratch
    // (all-or-none: any invalid item aborts the WHOLE batch before any live
    // dispatch). New tools use snake_case wire names (Convention 8 / D-05);
    // the enum-level `rename_all = "camelCase"` still governs the legacy
    // variants, so each batch variant carries an EXPLICIT rename — without it
    // `AddClips` would serialize as `addClips` and parse_edit_tool would fail.
    #[serde(rename = "add_clips")]
    AddClips(AddClipsArgs),
    #[serde(rename = "insert_clips")]
    InsertClips(InsertClipsArgs),
    #[serde(rename = "remove_clips")]
    RemoveClips(RemoveClipsArgs),
    #[serde(rename = "move_clips")]
    MoveClips(MoveClipsArgs),
    #[serde(rename = "split_clips")]
    SplitClips(SplitClipsArgs),
    // ---- ripple delete (Phase 17-02, D-03) ----------------------------------
    // The one genuinely-COMPOSED batch tool: cut one or more track-addressed
    // time ranges AND close the gaps in one atomic call, built from the SAME
    // cut_range (removeSection) + close_gaps (tightenPacing) helpers — zero
    // new engine code, zero new Command variants (Split/Remove/Move only).
    #[serde(rename = "ripple_delete_ranges")]
    RippleDeleteRanges(RippleDeleteRangesArgs),
    // ---- project timebase + clip properties (Phase 18-04) -------------------
    // COMP-01: 1:1 wrap of the validated Command::SetProjectSettings.
    #[serde(rename = "set_project_settings")]
    SetProjectSettings(SetProjectSettingsArgs),
    // TOOL-03 (D-06): BATCH tool — one set of property values applied to every
    // clip in `clip_ids`, composed per-clip from the Plan-03 per-field commands
    // (+ SetClipVolume for volume, ABSORBED per D-03; TrimClip for trim) via
    // resolve_on_scratch (all-or-none).
    #[serde(rename = "set_clip_properties")]
    SetClipProperties(SetClipPropertiesArgs),
    #[serde(rename = "remove_tracks")]
    RemoveTracks(RemoveTracksArgs),
    // ---- add_track (live-UAT TRACK-ADD-AGENT-GAP fix) ------------------------
    // 1:1 wrap of the EXISTING Command::AddTrack (the SetProjectSettings
    // direct-wrap precedent) — the agent could previously only REMOVE tracks
    // (the 2026-07-09 "realistic cut" narrowing), which live UAT surfaced as a
    // real gap. No index arg: placement is the Command's own kind-aware
    // invariant (video inserts on top at index 0, audio appends at the bottom).
    #[serde(rename = "add_track")]
    AddTrack(AddTrackArgs),
    // ---- keyframe animation (Phase 19-02, COMP-04) ---------------------------
    // Single-clip, single-property, FULL-track-replace (D-01/D-10, palmier-
    // verified contract). The resolve maps property→value-arity ONLY; every
    // other rule (cap/sort/dup/values) is the Command's own validation,
    // exercised on the resolve_on_scratch clone so the call is all-or-none.
    #[serde(rename = "set_keyframes")]
    SetKeyframes(SetKeyframesArgs),
    // ---- text overlays (Phase 20-03, TEXT-01) -------------------------------
    // BATCH create + partial-merge style-edit for REAL text-overlay clips.
    // Both compose ONLY the Plan-02 commands (AddText / UpdateText /
    // SetClipTransform) through resolve_on_scratch (all-or-none) — ZERO new
    // mutation primitive (the v3 invariant). add_texts enforces the all-or-none
    // track rule; update_text validates every target is a text clip.
    #[serde(rename = "add_texts")]
    AddTexts(AddTextsArgs),
    #[serde(rename = "update_text")]
    UpdateText(UpdateTextArgs),
    // ---- transcript word-cuts (Phase 22-03, TEXT-03) ------------------------
    // Delete agent-supplied, PRE-RESOLVED SOURCE-media word time spans from ONE
    // clip and close the gaps (ripple), composed from the SAME cut_range +
    // ripple_shift_after_cut primitives RippleDeleteRanges uses — ZERO new
    // ripple/gap-close code, ZERO new Command variant. args are SOURCE
    // microseconds (transcript timestamps are continuous time — no fps
    // ambiguity, so no frames+fps here); resolve() maps each source span to the
    // clip's CURRENT [in_us,out_us) timeline position (never a cached index),
    // so a prior same-turn trim/split/move is reflected and an out-of-window
    // span aborts the WHOLE call (the SC-3 edit-survival property). This tool
    // does NOT transcribe (crates/core is I/O-pinned, offline_guard.rs) — the
    // agent calls get_transcript (Plan 05) first, then passes the ranges here.
    #[serde(rename = "remove_words")]
    RemoveWords(RemoveWordsArgs),
    // ---- captions (Phase 22-04, TEXT-04) ------------------------------------
    // Mint N ORDINARY text-overlay clips (the SAME Phase-20 TextPayload clips,
    // rendered through the SAME WYSIWYG compositor path), each tagged with a
    // shared `caption_group_id` + shared style, timed to the agent-supplied
    // caption entries. This is the AddTexts minting path with ONE extra field
    // set — ZERO new Command variant, ZERO new render path, ZERO new caption
    // structure. The group id rides ON the clip's text payload, so a caption
    // survives trim/split/move like any other clip (T-22-13), and the group can
    // be bulk-restyled by `update_text`'s groupId mode. The agent calls
    // get_transcript first and groups words into caption-sized cards itself
    // (Open Q1: agent-side grouping); this tool only mints the clips.
    #[serde(rename = "add_captions")]
    AddCaptions(AddCaptionsArgs),
    // ---- layouts (Phase 23, COMP-03) -----------------------------------------
    // Named-template, RE-LAYOUT-EXISTING-CLIPS-ONLY (no "place a new clip into
    // a slot" mode -- every assignment's clip_id must already be on the
    // timeline). Composes ONLY SetClipTransform + SetClipCrop (+ the D-06
    // keyframe-track-clear helper) through resolve_on_scratch -- ZERO new
    // mutation primitive, ZERO engine change. Structurally distinct from
    // set_clip_properties: this tool owns the template->slot geometry table +
    // the crop-to-cover formula so the agent never hand-derives per-clip
    // transform/crop numbers for a layout -- never call set_clip_properties in
    // a loop to build one instead (see this tool's authored description).
    #[serde(rename = "apply_layout")]
    ApplyLayout(ApplyLayoutArgs),
    // ---- media library folders (Phase 25, LIB-01) ---------------------------
    // Path-addressed folder create/move/rename/delete + media move/rename/
    // delete, as ONE atomic batch. Reuses AddMediaBinItem/RemoveMediaBinItem
    // for media create/delete (delete_media, and cascade); composes the 5 NEW
    // Plan-25-02 Commands for everything folder-shaped. Fixed phase order
    // (create -> move/rename -> delete) regardless of caller order. Path
    // sanitization + cycle detection run BEFORE any scratch work (SC-1/SC-2).
    #[serde(rename = "organize_media")]
    OrganizeMedia(OrganizeMediaArgs),
}

// ---------------------------------------------------------------------------
// Private helpers — the ONE place each conversion lives (AGENT-02 thesis:
// absorb domain arithmetic once, tested, not re-derived per tool).
// ---------------------------------------------------------------------------

/// Locate a clip by id anywhere on the timeline (read-only borrow).
fn find_clip<'a>(project: &'a Project, id: &str) -> Result<&'a Clip, ToolError> {
    project
        .timeline
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .find(|c| c.id == id)
        .ok_or_else(|| ToolError::ClipNotFound(id.to_string()))
}

/// Locate a clip by id AND the index of the track it sits on (read-only
/// borrow). Used by `remove_words` (Phase 22-03), whose source-time->timeline
/// mapping needs the clip's current window and its track for the cut steps.
fn find_clip_with_track<'a>(
    project: &'a Project,
    id: &str,
) -> Result<(usize, &'a Clip), ToolError> {
    project
        .timeline
        .tracks
        .iter()
        .enumerate()
        .find_map(|(idx, t)| t.clips.iter().find(|c| c.id == id).map(|c| (idx, c)))
        .ok_or_else(|| ToolError::ClipNotFound(id.to_string()))
}

// ---------------------------------------------------------------------------
// Track-label addressing (label-based-track-addressing change): the agent
// addresses tracks by the SAME labels the UI's timeline gutter shows
// (frontend/src/main.ts renderTimeline), never by a raw backend index.
//
// The convention, replicated byte-for-byte from the frontend formula:
// - VIDEO tracks are numbered BOTTOM-UP: the BOTTOM-most video track is `v1`
//   (the user's anchor lane), the next one up `v2`, ... the TOP video track is
//   `v{totalVideo}`. Because `Command::AddTrack{Video}` inserts the new track
//   at index 0 (on top), a fresh video track gets the HIGHEST v-number and
//   every existing video track KEEPS its label.
// - AUDIO tracks are numbered TOP-DOWN: the first audio track (top of the
//   audio group) is `a1`, the next `a2`, ...
// Worked example: tracks = [Video, Video, Audio, Audio] (top->bottom) labels
// as ["v2", "v1", "a1", "a2"].
//
// Labels are produced lowercase (`v1`/`a1`) and resolved case-insensitively
// (`V1` == `v1`).
// ---------------------------------------------------------------------------

/// The UI gutter label of the track at backend index `index` (lowercase, e.g.
/// `"v1"` / `"a2"`), or `None` when `index` is out of range. Pure — no I/O.
pub fn track_label(timeline: &Timeline, index: usize) -> Option<String> {
    let total_video = timeline
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .count();
    let mut video_seen = 0usize;
    let mut audio_seen = 0usize;
    for (i, t) in timeline.tracks.iter().enumerate() {
        match t.kind {
            TrackKind::Video => video_seen += 1,
            TrackKind::Audio => audio_seen += 1,
        }
        if i == index {
            return Some(match t.kind {
                // Top-down running counter inverted against the total video
                // count => bottom-up numbering (the frontend's exact formula).
                TrackKind::Video => format!("v{}", total_video - video_seen + 1),
                TrackKind::Audio => format!("a{audio_seen}"),
            });
        }
    }
    None
}

/// Resolve a track LABEL string (`"v1"`, `"a2"`, case-insensitive, surrounding
/// whitespace tolerated) to its backend track index against `timeline` — the
/// exact inverse of [`track_label`]. A malformed label (wrong prefix, no
/// number, zero, junk) or an out-of-range number (`v9` on a 2-video timeline)
/// is [`ToolError::UnknownTrackLabel`]. Pure — no I/O.
pub fn resolve_track_label(timeline: &Timeline, label: &str) -> Result<usize, ToolError> {
    let unknown = || ToolError::UnknownTrackLabel {
        label: label.to_string(),
    };
    let trimmed = label.trim();
    let mut chars = trimmed.chars();
    let kind = match chars.next() {
        Some(c) if c.eq_ignore_ascii_case(&'v') => TrackKind::Video,
        Some(c) if c.eq_ignore_ascii_case(&'a') => TrackKind::Audio,
        _ => return Err(unknown()),
    };
    let digits = chars.as_str();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(unknown());
    }
    let n: usize = digits.parse().map_err(|_| unknown())?;
    if n == 0 {
        return Err(unknown());
    }
    let total_of_kind = timeline.tracks.iter().filter(|t| t.kind == kind).count();
    if n > total_of_kind {
        return Err(unknown());
    }
    // Video: the n-th from the BOTTOM is the (total - n + 1)-th from the top.
    // Audio: the n-th from the top.
    let ordinal_from_top = match kind {
        TrackKind::Video => total_of_kind - n + 1,
        TrackKind::Audio => n,
    };
    let mut seen = 0usize;
    for (i, t) in timeline.tracks.iter().enumerate() {
        if t.kind == kind {
            seen += 1;
            if seen == ordinal_from_top {
                return Ok(i);
            }
        }
    }
    // Unreachable: `n <= total_of_kind` guarantees the walk finds the track.
    Err(unknown())
}

/// Validate agent-supplied SOURCE-media word spans against a clip's CURRENT
/// `[in_us, out_us)` window and map them to disjoint, ascending TIMELINE ranges
/// (Phase 22-03, TEXT-03). All-or-none: an empty list (`EmptyBatch`), an
/// inverted span (`InvalidFrameRange`), or a span reaching outside the clip's
/// current source window (`WordRangeOutOfClip` — the SC-3 edit-survival reject,
/// Pitfall 5 / T-22-08) returns `Err` before any timeline work. The mapping is
/// the inverse of [`crate::model::Timeline::active_at`]'s
/// `source_us = in_us + (pos - start)`, i.e. `timeline_us = start_us +
/// (source_us - in_us)`, valid because every span is proven in-window first.
/// Overlapping/adjacent ranges are merged (same discipline as
/// `RippleDeleteRanges`) so a later cut's frame math is never applied to a span
/// an earlier overlapping cut already shifted.
fn merge_word_ranges_to_timeline(
    clip_id: &str,
    ranges: &[SourceRange],
    clip: &Clip,
) -> Result<Vec<(i64, i64)>, ToolError> {
    if ranges.is_empty() {
        return Err(ToolError::EmptyBatch(
            "ranges must contain at least one source-time word range",
        ));
    }
    let mut timeline_ranges: Vec<(i64, i64)> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if r.from_us >= r.to_us {
            return Err(ToolError::InvalidFrameRange);
        }
        if r.from_us < clip.in_us || r.to_us > clip.out_us {
            return Err(ToolError::WordRangeOutOfClip {
                clip_id: clip_id.to_string(),
                from_us: r.from_us,
                to_us: r.to_us,
            });
        }
        let t_from = clip.start_us + (r.from_us - clip.in_us);
        let t_to = clip.start_us + (r.to_us - clip.in_us);
        timeline_ranges.push((t_from, t_to));
    }
    timeline_ranges.sort_by_key(|(from, _)| *from);
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(timeline_ranges.len());
    for (from_us, to_us) in timeline_ranges {
        match merged.last_mut() {
            Some((_, cur_to)) if from_us <= *cur_to => {
                *cur_to = (*cur_to).max(to_us);
            }
            _ => merged.push((from_us, to_us)),
        }
    }
    Ok(merged)
}

/// The referenced clip's OWN media fps, falling back to the PROJECT fps when
/// the media carries no fps of its own (audio-only media probes fps 0.0 —
/// live-UAT AUDIO-ONLY-PLACEMENT-BLOCKED fix: the project fps is the canonical
/// timebase, so interpreting the agent's frame numbers at project fps is
/// correct and unambiguous). Still rejects a non-positive result so frame
/// conversion never silently produces 0.
fn clip_fps(project: &Project, clip: &Clip) -> Result<f64, ToolError> {
    project
        .media_bin
        .iter()
        .find(|m| m.id == clip.media_id)
        .map(|m| if m.fps > 0.0 { m.fps } else { project.fps })
        .filter(|fps| *fps > 0.0)
        .ok_or_else(|| ToolError::NoFps(clip.media_id.clone()))
}

/// A media bin item's frame-interpretation rate for PLACEMENT tools
/// (placeClip / add_clips / insert_clips): the media's own fps, or the PROJECT
/// fps when the media carries none (audio-only — the same fallback as
/// [`clip_fps`], applied before a clip exists). Errors only if BOTH are
/// non-positive (defense-in-depth; `Project.fps` is validated > 0 on apply).
fn media_fps(project: &Project, media: &crate::model::MediaBinItem) -> Result<f64, ToolError> {
    let fps = if media.fps > 0.0 { media.fps } else { project.fps };
    if fps > 0.0 {
        Ok(fps)
    } else {
        Err(ToolError::NoFps(media.id.clone()))
    }
}

/// A media bin item's placement LENGTH in microseconds (placeClip /
/// add_clips / insert_clips): the REAL probed container duration, or
/// [`DEFAULT_STILL_DURATION_US`] when the media has none — a still image
/// (`MediaKind::Image`) probes `duration_us == 0` (live-UAT
/// GENERATE-IMAGE-UNPLACEABLE fix, backlog 999.3). Video/audio media always
/// keeps its real probed duration; nothing is invented for media that has
/// one. The companion of [`media_fps`]'s project-fps fallback: together they
/// make an image (fps 0, duration 0) both frame-addressable AND placeable.
fn placement_len_us(media: &crate::model::MediaBinItem) -> i64 {
    if media.duration_us > 0 {
        media.duration_us
    } else {
        DEFAULT_STILL_DURATION_US
    }
}

/// Frame number -> microseconds against a specific fps. Reuses the crate's
/// single `frame_step_us` rounding — no ad-hoc `1_000_000/fps` scattered here.
fn frame_to_us(frame: i64, fps: f64) -> i64 {
    frame * frame_step_us(fps)
}

/// Resolve a sequence of per-item closures against ONE progressively-mutated
/// clone. Each closure sees prior items' effects (minted child ids, shifts) and
/// returns the Command(s) for its item; every Command is applied to the clone
/// so deterministic child ids are captured EXACTLY (no unique_child_id
/// re-derivation). ANY error aborts the WHOLE batch (Err) -> caller dispatches
/// nothing -> timeline byte-unchanged (D-02 / SC-2), zero rollback logic.
///
/// A pure free function (NOT a Store method): it generalizes removeSection's
/// clone-and-apply pattern for every Phase-17+ batch tool, staying inside
/// core's serde+thiserror dependency pin.
fn resolve_on_scratch<F>(project: &Project, steps: Vec<F>) -> Result<Vec<Command>, ToolError>
where
    F: FnOnce(&Project) -> Result<Vec<Command>, ToolError>,
{
    let mut scratch = project.clone();
    let mut out: Vec<Command> = Vec::new();
    for step in steps {
        for cmd in step(&scratch)? {
            cmd.apply(&mut scratch)?;
            out.push(cmd);
        }
    }
    Ok(out)
}

/// Fixed, resolution-independent layout template catalog (Phase 23,
/// COMP-03). Every `(template, slot)` maps to a `(position, scale)`
/// `ClipTransform` sub-shape -- see 23-RESEARCH.md "Layout Math" for the
/// worked derivations at the 1920x1080 project default. Adding a
/// template/slot here is the ONLY change needed to extend the catalog:
/// zero engine change, zero new `Command`.
const LAYOUT_TEMPLATE_NAMES: [&str; 10] = [
    "full",
    "side_by_side",
    "top_bottom",
    "grid_2x2",
    "pip_bottom_right",
    "pip_bottom_left",
    "pip_top_right",
    "pip_top_left",
    "main_sidebar",
    "three_up",
];

/// Look up one template/slot's fixed dest-rect (position, scale). `None`
/// means either an unknown template OR an unknown slot for a KNOWN
/// template -- the caller (`Tool::ApplyLayout::resolve`) distinguishes the
/// two cases by first checking `LAYOUT_TEMPLATE_NAMES`.
fn layout_slot_rect(template: &str, slot: &str) -> Option<((f32, f32), (f32, f32))> {
    match (template, slot) {
        ("full", "main") => Some(((0.0, 0.0), (1.0, 1.0))),
        ("side_by_side", "left") => Some(((0.0, 0.0), (0.5, 1.0))),
        ("side_by_side", "right") => Some(((0.5, 0.0), (0.5, 1.0))),
        ("top_bottom", "top") => Some(((0.0, 0.0), (1.0, 0.5))),
        ("top_bottom", "bottom") => Some(((0.0, 0.5), (1.0, 0.5))),
        ("grid_2x2", "top_left") => Some(((0.0, 0.0), (0.5, 0.5))),
        ("grid_2x2", "top_right") => Some(((0.5, 0.0), (0.5, 0.5))),
        ("grid_2x2", "bottom_left") => Some(((0.0, 0.5), (0.5, 0.5))),
        ("grid_2x2", "bottom_right") => Some(((0.5, 0.5), (0.5, 0.5))),
        ("pip_bottom_right", "background") => Some(((0.0, 0.0), (1.0, 1.0))),
        ("pip_bottom_right", "inset") => Some(((0.68, 0.68), (0.28, 0.28))),
        ("pip_bottom_left", "background") => Some(((0.0, 0.0), (1.0, 1.0))),
        ("pip_bottom_left", "inset") => Some(((0.04, 0.68), (0.28, 0.28))),
        ("pip_top_right", "background") => Some(((0.0, 0.0), (1.0, 1.0))),
        ("pip_top_right", "inset") => Some(((0.68, 0.04), (0.28, 0.28))),
        ("pip_top_left", "background") => Some(((0.0, 0.0), (1.0, 1.0))),
        ("pip_top_left", "inset") => Some(((0.04, 0.04), (0.28, 0.28))),
        ("main_sidebar", "main") => Some(((0.0, 0.0), (0.7, 1.0))),
        ("main_sidebar", "sidebar") => Some(((0.7, 0.0), (0.3, 1.0))),
        ("three_up", "left") => Some(((0.0, 0.0), (1.0 / 3.0, 1.0))),
        ("three_up", "center") => Some(((1.0 / 3.0, 0.0), (1.0 / 3.0, 1.0))),
        ("three_up", "right") => Some(((2.0 / 3.0, 0.0), (1.0 / 3.0, 1.0))),
        _ => None,
    }
}

/// Crop-to-cover (Phase 23, COMP-03, 23-RESEARCH.md Pattern 2): given the
/// clip's RAW (container) source dims + display rotation and the slot's
/// dest-rect aspect ratio (`target_ar`, already computed from the
/// PROJECT's own width/height by the caller), returns the symmetric
/// `ClipCrop` insets that make the contain-fitted source exactly fill the
/// slot with no letterbox bars. Rotation-aware (Pitfall 1): a 90/270
/// display rotation swaps the effective w/h BEFORE computing the source
/// aspect -- `crates/core` cannot depend on `crates/engine`'s own
/// `rotated_dims`, so this is an inline equivalent. Degenerate/non-finite
/// inputs fall back to `ClipCrop::default()` (identity) rather than
/// propagate NaN (T-18-01 discipline) -- `Command::SetClipCrop`'s own
/// clamp+reject guard is the last-line defense regardless.
fn layout_cover_crop(source_w: u32, source_h: u32, rotation_degrees: u32, target_ar: f32) -> ClipCrop {
    let (eff_w, eff_h) = if rotation_degrees % 360 == 90 || rotation_degrees % 360 == 270 {
        (source_h, source_w)
    } else {
        (source_w, source_h)
    };
    if eff_w == 0 || eff_h == 0 || !target_ar.is_finite() || target_ar <= 0.0 {
        return ClipCrop::default();
    }
    let source_ar = eff_w as f32 / eff_h as f32;
    if !source_ar.is_finite() || source_ar <= 0.0 {
        return ClipCrop::default();
    }
    if (source_ar - target_ar).abs() < 1e-6 {
        return ClipCrop::default();
    }
    if source_ar > target_ar {
        // source relatively WIDER than the slot -> crop left/right.
        let frac = 1.0 - target_ar / source_ar;
        ClipCrop { left: frac / 2.0, right: frac / 2.0, top: 0.0, bottom: 0.0 }
    } else {
        // source relatively TALLER than the slot -> crop top/bottom.
        let frac = 1.0 - source_ar / target_ar;
        ClipCrop { top: frac / 2.0, bottom: frac / 2.0, left: 0.0, right: 0.0 }
    }
}

impl Tool {
    /// Resolve this tool against a read-only `Project` into 1+ EXISTING
    /// `Command` values. Never constructs a new mutation primitive (SC-5).
    pub fn resolve(&self, project: &Project) -> Result<Vec<Command>, ToolError> {
        match self {
            // ---- placement ------------------------------------------------
            Tool::PlaceClip(args) => {
                // Label-based track addressing: resolve the v1/a1 label FIRST
                // (UnknownTrackLabel on a bad/out-of-range label).
                let track = resolve_track_label(&project.timeline, &args.track)?;
                let media = project
                    .media_bin
                    .iter()
                    .find(|m| m.id == args.media_id)
                    .ok_or_else(|| ToolError::MediaNotFound(args.media_id.clone()))?;
                // Audio-only media has no fps of its own; media_fps falls back
                // to the PROJECT fps so it stays placeable (a real probed
                // DURATION is used as-is; only a still image — duration 0 —
                // gets the DEFAULT_STILL_DURATION_US placement length via
                // placement_len_us, live-UAT GENERATE-IMAGE-UNPLACEABLE fix).
                let start_us = frame_to_us(args.start_frame, media_fps(project, media)?);
                Ok(vec![Command::AddClip {
                    track,
                    clip: Clip {
                        id: args.clip_id.clone(),
                        media_id: args.media_id.clone(),
                        start_us,
                        in_us: 0,
                        out_us: placement_len_us(media),
                        volume: 1.0,
                        audio_detached: false,
                        // A BRAND-NEW clip from args has no parent to inherit
                        // from: identity visuals are semantically correct
                        // (Phase 18; NOT a blind default-fill — contrast the
                        // SplitClip/DetachAudio struct-update inheritance).
                        transform: crate::model::ClipTransform::default(),
                        opacity: 1.0,
                        crop: crate::model::ClipCrop::default(),
                        // New clip: no animation yet (Phase 19).
                        keyframes: crate::model::KeyframeTracks::default(),
                        // New clip is media, not text (Phase 20).
                        text: None,
                        // Brand-new clip: straight alpha (Phase 28, OVL-01).
                        alpha_mode: crate::model::AlphaMode::default(),
                        // Brand-new clip plays at 1:1 (quick task 260730-x2t).
                        // Written explicitly, NOT via `..Default::default()`:
                        // this literal is deliberately exhaustive so a future
                        // Clip field fails the build here instead of silently
                        // default-filling (the "NOT a blind default-fill" note
                        // above).
                        retime: None,
                    },
                }])
            }

            // ---- trim (absorbs the coupled left-edge delta math) ----------
            Tool::TrimClip(args) => {
                let clip = find_clip(project, &args.clip_id)?;
                let fps = clip_fps(project, clip)?;
                let target_us = frame_to_us(args.to_frame, fps);
                // The requested TIMELINE offset becomes a SOURCE offset through
                // the clip's speed integral (260730-x2t, RT-04). For an
                // un-retimed clip `source_offset_at` is the identity, so this
                // is byte-identical to the pre-retime math and every existing
                // trim test stays green — that is the regression proof.
                let src_off = clip.source_offset_at(target_us - clip.start_us);
                let (new_in_us, new_out_us) = match args.edge {
                    // Left edge: the requested timeline frame becomes the new
                    // start. Command::TrimClip's OWN delta math (the INVERSE
                    // integral of new_in - old_in == the start shift) lands
                    // start_us exactly at target_us.
                    TrimEdge::Start => (clip.in_us + src_off, clip.out_us),
                    // Right edge: in_us must NOT move; only out_us changes so
                    // the clip's timeline end lands at target_us.
                    TrimEdge::End => (clip.in_us, clip.in_us + src_off),
                };
                Ok(vec![Command::TrimClip {
                    id: args.clip_id.clone(),
                    new_in_us,
                    new_out_us,
                    // Forward caller: apply derives the keyframe remap itself.
                    restore_keyframes: None,
                    // Forward caller: apply derives the retime remap too.
                    restore_retime: None,
                }])
            }

            // ---- split (implicit playhead target when omitted) ------------
            Tool::SplitClip(args) => {
                // Resolve the target clip: explicit id, else the clip active on
                // the top video track at the program playhead.
                let (clip_id, fps) = match &args.clip_id {
                    Some(id) => {
                        let clip = find_clip(project, id)?;
                        (id.clone(), clip_fps(project, clip)?)
                    }
                    None => {
                        let hit = project
                            .timeline
                            .top_video_active_at(project.playback.position_us)
                            .ok_or(ToolError::NoActiveClip)?;
                        let clip = find_clip(project, &hit.clip_id)?;
                        (hit.clip_id, clip_fps(project, clip)?)
                    }
                };
                // Resolve the split point: explicit frame (clip-fps converted),
                // else the program playhead (already a timeline microsecond).
                let at_position_us = match args.at_frame {
                    Some(frame) => frame_to_us(frame, fps),
                    None => project.playback.position_us,
                };
                Ok(vec![Command::SplitClip {
                    id: clip_id,
                    at_position_us,
                }])
            }

            // ---- removal / duplicate / move (1:1) -------------------------
            Tool::RemoveClip(args) => {
                // Validate existence first so ToolError::ClipNotFound is raised
                // before Command::apply's own error (defense-in-depth, T-11-05).
                find_clip(project, &args.clip_id)?;
                Ok(vec![Command::RemoveClip {
                    id: args.clip_id.clone(),
                }])
            }

            Tool::DuplicateClip(args) => {
                find_clip(project, &args.clip_id)?;
                Ok(vec![Command::DuplicateClip {
                    id: args.clip_id.clone(),
                }])
            }

            Tool::MoveClip(args) => {
                let clip = find_clip(project, &args.clip_id)?;
                let fps = clip_fps(project, clip)?;
                let new_start_us = frame_to_us(args.to_frame, fps);
                Ok(vec![Command::MoveClip {
                    id: args.clip_id.clone(),
                    new_start_us,
                }])
            }

            // ---- audio: volume (absorbs the dB->linear-gain conversion) ---
            Tool::SetClipVolume(args) => {
                // dB -> linear multiplier: 10^(dB/20). 0dB -> 1.0 (unity),
                // -6.0206dB -> 0.5. The caller works in dB, never a raw ratio.
                let volume = 10f32.powf(args.gain_db / 20.0);
                let mut cmds = vec![Command::SetClipVolume {
                    id: args.clip_id.clone(),
                    volume,
                }];
                // D-06: setting the static volume must clear any volume track
                // (else the track would override the just-set value under
                // sample_at's unwrap_or). Same tool layer, same contract as
                // set_clip_properties.
                clear_volume_track_if_present(project, &args.clip_id, &mut cmds);
                Ok(cmds)
            }

            // ---- audio: mute (maps onto SetClipVolume) --------------------
            // D-04: mute retained as sugar over SetClipVolume;
            // boolean-flag migration deferred (set_clip_properties absorbs
            // volume-SETTING only — mute stays this separate on/off sugar).
            Tool::SetClipMuted(args) => {
                // KNOWN LIMITATION: un-muting restores UNITY gain (1.0), NOT a
                // remembered pre-mute custom level — v1 has no "remembered
                // volume" state. Documented for the Phase 12 rulebook.
                let volume = if args.muted { 0.0 } else { 1.0 };
                let mut cmds = vec![Command::SetClipVolume {
                    id: args.clip_id.clone(),
                    volume,
                }];
                // D-06: muting sets a static volume too, so the same sibling
                // clear applies (an animated track would make the mute an
                // inaudible no-op otherwise).
                clear_volume_track_if_present(project, &args.clip_id, &mut cmds);
                Ok(cmds)
            }

            // ---- audio: detach / reattach (1:1) ---------------------------
            Tool::DetachAudio(args) => Ok(vec![Command::DetachAudio {
                clip_id: args.clip_id.clone(),
            }]),

            Tool::ReattachAudio(args) => Ok(vec![Command::ReattachAudio {
                video_id: args.video_clip_id.clone(),
                audio_id: args.audio_clip_id.clone(),
            }]),

            // ---- composite: removeSection (split both edges + remove) -----
            // Delegates to the shared `cut_range` helper (Phase 17-02: the SAME
            // cut logic ripple_delete_ranges composes — one implementation, no
            // drift). Leaves a GAP — no ripple, matching v1's non-ripple model.
            Tool::RemoveSection(args) => {
                // Label-based track addressing: resolve the v1/a1 label FIRST.
                let track = resolve_track_label(&project.timeline, &args.track)?;
                let from_us = frame_to_us(args.from_frame, args.fps);
                let to_us = frame_to_us(args.to_frame, args.fps);
                if from_us >= to_us {
                    return Err(ToolError::InvalidFrameRange);
                }
                cut_range(project, track, from_us, to_us)
            }

            // ---- composite: tightenPacing (gap-closing MoveClips) ---------
            // Delegates to the shared `close_gaps` helper (Phase 17-02: the
            // SAME gap-close walk ripple_delete_ranges composes).
            Tool::TightenPacing(args) => {
                // Label-based track addressing: resolve the v1/a1 label FIRST.
                let track = resolve_track_label(&project.timeline, &args.track)?;
                let max_gap_us = frame_to_us(args.max_gap_frames.unwrap_or(0), args.fps);
                close_gaps(project, track, max_gap_us)
            }

            // ---- canvas delete (1:1 wraps of existing Commands) -----------
            // No project inspection here: an unknown/hallucinated id is caught
            // by Command::RemoveAnnotation's own CoreError::AnnotationNotFound at
            // apply time (surfaces as an is_error tool_result, round continues).
            Tool::RemoveAnnotation(args) => Ok(vec![Command::RemoveAnnotation {
                id: args.id.clone(),
            }]),

            Tool::ClearCanvas(_) => Ok(vec![Command::ClearCanvas { space: None }]),

            // ---- batch: add_clips (N x PlaceClip semantics, atomic) --------
            Tool::AddClips(args) => {
                let steps: Vec<_> = args
                    .clips
                    .iter()
                    .map(|item| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            // Mirror PlaceClip exactly: media lookup, frame
                            // conversion via the media's OWN fps (project-fps
                            // fallback for audio-only media), still-image
                            // default placement length (placement_len_us).
                            // Label-based track addressing: resolve the v1/a1
                            // label FIRST (labels are stable across the batch:
                            // add_clips never adds/removes tracks).
                            let track = resolve_track_label(&scratch.timeline, &item.track)?;
                            let media = scratch
                                .media_bin
                                .iter()
                                .find(|m| m.id == item.media_id)
                                .ok_or_else(|| ToolError::MediaNotFound(item.media_id.clone()))?;
                            let start_us =
                                frame_to_us(item.start_frame, media_fps(scratch, media)?);
                            Ok(vec![Command::AddClip {
                                track,
                                clip: Clip {
                                    id: item.clip_id.clone(),
                                    media_id: item.media_id.clone(),
                                    start_us,
                                    in_us: 0,
                                    out_us: placement_len_us(media),
                                    volume: 1.0,
                                    audio_detached: false,
                                    // New clip from args: identity visuals
                                    // (no parent to inherit from — Phase 18).
                                    transform: crate::model::ClipTransform::default(),
                                    opacity: 1.0,
                                    crop: crate::model::ClipCrop::default(),
                                    // New clip: no animation yet (Phase 19).
                                    keyframes: crate::model::KeyframeTracks::default(),
                                    // New clip is media, not text (Phase 20).
                                    text: None,
                                    // Brand-new clip: straight alpha (Phase 28).
                                    alpha_mode: crate::model::AlphaMode::default(),
                                    // Brand-new clip plays 1:1 (260730-x2t).
                                    retime: None,
                                },
                            }])
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- batch: insert_clips (push-right then add, per item) -------
            Tool::InsertClips(args) => {
                let steps: Vec<_> = args
                    .clips
                    .iter()
                    .map(|item| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            let media = scratch
                                .media_bin
                                .iter()
                                .find(|m| m.id == item.media_id)
                                .ok_or_else(|| ToolError::MediaNotFound(item.media_id.clone()))?;
                            // Label-based track addressing: resolve the v1/a1
                            // label (UnknownTrackLabel on a bad label aborts
                            // the WHOLE batch — all-or-none).
                            let track = resolve_track_label(&scratch.timeline, &item.track)?;
                            // Placement length = real probed duration, or the
                            // still-image default (placement_len_us) — the SAME
                            // length shifts the pushed-right clips AND sizes the
                            // inserted clip, so the two can never disagree.
                            let len_us = placement_len_us(media);
                            // Same project-fps fallback as PlaceClip for
                            // audio-only media (no fps of its own).
                            let at_us = frame_to_us(item.at_frame, media_fps(scratch, media)?);
                            // Reading the SCRATCH here is what makes sequential
                            // insertions in one call stack correctly: the 2nd
                            // item sees the 1st item's shifts + inserted clip.
                            let mut cmds: Vec<Command> = scratch.timeline.tracks[track]
                                .clips
                                .iter()
                                .filter(|c| c.start_us >= at_us)
                                .map(|c| Command::MoveClip {
                                    id: c.id.clone(),
                                    new_start_us: c.start_us + len_us,
                                })
                                .collect();
                            cmds.push(Command::AddClip {
                                track,
                                clip: Clip {
                                    id: item.clip_id.clone(),
                                    media_id: item.media_id.clone(),
                                    start_us: at_us,
                                    in_us: 0,
                                    out_us: len_us,
                                    volume: 1.0,
                                    audio_detached: false,
                                    // New clip from args: identity visuals
                                    // (no parent to inherit from — Phase 18).
                                    transform: crate::model::ClipTransform::default(),
                                    opacity: 1.0,
                                    crop: crate::model::ClipCrop::default(),
                                    // New clip: no animation yet (Phase 19).
                                    keyframes: crate::model::KeyframeTracks::default(),
                                    // New clip is media, not text (Phase 20).
                                    text: None,
                                    // Brand-new clip: straight alpha (Phase 28).
                                    alpha_mode: crate::model::AlphaMode::default(),
                                    // Brand-new clip plays 1:1 (260730-x2t).
                                    retime: None,
                                },
                            });
                            Ok(cmds)
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- batch: remove_clips (gaps left, non-ripple) ---------------
            Tool::RemoveClips(args) => {
                let steps: Vec<_> = args
                    .clip_ids
                    .iter()
                    .map(|id| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            // Validate existence first so ToolError::ClipNotFound
                            // is raised before Command::apply's own error
                            // (defense-in-depth, matching singular RemoveClip).
                            find_clip(scratch, id)?;
                            Ok(vec![Command::RemoveClip { id: id.clone() }])
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- batch: move_clips (per-clip own-fps frame conversion) -----
            Tool::MoveClips(args) => {
                let steps: Vec<_> = args
                    .moves
                    .iter()
                    .map(|item| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            let clip = find_clip(scratch, &item.clip_id)?;
                            let fps = clip_fps(scratch, clip)?;
                            Ok(vec![Command::MoveClip {
                                id: item.clip_id.clone(),
                                new_start_us: frame_to_us(item.to_frame, fps),
                            }])
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- batch: split_clips (mirrors singular SplitClip per item) --
            Tool::SplitClips(args) => {
                let steps: Vec<_> = args
                    .splits
                    .iter()
                    .map(|item| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            // Replicates Tool::SplitClip's resolve body against
                            // the SCRATCH, so a later item can target an earlier
                            // item's minted `~s{n}` child id.
                            let (clip_id, fps) = match &item.clip_id {
                                Some(id) => {
                                    let clip = find_clip(scratch, id)?;
                                    (id.clone(), clip_fps(scratch, clip)?)
                                }
                                None => {
                                    let hit = scratch
                                        .timeline
                                        .top_video_active_at(scratch.playback.position_us)
                                        .ok_or(ToolError::NoActiveClip)?;
                                    let clip = find_clip(scratch, &hit.clip_id)?;
                                    (hit.clip_id, clip_fps(scratch, clip)?)
                                }
                            };
                            let at_position_us = match item.at_frame {
                                Some(frame) => frame_to_us(frame, fps),
                                None => scratch.playback.position_us,
                            };
                            Ok(vec![Command::SplitClip {
                                id: clip_id,
                                at_position_us,
                            }])
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- ripple: cut range(s) AND close the gaps, atomically --------
            Tool::RippleDeleteRanges(args) => {
                // Label-based track addressing: resolve the v1/a1 label FIRST.
                let track = resolve_track_label(&project.timeline, &args.track)?;
                // An empty ranges list is a nonsensical call — reject rather
                // than silently no-op (the authored description enforces
                // ">= 1 range" in text; this is the code-side guarantee).
                if args.ranges.is_empty() {
                    return Err(ToolError::InvalidFrameRange);
                }
                // Convert every range up-front; any inverted/empty range
                // aborts the WHOLE call before any scratch work.
                let mut ranges_us: Vec<(i64, i64)> = Vec::with_capacity(args.ranges.len());
                for r in &args.ranges {
                    let from_us = frame_to_us(r.from_frame, args.fps);
                    let to_us = frame_to_us(r.to_frame, args.fps);
                    if from_us >= to_us {
                        return Err(ToolError::InvalidFrameRange);
                    }
                    ranges_us.push((from_us, to_us));
                }
                // MERGE (Pitfall 6): sort by start and coalesce overlapping or
                // adjacent ranges (next.from <= cur.to) into disjoint ranges,
                // so range frame math is never applied to a timeline an
                // earlier overlapping cut already shifted.
                ranges_us.sort_by_key(|(from, _)| *from);
                let mut merged: Vec<(i64, i64)> = Vec::with_capacity(ranges_us.len());
                for (from_us, to_us) in ranges_us {
                    match merged.last_mut() {
                        Some((_, cur_to)) if from_us <= *cur_to => {
                            *cur_to = (*cur_to).max(to_us);
                        }
                        _ => merged.push((from_us, to_us)),
                    }
                }
                // Process the disjoint ranges RIGHT-TO-LEFT (descending
                // from_us): each cut+shift only moves clips at/after its own
                // cut point leftward, so the already-computed microsecond
                // positions of EARLIER (leftward) ranges stay valid.
                //
                // Per merged range: the cut_range step, then a
                // ripple_shift_after_cut step that pulls ONLY the clips at/after
                // this cut's own `to_us` left by exactly the cut width
                // (`to_us - from_us`). This closes the gap the cut itself made
                // WITHOUT touching pre-existing, unrelated gaps elsewhere on the
                // track (HI-02) and correctly reclaims the space when the cut
                // removes the current first clip (HI-03) — the shift is anchored
                // to the cut geometry, never to "whichever clip is first after
                // removal". resolve_on_scratch applies each step's commands to
                // the shared scratch BEFORE the next step runs, so the shift sees
                // the cut's effect (and a later range's shift sees everything
                // before it). Boxed closures because the two step kinds are
                // distinct closure types.
                let mut steps: Vec<Box<dyn FnOnce(&Project) -> Result<Vec<Command>, ToolError>>> =
                    Vec::with_capacity(merged.len() * 2);
                for (from_us, to_us) in merged.into_iter().rev() {
                    steps.push(Box::new(move |scratch: &Project| {
                        cut_range(scratch, track, from_us, to_us)
                    }));
                    steps.push(Box::new(move |scratch: &Project| {
                        ripple_shift_after_cut(scratch, track, from_us, to_us)
                    }));
                }
                resolve_on_scratch(project, steps)
            }

            // ---- project timebase (COMP-01, 1:1 wrap) ----------------------
            // Numeric validation (finite fps in (0, 240], dims 1..=7680) is
            // the Command::apply gate (T-18-02) — the tool passes floats
            // through.
            Tool::SetProjectSettings(args) => Ok(vec![Command::SetProjectSettings {
                fps: args.fps,
                width: args.width,
                height: args.height,
            }]),

            // ---- batch: set_clip_properties (TOOL-03, D-03/D-06) -----------
            // ONE set of values applied to every clip in clip_ids: per clip,
            // emit a command for each PRESENT optional field only — composed
            // from the Plan-03 per-field commands (transform/opacity/crop),
            // the ABSORBED SetClipVolume (volume), and the EXISTING TrimClip
            // semantics (trim: no ripple, no linked-partner relink — D-06).
            // All-or-none via resolve_on_scratch; each field's numeric
            // validation stays at its own Command::apply gate (T-18-01).
            Tool::SetClipProperties(args) => {
                if args.clip_ids.is_empty() {
                    return Err(ToolError::EmptyBatch(
                        "clip_ids must contain at least one clip id",
                    ));
                }
                let has_any_property = args.transform.is_some()
                    || args.opacity.is_some()
                    || args.crop.is_some()
                    || args.volume.is_some()
                    || args.trim.is_some()
                    || args.speed.is_some();
                if !has_any_property {
                    return Err(ToolError::EmptyBatch(
                        "at least one property (transform, opacity, crop, volume, trim, \
                         speed) must be provided",
                    ));
                }
                let steps: Vec<_> = args
                    .clip_ids
                    .iter()
                    .map(|id| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            // Validate existence FIRST so one unknown id aborts
                            // the whole batch (defense-in-depth, all-or-none).
                            let clip = find_clip(scratch, id)?;
                            let mut cmds: Vec<Command> = Vec::new();
                            if let Some(transform) = args.transform {
                                cmds.push(Command::SetClipTransform {
                                    id: id.clone(),
                                    transform,
                                });
                            }
                            if let Some(opacity) = args.opacity {
                                cmds.push(Command::SetClipOpacity {
                                    id: id.clone(),
                                    opacity,
                                });
                            }
                            if let Some(crop) = args.crop {
                                cmds.push(Command::SetClipCrop {
                                    id: id.clone(),
                                    crop,
                                });
                            }
                            if let Some(volume) = args.volume {
                                // D-03: ABSORBS the volume path — the SAME
                                // Command::SetClipVolume the legacy tool emits
                                // (which clamps to finite [0,10] at apply).
                                // Unit here is the clip state's own LINEAR
                                // multiplier, not dB (setClipVolume keeps dB).
                                cmds.push(Command::SetClipVolume {
                                    id: id.clone(),
                                    volume,
                                });
                            }
                            if let Some(trim) = args.trim {
                                // Mirrors Tool::TrimClip exactly (D-06: the
                                // standalone trim path's semantics, no new
                                // timing behavior): frames in this clip's OWN
                                // media fps; left edge shifts start via the
                                // command's own delta math; NEVER ripples
                                // neighbours or relinks a detached partner.
                                let fps = clip_fps(scratch, clip)?;
                                let target_us = frame_to_us(trim.to_frame, fps);
                                // The TIMELINE offset of the target becomes a
                                // SOURCE offset through the speed integral
                                // (260730-x2t, RT-04) — the identity for an
                                // un-retimed clip, so every pre-retime trim
                                // test stays green unchanged.
                                let src_off = clip.source_offset_at(target_us - clip.start_us);
                                let (new_in_us, new_out_us) = match trim.edge {
                                    TrimEdge::Start => (clip.in_us + src_off, clip.out_us),
                                    TrimEdge::End => (clip.in_us, clip.in_us + src_off),
                                };
                                cmds.push(Command::TrimClip {
                                    id: id.clone(),
                                    new_in_us,
                                    new_out_us,
                                    // Forward caller: apply derives the remap.
                                    restore_keyframes: None,
                                    restore_retime: None,
                                });
                            }
                            // Speed LAST among the mutations (260730-x2t,
                            // RT-01): a `trim` in the SAME call is resolved
                            // against the clip's PRE-call curve, so trim and
                            // speed both read one consistent snapshot instead
                            // of the trim seeing a half-applied retime. ONE
                            // command, no new mutation primitive.
                            //
                            // No `push_track_clear` is needed for speed: unlike
                            // the visual properties there is no separate speed
                            // KEYFRAME TRACK to clear — a ramp and a constant
                            // are the SAME `Clip.retime` field (RT-02), so
                            // `SetClipRetime`'s whole-field replace satisfies
                            // the "static setter clears the animation" contract
                            // structurally.
                            if let Some(speed) = args.speed {
                                cmds.push(Command::SetClipRetime {
                                    id: id.clone(),
                                    retime: Some(crate::model::Retime {
                                        curve: crate::model::RetimeCurve::Constant(speed),
                                        // Both derived fields are rebuilt at
                                        // apply from the clip's own span and
                                        // the project fps — never authored here.
                                        timeline_len_us: 0,
                                        timebase_fps: 0.0,
                                    }),
                                });
                            }
                            // D-06 (Phase 18's forward-contract): setting a
                            // STATIC property clears the matching keyframe
                            // track — CONDITIONALLY, only when the pre-dispatch
                            // clip's track is non-empty. `clip` is the scratch
                            // snapshot BEFORE this closure's commands apply, so
                            // it reflects the pre-set track state. Conditional
                            // emission keeps un-animated resolves byte-identical
                            // (no no-op undo pollution) and is what makes the
                            // 30+ existing eval fixtures byte-stable. The clear
                            // is a self-inverse SetKeyframes carrying the OLD
                            // track, so one turn-level undo restores static +
                            // track together (T-19-05).
                            //
                            // FOOTGUN: a RAW Command::SetClipTransform/Opacity/
                            // Crop/Volume dispatched OUTSIDE these tools does
                            // NOT clear tracks — any future direct-UI setter
                            // path must route through this same clear.
                            let kf = &clip.keyframes;
                            if args.transform.is_some() {
                                push_track_clear(
                                    &mut cmds,
                                    id,
                                    !kf.position.is_empty(),
                                    KeyframeTrackData::Position(Vec::new()),
                                );
                                push_track_clear(
                                    &mut cmds,
                                    id,
                                    !kf.scale.is_empty(),
                                    KeyframeTrackData::Scale(Vec::new()),
                                );
                                push_track_clear(
                                    &mut cmds,
                                    id,
                                    !kf.rotation.is_empty(),
                                    KeyframeTrackData::Rotation(Vec::new()),
                                );
                            }
                            if args.opacity.is_some() {
                                push_track_clear(
                                    &mut cmds,
                                    id,
                                    !kf.opacity.is_empty(),
                                    KeyframeTrackData::Opacity(Vec::new()),
                                );
                            }
                            if args.crop.is_some() {
                                push_track_clear(
                                    &mut cmds,
                                    id,
                                    !kf.crop.is_empty(),
                                    KeyframeTrackData::Crop(Vec::new()),
                                );
                            }
                            if args.volume.is_some() {
                                push_track_clear(
                                    &mut cmds,
                                    id,
                                    !kf.volume.is_empty(),
                                    KeyframeTrackData::Volume(Vec::new()),
                                );
                            }
                            Ok(cmds)
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- batch: remove_tracks (TL-06; its add-side sibling is the ------
            // Tool::AddTrack arm below, added by the live-UAT TRACK-ADD-AGENT-GAP
            // fix). Every index validated against the ORIGINAL project
            // up front (unique + in range), then removed HIGHEST index first so
            // removing one never invalidates a still-pending higher index in the
            // SAME batch (Pitfall 1, in-batch case).
            Tool::RemoveTracks(args) => {
                if args.tracks.is_empty() {
                    return Err(ToolError::EmptyBatch(
                        "tracks must contain at least one track label",
                    ));
                }
                // Label-based track addressing: every LABEL resolved against
                // the ORIGINAL project up front (unknown/out-of-range =>
                // UnknownTrackLabel; two labels resolving to the same track =>
                // DuplicateTrackLabel) — all-or-none before any scratch work.
                let mut seen = std::collections::HashSet::new();
                let mut indices: Vec<usize> = Vec::with_capacity(args.tracks.len());
                for label in &args.tracks {
                    let idx = resolve_track_label(&project.timeline, label)?;
                    if !seen.insert(idx) {
                        return Err(ToolError::DuplicateTrackLabel(label.clone()));
                    }
                    indices.push(idx);
                }
                indices.sort_unstable_by(|a, b| b.cmp(a));
                let steps: Vec<_> = indices
                    .into_iter()
                    .map(|idx| {
                        move |_scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            Ok(vec![Command::RemoveTrack { index: idx }])
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- add_track (live-UAT TRACK-ADD-AGENT-GAP fix) ----------------
            // 1:1 wrap of the validated Command::AddTrack (the
            // SetProjectSettings direct-wrap precedent — pure core, no I/O).
            // Infallible: TrackKind is a closed 2-variant enum (malformed kinds
            // are rejected at the serde boundary), and placement is the
            // Command's own kind-aware invariant — video inserts ON TOP at
            // index 0 (existing tracks shift down), audio appends at the
            // BOTTOM. Inverse (RemoveTrack at the resulting index) is minted by
            // Command::apply, so one call = one undo step for free.
            Tool::AddTrack(args) => Ok(vec![Command::AddTrack { kind: args.kind }]),

            // ---- keyframes: set_keyframes (Phase 19-02, COMP-04) -------------
            // Property→arity mapping ONLY happens here (position/scale = 2
            // [x,y]; rotation/opacity/volume = 1; crop = 4 [left,top,right,
            // bottom]); the typed KeyframeTrackData variant IS the property.
            // Everything else — cap (T-19-01), stable-sort + duplicate reject
            // (T-19-03), per-property value clamps/rejects (T-19-02), unknown
            // clip — is Command::SetKeyframes' OWN validate-then-apply,
            // exercised on the resolve_on_scratch clone so any invalid input
            // aborts with the live project untouched (all-or-none, D-09).
            Tool::SetKeyframes(args) => {
                // SPEED is resolved BEFORE the KeyframeTrackData match and does
                // NOT go into `KeyframeTracks` (260730-x2t, RT-02): a speed key
                // sets the RATE at that instant and is INTEGRATED over time,
                // whereas every other property here is SAMPLED at a time.
                // Adding a `Speed` variant to `KeyframeTrackData` would corrupt
                // `sample_at`, `animated_names`, `shifted_for_left_cut` and
                // `KeyframeTracks::replace`. It lands on `Clip.retime` via the
                // SAME `Command::SetClipRetime` the constant-speed half uses —
                // one field, one command, ZERO new tools (RT-01).
                if args.property == "speed" {
                    let keys = build_speed_keys(&args.keyframes)?;
                    let id = args.clip_id.clone();
                    // An EMPTY array clears the retime entirely (the D-01
                    // full-track-replace contract, applied to the curve).
                    let retime = if keys.is_empty() {
                        None
                    } else {
                        Some(crate::model::Retime {
                            curve: crate::model::RetimeCurve::Ramp(keys),
                            // Both derived fields are rebuilt at apply.
                            timeline_len_us: 0,
                            timebase_fps: 0.0,
                        })
                    };
                    let steps =
                        vec![move |_scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            Ok(vec![Command::SetClipRetime { id, retime }])
                        }];
                    return resolve_on_scratch(project, steps);
                }
                let track = build_keyframe_track(&args.property, &args.keyframes)?;
                let id = args.clip_id.clone();
                let steps = vec![move |_scratch: &Project| -> Result<Vec<Command>, ToolError> {
                    Ok(vec![Command::SetKeyframes { id, track }])
                }];
                resolve_on_scratch(project, steps)
            }

            // ---- text: add_texts (BATCH create, all-or-none track rule) ------
            // Composes ONE Command::AddText per entry through resolve_on_scratch.
            // start/end are PROJECT-fps frames -> in_us=0 / out_us=duration; each
            // entry mints a deterministic `text~{n}` id on the progressively-
            // mutated scratch clone (so batch entries never collide). Every
            // finiteness/family/times/content check is DELEGATED to
            // Command::AddText's own validate-then-apply — the resolve layer adds
            // ONLY the frame math, id minting, track-rule enforcement, the
            // auto-fit-sentinel intent, and the TEXT-EVICTS-CLIP content-loss
            // guard (occupied default target => new overlay track on top;
            // occupied EXPLICIT target => hard reject).
            Tool::AddTexts(args) => {
                if args.entries.is_empty() {
                    return Err(ToolError::EmptyBatch(
                        "entries must contain at least one text entry",
                    ));
                }
                if args.entries.len() > MAX_TEXT_BATCH {
                    return Err(ToolError::TextBatchTooLarge(args.entries.len()));
                }
                // All-or-none track rule (label-shift-bug prevention): either
                // EVERY entry names a track label, or NONE do; a MIXED batch is
                // rejected outright.
                let all_named = args.entries.iter().all(|e| e.track.is_some());
                let none_named = args.entries.iter().all(|e| e.track.is_none());
                if !all_named && !none_named {
                    return Err(ToolError::MixedTrackBatch);
                }
                let fps = project.fps;
                // Label-based track addressing: resolve every explicit v1/a1
                // label ONCE against the ORIGINAL project (an unknown label
                // aborts the WHOLE batch before any scratch work). Explicit
                // batches never add tracks, so labels stay valid throughout.
                let explicit_tracks: Vec<Option<usize>> = args
                    .entries
                    .iter()
                    .map(|e| {
                        e.track
                            .as_deref()
                            .map(|l| resolve_track_label(&project.timeline, l))
                            .transpose()
                    })
                    .collect::<Result<_, _>>()?;
                // Explicit-track path (all_named): NEVER silently cover
                // pre-existing footage (live-UAT TEXT-EVICTS-CLIP content-loss
                // guard). An entry whose range overlaps ANY clip already on its
                // named track is rejected up front — same-track overlap
                // resolves "last wins" in `Timeline::active_at`, so placing
                // there would shadow (= delete from preview AND export) the
                // existing clip for the whole overlap. Non-overlapping ranges
                // on an occupied track remain allowed.
                if all_named {
                    for (e, explicit) in args.entries.iter().zip(&explicit_tracks) {
                        let track = explicit.expect("all_named => every entry names a track");
                        let start_us = frame_to_us(e.start_frame, fps);
                        let dur_us = frame_to_us(e.end_frame - e.start_frame, fps);
                        if track_range_occupied(project, track, start_us, dur_us) {
                            return Err(ToolError::TextTrackOccupied {
                                // Report the CANONICAL label (the resolved
                                // track's own gutter label, lowercase).
                                track: track_label(&project.timeline, track)
                                    .unwrap_or_else(|| e.track.clone().unwrap_or_default()),
                                start_frame: e.start_frame,
                                end_frame: e.end_frame,
                            });
                        }
                    }
                }
                // Track resolution (Open Q2 DECISION, amended by the live-UAT
                // TEXT-EVICTS-CLIP fix): when NO entry names a track, place
                // every text clip on the TOP-most existing Video track (lowest
                // index among Video tracks) so text composites ABOVE video
                // (SC-1) — but ONLY when that track is FREE for every entry's
                // range. If ANY entry would overlap a clip already there,
                // compose ONE Command::AddTrack{Video} (inserts at index 0, ON
                // TOP — Phase 18.1 kind-aware placement, so the old "append =
                // bottom would hide text" objection no longer applies) and
                // place the WHOLE batch on that new, empty overlay track: a
                // title/caption must NEVER shadow/evict existing footage. No
                // Video track at all => a clear error instructing the user to
                // add one (the agent has add_track for exactly that).
                let (default_track, needs_overlay) = if none_named {
                    let top = top_video_track_index(project).ok_or(ToolError::NoVideoTrack)?;
                    let occupied = args.entries.iter().any(|e| {
                        track_range_occupied(
                            project,
                            top,
                            frame_to_us(e.start_frame, fps),
                            frame_to_us(e.end_frame - e.start_frame, fps),
                        )
                    });
                    // After AddTrack{Video} the new empty overlay track IS
                    // index 0 (existing tracks shift down by one).
                    if occupied {
                        (Some(0), true)
                    } else {
                        (Some(top), false)
                    }
                } else {
                    (None, false)
                };
                let steps: Vec<_> = args
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        let explicit = explicit_tracks[i];
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            let track = explicit
                                .or(default_track)
                                .ok_or(ToolError::NoVideoTrack)?;
                            let style = build_text_style(entry)?;
                            // M-01: an EXPLICIT scale (0,0) collides with the
                            // auto-fit sentinel — reject it so (0,0) is ONLY
                            // reachable by OMITTING transform (a zero-size text
                            // clip renders nothing, so this loses no capability).
                            if let Some(t) = &entry.transform {
                                if t.scale == (0.0, 0.0) {
                                    return Err(ToolError::TextAutofitScaleReserved);
                                }
                            }
                            let start_us = frame_to_us(entry.start_frame, fps);
                            // Duration in µs from the frame span; a non-positive
                            // span makes out_us <= in_us, which AddText rejects
                            // (InvalidTimes) — aborting the whole batch.
                            let out_us = frame_to_us(entry.end_frame - entry.start_frame, fps);
                            let id = unique_text_id(scratch);
                            // Content-loss guard: the FIRST entry of an
                            // overlay-bound batch carries the ONE AddTrack that
                            // mints the new empty top track (index 0) every
                            // entry then lands on — one track per call, applied
                            // on scratch before any AddText so the whole batch
                            // stays all-or-none.
                            let mut cmds: Vec<Command> = Vec::with_capacity(2);
                            if i == 0 && needs_overlay {
                                cmds.push(Command::AddTrack {
                                    kind: TrackKind::Video,
                                });
                            }
                            cmds.push(Command::AddText {
                                track_index: track,
                                clip: Clip {
                                    id,
                                    // Text clip: empty media-id sentinel.
                                    media_id: String::new(),
                                    start_us,
                                    in_us: 0,
                                    out_us,
                                    volume: 1.0,
                                    audio_detached: false,
                                    // Explicit transform when the caller pinned
                                    // one; else the auto-fit sentinel (Plan 04
                                    // interprets scale=(0,0) as "auto-fit").
                                    transform: entry
                                        .transform
                                        .unwrap_or(TEXT_AUTOFIT_TRANSFORM),
                                    opacity: 1.0,
                                    crop: crate::model::ClipCrop::default(),
                                    keyframes: crate::model::KeyframeTracks::default(),
                                    text: Some(TextPayload {
                                        content: entry.content.clone(),
                                        style,
                                        // A plain add_texts clip is NOT a caption.
                                        caption_group_id: None,
                                    }),
                                    // Text clip: straight alpha (Phase 28).
                                    alpha_mode: crate::model::AlphaMode::default(),
                                    // A text clip has no source media to
                                    // resample -- its timeline length IS its
                                    // authored duration (260730-x2t).
                                    retime: None,
                                },
                            });
                            Ok(cmds)
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- text: update_text (partial-merge style edit, all-or-none) ---
            // Composes ONE Command::UpdateText (style partial-merge) per target,
            // PLUS an optional Command::SetClipTransform in the SAME step when a
            // transform is supplied, so a style+transform edit is one atomic
            // batch. Every target is validated to be an existing TEXT clip BEFORE
            // any command (a non-text or missing target aborts the whole batch);
            // the merged-style finiteness/family re-validation stays
            // Command::UpdateText's own gate (single validation source).
            Tool::UpdateText(args) => {
                if args.updates.is_empty() {
                    return Err(ToolError::EmptyBatch(
                        "updates must contain at least one text update",
                    ));
                }
                if args.updates.len() > MAX_TEXT_BATCH {
                    return Err(ToolError::TextBatchTooLarge(args.updates.len()));
                }
                let steps: Vec<_> = args
                    .updates
                    .iter()
                    .map(|update| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            // Resolve the target clip id(s). groupId mode expands
                            // to EVERY text clip whose caption_group_id matches (a
                            // zero-match group is a hard reject); the original
                            // single-clip mode validates the one id is a text
                            // clip. Exactly one of clipId/groupId must be set.
                            let target_ids: Vec<String> = match update
                                .group_id
                                .as_ref()
                                .filter(|g| !g.is_empty())
                            {
                                Some(group_id) => {
                                    let ids: Vec<String> = scratch
                                        .timeline
                                        .tracks
                                        .iter()
                                        .flat_map(|t| t.clips.iter())
                                        .filter(|c| {
                                            c.text
                                                .as_ref()
                                                .and_then(|t| t.caption_group_id.as_deref())
                                                == Some(group_id.as_str())
                                        })
                                        .map(|c| c.id.clone())
                                        .collect();
                                    if ids.is_empty() {
                                        return Err(ToolError::CaptionGroupNotFound(
                                            group_id.clone(),
                                        ));
                                    }
                                    ids
                                }
                                None => {
                                    if update.clip_id.is_empty() {
                                        return Err(ToolError::NoTextTarget);
                                    }
                                    // Validate the target exists AND is a text
                                    // clip up front, so one bad target aborts the
                                    // whole batch.
                                    let clip = find_clip(scratch, &update.clip_id)?;
                                    if clip.text.is_none() {
                                        return Err(ToolError::Command(CoreError::NotATextClip(
                                            update.clip_id.clone(),
                                        )));
                                    }
                                    vec![update.clip_id.clone()]
                                }
                            };
                            // ONE shared patch, applied to every resolved target
                            // (reuses build_text_patch — no duplicate
                            // TextStylePatch machinery, Open Q2 decision).
                            let patch = build_text_patch(update)?;
                            let has_style = text_patch_has_any(&patch);
                            let mut cmds: Vec<Command> = Vec::new();
                            for id in &target_ids {
                                if has_style {
                                    cmds.push(Command::UpdateText {
                                        clip_id: id.clone(),
                                        patch: patch.clone(),
                                    });
                                }
                                if let Some(transform) = update.transform {
                                    cmds.push(Command::SetClipTransform {
                                        id: id.clone(),
                                        transform,
                                    });
                                }
                            }
                            if cmds.is_empty() {
                                return Err(ToolError::EmptyBatch(
                                    "each update must change at least one style field or the transform",
                                ));
                            }
                            Ok(cmds)
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- transcript word-cuts: remove_words (Phase 22-03, TEXT-03) --
            // Delete agent-supplied SOURCE-media word spans from ONE clip and
            // close the gaps, reusing the EXACT cut_range + ripple_shift_after_cut
            // primitives RippleDeleteRanges composes (zero new ripple code, zero
            // new Command variant). SC-3 property: each source span is mapped to
            // the clip's CURRENT [in_us,out_us) timeline range at resolve() time
            // (never a cached index — `project` already carries every prior
            // same-turn edit), and a span now OUTSIDE that window aborts the
            // WHOLE call (all-or-none reject, Pitfall 5 / T-22-08).
            Tool::RemoveWords(args) => {
                // Locate the clip + its track index on the CURRENT timeline
                // (`project` already carries every prior same-turn edit).
                let (track, clip) = find_clip_with_track(project, &args.clip_id)?;
                // Batch-size ceiling (DoS guard) — the same MAX_TEXT_BATCH cap
                // every other Phase-20/22 batch tool enforces, applied here for a
                // consistent defense-in-depth posture (IN-02). Cheap first gate,
                // before any span/window validation or scratch work.
                if args.ranges.len() > MAX_TEXT_BATCH {
                    return Err(ToolError::TextBatchTooLarge(args.ranges.len()));
                }
                // Validate the source spans against the clip's CURRENT window and
                // map them to disjoint (merged), right-to-left-ready TIMELINE
                // ranges — inverted span / empty list / out-of-window all abort
                // the WHOLE call here, before any scratch work (all-or-none;
                // SC-3 / T-22-08 / T-22-09).
                let merged = merge_word_ranges_to_timeline(&args.clip_id, &args.ranges, clip)?;
                // Process RIGHT-TO-LEFT via the SHARED cut_range +
                // ripple_shift_after_cut primitives (zero new ripple code).
                let mut steps: Vec<Box<dyn FnOnce(&Project) -> Result<Vec<Command>, ToolError>>> =
                    Vec::with_capacity(merged.len() * 2);
                for (from_us, to_us) in merged.into_iter().rev() {
                    steps.push(Box::new(move |scratch: &Project| {
                        cut_range(scratch, track, from_us, to_us)
                    }));
                    steps.push(Box::new(move |scratch: &Project| {
                        ripple_shift_after_cut(scratch, track, from_us, to_us)
                    }));
                }
                resolve_on_scratch(project, steps)
            }

            // ---- captions: add_captions (Phase 22-04, TEXT-04) --------------
            // COPY of the AddTexts arm with exactly two changes: (a) resolve ONE
            // caption_group_id up front (caller-supplied, else a deterministic
            // `caption~grp~{n}` minted from the ORIGINAL project so every clip in
            // the batch shares it); (b) set that id on every minted clip's
            // TextPayload. Everything else — empty-check, MAX cap, all-or-none
            // track rule, top_video_track_index default, per-entry AddText
            // validation, resolve_on_scratch atomicity — is IDENTICAL to
            // add_texts (a caption IS an ordinary text clip). ZERO new Command
            // variant: Command::AddText is reused.
            Tool::AddCaptions(args) => {
                if args.entries.is_empty() {
                    return Err(ToolError::EmptyBatch(
                        "entries must contain at least one caption entry",
                    ));
                }
                if args.entries.len() > MAX_TEXT_BATCH {
                    return Err(ToolError::TextBatchTooLarge(args.entries.len()));
                }
                // All-or-none track rule (mirrors add_texts): either EVERY entry
                // names a track label or NONE do; a MIXED batch is rejected.
                let all_named = args.entries.iter().all(|e| e.track.is_some());
                let none_named = args.entries.iter().all(|e| e.track.is_none());
                if !all_named && !none_named {
                    return Err(ToolError::MixedTrackBatch);
                }
                let fps = project.fps;
                // Label-based track addressing (mirrors add_texts): resolve
                // every explicit v1/a1 label ONCE against the ORIGINAL project.
                let explicit_tracks: Vec<Option<usize>> = args
                    .entries
                    .iter()
                    .map(|e| {
                        e.track
                            .as_deref()
                            .map(|l| resolve_track_label(&project.timeline, l))
                            .transpose()
                    })
                    .collect::<Result<_, _>>()?;
                // Content-loss guards (live-UAT TEXT-EVICTS-CLIP), IDENTICAL to
                // add_texts: an EXPLICIT track whose range overlaps existing
                // footage is a hard reject; an occupied DEFAULT target routes
                // the whole batch onto ONE new overlay track inserted on top.
                if all_named {
                    for (e, explicit) in args.entries.iter().zip(&explicit_tracks) {
                        let track = explicit.expect("all_named => every entry names a track");
                        let start_us = frame_to_us(e.start_frame, fps);
                        let dur_us = frame_to_us(e.end_frame - e.start_frame, fps);
                        if track_range_occupied(project, track, start_us, dur_us) {
                            return Err(ToolError::TextTrackOccupied {
                                track: track_label(&project.timeline, track)
                                    .unwrap_or_else(|| e.track.clone().unwrap_or_default()),
                                start_frame: e.start_frame,
                                end_frame: e.end_frame,
                            });
                        }
                    }
                }
                let (default_track, needs_overlay) = if none_named {
                    let top = top_video_track_index(project).ok_or(ToolError::NoVideoTrack)?;
                    let occupied = args.entries.iter().any(|e| {
                        track_range_occupied(
                            project,
                            top,
                            frame_to_us(e.start_frame, fps),
                            frame_to_us(e.end_frame - e.start_frame, fps),
                        )
                    });
                    if occupied {
                        (Some(0), true)
                    } else {
                        (Some(top), false)
                    }
                } else {
                    (None, false)
                };
                // ONE shared group id for the WHOLE batch: caller-supplied
                // (non-empty), else a deterministic id minted from the ORIGINAL
                // project (stable across runs/redo, collision-free).
                let group_id = match args.group_id.as_ref().filter(|g| !g.is_empty()) {
                    Some(g) => g.clone(),
                    None => unique_caption_group_id(project),
                };
                let steps: Vec<_> = args
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        let group_id = group_id.clone();
                        let explicit = explicit_tracks[i];
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            let track = explicit
                                .or(default_track)
                                .ok_or(ToolError::NoVideoTrack)?;
                            let style = build_text_style(entry)?;
                            if let Some(t) = &entry.transform {
                                if t.scale == (0.0, 0.0) {
                                    return Err(ToolError::TextAutofitScaleReserved);
                                }
                            }
                            let start_us = frame_to_us(entry.start_frame, fps);
                            let out_us = frame_to_us(entry.end_frame - entry.start_frame, fps);
                            let id = unique_text_id(scratch);
                            // Content-loss guard (mirrors add_texts): the FIRST
                            // entry of an overlay-bound batch carries the ONE
                            // AddTrack minting the new empty top track.
                            let mut cmds: Vec<Command> = Vec::with_capacity(2);
                            if i == 0 && needs_overlay {
                                cmds.push(Command::AddTrack {
                                    kind: TrackKind::Video,
                                });
                            }
                            cmds.push(Command::AddText {
                                track_index: track,
                                clip: Clip {
                                    id,
                                    media_id: String::new(),
                                    start_us,
                                    in_us: 0,
                                    out_us,
                                    volume: 1.0,
                                    audio_detached: false,
                                    transform: entry
                                        .transform
                                        .unwrap_or(TEXT_AUTOFIT_TRANSFORM),
                                    opacity: 1.0,
                                    crop: crate::model::ClipCrop::default(),
                                    keyframes: crate::model::KeyframeTracks::default(),
                                    text: Some(TextPayload {
                                        content: entry.content.clone(),
                                        style,
                                        // The ONE difference from add_texts: this
                                        // clip carries the shared group tag.
                                        caption_group_id: Some(group_id.clone()),
                                    }),
                                    // Text clip: straight alpha (Phase 28).
                                    alpha_mode: crate::model::AlphaMode::default(),
                                    // A text clip has no source media to
                                    // resample -- its timeline length IS its
                                    // authored duration (260730-x2t).
                                    retime: None,
                                },
                            });
                            Ok(cmds)
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }

            // ---- layouts: apply_layout (Phase 23-01, COMP-03) ---------------
            // Named-template re-layout: validate the WHOLE batch (unknown
            // template/slot, duplicate slot/clip, empty) BEFORE any scratch
            // step, then compose ONLY SetClipTransform + SetClipCrop (+ the
            // D-06 keyframe-track-clear) per clip via resolve_on_scratch, so an
            // invalid request leaves the timeline byte-unchanged (SC-4) and the
            // whole batch reverses as ONE turn undo (SC-3, dispatched inside the
            // caller's open turn). ZERO new Command variant.
            Tool::ApplyLayout(args) => {
                if args.assignments.is_empty() {
                    return Err(ToolError::EmptyBatch(
                        "assignments must contain at least one slot assignment",
                    ));
                }
                if !LAYOUT_TEMPLATE_NAMES.contains(&args.template.as_str()) {
                    return Err(ToolError::UnknownLayoutTemplate(args.template.clone()));
                }
                let mut seen_slots = std::collections::HashSet::new();
                let mut seen_clips = std::collections::HashSet::new();
                let mut rects: Vec<((f32, f32), (f32, f32))> =
                    Vec::with_capacity(args.assignments.len());
                for a in &args.assignments {
                    let rect = layout_slot_rect(&args.template, &a.slot).ok_or_else(|| {
                        ToolError::UnknownLayoutSlot {
                            template: args.template.clone(),
                            slot: a.slot.clone(),
                        }
                    })?;
                    if !seen_slots.insert(a.slot.clone()) {
                        return Err(ToolError::DuplicateLayoutSlot(a.slot.clone()));
                    }
                    if !seen_clips.insert(a.clip_id.clone()) {
                        return Err(ToolError::DuplicateLayoutClip(a.clip_id.clone()));
                    }
                    rects.push(rect);
                }
                let project_w = project.width as f32;
                let project_h = project.height as f32;
                let steps: Vec<_> = args
                    .assignments
                    .iter()
                    .zip(rects)
                    .map(|(a, (position, scale))| {
                        let clip_id = a.clip_id.clone();
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            let clip = find_clip(scratch, &clip_id)?;
                            let media = scratch
                                .media_bin
                                .iter()
                                .find(|m| m.id == clip.media_id)
                                .ok_or_else(|| ToolError::MediaNotFound(clip.media_id.clone()))?;
                            let target_ar = (scale.0 * project_w) / (scale.1 * project_h);
                            let crop = layout_cover_crop(
                                media.width,
                                media.height,
                                media.rotation_degrees,
                                target_ar,
                            );
                            let transform = ClipTransform { position, scale, rotation_deg: 0.0 };
                            let mut cmds = vec![
                                Command::SetClipTransform { id: clip_id.clone(), transform },
                                Command::SetClipCrop { id: clip_id.clone(), crop },
                            ];
                            // D-06 forward rule (Pitfall 2): clear stale keyframe
                            // tracks that would otherwise override this static
                            // transform/crop at sample time (mirrors
                            // Tool::SetClipProperties).
                            let kf = &clip.keyframes;
                            push_track_clear(
                                &mut cmds,
                                &clip_id,
                                !kf.position.is_empty(),
                                KeyframeTrackData::Position(Vec::new()),
                            );
                            push_track_clear(
                                &mut cmds,
                                &clip_id,
                                !kf.scale.is_empty(),
                                KeyframeTrackData::Scale(Vec::new()),
                            );
                            push_track_clear(
                                &mut cmds,
                                &clip_id,
                                !kf.rotation.is_empty(),
                                KeyframeTrackData::Rotation(Vec::new()),
                            );
                            push_track_clear(
                                &mut cmds,
                                &clip_id,
                                !kf.crop.is_empty(),
                                KeyframeTrackData::Crop(Vec::new()),
                            );
                            Ok(cmds)
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }
            Tool::OrganizeMedia(args) => {
                if args.operations.is_empty() {
                    return Err(ToolError::EmptyBatch(
                        "operations must contain at least one entry",
                    ));
                }
                if args.operations.len() > MAX_ORGANIZE_MEDIA_BATCH {
                    return Err(ToolError::OrganizeMediaBatchTooLarge(args.operations.len()));
                }

                // ---- Structural pre-check on the RAW args, BEFORE any scratch work ----
                // (mirrors ApplyLayout's placement: cheapest gate first). Sanitizes
                // every path/name AND rejects every move/rename-folder cycle before any
                // Command is even constructed -- SC-1/SC-2's "all-or-none, before any
                // mutation" contract.
                let mut seen_targets = std::collections::HashSet::new();
                for op in &args.operations {
                    match op {
                        OrganizeMediaOp::CreateFolder { path } => {
                            sanitize_folder_path(path, false)?;
                            if !seen_targets.insert(format!("folder:{path}")) {
                                return Err(ToolError::DuplicateOrganizeTarget(path.clone()));
                            }
                        }
                        OrganizeMediaOp::MoveFolder { from, to } => {
                            sanitize_folder_path(from, false)?;
                            sanitize_folder_path(to, false)?;
                            if to == from || to.starts_with(&format!("{from}/")) {
                                return Err(ToolError::FolderMoveCycle(from.clone(), to.clone()));
                            }
                            if !seen_targets.insert(format!("folder:{from}")) {
                                return Err(ToolError::DuplicateOrganizeTarget(from.clone()));
                            }
                        }
                        OrganizeMediaOp::RenameFolder { path, name } => {
                            sanitize_folder_path(path, false)?;
                            if !is_safe_path_segment(name) {
                                return Err(ToolError::InvalidFolderPath(name.clone()));
                            }
                            if !seen_targets.insert(format!("folder:{path}")) {
                                return Err(ToolError::DuplicateOrganizeTarget(path.clone()));
                            }
                        }
                        OrganizeMediaOp::MoveMedia { media_id, folder } => {
                            sanitize_folder_path(folder, true)?;
                            if !seen_targets.insert(format!("media:{media_id}")) {
                                return Err(ToolError::DuplicateOrganizeTarget(media_id.clone()));
                            }
                        }
                        OrganizeMediaOp::RenameMedia { media_id, .. } => {
                            if !seen_targets.insert(format!("media:{media_id}")) {
                                return Err(ToolError::DuplicateOrganizeTarget(media_id.clone()));
                            }
                        }
                        OrganizeMediaOp::DeleteFolder { path } => {
                            sanitize_folder_path(path, false)?;
                            if !seen_targets.insert(format!("folder:{path}")) {
                                return Err(ToolError::DuplicateOrganizeTarget(path.clone()));
                            }
                        }
                        OrganizeMediaOp::DeleteMedia { media_id } => {
                            if !seen_targets.insert(format!("media:{media_id}")) {
                                return Err(ToolError::DuplicateOrganizeTarget(media_id.clone()));
                            }
                        }
                    }
                }

                // ---- Fixed phase order (palmier-cited): create -> move/rename -> delete.
                // NOT the caller's given order -- lets one call "create a folder, move
                // media into it, rename something, delete stale folders" without the
                // agent pre-sorting dependencies. Within a phase, ops keep their
                // caller-given relative order (resolve_on_scratch's existing "later
                // items see earlier items' effects" contract).
                let mut creates = Vec::new();
                let mut moves_and_renames = Vec::new();
                let mut deletes = Vec::new();
                for op in args.operations.clone() {
                    match op {
                        OrganizeMediaOp::CreateFolder { .. } => creates.push(op),
                        OrganizeMediaOp::MoveFolder { .. }
                        | OrganizeMediaOp::RenameFolder { .. }
                        | OrganizeMediaOp::MoveMedia { .. }
                        | OrganizeMediaOp::RenameMedia { .. } => moves_and_renames.push(op),
                        OrganizeMediaOp::DeleteFolder { .. } | OrganizeMediaOp::DeleteMedia { .. } => {
                            deletes.push(op)
                        }
                    }
                }

                let steps: Vec<_> = creates
                    .into_iter()
                    .chain(moves_and_renames)
                    .chain(deletes)
                    .map(|op| {
                        move |scratch: &Project| -> Result<Vec<Command>, ToolError> {
                            match op {
                                OrganizeMediaOp::CreateFolder { path } => {
                                    Ok(vec![Command::CreateMediaFolder { path }])
                                }
                                OrganizeMediaOp::MoveFolder { from, to } => {
                                    Ok(vec![Command::MoveMediaFolder { old_path: from, new_path: to }])
                                }
                                OrganizeMediaOp::RenameFolder { path, name } => {
                                    let new_path = match path.rsplit_once('/') {
                                        Some((parent, _)) => format!("{parent}/{name}"),
                                        None => name,
                                    };
                                    Ok(vec![Command::MoveMediaFolder { old_path: path, new_path }])
                                }
                                OrganizeMediaOp::MoveMedia { media_id, folder } => {
                                    Ok(vec![Command::MoveMediaItem { id: media_id, new_folder: folder }])
                                }
                                OrganizeMediaOp::RenameMedia { media_id, name } => {
                                    Ok(vec![Command::RenameMediaItem { id: media_id, display_name: name }])
                                }
                                OrganizeMediaOp::DeleteFolder { path } => {
                                    // Cascade: expand to every contained sub-folder
                                    // (deepest first) + every contained item, THEN the
                                    // folder itself. An in-use contained item's EXISTING
                                    // CoreError::MediaBinItemInUse fires inside
                                    // resolve_on_scratch and aborts the WHOLE
                                    // organize_media call (all-or-none, zero new logic).
                                    //
                                    // A NEVER-REGISTERED path yields an empty match set
                                    // here -> zero commands -> an explicit, DOCUMENTED
                                    // idempotent no-op (delete-what-is-not-there
                                    // succeeds, like `rm -f`); it never aborts the
                                    // batch. Covered by
                                    // organize_media_delete_never_registered_folder_is_noop.
                                    let mut sub_folders: Vec<String> = scratch
                                        .media_folders
                                        .iter()
                                        .filter(|f| **f == path || f.starts_with(&format!("{path}/")))
                                        .cloned()
                                        .collect();
                                    // Deepest first: a descendant's path string is
                                    // always longer than its ancestor's.
                                    sub_folders.sort_by_key(|f| std::cmp::Reverse(f.len()));
                                    let mut cmds = Vec::new();
                                    for f in &sub_folders {
                                        for item in scratch.media_bin.iter().filter(|m| m.folder == *f) {
                                            cmds.push(Command::RemoveMediaBinItem { id: item.id.clone() });
                                        }
                                    }
                                    for f in &sub_folders {
                                        cmds.push(Command::DeleteMediaFolder { path: f.clone() });
                                    }
                                    Ok(cmds)
                                }
                                OrganizeMediaOp::DeleteMedia { media_id } => {
                                    Ok(vec![Command::RemoveMediaBinItem { id: media_id }])
                                }
                            }
                        }
                    })
                    .collect();
                resolve_on_scratch(project, steps)
            }
        }
    }
}

/// Tool-layer wrapper over the shared [`crate::model::is_safe_folder_segment`]
/// reject-list (the ONE definition guarding both layers — 25-REVIEW CR-01).
/// Used by rename_folder's single-segment `name` fail-fast check.
fn is_safe_path_segment(segment: &str) -> bool {
    crate::model::is_safe_folder_segment(segment)
}

/// Tool-layer wrapper over the shared [`crate::model::is_valid_folder_path`]
/// reject-list, mapping a rejection to `ToolError::InvalidFolderPath`. This is
/// the FAIL-FAST (all-or-none) pre-check; `Command::apply` re-runs the SAME
/// shared validator as the authoritative guard (25-REVIEW CR-01). `allow_root`
/// permits the empty string (legal ONLY for move_media's `folder` field
/// addressing the library root).
fn sanitize_folder_path(path: &str, allow_root: bool) -> Result<(), ToolError> {
    if crate::model::is_valid_folder_path(path, allow_root) {
        Ok(())
    } else {
        Err(ToolError::InvalidFolderPath(path.to_string()))
    }
}

/// The index of the TOP-most Video track (lowest index among Video tracks),
/// or `None` if the timeline has no Video track. `add_texts` uses this to place
/// text ABOVE video (SC-1) when the caller names no track.
fn top_video_track_index(project: &Project) -> Option<usize> {
    project
        .timeline
        .tracks
        .iter()
        .position(|t| t.kind == TrackKind::Video)
}

/// True iff a clip spanning `[start_us, start_us + dur_us)` on `track` would
/// OVERLAP any clip already on that track (live-UAT TEXT-EVICTS-CLIP
/// content-loss guard). Overlap matters because `Timeline::active_at` resolves
/// same-track overlap as "LAST clip in track order wins" — a text clip pushed
/// onto an occupied range SHADOWS the pre-existing clip for the whole overlap
/// in preview AND export (the one composite path), which is real content loss
/// even though the model still holds the clip. `start_us` is clamped to 0 to
/// mirror Command::AddText's own negative-start clamp; a non-positive duration
/// is an empty range (overlaps nothing — the batch aborts later on
/// InvalidTimes either way). An out-of-range `track` reports unoccupied and
/// leaves the range check to Command::AddText's TrackOutOfRange validation.
fn track_range_occupied(project: &Project, track: usize, start_us: i64, dur_us: i64) -> bool {
    let start = start_us.max(0);
    let end = start + dur_us.max(0);
    project.timeline.tracks.get(track).is_some_and(|t| {
        t.clips
            .iter()
            .any(|c| c.start_us < end && start < c.timeline_end_us())
    })
}

/// Mint a deterministic, collision-free text-clip id (`text~{n}`, smallest free
/// `n >= 1` not used by any clip on the timeline). State-derived (never random)
/// so ids are stable across runs and identical on redo — the `unique_child_id`
/// discipline, applied on the progressively-mutated scratch clone so each
/// `add_texts` entry gets a distinct id.
fn unique_text_id(project: &Project) -> String {
    let mut n: u64 = 1;
    loop {
        let candidate = format!("text~{n}");
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

/// Mint a deterministic, collision-free caption-group id (`caption~grp~{n}`,
/// smallest free `n >= 1` not already used by any text clip's
/// `caption_group_id`). State-derived (never random) so the id is stable across
/// runs and identical on redo — the same discipline as [`unique_text_id`].
/// Minted ONCE per `add_captions` batch so every caption in the group shares it.
fn unique_caption_group_id(project: &Project) -> String {
    let mut n: u64 = 1;
    loop {
        let candidate = format!("caption~grp~{n}");
        let taken = project
            .timeline
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .any(|c| {
                c.text
                    .as_ref()
                    .and_then(|t| t.caption_group_id.as_deref())
                    == Some(candidate.as_str())
            });
        if !taken {
            return candidate;
        }
        n += 1;
    }
}

/// Parse a fill color string into RGBA bytes. Accepts `#RRGGBB` / `#RRGGBBAA`
/// hex (case-insensitive) and `rgb(r,g,b)` / `rgba(r,g,b,a)` where r/g/b are
/// 0-255 integers and the optional alpha is either a 0-1 float (contains `.`)
/// or a 0-255 integer. An unparseable string is a tool-level `Err` (all-or-none
/// gate) — never a panic.
pub(crate) fn parse_fill(s: &str) -> Result<[u8; 4], ToolError> {
    let t = s.trim();
    let invalid = || ToolError::InvalidFill(s.to_string());
    if let Some(hex) = t.strip_prefix('#') {
        // CR-01: `hex[from..from+2]` slices by BYTE offset — on a multi-byte
        // UTF-8 payload (e.g. "#1é234", whose 'é' is 2 bytes so `hex.len() == 6`
        // selects the 6-digit branch while a 2-byte window cuts mid-character)
        // that slice would PANIC on a non-char-boundary, violating the documented
        // "never panics" invariant for LLM-authored input. A hex color is ASCII by
        // definition, so reject any non-ASCII payload up front: after this guard
        // every byte is one char, so `len()` == digit count and every 2-byte
        // window lands on a char boundary.
        if !hex.is_ascii() {
            return Err(invalid());
        }
        let bytes = |from: usize| u8::from_str_radix(&hex[from..from + 2], 16).map_err(|_| invalid());
        return match hex.len() {
            6 => Ok([bytes(0)?, bytes(2)?, bytes(4)?, 255]),
            8 => Ok([bytes(0)?, bytes(2)?, bytes(4)?, bytes(6)?]),
            _ => Err(invalid()),
        };
    }
    let lower = t.to_ascii_lowercase();
    let inner = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(invalid)?;
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    if parts.len() != 3 && parts.len() != 4 {
        return Err(invalid());
    }
    let chan = |p: &str| p.parse::<u32>().ok().filter(|v| *v <= 255).map(|v| v as u8);
    let r = chan(parts[0]).ok_or_else(invalid)?;
    let g = chan(parts[1]).ok_or_else(invalid)?;
    let b = chan(parts[2]).ok_or_else(invalid)?;
    let a = match parts.get(3) {
        None => 255,
        Some(p) if p.contains('.') => {
            let f = p.parse::<f32>().ok().filter(|v| (0.0..=1.0).contains(v)).ok_or_else(invalid)?;
            (f * 255.0).round() as u8
        }
        Some(p) => chan(p).ok_or_else(invalid)?,
    };
    Ok([r, g, b, a])
}

/// Build a full [`TextStyle`] from an `add_texts` entry's flat style fields,
/// starting from [`TextStyle::default`] and overwriting only the supplied
/// fields. `font_size` finiteness/`> 0` and the bundled-family check are
/// DELEGATED to `Command::AddText`'s `validate_text_payload` (single source).
fn build_text_style(entry: &TextEntry) -> Result<TextStyle, ToolError> {
    let mut style = TextStyle::default();
    if let Some(f) = &entry.font_family {
        style.font_family = f.clone();
    }
    if let Some(s) = entry.font_size {
        style.font_size = s;
    }
    if let Some(f) = &entry.fill {
        style.fill = parse_fill(f)?;
    }
    if let Some(b) = entry.bold {
        style.bold = b;
    }
    if let Some(i) = entry.italic {
        style.italic = i;
    }
    if let Some(a) = entry.align {
        style.align = a;
    }
    if let Some(w) = entry.wrap_width {
        style.wrap_width = Some(w);
    }
    Ok(style)
}

/// Build a partial-merge [`TextStylePatch`] from an `update_text` entry's flat
/// optional fields. `wrap_width: Some(x)` sets an explicit wrap; the tool has no
/// way to reset to auto-fit (that stays `Command::RestoreText`'s content-edit
/// path), so `None` always means "leave alone".
fn build_text_patch(update: &TextUpdate) -> Result<TextStylePatch, ToolError> {
    Ok(TextStylePatch {
        font_family: update.font_family.clone(),
        font_size: update.font_size,
        fill: match &update.fill {
            Some(f) => Some(parse_fill(f)?),
            None => None,
        },
        bold: update.bold,
        italic: update.italic,
        align: update.align,
        // Some(x) -> Some(Some(x)) (set explicit wrap); None -> None (leave alone).
        wrap_width: update.wrap_width.map(Some),
    })
}

/// Whether a [`TextStylePatch`] overwrites at least one field (so an all-`None`
/// patch is skipped rather than dispatched as a no-op `UpdateText`).
fn text_patch_has_any(patch: &TextStylePatch) -> bool {
    patch.font_family.is_some()
        || patch.font_size.is_some()
        || patch.fill.is_some()
        || patch.bold.is_some()
        || patch.italic.is_some()
        || patch.align.is_some()
        || patch.wrap_width.is_some()
}

/// D-06 sibling clear: append a self-inverse `SetKeyframes` carrying the EMPTY
/// same-property variant, but ONLY when `non_empty` (the pre-dispatch track had
/// keyframes). Conditional emission keeps un-animated resolves byte-identical
/// (no no-op undo pollution) while guaranteeing a just-set STATIC value is not
/// silently overridden by a stale keyframe track (Plan 01 `sample_at` unwrap_or).
fn push_track_clear(cmds: &mut Vec<Command>, id: &str, non_empty: bool, empty: KeyframeTrackData) {
    if non_empty {
        cmds.push(Command::SetKeyframes {
            id: id.to_string(),
            track: empty,
        });
    }
}

/// D-06 for the volume tools (`setClipVolume` / `setClipMuted`, which both
/// resolve to `Command::SetClipVolume`): if the clip currently carries a
/// non-empty volume keyframe track, append the sibling clear so the just-set
/// static volume is audible rather than overridden. Missing-clip case is left
/// to the command's own validation (these arms intentionally don't pre-validate
/// existence — preserving their pre-19 behavior when there is no track).
fn clear_volume_track_if_present(project: &Project, clip_id: &str, cmds: &mut Vec<Command>) {
    if let Ok(clip) = find_clip(project, clip_id) {
        push_track_clear(
            cmds,
            clip_id,
            !clip.keyframes.volume.is_empty(),
            KeyframeTrackData::Volume(Vec::new()),
        );
    }
}

/// Map a `set_keyframes` property string + uniform number-array values onto
/// the typed [`KeyframeTrackData`] variant. The ONLY validation performed
/// here is the property name and each value's ARITY (a wrong-shaped value
/// can't be typed at all); numeric rules stay with the Command (T-19-02 —
/// single validation source). Omitted `interp` defaults to `Smooth` (O-2).
pub(crate) fn build_keyframe_track(
    property: &str,
    keyframes: &[KeyframeArg],
) -> Result<KeyframeTrackData, ToolError> {
    fn typed<V, F>(
        property: &str,
        arity: usize,
        keyframes: &[KeyframeArg],
        make: F,
    ) -> Result<Vec<Keyframe<V>>, ToolError>
    where
        F: Fn(&[f64]) -> V,
    {
        keyframes
            .iter()
            .map(|k| {
                if k.value.len() != arity {
                    return Err(ToolError::InvalidKeyframes(format!(
                        "property `{property}` takes exactly {arity} number(s) per \
                         keyframe value, got {} at frame {}",
                        k.value.len(),
                        k.frame
                    )));
                }
                Ok(Keyframe {
                    frame: k.frame,
                    value: make(&k.value),
                    interp: k.interp.unwrap_or_default(),
                })
            })
            .collect()
    }

    let pair = |v: &[f64]| (v[0] as f32, v[1] as f32);
    let scalar = |v: &[f64]| v[0] as f32;
    match property {
        "position" => Ok(KeyframeTrackData::Position(typed(
            property, 2, keyframes, pair,
        )?)),
        "scale" => Ok(KeyframeTrackData::Scale(typed(property, 2, keyframes, pair)?)),
        "rotation" => Ok(KeyframeTrackData::Rotation(typed(
            property, 1, keyframes, scalar,
        )?)),
        "opacity" => Ok(KeyframeTrackData::Opacity(typed(
            property, 1, keyframes, scalar,
        )?)),
        "volume" => Ok(KeyframeTrackData::Volume(typed(
            property, 1, keyframes, scalar,
        )?)),
        "crop" => Ok(KeyframeTrackData::Crop(typed(
            property,
            4,
            keyframes,
            |v: &[f64]| CropValue {
                left: v[0] as f32,
                top: v[1] as f32,
                right: v[2] as f32,
                bottom: v[3] as f32,
            },
        )?)),
        other => Err(ToolError::InvalidKeyframes(format!(
            "unknown property `{other}` — must be one of position, scale, \
             rotation, opacity, crop, volume, speed"
        ))),
    }
}

/// Build a SPEED ramp's keys from `set_keyframes`' generic keyframe args
/// (quick task 260730-x2t). Arity 1, exactly like rotation/opacity/volume, and
/// the SAME error shape — but the result is a `Vec<Keyframe<f32>>` bound for
/// `Clip.retime`, NOT a `KeyframeTrackData` (RT-02). Value bounds, the key cap
/// and the duplicate-frame reject are `Command::SetClipRetime`'s own
/// validate-then-apply gate, exercised on the `resolve_on_scratch` clone.
pub(crate) fn build_speed_keys(
    keyframes: &[KeyframeArg],
) -> Result<Vec<Keyframe<f32>>, ToolError> {
    keyframes
        .iter()
        .map(|k| {
            if k.value.len() != 1 {
                return Err(ToolError::InvalidKeyframes(format!(
                    "property `speed` takes exactly 1 number(s) per keyframe \
                     value, got {} at frame {}",
                    k.value.len(),
                    k.frame
                )));
            }
            Ok(Keyframe {
                frame: k.frame,
                value: k.value[0] as f32,
                interp: k.interp.unwrap_or_default(),
            })
        })
        .collect()
}

/// The id of a clip on `track` whose timeline occupancy STRICTLY straddles
/// `position_us` (`start_us < position_us < timeline_end_us()`) — i.e. a split
/// there produces two non-empty clips. `None` when `position_us` already lands
/// on a clip edge (or the track has no clip there).
fn straddling_clip(project: &Project, track: usize, position_us: i64) -> Option<String> {
    project
        .timeline
        .tracks
        .get(track)?
        .clips
        .iter()
        .find(|c| c.start_us < position_us && position_us < c.timeline_end_us())
        .map(|c| c.id.clone())
}

/// The ONE cut implementation (Phase 17-02): remove all clip content on
/// `track` in `[from_us, to_us)`, splitting clips straddling either edge, then
/// removing every clip fully enclosed. Leaves a GAP (non-ripple). Both
/// `removeSection`'s resolve arm AND `ripple_delete_ranges`' per-range steps
/// call this — a single implementation so the two can never drift.
///
/// Scratch-simulates against a clone so SplitClip's deterministic `~s{n}`
/// child ids are captured EXACTLY from `Command::apply`, without duplicating
/// `unique_child_id`'s prediction logic. The caller validates `track` is in
/// range BEFORE calling (this indexes `tracks[track]` directly).
fn cut_range(
    project: &Project,
    track: usize,
    from_us: i64,
    to_us: i64,
) -> Result<Vec<Command>, ToolError> {
    let mut scratch = project.clone();
    let mut out: Vec<Command> = Vec::new();

    // (1) split any clip STRICTLY straddling from_us.
    if let Some(id) = straddling_clip(&scratch, track, from_us) {
        let cmd = Command::SplitClip {
            id,
            at_position_us: from_us,
        };
        cmd.apply(&mut scratch)?;
        out.push(cmd);
    }
    // (2) re-scan; split any clip STRICTLY straddling to_us.
    if let Some(id) = straddling_clip(&scratch, track, to_us) {
        let cmd = Command::SplitClip {
            id,
            at_position_us: to_us,
        };
        cmd.apply(&mut scratch)?;
        out.push(cmd);
    }
    // (3) re-scan; remove every clip now fully contained in [from_us, to_us).
    //     Collect ids first, then apply removals one at a time (indices shift
    //     after each removal).
    let enclosed: Vec<String> = scratch.timeline.tracks[track]
        .clips
        .iter()
        .filter(|c| c.start_us >= from_us && c.timeline_end_us() <= to_us)
        .map(|c| c.id.clone())
        .collect();
    for id in enclosed {
        let cmd = Command::RemoveClip { id };
        cmd.apply(&mut scratch)?;
        out.push(cmd);
    }
    Ok(out)
}

/// The ONE gap-close implementation (Phase 17-02): walk `track`'s clips in
/// timeline order (ascending start); the first clip never moves; each later
/// clip whose gap-to-previous exceeds `max_gap_us` gets a MoveClip pulling it
/// to `prev_end + max_gap_us`. Both `tightenPacing`'s resolve arm AND
/// `ripple_delete_ranges`' per-range steps (with `max_gap_us = 0`) call this.
///
/// Ids are stable identifiers here (MoveClip mints no new id), so they are
/// captured up-front from a start-sorted snapshot. The caller validates
/// `track` is in range BEFORE calling.
fn close_gaps(project: &Project, track: usize, max_gap_us: i64) -> Result<Vec<Command>, ToolError> {
    let mut scratch = project.clone();
    let mut out: Vec<Command> = Vec::new();

    let mut ordered: Vec<(String, i64, i64)> = scratch.timeline.tracks[track]
        .clips
        .iter()
        .map(|c| (c.id.clone(), c.start_us, c.timeline_len_us()))
        .collect();
    ordered.sort_by_key(|(_, start, _)| *start);

    let mut cursor: Option<i64> = None; // previous clip's end
    for (id, start_us, len_us) in ordered {
        match cursor {
            None => {
                // First clip: never moves.
                cursor = Some(start_us + len_us);
            }
            Some(prev_end) => {
                let gap = start_us - prev_end;
                if gap > max_gap_us {
                    let new_start = prev_end + max_gap_us;
                    let cmd = Command::MoveClip {
                        id: id.clone(),
                        new_start_us: new_start,
                    };
                    cmd.apply(&mut scratch)?;
                    out.push(cmd);
                    cursor = Some(new_start + len_us);
                } else {
                    // Small-enough gap: leave untouched, advance the cursor to
                    // this clip's OWN (unmoved) end.
                    cursor = Some(start_us + len_us);
                }
            }
        }
    }
    Ok(out)
}

/// Ripple-shift helper (Phase 17, HI-02/HI-03): after `cut_range` emptied
/// `[from_us, to_us)` on `track`, pull every clip whose start is at/after
/// `to_us` LEFT by exactly the cut width (`to_us - from_us`). This closes ONLY
/// the gap the cut itself created — every downstream clip keeps its RELATIVE
/// offset, so pre-existing pacing gaps elsewhere on the track survive untouched
/// (HI-02), and a cut that consumes the current first clip still reclaims that
/// space instead of stranding a dead leading gap (HI-03: the shift is anchored
/// to the cut geometry, never to "whichever clip is first after removal").
/// Clips strictly before the cut (`start_us < to_us`) are never moved.
///
/// Ids are stable identifiers (MoveClip mints no new id), captured up-front from
/// a start-sorted snapshot and applied ascending so no two clips transiently
/// overlap mid-apply. The caller validates `track` is in range BEFORE calling.
fn ripple_shift_after_cut(
    project: &Project,
    track: usize,
    from_us: i64,
    to_us: i64,
) -> Result<Vec<Command>, ToolError> {
    let shift = to_us - from_us;
    if shift <= 0 {
        return Ok(Vec::new());
    }
    let mut scratch = project.clone();
    let mut out: Vec<Command> = Vec::new();

    let mut movers: Vec<(String, i64)> = scratch.timeline.tracks[track]
        .clips
        .iter()
        .filter(|c| c.start_us >= to_us)
        .map(|c| (c.id.clone(), c.start_us))
        .collect();
    movers.sort_by_key(|(_, start)| *start);

    for (id, start_us) in movers {
        let cmd = Command::MoveClip {
            id,
            new_start_us: start_us - shift,
        };
        cmd.apply(&mut scratch)?;
        out.push(cmd);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tool argument structs — the 12 Phase-11 clip tools + the 2 Phase-14.1 canvas
// deletes + the 5 Phase-17 batch tools. Legacy wire field names are camelCase;
// the batch tools' wire NAMES are snake_case (their args stay camelCase).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceClipArgs {
    pub clip_id: String,
    pub media_id: String,
    /// Track LABEL as shown in get_timeline (label-based track addressing):
    /// `"v1"` (video, bottom-up) / `"a1"` (audio, top-down), case-insensitive.
    pub track: String,
    pub start_frame: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrimClipArgs {
    pub clip_id: String,
    pub edge: TrimEdge,
    pub to_frame: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrimEdge {
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitClipArgs {
    #[serde(default)]
    pub clip_id: Option<String>,
    #[serde(default)]
    pub at_frame: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveClipArgs {
    pub clip_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveSectionArgs {
    /// Track LABEL as shown in get_timeline (`"v1"` / `"a1"`, case-insensitive).
    pub track: String,
    pub from_frame: i64,
    pub to_frame: i64,
    /// No project-level fps exists in v1 (research's flagged gap) — the caller
    /// supplies which fps these TRACK-level frame numbers are expressed in
    /// (normally the fps of whichever clip prompted the request). This is a
    /// tool-ARGS field, not new Project/Store state.
    pub fps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateClipArgs {
    pub clip_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveClipArgs {
    pub clip_id: String,
    pub to_frame: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetClipVolumeArgs {
    pub clip_id: String,
    pub gain_db: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetClipMutedArgs {
    pub clip_id: String,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetachAudioArgs {
    pub clip_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReattachAudioArgs {
    pub video_clip_id: String,
    pub audio_clip_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TightenPacingArgs {
    /// Track LABEL as shown in get_timeline (`"v1"` / `"a1"`, case-insensitive).
    pub track: String,
    #[serde(default)]
    pub max_gap_frames: Option<i64>,
    /// Same rationale as RemoveSectionArgs::fps.
    pub fps: f64,
}

/// Args for `removeAnnotation`: the exact id of one canvas annotation to delete.
/// The id is validated at Command::apply time (CoreError::AnnotationNotFound on
/// an unknown id), not here — matching removeClip's defense-in-depth shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveAnnotationArgs {
    pub id: String,
}

/// Args for `clearCanvas`: intentionally EMPTY. Modeled as a tuple variant over
/// an empty struct (not a bare unit variant) because the enum is adjacently
/// tagged with `content = "args"`, and `parse_edit_tool` ALWAYS injects an
/// `"args"` object — so the wire form is `{"tool":"clearCanvas","args":{}}`. A
/// bare unit variant rejects that (`invalid type: map, expected unit variant`);
/// an empty struct deserializes cleanly from `{}`. The round-trip test in
/// `agent_tools.rs` is the arbiter of this choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearCanvasArgs {}

// ---------------------------------------------------------------------------
// Batch tool args (Phase 17-01). Each list applies ALL-OR-NONE: one invalid
// item makes the whole resolve() Err before any live dispatch (D-02).
// ---------------------------------------------------------------------------

/// Args for `add_clips`: place MANY media items as new clips in one atomic call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddClipsArgs {
    pub clips: Vec<AddClipItem>,
}

/// One `add_clips` item — the exact PlaceClip argument set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddClipItem {
    pub clip_id: String,
    pub media_id: String,
    /// Track LABEL as shown in get_timeline (`"v1"` / `"a1"`, case-insensitive).
    pub track: String,
    pub start_frame: i64,
}

/// Args for `insert_clips`: insert clips, PUSHING later same-track clips right
/// by each inserted clip's timeline length to make room.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertClipsArgs {
    pub clips: Vec<InsertClipItem>,
}

/// One `insert_clips` item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertClipItem {
    pub clip_id: String,
    pub media_id: String,
    /// Track LABEL as shown in get_timeline (`"v1"` / `"a1"`, case-insensitive).
    pub track: String,
    pub at_frame: i64,
}

/// Args for `remove_clips`: remove many clips by id (gaps left, non-ripple).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveClipsArgs {
    pub clip_ids: Vec<String>,
}

/// Args for `move_clips`: move many clips to new start frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveClipsArgs {
    pub moves: Vec<MoveClipItem>,
}

/// One `move_clips` item — frames in that clip's OWN media fps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveClipItem {
    pub clip_id: String,
    pub to_frame: i64,
}

/// Args for `split_clips`: many splits, each with SplitClip's optional
/// explicit-id/explicit-frame (else playhead) semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitClipsArgs {
    pub splits: Vec<SplitClipItem>,
}

/// One `split_clips` item — mirrors `SplitClipArgs` exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitClipItem {
    #[serde(default)]
    pub clip_id: Option<String>,
    #[serde(default)]
    pub at_frame: Option<i64>,
}

/// Args for `ripple_delete_ranges` (Phase 17-02, D-03): cut one or more time
/// ranges on ONE track AND close the gaps, in one atomic call. Overlapping /
/// adjacent ranges are merged to disjoint before cutting; ranges must be
/// non-empty (an empty list resolves `Err`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RippleDeleteRangesArgs {
    /// Track LABEL as shown in get_timeline (`"v1"` / `"a1"`, case-insensitive).
    pub track: String,
    pub ranges: Vec<RippleRange>,
    /// Same rationale as RemoveSectionArgs::fps — no project-level fps exists
    /// until Phase 18; the caller supplies which fps the range frame numbers
    /// are expressed in.
    pub fps: f64,
}

/// One `ripple_delete_ranges` range — a half-open `[fromFrame, toFrame)`
/// TIMELINE window in the call's explicit fps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RippleRange {
    pub from_frame: i64,
    pub to_frame: i64,
}

/// Args for `set_project_settings` (Phase 18-04, COMP-01): the project's
/// canonical output timebase. Validation (finite fps in (0, 240], dims
/// 1..=7680) is `Command::SetProjectSettings`'s apply gate (T-18-02).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetProjectSettingsArgs {
    pub fps: f64,
    pub width: u32,
    pub height: u32,
}

/// Args for `set_clip_properties` (Phase 18-04, TOOL-03/D-06): a BATCH of
/// clip ids plus ONE set of optional property values applied to all of them,
/// all-or-none. Absent fields emit no command.
///
/// HISTORICAL NOTE (corrected 2026-07-30, quick task 260730-x2t): this doc
/// used to say `speed`/time-remap was "deliberately NOT a field (no engine
/// backing — Real Data Only, D-03)". That was true then and is FALSE now.
/// `speed` IS a field; it resolves to [`Command::SetClipRetime`], and the
/// engine backing is real: the per-frame source-timestamp remap at
/// `model.rs`'s single `Timeline::active_at` resolver (which both preview and
/// export feed through) plus the LGPL `atempo` audio time-stretch. Proven by
/// export in `src-tauri`'s `export_retime_is_real_on_disk_for_constant_and_ramp`
/// — a test DELETED with that shell at Phase 55 (GATE-07); the surviving
/// retime coverage is in `crates/app-core/src/export.rs`
/// (`retimed_clip_is_never_export_degenerate`,
/// `constant_speed_export_audio_reaches_its_retimed_tail`).
///
/// Speed RAMPS live on `set_keyframes` (`property: "speed"`), NOT here: one
/// curve cannot meaningfully apply to a BATCH of clips of different lengths,
/// and speed keys are INTEGRATED over time rather than sampled at a time.
///
/// The nested `transform`/`crop` values reuse the model's OWN typed
/// vocabulary verbatim (`ClipTransform`/`ClipCrop` — the same shapes the
/// agent reads back from `ClipView`), so tool args and state can never drift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetClipPropertiesArgs {
    pub clip_ids: Vec<String>,
    /// Placement transform (position = TOP-LEFT normalized [0,1], scale =
    /// normalized dims, rotation = degrees clockwise about the rect centre).
    #[serde(default)]
    pub transform: Option<crate::model::ClipTransform>,
    /// Layer opacity in [0,1] (clamped at apply).
    #[serde(default)]
    pub opacity: Option<f32>,
    /// Source-crop insets, each in [0,1] of the source dimension.
    #[serde(default)]
    pub crop: Option<crate::model::ClipCrop>,
    /// LINEAR gain multiplier (1.0 = original), the clip state's own unit —
    /// NOT decibels (the legacy `setClipVolume` tool keeps the dB vocabulary;
    /// both resolve to the identical `Command::SetClipVolume`).
    #[serde(default)]
    pub volume: Option<f32>,
    /// Edge trim with the EXISTING standalone-trim semantics (D-06).
    #[serde(default)]
    pub trim: Option<ClipTrimSpec>,
    /// Playback speed multiplier (1.0 = original, normalized to "no retime").
    /// Range `[MIN_SPEED, MAX_SPEED]`; validated at
    /// [`Command::SetClipRetime`]'s apply gate, never clamped here. Setting
    /// this REPLACES any speed ramp on the clip (quick task 260730-x2t, RT-01
    /// — a constant and a ramp are the same field on the model, so the last
    /// writer wins by construction).
    #[serde(default)]
    pub speed: Option<f32>,
}

/// One `set_clip_properties` trim spec — mirrors `TrimClipArgs` minus the id
/// (the batch's clip_ids supply those): trim `edge` to TIMELINE frame
/// `to_frame` in each target clip's OWN media fps. Standard trim semantics —
/// does NOT ripple neighbours, does NOT relink linked/detached partners.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipTrimSpec {
    pub edge: TrimEdge,
    pub to_frame: i64,
}

/// Args for `remove_tracks` (Phase 18.1, TL-06): remove one or more
/// tracks (and every clip on them) in one atomic call. All-or-none: a
/// duplicate or unknown/out-of-range label leaves the timeline unchanged.
/// Label-based track addressing: `tracks` carries LABELS as shown in
/// get_timeline (`"v1"` / `"a1"`, case-insensitive), never raw indices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveTracksArgs {
    pub tracks: Vec<String>,
}

/// Args for `add_track` (live-UAT TRACK-ADD-AGENT-GAP fix): add ONE new,
/// EMPTY track of `kind` ("video" | "audio" on the wire, via [`TrackKind`]'s
/// own snake_case serde). Deliberately NO index arg — placement is
/// [`Command::AddTrack`]'s kind-aware invariant: a video track inserts ON TOP
/// at index 0 (every existing track shifts down by one), an audio track
/// appends at the BOTTOM, preserving the video-prefix / audio-suffix layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddTrackArgs {
    pub kind: TrackKind,
}

/// Args for `set_keyframes` (Phase 19-02, COMP-04): FULL-track-replace of ONE
/// property's keyframe animation on ONE clip (D-01) — an empty `keyframes`
/// array clears that property's animation. `property` is one of
/// `"position"|"scale"|"rotation"|"opacity"|"crop"|"volume"|"speed"`; each
/// keyframe's `value` is a uniform number array whose required arity depends
/// on the property (2 for position/scale, 1 for rotation/opacity/volume/speed,
/// 4 for crop).
///
/// `"speed"` (quick task 260730-x2t) is resolved SEPARATELY from the other
/// six: it lands on `Clip.retime` via `Command::SetClipRetime`, never on
/// `Clip.keyframes` (RT-02 — speed is INTEGRATED over time, not sampled at a
/// time). Everything else about the call shape is identical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetKeyframesArgs {
    pub clip_id: String,
    pub property: String,
    pub keyframes: Vec<KeyframeArg>,
}

/// One `set_keyframes` keyframe: a clip-relative frame number in the PROJECT
/// fps timebase (O-1), the property-shaped value array, and an optional
/// interpolation for the segment FROM this key to the next — omitted =
/// `Smooth` (O-2), matching the model's own serde default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyframeArg {
    pub frame: u32,
    pub value: Vec<f64>,
    #[serde(default)]
    pub interp: Option<Interpolation>,
}

// ---------------------------------------------------------------------------
// Text tool args (Phase 20-03, TEXT-01). add_texts is BATCH create; update_text
// is a partial-merge style edit. Both apply ALL-OR-NONE (one invalid entry
// leaves the timeline byte-unchanged) via resolve_on_scratch.
// ---------------------------------------------------------------------------

/// Args for `add_texts` (Phase 20-03): create one or more text-overlay clips in
/// one atomic call. All-or-none track rule: EVERY entry sets `trackIndex` or
/// none do (mixed is rejected). Omitting `trackIndex` on all entries places them
/// on the top-most existing Video track (errors if there is no Video track).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddTextsArgs {
    pub entries: Vec<TextEntry>,
}

/// One `add_texts` entry: timing (PROJECT-fps frames), the literal text content,
/// the flat style fields (each optional — an omitted field takes the default
/// caption style), and an optional explicit placement transform. Omitting
/// `transform` leaves the clip at the auto-fit sentinel ([`TEXT_AUTOFIT_TRANSFORM`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEntry {
    /// Track LABEL as shown in get_timeline (`"v1"` / `"a1"`,
    /// case-insensitive — label-based track addressing). Present on EVERY
    /// entry or NONE (mixed rejected).
    #[serde(default)]
    pub track: Option<String>,
    /// Timeline start frame in the PROJECT fps.
    pub start_frame: i64,
    /// Timeline end frame (exclusive) in the PROJECT fps; must be > `start_frame`
    /// (a non-positive span is rejected by Command::AddText's InvalidTimes gate).
    pub end_frame: i64,
    /// The literal overlay text (non-empty, length-capped — validated at apply).
    pub content: String,
    /// Bundled font family (default Inter). Validated ∈ bundled set at apply.
    #[serde(default)]
    pub font_family: Option<String>,
    /// NORMALIZED font size = fraction of canvas HEIGHT (default 0.1 = 10%).
    #[serde(default)]
    pub font_size: Option<f32>,
    /// Fill color: hex `#RRGGBB`/`#RRGGBBAA` or `rgb(a)(...)` (default opaque white).
    #[serde(default)]
    pub fill: Option<String>,
    #[serde(default)]
    pub bold: Option<bool>,
    #[serde(default)]
    pub italic: Option<bool>,
    #[serde(default)]
    pub align: Option<TextAlign>,
    /// Normalized `[0,1]` fraction of canvas WIDTH to wrap within; omit for
    /// auto-fit natural width.
    #[serde(default)]
    pub wrap_width: Option<f32>,
    /// Explicit placement transform (position = dest-rect TOP-LEFT normalized,
    /// scale = normalized canvas fractions). Omit for auto-fit.
    #[serde(default)]
    pub transform: Option<ClipTransform>,
}

/// Args for `update_text` (Phase 20-03): partial-merge style edits onto one or
/// more existing TEXT clips by id, all-or-none. Every target must be a text clip
/// (a non-text/missing target aborts the batch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTextArgs {
    pub updates: Vec<TextUpdate>,
}

/// One `update_text` target: the clip id plus any subset of style fields to
/// overwrite (only supplied fields change) and an optional new transform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextUpdate {
    /// Target a SINGLE text clip by id (the original Phase-20 mode). Mutually
    /// exclusive with `group_id`: supply exactly one. `#[serde(default)]` (empty
    /// string) lets a group-only wire payload omit it.
    #[serde(default)]
    pub clip_id: String,
    /// Target an ENTIRE caption group by its `caption_group_id` (Phase 22-04):
    /// the patch resolves to EVERY text clip whose `caption_group_id` matches,
    /// applying the SAME TextStylePatch to all (bulk restyle). A group id
    /// matching zero clips is a hard reject (all-or-none), never a silent no-op.
    /// Omit for the single-clip `clip_id` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default)]
    pub font_family: Option<String>,
    #[serde(default)]
    pub font_size: Option<f32>,
    /// Fill color: hex `#RRGGBB`/`#RRGGBBAA` or `rgb(a)(...)`.
    #[serde(default)]
    pub fill: Option<String>,
    #[serde(default)]
    pub bold: Option<bool>,
    #[serde(default)]
    pub italic: Option<bool>,
    #[serde(default)]
    pub align: Option<TextAlign>,
    /// Normalized `[0,1]` fraction of canvas WIDTH to wrap within.
    #[serde(default)]
    pub wrap_width: Option<f32>,
    /// New explicit placement transform (pins the clip so content edits do not
    /// re-fit). Omit to leave placement unchanged.
    #[serde(default)]
    pub transform: Option<ClipTransform>,
}

/// Args for `add_captions` (Phase 22-04, TEXT-04): mint one or more ORDINARY
/// text-overlay clips in one atomic call, each tagged with a shared
/// `caption_group_id` so the group can be bulk-restyled later. The entry shape
/// is [`TextEntry`] VERBATIM (same timing/content/style fields as `add_texts`) —
/// a caption IS an ordinary text clip. All-or-none track rule matches add_texts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCaptionsArgs {
    /// Optional caller-supplied group id shared by every caption in this batch.
    /// Omit (or pass empty) to have a deterministic `caption~grp~{n}` minted.
    #[serde(default)]
    pub group_id: Option<String>,
    /// The caption entries — one text clip minted per entry, all sharing the
    /// group id and (per-entry) style. At least one entry.
    pub entries: Vec<TextEntry>,
}

// ---------------------------------------------------------------------------
// Transcript word-cut args (Phase 22-03, TEXT-03). remove_words takes
// agent-supplied, PRE-RESOLVED SOURCE-media word spans (microseconds) — NOT
// frames+fps (transcript timestamps are continuous time, no fps ambiguity) and
// NOT timeline positions (resolve() re-derives the timeline mapping from the
// clip's CURRENT window). All-or-none: one out-of-window span leaves the
// timeline byte-unchanged.
// ---------------------------------------------------------------------------

/// Args for `remove_words` (Phase 22-03): delete one or more SOURCE-media word
/// spans from ONE clip and close the gaps (ripple), all-or-none. Each span is a
/// half-open `[fromUs, toUs)` window of the ORIGINAL media (from a prior
/// `get_transcript`); a span now outside the clip's trimmed source range rejects
/// the WHOLE call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveWordsArgs {
    pub clip_id: String,
    pub ranges: Vec<SourceRange>,
}

/// One `remove_words` span — a half-open `[fromUs, toUs)` window in MICROSECONDS
/// of the clip's SOURCE media (not timeline position). `toUs` must be > `fromUs`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRange {
    pub from_us: i64,
    pub to_us: i64,
}

// ---------------------------------------------------------------------------
// Layout args (Phase 23, COMP-03). apply_layout is a NAMED-TEMPLATE tool
// (fixed catalog + named slots), structurally distinct from
// set_clip_properties's raw per-clip batch. Re-layout-existing-clips-only:
// every assignment's clip_id must reference a clip ALREADY on the timeline.
// ---------------------------------------------------------------------------

/// Args for `apply_layout` (Phase 23, COMP-03): arrange 2+ already-placed
/// clips into a NAMED layout template (`side_by_side`, `grid_2x2`,
/// `pip_bottom_right`, ...) in one atomic call. Partial slot assignment is
/// allowed (an unmapped slot is left untouched). "Cover" fit (crop, no
/// letterbox bars) is the ONLY v1 behavior -- there is no `fit_mode` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyLayoutArgs {
    pub template: String,
    pub assignments: Vec<LayoutSlotAssignment>,
}

/// One `apply_layout` slot assignment: an EXISTING clip id filling one
/// named slot of the call's `template`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutSlotAssignment {
    pub slot: String,
    pub clip_id: String,
}

/// Args for `organize_media` (Phase 25, LIB-01): one atomic batch of
/// folder/media create/move/rename/delete operations. Validated whole
/// (path sanitization + cycle detection) BEFORE any live mutation; resolved
/// in a FIXED create -> move/rename -> delete phase order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizeMediaArgs {
    pub operations: Vec<OrganizeMediaOp>,
}

/// One organize_media batch entry, discriminated by `op` (wire values are
/// snake_case: "create_folder"/"move_folder"/"rename_folder"/"move_media"/
/// "rename_media"/"delete_folder"/"delete_media"). `rename_all_fields`
/// (Phase 24 precedent, crates/core/src/scene_spec.rs) carries camelCase
/// into every variant's fields -- `media_id` serializes as `mediaId`,
/// matching every other batch tool's per-item field convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum OrganizeMediaOp {
    CreateFolder { path: String },
    MoveFolder { from: String, to: String },
    /// `name` is a SINGLE path segment (no "/") -- the parent stays
    /// unchanged; use MoveFolder to change the parent.
    RenameFolder { path: String, name: String },
    MoveMedia { media_id: String, folder: String },
    /// `name: None` (omitted or explicit `null`) clears the display-name
    /// override, reverting to basename(path).
    RenameMedia { media_id: String, #[serde(default)] name: Option<String> },
    /// Cascades: removes every contained sub-folder and media item too.
    DeleteFolder { path: String },
    DeleteMedia { media_id: String },
}

#[cfg(test)]
mod track_label_tests {
    use super::{resolve_track_label, track_label, ToolError};
    use crate::model::{Timeline, Track, TrackKind};

    /// Build a timeline from a top->bottom kind list ('v' = video, 'a' = audio).
    fn timeline(kinds: &str) -> Timeline {
        Timeline {
            tracks: kinds
                .chars()
                .map(|c| Track {
                    kind: if c == 'v' { TrackKind::Video } else { TrackKind::Audio },
                    clips: Vec::new(),
                })
                .collect(),
        }
    }

    /// The frontend gutter formula (frontend/src/main.ts), transliterated —
    /// the convention's independent source of truth for the tests below.
    fn frontend_labels(tl: &Timeline) -> Vec<String> {
        let total_video = tl.tracks.iter().filter(|t| t.kind == TrackKind::Video).count();
        let (mut v, mut a) = (0usize, 0usize);
        tl.tracks
            .iter()
            .map(|t| match t.kind {
                TrackKind::Video => {
                    v += 1;
                    format!("v{}", total_video - v + 1)
                }
                TrackKind::Audio => {
                    a += 1;
                    format!("a{a}")
                }
            })
            .collect()
    }

    #[test]
    fn worked_example_matches_the_task_spec() {
        // tracks = [Video(idx0), Video(idx1), Audio(idx2), Audio(idx3)]
        // -> labels idx0="v2", idx1="v1", idx2="a1", idx3="a2".
        let tl = timeline("vvaa");
        let labels: Vec<String> = (0..4).map(|i| track_label(&tl, i).unwrap()).collect();
        assert_eq!(labels, vec!["v2", "v1", "a1", "a2"]);
    }

    #[test]
    fn labels_match_the_frontend_formula_for_various_layouts() {
        for kinds in ["v", "a", "va", "vva", "vvaa", "vvvaa", "avvaav", "aaa", "vvvvv"] {
            let tl = timeline(kinds);
            let expected = frontend_labels(&tl);
            let actual: Vec<String> = (0..tl.tracks.len())
                .map(|i| track_label(&tl, i).unwrap())
                .collect();
            assert_eq!(actual, expected, "layout `{kinds}`: video bottom-up, audio top-down");
        }
    }

    #[test]
    fn resolve_is_the_exact_inverse_of_track_label() {
        for kinds in ["v", "a", "va", "vva", "vvaa", "vvvaa", "avvaav"] {
            let tl = timeline(kinds);
            for i in 0..tl.tracks.len() {
                let label = track_label(&tl, i).unwrap();
                assert_eq!(
                    resolve_track_label(&tl, &label).unwrap(),
                    i,
                    "layout `{kinds}` index {i} label `{label}` must round-trip"
                );
            }
        }
    }

    #[test]
    fn resolution_is_case_insensitive_and_trims_whitespace() {
        let tl = timeline("vvaa");
        assert_eq!(resolve_track_label(&tl, "V1").unwrap(), 1);
        assert_eq!(resolve_track_label(&tl, "V2").unwrap(), 0);
        assert_eq!(resolve_track_label(&tl, "A2").unwrap(), 3);
        assert_eq!(resolve_track_label(&tl, " v1 ").unwrap(), 1);
    }

    #[test]
    fn new_video_track_on_top_gets_the_highest_number_and_v1_stays_anchored() {
        // [v, a] -> v1 at index 0. AddTrack{Video} inserts at index 0:
        // [v, v, a] -> the ORIGINAL video track (now index 1) is STILL v1, and
        // the new top track is v2.
        let before = timeline("va");
        assert_eq!(track_label(&before, 0).unwrap(), "v1");
        let after = timeline("vva");
        assert_eq!(track_label(&after, 1).unwrap(), "v1", "the anchor lane keeps v1");
        assert_eq!(track_label(&after, 0).unwrap(), "v2", "the new top track takes v2");
    }

    #[test]
    fn malformed_and_out_of_range_labels_are_unknown_track_label() {
        let tl = timeline("vvaa");
        for bad in [
            "", "v", "a", "v0", "a0", "v3", "a3", "v9", "x1", "1", "video1", "v-1", "v1.5",
            "v 1", "1v", "t0", "vv1", "va", "v١", // non-ASCII digit
        ] {
            match resolve_track_label(&tl, bad) {
                Err(ToolError::UnknownTrackLabel { label }) => assert_eq!(label, bad),
                other => panic!("label `{bad}` must be UnknownTrackLabel, got {other:?}"),
            }
        }
    }

    #[test]
    fn out_of_range_index_has_no_label() {
        let tl = timeline("va");
        assert_eq!(track_label(&tl, 2), None);
        assert_eq!(track_label(&timeline(""), 0), None);
    }
}

#[cfg(test)]
mod parse_fill_tests {
    use super::parse_fill;

    #[test]
    fn valid_colors_still_parse_byte_for_byte() {
        assert_eq!(parse_fill("#FF5533").unwrap(), [0xFF, 0x55, 0x33, 255]);
        assert_eq!(parse_fill("#ff5533ff").unwrap(), [0xFF, 0x55, 0x33, 255]);
        assert_eq!(parse_fill("#10182080").unwrap(), [0x10, 0x18, 0x20, 0x80]);
        assert_eq!(parse_fill("  #000000  ").unwrap(), [0, 0, 0, 255]);
        assert_eq!(parse_fill("rgb(255, 85, 51)").unwrap(), [255, 85, 51, 255]);
        assert_eq!(parse_fill("rgba(0,0,0,0.5)").unwrap(), [0, 0, 0, 128]);
        assert_eq!(parse_fill("rgba(1,2,3,200)").unwrap(), [1, 2, 3, 200]);
    }

    // CR-01: a multi-byte-UTF-8 hex string whose BYTE length is 6/8 must return
    // Err, never panic on a non-char-boundary slice.
    #[test]
    fn multibyte_hex_returns_err_not_panic() {
        // 'é' = 2 UTF-8 bytes => "1é234" is 5 chars but 6 bytes (6-digit branch).
        assert!(parse_fill("#1é234").is_err());
        // 8-byte multibyte payload (two 'é').
        assert!(parse_fill("#éé2345").is_err());
        // Non-ASCII with the '#' prefix but odd byte layout.
        assert!(parse_fill("#straße").is_err());
        // Non-ASCII digits (fullwidth) that are multi-byte.
        assert!(parse_fill("#１２３４５６").is_err());
    }

    #[test]
    fn malformed_ascii_hex_returns_err() {
        assert!(parse_fill("#12345").is_err()); // too short (5)
        assert!(parse_fill("#123456789").is_err()); // too long (9)
        assert!(parse_fill("#GGGGGG").is_err()); // non-hex digits
        assert!(parse_fill("not-a-color").is_err());
        assert!(parse_fill("#").is_err());
        assert!(parse_fill("").is_err());
    }
}
