//! Object tracking (Phase 30, TRK-01/TRK-02) — the engine-facing tracker API.
//!
//! ## WAVE-0 TOOLCHAIN DECISION (Plan 30-01): **FALLBACK — Python `opencv-contrib-python` sidecar**
//!
//! The Wave-0 spike installed the FULL native toolchain (libclang extracted
//! no-admin from the LLVM installer, plus a SHA256-pinned prebuilt Apache-2.0
//! OpenCV 4.11.0 Windows SDK) and the native `opencv` crate compiled, linked, and
//! generated bindings cleanly. But the gate FAILED for a fundamental,
//! well-troubleshot reason: **the license-clean, contrib-FREE official OpenCV
//! Windows SDK does not ship `TrackerCSRT`/`TrackerKCF` at all.** Those classical
//! correlation-filter trackers live ONLY in `opencv_contrib`'s `tracking` module.
//! The main `video` module carries `TrackerMIL` (weightless, but not the SC-1
//! default) plus `TrackerGOTURN`/`DaSiamRPN`/`Nano`/`Vit` (all DNN trackers that
//! require model weights — violating SC-3's no-weights rule). Research Assumption
//! A2 ("CSRT/KCF promoted to the main video module since 4.5.1") is therefore
//! factually wrong for the shipped OpenCV releases, verified by inspecting the
//! generated bindings AND the SDK's `opencv2/video/tracking.hpp` directly.
//!
//! Since SC-1 mandates CSRT and the only license-clean, weightless, reproducible
//! way to get real CSRT/KCF on Windows is `opencv-contrib-python` (a prebuilt
//! MIT-wrapper / Apache-2.0-OpenCV wheel — no from-source `opencv_contrib` build,
//! no vcpkg risk), the plan's explicit **FAIL → FALLBACK** branch applies.
//!
//! ## Backend
//!
//! A bundled, PSF-licensed **embeddable Python** + `opencv-contrib-python` +
//! `numpy`, plus `track_cli.py` (a stdio JSON CLI), live under
//! `runtime/binaries/opencv/` and are invoked exactly like the existing
//! `ffmpeg.exe` / `whisper-cli.exe` sidecars: `std::process::Command`, bundled-
//! first resolution (never a PATH-resolved python — threat T-30-01), args passed
//! via the argument array (never shell-interpolated — threat T-30-07). NO network
//! at runtime; NO model weights (CSRT/KCF are correlation filters). See
//! PROVENANCE.md Entry 12.
//!
//! ## Backend-agnostic contract
//!
//! [`track_region`] (added in Plan 30-01 Task 2) keeps the IDENTICAL signature
//! the native PRIMARY would have exposed, so downstream waves (30-02 confidence
//! heuristic, 30-03 `track_object` tool) never learn which backend ran. MOSSE is
//! available in the wheel but intentionally omitted from the public API
//! (contrib-only elsewhere; SC-1 needs only CSRT + KCF).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::ffmpeg::Frame;
use crate::EngineError;

/// Which classical correlation-filter tracker to run. Both come from the
/// bundled `opencv-contrib-python` wheel's tracking module (Apache-2.0 OpenCV,
/// weightless). MOSSE is available in the wheel but intentionally omitted —
/// SC-1 needs only CSRT (accurate default) + KCF (speed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerKind {
    /// CSRT — the accurate default (SC-1's mandated tracker).
    Csrt,
    /// KCF — faster, less accurate; the roadmap's "for speed" alternative.
    Kcf,
}

impl TrackerKind {
    /// The sidecar's `tracker` argument string.
    fn as_arg(self) -> &'static str {
        match self {
            TrackerKind::Csrt => "csrt",
            TrackerKind::Kcf => "kcf",
        }
    }
}

/// Per-frame confidence classification. In Plan 30-01 this is deliberately
/// coarse: every tracked frame is [`TrackConfidence::Ok`] and a raw
/// `update() == false` (the sidecar's `"lost"` status) is
/// [`TrackConfidence::Lost`]. The [`TrackConfidence::LowConfidence`] drift
/// heuristic (bbox-jump / area-ratio / bounds-exit thresholds) is filled in by
/// Plan 30-02 — OpenCV itself exposes only a success bool, no numeric score
/// (Research Pitfall 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackConfidence {
    Ok,
    LowConfidence,
    Lost,
}

// ---------------------------------------------------------------------------
// Lost-track confidence heuristic (Plan 30-02, SC-4)
// ---------------------------------------------------------------------------
//
// OpenCV's `Tracker::update()` returns ONLY a success bool — there is NO numeric
// confidence score (Research Pitfall 3). A `true` result means "the internal
// correlation search converged," NOT "this box is the right object": CSRT/KCF
// can silently drift or collapse while still returning `true`. These constants
// are a SUPPLEMENTARY drift/loss heuristic layered on top of the raw bool — an
// original engineering design (Assumption A1), NOT a documented OpenCV feature.
//
// CALIBRATED against `test-media/track_occluded.mp4` (a moving textured patch
// fully occluded by a black overlay from frame ~75) and cross-checked against
// `test-media/track_linear.mp4` (legitimate fast linear motion that must NOT be
// flagged) — see `crates/engine/tests/tracking.rs`. MEASURED basis (recorded
// from `--nocapture` instrumentation runs, not guessed — Assumption A1 closed):
//   * Legitimate motion (track_linear): MAX per-frame centre jump / bbox
//     diagonal = 0.082; bbox area stays ~0.81–1.21x the seed area.
//   * Occlusion onset (track_occluded, frame 75): the box teleports
//     (139,89)->(128,113), jump/diag = 0.52 — 6.3x the legitimate max — so the
//     jump rule flags it `LowConfidence`; the box then freezes on the black
//     frame until CSRT's update() finally returns false at frame 84, which the
//     `"lost"` rule classifies `Lost` and TRUNCATES the path.
// The 0.30 jump threshold sits ~3.6x above the legitimate envelope (0.082) and
// well below the occlusion teleport (0.52); the area band (0.35–3.0) brackets
// the ~0.81–1.21 legitimate range with margin — so neither fixture is
// mis-classified (no over-flag on real motion, real flag on the occlusion).

/// A one-frame bbox-centre jump exceeding this fraction of the bbox diagonal is
/// physically implausible for a coherently-tracked object → `LowConfidence`.
/// Legitimate motion measured 0.082 here; the occlusion teleport measured 0.52.
const LOST_JUMP_DIAG_RATIO: f32 = 0.30;
/// The tracked bbox area collapsing below this fraction of the SEED area (the
/// filter locked onto a sub-feature / the target shrank away) → `LowConfidence`.
/// Legitimate motion measured a minimum area ratio of ~0.81.
const LOST_AREA_MIN_RATIO: f32 = 0.35;
/// The tracked bbox area ballooning above this multiple of the SEED area (the
/// filter spread onto the background) → `LowConfidence`. Legitimate max ~1.21.
const LOST_AREA_MAX_RATIO: f32 = 3.0;

/// Layer the supplementary lost-track heuristic onto the raw sidecar per-frame
/// results, TRUNCATING the path at the first hard `Lost` (never fabricating or
/// emitting boxes past a loss — SC-4's "flag, don't silently continue" mandate).
///
/// Classification per frame (in priority order):
///   * sidecar `"lost"` (OpenCV `update() == false`) → `Lost`, truncate.
///   * bbox exits the frame bounds → `Lost`, truncate.
///   * centre jumped > `LOST_JUMP_DIAG_RATIO` * the bbox diagonal vs the previous
///     frame → `LowConfidence` (kept in the path, but flagged).
///   * bbox area < `LOST_AREA_MIN_RATIO` or > `LOST_AREA_MAX_RATIO` of the seed
///     area → `LowConfidence`.
///   * otherwise → `Ok`.
fn classify_confidence(
    raw: &[SidecarFrame],
    initial_bbox: (f32, f32, f32, f32),
    frame_w: f32,
    frame_h: f32,
) -> Vec<TrackResult> {
    let seed_area = (initial_bbox.2 * initial_bbox.3).max(1.0);
    let mut out = Vec::with_capacity(raw.len());
    let mut prev_center: Option<(f32, f32)> = None;

    for r in raw {
        let (x, y, w, h) = (r.bbox[0], r.bbox[1], r.bbox[2], r.bbox[3]);
        let center = (x + w / 2.0, y + h / 2.0);

        // (a) Hard loss reported by the backend (update() == false).
        if r.status == "lost" {
            out.push(TrackResult {
                frame_index: r.frame_index,
                bbox_px: (x, y, w, h),
                confidence: TrackConfidence::Lost,
            });
            break;
        }

        // (d) Hard loss: the tracked box has left the frame — a pinned overlay
        // past this point would be off-screen / wrong. Truncate.
        let out_of_bounds =
            x < 0.0 || y < 0.0 || x + w > frame_w || y + h > frame_h || w <= 0.0 || h <= 0.0;
        if out_of_bounds {
            out.push(TrackResult {
                frame_index: r.frame_index,
                bbox_px: (x, y, w, h),
                confidence: TrackConfidence::Lost,
            });
            break;
        }

        // Soft-flag heuristics (kept in the path, marked LowConfidence).
        let mut confidence = TrackConfidence::Ok;

        // (b) Implausible one-frame centre jump relative to the box diagonal.
        let diag = (w * w + h * h).sqrt().max(1.0);
        if let Some((px, py)) = prev_center {
            let jump = ((center.0 - px).powi(2) + (center.1 - py).powi(2)).sqrt();
            if jump > LOST_JUMP_DIAG_RATIO * diag {
                confidence = TrackConfidence::LowConfidence;
            }
        }

        // (c) Area collapse / balloon vs the seed.
        let area_ratio = (w * h) / seed_area;
        if area_ratio < LOST_AREA_MIN_RATIO || area_ratio > LOST_AREA_MAX_RATIO {
            confidence = TrackConfidence::LowConfidence;
        }

        out.push(TrackResult {
            frame_index: r.frame_index,
            bbox_px: (x, y, w, h),
            confidence,
        });
        prev_center = Some(center);
    }

    out
}

/// One tracked frame's result: the source-pixel bbox at a window-relative index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackResult {
    /// 0-based, relative to the analysis window (frame 0 = the seed frame).
    pub frame_index: u32,
    /// `(x, y, w, h)` in SOURCE pixel coordinates (the decoded [`Frame`] space).
    pub bbox_px: (f32, f32, f32, f32),
    pub confidence: TrackConfidence,
}

/// One raw per-frame result parsed back from the sidecar (internal shape shared
/// by the smoke path and [`track_region`]).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SidecarFrame {
    pub frame_index: u32,
    /// `[x, y, w, h]` in SOURCE pixel coords.
    pub bbox: [f32; 4],
    /// `"ok"` or `"lost"`.
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct SidecarResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Vec<SidecarFrame>,
    #[serde(default)]
    #[allow(dead_code)]
    lost_at: Option<u32>,
}

/// Resolve the bundled tracking sidecar `(python.exe, track_cli.py)` using the
/// SAME bundled-first order as [`crate::ffmpeg::locate`] — the shipped build
/// NEVER trusts a PATH-resolved python (threat T-30-01).
///
/// Order:
///   1. `RUDIS_TRACK_DIR` — explicit path to the `opencv/` bundle dir.
///   2. `RUDIS_FFMPEG_DIR`/opencv — an OVERRIDE SLOT ONLY, for when that var is
///      already pointed at a bundle dir by hand or by a test. Nothing in the
///      shipped app sets it (see [`crate::ffmpeg::locate`]).
///   3. exe-adjacent `binaries/opencv/` and `opencv/` — THE SHIPPED LAYOUTS,
///      populated by `dotnet publish` via `StageRustFfiPublish`
///      (`crates/ffi/Rudis.Ffi.targets`), which stages `runtime/binaries/**`
///      into `$(PublishDir)\binaries\` with the tree preserved.
///   4. DEV fallback: `<repo>/runtime/binaries/opencv/` (relocated out of the
///      retired `src-tauri/` by Phase 55 plan 55-01), so
///      `cargo test -p engine --features tracking` works with no env set.
pub(crate) fn locate_sidecar() -> Result<(PathBuf, PathBuf), EngineError> {
    for dir in sidecar_bundle_candidates() {
        let python = dir.join("python").join("python.exe");
        let script = dir.join("track_cli.py");
        if python.is_file() && script.is_file() {
            return Ok((python, script));
        }
    }
    Err(EngineError::BinaryNotFound(
        "opencv tracking sidecar (python/python.exe + track_cli.py) — run scripts/windows/fetch-opencv-sdk.ps1".into(),
    ))
}

fn sidecar_bundle_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::var_os("RUDIS_TRACK_DIR") {
        out.push(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("RUDIS_FFMPEG_DIR") {
        out.push(Path::new(&dir).join("opencv"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            out.push(exe_dir.join("binaries").join("opencv"));
            out.push(exe_dir.join("opencv"));
        }
    }
    // DEV fallback: repo-relative bundle, computed from the crate dir at compile
    // time (crates/engine -> repo root -> runtime/binaries/opencv; relocated out
    // of src-tauri/ by Phase 55 plan 55-01 so it survives GATE-07's deletion).
    let dev = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("runtime")
        .join("binaries")
        .join("opencv");
    out.push(dev);
    out
}

/// Serialize `frames` (all same dims) to a temp raw-RGBA blob + invoke the
/// sidecar tracker; return the parsed per-frame results. Shared by
/// [`track_region`] and [`csrt_init_update_smoke`].
///
/// `tracker` is `"csrt"` or `"kcf"`. `init_bbox` is `(x, y, w, h)` in source px.
/// Caller MUST have already validated `frames` non-empty + bbox finiteness/bounds
/// (the FFI-boundary crash guard, T-30-03) before calling this.
pub(crate) fn run_sidecar(
    frames: &[Frame],
    init_bbox: (f32, f32, f32, f32),
    tracker: &str,
) -> Result<Vec<SidecarFrame>, EngineError> {
    let (w, h) = (frames[0].width, frames[0].height);
    let frame_bytes = (w as usize) * (h as usize) * 4;

    // Write all frames as one concatenated RGBA blob to a unique temp file.
    let blob_path = std::env::temp_dir().join(format!(
        "rudis-track-{}-{}.rgba",
        std::process::id(),
        unique_suffix()
    ));
    {
        let mut f = std::fs::File::create(&blob_path)?;
        for (i, frame) in frames.iter().enumerate() {
            if frame.width != w || frame.height != h {
                let _ = std::fs::remove_file(&blob_path);
                return Err(EngineError::Tracking(format!(
                    "frame {i} dims {}x{} != window dims {}x{}",
                    frame.width, frame.height, w, h
                )));
            }
            if frame.rgba.len() != frame_bytes {
                let _ = std::fs::remove_file(&blob_path);
                return Err(EngineError::Tracking(format!(
                    "frame {i} rgba len {} != w*h*4 {}",
                    frame.rgba.len(),
                    frame_bytes
                )));
            }
            f.write_all(&frame.rgba)?;
        }
        f.flush()?;
    }

    let (python, script) = locate_sidecar()?;
    let request = serde_json::json!({
        "width": w,
        "height": h,
        "frame_count": frames.len(),
        "frames_path": blob_path.to_string_lossy(),
        "init_bbox": [init_bbox.0, init_bbox.1, init_bbox.2, init_bbox.3],
        "tracker": tracker,
    });

    // Args via the argument array (never shell-interpolated — T-30-07).
    let mut child = Command::new(&python)
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| EngineError::Tracking(format!("failed to spawn tracking sidecar: {e}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(request.to_string().as_bytes())?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| EngineError::Tracking(format!("sidecar wait failed: {e}")))?;
    let _ = std::fs::remove_file(&blob_path);

    if !output.status.success() {
        // The sidecar prints a JSON {ok:false,error} on hard failure.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Ok(resp) = serde_json::from_str::<SidecarResponse>(&stdout) {
            if let Some(err) = resp.error {
                return Err(EngineError::Tracking(format!("sidecar error: {err}")));
            }
        }
        return Err(EngineError::SidecarFailed {
            tool: "track_cli.py".into(),
            status: output.status.code().unwrap_or(-1),
            stderr: stderr.trim().to_string(),
        });
    }

    let resp: SidecarResponse = serde_json::from_slice(&output.stdout).map_err(|e| {
        EngineError::Tracking(format!(
            "cannot parse sidecar output: {e}; raw: {}",
            String::from_utf8_lossy(&output.stdout)
        ))
    })?;
    if !resp.ok {
        return Err(EngineError::Tracking(
            resp.error.unwrap_or_else(|| "sidecar reported ok=false".into()),
        ));
    }
    Ok(resp.results)
}

fn unique_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Track `initial_bbox_px` across `frames` with `tracker_kind`, producing the
/// real per-frame motion path — the **backend-agnostic engine tracker API**.
///
/// The signature is IDENTICAL to what the native PRIMARY would have exposed, so
/// downstream waves (30-02/30-03) never learn a Python sidecar runs behind it.
///
/// Contract:
///   * `frames` must be non-empty; `initial_bbox_px` must be finite, positive,
///     and fully within `frames[0]`'s pixel bounds — all validated BEFORE any
///     work is handed to the backend (the FFI/subprocess-boundary crash guard,
///     T-30-03: never trust the caller at the boundary).
///   * Emits one [`TrackResult`] per tracked frame, `frame_index` = the
///     analysis-window-relative index (0 = the seed frame).
///   * On loss (`update() == false`) the sidecar STOPS; the final result carries
///     [`TrackConfidence::Lost`] and no boxes are fabricated past the loss.
pub fn track_region(
    frames: &[Frame],
    initial_bbox_px: (f32, f32, f32, f32),
    tracker_kind: TrackerKind,
) -> Result<Vec<TrackResult>, EngineError> {
    if frames.is_empty() {
        return Err(EngineError::Tracking("track_region: no frames".into()));
    }
    let (x, y, w, h) = initial_bbox_px;
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
        return Err(EngineError::Tracking(format!(
            "track_region: non-finite bbox {initial_bbox_px:?}"
        )));
    }
    if w <= 0.0 || h <= 0.0 {
        return Err(EngineError::Tracking(format!(
            "track_region: bbox w/h must be positive (got {w}x{h})"
        )));
    }
    let (fw, fh) = (frames[0].width as f32, frames[0].height as f32);
    if x < 0.0 || y < 0.0 || x + w > fw || y + h > fh {
        return Err(EngineError::Tracking(format!(
            "track_region: bbox {initial_bbox_px:?} out of frame bounds {fw}x{fh}"
        )));
    }

    let raw = run_sidecar(frames, initial_bbox_px, tracker_kind.as_arg())?;
    // Layer the calibrated lost-track heuristic on the raw sidecar bool, and
    // truncate the path at the first hard Lost (SC-4). OpenCV exposes no numeric
    // confidence — see `classify_confidence` (Research Pitfall 3 / Assumption A1).
    let results = classify_confidence(&raw, initial_bbox_px, fw, fh);
    Ok(results)
}

/// Smooth a tracked motion path with a symmetric shrinking-window moving average
/// over the bbox CENTERS — the anti-jitter pass applied in the ENGINE before the
/// `track_object` tool maps centers to keyframes (kills per-frame CSRT center
/// noise most visible on slow-motion content).
///
/// Returns a Vec of the SAME length as `path`, each element keeping its OWN
/// `frame_index`, `w`/`h`, and `confidence` — only the bbox CENTER is replaced by
/// the windowed average.
///
/// The window is symmetric AND shrinking at the edges: for element `k` with
/// radius `r = window / 2`, the average spans indices `[k-rr, k+rr]` where
/// `rr = min(r, k, n-1-k)`. Because the window is symmetric about `k` and
/// shrinks so it never runs off either end, a LINEAR center path is reproduced
/// EXACTLY (endpoints preserved, no edge drift) — the key invariant that leaves
/// a legitimate fast/linear track unchanged by default smoothing.
///
/// `window <= 1` (smoothness 0 or 1) → identity (a clone of the input).
pub fn smooth_track_path(path: &[TrackResult], window: usize) -> Vec<TrackResult> {
    if window <= 1 {
        return path.to_vec();
    }
    let n = path.len();
    let r = window / 2;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let rr = r.min(k).min(n - 1 - k);
        let mut sum_cx = 0.0f64;
        let mut sum_cy = 0.0f64;
        for j in (k - rr)..=(k + rr) {
            let (x, y, w, h) = path[j].bbox_px;
            sum_cx += (x + w / 2.0) as f64;
            sum_cy += (y + h / 2.0) as f64;
        }
        let count = (2 * rr + 1) as f64;
        let avg_cx = (sum_cx / count) as f32;
        let avg_cy = (sum_cy / count) as f32;
        // Keep THIS element's own w/h/frame_index/confidence; move only the center.
        let (_, _, w, h) = path[k].bbox_px;
        out.push(TrackResult {
            frame_index: path[k].frame_index,
            bbox_px: (avg_cx - w / 2.0, avg_cy - h / 2.0, w, h),
            confidence: path[k].confidence,
        });
    }
    out
}

/// **Wave-0 decision-gate smoke path.** Run a real `TrackerCSRT` init/update over
/// `frame_a` (seed) + `frame_b` through [`track_region`], returning
/// `(converged, bbox_px)` for `frame_b`.
///
/// This is the minimal proof the resolved backend (the Python
/// `opencv-contrib-python` sidecar) links up and the classical CSRT tracker runs
/// on real decoded-frame pixels. `converged` is `true` when the tracker produced
/// a non-`Lost` result for the second frame.
pub fn csrt_init_update_smoke(
    frame_a: &Frame,
    frame_b: &Frame,
    init_bbox: (i32, i32, i32, i32),
) -> Result<(bool, (i32, i32, i32, i32)), EngineError> {
    let bbox_f = (
        init_bbox.0 as f32,
        init_bbox.1 as f32,
        init_bbox.2 as f32,
        init_bbox.3 as f32,
    );
    let results = track_region(
        &[frame_a.clone(), frame_b.clone()],
        bbox_f,
        TrackerKind::Csrt,
    )?;
    let second = results
        .iter()
        .find(|r| r.frame_index == 1)
        .ok_or_else(|| EngineError::Tracking("tracker returned no result for frame 1".into()))?;
    let converged = second.confidence != TrackConfidence::Lost;
    let b = second.bbox_px;
    Ok((
        converged,
        (b.0 as i32, b.1 as i32, b.2 as i32, b.3 as i32),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_box(w: u32, h: u32, bx: u32, by: u32, bw: u32, bh: u32) -> Frame {
        let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
        for yy in by..(by + bh).min(h) {
            for xx in bx..(bx + bw).min(w) {
                let o = ((yy as usize) * (w as usize) + (xx as usize)) * 4;
                rgba[o] = 255;
                rgba[o + 1] = 255;
                rgba[o + 2] = 255;
                rgba[o + 3] = 255;
            }
        }
        Frame { width: w, height: h, rgba }
    }

    /// track_region produces one non-empty per-frame path through the sidecar,
    /// with frame 0 = the seed and a following tracked frame.
    #[test]
    fn track_region_two_frames() {
        let f0 = synth_box(320, 240, 100, 100, 40, 40);
        let f1 = synth_box(320, 240, 110, 108, 40, 40);
        let path = track_region(&[f0, f1], (100.0, 100.0, 40.0, 40.0), TrackerKind::Csrt)
            .expect("track_region ran");
        assert!(!path.is_empty(), "path must be non-empty");
        assert_eq!(path[0].frame_index, 0, "frame 0 is the seed");
        assert!(
            path.iter().any(|r| r.frame_index == 1),
            "a tracked result for frame 1 should exist"
        );
        // The seed box sits at (100,100); a converged track should not be Lost.
        assert!(
            path.last().unwrap().confidence != TrackConfidence::Lost,
            "a high-contrast box should not be immediately lost"
        );
    }

    /// Build a synthetic path whose centers follow `center_fn(k)`, with a fixed
    /// box size, `Ok` confidence throughout.
    fn synth_path(n: usize, w: f32, h: f32, center_fn: impl Fn(usize) -> (f32, f32)) -> Vec<TrackResult> {
        (0..n)
            .map(|k| {
                let (cx, cy) = center_fn(k);
                TrackResult {
                    frame_index: k as u32,
                    bbox_px: (cx - w / 2.0, cy - h / 2.0, w, h),
                    confidence: TrackConfidence::Ok,
                }
            })
            .collect()
    }

    fn center_of(r: &TrackResult) -> (f32, f32) {
        let (x, y, w, h) = r.bbox_px;
        (x + w / 2.0, y + h / 2.0)
    }

    /// A symmetric shrinking-window average of a LINEAR center path is EXACTLY
    /// identity — endpoints preserved, no edge drift. Protects legitimate
    /// fast/linear tracks from being altered by default smoothing.
    #[test]
    fn smoothing_of_linear_path_is_identity() {
        let raw = synth_path(20, 30.0, 20.0, |k| (10.0 + 2.0 * k as f32, 5.0 + k as f32));
        let smoothed = smooth_track_path(&raw, 5);
        assert_eq!(smoothed.len(), raw.len());
        for (r, s) in raw.iter().zip(smoothed.iter()) {
            let (rcx, rcy) = center_of(r);
            let (scx, scy) = center_of(s);
            assert!((rcx - scx).abs() < 1e-3, "cx drift at {}: {} vs {}", r.frame_index, rcx, scx);
            assert!((rcy - scy).abs() < 1e-3, "cy drift at {}: {} vs {}", r.frame_index, rcy, scy);
            // w/h/frame_index/confidence preserved.
            assert_eq!(r.frame_index, s.frame_index);
            assert_eq!(r.bbox_px.2, s.bbox_px.2);
            assert_eq!(r.bbox_px.3, s.bbox_px.3);
            assert_eq!(r.confidence, s.confidence);
        }
    }

    /// Smoothing a linear path corrupted by an alternating ±2px zigzag reduces
    /// the summed |second-difference| (jerk) to < 25% of the raw, while the net
    /// travelled distance stays ~the same (non-degenerate — not collapsed).
    #[test]
    fn smoothing_reduces_jitter_on_noisy_path() {
        let n = 40usize;
        let raw = synth_path(n, 30.0, 20.0, |k| {
            let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
            (10.0 + 2.0 * k as f32 + sign * 2.0, 5.0 + k as f32 + sign * 2.0)
        });
        let smoothed = smooth_track_path(&raw, 5);

        // Summed absolute second-difference (jerk) of the center sequence.
        let jerk = |path: &[TrackResult]| -> f64 {
            let c: Vec<(f32, f32)> = path.iter().map(center_of).collect();
            let mut acc = 0.0f64;
            for k in 1..c.len() - 1 {
                let dxx = (c[k + 1].0 - 2.0 * c[k].0 + c[k - 1].0) as f64;
                let dyy = (c[k + 1].1 - 2.0 * c[k].1 + c[k - 1].1) as f64;
                acc += dxx.abs() + dyy.abs();
            }
            acc
        };
        let raw_jerk = jerk(&raw);
        let sm_jerk = jerk(&smoothed);
        assert!(
            sm_jerk < 0.25 * raw_jerk,
            "smoothed jerk {sm_jerk} not < 25% of raw {raw_jerk}"
        );

        // Net travel distance (first -> last center) stays close (non-degenerate).
        let travel = |path: &[TrackResult]| -> f64 {
            let a = center_of(&path[0]);
            let b = center_of(&path[path.len() - 1]);
            (((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)) as f64).sqrt()
        };
        let raw_travel = travel(&raw);
        let sm_travel = travel(&smoothed);
        assert!(
            (sm_travel - raw_travel).abs() < 0.3 * raw_travel,
            "smoothed travel {sm_travel} diverged from raw {raw_travel}"
        );
        assert!(sm_travel > 1.0, "smoothed path degenerated to ~zero travel");
    }

    /// window 0 and 1 both return the input centers unchanged (identity).
    #[test]
    fn smoothing_window_le_one_is_identity() {
        let raw = synth_path(10, 30.0, 20.0, |k| {
            let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
            (10.0 + 2.0 * k as f32 + sign * 3.0, 5.0 + k as f32)
        });
        for window in [0usize, 1usize] {
            let smoothed = smooth_track_path(&raw, window);
            assert_eq!(smoothed.len(), raw.len());
            for (r, s) in raw.iter().zip(smoothed.iter()) {
                assert_eq!(r.bbox_px, s.bbox_px, "window {window} altered center");
            }
        }
    }

    /// The bbox is validated BEFORE any backend work (T-30-03): non-finite,
    /// non-positive, and out-of-bounds bboxes are rejected without spawning the
    /// sidecar.
    #[test]
    fn track_region_rejects_bad_bbox() {
        let f = synth_box(320, 240, 100, 100, 40, 40);
        let frames = [f];
        assert!(track_region(&frames, (f32::NAN, 0.0, 10.0, 10.0), TrackerKind::Csrt).is_err());
        assert!(track_region(&frames, (0.0, 0.0, 0.0, 10.0), TrackerKind::Csrt).is_err());
        assert!(track_region(&frames, (-5.0, 0.0, 10.0, 10.0), TrackerKind::Csrt).is_err());
        assert!(
            track_region(&frames, (300.0, 0.0, 40.0, 10.0), TrackerKind::Csrt).is_err(),
            "x+w beyond frame width must be rejected"
        );
        // A valid in-bounds bbox is accepted (returns a path).
        assert!(track_region(&frames, (100.0, 100.0, 40.0, 40.0), TrackerKind::Csrt).is_ok());
    }
}
