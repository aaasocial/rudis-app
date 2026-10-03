//! The D-08 heaviness predicate: is this source heavy enough that a proxy is
//! worth building for it?
//!
//! # Why this is a named function and not an `if` at the trigger site
//!
//! D-08 is explicit: *"the predicate is a named, testable function with recorded
//! thresholds, not an inline `if`. A predicate that cannot be re-scored later is
//! a guess."* So it lives here, alone, depending on nothing — three numbers and
//! four arguments, no filesystem, no subprocess, no `MediaInfo`. That is what
//! lets the table below be a table of literal rows rather than a fixture rig,
//! and what lets plan 58-09 re-score it without touching the import path.
//!
//! # What it reads, and what it deliberately does NOT (D-31)
//!
//! **Resolution × bitrate × codec-class. There is no GOP or keyframe scan, and
//! that absence is a decision, not an omission.** Two findings force it:
//!
//! 1. **RQ2 measured GOP length at ~zero effect on steady-state decode.** A
//!    heaviness *trigger* therefore does not need to tell Long-GOP from
//!    all-intra: what makes a source expensive to play is its pixel rate, and
//!    what a proxy fixes first is that. (GOP is why a proxy helps *seeking*, and
//!    that follows for free from proxying a heavy source at all.)
//! 2. **A real keyframe census costs a full decode.** `ffprobe -skip_frame
//!    nokey -count_frames` is what `scripts/windows/generate-bench-media.ps1`
//!    runs OFFLINE at fixture-build time; charging it to every import would
//!    spend seconds of a beginner's import budget to answer a question the
//!    measurement above says does not change the answer.
//!
//! Everything the predicate does read is already in hand: `width` / `height` /
//! `vcodec` have been in `engine::MediaInfo` since Phase 3, and `bit_rate` was
//! added by plan 58-01 from the SAME `-show_format` JSON `probe()` already
//! parses — no extra `ffprobe` spawn, pinned by a spawn-count test. D-31 ordered
//! that field strictly BEFORE this predicate for exactly this reason.
//!
//! # Calibration: what these three numbers were chosen against
//!
//! The targets are the BENCH-02 fixture inventory (`crates/preview/tests/common/mod.rs`),
//! whose real parameters were re-measured with the bundled `ffprobe` on
//! 2026-08-03:
//!
//! | fixture | dims | codec | container bitrate | verdict |
//! |---|---|---|---|---|
//! | `longgop_4k30_60s.mp4` (BENCH-02 **F2** — the one PROXY-06 exists for) | 3840x2160 | h264 | 62.8 Mbps | **HEAVY** |
//! | `bars_4k30_5s.mp4` | 3840x2160 | h264 | 24.5 Mbps | **HEAVY** (resolution rung; the bitrate rung alone would have missed it) |
//! | `bars_av_720p30_75s.mp4` (the BENCH-02 bed clip) | 1280x720 | h264 | 0.15 Mbps | not heavy |
//! | `bars_720p30_5s.mp4` | 1280x720 | h264 | 3.2 Mbps | not heavy |
//! | `bars_720p30_240s.mp4` | 1280x720 | h264 | 1.3 Mbps | not heavy |
//! | `vertical_720x1280_30p_20s.mp4` (the BENCH-02 vertical overlay) | 720x1280 | h264 | 1.3 Mbps | not heavy |
//!
//! i.e. **the 4K Long-GOP BENCH fixture passes and every 720p fixture fails** —
//! the phase's stated bar. Those rows are asserted as literals in this module's
//! tests, so the calibration cannot rot into a comment: no test here opens a
//! file, and in particular none depends on `longgop_4k30_60s.mp4`, which is
//! regenerated rather than committed.
//!
//! # These constants are provisional, and that is stated on purpose
//!
//! Plan **58-09** re-scores all three against REAL probes of a wider corpus and
//! writes the result to
//! `.planning/phases/58-.../artifacts/58-HEAVINESS-CALIBRATION.md`, following the
//! `57-DYNRES-CALIBRATION.md` precedent. Until then they are a defensible
//! starting point, not a measurement — and D-08's whole point is that the
//! difference is legible: three named `pub const`s in one file, one pure
//! function, and a table that says what each row stands for.
//!
//! Note that a *wrong* answer here is cheap in both directions, which is why the
//! bar is allowed to be simple: a false positive spends one background encode at
//! below-normal priority on a source that did not need it, and a false negative
//! means playback stays exactly as good as it is on `main` today.

/// **Resolution rung.** A source whose long edge reaches this is heavy
/// regardless of bitrate or codec.
///
/// 2560 sits above every common 1080p/2K delivery format and below UHD (3840),
/// so it catches 4K, 5K, 6K and 8K camera originals — the sources that cannot
/// sustain real-time preview — without sweeping in the 1920x1080 media that is
/// most of a beginner's footage. It is deliberately a *long edge*, not a pixel
/// count, so a 3840x1600 anamorphic frame and a 2160x3840 phone video are judged
/// the same way as a 3840x2160 one.
pub const HEAVY_LONG_EDGE_PX: u32 = 2560;

/// **Bitrate rung, lower bound.** Below [`HEAVY_LONG_EDGE_PX`], the bitrate test
/// only applies from this long edge upward.
///
/// A 1280x720 source at 30 Mbps exists (a screen capture, a high-quality
/// intermediate), but it decodes comfortably in real time: the pixel rate, not
/// the bit rate, is what costs. Gating the bitrate rung at 1920 keeps the
/// predicate from proxying small-but-fat media that was never a problem.
pub const HEAVY_BITRATE_LONG_EDGE_PX: u32 = 1920;

/// **Bitrate rung, threshold.** At or above [`HEAVY_BITRATE_LONG_EDGE_PX`], an
/// inter-frame source carrying at least this many bits per second is heavy.
///
/// 25 Mbps is roughly 3x a streaming-delivery 1080p file (~8 Mbps) and squarely
/// in camera-original territory (consumer 4K/1080p cameras record 50-100 Mbps;
/// high-bitrate 1080p Long-GOP lands at 25-50). It is compared against the
/// CONTAINER bitrate, which includes audio — a small over-count that biases
/// toward proxying, i.e. toward the cheap error.
pub const HEAVY_BITRATE_BPS: u64 = 25_000_000;

/// Video codecs treated as **inter-frame** (Long-GOP), where a high bitrate
/// really does signal expensive decode.
///
/// The complement — ProRes, DNxHR, MJPEG, uncompressed, and anything else not
/// named here — is intra-frame, where a high bitrate is a property of the format
/// rather than evidence of decode cost. Such a source is already cheap to seek
/// and decode, so the bitrate rung must not fire on it; the resolution rung
/// still does, because a 4K ProRes file is expensive for its pixel count.
const INTERFRAME_CODECS: &[&str] = &[
    "h264",
    "hevc",
    "h265",
    "vp9",
    "av1",
    "mpeg4",
    "mpeg2video",
    "vc1",
];

/// Is a source with these properties heavy enough to be worth proxying?
/// (D-07/D-08/D-31.)
///
/// Pure: no I/O, no clock, no environment. `width`/`height` are CONTAINER
/// (unrotated) pixel dimensions — the same ones
/// [`crate::cache::proxy_dims`] derives geometry from, so the trigger and the
/// geometry policy can never disagree about which edge is long.
/// `bit_rate_bps` is the container bitrate and `vcodec` the ffprobe codec name;
/// both are `Option` because both can genuinely be absent.
///
/// # The two rungs
///
/// ```text
/// long edge >= HEAVY_LONG_EDGE_PX (2560)                         -> heavy
/// long edge >= HEAVY_BITRATE_LONG_EDGE_PX (1920)
///     AND the codec is inter-frame (or unknown)
///     AND bitrate >= HEAVY_BITRATE_BPS (25 Mbps)                 -> heavy
/// otherwise                                                      -> not heavy
/// ```
///
/// # Where it is conservative, and why in that direction
///
/// An **unknown codec** counts as inter-frame, and an unknown bitrate simply
/// fails the bitrate rung rather than rescuing anything from the resolution
/// rung. Both lean toward "proxy it": a needless proxy costs one background
/// encode at below-normal priority (D-10) plus cache bytes the byte budget
/// already bounds (D-15), while a missed one costs the dropped frames this whole
/// phase exists to prevent.
///
/// A zero dimension answers `false` rather than panicking — audio-only and
/// still-image media reach `MediaInfo` with `width == 0`, and they have no
/// playback cost to fix.
pub fn needs_proxy(width: u32, height: u32, bit_rate_bps: Option<u64>, vcodec: Option<&str>) -> bool {
    let long = width.max(height);
    if long == 0 {
        // Audio-only, a still, or unparsable media. Nothing to proxy.
        return false;
    }

    // Rung 1: resolution, unconditional.
    if long >= HEAVY_LONG_EDGE_PX {
        return true;
    }

    // Rung 2: codec class x bitrate, from HEAVY_BITRATE_LONG_EDGE_PX upward.
    let interframe = match vcodec {
        // Unknown codec -> assume the expensive case (see "conservative" above).
        None => true,
        Some(codec) => {
            let codec = codec.trim().to_ascii_lowercase();
            INTERFRAME_CODECS.contains(&codec.as_str())
        }
    };

    interframe
        && long >= HEAVY_BITRATE_LONG_EDGE_PX
        && bit_rate_bps.is_some_and(|bps| bps >= HEAVY_BITRATE_BPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of the calibration table:
    /// `(width, height, bitrate, codec, expected verdict, what it stands for)`.
    type CalibrationRow = (u32, u32, Option<u64>, Option<&'static str>, bool, &'static str);

    /// One row of the measured-fixture table:
    /// `(fixture, width, height, container bitrate, codec, expected verdict)`.
    type MeasuredRow = (&'static str, u32, u32, u64, &'static str, bool);

    /// Behaviour 1 — the calibration table, as literal rows.
    ///
    /// Each row names the fixture or scenario it stands for, so plan 58-09's
    /// re-scoring can see what each threshold was chosen against. No row opens a
    /// file: these are the fixtures' MEASURED parameters, not the fixtures.
    #[test]
    fn the_predicate_matches_the_bench_fixture_table() {
        let rows: &[CalibrationRow] = &[
            (3840, 2160, Some(60_000_000), Some("h264"), true,
             "BENCH-02 F2: the 4K Long-GOP overlay PROXY-06 exists for"),
            (1280, 720, Some(4_000_000), Some("h264"), false,
             "BENCH-02 bed clip: an ordinary 720p source must cost nothing"),
            (720, 1280, Some(4_000_000), Some("h264"), false,
             "BENCH-02 vertical overlay: orientation must not smuggle a source in"),
            (1920, 1080, Some(8_000_000), Some("h264"), false,
             "ordinary 1080p delivery media: not heavy"),
            (1920, 1080, Some(30_000_000), Some("h264"), true,
             "high-bitrate 1080p Long-GOP (camera original): heavy without being 4K"),
        ];
        for &(w, h, br, codec, expected, why) in rows {
            assert_eq!(
                needs_proxy(w, h, br, codec),
                expected,
                "{w}x{h} br={br:?} codec={codec:?} — {why}"
            );
        }
    }

    /// Behaviour 2 — an unknown bitrate/codec never SAVES a 4K source from being
    /// proxied. The resolution rung is unconditional on purpose.
    #[test]
    fn unknown_bitrate_and_codec_never_rescue_a_4k_source() {
        assert!(
            needs_proxy(3840, 2160, None, None),
            "a 4K source with no bitrate and no codec must still be heavy"
        );
    }

    /// Behaviour 3 — an intra-frame codec does not get the bitrate rung applied
    /// to it (its bitrate is high by construction, not by being hard to decode).
    /// The RESOLUTION rung still applies to it, which the control below proves.
    #[test]
    fn an_intra_frame_codec_is_not_heavy_on_bitrate_alone() {
        assert!(
            !needs_proxy(1920, 1080, Some(30_000_000), Some("prores")),
            "ProRes at 30 Mbps is already all-intra — the bitrate rung must not fire"
        );
        // CONTROL: the codec exemption is scoped to the BITRATE rung only.
        assert!(
            needs_proxy(3840, 2160, Some(30_000_000), Some("prores")),
            "4K ProRes is still expensive for its pixel count — the resolution rung \
             must not be codec-dependent"
        );
    }

    /// Behaviour 4 — degenerate input answers, it does not panic.
    #[test]
    fn degenerate_dimensions_are_not_heavy() {
        assert!(
            !needs_proxy(0, 0, None, None),
            "audio-only and unparsable media reach MediaInfo with 0x0 and have no \
             playback cost to fix"
        );
        assert!(
            !needs_proxy(0, 0, Some(u64::MAX), Some("h264")),
            "not even an absurd bitrate makes a source with no pixels heavy"
        );
        // The other end of the range: enormous dimensions must answer, not
        // overflow. (`max` on u32 cannot overflow; this pins that no arithmetic
        // is introduced later that could.)
        assert!(
            needs_proxy(u32::MAX, u32::MAX, None, None),
            "an enormous source is heavy, and getting there must not panic"
        );
    }

    /// The MEASURED parameters of every COMMITTED fixture this phase touches
    /// (bundled `ffprobe`, 2026-08-03). This is the must-have stated as a test:
    /// **every 720p fixture fails the predicate**, and the committed 4K one
    /// passes.
    #[test]
    fn every_committed_720p_fixture_fails_and_the_4k_one_passes() {
        let measured: &[MeasuredRow] = &[
            ("bars_av_720p30_75s.mp4", 1280, 720, 150_477, "h264", false),
            ("bars_720p30_5s.mp4", 1280, 720, 3_187_662, "h264", false),
            ("bars_720p30_75s.mp4", 1280, 720, 48_632, "h264", false),
            ("bars_720p30_240s.mp4", 1280, 720, 1_327_579, "h264", false),
            ("vertical_720x1280_30p_20s.mp4", 720, 1280, 1_287_523, "h264", false),
            ("bars_640x480_30p_2s_untagged.mp4", 640, 480, 32_924, "h264", false),
            ("hevc10.mp4", 640, 360, 542_697, "hevc", false),
            ("bars_4k30_5s.mp4", 3840, 2160, 24_548_155, "h264", true),
            // Regenerated, never committed — named here only as a parameter row.
            ("the BENCH-02 4K Long-GOP fixture", 3840, 2160, 62_841_832, "h264", true),
        ];
        for &(name, w, h, br, codec, expected) in measured {
            assert_eq!(
                needs_proxy(w, h, Some(br), Some(codec)),
                expected,
                "{name} ({w}x{h}, {br} bps, {codec})"
            );
        }
    }

    /// Codec names arrive from ffprobe and are compared, so the comparison must
    /// not be brittle about case or stray whitespace.
    #[test]
    fn codec_matching_is_case_and_whitespace_insensitive() {
        assert!(needs_proxy(1920, 1080, Some(30_000_000), Some("H264")));
        assert!(needs_proxy(1920, 1080, Some(30_000_000), Some(" h264 ")));
        assert!(needs_proxy(1920, 1080, Some(30_000_000), Some("HEVC")));
    }

    /// The two rungs must be independently reachable, or one of them is dead
    /// code that the table above would never notice.
    #[test]
    fn each_rung_fires_on_its_own() {
        // Resolution only: below the bitrate threshold, and an intra codec.
        assert!(
            needs_proxy(HEAVY_LONG_EDGE_PX, 1080, Some(1_000_000), Some("prores")),
            "the resolution rung must fire with the bitrate rung inapplicable"
        );
        // Bitrate only: below the resolution threshold.
        assert!(
            needs_proxy(HEAVY_LONG_EDGE_PX - 2, 1080, Some(HEAVY_BITRATE_BPS), Some("h264")),
            "the bitrate rung must fire below the resolution threshold"
        );
        // And the boundaries are inclusive, exactly as documented.
        assert!(!needs_proxy(HEAVY_BITRATE_LONG_EDGE_PX - 1, 1080, Some(HEAVY_BITRATE_BPS), Some("h264")));
        assert!(!needs_proxy(HEAVY_BITRATE_LONG_EDGE_PX, 1080, Some(HEAVY_BITRATE_BPS - 1), Some("h264")));
    }
}
