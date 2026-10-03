//! The clip-edit (video-to-video) SOURCE-RANGE building block: the D-01 window
//! refusal and the D-02 trim-respecting extraction.
//!
//! # What this module is for
//!
//! Phase 56 adds one generation capability that takes an EXISTING timeline
//! clip's own frames and re-renders them. Two facts about that capability have
//! to be enforced locally, before a single byte leaves this machine:
//!
//! * **D-01 / SC-2 — the input window.** The provider accepts a bounded input
//!   length and refuses everything outside it. Rudis refuses FIRST, by name, at
//!   BOTH edges, and offers what is real in the same breath — the honest-refusal
//!   dialect `crates/agent-llm/src/skills/vfx.md` already teaches ("Name the
//!   limit and offer what is real in the same breath"). It never silently
//!   truncates a long clip to fit, because a silently wrong promise is the one
//!   failure mode 56-CONTEXT's own § Specific Ideas calls unrecoverable — and,
//!   at the corrected `aleph2` price, an expensive one to discover remotely.
//! * **D-02 — trim-respecting frames.** What gets sent is exactly what the
//!   timeline clip currently SHOWS: `[in_us, out_us)` with `out_us`
//!   **EXCLUSIVE**, at or below the provider's frame-rate ceiling, with no audio
//!   track at all.
//!
//! # Where the numbers come from — and where they do NOT
//!
//! Every bound in this file is READ from `agent_gen`'s probe-derived consts
//! (`RUNWAY_V2V_INPUT_MIN_SECONDS` / `RUNWAY_V2V_INPUT_MAX_SECONDS` /
//! `RUNWAY_V2V_INPUT_MAX_FPS`), never re-declared here. That is not tidiness:
//! 56-CONTEXT's D-01 was written believing the ceiling was ten seconds,
//! 56-RESEARCH § Q3 retired that number against three official-domain sources,
//! and 56-01's probe then declined to promote the replacement to CONFIRMED
//! because the endpoint fails on content before it ever range-checks length. A
//! literal copied into this file would be a fourth place for that correction to
//! fail to reach. One source of truth, and the confidence label travels with the
//! value at its definition site.
//!
//! **Honest status of the window, stated once here rather than in a user-facing
//! string:** the 2–30 second range and the frame-rate ceiling are DOCUMENTED and
//! NOT live-verified (56-01 follow-up F-3 / probe V-6 is the named closer). The
//! cost of each direction of being wrong is asymmetric and cheap: too tight and
//! we refuse locally at $0.00 something the provider would have taken; too loose
//! and the provider 400s loudly, also at $0.00. Neither direction spends money
//! silently, which is why the refusal ships on documented numbers rather than
//! waiting for F-3. The refusal MESSAGE deliberately does not recite this
//! epistemics at a beginner — 56-CONTEXT's whole audience is someone with no
//! editing experience — but the reader of this code gets it in full.
//!
//! # What this module deliberately does NOT do
//!
//! * It does not call the window check from inside the extraction. There is ONE
//!   refusal site (the caller, Plan 06's host seam), because two sites means two
//!   messages and eventually two thresholds.
//! * It does not build a decode or encode pipeline. It composes `engine`'s
//!   existing sidecar primitives, and `crates/engine/src/ffmpeg.rs` is untouched
//!   by this plan (CLAUDE.md rule 6 — a parallel encode path is exactly how a
//!   GPL codec gets silently reintroduced).

/// D-01 / SC-2: refuse a clip whose visible range falls outside the clip-edit
/// model's input window — **at both edges** — naming the limit and offering
/// what is real in the same breath.
///
/// `visible_us` is the length of what the clip actually SHOWS (`out_us -
/// in_us`), not the underlying media's duration. `Ok(())` means the range is
/// inside the window; `Err(message)` is a finished, user-facing sentence the
/// agent seam can hand straight to the user.
///
/// Both bounds are read from [`agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS`] and
/// [`agent_gen::RUNWAY_V2V_INPUT_MAX_SECONDS`]; see this module's doc for why
/// no literal appears here.
///
/// # The minimum is not an afterthought
///
/// 56-CONTEXT's D-01 as originally discussed had only a ceiling. The 2026-08-01
/// correction added the floor, and a floor is the easier one to trip by
/// accident: a two-frame clip left over from a split is an ordinary thing to
/// find on a timeline, and it is under the minimum by a factor of thirty.
pub fn clip_edit_window_check(visible_us: i64) -> Result<(), String> {
    let min_s = agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS;
    let max_s = agent_gen::RUNWAY_V2V_INPUT_MAX_SECONDS;
    let min_us = i64::from(min_s) * 1_000_000;
    let max_us = i64::from(max_s) * 1_000_000;
    let visible_s = visible_us as f64 / 1_000_000.0;

    if visible_us > max_us {
        return Err(format!(
            "this clip's visible range is {visible_s:.1}s, but the clip-edit model accepts \
             {min_s}-{max_s} seconds of input. Split the clip (splitClip) or trim it to a \
             range inside the window first — never send a silently truncated range."
        ));
    }
    if visible_us < min_us {
        return Err(format!(
            "this clip's visible range is {visible_s:.1}s, under the clip-edit model's \
             {min_s}-second minimum ({min_s}-{max_s}s accepted). Use a longer clip, or \
             extend the trim before editing."
        ));
    }
    Ok(())
}

/// The bitrate FLOOR handed to [`engine::RenderCacheEncoder::new`], restated
/// here rather than imported.
///
/// Two reasons it is a local number and not a borrowed one. First, on every
/// SHIPPED encoder branch it never reaches the command line at all: engine's
/// `render_cache_encode_args` is quality-targeted on Media Foundation and
/// constant-QP on NVENC, and this value is emitted only by the DEV-override
/// branch (which also happens to be the only branch that can name an encoder we
/// did not clear). The constructor still validates it non-zero, so it must be
/// something. Second, the crate that owns the original number may be named by
/// exactly one file in `crates/app-core/src`, and this is not that file — the
/// split its own manifest declares. `crates/engine/tests/render_cache_encode.rs`
/// restates it for the first of those reasons; this file restates it for both.
const CLIP_RANGE_ENCODE_BITRATE_FLOOR_BPS: u64 = 24_000_000;

/// A temp file that deletes itself — on the success path, on every `?` return,
/// and on a panic.
///
/// T-56-FOOTAGE-04: the extracted bytes are a frame-accurate copy of the user's
/// own footage. They exist on disk only for as long as the sidecar needs a mux
/// target, and "we remembered to clean up on the error path too" is a claim a
/// `Drop` makes structurally and a sequence of statements only promises.
struct SelfDeletingMp4(std::path::PathBuf);

impl Drop for SelfDeletingMp4 {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// D-02: encode exactly what the timeline clip currently SHOWS — `[in_us,
/// out_us)`, **`out_us` EXCLUSIVE** — as a small, video-only MP4, and hand back
/// its bytes.
///
/// `source_fps` is the clip's own working rate as the caller knows it (the
/// `avg_frame_rate` normalized at import). A non-positive or non-finite value
/// falls back to the probe's own `avg_frame_rate` rather than guessing.
///
/// # The four properties this function exists to guarantee
///
/// 1. **Trim-respecting, exclusive end.** The range starts at `in_us` and stops
///    strictly before `out_us`, the same convention `last_visible_frame_us`
///    (`crates/ffi/src/ctx.rs`) walks for a single instant — here applied to a
///    RANGE. The underlying media's own head and tail never appear.
/// 2. **At or below the frame-rate ceiling.** Output cadence is
///    `min(source_fps, RUNWAY_V2V_INPUT_MAX_FPS)`. A 60 fps phone clip is an
///    entirely ordinary input and nothing else in the pipeline would notice it
///    was over the limit. The downconversion is frame SELECTION performed by the
///    sidecar's own `fps` filter inside [`engine::ExportRunDecoder`] — every Nth
///    frame, never a duplicate and never a blend.
/// 3. **No audio track, by construction.** 56-RESEARCH's prescriptive
///    recommendation: the model's output is picture-only, so audio in the upload
///    buys nothing, and a fatter file spills more clips off the inline-data-URI
///    path onto the upload fallback. D-07's audio preservation is a separate,
///    later, placement-time behaviour on the ORIGINAL clip's own audio — never a
///    round trip through a provider.
/// 4. **The license-safe encoder, through the existing sidecar.** No new encode
///    pipeline is built here and no encoder name is chosen here; both are
///    engine's, already cleared (CLAUDE.md rule 6, GPU-07 posture).
///
/// # Why not `engine::VideoEncoder`
///
/// This plan was written to call it, and it cannot satisfy property 3.
/// [`engine::VideoEncoder::new`] takes an audio WAV as a required parameter and
/// its command line hardcodes `-c:a aac -shortest`; there is no door to turn
/// audio off. That is not an oversight — it is export's frozen instrument, and
/// engine's own `RenderCacheEncoder` doc records the identical limitation as one
/// of the three reasons IT had to exist (Phase 58 D-32). So the video-only
/// frame-push encoder this needs already exists in the frozen file:
/// [`engine::RenderCacheEncoder`], whose `-an` is stated as a design property
/// and which resolves its encoder through the same single cleared-encoder
/// chokepoint (hardware first, `DEFAULT_VIDEO_ENCODER` otherwise). Nothing in
/// `crates/engine` was modified to reach it.
///
/// The one cost, recorded rather than hidden: that encoder is ALL-INTRA, so the
/// payload is larger than an inter-coded one at the same quality and more ranges
/// will route through the `/v1/uploads` overflow instead of an inline data URI.
/// Correctness is unaffected — the transport gate is a pure size dispatch — but
/// a future plan wanting smaller payloads has a real reason to add a non-all-intra
/// video-only mode, and this is where to look.
///
/// # What this deliberately does NOT do
///
/// It does not run [`clip_edit_window_check`]. There is ONE refusal site and it
/// is the caller's (Plan 06's host seam, which checks and only then extracts) —
/// two sites means two messages and eventually two thresholds.
pub fn extract_clip_range_mp4(
    media_path: &std::path::Path,
    in_us: i64,
    out_us: i64,
    source_fps: f64,
) -> Result<Vec<u8>, String> {
    if in_us < 0 {
        return Err(format!(
            "clip-edit extraction (range): in_us {in_us} is negative for {}",
            media_path.display()
        ));
    }
    if out_us <= in_us {
        return Err(format!(
            "clip-edit extraction (range): out_us {out_us} must be strictly greater than \
             in_us {in_us} (the range is half-open, out_us EXCLUSIVE) for {}",
            media_path.display()
        ));
    }

    let info = engine::probe(media_path).map_err(|e| {
        format!(
            "clip-edit extraction (probe) failed for {}: {e}",
            media_path.display()
        )
    })?;

    // Upright geometry, then rounded DOWN to even: yuv420p is 2x2 subsampled and
    // an odd dimension cannot be encoded at all. Rounding down (never up) keeps
    // every output pixel a real source pixel.
    let (up_w, up_h) = match info.rotation_degrees % 360 {
        90 | 270 => (info.height, info.width),
        _ => (info.width, info.height),
    };
    let (out_w, out_h) = (up_w & !1, up_h & !1);
    if out_w == 0 || out_h == 0 {
        return Err(format!(
            "clip-edit extraction (probe): {} reports unusable video geometry {up_w}x{up_h}",
            media_path.display()
        ));
    }

    // The caller's rate wins; the probe's normalized rate is the fallback. Never
    // a guess — a wrong cadence here is a silently sped-up or slowed-down clip.
    let src_fps = if source_fps.is_finite() && source_fps > 0.0 {
        source_fps
    } else {
        info.avg_frame_rate
    };
    if !(src_fps.is_finite() && src_fps > 0.0) {
        return Err(format!(
            "clip-edit extraction (probe): no usable frame rate for {} \
             (caller said {source_fps}, probe said {})",
            media_path.display(),
            info.avg_frame_rate
        ));
    }
    let ceiling_fps = f64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_FPS);
    let out_fps = src_fps.min(ceiling_fps);

    let dur_us = out_us - in_us;
    // The half-open frame count on the OUTPUT grid: timestamps 0, 1/f, 2/f, ...
    // strictly below `dur_us`. Same `ceil` the sidecar's own `-t` bound applies,
    // which is what keeps `out_us` exclusive after the resample.
    let expected_frames = (((dur_us as f64) * out_fps / 1_000_000.0).ceil() as i64).max(1) as usize;

    let temp = SelfDeletingMp4(std::env::temp_dir().join(format!(
        "rudis-v2v-{}-{}-{}.mp4",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        in_us
    )));

    let decoder = engine::ExportRunDecoder::start(
        media_path,
        in_us,
        dur_us,
        info.rotation_degrees,
        out_w,
        out_h,
        out_fps,
    )
    .map_err(|e| {
        format!(
            "clip-edit extraction (decode) failed to open {} at {in_us}us: {e}",
            media_path.display()
        )
    })?;

    let mut encoder = engine::RenderCacheEncoder::new(
        &temp.0,
        out_w,
        out_h,
        out_fps,
        CLIP_RANGE_ENCODE_BITRATE_FLOOR_BPS,
    )
    .map_err(|e| {
        format!(
            "clip-edit extraction (encode) failed to open {}: {e}",
            temp.0.display()
        )
    })?;

    let mut pushed = 0usize;
    while pushed < expected_frames {
        let Some(frame) = decoder.next_frame() else {
            break; // genuine end of stream — encode what the range really had
        };
        encoder.push_frame(&frame.rgba).map_err(|e| {
            format!(
                "clip-edit extraction (encode) failed on frame {pushed} of {} from {}: {e}",
                expected_frames,
                media_path.display()
            )
        })?;
        pushed += 1;
    }
    drop(decoder);

    if pushed == 0 {
        // The encoder is dropped (killed + reaped) by falling out of scope; the
        // temp file goes with `temp`.
        return Err(format!(
            "clip-edit extraction (decode): {} yielded no frames over [{in_us}, {out_us}) — \
             the range is outside the media",
            media_path.display()
        ));
    }

    encoder.finish().map_err(|e| {
        format!(
            "clip-edit extraction (encode) failed to finalize {} after {pushed} frames: {e}",
            temp.0.display()
        )
    })?;

    let bytes = std::fs::read(&temp.0).map_err(|e| {
        format!(
            "clip-edit extraction (io) failed to read back {}: {e}",
            temp.0.display()
        )
    })?;
    // `temp` drops here and takes the file with it, on this path and every
    // early-return path above.
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture, mad};
    use std::path::{Path, PathBuf};

    /// Seconds -> microseconds, so no test below has to spell a bound as a
    /// literal (the whole point of `window_check_reads_the_agent_gen_consts`).
    fn secs_us(seconds: f64) -> i64 {
        (seconds * 1_000_000.0).round() as i64
    }

    fn min_s() -> f64 {
        f64::from(agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS)
    }

    fn max_s() -> f64 {
        f64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_SECONDS)
    }

    #[test]
    fn window_check_passes_inside_the_window() {
        let inside = secs_us(min_s() + 1.0);
        assert!(
            clip_edit_window_check(inside).is_ok(),
            "a range one second above the floor is squarely inside the window"
        );
    }

    #[test]
    fn window_check_refuses_over_the_ceiling_by_name() {
        let over = secs_us(max_s() + 5.0);
        let msg = clip_edit_window_check(over)
            .expect_err("a range five seconds past the ceiling must refuse");

        // BOTH bounds are named, so the user learns the window rather than just
        // that they are outside it.
        assert!(
            msg.contains(&agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS.to_string()),
            "the over-ceiling refusal must name the minimum too: {msg}"
        );
        assert!(
            msg.contains(&agent_gen::RUNWAY_V2V_INPUT_MAX_SECONDS.to_string()),
            "the over-ceiling refusal must name the maximum: {msg}"
        );
        // The MEASURED length, to one decimal — so the user can see how far out
        // they are, not merely that they are out.
        assert!(
            msg.contains(&format!("{:.1}s", max_s() + 5.0)),
            "the refusal must quote the measured visible length to one decimal: {msg}"
        );
        // The OFFER, in the vfx.md dialect: name the limit AND what is real.
        assert!(
            msg.contains("split"),
            "the refusal must offer the split (vfx.md:160-162 posture): {msg}"
        );
        assert!(
            msg.contains("trim"),
            "the refusal must offer the trim as well as the split: {msg}"
        );
        // The failure mode this criterion exists to prevent.
        assert!(
            msg.contains("truncated"),
            "the refusal must say a silently truncated range is not what happens: {msg}"
        );
    }

    #[test]
    fn window_check_refuses_under_the_floor_by_name() {
        // One second — under the documented floor, and the case the ORIGINAL
        // D-01 never contemplated at all.
        let under = secs_us(1.0);
        assert!(
            under < secs_us(min_s()),
            "this fixture only tests the floor if it is actually under it"
        );
        let msg =
            clip_edit_window_check(under).expect_err("a sub-minimum range must refuse, not pass");

        assert!(
            msg.contains("minimum"),
            "the under-floor refusal must name the limit as a MINIMUM: {msg}"
        );
        assert!(
            msg.contains(&agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS.to_string()),
            "the under-floor refusal must name the minimum's value: {msg}"
        );
        assert!(
            msg.contains("longer clip"),
            "the under-floor refusal must offer what is real — a longer clip: {msg}"
        );
        assert!(
            msg.contains("1.0s"),
            "the under-floor refusal must quote the measured length: {msg}"
        );
        // The floor's offer is NOT the ceiling's offer: splitting a too-short
        // clip makes it shorter. A copy-pasted message would fail here.
        assert!(
            !msg.contains("splitClip"),
            "offering a split to a clip that is already too SHORT is the wrong \
             remedy and would make the refusal dishonest: {msg}"
        );
    }

    /// The desynchronization guard: this test derives its expectations FROM the
    /// `agent_gen` consts, so a probe-driven correction to the window (F-3 / V-6
    /// is the named closer) moves the test and the check together or fails
    /// loudly. It is what makes "read the one source of truth" a checked
    /// property rather than a convention.
    #[test]
    fn window_check_reads_the_agent_gen_consts() {
        let min_us = secs_us(min_s());
        let max_us = secs_us(max_s());

        assert!(
            clip_edit_window_check(min_us).is_ok(),
            "exactly the documented minimum is INSIDE the window (inclusive)"
        );
        assert!(
            clip_edit_window_check(max_us).is_ok(),
            "exactly the documented maximum is INSIDE the window (inclusive)"
        );
        assert!(
            clip_edit_window_check(min_us - 1).is_err(),
            "one microsecond under the documented minimum must refuse"
        );
        assert!(
            clip_edit_window_check(max_us + 1).is_err(),
            "one microsecond over the documented maximum must refuse"
        );

        // And the retired number is provably not the live threshold: a range the
        // retired ten-second ceiling would have refused is accepted today.
        assert!(
            clip_edit_window_check(secs_us(12.0)).is_ok(),
            "12s is inside the corrected window; if this fails, the retired \
             ten-second ceiling has crept back in"
        );
    }

    // -----------------------------------------------------------------------
    // Task 2 — REAL-MEDIA extraction gates (CLAUDE.md rule 3: every claim below
    // is settled by DECODING the produced file, never by asserting an argv).
    //
    // The sidecar these drive is pinned to the repo's BUNDLED LGPL build by
    // `test_support::fixture`, which every one of them calls before it spawns
    // anything. That pin is not boilerplate: PATH on a dev machine resolves a
    // GPL-configured build (which CLAUDE.md rule 6 forbids scoring anything
    // against) and, when it is a launcher SHIM, makes a decoder session's
    // teardown hang for ever (debug session `pool-adopt-test-hang`). The GPL
    // encoder names themselves are deliberately not written anywhere in this
    // file, so a grep for them is a clean licence check rather than a hunt for
    // prose exceptions.
    // -----------------------------------------------------------------------

    /// Where a test writes bytes it needs to probe/decode. Carries the SAME
    /// `rudis-v2v-` prefix the production temp file uses, and the same
    /// self-deleting discipline.
    fn scratch_mp4(tag: &str) -> SelfDeletingMp4 {
        SelfDeletingMp4(std::env::temp_dir().join(format!(
            "rudis-v2v-test-{tag}-{}-{}.mp4",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )))
    }

    fn write_bytes(dst: &Path, bytes: &[u8]) {
        std::fs::write(dst, bytes).expect("scratch write must succeed");
    }

    /// Mean of the RED channel only. The A byte is a constant 255 after decode
    /// and would dilute any signal averaged over all four.
    fn mean_red(frame: &engine::Frame) -> f64 {
        let sum: u64 = frame.rgba.chunks_exact(4).map(|px| u64::from(px[0])).sum();
        sum as f64 / (frame.rgba.len() / 4) as f64
    }

    /// A synthetic source whose every frame is INDIVIDUALLY IDENTIFIABLE: frame
    /// `i` is a flat field at level `i * step`. That is what turns "did the
    /// downconversion SELECT every Nth frame or DUPLICATE one" from an argv
    /// claim into a measurement on decoded pixels.
    ///
    /// Flat fields are chosen deliberately: they survive an H.264 round trip
    /// essentially intact, so a level read back out is evidence about WHICH
    /// source frame it was rather than about the encoder's rate control.
    fn write_level_ramp(dst: &Path, frames: usize, fps: f64, step: u8, w: u32, h: u32) {
        let mut enc = engine::RenderCacheEncoder::new(
            dst,
            w,
            h,
            fps,
            CLIP_RANGE_ENCODE_BITRATE_FLOOR_BPS,
        )
        .expect("the ramp fixture's encoder must open");
        for i in 0..frames {
            let level = (i as u32 * u32::from(step)).min(255) as u8;
            let px = [level, level, level, 255u8];
            let frame: Vec<u8> = px
                .iter()
                .copied()
                .cycle()
                .take(w as usize * h as usize * 4)
                .collect();
            enc.push_frame(&frame).expect("ramp push must succeed");
        }
        enc.finish().expect("the ramp fixture must finalize");
    }

    /// D-02's central claim, on REAL media with a REAL non-zero trim: the
    /// extracted file's FIRST frame is the trim's first frame — not the media's.
    ///
    /// The proof is deliberately two-sided. Matching the source at `in_us` alone
    /// would also pass if the extractor had accidentally produced something
    /// close to everything; so the same output frame is ALSO diffed against the
    /// media's own frame 0, and that difference must be far larger. On a static
    /// fixture both numbers would be near zero and the test would be vacuous,
    /// which is why the fixture is `testsrc` (a moving pattern) and why the
    /// source's own 0-vs-`in_us` difference is asserted first.
    #[test]
    fn extraction_is_trim_respecting_on_a_real_fixture() {
        let src = fixture("testsrc_720p30_5s.mp4");
        let src = PathBuf::from(src);
        let in_us = 1_000_000i64;
        let out_us = 3_000_000i64;

        let src_at_in = engine::decode_frame_rgba_at(&src, in_us, 0).expect("source decode @in_us");
        let src_at_zero = engine::decode_frame_rgba_at(&src, 0, 0).expect("source decode @0");
        let source_moves = mad(&src_at_in.rgba, &src_at_zero.rgba);
        assert!(
            source_moves > 5.0,
            "this gate is only meaningful on MOVING content; {} differs by only \
             {source_moves:.2} between 0s and 1s",
            src.display()
        );

        let bytes = extract_clip_range_mp4(&src, in_us, out_us, 30.0)
            .expect("extraction of a real 2s range must succeed");
        assert!(!bytes.is_empty(), "the extraction must return real bytes");

        let scratch = scratch_mp4("trim");
        write_bytes(&scratch.0, &bytes);

        // --- what the produced FILE says about itself ---
        let out_info = engine::probe(&scratch.0).expect("the extracted mp4 must probe");
        let step = engine::frame_step_us(out_info.avg_frame_rate.max(1.0));
        let want_us = out_us - in_us;
        assert!(
            (out_info.duration_us - want_us).abs() <= step,
            "the extracted duration {}us must be within one frame step ({step}us) of the \
             visible range {want_us}us — out_us is EXCLUSIVE",
            out_info.duration_us
        );
        assert!(
            out_info.avg_frame_rate <= f64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_FPS) + 0.01,
            "the extracted cadence {} must sit at or under the ceiling",
            out_info.avg_frame_rate
        );

        // --- what the produced PIXELS say (the claim that actually matters) ---
        let out0 = engine::decode_frames_rgba_seq(&scratch.0, 0, 1, 0)
            .expect("the extracted mp4 must decode");
        let out0 = &out0[0];
        assert_eq!(
            (out0.width, out0.height),
            (src_at_in.width, src_at_in.height),
            "geometry must survive the round trip"
        );

        let to_trim_start = mad(&out0.rgba, &src_at_in.rgba);
        let to_media_start = mad(&out0.rgba, &src_at_zero.rgba);
        println!(
            "V2V-TRIM mad(out[0], src@{in_us}us)={to_trim_start:.3}  \
             mad(out[0], src@0)={to_media_start:.3}  src_moves={source_moves:.3}"
        );
        assert!(
            to_trim_start < to_media_start / 3.0,
            "the extracted first frame must be the TRIM's first frame: it differs from \
             src@{in_us}us by {to_trim_start:.3} but from src@0 by {to_media_start:.3}"
        );
        // An absolute bound as well as a relative one, so a future encoder
        // regression that degrades BOTH numbers equally still fails here. The
        // budget is set FROM the measurement (0.511 on the bundled LGPL build,
        // 2026-08-09) with ~6x headroom for encoder/driver variation — an H.264
        // round trip's worth of loss on a detailed test pattern, not a
        // pixel-exactness claim, and not a rubber stamp either.
        assert!(
            to_trim_start < 3.0,
            "the extracted first frame should match the source at in_us within an \
             encode tolerance; measured {to_trim_start:.3}"
        );
    }

    /// D-02's audio half: the transport payload carries picture only.
    ///
    /// Non-vacuous by construction — the SOURCE is asserted to have an audio
    /// track first, so a fixture that quietly lost its audio could not make this
    /// pass for the wrong reason.
    #[test]
    fn extraction_carries_no_audio_track() {
        let src = PathBuf::from(fixture("bars_av_720p30_75s.mp4"));
        let src_info = engine::probe(&src).expect("the AV fixture must probe");
        assert!(
            src_info.has_audio,
            "this gate is only meaningful if the SOURCE has audio to lose"
        );

        let bytes = extract_clip_range_mp4(&src, 500_000, 2_500_000, src_info.avg_frame_rate)
            .expect("extraction from an AV source must succeed");
        let scratch = scratch_mp4("noaudio");
        write_bytes(&scratch.0, &bytes);

        let out_info = engine::probe(&scratch.0).expect("the extracted mp4 must probe");
        assert!(
            !out_info.has_audio,
            "the extracted payload must carry NO audio track (56-RESEARCH's \
             prescriptive recommendation): probe says has_audio"
        );
        assert!(
            out_info.acodec.is_none(),
            "no audio codec should be reported at all; got {:?}",
            out_info.acodec
        );
        assert_eq!(
            out_info.media_kind,
            engine::MediaKind::Video,
            "and it must still be real video"
        );
    }

    /// The frame-rate ceiling, proven to be SELECTION rather than duplication.
    ///
    /// A 60 fps source is built in-test (no repo fixture runs above 30) as a
    /// level ramp, so each source frame is identifiable by its own pixels. At a
    /// 30 fps output the decoded levels must advance by TWO source steps per
    /// output frame. Duplication would show as a zero delta somewhere; no
    /// downconversion at all would show as a one-step delta.
    #[test]
    fn extraction_downconverts_over_ceiling_fps() {
        // Pin the sidecar before anything spawns (this test builds its own
        // source, so it never calls `fixture`).
        let _ = fixture("testsrc_720p30_5s.mp4");

        const STEP: u8 = 8;
        // 32, not 30 — an even count that halves cleanly, and a value that keeps
        // this file's "no re-declared window bound" grep
        // (`= (2|10|30)[^0-9]`) free of false positives it would have to be
        // taught to ignore.
        const SRC_FRAMES: usize = 32;
        const SRC_FPS: f64 = 60.0;
        let ramp = scratch_mp4("ramp60");
        write_level_ramp(&ramp.0, SRC_FRAMES, SRC_FPS, STEP, 320, 240);

        let ramp_info = engine::probe(&ramp.0).expect("the ramp fixture must probe");
        assert!(
            ramp_info.avg_frame_rate > f64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_FPS),
            "this gate is only meaningful if the SOURCE is over the ceiling; \
             the ramp probes at {}",
            ramp_info.avg_frame_rate
        );

        let dur_us = (SRC_FRAMES as f64 / SRC_FPS * 1_000_000.0) as i64;
        let bytes = extract_clip_range_mp4(&ramp.0, 0, dur_us, SRC_FPS)
            .expect("extraction from an over-ceiling source must succeed");
        let scratch = scratch_mp4("downconv");
        write_bytes(&scratch.0, &bytes);

        let out_info = engine::probe(&scratch.0).expect("the extracted mp4 must probe");
        assert!(
            out_info.avg_frame_rate <= f64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_FPS) + 0.01,
            "a {SRC_FPS} fps source must come out at or under the ceiling; got {}",
            out_info.avg_frame_rate
        );

        let want_out_frames = SRC_FRAMES / 2;
        let frames = engine::decode_frames_rgba_seq(&scratch.0, 0, want_out_frames, 0)
            .expect("the downconverted mp4 must decode");
        assert_eq!(
            frames.len(),
            want_out_frames,
            "half the source frames must survive a 2:1 downconversion"
        );

        let levels: Vec<f64> = frames.iter().map(mean_red).collect();
        println!("V2V-FPS out_fps={} levels={levels:?}", out_info.avg_frame_rate);

        let want_delta = f64::from(STEP) * 2.0;
        for pair in levels.windows(2) {
            let delta = pair[1] - pair[0];
            assert!(
                delta > 1.0,
                "a zero/negative step between consecutive output frames is a DUPLICATED \
                 frame, which is exactly what the ceiling must not do: levels {levels:?}"
            );
            assert!(
                (delta - want_delta).abs() <= 4.0,
                "each output frame must be TWO source frames on (expected a level step of \
                 {want_delta}, saw {delta:.2}): levels {levels:?}"
            );
        }
    }

    /// **The 413 regression, on REAL 4K bytes** — debug session
    /// `v2v-413-on-4k-source-unprobed-upload-window` (2026-08-13).
    ///
    /// The owner's first live clip edit on a 2160x4096 source met
    /// `HTTP 413 {"message":"Request Entity Too Large"}` twice, at $0.00 but with
    /// nothing actionable to show for it. The cause was not the upload window —
    /// probe F-4 read Runway's own signed S3 policy and it says
    /// `content-length-range 512 .. 209715200`, agreeing with both shipped upload
    /// constants. It was the INLINE arm: `RUNWAY_MAX_VIDEO_DATA_URI_BYTES` was
    /// the DOCUMENTED 16 MB per-asset figure, while probe F-5 MEASURED the API
    /// host's real request-body ceiling at 10 MiB. Every data URI in between was
    /// inlined here and refused by an edge proxy — before Runway's validator, so
    /// with no field path and no `docUrl`.
    ///
    /// # Why this fixture and this range
    ///
    /// `bars_4k30_5s.mp4` at **exactly two seconds** is the D-01 window's own
    /// FLOOR — the SHORTEST clip the product accepts for an edit. Its extracted
    /// payload measured 10 253 051 raw bytes / 13 670 758 bytes of data URI on
    /// the bundled LGPL sidecar, i.e. 3 184 998 bytes into the retired dead zone.
    /// So the bug was not reachable only by unusual input: the minimum legal 4K
    /// edit hit it.
    ///
    /// The assertion is on the PROPERTY, not on a byte count, so an encoder or
    /// sidecar change cannot make it vacuous or brittle: whatever this range
    /// weighs, it must not be inlined into a body the host will refuse.
    ///
    /// This test EXTENDS the D-02 pins above and changes none of them.
    #[test]
    fn a_minimum_window_4k_range_never_inlines_past_the_measured_body_ceiling() {
        let src = PathBuf::from(fixture("bars_4k30_5s.mp4"));
        let info = engine::probe(&src).expect("the 4K fixture must probe");
        assert!(
            info.width >= 3840 || info.height >= 3840,
            "this gate is only meaningful on a genuinely 4K source; got {}x{}",
            info.width,
            info.height
        );

        // Exactly the D-01 floor, read from the same const the refusal reads.
        let out_us = i64::from(agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS) * 1_000_000;
        assert!(
            clip_edit_window_check(out_us).is_ok(),
            "the floor is INSIDE the window, so this range is a legal edit"
        );

        let bytes = extract_clip_range_mp4(&src, 0, out_us, info.avg_frame_rate)
            .expect("extracting the minimum window from a 4K source must succeed");
        let raw = bytes.len();
        let data_uri_len = agent_gen::projected_video_data_uri_len(raw);
        let transport = agent_gen::video_transport_for_len(data_uri_len, raw)
            .expect("a legal 4K range must have a transport at all");

        println!(
            "V2V-413-REGRESSION raw={raw} data_uri={data_uri_len} \
             body_ceiling={} inline_budget={} transport={transport:?}",
            agent_gen::RUNWAY_MAX_REQUEST_BODY_BYTES,
            agent_gen::RUNWAY_MAX_VIDEO_DATA_URI_BYTES,
        );

        // Non-vacuous by construction: if this range ever stops being big enough
        // to have reached the dead zone, the test says so instead of passing for
        // the wrong reason.
        assert!(
            data_uri_len > agent_gen::RUNWAY_MAX_REQUEST_BODY_BYTES,
            "this gate is only meaningful while the minimum 4K range still \
             exceeds the measured {}-byte body ceiling; it is now {data_uri_len}",
            agent_gen::RUNWAY_MAX_REQUEST_BODY_BYTES
        );

        // The property itself.
        assert_eq!(
            transport,
            agent_gen::VideoTransport::Upload,
            "a {data_uri_len}-byte data URI is over the API host's measured \
             {}-byte request-body ceiling, so inlining it earns a contentless 413; \
             it must take the /v1/uploads arm (live-proven at HTTP 204 by probe F-4)",
            agent_gen::RUNWAY_MAX_REQUEST_BODY_BYTES
        );
    }

    /// The half-open contract and the two malformed calls, named at the stage
    /// they failed and never echoing bytes.
    #[test]
    fn extraction_refuses_a_degenerate_range_without_spawning() {
        let src = PathBuf::from(fixture("testsrc_720p30_5s.mp4"));

        let empty = extract_clip_range_mp4(&src, 1_000_000, 1_000_000, 30.0)
            .expect_err("an empty range must refuse — out_us is EXCLUSIVE");
        assert!(empty.contains("EXCLUSIVE"), "{empty}");
        assert!(empty.contains("range"), "{empty}");

        let backwards = extract_clip_range_mp4(&src, 2_000_000, 1_000_000, 30.0)
            .expect_err("a reversed range must refuse");
        assert!(backwards.contains("range"), "{backwards}");

        let negative = extract_clip_range_mp4(&src, -1, 1_000_000, 30.0)
            .expect_err("a negative in_us must refuse");
        assert!(negative.contains("negative"), "{negative}");
    }

    /// T-56-FOOTAGE-04: the user's own frames do not accumulate in the temp
    /// directory — on the success path OR on an error path that fails AFTER the
    /// file already exists.
    ///
    /// The two `in_us` values are used by no other test in this file, and the
    /// production temp name ends in its own `in_us`, so this count cannot see a
    /// concurrently-running sibling's in-flight file. (It could, and did, when
    /// the filter was the process-wide prefix alone — a real flake, caught by
    /// running the file with more than one thread.)
    ///
    /// The error arm is deliberately one that trips LATE: a range past the end
    /// of the media opens the decoder and creates the temp file, then finds no
    /// frames. The early argument refusals return before anything is created and
    /// would make this half vacuous.
    #[test]
    fn extraction_leaves_no_temp_file_behind() {
        let src = PathBuf::from(fixture("testsrc_720p30_5s.mp4"));
        const OK_IN_US: i64 = 777_777;
        const PAST_END_IN_US: i64 = 9_888_888; // the fixture is 5s long

        let prefix = format!("rudis-v2v-{}-", std::process::id());
        let leftovers = |in_us: i64| -> Vec<String> {
            let suffix = format!("-{in_us}.mp4");
            std::fs::read_dir(std::env::temp_dir())
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| n.starts_with(&prefix) && n.ends_with(&suffix))
                        .collect()
                })
                .unwrap_or_default()
        };

        assert!(
            leftovers(OK_IN_US).is_empty() && leftovers(PAST_END_IN_US).is_empty(),
            "precondition: neither marker range has a temp file yet"
        );

        extract_clip_range_mp4(&src, OK_IN_US, OK_IN_US + 1_000_000, 30.0)
            .expect("the success path must succeed");
        assert!(
            leftovers(OK_IN_US).is_empty(),
            "the SUCCESS path must delete its temp file; found {:?}",
            leftovers(OK_IN_US)
        );

        let err = extract_clip_range_mp4(&src, PAST_END_IN_US, PAST_END_IN_US + 1_000_000, 30.0)
            .expect_err("a range past the end of the media must error");
        assert!(
            err.contains("no frames"),
            "the late failure must name the decode stage: {err}"
        );
        assert!(
            leftovers(PAST_END_IN_US).is_empty(),
            "the ERROR path must delete its temp file too; found {:?}",
            leftovers(PAST_END_IN_US)
        );
    }
}
