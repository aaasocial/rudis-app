//! Phase 49 (SEEK-02): real-PTS → timeline stamping for preview ring entries.
//!
//! The 2026-07-12 diagnosis (`.planning/debug/preview-vfr-frame-stamp-drift.md`)
//! confirmed that stamping `RingEntry::timeline_us` synthetically as
//! `frame_index × frame_step` makes VFR/frame-dropping media run content-AHEAD
//! of the sample-accurate audio (~250-500ms by 30s), a real preview≠export
//! WYSIWYG gap — export decodes by REAL PTS and is correct; preview lied. This
//! module is the ONE shared mapping both decode paths stamp through:
//!
//! * hardware — `gpu_producer_loop` maps `engine::GpuFrame::pts_us` (absolute
//!   source-media µs, `av_rescale_q` in `HwFrame::pts_us`);
//! * software — `producer_loop` maps the `start_with_pts` showinfo side-channel
//!   records (seek-relative µs, anchored on the first received pts).
//!
//! ONE helper, both paths, per the phase's locked decision: duplicated
//! rescale/offset logic is exactly how the two paths drift apart later, and a
//! fix on only one path is a preview≠preview split no existing test would
//! catch (GPU-05 keeps software decode first-class).
//!
//! # Why the fallback is LOUD
//!
//! On `AV_NOPTS_VALUE` (a missing PTS) or a non-monotonic mapped PTS the stamp
//! falls back to the synthetic producer grid EXPLICITLY — counted in
//! [`SYNTHETIC_STAMP_FALLBACKS`] and logged per frame. A silent fallback would
//! faithfully re-create today's bug and hide it, on precisely the media most
//! likely to trip it (the locked decision's observability requirement). The
//! non-monotonic guard also protects `RingEntry::timeline_us`'s documented
//! "strictly increasing within a gen" invariant from hostile media PTS
//! (threat T-49-05-01): a backward-jumping mapped stamp is never pushed.

use std::sync::atomic::{AtomicU64, Ordering};

/// Count of frames stamped with the synthetic grid because no usable real PTS
/// existed (missing, or non-monotonic after mapping). ALWAYS compiled (the
/// `STREAM_SPAWN_COUNT` precedent) so integration tests can read it via
/// [`synthetic_fallbacks`]; Relaxed ordering — a diagnostic counter, never a
/// synchronization edge.
pub static SYNTHETIC_STAMP_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// The current synthetic-fallback count (see [`SYNTHETIC_STAMP_FALLBACKS`]).
pub fn synthetic_fallbacks() -> u64 {
    SYNTHETIC_STAMP_FALLBACKS.load(Ordering::Relaxed)
}

/// Map an ABSOLUTE source-media PTS onto the timeline: the clip plays its
/// source from `clip_source_us` starting at timeline position
/// `clip_start_pos`, so a frame whose content sits at `source_pts_us` belongs
/// at `clip_start_pos + (source_pts_us - clip_source_us)`.
///
/// Pure integer arithmetic — the same offset mapping the clip's own
/// `source_us`→`start_pos` resolution already uses (49-RESEARCH § Architecture
/// Patterns, the recommended helper verbatim).
///
/// # Retime (quick task 260730-x2t, SR-2)
///
/// `retime` is the clip's time remap, or `None` for an un-retimed clip — in
/// which case this is byte-for-byte the 1:1 mapping above.
///
/// Under retime the 1:1 form is WRONG, and wrong in the most dangerous way this
/// module exists to prevent: preview frames would be stamped at the wrong
/// timeline positions and drift, while EXPORT — which resolves per tick through
/// `active_layers_at` → `Clip::source_offset_at` — stays correct. That is
/// exactly the preview≠export class of
/// `.planning/debug/preview-vfr-frame-stamp-drift.md`, and this module's own
/// non-monotonic guard would SWALLOW the symptom into
/// [`SYNTHETIC_STAMP_FALLBACKS`] rather than failing.
///
/// So the retimed branch applies the INVERSE of the very same forward integral
/// export resolves through — never a second map. The inverse is found by
/// monotone integer bisection over `[0, retime.timeline_len_us]`: the forward
/// map is non-decreasing because speed is validated `> 0`, so bisection is
/// guaranteed convergent. **Cost**: this IS on the preview per-frame path, and
/// it is bounded at `ceil(log2(timeline_len_us)) <= 64` iterations, each a walk
/// of at most `MAX_RETIME_KEYS` (64) segments — a few thousand integer ops per
/// frame, against a 33 ms frame budget.
pub fn map_source_pts_to_timeline(
    source_pts_us: i64,
    clip_source_us: i64,
    clip_start_pos: i64,
    retime: Option<&rudis_core::Retime>,
) -> i64 {
    let Some(r) = retime else {
        return clip_start_pos + (source_pts_us - clip_source_us);
    };
    let want = source_pts_us - clip_source_us;
    if want <= 0 {
        // Before the clip's in-point (an accurate-seek overshoot frame). Map it
        // linearly at the curve's leading rate so the value stays ordered and
        // the monotonicity guard — not this function — owns rejecting it.
        let rate = r.curve.speed_at(0, r.timebase_fps).max(f32::MIN_POSITIVE);
        return clip_start_pos + ((want as f64 / rate as f64) as i64);
    }
    // Smallest t in [0, len] with forward(t) >= want.
    let mut lo = 0i64;
    let mut hi = r.timeline_len_us.max(0);
    if rudis_core::retime_source_offset(&r.curve, r.timebase_fps, hi) < want {
        // Past the clip's own content (a tail frame). Extend linearly at the
        // trailing rate rather than clamping, so stamps stay strictly ordered.
        let rate = r
            .curve
            .speed_at(hi, r.timebase_fps)
            .max(f32::MIN_POSITIVE);
        let over = want - rudis_core::retime_source_offset(&r.curve, r.timebase_fps, hi);
        return clip_start_pos + hi + ((over as f64 / rate as f64) as i64);
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if rudis_core::retime_source_offset(&r.curve, r.timebase_fps, mid) >= want {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    clip_start_pos + lo
}

/// The stamping decision both push sites funnel through: return the mapped
/// real-PTS stamp when it exists AND moves strictly forward of `last_stamp`;
/// otherwise fall back to `synthetic` (the caller's existing `pos`/`prod_pos`
/// grid — the caller guarantees synthetic stamps are strictly increasing),
/// incrementing [`SYNTHETIC_STAMP_FALLBACKS`] and logging the event.
///
/// * `mapped` — the timeline-mapped real PTS, `None` when the frame carried no
///   PTS (`AV_NOPTS_VALUE` / a missing showinfo record).
/// * `synthetic` — the synthetic grid stamp for THIS frame (the pre-SEEK-02
///   stamp source, kept as the production clock).
/// * `last_stamp` — the previous stamp pushed within this session/gen; `None`
///   for the first frame. A mapped value `<=` this is the non-monotonic case
///   (Pitfall 5) and takes the fallback.
/// * `path_label` — `"hw"` or `"sw"`, so the log line names which decode path
///   fell back.
pub fn stamp_or_fallback(
    mapped: Option<i64>,
    synthetic: i64,
    last_stamp: Option<i64>,
    path_label: &str,
) -> i64 {
    if let Some(m) = mapped {
        if last_stamp.is_none_or(|last| m > last) {
            return m;
        }
    }
    let n = SYNTHETIC_STAMP_FALLBACKS.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!(
        "pts_stamp[{path_label}]: no usable PTS (missing or non-monotonic) — synthetic stamp {synthetic}us (total fallbacks {n})"
    );
    synthetic
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The fallback counter is process-global; serialize the tests that
    /// read/compare it so parallel siblings never inflate each other's deltas.
    static COUNTER_SERIAL: Mutex<()> = Mutex::new(());

    fn counter_guard() -> std::sync::MutexGuard<'static, ()> {
        COUNTER_SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- map_source_pts_to_timeline: the pure mapping ----

    #[test]
    fn map_identity_at_clip_origin() {
        // Frame content exactly at the clip's source origin lands exactly at
        // the clip's timeline start.
        assert_eq!(map_source_pts_to_timeline(0, 0, 0, None), 0);
        assert_eq!(
            map_source_pts_to_timeline(2_000_000, 2_000_000, 5_000_000, None),
            5_000_000
        );
    }

    #[test]
    fn map_positive_offsets() {
        // 33_333us into the source of a clip trimmed to source 2s, placed at
        // timeline 1s → timeline 1s + 33_333us.
        assert_eq!(
            map_source_pts_to_timeline(2_033_333, 2_000_000, 1_000_000, None),
            1_033_333
        );
        // Zero clip offsets: pts maps through unchanged.
        assert_eq!(map_source_pts_to_timeline(66_667, 0, 0, None), 66_667);
    }

    #[test]
    fn map_edge_pts_before_clip_source() {
        // A pts BEFORE the clip's source position (an accurate-seek overshoot
        // frame) maps BEHIND start_pos — the mapping is pure arithmetic; the
        // monotonicity guard in stamp_or_fallback owns rejecting it.
        assert_eq!(
            map_source_pts_to_timeline(1_950_000, 2_000_000, 500_000, None),
            450_000
        );
    }

    // ---- stamp_or_fallback: mapped vs fallback decisions ----

    #[test]
    fn monotonic_mapped_pts_is_used_verbatim() {
        let _g = counter_guard();
        let before = synthetic_fallbacks();
        // First frame (no last_stamp): mapped wins.
        assert_eq!(stamp_or_fallback(Some(100), 0, None, "test"), 100);
        // Strictly forward of the last stamp: mapped wins.
        assert_eq!(
            stamp_or_fallback(Some(166_667), 133_332, Some(100_000), "test"),
            166_667
        );
        assert_eq!(
            synthetic_fallbacks(),
            before,
            "usable mapped stamps must not count as fallbacks"
        );
    }

    #[test]
    fn missing_pts_falls_back_and_counts() {
        let _g = counter_guard();
        let before = synthetic_fallbacks();
        assert_eq!(
            stamp_or_fallback(None, 33_333, Some(10_000), "test"),
            33_333
        );
        assert_eq!(
            synthetic_fallbacks(),
            before + 1,
            "a missing PTS must increment the observable counter"
        );
    }

    #[test]
    fn non_monotonic_mapped_pts_falls_back_and_counts() {
        let _g = counter_guard();
        let before = synthetic_fallbacks();
        // Equal to the last stamp: NOT strictly forward → fallback.
        assert_eq!(
            stamp_or_fallback(Some(50_000), 66_666, Some(50_000), "test"),
            66_666
        );
        // Backward jump: fallback.
        assert_eq!(
            stamp_or_fallback(Some(40_000), 99_999, Some(50_000), "test"),
            99_999
        );
        assert_eq!(
            synthetic_fallbacks(),
            before + 2,
            "each non-monotonic mapped PTS must increment the counter"
        );
    }

    #[test]
    fn counter_is_readable_and_monotonic() {
        let _g = counter_guard();
        let a = synthetic_fallbacks();
        let _ = stamp_or_fallback(None, 0, None, "test");
        let b = synthetic_fallbacks();
        assert!(b > a, "synthetic_fallbacks() must observe the increment");
    }

    // -----------------------------------------------------------------------
    // Quick task 260730-x2t, Task 6: SR-2 — the stamp is retime-aware, and it
    // agrees with what EXPORT resolves for the same timeline position.
    // -----------------------------------------------------------------------

    const FPS: f64 = 30.0;

    fn retime(curve: rudis_core::RetimeCurve, source_span_us: i64) -> rudis_core::Retime {
        rudis_core::Retime {
            timeline_len_us: rudis_core::retimed_timeline_len_us(&curve, FPS, source_span_us),
            curve,
            timebase_fps: FPS,
        }
    }

    fn ramped_clip(start_us: i64, in_us: i64, out_us: i64, r: rudis_core::Retime) -> rudis_core::Clip {
        rudis_core::Clip {
            id: "c".into(),
            media_id: "m".into(),
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
            retime: Some(r),
        }
    }

    /// SR-2: at 2x, a source PTS 1 s past the clip's in-point belongs 0.5 s
    /// past the clip start — NOT 1 s. Stamping it 1:1 would drift preview
    /// against a correct export, and the drift would be absorbed by the
    /// SYNTHETIC_STAMP_FALLBACKS counter rather than failing anything.
    #[test]
    fn a_2x_clips_stamp_is_the_inverse_of_the_speed_integral() {
        let r = retime(rudis_core::RetimeCurve::Constant(2.0), 4_000_000);
        assert_eq!(
            map_source_pts_to_timeline(3_000_000, 2_000_000, 5_000_000, Some(&r)),
            5_500_000,
            "1 s of SOURCE at 2x is 0.5 s of TIMELINE"
        );
        // And 0.5x stretches the other way.
        let half = retime(rudis_core::RetimeCurve::Constant(0.5), 4_000_000);
        assert_eq!(
            map_source_pts_to_timeline(3_000_000, 2_000_000, 5_000_000, Some(&half)),
            7_000_000,
            "1 s of SOURCE at 0.5x is 2 s of TIMELINE"
        );
    }

    #[test]
    fn an_unretimed_stamp_is_byte_identical_to_the_pre_retime_mapping() {
        for (pts, src, pos) in [
            (0i64, 0i64, 0i64),
            (2_033_333, 2_000_000, 1_000_000),
            (66_667, 0, 0),
            (1_950_000, 2_000_000, 500_000),
        ] {
            assert_eq!(
                map_source_pts_to_timeline(pts, src, pos, None),
                pos + (pts - src),
                "the un-retimed mapping must stay the plain 1:1 offset"
            );
        }
    }

    /// **The preview≠export gate.** For every 10 ms tick across a full ramped
    /// clip, stamping the frame EXPORT would resolve at that tick must land
    /// back on the tick — within one project frame. One map, no fork.
    #[test]
    fn stamp_round_trips_with_what_export_resolves_across_a_full_ramp() {
        use rudis_core::{Interpolation, Keyframe, RetimeCurve, Timeline, Track, TrackKind};
        let key = |frame: u32, value: f32| Keyframe {
            frame,
            value,
            interp: Interpolation::Smooth,
        };
        let curve = RetimeCurve::Ramp(vec![key(0, 1.0), key(75, 0.4), key(149, 1.0)]);
        let r = retime(curve, 5_000_000);
        let clip = ramped_clip(1_000_000, 0, 5_000_000, r.clone());
        let len = clip.timeline_len_us();
        let timeline = Timeline {
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip.clone()],
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![],
                },
            ],
        };
        let one_frame = (1_000_000.0 / FPS) as i64;
        let mut t = clip.start_us;
        let mut checked = 0usize;
        while t < clip.start_us + len {
            // What EXPORT resolves at this tick...
            let hit = timeline
                .active_at(0, t)
                .unwrap_or_else(|| panic!("clip active at {t}"));
            // ...stamped back through the PREVIEW helper.
            let stamped =
                map_source_pts_to_timeline(hit.source_us, clip.in_us, clip.start_us, Some(&r));
            assert!(
                (stamped - t).abs() <= one_frame,
                "preview/export disagree at t={t}: export resolved source \
                 {} which preview stamps at {stamped} (drift {})",
                hit.source_us,
                stamped - t
            );
            checked += 1;
            t += 10_000;
        }
        assert!(checked > 400, "the sweep must actually cover the clip");
    }

    /// The stamp must be NON-DECREASING across a full ramp, so
    /// `stamp_or_fallback` never takes the synthetic path — the counter must
    /// be UNCHANGED. This is SR-2's real closure: the symptom must fail an
    /// assertion, not be quietly absorbed by a counter nobody reads.
    #[test]
    fn a_full_ramp_sweep_never_increments_the_synthetic_fallback_counter() {
        use rudis_core::{Interpolation, Keyframe, RetimeCurve};
        let _g = counter_guard();
        let key = |frame: u32, value: f32| Keyframe {
            frame,
            value,
            interp: Interpolation::Smooth,
        };
        let r = retime(
            RetimeCurve::Ramp(vec![key(0, 1.0), key(75, 0.4), key(149, 1.0)]),
            5_000_000,
        );
        let before = synthetic_fallbacks();
        let mut last: Option<i64> = None;
        let mut src = 0i64;
        while src < 5_000_000 {
            let mapped = map_source_pts_to_timeline(src, 0, 1_000_000, Some(&r));
            assert!(
                last.is_none_or(|l| mapped >= l),
                "the retimed stamp went BACKWARDS at source {src}: {last:?} -> {mapped}"
            );
            // Only feed strictly-forward stamps through the guard, exactly as
            // the producer does (equal stamps come from sub-µs source steps,
            // which real decoders never emit).
            if last.is_none_or(|l| mapped > l) {
                let out = stamp_or_fallback(Some(mapped), 0, last, "test");
                assert_eq!(out, mapped);
                last = Some(out);
            }
            src += 33_333; // one source frame at 30 fps
        }
        assert_eq!(
            synthetic_fallbacks(),
            before,
            "a correctly retimed sweep must NOT push a single frame onto the \
             synthetic grid — SYNTHETIC_STAMP_FALLBACKS is the counter that \
             would otherwise silently absorb a preview/export split (SR-2)"
        );
    }
}
