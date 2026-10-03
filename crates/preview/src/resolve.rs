//! Preview resolution (plan 46-06): WHAT the active monitor decodes at a
//! position, and WHICH audio contributors the preview mix sums there.
//!
//! Moved out of `src-tauri/src/native_surface.rs`. Every body below is the one
//! that lived there, line for line; the ONLY edit is the accessor at the top of
//! each store-reading function:
//!
//! ```text
//! let store = app.try_state::<crate::SharedStore>()?;   ->  let guard = host.store()?;
//! let guard = store.lock().ok()?;
//! ```
//!
//! # Why this is the wave that matters
//!
//! [`crate::PreviewHost::store`] was defined by plan 46-02 and implemented by
//! 46-03, but nothing called it. These six items are its FIRST real consumers,
//! so this file is where the port stops being a design and starts being load
//! bearing.
//!
//! `store()` returns `None` when the store is unmanaged (a mock runtime that
//! never ran `setup()`) or poisoned — exactly the two cases today's
//! `app.try_state::<SharedStore>()?` / `.lock().ok()?` pair folds into `None`.
//! Every function here therefore keeps treating `None` as "nothing resolves
//! here": `None`, `Vec::new()`, or `false`. There is no new panic path, and no
//! default is substituted for a missing store.
//!
//! # Lock discipline (unchanged, deliberately)
//!
//! The guard is taken as late and held as briefly as it is today:
//!
//! - [`resolve_active`] takes it once and drops it at return.
//! - [`resolve_audio_mix`]'s Program branch takes it and hands `&guard` straight
//!   to [`program_mix_sources`], which is pure and re-enters nothing.
//! - [`resolve_program_audio`] calls [`resolve_active`] FIRST (that call takes
//!   and releases the lock on its own), and only its gap arm takes a guard of
//!   its own — never one held across a call that could re-enter the store. This
//!   is the same take/release order the shell had, so a lock cycle is
//!   unreachable by construction rather than by inspection.

use std::path::PathBuf;

use rudis_core::MediaKind;

use crate::{clip_covers, PreviewHost};

/// What to decode for the active preview monitor at a given position.
///
/// 18.3-02: `pub(crate)` (with `pub(crate)` fields) so the `preview_ring`
/// producer thread can resolve + read the active single-layer clip. Visibility
/// only — the shape and `resolve_active`'s body are unchanged.
///
/// Phase 46 (plan 46-06): now plain `pub`, because the producer thread reading
/// it lives in the shell crate on the far side of a crate boundary. The shape is
/// still unchanged.
pub struct Resolved {
    pub path: PathBuf,
    /// Position within the SOURCE media to decode/stream from.
    pub source_us: i64,
    pub rotation: u32,
    pub fps: f64,
    /// The active timeline clip id (Program mode) for boundary detection; `None`
    /// in Source mode.
    pub clip_id: Option<String>,
    /// Whether the underlying media has an audio stream at all.
    pub has_audio: bool,
    /// The media's stored display dimensions (`MediaBinItem.width`/`height`).
    /// Phase 48 (plan 48-09): the GPU decode gate needs a frame-size ESTIMATE
    /// before any decoder is open (the provisional `gpu_ring_depth` — Pitfall
    /// 4's ordering), and the imported item already carries the probed dims.
    /// Zero when the import never probed them; the consumer must `.max(1)`.
    pub width: u32,
    pub height: u32,
    /// Audio gain for the active clip (1.0 in Source mode).
    pub volume: f32,
    /// Source end position for the audio range: the active clip's `out_us`
    /// (Program) or the media duration (Source). Bounds `render_audio_pcm`.
    pub audio_end_us: i64,
    /// The active clip's time remap (quick task 260730-x2t), `None` in Source
    /// mode and for an un-retimed clip. Carried here because the PREVIEW ring
    /// stamps decoded frames through `pts_map::map_source_pts_to_timeline`,
    /// which needs the INVERSE of the very same speed integral export resolves
    /// through — a preview that stamped 1:1 under retime would drift against a
    /// correct export (SR-2).
    pub retime: Option<rudis_core::Retime>,
    /// TIMELINE µs remaining in the resolved clip FROM the position this
    /// `Resolved` was resolved AT (quick task 260730-x2t, WR-06).
    ///
    /// Derived from the clip's precomputed retimed occupancy
    /// (`timeline_end_us() - position_us`, O(1) and allocation-free), NOT by
    /// re-integrating the speed curve from its ORIGIN. `retimed_timeline_len_us`
    /// answers "how much timeline, starting at clip-relative **0**, consumes
    /// this much source" — and the playhead is generally NOT at clip-relative 0.
    /// For a `Constant` curve the two agree; for a RAMP the origin-based form
    /// applies the curve's EARLY rates to the clip's LATE source, so it is wrong
    /// in whichever direction the ramp leans. That sized the decode session and
    /// the audio mix window, ending a ramped clip's session early (a visible
    /// stall at the clip tail as the ring starves) or late (decoding past the
    /// clip).
    pub remaining_timeline_us: i64,
}

/// Resolve what the ACTIVE monitor shows at `position_us`:
///   * Source mode → the loaded MediaBin clip, decoded straight from
///     `position_us` (the source playhead).
///   * Program mode → `top_video_active_at` on the timeline → the active clip's
///     source at the mapped `source_us` (or `None` in a gap / empty timeline).
/// Occasional store lock (session start / boundary re-check), NOT per frame.
/// 18.3-02: the `preview_ring` producer resolves the active single-layer clip
/// per iteration (body unchanged).
pub fn resolve_active(
    host: &dyn PreviewHost,
    is_source: bool,
    position_us: i64,
) -> Option<Resolved> {
    let guard = host.store()?;
    if is_source {
        let media_id = guard.source_playback().loaded_media_id.clone()?;
        let item = guard.media_item(&media_id)?;
        if item.media_kind != MediaKind::Video {
            return None;
        }
        Some(Resolved {
            path: PathBuf::from(&item.path),
            source_us: position_us,
            rotation: item.rotation_degrees,
            fps: item.fps,
            clip_id: None,
            has_audio: item.has_audio,
            width: item.width,
            height: item.height,
            volume: 1.0,
            audio_end_us: item.duration_us,
            // Source mode previews a RAW MediaBin item — there is no clip and
            // therefore no retime.
            retime: None,
            // Source mode is 1:1 by construction: the remaining timeline extent
            // IS the remaining media (WR-06).
            remaining_timeline_us: (item.duration_us - position_us).max(0),
        })
    } else {
        let hit = guard.timeline().top_video_active_at(position_us)?;
        let item = guard.media_item(&hit.media_id)?;
        if item.media_kind != MediaKind::Video {
            return None;
        }
        // The active clip carries the volume / out-point the audio range needs.
        // `top_video_active_at` only returns the resolved hit, so look the clip
        // itself up by id (same store guard, not per-frame). Detach handling now
        // lives in `resolve_audio_mix` via `Timeline::audio_contributors`.
        let clip = guard
            .timeline()
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .find(|c| c.id == hit.clip_id);
        // `audio_end_us` is a SOURCE time; the caller turns it into a timeline
        // extent via `audio_end_us - source_us`, which is only a 1:1 identity
        // when the clip is un-retimed. Under retime the SOURCE end is still
        // `out_us` (correct: that IS where the clip's content stops), and the
        // TIMELINE extent is derived from `retime` by the callers below —
        // never by subtracting source times (260730-x2t).
        let (volume, audio_end_us, retime, remaining_timeline_us) = match clip {
            Some(c) => (
                c.volume,
                c.out_us,
                c.retime.clone(),
                // WR-06: the clip's PRECOMPUTED retimed occupancy, measured
                // FROM the resolving position. O(1), and correct at every point
                // inside a ramp — unlike re-integrating the curve from its
                // origin, which describes the clip's EARLY rates, not the
                // source that is actually left.
                (c.timeline_end_us() - position_us).max(0),
            ),
            None => (1.0, hit.source_us, None, 0),
        };
        Some(Resolved {
            path: PathBuf::from(&item.path),
            source_us: hit.source_us,
            rotation: item.rotation_degrees,
            fps: item.fps,
            clip_id: Some(hit.clip_id),
            has_audio: item.has_audio,
            width: item.width,
            height: item.height,
            volume,
            audio_end_us,
            retime,
            remaining_timeline_us,
        })
    }
}

/// Gather the audio contributors (as [`engine::MixSource`]) overlapping the
/// current session's timeline range `[tl_start, tl_end)` for
/// [`engine::AudioOutput::start_mix`]. Program mode: the timeline's
/// `audio_contributors()` (audio-track clips + non-detached video clips), each
/// filtered to a real audio stream, non-zero volume, and range overlap — so a
/// DETACHED clip's audio (now living on an audio track) is mixed in instead of
/// being dropped. Source mode: just the loaded clip's own audio (no
/// timeline/detach concept). Same brief store-lock discipline as
/// [`resolve_active`]. Returns `[]` when nothing is audible → the caller keeps
/// video-only wall-clock pacing.
pub fn resolve_audio_mix(
    host: &dyn PreviewHost,
    is_source: bool,
    tl_start: i64,
    tl_end: i64,
    r: &Resolved,
) -> Vec<engine::MixSource> {
    if is_source {
        if r.has_audio && r.volume > 0.0 && tl_end > tl_start {
            // Source mode is never retimed (no clip): the plain 1:1 window.
            return vec![engine::MixSource::plain(
                r.path.clone(),
                tl_start,
                r.source_us,
                r.source_us + (tl_end - tl_start),
                r.volume,
            )];
        }
        return Vec::new();
    }
    let Some(guard) = host.store() else {
        return Vec::new();
    };
    program_mix_sources(&guard, tl_start, tl_end)
}

/// The Program-mode contributor scan, factored out of [`resolve_audio_mix`] so
/// callers that ALREADY hold the store guard reuse it under ONE lock (quick-k0q:
/// the gap-window fallback in [`resolve_program_audio`] needs the same sources
/// without fabricating a dummy [`Resolved`]). Body is byte-for-byte the loop
/// that used to live inline: every audible `audio_contributors()` entry
/// overlapping `[tl_start, tl_end)`, at its ABSOLUTE timeline placement.
///
/// Module-private on purpose: nothing outside this file has ever called it, and
/// keeping it private is what stops the crate boundary from widening.
fn program_mix_sources(
    guard: &rudis_core::Store,
    tl_start: i64,
    tl_end: i64,
) -> Vec<engine::MixSource> {
    let mut out = Vec::new();
    for c in guard.timeline().audio_contributors() {
        // Retime (260730-x2t): a contributor's TIMELINE extent is its RETIMED
        // length, not its source span. `contributor_timeline_end` is the one
        // helper every overlap test in this file goes through.
        let c_tl_end = contributor_timeline_end(&c);
        if c.start_us >= tl_end || c_tl_end <= tl_start {
            continue; // no overlap with this session's window
        }
        let Some(item) = guard.media_item(&c.media_id) else {
            continue;
        };
        if !item.has_audio || c.volume <= 0.0 {
            continue; // contributes silence — leave it out
        }
        // ONE MixSource per CONSTANT-TEMPO window, from the same core
        // segmenter the export audio path uses — so live preview and export
        // stretch through the same integral, the same tempo derivation and the
        // same truncation rule. An un-retimed contributor yields exactly one
        // window at tempo 1.0, i.e. today's output unchanged.
        //
        // The ONE deliberate difference is the staircase STEP: preview passes
        // `PREVIEW_RETIME_AUDIO_WINDOW_US`, export keeps the finer
        // `RETIME_AUDIO_WINDOW_US`. Live is a REAL-TIME budget — the mixer
        // spends one ffmpeg render per window per 2 s chunk and the export step
        // costs ~1.9x real time on a ramp, so the producer starved and the mix
        // stuttered (live-UAT bug `retime-live-uat-frontend-mirror-undo-audio`,
        // symptom 3). The resulting content difference is bounded and measured
        // — see that constant's doc and
        // `core/tests/retime.rs::the_preview_staircase_never_moves_audio_a_
        // perceptible_amount_against_the_picture`.
        for w in rudis_core::retime_audio_windows_with(
            &c,
            rudis_core::PREVIEW_RETIME_AUDIO_WINDOW_US,
        ) {
            out.push(engine::MixSource {
                path: PathBuf::from(&item.path),
                timeline_start_us: w.timeline_start_us,
                in_us: w.in_us,
                out_us: w.out_us,
                volume: c.volume,
                tempo: w.tempo,
                out_len_us: w.timeline_len_us,
            });
        }
    }
    out
}

/// A contributor's exclusive TIMELINE end. Pre-retime this was
/// `start_us + (out_us - in_us)`; under retime the timeline extent is the
/// RETIMED length (quick task 260730-x2t). ONE helper so no overlap test in
/// this module can drift back to the source-span form.
fn contributor_timeline_end(c: &rudis_core::AudioContributor) -> i64 {
    match c.retime.as_ref() {
        Some(r) => c.start_us + r.timeline_len_us,
        None => c.start_us + (c.out_us - c.in_us),
    }
}

/// PLAY-08 (plan 57-05): how far past the playhead [`resolve_program_audio`]
/// may look ahead when it sizes ONE preview audio mix, in microseconds.
///
/// **120 s, chosen against a MEASURED cost, not a guess** (57-RESEARCH Open
/// Question 3). `resolve_program_audio`'s own measurement test
/// (`play08_window_resolve_cost_is_measured_not_assumed`) resolves three
/// shapes on this machine, debug profile:
///
/// | shape | window | contributors | median resolve | max |
/// |---|---|---|---|---|
/// | F3 `dense_cut` (12 x 5 s), pos 1 s | 59 s | 12 | **12 µs** | 28 µs |
/// | uncut 10-minute single clip | 600 s | 1 | 2 µs | 2 µs |
/// | 10 min of 5 s cuts (120 clips) | **120 s (capped)** | 24 | 53 µs | 91 µs |
///
/// Two orders of magnitude under the plan's 5 ms budget, so the cap is NOT
/// sized by scan cost — `program_mix_sources` walks `audio_contributors()` in
/// FULL whatever the window is (the window only decides which entries survive
/// the overlap test), so widening the window does not widen the scan at all.
/// What the cap actually bounds is the **returned `MixSource` count**: every
/// contributor in the window is carried into `AudioOutput::start_mix`, which
/// re-tests all of them per 2 s chunk, and a RETIMED contributor fans out one
/// `MixSource` per `PREVIEW_RETIME_AUDIO_WINDOW_US` (400 ms) staircase step.
/// 120 s keeps that set small on an arbitrarily long project while being ~24x
/// the 5 s cut spacing BENCH-01's F3 fixture measured the reopen storm on.
pub const AUDIO_WINDOW_CAP_US: i64 = 120_000_000;

/// THE single Program-mode preview mix-resolution seam: given the presentation
/// playhead, return exactly the value set handed to
/// [`engine::AudioOutput::start_mix`] — `(sources, tl_start, tl_end)` — or `None`
/// when this playhead is genuinely silent (no mix, and specifically NO
/// `start_mix` spawn, so a silent gap costs one brief store-lock scan per tick,
/// never an ffmpeg storm).
///
/// Both preview play paths (the streaming play loop and `start_multi_audio`)
/// route through here so they cannot drift apart. Source mode is NOT handled
/// here — it keeps its own `resolve_active` + [`resolve_audio_mix`] path.
///
/// Window semantics:
///   * A VIDEO clip is active at `position_us` → a CAPPED LOOKAHEAD window
///     (PLAY-08, plan 57-05): `[pos, max(clip extent, min(pos +
///     AUDIO_WINDOW_CAP_US, timeline duration)))`. It deliberately reaches
///     PAST the active clip so a clip cut no longer ends the mix span.
///   * No video clip active (quick-k0q / GEN-03 UAT gap) → the window is derived
///     from the AUDIO contributors themselves; see the `None` arm below.
pub fn resolve_program_audio(
    host: &dyn PreviewHost,
    position_us: i64,
) -> Option<(Vec<engine::MixSource>, i64, i64)> {
    match resolve_active(host, false, position_us) {
        Some(r) => {
            let tl_start = position_us;
            // ONE store guard for the whole arm (it was two: `resolve_active`
            // above took and released its own, and `resolve_audio_mix` would
            // take another). `program_mix_sources` is pure and re-enters
            // nothing, so holding across it is the SAME discipline the gap arm
            // below already uses — see the module doc's lock note.
            let guard = host.store()?;

            // ---- PLAY-08 (plan 57-05): the window is a CAPPED LOOKAHEAD ----
            //
            // It used to be `position_us + r.remaining_timeline_us` — the ONE
            // active clip's remaining extent. That is why **every clip cut in
            // Program mode reopened the OS audio device**: crossing the clip
            // end left `audio_span`, `reuse_audio` said no, and
            // `AudioOutput::start_mix` built a fresh `cpal` stream (0.27-0.39 s,
            // measured in 49.1). BENCH-01 measured the consequence at ~200 ms
            // per cut on a SINGLE-track fixture that never composites
            // (`composite=0`), with 11 cuts x 5 runs = 55 `presentation drift
            // resync` log lines — so this was never a multi-layer-only cost
            // (57-RESEARCH Pitfall 9, confirmed in 57-BENCH-BASELINE.md § 3).
            //
            // Widening costs nothing per frame and needs no contiguity walk:
            //   * `program_mix_sources` already gathers ALL contributors
            //     overlapping an ARBITRARY window in one scan — and that scan
            //     walks the full contributor list either way, so a wider window
            //     does not make it wider (measured: see AUDIO_WINDOW_CAP_US).
            //   * `AudioOutput::start_mix`'s producer renders progressively in
            //     2 s `CHUNK_US` windows with a 3-window bounded channel,
            //     whatever the nominal range is — so a 60 s window buffers no
            //     more audio than a 5 s one.
            //   * A genuine GAP inside the window is simply a stretch no
            //     contributor overlaps; the mixer emits a silent buffer for it
            //     (it sends one per chunk, silent ones included), which is
            //     correct AND keeps the audio clock advancing across the gap.
            //
            // The cap is a CEILING, never a floor: `max` with the active clip's
            // own extent. A bare `min(pos + cap, duration)` would SHRINK the
            // window for any clip longer than the cap and introduce a mid-clip
            // reopen HEAD does not have — a new stall of exactly the class this
            // change removes. Clamping to the timeline duration only avoids
            // rendering silent chunks past the end of the project.
            //
            // Seek/flush is untouched: `present_loop` already drops `audio` and
            // `audio_span` on a reposition (present_loop.rs:299-304) and on an
            // edit that touches the playhead's audio (:405-422), so a widened
            // window can neither outlive a seek nor swallow an edit.
            let clip_extent_end = position_us + r.remaining_timeline_us;
            let capped_lookahead = position_us
                .saturating_add(AUDIO_WINDOW_CAP_US)
                .min(guard.timeline().duration_us());
            let tl_end = clip_extent_end.max(capped_lookahead);
            if tl_end <= tl_start {
                return None;
            }
            let sources = program_mix_sources(&guard, tl_start, tl_end);
            if sources.is_empty() {
                return None;
            }
            Some((sources, tl_start, tl_end))
        }
        None => {
            // quick-k0q (GEN-03 UAT gap): no VIDEO clip under the playhead.
            // Export mixes `audio_contributors()` INDEPENDENTLY of video, so
            // preview must too (WYSIWYG, CLAUDE.md rule 3) — derive the window
            // from the audible contributors that COVER the playhead instead of
            // from a video clip that isn't there.
            let guard = host.store()?;
            let tl_start = position_us;
            let mut tl_end = tl_start;
            for c in guard.timeline().audio_contributors() {
                let c_tl_end = contributor_timeline_end(&c);
                if c.start_us > position_us || position_us >= c_tl_end {
                    continue; // does not cover the playhead
                }
                let Some(item) = guard.media_item(&c.media_id) else {
                    continue;
                };
                if !item.has_audio || c.volume <= 0.0 {
                    continue; // contributes silence — never widens the window
                }
                tl_end = tl_end.max(c_tl_end);
            }
            if tl_end <= tl_start {
                // Genuine silence: no mix AND no `AudioOutput::start_mix`
                // spawn (T-k0q-01 — same per-tick cost class as the old `None`
                // arm: one brief store-lock scan, never an ffmpeg storm).
                return None;
            }
            // Same overlap semantics the video-window path uses: contributors
            // starting mid-window join at their absolute placement.
            let sources = program_mix_sources(&guard, tl_start, tl_end);
            if sources.is_empty() {
                return None;
            }
            Some((sources, tl_start, tl_end))
        }
    }
}

/// True iff a mid-play edit changed the AUDIBLE mix at the playhead (18.3-03
/// regression #3). The preview audio mix SUMS every contributor overlapping the
/// playhead, so an edit to ANY clip whose timeline span covers `position_us` —
/// the visible clip's own audio, OR a detached audio-track clip, OR another
/// track's audible clip — must force an audio rebuild EVEN WITHOUT a reposition;
/// otherwise Stage-0 `reuse_audio` (repositioned=false) keeps the stale mix
/// alive until a genuine seek (the pause→play the user had to do). A structural
/// change may have removed an audible clip → rebuild. Source mode: a timeline
/// edit never touches the previewed MediaBin item's own audio. This is
/// INDEPENDENT of the video-ring flush: a non-overlapping cross-track edit
/// rebuilds neither audio nor video (the Issue-B win is preserved).
pub fn edit_touches_playhead_audio(
    host: &dyn PreviewHost,
    changed_ids: &[String],
    position_us: i64,
    is_source: bool,
    structural: bool,
) -> bool {
    if is_source {
        return false;
    }
    if structural {
        return true;
    }
    if changed_ids.is_empty() {
        return false;
    }
    let Some(guard) = host.store() else {
        return false;
    };
    guard
        .timeline()
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .any(|c| {
            changed_ids.iter().any(|id| id == &c.id)
                // Retime-aware (260730-x2t): the clip's TIMELINE occupancy,
                // not its source span.
                && clip_covers(c, position_us)
        })
}

#[cfg(test)]
mod tests {
    //! Quick task 260730-x2t, Task 6: the PREVIEW audio half of retime parity.
    //!
    //! `program_mix_sources` is module-private, so these tests live here (the
    //! file's own style) and drive it against a REAL `rudis_core::Store` built
    //! through real commands — no `PreviewHost` mock needed.

    use super::*;
    use rudis_core::{
        Clip, Command, MediaBinItem, MediaKind, Retime, RetimeCurve, Store,
    };

    const FPS: f64 = 30.0;

    fn media(id: &str, duration_us: i64) -> MediaBinItem {
        MediaBinItem {
            id: id.into(),
            path: format!("test-media/{id}.mp4"),
            media_kind: MediaKind::Video,
            duration_us,
            width: 1280,
            height: 720,
            fps: FPS,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: true,
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
            transform: Default::default(),
            opacity: 1.0,
            crop: Default::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        }
    }

    /// One 8 s media + a 4 s clip at timeline 0 on the VIDEO track.
    fn store_with_clip() -> Store {
        let mut store = Store::new();
        store
            .dispatch(Command::AddMediaBinItem(media("m1", 8_000_000)))
            .expect("media");
        store
            .dispatch(Command::AddClip {
                track: 0,
                clip: clip("clip-1", "m1", 0, 0, 4_000_000),
            })
            .expect("clip");
        store
    }

    fn set_retime(store: &mut Store, id: &str, curve: RetimeCurve) {
        store
            .dispatch(Command::SetClipRetime {
                id: id.into(),
                retime: Some(Retime {
                    curve,
                    timeline_len_us: 0,
                    timebase_fps: FPS,
                }),
            })
            .expect("set retime");
    }

    #[test]
    fn an_unretimed_contributor_yields_exactly_one_unit_tempo_mix_source() {
        let store = store_with_clip();
        let out = program_mix_sources(&store, 0, 10_000_000);
        assert_eq!(out.len(), 1, "one window, exactly as before retime existed");
        assert_eq!(out[0].tempo, 1.0);
        assert_eq!(out[0].in_us, 0);
        assert_eq!(out[0].out_us, 4_000_000);
        assert_eq!(out[0].out_len_us, 4_000_000);
        assert_eq!(out[0].timeline_start_us, 0);
    }

    #[test]
    fn a_constant_speed_contributor_is_one_window_carrying_the_tempo() {
        let mut store = store_with_clip();
        set_retime(&mut store, "clip-1", RetimeCurve::Constant(2.0));
        let out = program_mix_sources(&store, 0, 10_000_000);
        assert_eq!(
            out.len(),
            1,
            "a constant speed is linear over the whole clip — staircasing it \
             would buy nothing but ffmpeg spawns"
        );
        assert_eq!(out[0].tempo, 2.0);
        assert_eq!(
            out[0].out_len_us, 2_000_000,
            "the window must occupy the RETIMED timeline length"
        );
        assert_eq!((out[0].in_us, out[0].out_us), (0, 4_000_000));
    }

    #[test]
    fn a_ramped_contributor_is_staircased_into_constant_tempo_windows() {
        use rudis_core::{Interpolation, Keyframe};
        let mut store = store_with_clip();
        set_retime(
            &mut store,
            "clip-1",
            RetimeCurve::Ramp(vec![
                Keyframe {
                    frame: 0,
                    value: 1.0,
                    interp: Interpolation::Linear,
                },
                Keyframe {
                    frame: 120,
                    value: 2.0,
                    interp: Interpolation::Linear,
                },
            ]),
        );
        let out = program_mix_sources(&store, 0, 10_000_000);
        assert!(out.len() > 5, "a ramp must be staircased, got {} windows", out.len());
        // Every window is inside the legal tempo range, contiguous in TIMELINE
        // and non-overlapping in SOURCE — the two properties that make the
        // staircase a faithful approximation rather than a gap generator.
        let mut expect_start = 0i64;
        let mut expect_src = 0i64;
        for w in &out {
            assert!(
                w.tempo >= rudis_core::MIN_SPEED && w.tempo <= rudis_core::MAX_SPEED,
                "window tempo {} out of the validated range",
                w.tempo
            );
            assert_eq!(
                w.timeline_start_us, expect_start,
                "windows must TILE the timeline with no gap"
            );
            assert!(
                w.in_us >= expect_src,
                "windows must not re-read source they already consumed"
            );
            expect_start += w.out_len_us;
            expect_src = w.out_us;
        }
        let total: i64 = out.iter().map(|w| w.out_len_us).sum();
        let clip_len = store.timeline().tracks[0].clips[0].timeline_len_us();
        assert_eq!(
            total, clip_len,
            "the windows must sum to EXACTLY the clip's timeline occupancy"
        );
        assert!(
            out.last().expect("windows").out_us <= 4_000_000,
            "no window may read past the clip's out-point"
        );
    }

    /// LIVE-UAT REGRESSION (`retime-live-uat-frontend-mirror-undo-audio`,
    /// symptom 3): the live mixer must staircase at the PREVIEW step, not the
    /// export one.
    ///
    /// `program_mix_sources` fans out one `MixSource` per window and
    /// `engine::AudioOutput::start_mix` spends one ffmpeg render per window per
    /// 2 s chunk, inside a hard real-time budget. At the export step
    /// (`RETIME_AUDIO_WINDOW_US`, 100 ms) a ramp costs ~4.3 s of sidecar work
    /// per 2 s of audio (MEASURED, bundled binary, `bars_720p30_5s.mp4`) —
    /// the producer starves and the mix stutters. At
    /// `PREVIEW_RETIME_AUDIO_WINDOW_US` (400 ms) the same chunk costs ~0.77 s.
    ///
    /// `core/tests/retime.rs` owns the BUDGET and the deviation bound; this
    /// pins that THIS call site is the one that gets it. Without it, reverting
    /// the argument back to `retime_audio_windows` would fail nothing.
    #[test]
    fn the_live_mix_staircases_at_the_preview_step_not_the_export_one() {
        use rudis_core::{
            Interpolation, Keyframe, PREVIEW_RETIME_AUDIO_WINDOW_US, RETIME_AUDIO_WINDOW_US,
        };
        let mut store = store_with_clip();
        let ramp = RetimeCurve::Ramp(vec![
            Keyframe {
                frame: 0,
                value: 1.0,
                interp: Interpolation::Linear,
            },
            Keyframe {
                frame: 120,
                value: 2.0,
                interp: Interpolation::Linear,
            },
        ]);
        set_retime(&mut store, "clip-1", ramp);
        let out = program_mix_sources(&store, 0, 10_000_000);

        // Every INTERIOR window is exactly one preview step long (the last one
        // is the remainder, and IN-01's absorb rule can lengthen it).
        assert!(out.len() >= 2, "the ramp must actually staircase");
        for w in &out[..out.len() - 1] {
            assert_eq!(
                w.out_len_us, PREVIEW_RETIME_AUDIO_WINDOW_US,
                "the live mixer must staircase at the PREVIEW step"
            );
        }
        // NEGATIVE CONTROL: the export step would have produced strictly more
        // windows for the same contributor — that gap IS the fix.
        let c = &store.timeline().audio_contributors()[0];
        let exported = rudis_core::retime_audio_windows_with(c, RETIME_AUDIO_WINDOW_US);
        assert!(
            exported.len() > out.len(),
            "export must stay the FINE staircase ({} vs {} live)",
            exported.len(),
            out.len()
        );
    }

    #[test]
    fn a_retimed_contributor_that_ends_before_the_window_is_excluded() {
        // At 2x the 4 s clip occupies [0, 2 s). A session window starting at
        // 2.5 s must NOT pick it up — with the pre-retime source-span extent it
        // would have looked like it ran to 4 s and been mixed in wrongly.
        let mut store = store_with_clip();
        set_retime(&mut store, "clip-1", RetimeCurve::Constant(2.0));
        let out = program_mix_sources(&store, 2_500_000, 5_000_000);
        assert!(
            out.is_empty(),
            "a 2x clip ends at 2 s — it must not contribute to a window that \
             starts at 2.5 s (its SOURCE span would have said 4 s)"
        );
        // ...and it IS picked up for a window it really covers.
        assert_eq!(program_mix_sources(&store, 0, 1_000_000).len(), 1);
    }

    #[test]
    fn contributor_timeline_end_follows_the_retimed_occupancy() {
        let mut store = store_with_clip();
        let before = store.timeline().audio_contributors();
        assert_eq!(contributor_timeline_end(&before[0]), 4_000_000);
        set_retime(&mut store, "clip-1", RetimeCurve::Constant(2.0));
        let after = store.timeline().audio_contributors();
        assert_eq!(contributor_timeline_end(&after[0]), 2_000_000);
    }

    // -----------------------------------------------------------------------
    // WR-06: "remaining timeline time" is measured FROM THE PLAYHEAD
    // -----------------------------------------------------------------------

    /// The minimum [`PreviewHost`] `resolve_active` needs: a store and a
    /// mirror. Everything else is unreachable from these tests.
    struct ResolveHost {
        store: std::sync::Mutex<Store>,
        mirror: crate::PlaybackMirror,
    }

    impl ResolveHost {
        fn new(store: Store) -> Self {
            Self {
                store: std::sync::Mutex::new(store),
                mirror: crate::PlaybackMirror::new(),
            }
        }
    }

    impl PreviewHost for ResolveHost {
        fn store(&self) -> Option<std::sync::MutexGuard<'_, Store>> {
            self.store.lock().ok()
        }
        fn playback_mirror(&self) -> &crate::PlaybackMirror {
            &self.mirror
        }
        fn resolve_overlay(&self) -> Vec<(rudis_core::Annotation, f32)> {
            Vec::new()
        }
        fn live_gesture(&self) -> Vec<(f32, f32)> {
            Vec::new()
        }
        fn overlay_ink(&self) -> [u8; 4] {
            [0, 0, 0, 0xFF]
        }
        fn draw_ink(
            &self,
            _frame: &mut engine::Frame,
            _annotations: &[rudis_core::Annotation],
            _ink: [u8; 4],
            _dashed: bool,
        ) {
        }
        fn rasterize_text(
            &self,
            _text: &rudis_core::TextPayload,
            _transform: engine::LayerTransform,
            _opacity: f32,
            _crop: engine::LayerCrop,
            _project_w: u32,
            _project_h: u32,
        ) -> engine::Layer {
            unimplemented!("these timelines carry no text layers")
        }
        fn emit_canvas_viewport(&self, _w: u32, _h: u32, _fw: u32, _fh: u32) {}
    }

    /// The formula `clip_remaining_timeline_us` / `resolve_program_audio` used
    /// BEFORE WR-06: convert the remaining SOURCE through the inverse integral,
    /// which starts at clip-relative **0**. Kept here as the negative control —
    /// the test must PROVE the two answers differ for a ramp, or it would pass
    /// vacuously.
    fn origin_integrated_remaining(r: &Resolved) -> i64 {
        let src_remaining = (r.audio_end_us - r.source_us).max(0);
        match r.retime.as_ref() {
            Some(rt) => {
                rudis_core::retimed_timeline_len_us(&rt.curve, rt.timebase_fps, src_remaining)
            }
            None => src_remaining,
        }
    }

    /// A steep 0.25 -> 4.0 ramp, so the curve's EARLY rates describe the LATE
    /// source as badly as the legal speed range allows.
    fn steep_ramp() -> RetimeCurve {
        use rudis_core::{Interpolation, Keyframe};
        RetimeCurve::Ramp(vec![
            Keyframe {
                frame: 0,
                value: 0.25,
                interp: Interpolation::Linear,
            },
            Keyframe {
                frame: 120,
                value: 4.0,
                interp: Interpolation::Linear,
            },
        ])
    }

    #[test]
    fn remaining_timeline_is_measured_from_the_playhead_not_the_curve_origin() {
        let mut store = store_with_clip();
        set_retime(&mut store, "clip-1", steep_ramp());
        let clip = store.timeline().tracks[0].clips[0].clone();
        let end = clip.timeline_end_us();
        assert!(end > 0);

        let host = ResolveHost::new(store);

        // 80 % into the clip's own TIMELINE occupancy — well inside the ramp,
        // where the curve's early rates no longer describe what is left.
        let position_us = clip.start_us + (end - clip.start_us) * 4 / 5;
        let r = resolve_active(&host, false, position_us).expect("the clip resolves");

        assert_eq!(
            r.remaining_timeline_us,
            end - position_us,
            "WR-06: the remaining TIMELINE extent must come from the clip's \
             precomputed retimed occupancy measured FROM the playhead"
        );

        // The negative control: the origin-integrated form must be materially
        // WRONG here, or this test proves nothing.
        let origin = origin_integrated_remaining(&r);
        let correct = end - position_us;
        assert!(
            (origin - correct).abs() > correct / 4,
            "the control must discriminate: origin-integrated {origin} us vs \
             the true {correct} us remaining"
        );

        // ...and the session end derived from it lands exactly on the clip's
        // end, which is what `ring.rs` sizes the decode session with.
        assert_eq!(position_us + r.remaining_timeline_us, end);
    }

    #[test]
    fn remaining_timeline_is_unchanged_for_unretimed_and_constant_speed_clips() {
        // The two cases where the origin-integrated form was already correct
        // must stay byte-identical — WR-06 must not move them.
        for curve in [None, Some(RetimeCurve::Constant(2.0)), Some(RetimeCurve::Constant(0.5))] {
            let mut store = store_with_clip();
            if let Some(c) = curve.clone() {
                set_retime(&mut store, "clip-1", c);
            }
            let clip = store.timeline().tracks[0].clips[0].clone();
            let end = clip.timeline_end_us();
            let host = ResolveHost::new(store);

            for frac in [0, 1, 2, 3, 4] {
                let position_us = end * frac / 5;
                let r = resolve_active(&host, false, position_us).expect("resolves");
                assert_eq!(
                    r.remaining_timeline_us,
                    end - position_us,
                    "{curve:?} at {position_us}: remaining must reach the clip end"
                );
                assert_eq!(
                    r.remaining_timeline_us,
                    origin_integrated_remaining(&r),
                    "{curve:?} at {position_us}: the un-retimed / constant-speed \
                     answer must be UNCHANGED by WR-06"
                );
            }
        }
    }

    #[test]
    fn program_audio_window_ends_at_the_ramped_clip_end() {
        let mut store = store_with_clip();
        set_retime(&mut store, "clip-1", steep_ramp());
        let clip = store.timeline().tracks[0].clips[0].clone();
        let end = clip.timeline_end_us();
        let host = ResolveHost::new(store);

        let position_us = end * 4 / 5;
        let (sources, tl_start, tl_end) =
            resolve_program_audio(&host, position_us).expect("a ramped clip is audible");
        assert_eq!(tl_start, position_us);
        assert_eq!(
            tl_end, end,
            "WR-06: the preview audio mix window must end at the clip's real \
             timeline end, not at an origin-integrated guess"
        );
        assert!(!sources.is_empty(), "the mix must carry real windows");
    }

    // -----------------------------------------------------------------------
    // PLAY-08 (plan 57-05): the mix window is a CAPPED LOOKAHEAD, not one clip
    //
    // MEASURED, not assumed: BENCH-01's F3 fixture (one video track, twelve
    // contiguous 5 s clips) pays ~200 ms at EVERY cut, and the engine log
    // corroborated it with 55 `presentation drift resync` lines = exactly
    // 11 cuts x 5 runs (57-BENCH-BASELINE.md § 3). The cause is entirely in
    // the window formula below: `tl_end` came from the ONE active clip, so
    // crossing a cut ended the mix span and `AudioOutput::start_mix` reopened
    // the OS audio device (0.27-0.39 s, measured in 49.1).
    // -----------------------------------------------------------------------

    /// A single-video-track store whose clips are `(timeline_start, in, out)`,
    /// all drawn from one `media_duration_us`-long audible media.
    fn store_with_clips(media_duration_us: i64, spans: &[(i64, i64, i64)]) -> Store {
        let mut store = Store::new();
        store
            .dispatch(Command::AddMediaBinItem(media("m1", media_duration_us)))
            .expect("media");
        for (k, (start_us, in_us, out_us)) in spans.iter().enumerate() {
            store
                .dispatch(Command::AddClip {
                    track: 0,
                    clip: clip(&format!("clip-{k:03}"), "m1", *start_us, *in_us, *out_us),
                })
                .expect("clip");
        }
        store
    }

    /// N contiguous `len_us` clips from timeline 0, source contiguous too —
    /// the F3 `dense_cut_single_track` shape at unit scale.
    fn contiguous_spans(n: i64, len_us: i64) -> Vec<(i64, i64, i64)> {
        (0..n).map(|k| (k * len_us, k * len_us, k * len_us + len_us)).collect()
    }

    /// PLAY-08 behaviour 1: the window CROSSES clip cuts. Three contiguous
    /// 5 s clips (0-5 / 5-10 / 10-15) with the playhead inside the first one:
    /// the mix must span all three, so playing through both cuts reuses ONE
    /// `cpal` stream instead of reopening the device twice.
    #[test]
    fn the_audio_window_crosses_clip_cuts_instead_of_ending_at_the_active_clip() {
        let store = store_with_clips(20_000_000, &contiguous_spans(3, 5_000_000));
        let host = ResolveHost::new(store);

        let (sources, tl_start, tl_end) =
            resolve_program_audio(&host, 1_000_000).expect("a covered playhead is audible");

        assert_eq!(tl_start, 1_000_000, "tl_start is still the playhead");
        assert_eq!(
            tl_end, 15_000_000,
            "PLAY-08: the mix window must reach past BOTH cuts (the old \
             clip-scoped formula ended it at 5_000_000 and reopened the OS \
             audio device at every cut)"
        );
        assert_eq!(
            sources.len(),
            3,
            "every clip inside the widened window contributes at its own \
             absolute timeline placement"
        );
    }

    /// PLAY-08 behaviour 2: a genuine GAP does not end the window early. The
    /// mixer renders silence where nothing overlaps (it sends a buffer for
    /// EVERY chunk, silent ones included), so a gap needs no contiguity walk
    /// and no early cut-off — and the clip AFTER the gap is already in the mix
    /// when the playhead reaches it.
    #[test]
    fn a_gap_inside_the_window_does_not_end_it_early() {
        // clip 0: timeline 0-5 s · GAP 5-8 s · clip 1: timeline 8-15 s
        let store = store_with_clips(
            20_000_000,
            &[(0, 0, 5_000_000), (8_000_000, 5_000_000, 12_000_000)],
        );
        let host = ResolveHost::new(store);

        let (sources, _tl_start, tl_end) =
            resolve_program_audio(&host, 1_000_000).expect("the first clip is audible");

        assert_eq!(
            tl_end, 15_000_000,
            "a gap renders as silence inside the window; it must not truncate it"
        );
        assert_eq!(sources.len(), 2, "both sides of the gap are contributors");
    }

    /// PLAY-08 behaviour 3 (threat T-57-09): the lookahead is BOUNDED. Ten
    /// minutes of uncut coverage (120 contiguous 5 s clips) from position 0
    /// resolves to exactly [`AUDIO_WINDOW_CAP_US`], not to ten minutes — so
    /// neither the contributor set handed to `start_mix` nor the retime
    /// staircase it fans out can grow with project length.
    #[test]
    fn the_window_is_bounded_by_the_lookahead_cap() {
        let store = store_with_clips(700_000_000, &contiguous_spans(120, 5_000_000));
        let host = ResolveHost::new(store);

        let (sources, tl_start, tl_end) =
            resolve_program_audio(&host, 0).expect("the first clip is audible");

        assert_eq!(
            tl_end - tl_start,
            AUDIO_WINDOW_CAP_US,
            "the window must be capped at AUDIO_WINDOW_CAP_US, not run to the \
             10-minute end of contiguous coverage"
        );
        assert_eq!(
            sources.len(),
            (AUDIO_WINDOW_CAP_US / 5_000_000) as usize,
            "the capped window bounds the contributor count too"
        );
    }

    /// PLAY-08 behaviour 4 — the CAP IS A CEILING, NEVER A FLOOR. One
    /// 10-minute clip already gets one mix for its whole extent today; a bare
    /// `min(pos + cap, duration)` would SHRINK that to 120 s and introduce a
    /// mid-clip device reopen where none exists — a new stall of exactly the
    /// class this plan removes. The window is therefore
    /// `max(today's clip extent, capped lookahead)`.
    #[test]
    fn the_window_never_shrinks_below_the_active_clips_own_extent() {
        let store = store_with_clips(700_000_000, &[(0, 0, 600_000_000)]);
        let host = ResolveHost::new(store);

        let (_sources, _tl_start, tl_end) =
            resolve_program_audio(&host, 0).expect("the long clip is audible");

        assert_eq!(
            tl_end, 600_000_000,
            "an uncut 10-minute clip must keep its single whole-clip mix — \
             capping it would ADD a reopen at 120 s that HEAD does not have"
        );
    }

    /// Open Question 3, answered with a MEASUREMENT rather than a guess: what
    /// does the widened window cost per resolve? Prints one machine-greppable
    /// line per shape; the assertion is the plan's own 5 ms budget on the F3
    /// shape (Task 1.2: "if the 120 s window scan exceeds 5 ms on F3, halve
    /// the cap and re-measure").
    ///
    /// Note what the numbers say about WHERE the cost is: `program_mix_sources`
    /// walks `audio_contributors()` in FULL regardless of the window — the
    /// window only decides which entries survive the overlap test — so
    /// widening does not widen the scan. What it grows is the returned
    /// `MixSource` count, which is what `AUDIO_WINDOW_CAP_US` is really
    /// bounding.
    #[test]
    fn play08_window_resolve_cost_is_measured_not_assumed() {
        use std::time::Instant;

        let shapes: Vec<(&str, Store, i64)> = vec![
            (
                "f3_dense_cut_12x5s",
                store_with_clips(70_000_000, &contiguous_spans(12, 5_000_000)),
                1_000_000,
            ),
            (
                "uncut_10min_single_clip",
                store_with_clips(700_000_000, &[(0, 0, 600_000_000)]),
                0,
            ),
            (
                "dense_cut_10min_120x5s",
                store_with_clips(700_000_000, &contiguous_spans(120, 5_000_000)),
                0,
            ),
        ];

        for (name, store, position_us) in shapes {
            let host = ResolveHost::new(store);
            // Warm the allocator/branch predictors, then take the MEDIAN of
            // 21 resolves — a single sample on a debug build is noise.
            for _ in 0..3 {
                let _ = resolve_program_audio(&host, position_us);
            }
            let mut us: Vec<u128> = Vec::new();
            let mut sources = 0usize;
            let mut window_us = 0i64;
            for _ in 0..21 {
                let t0 = Instant::now();
                let got = resolve_program_audio(&host, position_us);
                us.push(t0.elapsed().as_micros());
                let (s, a, b) = got.expect("audible");
                sources = s.len();
                window_us = b - a;
            }
            us.sort_unstable();
            let median_us = us[us.len() / 2];
            println!(
                "PLAY08-SCAN shape={name} window_us={window_us} sources={sources} \
                 median_us={median_us} max_us={}",
                us[us.len() - 1]
            );
            assert!(
                median_us < 5_000,
                "{name}: resolve_program_audio median {median_us}us exceeds the \
                 5ms budget — halve AUDIO_WINDOW_CAP_US and re-measure"
            );
        }
    }
}
