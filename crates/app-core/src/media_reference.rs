//! The ONE decode path from a MediaBin item to a CONDITIONING REFERENCE PNG —
//! the image a paid external-generation call is conditioned on (GEN-10).
//!
//! # Why this module exists
//!
//! Until quick 260801-n7q there were TWO byte-identical copies of
//! [`decode_item_reference_png`] (`src-tauri/src/lib.rs:715-762` and
//! `crates/ffi/src/ctx.rs:272-314`) and TWO of [`clamp_raster_dims`]
//! (`src-tauri/src/lib.rs:317-324` and `crates/ffi/src/vision.rs:75-82`). Phase
//! 54.1's `deferred-items.md` §1 named the exact hazard that arrangement
//! creates — *"a future payload-size fix can land on one host only"* — and
//! recommended this hoist. This module IS that hoist, executed by the fix §1
//! predicted. Both hosts then shimmed onto these bodies, and
//! `src-tauri/tests/single_generation_path.rs` (the SC-5 source scan) pinned the
//! single-definition property for both functions so the fork could not silently
//! come back.
//!
//! ⚠ There is now only ONE host (`crates/ffi`), and that scan was deleted with
//! `src-tauri` at Phase 55 (GATE-07) without a successor — so nothing
//! mechanically stops a second copy of these bodies appearing. Do not add one.
//!
//! # The defect this module fixes
//!
//! A conditioning reference feeds a BILLED provider call. The old Image branch
//! REJECTED a still whose long edge exceeded the bound, while the Video branch
//! silently scaled a 4K frame down to it — so a frozen frame or PIP still used
//! as a video-transition endpoint failed where the identical picture, sourced
//! from a video, succeeded. That asymmetry burned paid generation attempts in a
//! live test (2026-08-01). The bound itself is UNCHANGED; only its ENFORCEMENT
//! changed, from reject to downscale.

/// The long-edge bound for a MEDIA-sourced conditioning reference (Phase 34.1,
/// GEN-10).
///
/// # Where 1568 actually comes from
///
/// It is the VISION-SNAPSHOT path's own long-edge discipline — Claude's
/// **Standard vision tier** (T-14.3-06 / T-34.1-03) — reused here rather than
/// inventing a second number. It is **not** a Runway pixel limit; Runway
/// imposes no pixel cap on a conditioning frame at all.
///
/// # The provider bound it actually serves
///
/// Runway's real constraint is a BYTE budget on an inlined `data:` URI:
/// `agent_gen::runway::RUNWAY_MAX_DATA_URI_BYTES` = 5 MiB
/// (`crates/agent-gen/src/runway.rs:168`). A PNG at a 1568px long edge sits
/// comfortably under it — which is that constant's own doc's words, and why it
/// describes itself as "a belt-and-braces pre-egress bound, not the primary
/// control". THIS clamp is the primary control.
///
/// # Enforcement
///
/// An oversized source is **DOWNSCALED** to this bound, never rejected (quick
/// 260801-n7q). A source already at or under it is passed through untouched.
///
/// Moved here from `src-tauri/src/lib.rs:669` and its reproduction at
/// `crates/ffi/src/ctx.rs:255`, so the bound has ONE home (Phase 54.1
/// `deferred-items.md` §1).
pub const MAX_MEDIA_REFERENCE_LONG_EDGE: u32 = 1568;

/// Preserve the reported aspect, capping the LONGEST side at `max_long_edge`
/// (1280 for the whiteboard board — continuity with the retired board's long
/// edge; 1568 for a decoded frame — Claude's Standard vision tier). Floors a
/// 200px short edge so an extreme panel resize never yields a degenerate
/// sliver. Never upscales *the aspect* (the scale factor is capped at 1.0).
/// Pure — unit-tested.
///
/// # The 200px floor is a TRAP for a resize gate
///
/// The floor applies even when `scale == 1.0`, so `clamp_raster_dims(100, 100,
/// 1568)` returns `(200, 200)` — dimensions LARGER than the input. Any caller
/// that resizes "whenever the clamped dims differ from the source dims" will
/// therefore UPSCALE small inputs. [`decode_item_reference_png`] avoids this by
/// gating its resize on `long_edge > max`, never on a dimension comparison; see
/// its Image branch.
///
/// Moved here from `src-tauri/src/lib.rs:317-324` (byte-identical to its port
/// at `crates/ffi/src/vision.rs:75-82`) by quick 260801-n7q. Both hosts now
/// re-export this one body, and both hosts' verbatim test tables now exercise
/// it.
pub fn clamp_raster_dims(panel_w: u32, panel_h: u32, max_long_edge: u32) -> (u32, u32) {
    let (w, h) = (panel_w.max(1) as f64, panel_h.max(1) as f64);
    let scale = (max_long_edge as f64 / w.max(h)).min(1.0);
    (
        (w * scale).round().max(200.0).min(max_long_edge as f64) as u32,
        (h * scale).round().max(200.0).min(max_long_edge as f64) as u32,
    )
}

/// Resample one decoded RGBA frame to `out_w` x `out_h`.
///
/// MIRRORS the mechanism of `engine::annotate::encode_jpeg_bytes`'s clamp block
/// (`crates/engine/src/annotate.rs:325-333`) — `image::imageops::resize` with
/// `FilterType::Triangle` — so the two paths produce the same pixels for the
/// same input. One deliberate difference: this KEEPS the alpha channel, because
/// its output is a PNG; the JPEG path drops to `Rgb8` first (JPEG has no alpha).
///
/// Mirrored rather than shared because `crates/engine/**` is FROZEN
/// (`engine-axis-freeze`, call-only for phases 50-55) — the engine exposes no
/// standalone "resize this `Frame`" entry point, and adding one is exactly the
/// kind of edit the freeze forbids.
fn downscale_frame_rgba(
    frame: &engine::Frame,
    out_w: u32,
    out_h: u32,
) -> Result<engine::Frame, String> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| {
            format!(
                "decoded frame buffer is {} bytes, not the {} a {}x{} RGBA frame needs",
                frame.rgba.len(),
                frame.width as usize * frame.height as usize * 4,
                frame.width,
                frame.height
            )
        })?;
    let resized = image::imageops::resize(&img, out_w, out_h, image::imageops::FilterType::Triangle);
    Ok(engine::Frame {
        width: out_w,
        height: out_h,
        rgba: resized.into_raw(),
    })
}

/// Decode ONE already-resolved MediaBin item at a SOURCE position into a
/// conditioning reference PNG — the shared body behind every GEN-10 reference
/// source (first-frame, clip-start and clip-end) on BOTH hosts.
///
/// `position_us` is a SOURCE-media timestamp (the same coordinate space
/// `Clip::in_us`/`out_us` live in), and is only meaningful for a Video item —
/// an Image has a single frame, so the position is ignored for it rather than
/// erroring. `label` names the caller's id in error text so a failure says which
/// clip or media item could not be read. Both semantics are unchanged by the
/// move.
///
/// # The long-edge bound, and how each branch reaches it
///
/// Both branches target the SAME
/// [`clamp_raster_dims`]`(.., MAX_MEDIA_REFERENCE_LONG_EDGE)` dimensions:
///
/// * **Video** — `engine::decode_frame_rgba_at_scaled` scales inside the
///   sidecar, as it always has.
/// * **Image** — `decode_frame_rgba_at_scaled` hard-rejects any non-Video item
///   at its front door (`crates/engine/src/ffmpeg.rs:735`, and the engine is
///   FROZEN), so a still is decoded natively and resampled HERE by
///   [`downscale_frame_rgba`]. Before quick 260801-n7q this branch returned an
///   `Err` instead — the defect: the same picture succeeded as a video frame
///   and failed as a still, burning paid attempts.
///
/// The resize is gated on the probed upright long edge being `>` the bound —
/// the identical predicate the old rejection used, and deliberately NOT
/// "clamped dims differ from source dims", which [`clamp_raster_dims`]'s 200px
/// floor would turn into a silent UPSCALE of small stills.
///
/// Moved here from `src-tauri/src/lib.rs:715-762` and its reproduction at
/// `crates/ffi/src/ctx.rs:272-314` by quick 260801-n7q, executing Phase 54.1
/// `deferred-items.md` §1. Everything except the Image branch is unchanged,
/// error strings included.
pub fn decode_item_reference_png(
    item: &rudis_core::MediaBinItem,
    label: &str,
    position_us: i64,
) -> Result<agent_gen::ReferenceImage, String> {
    let media_id = label;
    if item.media_kind == rudis_core::MediaKind::Audio {
        return Err(format!(
            "media item '{media_id}' is audio-only and has no visual content to use as a reference"
        ));
    }
    let (upright_w, upright_h) = if item.rotation_degrees == 90 || item.rotation_degrees == 270 {
        (item.height, item.width)
    } else {
        (item.width, item.height)
    };
    let (out_w, out_h) = clamp_raster_dims(upright_w, upright_h, MAX_MEDIA_REFERENCE_LONG_EDGE);
    let path = std::path::Path::new(&item.path);
    let frame = if item.media_kind == rudis_core::MediaKind::Video {
        engine::decode_frame_rgba_at_scaled(path, position_us, item.rotation_degrees, out_w, out_h)
            .map_err(|e| format!("decode {} at {position_us}us: {e}", item.path))?
    } else {
        // MediaKind::Image: `decode_frame_rgba_at_scaled` is Video-ONLY by
        // construction, so decode natively and resample here when the asset
        // exceeds the bound. Gated on the PROBED UPRIGHT LONG EDGE, never on a
        // dimension comparison against `(out_w, out_h)` — see this function's
        // doc and `clamp_raster_dims`'s 200px-floor note.
        let native = engine::decode_frame_rgba_at(path, 0, item.rotation_degrees)
            .map_err(|e| format!("decode {}: {e}", item.path))?;
        if upright_w.max(upright_h) > MAX_MEDIA_REFERENCE_LONG_EDGE {
            downscale_frame_rgba(&native, out_w, out_h)?
        } else {
            native
        }
    };
    let bytes = engine::encode_png_bytes(&frame).map_err(|e| e.to_string())?;
    Ok(agent_gen::ReferenceImage {
        bytes,
        width: frame.width,
        height: frame.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A still-image MediaBin item at `path`, probed as `w` x `h`. Field-by-field
    /// (MediaBinItem has no `Default`), the same construction shape
    /// `generation_host.rs`'s landing bridge and `inspect.rs`'s `witem` use.
    fn image_item(path: &std::path::Path, w: u32, h: u32) -> rudis_core::MediaBinItem {
        rudis_core::MediaBinItem {
            id: "m1".to_string(),
            path: path.to_string_lossy().into_owned(),
            media_kind: rudis_core::MediaKind::Image,
            duration_us: 0,
            width: w,
            height: h,
            fps: 0.0,
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

    /// Write a deterministic, NON-uniform `w` x `h` PNG (a two-axis gradient, so
    /// a resample cannot be confused with a solid fill) and return its path.
    fn write_gradient_png(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let mut img = image::RgbaImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([
                (x * 255 / w.max(1)) as u8,
                (y * 255 / h.max(1)) as u8,
                ((x + y) % 256) as u8,
                255,
            ]);
        }
        let path = dir.join(name);
        img.save(&path).expect("write test png");
        path
    }

    /// Test 1 (pure — no sidecar): the downscale helper hits the clamped dims
    /// exactly and PRESERVES the alpha channel.
    ///
    /// Alpha is uniform on the way in, so `Triangle` (a weighted average of
    /// equal values) must reproduce it exactly — an RGB-only resize would have
    /// dropped or defaulted it, which the PNG output would then ship wrong.
    #[test]
    fn downscale_hits_clamped_dims_and_keeps_alpha() {
        let (w, h) = (1920u32, 1080u32);
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[
                    (x % 256) as u8,
                    (y % 256) as u8,
                    ((x + y) % 256) as u8,
                    128,
                ]);
            }
        }
        let frame = engine::Frame {
            width: w,
            height: h,
            rgba,
        };

        let (out_w, out_h) = clamp_raster_dims(w, h, MAX_MEDIA_REFERENCE_LONG_EDGE);
        assert_eq!((out_w, out_h), (1568, 882), "clamp target for 1920x1080");

        let small = downscale_frame_rgba(&frame, out_w, out_h).expect("downscale");
        assert_eq!((small.width, small.height), (1568, 882));
        assert_eq!(
            small.rgba.len(),
            1568 * 882 * 4,
            "buffer matches the reported dims"
        );
        assert!(
            small.rgba.chunks_exact(4).all(|px| px[3] == 128),
            "alpha survives the resample (RGBA in, RGBA out)"
        );
    }

    /// Test 2 — THE INVERTED TEST (quick 260801-n7q, REF-01). An oversized STILL
    /// now SUCCEEDS, downscaled, where it used to be rejected outright.
    ///
    /// Verified on the OUTPUT, per CLAUDE.md rule 3: the returned PNG bytes are
    /// re-decoded and their dimensions asserted, so the claim is about the
    /// payload that would actually cross the wire to a billed provider call —
    /// not about a struct field (T-n7q-01).
    #[test]
    fn oversized_still_downscales_instead_of_being_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_gradient_png(dir.path(), "oversized.png", 2560, 1440);
        let item = image_item(&path, 2560, 1440);

        let reference = decode_item_reference_png(&item, "m1", 0)
            .expect("an oversized still is DOWNSCALED, never rejected");

        assert_eq!(
            (reference.width, reference.height),
            (1568, 882),
            "2560x1440 scales to the 1568 long edge with aspect preserved"
        );
        let decoded = image::load_from_memory(&reference.bytes).expect("re-decode the emitted PNG");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (1568, 882),
            "the PNG PAYLOAD itself is bounded, not just the reported dims"
        );
    }

    /// Test 3 (REF-03): an at-or-under-cap still passes through UNTOUCHED, and a
    /// tiny one is NOT floored up to 200.
    ///
    /// The 120x90 case is the load-bearing one: `clamp_raster_dims(120, 90,
    /// 1568)` returns `(200, 200)` because of the short-edge floor, so a resize
    /// gated on "dims differ" would upscale this still AND distort its aspect.
    /// The gate is on the long edge for exactly this reason.
    #[test]
    fn under_cap_stills_pass_through_unresized() {
        let dir = tempfile::tempdir().expect("tempdir");

        let ordinary = write_gradient_png(dir.path(), "ordinary.png", 640, 360);
        let reference = decode_item_reference_png(&image_item(&ordinary, 640, 360), "m1", 0)
            .expect("an under-cap still decodes");
        assert_eq!((reference.width, reference.height), (640, 360), "no resize");

        let tiny = write_gradient_png(dir.path(), "tiny.png", 120, 90);
        let reference = decode_item_reference_png(&image_item(&tiny, 120, 90), "m1", 0)
            .expect("a tiny still decodes");
        assert_eq!(
            (reference.width, reference.height),
            (120, 90),
            "never upscaled to the clamp's 200px short-edge floor"
        );
        assert_eq!(
            clamp_raster_dims(120, 90, MAX_MEDIA_REFERENCE_LONG_EDGE),
            (200, 200),
            "and the floor really would have upscaled it, had the gate been on dims"
        );
    }

    /// Test 4 (REF-03, no sidecar): the audio rejection is UNCHANGED by the move
    /// — same condition, same words.
    #[test]
    fn audio_only_item_is_still_rejected() {
        let mut item = image_item(std::path::Path::new("does-not-exist.wav"), 0, 0);
        item.media_kind = rudis_core::MediaKind::Audio;

        let err = decode_item_reference_png(&item, "a1", 0)
            .expect_err("audio has no visual content to condition on");
        assert!(
            err.contains("audio-only and has no visual content"),
            "verbatim rejection preserved, got: {err}"
        );
    }
}
