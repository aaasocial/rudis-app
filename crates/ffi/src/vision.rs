//! The FFI host's AGENT-VISION snapshot cluster (Phase 54.1, plan 54.1-02).
//!
//! A re-implementation of `src-tauri/src/lib.rs`'s agent-vision snapshot
//! cluster for the FFI host — behaviour is reproduced EXACTLY; the two
//! implementations must produce the same picture for the same project or the
//! agent sees something different depending on which shell hosts it.
//! Collapsing both onto one shared home is Phase 55+ work.
//!
//! # Why a re-implementation and not a `pub` widening
//!
//! `crates/ffi` **cannot link the shell crate at all** — that is the whole
//! point of the C ABI — so the same discipline `panel/overlay.rs` records
//! applies here verbatim. Every item below names the `src-tauri/src/lib.rs`
//! line it was ported from, so a reviewer can diff the two by eye.
//!
//! # What is NOT duplicated
//!
//! The ink PAINTER is not re-derived: [`whiteboard_snapshot_png`] and
//! [`vision_snapshot_png`]/[`vision_snapshot_jpeg`] all call
//! [`crate::panel::overlay::draw_annotations_onto_styled`], the copy this crate
//! already ported once at Phase 51 for the live preview overlay. Two
//! independent renderers of the same ink is exactly the drift class that port
//! warns about — and "what the agent SEES" and "what the user sees" disagreeing
//! is the worst version of it. The FRAME-LINKED filter is likewise reused
//! (`overlay::frame_linked`), not re-written.
//!
//! # The one permitted mechanical substitution
//!
//! `tauri::async_runtime::spawn_blocking` becomes `tokio::task::spawn_blocking`
//! (a 1:1 substitute — the former IS a tokio runtime, proven by `src-tauri`'s
//! own `mod spike` regression test). `FfiAppCtx::block_on` drives a real
//! multi-threaded runtime with `enable_all()`, so the handle joins exactly as
//! it does under the shell host. Nothing else changed.
// Plan 54.1-02 carried a module-wide `#![allow(dead_code)]` here, whose written
// reason was that the two PNG BYTE producers (`vision_snapshot_png`,
// `whiteboard_snapshot_png`) had no production caller YET and that plan 54.1-04's
// `resolve_reference_image` was the caller they were ported for. That plan has
// landed and both are called from `ctx.rs`'s GEN-10 Sketch/Frame arms, so the
// allow is DELETED rather than left standing with a reason that is no longer
// true — a stale allow is how a genuinely dead item hides.

use crate::panel::overlay;

/// Whiteboard background for the AGENT-FACING vision snapshot
/// (`src-tauri/src/lib.rs:288`; originally Phase 14.3 D-04, revised by the
/// debug session `canvas-background-leaks-into-agent-vision`, 2026-07-22).
///
/// TRANSPARENT WHITE — alpha `0x00`, so untouched board pixels carry real
/// transparency and the agent can tell "the user drew a dark rectangle" apart
/// from "this board's page renders dark". The earlier opaque near-black value
/// made a cosmetic UI backdrop indistinguishable from drawn content, which
/// stayed invisible until image generation gave the agent a reason to
/// TRANSCRIBE what it saw. RGB `0xFFFFFF` is the neutral fallback for any
/// downstream consumer that drops rather than honours alpha.
///
/// Deliberately DECOUPLED from whatever background the on-screen Canvas paints:
/// that token describes what the USER sees, never what counts as "content" for
/// the agent.
pub(crate) const WHITEBOARD_BG: [u8; 4] = [0xFF, 0xFF, 0xFF, 0x00];

/// The CHAT-HISTORY vision-snapshot JPEG quality (`src-tauri/src/lib.rs:508`;
/// `bug/agent-history-413`). Mirrors the `inspect_*` JPEG discipline
/// (`INSPECT_JPEG_QUALITY = 70`) while keeping the 1568px Standard-vision-tier
/// long-edge clamp (T-14.3-06) rather than `inspect_*`'s tighter 512px, since
/// THIS is the agent's one full look at the frame each turn.
pub(crate) const VISION_HISTORY_JPEG_QUALITY: u8 = 70;

// Quick 260801-n7q: the "byte-identical port of `src-tauri/src/lib.rs:317-324`"
// that used to sit here is GONE, along with the `src-tauri` original it ported.
// `clamp_raster_dims` now lives ONCE, in `app_core::media_reference`, having
// travelled there with `decode_item_reference_png` (which derives its DoS bound
// from it — a second copy of the clamp is a second copy of the bound).
//
// `crate::vision::clamp_raster_dims` still resolves, so this module's own caller
// and `ctx.rs`'s are unchanged, and the verbatim test table at
// `ctx.rs:1589-1622` stays exactly where it is as the drift tripwire — it now
// pins the shared body, which is strictly more than it used to pin.
pub(crate) use app_core::media_reference::clamp_raster_dims;

/// Which media file, at which source timestamp, the agent's frame snapshot
/// should decode — resolved PURELY from `Project` state, with zero dependency
/// on any composited/on-screen pixel. Port of `src-tauri/src/lib.rs:217-243`.
///
/// Source mode: the loaded MediaBin clip, iff it is Video. Program mode:
/// `active_layers_at(playback.position_us)`'s FIRST video hit — `find_map`
/// rather than "take the top layer and reject if not video", because a
/// non-video overlay sitting above a real video frame must not shadow it (the
/// `preview-multilayer-not-compositing` bug class the live compositor fixed
/// first).
pub(crate) fn resolve_active_snapshot_frame(
    project: &rudis_core::Project,
) -> Option<(std::path::PathBuf, i64, u32)> {
    match project.preview_mode {
        rudis_core::PreviewMode::Source => {
            let media_id = project.source_playback.loaded_media_id.as_ref()?;
            let item = project.media_bin.iter().find(|m| &m.id == media_id)?;
            if item.media_kind != rudis_core::MediaKind::Video {
                return None;
            }
            Some((
                std::path::PathBuf::from(&item.path),
                project.source_playback.position_us,
                item.rotation_degrees,
            ))
        }
        rudis_core::PreviewMode::Program => project
            .timeline
            .active_layers_at(project.playback.position_us)
            .into_iter()
            .find_map(|hit| {
                let item = project.media_bin.iter().find(|m| m.id == hit.media_id)?;
                if item.media_kind != rudis_core::MediaKind::Video {
                    return None;
                }
                Some((
                    std::path::PathBuf::from(&item.path),
                    hit.source_us,
                    item.rotation_degrees,
                ))
            }),
    }
}

/// The FRAME-LINKED subset of the canvas — the marks that belong on real video
/// (`src-tauri/src/lib.rs:381-389`).
///
/// Deliberately a THIN FORWARD to [`overlay::frame_linked`], the filter this
/// crate already ported at Phase 51 for the live preview mirror, rather than a
/// second copy of the same three-line predicate: the preview overlay and the
/// agent's frame snapshot must agree, byte for byte, on which marks belong on
/// the video (Phase 14.2 SC-2 no-leak / T-51-13).
pub(crate) fn frame_linked_marks(project: &rudis_core::Project) -> Vec<rudis_core::Annotation> {
    overlay::frame_linked(&project.canvas)
}

/// The WHITEBOARD subset of the canvas — the project-global marks the agent
/// sees as a rasterized PNG on a blank board, NEVER on real video. The exact
/// complement of [`frame_linked_marks`]. Pure, decode-free. Port of
/// `src-tauri/src/lib.rs:394-402`.
///
/// (No `overlay::` twin to forward to: the preview mirror has no reason to know
/// about whiteboard marks — excluding them is its whole contract — so this
/// filter genuinely arrives for the first time here.)
pub(crate) fn whiteboard_marks(project: &rudis_core::Project) -> Vec<rudis_core::Annotation> {
    project
        .canvas
        .annotations
        .iter()
        .filter(|a| a.space == rudis_core::AnnotationSpace::Whiteboard)
        .cloned()
        .collect()
}

/// A synthetic blank `engine::Frame` of `width x height`, every pixel filled
/// with `rgba_bg` — the whiteboard's backdrop, so project-global marks
/// rasterize onto a clean page and never onto real video. Port of
/// `src-tauri/src/lib.rs:593-604`.
pub(crate) fn blank_whiteboard_frame(width: u32, height: u32, rgba_bg: [u8; 4]) -> engine::Frame {
    let (w, h) = (width.max(1), height.max(1));
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for _ in 0..(w as usize * h as usize) {
        rgba.extend_from_slice(&rgba_bg);
    }
    engine::Frame {
        width: w,
        height: h,
        rgba,
    }
}

/// The source's UPRIGHT pixel dims for the media at `path`, portrait rotation
/// swapping w/h — the decode target `clamp_raster_dims` is derived from. Split
/// out only because [`vision_snapshot_png`] and [`vision_snapshot_jpeg`] both
/// need it; the body is `src-tauri/src/lib.rs:454-464` unchanged.
fn upright_source_dims(
    project: &rudis_core::Project,
    path: &std::path::Path,
    rotation_degrees: u32,
) -> Option<(u32, u32)> {
    project
        .media_bin
        .iter()
        .find(|m| std::path::Path::new(&m.path) == path)
        .map(|m| {
            if rotation_degrees == 90 || rotation_degrees == 270 {
                (m.height, m.width)
            } else {
                (m.width, m.height)
            }
        })
}

/// The BYTE-PRODUCING half of the frame vision snapshot, PNG (lossless):
/// decode the active preview frame from DISK, paint the frame-linked ink on it,
/// PNG-encode, and report the raster dims alongside. Port of
/// `src-tauri/src/lib.rs:434-500` (`build_vision_snapshot_png`).
///
/// Consumed by the GEN-10 reference-image conditioning seam (plan 54.1-04), so
/// what conditions an outbound generation call is byte-identical to what the
/// agent SAW. The CHAT-HISTORY block deliberately does NOT use this — see
/// [`vision_snapshot_jpeg`].
///
/// Skipped entirely (returns `None`, zero blocking work) when the canvas has no
/// frame-linked marks — token/cost discipline (T-13-15), and the reason a
/// whiteboard-only project produces no frame snapshot at all. ANY failure logs
/// and returns `None`: a failed snapshot degrades the turn to text-only, never
/// fails it (T-13-16).
pub(crate) async fn vision_snapshot_png(
    project: &rudis_core::Project,
) -> Option<(Vec<u8>, u32, u32)> {
    let annotations = frame_linked_marks(project);
    if annotations.is_empty() {
        return None;
    }
    let (path, source_us, rotation_degrees) = resolve_active_snapshot_frame(project)?;
    // E-01 / T-14.3-06 (D-10 lever): cap the decoded frame's long edge at 1568
    // (Claude's Standard vision tier — the model downscales past this SERVER-
    // side anyway, so a full-native-res decode+PNG+base64 is wasted LOCAL
    // cost). The media item always exists here (resolve_active_snapshot_frame
    // just found it); the `None` fallback to a plain full-res decode is
    // defensive-only.
    let source_dims = upright_source_dims(project, &path, rotation_degrees);
    let joined = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, u32, u32), String> {
        let mut frame = decode_for_snapshot(&path, source_us, rotation_degrees, source_dims)?;
        // Capture the raster dims BEFORE drawing — annotations paint onto the
        // frame in place and never change its width/height.
        let (w, h) = (frame.width, frame.height);
        overlay::draw_annotations_onto_styled(
            &mut frame,
            &annotations,
            overlay::OVERLAY_INK,
            false,
        );
        let bytes = engine::encode_png_bytes(&frame).map_err(|e| e.to_string())?;
        Ok((bytes, w, h))
    })
    .await;
    match joined {
        Ok(Ok(result)) => Some(result),
        Ok(Err(e)) => {
            eprintln!("vision snapshot skipped (falling back to text-only turn): {e}");
            None
        }
        Err(e) => {
            eprintln!("vision snapshot task failed (falling back to text-only turn): {e}");
            None
        }
    }
}

/// The decode arm shared by the PNG and JPEG byte producers — the scaled path
/// when the source dims are known, a plain full-res decode otherwise
/// (`src-tauri/src/lib.rs:466-480`, identical in both of its builders).
fn decode_for_snapshot(
    path: &std::path::Path,
    source_us: i64,
    rotation_degrees: u32,
    source_dims: Option<(u32, u32)>,
) -> Result<engine::Frame, String> {
    match source_dims {
        Some((sw, sh)) => {
            let (out_w, out_h) = clamp_raster_dims(sw, sh, 1568);
            engine::decode_frame_rgba_at_scaled(path, source_us, rotation_degrees, out_w, out_h)
                .map_err(|e| format!("decode {} at {source_us}us: {e}", path.display()))
        }
        None => engine::decode_frame_rgba_at(path, source_us, rotation_degrees)
            .map_err(|e| format!("decode {} at {source_us}us: {e}", path.display())),
    }
}

/// [bug/agent-history-413] the JPEG sibling of [`vision_snapshot_png`], used
/// ONLY for the CHAT-HISTORY vision block. Port of
/// `src-tauri/src/lib.rs:528-579`.
///
/// `AgentSession.history` is permanent and every request re-sends the WHOLE
/// history verbatim, so a lossless PNG frame snapshot (~2-4MB) is the single
/// biggest per-turn contributor to the Anthropic Messages API's 32MB
/// request-body cap (`413 request_too_large`); a 1568px JPEG at quality 70 is
/// ~150-250KB.
pub(crate) async fn vision_snapshot_jpeg(
    project: &rudis_core::Project,
) -> Option<(Vec<u8>, u32, u32)> {
    let annotations = frame_linked_marks(project);
    if annotations.is_empty() {
        return None;
    }
    let (path, source_us, rotation_degrees) = resolve_active_snapshot_frame(project)?;
    let source_dims = upright_source_dims(project, &path, rotation_degrees);
    let joined = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, u32, u32), String> {
        let mut frame = decode_for_snapshot(&path, source_us, rotation_degrees, source_dims)?;
        let (w, h) = (frame.width, frame.height);
        overlay::draw_annotations_onto_styled(
            &mut frame,
            &annotations,
            overlay::OVERLAY_INK,
            false,
        );
        let bytes = engine::encode_jpeg_bytes(&frame, 1568, VISION_HISTORY_JPEG_QUALITY)
            .map_err(|e| e.to_string())?;
        Ok((bytes, w, h))
    })
    .await;
    match joined {
        Ok(Ok(result)) => Some(result),
        Ok(Err(e)) => {
            eprintln!("vision snapshot (jpeg) skipped (falling back to text-only turn): {e}");
            None
        }
        Err(e) => {
            eprintln!("vision snapshot (jpeg) task failed (falling back to text-only turn): {e}");
            None
        }
    }
}

/// The CHAT-HISTORY frame vision block — JPEG, per `bug/agent-history-413`.
/// Port of `src-tauri/src/lib.rs:581-585`.
pub(crate) async fn vision_snapshot_block(
    project: &rudis_core::Project,
) -> Option<agent_llm::ContentBlock> {
    vision_snapshot_jpeg(project)
        .await
        .map(|(bytes, _, _)| agent_llm::image_content_block_jpeg(&bytes))
}

/// The BYTE-PRODUCING half of the WHITEBOARD snapshot: rasterize every
/// project-global whiteboard mark onto a blank `raster_w x raster_h` board
/// ([`WHITEBOARD_BG`]) in the shared ink, PNG-encode. Port of
/// `src-tauri/src/lib.rs:626-652` (`build_whiteboard_snapshot_png`).
///
/// The dims are the `(raster_w, raster_h)` params echoed back — the blank board
/// IS constructed at exactly that size, so there is nothing further to compute.
/// Same discipline as the frame snapshot: `None` with zero blocking work when
/// there are no whiteboard marks, the draw+encode runs off the async runtime,
/// and ANY failure logs and degrades to `None` rather than failing the turn.
pub(crate) async fn whiteboard_snapshot_png(
    project: &rudis_core::Project,
    raster_w: u32,
    raster_h: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    let marks = whiteboard_marks(project);
    if marks.is_empty() {
        return None;
    }
    let joined = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let mut frame = blank_whiteboard_frame(raster_w, raster_h, WHITEBOARD_BG);
        overlay::draw_annotations_onto_styled(&mut frame, &marks, overlay::OVERLAY_INK, false);
        engine::encode_png_bytes(&frame).map_err(|e| e.to_string())
    })
    .await;
    match joined {
        Ok(Ok(png_bytes)) => Some((png_bytes, raster_w, raster_h)),
        Ok(Err(e)) => {
            eprintln!("whiteboard snapshot skipped (falling back to no board image): {e}");
            None
        }
        Err(e) => {
            eprintln!("whiteboard snapshot task failed (falling back to no board image): {e}");
            None
        }
    }
}

/// The whiteboard vision block — PNG, deliberately NOT JPEG: the board is
/// flat-colour line art, where JPEG ringing smears strokes (the
/// `canvas-sketch-generate-picture-garbled-output` /
/// `generate-me-rectangle-ignores-drawn-geometry` debug sessions;
/// `crates/agent-llm/src/vision.rs`'s module doc records the same split). Port
/// of `src-tauri/src/lib.rs:654-662`.
pub(crate) async fn whiteboard_snapshot_block(
    project: &rudis_core::Project,
    raster_w: u32,
    raster_h: u32,
) -> Option<agent_llm::ContentBlock> {
    whiteboard_snapshot_png(project, raster_w, raster_h)
        .await
        .map(|(bytes, _, _)| agent_llm::image_content_block_png(&bytes))
}
