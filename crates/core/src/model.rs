//! Domain model types (OpenTimelineIO-flavored, serde-serializable).
//!
//! All times are `i64` MICROSECONDS. // seam: frame-accurate boundary math
//! (rational timebases, snapping edits to frame boundaries) is Phase 6's
//! concern — Phase 2 only plumbs times through; do not add rounding here.

use serde::{Deserialize, Serialize};

use crate::canvas::CanvasState;

/// The whole editable document. This is the single source of truth, owned by
/// the Rust backend; the renderer's mirror is rebuilt from serialized
/// snapshots of this type.
///
/// (`Eq` dropped in Phase 3: `MediaBinItem::fps` is an `f64`. Tests compare
/// with `assert_eq!`, which only needs `PartialEq`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub media_bin: Vec<MediaBinItem>,
    pub timeline: Timeline,
    /// Transport/preview state (Phase 4). Lives on the snapshot so the
    /// renderer mirrors it, but it is SESSION state, not document state:
    /// transport ops go through [`crate::Store::transport`], never through
    /// the undoable command path (`#[serde(default)]` keeps old snapshots
    /// loadable).
    /// PROGRAM/Timeline playback (the edited result at the playhead). The
    /// Timeline ruler + red-line playhead track THIS.
    #[serde(default)]
    pub playback: Playback,
    /// SOURCE playback: previewing a raw MediaBin clip (the "Source monitor"),
    /// INDEPENDENT of the timeline playhead. Double-clicking a MediaBin clip
    /// loads it here and switches `preview_mode` to Source, so the Timeline
    /// red line does not move for a source preview.
    #[serde(default)]
    pub source_playback: Playback,
    /// Which monitor the Preview region (and native surface) currently shows.
    #[serde(default)]
    pub preview_mode: PreviewMode,
    /// Canvas annotations (Phase 13, CANV-01) — real, inspectable,
    /// serde-serializable sketch data, mutated ONLY via the undoable
    /// annotation `Command`s. `#[serde(default)]` keeps every pre-Phase-13
    /// snapshot/fixture (none of which has a `"canvas"` key) loadable
    /// unchanged — the exact pattern `playback`/`source_playback`/
    /// `preview_mode` already use.
    #[serde(default)]
    pub canvas: CanvasState,
    /// Project timebase (Phase 18, COMP-01): the ONE authoritative output
    /// frame rate the compositor/export render against. FLAT fields (not a
    /// nested `ProjectSettings` struct) — mirrors `MediaBinItem`'s
    /// fps/width/height vocabulary and keeps `SetProjectSettings` a plain
    /// three-field command. `#[serde(default)]` keeps every pre-Phase-18
    /// snapshot (no `"fps"` key) loadable at the 1920x1080@30 default.
    /// Mutated ONLY via the undoable `Command::SetProjectSettings`
    /// (validated: finite, in (0, 240] — 18-REVIEW MED-01 ceiling).
    #[serde(default = "default_project_fps")]
    pub fps: f64,
    /// Project output width in pixels (bounded 1..=7680 at apply).
    #[serde(default = "default_project_width")]
    pub width: u32,
    /// Project output height in pixels (bounded 1..=7680 at apply).
    #[serde(default = "default_project_height")]
    pub height: u32,
    /// Phase 25 (LIB-01): the set of EXISTING non-root virtual folder paths.
    /// Root ("") is implicit and always exists -- never in this Vec, never
    /// created/renamed/deleted. An empty folder is representable because its
    /// path lives HERE even when no MediaBinItem references it (no
    /// parent/child node-tree -- hierarchy is entirely implicit in
    /// path-string prefixes: children of "a" = every entry equal to "a" or
    /// starting with "a/"). #[serde(default)] keeps every pre-Phase-25
    /// snapshot (no "media_folders" key) loadable at the empty-vec default.
    #[serde(default)]
    pub media_folders: Vec<String>,
    /// Phase 26 (LIB-02): the project's own display name / filesystem identity.
    /// Written into the `.rud` file the host persists this project as, and
    /// read back by `get_projects`'s directory scan. Empty string ("") for any
    /// project that predates Phase 26 or was never explicitly named.
    /// #[serde(default)] keeps every pre-Phase-26 snapshot (no "name" key)
    /// loadable at the empty-string default -- crates/core stays zero-I/O; ALL
    /// filesystem/`.rud` work lives in `crates/app-core`
    /// (`project_store.rs`; offline_guard.rs pin).
    #[serde(default)]
    pub name: String,
}

// ---------------------------------------------------------------------------
// Phase 25 (LIB-01, SC-2): the ONE authoritative virtual-folder-path reject
// list. Lives HERE (a leaf module both `tools` and `command` depend on) so a
// SINGLE definition guards BOTH the Tool layer (`Tool::OrganizeMedia::resolve`,
// fail-fast) AND the authoritative Command layer (`Command::apply`, reachable
// directly via `dispatch_command` IPC) — no duplicated/driftable copy. See
// 25-REVIEW CR-01 / WR-02 / WR-03.
// ---------------------------------------------------------------------------

/// Max bytes for a SINGLE path segment / rename name. Mirrors
/// `MAX_MEDIA_NAME_LEN` (255) — the same DoS/UX cap already applied to every
/// other agent-supplied name string in this crate (25-REVIEW WR-02).
pub(crate) const MAX_FOLDER_SEGMENT_LEN: usize = 255;

/// Max bytes for a WHOLE virtual folder path (all `/`-joined segments). A
/// generous cap (well above any sane nesting) that still bounds the string
/// cloned into every snapshot/undo-entry/IPC delta, closing the multi-megabyte
/// path DoS class (25-REVIEW WR-02).
pub(crate) const MAX_FOLDER_PATH_LEN: usize = 2048;

/// A single virtual path SEGMENT is safe iff: non-empty, within the length cap
/// (WR-02), not "."/".." (parent-escape), no path separator ("/" or "\\"), no
/// colon `:` in ANY position (Windows drive-letter `C:` AND NTFS
/// alternate-data-stream `name:stream` injection — WR-03: checked per-segment,
/// which subsumes the old whole-path first-two-char drive check), and no
/// control character. Shared by the full-path validator (each "/"-split
/// segment) and rename_folder's single-segment `name` field (a rename target
/// is ALWAYS one segment — it can never introduce a "/", which would silently
/// become a multi-level move).
pub(crate) fn is_safe_folder_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_FOLDER_SEGMENT_LEN
        && segment != "."
        && segment != ".."
        && !segment.contains('/')
        && !segment.contains('\\')
        && !segment.contains(':')
        && !segment.chars().any(|c| c.is_control())
}

/// Validate a virtual library folder PATH (Phase 25, LIB-01, SC-2): "/"-joined
/// segments, each independently [`is_safe_folder_segment`], with NO
/// leading/trailing slash, no leading backslash (UNC/absolute), and within the
/// whole-path length cap — a VIRTUAL in-memory path (never touches disk;
/// `MediaBinItem.path` remains the only real filesystem field, untouched by
/// this phase). `allow_root` permits the empty string (legal ONLY for
/// move_media's `folder` field addressing the library root). Returns `true`
/// when the path is acceptable. Each layer wraps a `false` in its OWN error
/// type (`ToolError::InvalidFolderPath` / `CoreError::InvalidFolderPath`).
///
/// `pub` (quick task 260726-hgu, threat T-hgu-01): the host's recursive
/// FOLDER import walks REAL disk directories and turns their names into
/// candidate virtual paths. It PRE-VALIDATES every candidate with THIS
/// function so an unmappable disk name is skipped-with-a-log at the walk,
/// never reaching a `Command` mid-walk (a rejected dispatch inside an open
/// turn would leave a half-mirrored tree). The Command layer still re-validates
/// on apply — the promotion adds a caller, never a bypass.
pub fn is_valid_folder_path(path: &str, allow_root: bool) -> bool {
    if path.is_empty() {
        return allow_root;
    }
    if path.len() > MAX_FOLDER_PATH_LEN {
        return false;
    }
    if path.starts_with('/') || path.starts_with('\\') || path.ends_with('/') {
        return false;
    }
    path.split('/').all(is_safe_folder_segment)
}

/// Serde defaults for the Phase-18 project timebase — pre-Phase-18 snapshots
/// deserialize as 1920x1080@30 (the v1 de-facto output shape).
fn default_project_fps() -> f64 {
    30.0
}
fn default_project_width() -> u32 {
    1920
}
fn default_project_height() -> u32 {
    1080
}

/// Which preview monitor is active — Source (a raw MediaBin clip) or Program
/// (the edited timeline at the playhead).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PreviewMode {
    /// Editing/timeline result (default).
    #[default]
    Program,
    /// A raw MediaBin clip preview.
    Source,
}

impl Project {
    /// A new empty project with the default track layout: one video track and
    /// one audio track. Later phases add track management commands.
    pub fn new() -> Self {
        Self {
            media_bin: Vec::new(),
            timeline: Timeline {
                tracks: vec![
                    Track {
                        kind: TrackKind::Video,
                        clips: Vec::new(),
                    },
                    Track {
                        kind: TrackKind::Audio,
                        clips: Vec::new(),
                    },
                ],
            },
            playback: Playback::default(),
            source_playback: Playback::default(),
            preview_mode: PreviewMode::default(),
            canvas: CanvasState::default(),
            fps: default_project_fps(),
            width: default_project_width(),
            height: default_project_height(),
            media_folders: Vec::new(),
            name: String::new(),
        }
    }

    /// Re-establish every clip's retime invariant after DESERIALIZATION
    /// (quick task 260730-x2t, CR-01 / threats T-x2t-01/02/08).
    ///
    /// [`Retime::timeline_len_us`] is DERIVED state and the curve is UNTRUSTED:
    /// a `.rud` is a plain user-editable JSON file that can also be shared or
    /// downloaded, so the LOAD boundary must apply exactly the rules
    /// `Command::SetClipRetime`/`AddClip` apply. Without this, four guarantees
    /// the command layer establishes are simply absent for anything off disk:
    ///
    /// 1. a stored occupancy that DISAGREES with the curve misplaces the clip's
    ///    own end and every downstream overlap/snap/export length;
    /// 2. `timeline_len_us: i64::MAX` overflows `Clip::timeline_end_us`;
    /// 3. the [`MAX_RETIME_KEYS`] cap — which exists because
    ///    [`retime_source_offset`] walks the key array on the PER-OUTPUT-FRAME
    ///    hot path — is bypassed entirely;
    /// 4. `speed <= 0` / non-finite breaks the strict monotonicity that BOTH
    ///    bisection inverses ([`retimed_timeline_len_us`] and the preview's
    ///    `pts_map`) assume, so they return arbitrary answers rather than
    ///    erroring.
    ///
    /// A clip whose curve fails validation LOSES its retime (plays at 1:1)
    /// rather than poisoning the hot path — a load must not hard-fail on one
    /// bad clip, and refusing to open the project would strand the user's other
    /// work.
    ///
    /// Routed through the SAME [`sanitized_retime`] every command carrier uses,
    /// so the two boundaries cannot drift.
    pub fn sanitize_retime_after_load(&mut self) {
        let fps = if self.fps.is_finite() && self.fps > 0.0 && self.fps <= MAX_TIMEBASE_FPS {
            self.fps
        } else {
            default_project_fps()
        };
        for track in &mut self.timeline.tracks {
            for clip in &mut track.clips {
                if clip.retime.is_none() {
                    continue; // un-retimed: byte-for-byte the pre-retime load
                }
                let span = clip.out_us.saturating_sub(clip.in_us);
                clip.retime = sanitized_retime(clip.retime.as_ref(), fps, span).unwrap_or(None);
            }
        }
    }
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

/// (`Eq` dropped in Phase 6: clips carry an `f32` volume.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Timeline {
    pub tracks: Vec<Track>,
}

/// Resolution of "what plays at timeline position T on a track" (Phase 5).
/// This is the seed of the composite path: the timeline preview resolves the
/// top video track's hit and decodes `media_id` at `source_us`; Phase 7
/// export iterates the same resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipHit {
    /// `Clip::id` of the active clip.
    pub clip_id: String,
    /// `MediaBinItem::id` the clip references.
    pub media_id: String,
    /// Position within the SOURCE media = `in_us + (position_us - start_us)`.
    /// Always in `[in_us, out_us)` by construction.
    pub source_us: i64,
}

impl Timeline {
    /// The clip active on track `track_idx` at `position_us`, with the
    /// resolved source position. A clip is active iff
    /// `start_us <= position_us < timeline_end_us()` — start INCLUSIVE, end
    /// EXCLUSIVE, so adjacent clips never both claim their shared boundary.
    ///
    /// M1 assumes NON-OVERLAPPING clips per track. If clips do overlap (raw
    /// `AddClip` does not forbid it), the LAST clip in track order wins —
    /// i.e. the most recently added / topmost one. Deterministic, matches
    /// the "later clips win, no crossfades" M1 decision.
    pub fn active_at(&self, track_idx: usize, position_us: i64) -> Option<ClipHit> {
        let track = self.tracks.get(track_idx)?;
        track
            .clips
            .iter()
            .rev() // last-in-order wins on overlap
            .find(|c| c.start_us <= position_us && position_us < c.timeline_end_us())
            .map(|c| ClipHit {
                clip_id: c.id.clone(),
                media_id: c.media_id.clone(),
                // THE timeline->source map (quick task 260730-x2t, RT-04).
                // BOTH the export composite loop and the preview present loop
                // resolve here; `Clip::source_offset_at` is the identity for
                // an un-retimed clip, so this line is byte-unchanged for every
                // pre-retime project. Implementing the remap a SECOND time
                // anywhere else (a `setpts` filter in one of the four decoder
                // invocations, a decoder-side rescale) forks the one composite
                // path CONTEXT.md locks — add callers of this function, never
                // twins of it.
                source_us: c.in_us + c.source_offset_at(position_us - c.start_us),
            })
    }

    /// The active hit on the TOP video track at `position_us` — what the
    /// timeline preview shows. Video tracks are scanned in `tracks` order
    /// and the FIRST one with an active clip wins (track order = top-to-
    /// bottom priority; M1 has a single video track, so this is simply "the
    /// video track's active clip").
    pub fn top_video_active_at(&self, position_us: i64) -> Option<ClipHit> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == TrackKind::Video)
            .find_map(|(idx, _)| self.active_at(idx, position_us))
    }

    /// Every ACTIVE video layer at `position_us`, STACK-ORDERED: index 0 is
    /// the TOP-MOST layer (Phase 18, COMP-02). Video tracks are scanned in
    /// `tracks` order — the same top-to-bottom priority
    /// [`top_video_active_at`](Self::top_video_active_at) uses, whose winner
    /// is always element 0 here (when any layer is active). Audio tracks and
    /// video tracks with no clip covering `position_us` contribute nothing.
    ///
    /// ORDERING CONTRACT (Plan 18-02 canonicalization): this is exactly the
    /// order the engine compositor expects — its `layers` slice is
    /// TRACK-ORDERED with index-0 = top, painted LAST (back-to-front via
    /// reverse iteration inside `encode_layers_pass`). Hand this Vec to the
    /// compositor directly; do NOT pre-reverse it caller-side.
    pub fn active_layers_at(&self, position_us: i64) -> Vec<ClipHit> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == TrackKind::Video)
            .filter_map(|(idx, _)| self.active_at(idx, position_us))
            .collect()
    }

    /// Exclusive end of the LAST clip across all tracks, in microseconds —
    /// the timeline's playable extent. 0 for an empty timeline. Covers
    /// clips on EVERY track (video and audio) regardless of rearrangement —
    /// this is a `max()` over all clips' ends, not just the video track's,
    /// so trims/splits/moves/detach on any track are reflected.
    pub fn duration_us(&self) -> i64 {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(|c| c.timeline_end_us())
            .max()
            .unwrap_or(0)
    }

    /// Every clip that contributes AUDIO to the composite mix (Phase 7
    /// export): every clip on an Audio track, PLUS every clip on a Video
    /// track whose audio is NOT detached (`audio_detached == false`) — a
    /// video clip that HAS been detached contributes silence itself; its
    /// original loudness lives on the separate audio-track clip `DetachAudio`
    /// created, which is already covered by the Audio-track half of this
    /// scan. Order is NOT significant (contributors are summed, not
    /// layered); each carries exactly the data `render_audio_pcm` needs plus
    /// `start_us` to place it in the full-duration mix buffer.
    pub fn audio_contributors(&self) -> Vec<AudioContributor> {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(move |c| (t.kind, c)))
            // Phase 20 (T-20-05 / Pitfall 4): a TEXT clip lives on a Video
            // track with media_id="" and contributes NO audio — exclude it from
            // the mix so the audio renderer is never handed the empty sentinel.
            .filter(|(kind, c)| {
                (*kind == TrackKind::Audio || !c.audio_detached) && c.text.is_none()
            })
            .map(|(_, c)| AudioContributor {
                clip_id: c.id.clone(),
                media_id: c.media_id.clone(),
                start_us: c.start_us,
                in_us: c.in_us,
                out_us: c.out_us,
                volume: c.volume,
                volume_keyframes: c.keyframes.volume.clone(),
                retime: c.retime.clone(),
            })
            .collect()
    }
}

/// One clip's contribution to the composited export audio mix (Phase 7):
/// render `media_id`'s audio over `[in_us, out_us)` at `volume`, then add it
/// into the full-duration buffer starting at `start_us`. A twin of
/// [`ClipHit`] for audio (which resolves ONE active clip at a point; this
/// enumerates ALL contributing clips across the whole timeline for mixing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioContributor {
    /// `Clip::id` this contribution comes from (traceability/debugging).
    pub clip_id: String,
    /// `MediaBinItem::id` to render audio from.
    pub media_id: String,
    /// Timeline position (microseconds) where this contribution begins.
    pub start_us: i64,
    /// Source in-point (microseconds).
    pub in_us: i64,
    /// Source out-point (microseconds, exclusive).
    pub out_us: i64,
    /// Audio gain multiplier (>= 0; 1.0 = unity), carried from the clip.
    pub volume: f32,
    /// The clip's volume keyframe track (Phase 19, COMP-04), copied verbatim.
    /// EMPTY for an un-animated clip — the export mix then takes the static
    /// `volume` path BYTE-UNCHANGED from pre-19. NON-EMPTY OVERRIDES the static
    /// gain with a per-sample envelope sampled by [`sample_scalar_track`] (D-06,
    /// exactly as [`Clip::sample_at`] overrides the visual static fields).
    /// `#[serde(default)]` so any pre-19 serialized form still deserializes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_keyframes: Vec<Keyframe<f32>>,
    /// The clip's time remap, copied verbatim (quick task 260730-x2t). `None`
    /// for an un-retimed contributor, whose [`retime_audio_windows`] collapses
    /// to exactly ONE window at tempo 1.0 — the pre-retime path, byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retime: Option<Retime>,
}

// ---------------------------------------------------------------------------
// Quick task 260730-x2t (RT-05): audio-side segmentation. Speed is CONSTANT
// within a window, so the timeline<->source map is LINEAR there and the engine
// never needs the integral — it just gets a tempo and an exact output length.
// ---------------------------------------------------------------------------

/// Ceiling on a single constant-tempo audio window for a RAMP, in µs. Speed is
/// held constant WITHIN a window, so a ramp is a staircase approximation of the
/// curve and the error scales with this value; the cost is one ffmpeg spawn per
/// window (the same per-spawn economics the video path fights). 100 ms is ~3
/// output frames at 30 fps — inaudible for any realistic ramp — and it is
/// deliberately `<=` the live producer's `CHUNK_US` (2 s, `engine/audio.rs`) so
/// the two granularities COMPOSE instead of interacting by accident: within one
/// constant-tempo window the timeline↔source map is LINEAR, so the engine's 2 s
/// chunker can slice it freely without re-deriving the integral.
///
/// A `Constant` curve is NOT staircased at all — it is already linear over the
/// whole clip, so it yields exactly ONE window (see [`retime_audio_windows`]).
pub const RETIME_AUDIO_WINDOW_US: i64 = 100_000;

/// The staircase window the LIVE PREVIEW mixer uses, in µs — deliberately
/// COARSER than the export's [`RETIME_AUDIO_WINDOW_US`].
///
/// # Why preview needs its own number (live-UAT bug
/// `retime-live-uat-frontend-mirror-undo-audio`)
///
/// Export is OFFLINE: it can afford one ffmpeg spawn per window and take as
/// long as it takes (the ramp path is pinned at 2.93 fps and that is accepted).
/// The live mixer cannot. `engine::AudioOutput::start_mix`'s producer must
/// deliver `CHUNK_US` (2 s) of audio per 2 s of WALL CLOCK, and it calls
/// `render_audio_pcm_retimed` once per window overlapping the chunk. MEASURED
/// on the owner's machine against the bundled LGPL binary and
/// `test-media/bars_720p30_5s.mp4`: **~190 ms per window** (ffprobe ~110-130 ms
/// + the atempo render ~90-120 ms). At 100 ms windows that is 20 windows =
/// ~3.8 s of work for 2 s of audio — a real-time factor of ~1.9, i.e. the
/// producer can NEVER keep up. Live consequence, exactly as reported: the
/// bounded channel starves, the cpal callback emits silence (audio STUTTER),
/// and because `samples_consumed` keeps ticking through underrun silence while
/// `preview_target_us` paces video off it, the picture stutters with it — plus
/// a ~3.8 s frozen-on-frame-0 stall before the FIRST chunk ever lands, since
/// the audio clock does not start until real samples flow.
///
/// At 400 ms a 2 s chunk touches at most 6 windows (`ceil(2000/400) + 1` — the
/// `+1` because the chunk grid starts at the PLAYHEAD, not at a window
/// boundary). With the mixer's redundant per-window probe hoisted out of the
/// loop (`engine::audio`), that is ~6 x 110 ms + one ~130 ms probe = ~0.79 s of
/// work for 2 s of audio: a real-time factor of ~0.40, i.e. ~2.5x of headroom,
/// and still under 1.0 with two simultaneously ramped contributors.
///
/// # What the coarser staircase costs, bounded and MEASURED
///
/// NOTHING drifts: every window's `in_us`/`out_us` come from the exact speed
/// INTEGRAL at its two ends, so the source position is EXACT at every window
/// boundary regardless of the window size — a bigger window cannot accumulate
/// error. The only difference is INSIDE a window, where the tempo is held at
/// the window's average instead of varying: the deviation from the exact map
/// peaks mid-window at about `W² · |d(speed)/dt| / 8`, i.e. it falls off with
/// the SQUARE of the step.
///
/// Measured against the exact integral (not the estimate above) by
/// `crates/core/tests/retime.rs::the_preview_staircase_never_moves_audio_a_
/// perceptible_amount_against_the_picture`: **8.4 ms** on the owner's
/// 1.0 -> 0.4 -> 1.0 ramp and **15.4 ms** on a deliberately steeper smooth
/// 2.0 -> 0.5 sweep — both inside HALF a 30 fps frame step (16.7 ms) and
/// ~3-5x under the ~40 ms threshold at which an A/V offset becomes
/// perceptible. 500 ms was tried first and MEASURED 11.9 ms / 23.9 ms, which
/// breaks the half-frame bound on the steep sweep; that is why the constant is
/// 400 ms and not the rounder number.
///
/// This is NOT a second implementation: preview and export both go through the
/// same [`retime_audio_windows_with`], the same integral, the same tempo
/// derivation and the same truncation rule. Only the staircase step differs.
pub const PREVIEW_RETIME_AUDIO_WINDOW_US: i64 = 400_000;

/// One constant-tempo audio window of a (possibly retimed) contributor.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioWindow {
    /// Timeline position where this window's audio begins.
    pub timeline_start_us: i64,
    /// The EXACT timeline length this window must occupy — and therefore the
    /// exact expected sample count. `atempo`'s output length is only
    /// APPROXIMATE (measured +0.12% at 2.0x, −0.36% at 0.25x), so THIS — never
    /// the returned PCM length — is what placement and truncation use (RT-05).
    pub timeline_len_us: i64,
    /// Source in-point for this window.
    pub in_us: i64,
    /// Source out-point (exclusive) for this window.
    pub out_us: i64,
    /// `(out_us - in_us) / timeline_len_us`. Exactly 1.0 for an un-retimed
    /// contributor, whose windows collapse to a SINGLE window — the pre-retime
    /// path, byte-identical.
    pub tempo: f32,
}

/// Split a contributor into constant-tempo audio windows via the speed
/// INTEGRAL. An un-retimed contributor yields exactly ONE window with
/// `tempo: 1.0` and the contributor's own `[in_us, out_us)` — i.e. the exact
/// call the pre-retime code made.
///
/// A `Constant` curve also yields ONE window (its map is linear across the
/// whole clip, so staircasing it would only add ffmpeg spawns for nothing).
/// A `Ramp` is staircased at [`RETIME_AUDIO_WINDOW_US`].
///
/// The EXPORT granularity. The live preview mixer calls
/// [`retime_audio_windows_with`] with [`PREVIEW_RETIME_AUDIO_WINDOW_US`] — same
/// segmenter, same integral, coarser step; see that constant for why and for
/// the bound on the difference.
pub fn retime_audio_windows(c: &AudioContributor) -> Vec<AudioWindow> {
    retime_audio_windows_with(c, RETIME_AUDIO_WINDOW_US)
}

/// [`retime_audio_windows`] with an explicit staircase step.
///
/// ONE segmenter, two granularities: export passes
/// [`RETIME_AUDIO_WINDOW_US`], the live preview mixer passes
/// [`PREVIEW_RETIME_AUDIO_WINDOW_US`]. Everything else — the integral at both
/// window ends, the derived tempo, the clamp, the IN-01 absorb rule, the
/// single-window `Constant`/un-retimed collapse — is shared, so the two can
/// never fork.
///
/// `window_us` is clamped to `>= 1`; a non-positive step would loop forever.
pub fn retime_audio_windows_with(c: &AudioContributor, window_us: i64) -> Vec<AudioWindow> {
    let window_us = window_us.max(1);
    let span = c.out_us - c.in_us;
    let one = |tempo: f32, len: i64| {
        vec![AudioWindow {
            timeline_start_us: c.start_us,
            timeline_len_us: len,
            in_us: c.in_us,
            out_us: c.out_us,
            tempo,
        }]
    };
    let Some(r) = c.retime.as_ref() else {
        return one(1.0, span);
    };
    let total = r.timeline_len_us;
    if span <= 0 || total <= 0 {
        return one(1.0, span.max(0));
    }
    if let RetimeCurve::Constant(s) = &r.curve {
        return one(*s, total);
    }
    let mut out: Vec<AudioWindow> = Vec::new();
    let mut t0 = 0i64;
    while t0 < total {
        let t1 = (t0 + window_us).min(total);
        // The window's SOURCE span comes from the integral at both ends — the
        // same `retime_source_offset` the video path resolves through, so audio
        // and picture can never disagree about where a window's content is.
        let s0 = c.in_us + retime_source_offset(&r.curve, r.timebase_fps, t0);
        let s1 = (c.in_us + retime_source_offset(&r.curve, r.timebase_fps, t1)).min(c.out_us);
        let win_len = t1 - t0;
        if s1 > s0 && win_len > 0 {
            let tempo = ((s1 - s0) as f64 / win_len as f64) as f32;
            out.push(AudioWindow {
                timeline_start_us: c.start_us + t0,
                timeline_len_us: win_len,
                in_us: s0,
                out_us: s1,
                // Defensive clamp: the average of in-range rates is in range,
                // but integer rounding at a short window could nudge it.
                tempo: tempo.max(MIN_SPEED).min(MAX_SPEED),
            });
        } else if win_len > 0 {
            // The source clamp (`.min(c.out_us)`) collapsed this window (IN-01).
            // DROPPING it would leave `win_len` µs of coverage simply ABSENT
            // from the returned vec — and the mixers write SILENCE wherever no
            // window covers, with no diagnostic. Absorb the time into the
            // PREVIOUS window instead: it stretches its own REAL audio a hair
            // further rather than cutting to a hole, and `windows` keeps TILING
            // `[0, total)` by construction rather than by argument.
            //
            // Unreachable for a CONSISTENT contributor today: `timeline_len_us`
            // is the smallest `t` whose area reaches the span, so
            // `offset(t0) < span` for every `t0 < total` and `MIN_SPEED = 0.1`
            // guarantees >= `window_us / 10` of source advance per window
            // (>= 10 µs at the export step, >= 50 ms at the preview one). It IS
            // reachable from a STALE/over-long `timeline_len_us` (the tail then
            // asks for source past `out_us`), and a future change to MIN_SPEED
            // or to the clamp could make an INTERIOR collapse — which would be
            // an audible hole, not a sub-window tail.
            if let Some(prev) = out.last_mut() {
                prev.timeline_len_us += win_len;
                // Keep the window internally consistent: `tempo` IS
                // `(out_us - in_us) / timeline_len_us`, and the render path
                // truncates to the sample count that length implies.
                let tempo = (prev.out_us - prev.in_us) as f64 / prev.timeline_len_us as f64;
                prev.tempo = (tempo as f32).max(MIN_SPEED).min(MAX_SPEED);
            }
        }
        t0 = t1;
    }
    if out.is_empty() {
        return one(1.0, span);
    }
    out
}

/// Snap `pos_us` to the nearest of `candidates` within `threshold_us`;
/// returns `pos_us` unchanged when no candidate is close enough. Pure — the
/// UI applies it during drag/drop with candidates = playhead + clip edges.
///
/// Deterministic ties: when two candidates are EQUIDISTANT, the smaller
/// (earlier) candidate wins, regardless of slice order.
pub fn snap(pos_us: i64, candidates: &[i64], threshold_us: i64) -> i64 {
    let mut best: Option<i64> = None;
    for &c in candidates {
        let d = (c - pos_us).abs();
        if d > threshold_us {
            continue;
        }
        best = Some(match best {
            None => c,
            Some(b) => {
                let bd = (b - pos_us).abs();
                if d < bd || (d == bd && c < b) {
                    c
                } else {
                    b
                }
            }
        });
    }
    best.unwrap_or(pos_us)
}

/// (`Eq` dropped in Phase 6: clips carry an `f32` volume.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
}

/// Per-clip alpha interpretation (Phase 28, OVL-01). Governs how the
/// compositor treats the clip's decoded RGBA at blend time. `Straight`
/// (default): the source carries UN-premultiplied ("straight") alpha — the
/// `fs_layer` shader premultiplies (`rgb *= texel.a`) exactly once at blend
/// (the LOCKED Phase-18 convention). `Premultiplied`: the source ALREADY
/// carries premultiplied RGB — the shader must SKIP its own premultiply or
/// the edges double-premultiply into a washout. Mirrors the derive-shape of
/// [`Interpolation`]/[`TrackKind`]; `#[serde(default)]` on the field keeps
/// every pre-Phase-28 snapshot loadable (missing field = `Straight`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AlphaMode {
    /// Straight (un-premultiplied) alpha — the shader premultiplies once.
    #[default]
    Straight,
    /// Source RGB is already premultiplied — the shader must not do it again.
    Premultiplied,
}

/// Serde default for [`Clip::volume`] — pre-Phase-6 snapshots deserialize
/// with unity gain.
fn default_volume() -> f32 {
    1.0
}

/// Serde default for [`Clip::opacity`] — pre-Phase-18 snapshots deserialize
/// fully opaque.
fn default_opacity() -> f32 {
    1.0
}

/// Per-clip placement transform (Phase 18, TOOL-03/D-08). The two
/// confusion-prone conventions, stated once and locked (they are the contract
/// Phase 19 keyframes animate; the engine's `LayerTransform` mirrors them):
///
/// - `position` is the dest rect's **TOP-LEFT corner** (NOT its centre),
///   normalized `[0,1]` against the project canvas.
/// - `scale` is the dest rect's **normalized DIMS** (width, height as
///   fractions of the canvas — NOT a multiplier); `(1.0, 1.0)` = full canvas.
/// - `rotation_deg` is degrees **clockwise about the dest-rect centre**.
///
/// Default = identity (full-canvas, unrotated). Range validation happens at
/// `Command::apply` (reject non-finite, T-18-01); the engine additionally
/// no-op-draws degenerate geometry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClipTransform {
    /// Dest-rect top-left, normalized [0,1] x [0,1].
    pub position: (f32, f32),
    /// Dest-rect dimensions as canvas fractions (NOT a multiplier).
    pub scale: (f32, f32),
    /// Degrees clockwise about the dest-rect centre.
    pub rotation_deg: f32,
}

impl Default for ClipTransform {
    fn default() -> Self {
        Self {
            position: (0.0, 0.0),
            scale: (1.0, 1.0),
            rotation_deg: 0.0,
        }
    }
}

/// Per-clip source crop as fractional INSETS from each edge, each in `[0,1]`
/// (Phase 18, TOOL-03/D-08): the remaining source window fills the clip's
/// fitted dest quad. `Command::apply` clamps each inset and rejects crops
/// where `left+right >= 1` or `top+bottom >= 1` (no source left).
/// Default = no crop.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ClipCrop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

// ---------------------------------------------------------------------------
// Phase 19 (COMP-04): keyframe animation model.
// ---------------------------------------------------------------------------

/// How a segment interpolates between two keyframes (D-04). Lives PER
/// KEYFRAME (O-2) and governs the segment FROM its key TO the NEXT key.
/// `smooth` is the serde default — a key whose `interp` is omitted in JSON
/// deserializes to `Smooth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Interpolation {
    /// Straight-line: `a + (b - a) * u`.
    Linear,
    /// Step: hold this key's value until the next key.
    Hold,
    /// Smoothstep (cubic Hermite, zero endpoint tangents):
    /// `s = u*u*(3 - 2u)`, `a + (b - a) * s`. Ease-in-ease-out; its exact
    /// midpoint equals the linear mean `(a+b)/2`.
    #[default]
    Smooth,
}

/// One keyframe. `interp` governs the segment FROM this key TO the NEXT key
/// (industry standard: interpolation lives on the LEFT key). The last key's
/// interp is inert (clamp-at-end, D-05).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Keyframe<V> {
    /// Clip-relative frame number in the PROJECT timebase fps (0 = the clip's
    /// first frame as placed on the timeline, D-02/O-1). `u32`: negatives
    /// impossible by construction.
    pub frame: u32,
    pub value: V,
    #[serde(default)]
    pub interp: Interpolation,
}

/// Crop keyframe value — the same 4 fractional edge insets as [`ClipCrop`].
/// A distinct type so `Keyframe<CropValue>` names the animated vocabulary
/// explicitly (interpolated componentwise per inset).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CropValue {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Per-property keyframe tracks on a [`Clip`] (Phase 19, COMP-04). One Vec
/// per animatable property, kept SORTED ascending by `frame` with no
/// duplicates (normalized/validated at `Command::SetKeyframes` apply).
/// Every field is `#[serde(default, skip_serializing_if)]` so an un-animated
/// project's JSON stays byte-identical to pre-Phase-19 output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct KeyframeTracks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub position: Vec<Keyframe<(f32, f32)>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scale: Vec<Keyframe<(f32, f32)>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rotation: Vec<Keyframe<f32>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opacity: Vec<Keyframe<f32>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crop: Vec<Keyframe<CropValue>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume: Vec<Keyframe<f32>>,
}

impl KeyframeTracks {
    /// True when EVERY property track is empty (no animation anywhere) —
    /// also the `skip_serializing_if` predicate that keeps un-animated
    /// clips' JSON pre-19-shaped.
    pub fn is_empty(&self) -> bool {
        self.position.is_empty()
            && self.scale.is_empty()
            && self.rotation.is_empty()
            && self.opacity.is_empty()
            && self.crop.is_empty()
            && self.volume.is_empty()
    }

    /// Property names with a NON-EMPTY track, in field order — the compact
    /// `ClipView.animated` signal (Plan 02) so the agent knows which static
    /// fields are overridden without the full arrays.
    pub fn animated_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if !self.position.is_empty() {
            names.push("position".to_string());
        }
        if !self.scale.is_empty() {
            names.push("scale".to_string());
        }
        if !self.rotation.is_empty() {
            names.push("rotation".to_string());
        }
        if !self.opacity.is_empty() {
            names.push("opacity".to_string());
        }
        if !self.crop.is_empty() {
            names.push("crop".to_string());
        }
        if !self.volume.is_empty() {
            names.push("volume".to_string());
        }
        names
    }

    /// Swap in the new track for `data`'s property, returning the OLD
    /// same-variant track — the self-inverse carrier for
    /// `Command::SetKeyframes` (an empty Vec == clear).
    pub fn replace(&mut self, data: KeyframeTrackData) -> KeyframeTrackData {
        match data {
            KeyframeTrackData::Position(v) => {
                KeyframeTrackData::Position(std::mem::replace(&mut self.position, v))
            }
            KeyframeTrackData::Scale(v) => {
                KeyframeTrackData::Scale(std::mem::replace(&mut self.scale, v))
            }
            KeyframeTrackData::Rotation(v) => {
                KeyframeTrackData::Rotation(std::mem::replace(&mut self.rotation, v))
            }
            KeyframeTrackData::Opacity(v) => {
                KeyframeTrackData::Opacity(std::mem::replace(&mut self.opacity, v))
            }
            KeyframeTrackData::Crop(v) => {
                KeyframeTrackData::Crop(std::mem::replace(&mut self.crop, v))
            }
            KeyframeTrackData::Volume(v) => {
                KeyframeTrackData::Volume(std::mem::replace(&mut self.volume, v))
            }
        }
    }

    /// Content-preserving remap for an operation that cuts `cut_rel_us`
    /// (clip-relative µs, may be NEGATIVE for a left-EXTEND) off the FRONT of
    /// the clip's animated span. The ONE remap used by BOTH `TrimClip` (left
    /// edge, `cut = delta`) and `SplitClip` (right half,
    /// `cut = at_position_us - start_us`) — a left trim and a split's right
    /// half are the SAME operation (drop pre-cut keys, renumber the rest by
    /// `-cut`, synthesize a frame-0 key holding the sampled cut value so the
    /// animation CONTINUES rather than restarting). See [`shift_track`] for the
    /// per-track rules; each track reuses its own sampler so no interpolation
    /// or frame↔µs math is ever duplicated (Pitfall 3).
    ///
    /// A degenerate `fps` (non-finite or `<= 0`, unreachable — `project.fps` is
    /// validated to `(0, 240]`) returns `self.clone()` unchanged; never panics.
    pub fn shifted_for_left_cut(&self, cut_rel_us: i64, fps: f64) -> KeyframeTracks {
        if !fps.is_finite() || fps <= 0.0 {
            return self.clone();
        }
        KeyframeTracks {
            position: shift_track(&self.position, cut_rel_us, fps, lerp_vec2),
            scale: shift_track(&self.scale, cut_rel_us, fps, lerp_vec2),
            rotation: shift_track(&self.rotation, cut_rel_us, fps, lerp_f32),
            opacity: shift_track(&self.opacity, cut_rel_us, fps, lerp_f32),
            crop: shift_track(&self.crop, cut_rel_us, fps, lerp_crop),
            volume: shift_track(&self.volume, cut_rel_us, fps, lerp_f32),
        }
    }
}

/// One property's whole keyframe track as a typed payload — the variant IS
/// the property, so a property/value-shape mismatch is impossible by
/// construction. Carried by `Command::SetKeyframes` (full-track-replace,
/// D-01); Plan 02's `set_keyframes` tool resolves into it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyframeTrackData {
    Position(Vec<Keyframe<(f32, f32)>>),
    Scale(Vec<Keyframe<(f32, f32)>>),
    Rotation(Vec<Keyframe<f32>>),
    Opacity(Vec<Keyframe<f32>>),
    Crop(Vec<Keyframe<CropValue>>),
    Volume(Vec<Keyframe<f32>>),
}

impl KeyframeTrackData {
    /// The property name this track animates (matches the
    /// [`KeyframeTracks`] field / `animated_names` vocabulary).
    pub fn name(&self) -> &'static str {
        match self {
            KeyframeTrackData::Position(_) => "position",
            KeyframeTrackData::Scale(_) => "scale",
            KeyframeTrackData::Rotation(_) => "rotation",
            KeyframeTrackData::Opacity(_) => "opacity",
            KeyframeTrackData::Crop(_) => "crop",
            KeyframeTrackData::Volume(_) => "volume",
        }
    }

    /// Number of keyframes in the carried track.
    pub fn len(&self) -> usize {
        match self {
            KeyframeTrackData::Position(v) => v.len(),
            KeyframeTrackData::Scale(v) => v.len(),
            KeyframeTrackData::Rotation(v) => v.len(),
            KeyframeTrackData::Opacity(v) => v.len(),
            KeyframeTrackData::Crop(v) => v.len(),
            KeyframeTrackData::Volume(v) => v.len(),
        }
    }

    /// True when the carried track has no keyframes (a clear payload).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-track keyframe-count ceiling enforced at `Command::SetKeyframes`
/// apply (threat T-19-01: keyframe arrays originate from LLM tool calls /
/// MCP — bound them BEFORE any allocation-heavy work). 1000 keys is plenty
/// for any real animation; pathological payloads are rejected loudly.
pub const MAX_KEYFRAMES_PER_TRACK: usize = 1000;

/// Keyframe frame-number -> clip-relative µs conversion. This lives HERE and
/// ONLY here (Pitfall 3: never convert at a call site — a second conversion
/// with different rounding or a different fps is exactly the preview/export
/// drift SC-4 forbids). `timebase_fps` is the PROJECT fps (O-1).
fn key_us(frame: u32, timebase_fps: f64) -> i64 {
    (frame as f64 * 1_000_000.0 / timebase_fps).round() as i64
}

/// The inverse of [`key_us`]: clip-relative µs -> nearest PROJECT-fps frame.
/// Lives ONLY here alongside `key_us` (Pitfall 3: one conversion site) — the
/// `shifted_for_left_cut` remap renumbers keys through this so trim/split can
/// never round differently than the sampler. Never negative (a µs at/below 0
/// clamps to frame 0).
fn us_to_frame(us: i64, timebase_fps: f64) -> u32 {
    let f = (us as f64 * timebase_fps / 1_000_000.0).round();
    if f <= 0.0 {
        0
    } else {
        f as u32
    }
}

/// Interpolate scalars through f64 (weights are f64; every plan-asserted
/// expected value is dyadic, so the round-trip through f64 is exact).
fn lerp_f32(a: f32, b: f32, w: f64) -> f32 {
    (a as f64 + (b as f64 - a as f64) * w) as f32
}

fn lerp_vec2(a: (f32, f32), b: (f32, f32), w: f64) -> (f32, f32) {
    (lerp_f32(a.0, b.0, w), lerp_f32(a.1, b.1, w))
}

fn lerp_crop(a: CropValue, b: CropValue, w: f64) -> CropValue {
    CropValue {
        left: lerp_f32(a.left, b.left, w),
        top: lerp_f32(a.top, b.top, w),
        right: lerp_f32(a.right, b.right, w),
        bottom: lerp_f32(a.bottom, b.bottom, w),
    }
}

/// The ONE generic bracket/clamp segment finder every value type shares (no
/// per-type duplication). Deterministic pure µs math (D-07):
///
/// - 0 keys, or a degenerate `timebase_fps` (non-finite or <= 0) -> `None`
///   (the caller falls back to the static field).
/// - 1 key, or `t` at/before the first key's µs -> the FIRST key's value
///   (clamp, D-05).
/// - `t` at/after the last key's µs -> the LAST key's value (clamp, D-05).
/// - else the adjacent pair A,B with `key_us(A) <= t < key_us(B)` brackets
///   `t`; that condition makes the denominator `us_b - us_a` provably > 0
///   before any divide (T-19-03). Equal key times are unreachable
///   post-validation (SetKeyframes rejects duplicate frames), but a
///   defensive guard returns A's value rather than divide.
///
/// Interpolation per A's `interp` (the LEFT key governs the segment):
/// Hold -> `a`; Linear -> `lerp(a, b, u)`; Smooth -> smoothstep
/// `s = u*u*(3 - 2u)`, `lerp(a, b, s)`. Smoothstep's exact midpoint equals
/// the linear mean `(a+b)/2` (at u=0.5, s = 0.25*(3-1) = 0.5).
fn sample_track<V: Copy>(
    track: &[Keyframe<V>],
    clip_relative_us: i64,
    timebase_fps: f64,
    lerp: impl Fn(V, V, f64) -> V,
) -> Option<V> {
    if track.is_empty() || !timebase_fps.is_finite() || timebase_fps <= 0.0 {
        return None;
    }
    let first = &track[0];
    let last = &track[track.len() - 1];
    if track.len() == 1 || clip_relative_us <= key_us(first.frame, timebase_fps) {
        return Some(first.value);
    }
    if clip_relative_us >= key_us(last.frame, timebase_fps) {
        return Some(last.value);
    }
    for pair in track.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let us_a = key_us(a.frame, timebase_fps);
        let us_b = key_us(b.frame, timebase_fps);
        if us_b <= us_a {
            // Defensive: duplicate/equal-rounded key times (unreachable
            // post-validation, D-08 reject) — return A's value, never divide.
            return Some(a.value);
        }
        if clip_relative_us >= us_a && clip_relative_us < us_b {
            return Some(match a.interp {
                Interpolation::Hold => a.value,
                Interpolation::Linear => {
                    let u = (clip_relative_us - us_a) as f64 / (us_b - us_a) as f64;
                    lerp(a.value, b.value, u)
                }
                Interpolation::Smooth => {
                    let u = (clip_relative_us - us_a) as f64 / (us_b - us_a) as f64;
                    let s = u * u * (3.0 - 2.0 * u);
                    lerp(a.value, b.value, s)
                }
            });
        }
    }
    // Unreachable: t is strictly between the first and last keys' µs, so some
    // adjacent pair brackets it. Clamp to the last key defensively.
    Some(last.value)
}

/// Sample a SCALAR keyframe track at a clip-relative time. `None` when the
/// track is empty or `timebase_fps` is degenerate (caller falls back to the
/// static field). `pub` — Plan 04's audio envelope reuses this exact sampler
/// for per-sample volume gains.
pub fn sample_scalar_track(
    track: &[Keyframe<f32>],
    clip_relative_us: i64,
    timebase_fps: f64,
) -> Option<f32> {
    sample_track(track, clip_relative_us, timebase_fps, lerp_f32)
}

/// Vector-pair twin of [`sample_scalar_track`] (position/scale) —
/// componentwise with the same interpolation weight.
fn sample_vec2_track(
    track: &[Keyframe<(f32, f32)>],
    clip_relative_us: i64,
    timebase_fps: f64,
) -> Option<(f32, f32)> {
    sample_track(track, clip_relative_us, timebase_fps, lerp_vec2)
}

/// Crop twin of [`sample_scalar_track`] — each of the 4 insets interpolates
/// independently with the same weight.
fn sample_crop_track(
    track: &[Keyframe<CropValue>],
    clip_relative_us: i64,
    timebase_fps: f64,
) -> Option<CropValue> {
    sample_track(track, clip_relative_us, timebase_fps, lerp_crop)
}

/// Content-preserving remap of ONE keyframe track for an operation that cuts
/// `cut_rel_us` (clip-relative µs) off the FRONT of the clip's animated span
/// (the generic engine behind [`KeyframeTracks::shifted_for_left_cut`]). Reuses
/// the exact same `key_us`/`us_to_frame` conversion and `sample_track`
/// interpolation as the sampler — never a second implementation (Pitfall 3).
///
/// `cut_rel_us > 0` (a left-edge trim's `delta`, or a split's
/// `at_position_us - start_us`):
/// - KEEP keys at/after the cut, renumbered by `-cut_rel_us`;
/// - DROP keys before the cut (their content was cut away);
/// - if any key was dropped and none survives at frame 0, SYNTHESIZE a frame-0
///   key at the value the track samples at the cut, carrying the dropped
///   left-bracketing key's interp — so the animation CONTINUES across the cut
///   instead of restarting (Linear/Hold continue exactly; a Smooth segment's
///   interior re-eases from the new key — the standard NLE approximation).
///
/// `cut_rel_us <= 0` (a left-EXTEND): pure renumber UP, no drops/synthesis.
///
/// Sub-frame rounding can only ever collide two ADJACENT renumbered keys; the
/// later (higher-original-frame) one wins so frames stay strictly increasing
/// (the SetKeyframes invariant).
fn shift_track<V: Copy>(
    track: &[Keyframe<V>],
    cut_rel_us: i64,
    timebase_fps: f64,
    lerp: impl Fn(V, V, f64) -> V,
) -> Vec<Keyframe<V>> {
    // KEEP + renumber every key at/after the cut (drop the rest).
    let mut kept: Vec<Keyframe<V>> = Vec::with_capacity(track.len() + 1);
    let mut any_dropped = false;
    for k in track {
        let kus = key_us(k.frame, timebase_fps);
        if kus < cut_rel_us {
            any_dropped = true;
            continue;
        }
        kept.push(Keyframe {
            frame: us_to_frame(kus - cut_rel_us, timebase_fps),
            value: k.value,
            interp: k.interp,
        });
    }
    // Collision guard: renumbered frames are non-decreasing (round is
    // monotonic), so equal frames are always adjacent — keep the LATER one.
    let mut out: Vec<Keyframe<V>> = Vec::with_capacity(kept.len());
    for k in kept {
        match out.last_mut() {
            Some(last) if last.frame == k.frame => *last = k,
            _ => out.push(k),
        }
    }
    // Synthesize a frame-0 continuity key when content was cut away and no
    // surviving key already lands on frame 0.
    if any_dropped && out.first().map_or(true, |f| f.frame != 0) {
        if let Some(value) = sample_track(track, cut_rel_us, timebase_fps, &lerp) {
            let interp = track
                .iter()
                .rev()
                .find(|k| key_us(k.frame, timebase_fps) < cut_rel_us)
                .map(|k| k.interp)
                .unwrap_or_default();
            out.insert(
                0,
                Keyframe {
                    frame: 0,
                    value,
                    interp,
                },
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Quick task 260730-x2t (RT-01..RT-08): TIME REMAP (speed / speed ramps).
//
// Speed is INTEGRATED over time, not sampled at a time — which is exactly why
// it does NOT live in `KeyframeTracks` (RT-02): `sample_at` answers "what is
// the value AT t"; retime answers "how much SOURCE has been consumed BY t".
// Putting a speed track in `KeyframeTracks` would silently corrupt `sample_at`,
// `animated_names`, `shifted_for_left_cut` and `KeyframeTrackData::replace`.
// ---------------------------------------------------------------------------

/// Speed bounds (RT-08): strictly positive, finite. Reverse playback
/// (negative speed) is a SEPARATE feature and is deliberately OUT OF SCOPE —
/// negative speed destroys the monotonicity of the timeline→source map that
/// [`Timeline::active_at`]'s ordering (and the preview stamp's bisection
/// inverse) depend on. Premiere ships reverse as its own "Reverse Speed"
/// checkbox for the same reason. Validators REJECT `speed <= 0`.
pub const MIN_SPEED: f32 = 0.1;
/// Upper speed bound (RT-08). See [`MIN_SPEED`].
pub const MAX_SPEED: f32 = 10.0;

/// Ramp key ceiling (threat T-x2t-02). DELIBERATELY far below
/// [`MAX_KEYFRAMES_PER_TRACK`] (1000): [`Clip::source_offset_at`] accumulates
/// segments LINEARLY on the per-output-frame hot path, so the key count IS the
/// per-frame cost. A real speed ramp needs 2-6 points (Resolve's Retime
/// Controls, FCP's speed segments); 64 is generous and keeps the hot path
/// trivially bounded without a derived prefix-sum cache serde would have to
/// keep in sync.
pub const MAX_RETIME_KEYS: usize = 64;

/// How a clip's playback rate varies over its own timeline span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetimeCurve {
    /// One multiplier for the whole clip. 2.0 = twice as fast (half the
    /// timeline length); 0.5 = half speed (double the timeline length).
    Constant(f32),
    /// A speed RAMP: clip-relative PROJECT-fps keys whose VALUE is the
    /// playback RATE at that instant. Sorted ascending, no duplicate frames,
    /// `<= MAX_RETIME_KEYS`. Reuses [`Keyframe<f32>`]/[`Interpolation`] so the
    /// frame/value/interp vocabulary and validation shape are shared with
    /// [`KeyframeTracks`] — but it deliberately does NOT LIVE there (RT-02).
    Ramp(Vec<Keyframe<f32>>),
}

/// A clip's time remap. Attached as [`Clip::retime`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Retime {
    pub curve: RetimeCurve,
    /// PRECOMPUTED timeline occupancy in µs (RT-03) — the `t*` where the
    /// accumulated source area equals `out_us - in_us`. Computed ONCE at
    /// `Command::SetClipRetime`/`TrimClip`/`SplitClip` apply and STORED, so
    /// [`Clip::timeline_len_us`] stays O(1) and allocation-free on the
    /// per-output-frame hot path (it is called inside `active_at`'s per-clip
    /// `find` predicate, per output frame, per track, per clip, from BOTH
    /// export and preview). INVARIANT (asserted in `tests/retime.rs`):
    /// ```text
    ///   source_offset_at(timeline_len_us)     >= out_us - in_us
    ///   source_offset_at(timeline_len_us - 1) <  out_us - in_us
    /// ```
    /// It is DERIVED STATE: every mutation of `in_us`/`out_us`/`curve` MUST
    /// recompute it via [`Clip::recomputed_retime`]. A stale value is an
    /// occupancy bug that silently misplaces every later clip, so
    /// `assert_retime_cache_consistent` asserts it project-wide in the
    /// edit-op tests (threat T-x2t-08).
    pub timeline_len_us: i64,
    /// The PROJECT fps the ramp keys were frozen against, so
    /// [`Clip::source_offset_at`] needs no fps parameter and
    /// [`Timeline::active_at`]'s signature is UNCHANGED (RT-04 — changing it
    /// would ripple into `export.rs`, `resolve.rs`, `lib.rs` and every test).
    /// Consequence, deliberate and documented: changing `project.fps` later
    /// does NOT rewarp an existing ramp. A ramp is a curve in REAL TIME; it
    /// should not move because the output cadence changed.
    pub timebase_fps: f64,
}

impl RetimeCurve {
    /// The playback RATE at `clip_relative_us`, clamped before the first key
    /// and after the last (D-05, no extrapolation). Reuses the shared
    /// [`sample_track`] sampler so a ramp's interpolation can never drift from
    /// the keyframe vocabulary it borrows.
    pub fn speed_at(&self, clip_relative_us: i64, timebase_fps: f64) -> f32 {
        match self {
            RetimeCurve::Constant(s) => *s,
            RetimeCurve::Ramp(keys) => {
                sample_scalar_track(keys, clip_relative_us, timebase_fps).unwrap_or(1.0)
            }
        }
    }

    /// Content-preserving remap for a cut of `cut_rel_us` TIMELINE µs off the
    /// FRONT of the clip — the rate-curve twin of
    /// [`KeyframeTracks::shifted_for_left_cut`], reusing the very same
    /// [`shift_track`] engine (Pitfall 3: never a second implementation).
    ///
    /// A [`RetimeCurve::Constant`] is closed under subdivision and passes
    /// through untouched. A [`RetimeCurve::Ramp`] drops pre-cut keys,
    /// renumbers by `-cut`, and synthesizes a frame-0 key holding the sampled
    /// RATE at the cut so the ramp CONTINUES rather than restarting.
    /// `Hold` and `Linear` cuts are EXACT (both closed under subdivision);
    /// `Smooth` re-eases within the cut segment — a bounded approximation of
    /// exactly the kind `shift_track` already accepts for visual properties,
    /// made lossless on undo by `TrimClip`'s `restore_retime` carrier.
    pub fn shifted_for_left_cut(&self, cut_rel_us: i64, timebase_fps: f64) -> RetimeCurve {
        match self {
            RetimeCurve::Constant(s) => RetimeCurve::Constant(*s),
            RetimeCurve::Ramp(keys) => {
                if !timebase_fps.is_finite() || timebase_fps <= 0.0 {
                    return RetimeCurve::Ramp(keys.clone());
                }
                RetimeCurve::Ramp(shift_track(keys, cut_rel_us, timebase_fps, lerp_f32))
            }
        }
    }
}

/// The real-valued SOURCE area consumed over `[0, t]` of clip-relative
/// TIMELINE time — the closed-form INTEGRAL of the speed curve. Never
/// quadrature: each segment has an exact antiderivative (verified against
/// 10_000-step numeric quadrature in `tests/retime.rs`).
///
/// | interp   | speed(u)                    | integral over `[0, uT]`             |
/// |----------|-----------------------------|-------------------------------------|
/// | `Hold`   | `a`                         | `T * a*u`                           |
/// | `Linear` | `a + (b-a)*u`               | `T * (a*u + (b-a)*u^2/2)`           |
/// | `Smooth` | `a + (b-a)*(3u^2 - 2u^3)`   | `T * (a*u + (b-a)*(u^3 - u^4/2))`   |
///
/// Clamps (D-05, no extrapolation): before the first key the rate is the first
/// key's value; after the last key it is the last key's value. `t <= 0`
/// returns `0.0` (the negative branch is handled by the odd-symmetric wrapper
/// in [`retime_source_offset`]).
fn retime_source_area(curve: &RetimeCurve, timebase_fps: f64, t: i64) -> f64 {
    if t <= 0 {
        return 0.0;
    }
    let t = t as f64;
    match curve {
        RetimeCurve::Constant(s) => t * (*s as f64),
        RetimeCurve::Ramp(keys) => {
            if keys.is_empty() || !timebase_fps.is_finite() || timebase_fps <= 0.0 {
                // Degenerate curve -> the identity map. Never divides, never
                // panics (the `source_offset_at` no-panic contract).
                return t;
            }
            if keys.len() == 1 {
                return t * (keys[0].value as f64);
            }
            let mut area = 0.0f64;
            // Leading clamp region: [0, first key) holds the FIRST key's rate.
            let first_us = key_us(keys[0].frame, timebase_fps).max(0);
            if t <= first_us as f64 {
                return t * (keys[0].value as f64);
            }
            area += first_us as f64 * (keys[0].value as f64);
            // Interior segments.
            for pair in keys.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                let us_a = key_us(a.frame, timebase_fps).max(0) as f64;
                let us_b = key_us(b.frame, timebase_fps).max(0) as f64;
                if us_b <= us_a {
                    // Defensive: duplicate/equal-rounded key times are
                    // unreachable post-validation — never divide.
                    continue;
                }
                if t <= us_a {
                    break;
                }
                let seg_t = us_b - us_a;
                let x = (t - us_a).min(seg_t);
                let u = x / seg_t;
                let (va, vb) = (a.value as f64, b.value as f64);
                area += match a.interp {
                    Interpolation::Hold => va * x,
                    Interpolation::Linear => seg_t * (va * u + (vb - va) * u * u / 2.0),
                    Interpolation::Smooth => {
                        let u3 = u * u * u;
                        seg_t * (va * u + (vb - va) * (u3 - u3 * u / 2.0))
                    }
                };
            }
            // Trailing clamp region: past the last key the rate holds.
            let last = &keys[keys.len() - 1];
            let last_us = key_us(last.frame, timebase_fps).max(0) as f64;
            if t > last_us {
                area += (t - last_us) * (last.value as f64);
            }
            area
        }
    }
}

/// Integer source offset for `clip_relative_us` — [`retime_source_area`]
/// FLOORED.
///
/// Floor, not round, is load-bearing: it is what keeps
/// `source_offset_at(timeline_len_us - 1) < out_us - in_us`, i.e. what keeps
/// [`ClipHit::source_us`] inside `[in_us, out_us)` at the clip's LAST frame
/// (rounding would let the final tick resolve to exactly `out_us`, one frame
/// past the trimmed content). It is also what makes a constant speed EXACT:
/// the smallest `t` with `floor(t*0.5) >= span` is exactly `2*span`, whereas
/// with rounding it would be `2*span - 1`.
///
/// Negative `t` (a left-EXTEND's delta) maps odd-symmetrically so the
/// timeline↔source map stays a single consistent function on both sides of
/// the clip origin.
pub fn retime_source_offset(curve: &RetimeCurve, timebase_fps: f64, clip_relative_us: i64) -> i64 {
    if clip_relative_us < 0 {
        return -(retime_source_area(curve, timebase_fps, -clip_relative_us).floor() as i64);
    }
    retime_source_area(curve, timebase_fps, clip_relative_us).floor() as i64
}

/// The INVERSE of [`retime_source_offset`]: the smallest `t >= 1` whose
/// accumulated source area reaches `source_span_us`. This is the number
/// STORED in [`Retime::timeline_len_us`].
///
/// Integer bisection over `[1, 10*span + 16]` — speed is bounded to
/// `[MIN_SPEED, MAX_SPEED] = [0.1, 10]`, so at the slowest legal rate `10*span`
/// µs of timeline consumes `span` µs of source and the answer is provably
/// inside the bracket. Fixed iteration count (~log2 of the bracket),
/// deterministic, allocation-free. **Called at APPLY time ONLY** (RT-03,
/// threat T-x2t-03) — a root-solve on the per-output-frame hot path would turn
/// every export tick into a numeric solve over the key array.
pub fn retimed_timeline_len_us(
    curve: &RetimeCurve,
    timebase_fps: f64,
    source_span_us: i64,
) -> i64 {
    if source_span_us <= 0 {
        return source_span_us;
    }
    let mut lo: i64 = 1;
    let mut hi: i64 = source_span_us.saturating_mul(10).saturating_add(16);
    // Guard the (unreachable post-validation) case where even `hi` cannot
    // reach the span: return `hi` rather than loop or panic.
    if retime_source_offset(curve, timebase_fps, hi) < source_span_us {
        return hi;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if retime_source_offset(curve, timebase_fps, mid) >= source_span_us {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// Validate-then-return a [`RetimeCurve`] (threats T-x2t-01/02/05): bounds,
/// finiteness, key cap, stable sort ascending, adjacent-duplicate REJECT —
/// mirroring `validate_keyframe_tracks` verbatim. Values are REJECTED, never
/// clamped: a silently-clamped speed is a silently-wrong export length.
pub fn validate_retime(curve: &RetimeCurve) -> Result<RetimeCurve, crate::CoreError> {
    let check = |v: f32, what: &str| -> Result<f32, crate::CoreError> {
        if !v.is_finite() || v <= 0.0 || v < MIN_SPEED || v > MAX_SPEED {
            return Err(crate::CoreError::InvalidSettings(format!(
                "{what} must be finite and within [{MIN_SPEED}, {MAX_SPEED}] \
                 (reverse playback / speed <= 0 is out of scope), got {v}"
            )));
        }
        Ok(v)
    };
    match curve {
        RetimeCurve::Constant(s) => Ok(RetimeCurve::Constant(check(*s, "clip speed")?)),
        RetimeCurve::Ramp(keys) => {
            // (a) Cap FIRST — bound the LLM/MCP-supplied array before any
            // allocation-heavy work (the MAX_KEYFRAMES_PER_TRACK precedent).
            if keys.len() > MAX_RETIME_KEYS {
                return Err(crate::CoreError::InvalidSettings(format!(
                    "speed ramp has {} keys, exceeds the {MAX_RETIME_KEYS} cap",
                    keys.len()
                )));
            }
            // (b) Per-key value rule.
            let mut out: Vec<Keyframe<f32>> = Vec::with_capacity(keys.len());
            for k in keys {
                out.push(Keyframe {
                    frame: k.frame,
                    value: check(k.value, "speed keyframe value")?,
                    interp: k.interp,
                });
            }
            // (c) Stable sort ascending + adjacent-duplicate REJECT.
            out.sort_by_key(|k| k.frame);
            if let Some(w) = out.windows(2).find(|w| w[0].frame == w[1].frame) {
                return Err(crate::CoreError::InvalidSettings(format!(
                    "duplicate speed keyframe at frame {}",
                    w[0].frame
                )));
            }
            Ok(RetimeCurve::Ramp(out))
        }
    }
}

/// The project-fps ceiling `Command::SetProjectSettings` already enforces —
/// reused as the sanity bound on a wire- OR DISK-carried
/// [`Retime::timebase_fps`].
pub const MAX_TIMEBASE_FPS: f64 = 240.0;

/// True for a curve that means "no retime at all": a `Constant` of exactly 1.0,
/// or an EMPTY ramp. Normalizing these to `None` is what keeps an un-retimed
/// clip's JSON byte-identical to pre-retime output (`skip_serializing_if`
/// omit-when-default, which Rudis already practises for `keyframes`/`text`).
///
/// A ramp whose keys ALL happen to be 1.0 is deliberately NOT identity: it is a
/// real authored curve (and the audio seam test in Task 5 depends on a flat
/// ramp surviving as a ramp).
pub fn retime_curve_is_identity(curve: &RetimeCurve) -> bool {
    match curve {
        RetimeCurve::Constant(s) => *s == 1.0,
        RetimeCurve::Ramp(keys) => keys.is_empty(),
    }
}

/// **THE one untrusted-[`Retime`] sanitizer.** Validate a carrier's curve and
/// REBUILD its derived state from ground truth: the curve goes through
/// [`validate_retime`], a degenerate `timebase_fps` falls back to
/// `project_fps`, and [`Retime::timeline_len_us`] is ALWAYS recomputed from
/// `source_span_us` — never trusted from the wire OR from disk (threats
/// T-x2t-05 / T-x2t-08). Identity curves normalize to `None`.
///
/// Rebuilding is EXACT for the legitimate undo path: `timeline_len_us` is a
/// pure function of `(curve, fps, span)` and the span is restored first, so a
/// trim/undo round-trip reproduces the identical `Retime`.
///
/// Lives HERE rather than in `command.rs` because there are TWO untrusted
/// boundaries, not one: the `dispatch_command` IPC surface (every whole-`Clip`
/// carrier) and the `.rud` LOAD path
/// ([`Project::sanitize_retime_after_load`]). A second implementation for the
/// load boundary is exactly how CR-01 happened.
pub fn sanitized_retime(
    carried: Option<&Retime>,
    project_fps: f64,
    source_span_us: i64,
) -> Result<Option<Retime>, crate::CoreError> {
    let Some(r) = carried else {
        return Ok(None);
    };
    let curve = validate_retime(&r.curve)?;
    if retime_curve_is_identity(&curve) {
        return Ok(None);
    }
    let fps = if r.timebase_fps.is_finite()
        && r.timebase_fps > 0.0
        && r.timebase_fps <= MAX_TIMEBASE_FPS
    {
        r.timebase_fps
    } else {
        project_fps
    };
    Ok(Some(Retime {
        timeline_len_us: retimed_timeline_len_us(&curve, fps, source_span_us),
        curve,
        timebase_fps: fps,
    }))
}

/// Concrete, already-sampled per-frame property values — the exact vocabulary
/// Phase 18's compositor consumes (mapped field-for-field to `engine::Layer`
/// at the two host call sites, identical to how the STATIC fields are
/// mapped today). Produced ONLY by [`Clip::sample_at`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampledProps {
    pub transform: ClipTransform,
    pub opacity: f32,
    pub crop: ClipCrop,
    pub volume: f32,
}

// ---------------------------------------------------------------------------
// Phase 20 (TEXT-01): text overlays as real clips.
// ---------------------------------------------------------------------------

/// Horizontal text alignment (Phase 20). Serialized snake_case; `Left` is the
/// serde default. Mirrors `engine::TextAlign` (this pure crate cannot depend on
/// the engine — keep the two in sync).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// The shared text-style vocabulary (Phase 20, TEXT-01 / D-06). This is the
/// SINGLE SOURCE OF TRUTH Phase 22 captions reuse VERBATIM — do NOT fork a
/// second styling model there. Every dimension is normalized so a style is
/// resolution-independent (reproducible across 1080p/4K), matching the
/// transform/crop/wrap conventions Phase 18 locked.
///
/// Validated at `Command::apply` (never here): `font_family` must be the
/// bundled font (`crate::command::is_bundled_font` — Inter only); `font_size`
/// finite & in `(0, 1]` (a normalized fraction of canvas height); `wrap_width`
/// in `(0, 1]` when `Some` (a normalized fraction of canvas width). Out-of-range
/// values are rejected, never clamped (M-02).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextStyle {
    /// Bundled font family name (validated against the bundled set).
    pub font_family: String,
    /// NORMALIZED font size: a fraction of the canvas HEIGHT
    /// (`px = font_size * project_height` at raster time). Resolution-
    /// independent; the shape Phase 22 captions inherit.
    pub font_size: f32,
    /// Fill color as RGBA bytes.
    pub fill: [u8; 4],
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub align: TextAlign,
    /// Normalized `[0,1]` fraction of canvas WIDTH to wrap within (SC-4);
    /// `None` = auto-fit to the text's natural width (no wrap).
    #[serde(default)]
    pub wrap_width: Option<f32>,
}

impl Default for TextStyle {
    /// A sensible default caption style: bundled Inter, 10% of canvas height,
    /// opaque white, left-aligned, auto-fit width.
    fn default() -> Self {
        Self {
            font_family: "Inter".to_string(),
            font_size: 0.1,
            fill: [255, 255, 255, 255],
            bold: false,
            italic: false,
            align: TextAlign::Left,
            wrap_width: None,
        }
    }
}

impl TextStyle {
    /// Partial-merge (palmier semantics, D-06): write ONLY the `Some` fields of
    /// `patch`, leaving every unpassed field untouched. THE shared merge Phase
    /// 22 captions reuse — a `TextStylePatch` with all-`None` fields is a no-op.
    /// `wrap_width` is `Option<Option<f32>>`: outer `None` = leave the field
    /// alone; `Some(inner)` overwrites (so `Some(None)` explicitly sets
    /// auto-fit, `Some(Some(x))` sets an explicit wrap).
    pub fn apply_patch(&mut self, patch: &TextStylePatch) {
        if let Some(v) = &patch.font_family {
            self.font_family = v.clone();
        }
        if let Some(v) = patch.font_size {
            self.font_size = v;
        }
        if let Some(v) = patch.fill {
            self.fill = v;
        }
        if let Some(v) = patch.bold {
            self.bold = v;
        }
        if let Some(v) = patch.italic {
            self.italic = v;
        }
        if let Some(v) = patch.align {
            self.align = v;
        }
        if let Some(v) = patch.wrap_width {
            self.wrap_width = v;
        }
    }
}

/// A text overlay's content + style (Phase 20, TEXT-01). Carried by
/// `Clip.text`: a text clip is an ordinary `Clip` whose `media_id` is the empty
/// sentinel (`""`, ignored while `text.is_some()`), `in_us = 0`,
/// `out_us = duration` (defining its timeline length via `timeline_len_us`), and
/// whose `text` is `Some`. It flows through `active_layers_at` → compositor →
/// export exactly like a media clip (D-01), inheriting trim/split/move/keyframes
/// with zero new plumbing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPayload {
    pub content: String,
    pub style: TextStyle,
    /// Caption-group membership (Phase 22, TEXT-04). When `Some(id)`, this text
    /// clip was minted by `add_captions` and shares `id` with every other clip
    /// in the same caption group — the tag that makes "restyle all captions"
    /// (`update_text` groupId mode) resolve to the whole group. Because it lives
    /// ON the text payload, it moves/trims/splits WITH the clip like any other
    /// field: there is NO separate caption structure to desync (the industry-wide
    /// caption-drift bug is structurally impossible here, T-22-13). `None` on an
    /// ordinary (non-caption) text clip; `skip_serializing_if` keeps every
    /// pre-Phase-22 snapshot byte-identical and loadable with no migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_group_id: Option<String>,
}

impl TextPayload {
    /// A text payload with the given content and the default caption style.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            style: TextStyle::default(),
            caption_group_id: None,
        }
    }
}

/// Partial-merge input for `Command::UpdateText` (Phase 20, D-06) — every field
/// is `Option`, and ONLY `Some` fields overwrite the target style. The SINGLE
/// source of truth Phase 22's caption-edit path reuses. `wrap_width` is
/// `Option<Option<f32>>` so "leave alone" (outer `None`) is distinct from
/// "set to auto-fit" (`Some(None)`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TextStylePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<[u8; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<TextAlign>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap_width: Option<Option<f32>>,
}

/// A clip placed on a track. `start_us` is the position on the timeline;
/// `in_us`/`out_us` are the trimmed range within the source media.
/// All microseconds. Trim/split are DATA ops on these fields (Phase 6):
/// frame-accuracy = data precision here + the engine's PTS-accurate decode.
///
/// (`Eq` dropped in Phase 6: `volume` is an `f32`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    /// Stable string id, unique across the whole timeline.
    pub id: String,
    /// References a `MediaBinItem::id`.
    pub media_id: String,
    /// Timeline position (microseconds).
    pub start_us: i64,
    /// Source in-point (microseconds).
    pub in_us: i64,
    /// Source out-point (microseconds, exclusive; must be > in_us).
    pub out_us: i64,
    /// Audio gain multiplier, clamped >= 0 (1.0 = unity). Applied when the
    /// clip's audio is rendered (`#[serde(default)]` keeps pre-Phase-6
    /// snapshots loadable).
    #[serde(default = "default_volume")]
    pub volume: f32,
    /// True after `DetachAudio`: this VIDEO clip no longer contributes audio
    /// (its audio lives on a separate audio-track clip). Always false for
    /// audio-track clips.
    #[serde(default)]
    pub audio_detached: bool,
    /// Placement transform (Phase 18, TOOL-03) — see [`ClipTransform`] for
    /// the position/scale conventions. `#[serde(default)]` = identity keeps
    /// every pre-Phase-18 snapshot loadable. Inert on audio-track clips.
    /// NOTE for production Clip-construction sites: a new clip derived from
    /// an EXISTING clip (split/duplicate/detach) must INHERIT these visual
    /// fields via struct-update (`..original`), never default-reset them.
    #[serde(default)]
    pub transform: ClipTransform,
    /// Layer opacity in `[0,1]` (1.0 = opaque), clamped at apply (Phase 18).
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    /// Source crop insets — see [`ClipCrop`] (Phase 18).
    #[serde(default)]
    pub crop: ClipCrop,
    /// Per-property keyframe tracks (Phase 19, COMP-04). A NON-EMPTY track
    /// for a property OVERRIDES that property's static field at sample time
    /// (D-06); an empty track falls back to the static field. Mutated ONLY
    /// via the undoable `Command::SetKeyframes` (full-track-replace, D-01).
    /// `#[serde(default)]` keeps every pre-Phase-19 snapshot loadable;
    /// `skip_serializing_if` keeps un-animated projects' JSON byte-identical
    /// to pre-19 (no `keyframes` key emitted). Like `transform`, derived-clip
    /// production sites (split/duplicate/detach) INHERIT this field via
    /// struct-update (`..original`) — never default-reset it.
    #[serde(default, skip_serializing_if = "KeyframeTracks::is_empty")]
    pub keyframes: KeyframeTracks,
    /// Text overlay payload (Phase 20, TEXT-01). `Some` makes this a TEXT clip:
    /// `media_id` is the empty sentinel and the clip rasterizes its
    /// [`TextPayload`] into the composite instead of decoding media. `None` for
    /// an ordinary media clip. `#[serde(default)]` keeps every pre-Phase-20
    /// snapshot loadable; `skip_serializing_if` keeps a non-text project's JSON
    /// byte-identical to pre-20 (no `text` key emitted). Like `transform`/
    /// `keyframes`, derived-clip production sites (split/duplicate/detach)
    /// INHERIT this field via struct-update (`..original`) — a split/duplicated
    /// text clip carries the SAME payload forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextPayload>,
    /// Per-clip alpha interpretation (Phase 28, OVL-01) — see [`AlphaMode`].
    /// `Straight` (default): the shader premultiplies (`rgb *= alpha`) at
    /// blend. `Premultiplied`: the source already carries premultiplied RGB —
    /// the shader must NOT premultiply again (double-premultiply washout).
    /// `#[serde(default)]` keeps every pre-Phase-28 snapshot loadable
    /// (default = `Straight`). Like `transform`/`keyframes`/`text`, derived-
    /// clip production sites (split/duplicate/detach) INHERIT this field via
    /// struct-update (`..original`) — never default-reset it.
    #[serde(default)]
    pub alpha_mode: AlphaMode,
    /// Time remap (quick task 260730-x2t): constant speed or a speed RAMP.
    /// `None` = play at 1:1, and [`Clip::timeline_len_us`] /
    /// [`Clip::source_offset_at`] then behave byte-for-byte as they did before
    /// retime existed. `#[serde(default)]` keeps every pre-retime snapshot
    /// loadable; `skip_serializing_if` keeps an un-retimed project's JSON
    /// byte-identical (no `retime` key emitted).
    ///
    /// Like `transform`/`keyframes`/`text`, derived-clip production sites
    /// (duplicate/detach) INHERIT this field via struct-update (`..original`).
    /// **EXCEPTION — split and trim:** `retime` carries a DERIVED
    /// `timeline_len_us` and a curve anchored to the clip's own origin, so
    /// `SplitClip`'s right half remaps the curve via
    /// [`RetimeCurve::shifted_for_left_cut`] and BOTH halves recompute the
    /// cached length via [`Clip::recomputed_retime`]. It is the SECOND field
    /// (with `keyframes`) that must not plain-inherit across a cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retime: Option<Retime>,
}

impl Clip {
    /// How long the clip occupies on the timeline. For an un-retimed clip
    /// this is its trimmed source length (`out_us - in_us`), byte-for-byte as
    /// before retime existed. For a RETIMED clip it is the PRECOMPUTED
    /// [`Retime::timeline_len_us`] (RT-03) — an O(1), allocation-free field
    /// read, never a root-solve: this is called inside `active_at`'s per-clip
    /// `find` predicate, per output frame, per track, from both export and
    /// preview.
    pub fn timeline_len_us(&self) -> i64 {
        match &self.retime {
            Some(r) => r.timeline_len_us,
            None => self.out_us - self.in_us,
        }
    }

    /// Source µs consumed from the clip's in-point after `clip_relative_us` of
    /// TIMELINE time — the INTEGRAL of the speed curve, not a multiply.
    ///
    /// **This is THE timeline→source map (RT-04).** Both preview and export
    /// resolve through it: export via [`Timeline::active_at`] /
    /// `active_layers_at` per output tick, preview via the same resolver plus
    /// `pts_map`'s bisection inverse of this exact function. A SECOND
    /// implementation anywhere — a `setpts` filter in one of the four decoder
    /// invocations, a decoder-side remap, an export-only multiply — is the
    /// preview/export fork CONTEXT.md forbids. Add callers, never twins.
    ///
    /// `retime: None` returns `clip_relative_us` verbatim (the pre-retime
    /// identity, byte-for-byte). Never panics; a degenerate curve returns the
    /// identity rather than dividing by zero.
    pub fn source_offset_at(&self, clip_relative_us: i64) -> i64 {
        match &self.retime {
            Some(r) => retime_source_offset(&r.curve, r.timebase_fps, clip_relative_us),
            None => clip_relative_us,
        }
    }

    /// This clip's retime with [`Retime::timeline_len_us`] RECOMPUTED from the
    /// CURRENT `in_us`/`out_us`. Call after ANY mutation of the source span or
    /// the curve — a stale cached length is an occupancy bug that silently
    /// misplaces every later clip (threat T-x2t-08).
    pub fn recomputed_retime(&self) -> Option<Retime> {
        self.retime.as_ref().map(|r| Retime {
            timeline_len_us: retimed_timeline_len_us(
                &r.curve,
                r.timebase_fps,
                self.out_us - self.in_us,
            ),
            curve: r.curve.clone(),
            timebase_fps: r.timebase_fps,
        })
    }

    /// Exclusive timeline end = `start_us + timeline_len_us()`. A position
    /// exactly here belongs to the NEXT clip (end-exclusive occupancy).
    ///
    /// SATURATING (260730-x2t CR-01): `timeline_len_us()` reads a stored,
    /// derived field. `Project::sanitize_retime_after_load` and
    /// [`sanitized_retime`] mean no legitimate path can present an
    /// `i64::MAX`-scale occupancy — but this is called from `active_at`'s
    /// per-clip predicate on the per-output-frame hot path, so a future gap in
    /// one of those boundaries must degrade to a clamped end, never to a debug
    /// panic or a release wrap-to-NEGATIVE (which would make the clip
    /// INVISIBLE to `active_at`'s half-open test).
    pub fn timeline_end_us(&self) -> i64 {
        self.start_us.saturating_add(self.timeline_len_us())
    }

    /// THE one shared sampler (D-07, SC-4): both the preview loop and the
    /// export loop call this — never a second implementation.
    /// `clip_relative_us = timeline_t - clip.start_us` (0 = the clip's first
    /// frame as placed on the timeline). `timebase_fps` = PROJECT fps (O-1)
    /// — both call sites MUST pass the same value or preview/export drift.
    ///
    /// A NON-EMPTY keyframe track OVERRIDES the static field (D-06); an
    /// empty track falls back to it verbatim. Sampling before the first /
    /// after the last key clamps to the nearest key (D-05, no
    /// extrapolation). `smooth` is smoothstep `s = u*u*(3 - 2u)` — its exact
    /// midpoint equals the linear mean `(a+b)/2`. Pure deterministic µs
    /// math; a degenerate `timebase_fps` (non-finite or <= 0) samples
    /// nothing and returns the static fields (never panics/divides).
    pub fn sample_at(&self, clip_relative_us: i64, timebase_fps: f64) -> SampledProps {
        let t = clip_relative_us;
        SampledProps {
            transform: ClipTransform {
                position: sample_vec2_track(&self.keyframes.position, t, timebase_fps)
                    .unwrap_or(self.transform.position),
                scale: sample_vec2_track(&self.keyframes.scale, t, timebase_fps)
                    .unwrap_or(self.transform.scale),
                rotation_deg: sample_scalar_track(&self.keyframes.rotation, t, timebase_fps)
                    .unwrap_or(self.transform.rotation_deg),
            },
            opacity: sample_scalar_track(&self.keyframes.opacity, t, timebase_fps)
                .unwrap_or(self.opacity),
            crop: sample_crop_track(&self.keyframes.crop, t, timebase_fps)
                .map(|v| ClipCrop {
                    left: v.left,
                    top: v.top,
                    right: v.right,
                    bottom: v.bottom,
                })
                .unwrap_or(self.crop),
            volume: sample_scalar_track(&self.keyframes.volume, t, timebase_fps)
                .unwrap_or(self.volume),
        }
    }
}

/// Transport/preview state (Phase 4): which media item is loaded in the
/// Preview and where the playhead is. All times are MICROSECONDS.
///
/// Mutated ONLY via [`crate::TransportCmd`] through [`crate::Store::transport`]
/// — playback navigation is deliberately NOT undoable (see store.rs for the
/// rationale). `duration_us`/`fps` are copied from the loaded
/// [`MediaBinItem`] so clock math never re-derives them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playback {
    /// `MediaBinItem::id` of the media loaded in the Preview, if any.
    pub loaded_media_id: Option<String>,
    pub playing: bool,
    /// Playhead position, clamped to `[0, duration_us]`.
    pub position_us: i64,
    /// Duration of the loaded media (0 when nothing is loaded).
    pub duration_us: i64,
    /// Working frame rate of the loaded media (0.0 when nothing is loaded).
    pub fps: f64,
    /// Wrap to 0 at the end instead of pausing.
    pub looping: bool,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            loaded_media_id: None,
            playing: false,
            position_us: 0,
            duration_us: 0,
            fps: 0.0,
            looping: false,
        }
    }
}

/// Nominal duration of one frame in MICROSECONDS = round(1_000_000 / fps);
/// 0 for non-positive fps.
///
/// NOTE: `engine::frame_step_us` is an intentional twin (this pure crate
/// cannot depend on the engine crate). Keep the two in sync.
pub fn frame_step_us(fps: f64) -> i64 {
    if fps <= 0.0 {
        return 0;
    }
    (1_000_000.0 / fps).round() as i64
}

impl Playback {
    /// One nominal frame of the loaded media, in microseconds.
    pub fn frame_step_us(&self) -> i64 {
        frame_step_us(self.fps)
    }

    /// Clamp a position into the valid playhead range `[0, duration_us]`.
    pub fn clamp_position(&self, position_us: i64) -> i64 {
        position_us.clamp(0, self.duration_us.max(0))
    }

    /// Frame-clock tick: advance the playhead by `dt_us` (elapsed WALL time
    /// from the play loop — the clock follows real time, so dropped ticks
    /// never slow playback down). No-op unless `playing`. At the end:
    /// wraps to the remainder when `looping`, else clamps to `duration_us`
    /// and pauses.
    pub fn advance(&mut self, dt_us: i64) {
        self.advance_with_duration(dt_us, self.duration_us);
    }

    /// Same clock math as [`advance`](Self::advance) against an EXPLICIT
    /// duration — Phase 5 timeline mode plays over the timeline's extent,
    /// not the loaded media's (`duration_us` stays media session state).
    pub fn advance_with_duration(&mut self, dt_us: i64, duration_us: i64) {
        if !self.playing || dt_us <= 0 {
            return;
        }
        let next = self.position_us.saturating_add(dt_us);
        if next < duration_us {
            self.position_us = next;
        } else if self.looping && duration_us > 0 {
            self.position_us = next % duration_us;
        } else {
            self.position_us = duration_us.max(0);
            self.playing = false;
        }
    }
}

/// What kind of media a bin item fundamentally is (classified by the real
/// ffprobe-based probe at import time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

/// Shared track/media compatibility rule (Phase quick-260726-52r): the ONE
/// place both `place_clip` (`app_core::place::run_place_clip`) and `Command::MoveClipToTrack`
/// (crates/core) check before letting a clip land on a track. A video
/// track accepts video or image media; an audio track accepts audio
/// media, or video media only when it actually has audio to contribute.
pub fn track_accepts_media(track_kind: TrackKind, media_kind: MediaKind, has_audio: bool) -> bool {
    match (track_kind, media_kind) {
        (TrackKind::Video, MediaKind::Video) => true,
        (TrackKind::Video, MediaKind::Image) => true,
        (TrackKind::Audio, MediaKind::Audio) => true,
        (TrackKind::Audio, MediaKind::Video) => has_audio,
        _ => false,
    }
}

/// A media file registered in the project bin, carrying metadata NORMALIZED
/// at import (Phase 3 / MEDIA-03): `duration_us` from the CONTAINER (not
/// frame_count x nominal fps), `fps` = the average frame rate (the
/// authoritative working rate), plus `is_vfr` / `rotation_degrees` so
/// downstream preview/export consumers read these fields instead of
/// re-deriving them from raw nominal rates (the classic VFR-drift and
/// rotated-video bugs).
///
/// The core does no I/O: probing happens in the app layer (`crates/app-core` ->
/// `crates/engine`) and the finished item is carried by `AddMediaBinItem`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaBinItem {
    /// Stable string id, unique within the media bin.
    pub id: String,
    /// Absolute path on disk (canonicalized at import; not validated by the
    /// core — media I/O is the engine's job).
    pub path: String,
    pub media_kind: MediaKind,
    /// Container duration in microseconds (0 for still images).
    pub duration_us: i64,
    /// Container (unrotated) pixel dimensions; 0 for audio-only items.
    pub width: u32,
    pub height: u32,
    /// Average frame rate — the normalized working rate. 0.0 for audio/image.
    pub fps: f64,
    /// True when the source is genuinely variable frame rate.
    pub is_vfr: bool,
    /// Display rotation captured at import, normalized to {0, 90, 180, 270}.
    pub rotation_degrees: u32,
    pub has_audio: bool,
    /// Absolute path of the generated poster image in the app cache dir
    /// (video/image items); `None` for audio-only items, which the renderer
    /// draws as a distinct audio tile.
    pub poster_path: Option<String>,
    /// Phase 25 (LIB-01): canonical library-folder path this item sits
    /// directly in. "" = root (the default). Segments joined by "/", no
    /// leading/trailing slash (e.g. "broll/city"). #[serde(default)] keeps
    /// every pre-Phase-25 fixture (no "folder" key) loadable at the root
    /// default.
    #[serde(default)]
    pub folder: String,
    /// Phase 25 (LIB-01): display-name override (organize_media's
    /// rename_media target). `None` (default): the UI derives the name from
    /// basename(path) exactly as it does today. `Some(name)`: overrides
    /// display ONLY -- never touches the real file at `path` (the core does
    /// zero I/O). #[serde(default)] keeps every pre-Phase-25 fixture loadable.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Phase 28 (OVL-01): true when this item is an imported numbered image
    /// sequence — its `path` is a confined image2 `%0Nd` pattern (e.g.
    /// `.../frame_%04d.png`), not a single media file. UI/plumbing hint; the
    /// composite/export decode path branches on this flag to decode via
    /// `engine::decode_frame_rgba_at_seq` at `fps` (the project fps baked at
    /// import) instead of the ordinary single-file decoder. `#[serde(default)]`
    /// keeps every pre-Phase-28 fixture loadable (default false).
    #[serde(default)]
    pub is_image_sequence: bool,
    /// Phase 60 (OCCL-01): whether this media's SOURCE can carry real per-pixel
    /// transparency, captured at import from the ffprobe metadata
    /// (`engine::MediaInfo::has_alpha` — an alpha-carrying pixel format, or the
    /// WebM `alpha_mode` side-channel tag VP9 needs). **TRI-STATE, and the third
    /// state is the important one:**
    ///
    /// | value | meaning | occluder-eligible? |
    /// |---|---|---|
    /// | `None` | UNKNOWN — a pre-Phase-60 project file, a stale probe-cache entry, an image sequence, or media whose probe answered nothing | **never** |
    /// | `Some(true)` | probed: the source carries a real alpha channel (VP9-alpha, RGBA PNG/ProRes 4444, ...) | never |
    /// | `Some(false)` | probed: no alpha channel | yes, IF the geometry rungs also pass |
    ///
    /// Read it through [`MediaBinItem::source_alpha`], never by comparing the
    /// `Option` by hand: [`SourceAlpha`] makes "unknown" a state a caller has to
    /// answer for explicitly, which is the whole safety property here. A
    /// confidently wrong `Some(false)` culls a layer that really does paint
    /// transparent pixels, and the culled frame then differs from the un-culled
    /// one — OCCL-01's exact failure mode. So the import path narrows to `None`
    /// whenever the probe had nothing to read, rather than guessing opaque.
    ///
    /// `#[serde(default)]` keeps every pre-Phase-60 project file loadable, at
    /// the conservative `None`. Note the asymmetry with the probe CACHE, whose
    /// equivalent field deliberately refuses a serde default and bumps its
    /// version instead: a project file's `None` is re-derived on the next import
    /// of that source, whereas a cache HIT would have pinned it forever.
    #[serde(default)]
    pub reports_alpha: Option<bool>,
}

/// Phase 60 (OCCL-01): the tri-state reading of
/// [`MediaBinItem::reports_alpha`], and the ONE place the "could this layer be
/// hiding what is behind it?" question is answered from source metadata.
///
/// This exists as an enum rather than as bare `Option<bool>` comparisons for a
/// single reason: **the unknown case must be hard to get wrong.** An
/// `Option<bool>` invites `item.reports_alpha.unwrap_or(false)` and similar
/// shapes that silently fold "we never probed this" into "it is opaque".
/// [`Self::is_proven_opaque`] instead matches all three states exhaustively with
/// no wildcard arm, so a state can only ever be treated as cullable by someone
/// writing that verdict out by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceAlpha {
    /// Nothing is known about this source's alpha: a pre-Phase-60 project file,
    /// an image sequence (a `%0Nd` pattern is not one probeable container), an
    /// audio-only item, or a probe that returned no pixel format. NEVER
    /// occluder-eligible — an unprobed source is not an opaque source.
    Unknown,
    /// Probed, and the source carries no per-pixel alpha channel. The only
    /// state the occlusion predicate may cull behind, and only then if the
    /// geometry rungs (identity transform/crop, zero rotation, opacity exactly
    /// 1.0, known nonzero dims, canvas-filling contain-fit) also pass.
    Opaque,
    /// Probed, and the source carries a real alpha channel. Refused outright:
    /// whatever sits underneath may genuinely show through.
    Transparent,
}

impl SourceAlpha {
    /// Read the wire/domain tri-state. Total, and deliberately the only
    /// constructor — there is no way to build a `SourceAlpha` that disagrees
    /// with the stored `Option<bool>`.
    pub fn from_reports_alpha(reports_alpha: Option<bool>) -> Self {
        match reports_alpha {
            None => SourceAlpha::Unknown,
            Some(false) => SourceAlpha::Opaque,
            Some(true) => SourceAlpha::Transparent,
        }
    }

    /// `true` only when a real probe positively established that this source
    /// has no alpha channel.
    ///
    /// The match below is EXHAUSTIVE ON PURPOSE and must stay that way: a
    /// wildcard arm would let a future fourth state inherit a verdict nobody
    /// chose for it, and a wrong verdict here is a wrong PIXEL. Pinned by
    /// `core/tests/source_alpha.rs::is_proven_opaque_has_no_wildcard_arm`,
    /// which reads this function's own source text.
    pub fn is_proven_opaque(self) -> bool {
        match self {
            SourceAlpha::Opaque => true,
            SourceAlpha::Unknown => false,
            SourceAlpha::Transparent => false,
        }
    }
}

impl MediaBinItem {
    /// [`Self::reports_alpha`] as the tri-state its consumers must reason
    /// about. Prefer this over touching the `Option` directly.
    pub fn source_alpha(&self) -> SourceAlpha {
        SourceAlpha::from_reports_alpha(self.reports_alpha)
    }
}
