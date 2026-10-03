//! FFmpeg sidecar integration.
//!
//! DECISION (Phase 1): FFmpeg is driven as a SIDECAR subprocess (`ffmpeg` /
//! `ffprobe` binaries via `std::process::Command`), NOT linked in-process via
//! `ffmpeg-next`. Rationale:
//!   * The dev toolchain ships FFmpeg 8.0.1 / libavcodec 62, which the
//!     `ffmpeg-next` / `ffmpeg-sys` bindings do not support.
//!   * Process separation gives crash isolation for free: a malformed file
//!     kills (or errors) the child process, never the host app. Verified by
//!     `test_malformed_isolation`.
//!   * Licensing: invoking the CLI as a separate process is mere aggregation —
//!     no copyleft obligations attach to our Rust code (see PROVENANCE.md).
//!
//! Dev builds use the Homebrew GPL FFmpeg (never shipped). The ship build
//! bundles an LGPL FFmpeg on Windows.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::EngineError;

/// Windows process-creation flag that suppresses the console window a child
/// process would otherwise pop up. Without it, every ffmpeg/ffprobe spawn (many
/// per second during preview playback) flashes a console window on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Windows process-creation flag that starts a child in the BELOW_NORMAL
/// priority class (Win32 `BELOW_NORMAL_PRIORITY_CLASS`).
///
/// Phase 58 D-10: **playback is the privileged consumer of CPU/GPU.** A proxy
/// transcode that costs the user presented frames is a regression, not a
/// feature — so the proxy-encode sidecar runs at below-normal priority and
/// yields to the decode/composite/present work whenever they compete.
///
/// Used ONLY by [`spawn_proxy_encode`]. Export and decode sidecars keep the
/// default (normal) priority: they ARE the foreground work.
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// Build a `Command` for an ffmpeg/ffprobe binary with the Windows console
/// window suppressed. On non-Windows platforms this is a plain `Command::new`.
/// ALL ffmpeg/ffprobe spawns in this module MUST go through this helper.
fn ffmpeg_command(bin: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(bin);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Resolved paths to the ffmpeg + ffprobe binaries.
#[derive(Debug, Clone)]
pub struct FfmpegBinaries {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// What kind of media a file fundamentally is, classified at probe time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

/// Media information extracted from `ffprobe` JSON output (Phase 3).
///
/// Timing is NORMALIZED at probe (MEDIA-03): `duration_us` comes from the
/// CONTAINER duration (not frame_count x nominal fps), `avg_frame_rate` is
/// the authoritative working rate, and `is_vfr` / `rotation_degrees` /
/// color metadata are captured here so downstream preview/export consumers
/// read normalized fields instead of re-deriving them (classic VFR-drift and
/// rotated-video bugs).
#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub media_kind: MediaKind,
    /// Container (unrotated) pixel dimensions. 0 for audio-only files.
    pub width: u32,
    pub height: u32,
    /// ffprobe's "real base" rate guess (fps). 0.0 when absent (audio/image).
    pub r_frame_rate: f64,
    /// Average rate = frames / duration (fps) — the authoritative working
    /// rate for editing math. 0.0 when absent (audio/image).
    pub avg_frame_rate: f64,
    /// Container duration in microseconds. 0 for still images.
    pub duration_us: i64,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub has_audio: bool,
    /// Display rotation normalized to {0, 90, 180, 270}, from the
    /// display-matrix side data (preferred) or the legacy `rotate` tag.
    pub rotation_degrees: u32,
    pub pix_fmt: Option<String>,
    pub color_space: Option<String>,
    /// True when the video stream's r_frame_rate and avg_frame_rate disagree
    /// (guarded against 0/0 and N/A) — i.e. genuinely variable frame rate.
    pub is_vfr: bool,
    /// True when the source is a numbered `image2` sequence (`frame_%0Nd.png`)
    /// rather than a single file. Such a sequence classifies as
    /// [`MediaKind::Video`] (it carries a real container duration/fps) but this
    /// flag lets import/UI treat it distinctly (e.g. decode via
    /// [`decode_frame_rgba_at_seq`] with the project framerate). False for a
    /// true single-file still (`*_pipe`) and for ordinary video/audio.
    pub is_image_sequence: bool,
    /// True when the video stream carries a per-pixel alpha channel — either an
    /// alpha-carrying pixel format (`yuva*`/`rgba`/`bgra`/`argb`/`abgr`) OR the
    /// WebM `alpha_mode=1` side-channel tag (VP9's native decoder reports a
    /// plain yuv420p pix_fmt even with alpha, so the tag is the only signal).
    /// Drives forced VP9 decoder selection (see `needs_libvpx_vp9`).
    pub has_alpha: bool,
    /// CONTAINER-level bit rate in bits/second, straight from ffprobe's
    /// `-show_format` `bit_rate` field (the same JSON [`probe`] already
    /// fetches — this costs no extra spawn). `None` when the container
    /// reports none, which some containers genuinely do.
    ///
    /// Phase 58 D-31: this is the bitrate input to the D-07/D-08 heaviness
    /// predicate (resolution x bitrate-per-pixel x codec). It is deliberately
    /// the CONTAINER rate, NOT a per-stream video rate — the predicate only
    /// needs an order-of-magnitude "is this source expensive" signal, and the
    /// container number is the one ffprobe always reports when it reports one
    /// at all.
    pub bit_rate: Option<u64>,
}

/// A single decoded video frame as tightly-packed RGBA bytes (w * h * 4).
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Locate `ffmpeg` and `ffprobe`, preferring a BUNDLED (license-safe LGPL)
/// build over whatever is on PATH. Errors if neither can be found.
///
/// Resolution order (license hygiene — CLAUDE.md: the shipped product must use
/// an LGPL FFmpeg, never a GPL build):
///   1. `RUDIS_FFMPEG_DIR` — a MANUAL/TEST override naming a directory of
///      bundled binaries. NOTHING in the shipped app sets it (there is no
///      writer anywhere under `shell/Rudis.Shell`); the tests are what use it —
///      see `crates/engine/tests/proxy_encode.rs` and
///      `shell/Rudis.Shell.Tests/RealMediaFixture.cs`. Checked first so a
///      deliberate override always wins.
///   2. The directory of the current executable (and its `binaries/` subdir) —
///      THE SHIPPED PATH. `dotnet publish` stages `runtime/binaries/**` into
///      `$(PublishDir)\binaries\` through the `StageRustFfiPublish` target in
///      `crates/ffi/Rudis.Ffi.targets`, so the packaged `.exe` finds the
///      bundled LGPL `ffmpeg.exe`/`ffprobe.exe` sitting beside itself, with no
///      env var and regardless of what a user has on PATH.
///   3. PATH — DEV FALLBACK ONLY. Never ships.
///
/// DEV-LOOP HAZARD (license hygiene): a plain `dotnet build` — as opposed to
/// `dotnet publish` — stages NO sidecars, so an unconfigured dev run finds
/// nothing in cases 1 and 2 and falls silently through to case 3, where PATH
/// may resolve a GPL ffmpeg. That is acceptable for local dev because it never
/// ships, but it INVALIDATES any license, codec or encoder-availability
/// observation made against it. Point `RUDIS_FFMPEG_DIR` at the repo's
/// `runtime/binaries` before trusting such a measurement.
pub fn locate() -> Result<FfmpegBinaries, EngineError> {
    let ffmpeg = resolve_binary("ffmpeg")?;
    let ffprobe = resolve_binary("ffprobe")?;
    Ok(FfmpegBinaries { ffmpeg, ffprobe })
}

/// Resolve one sidecar binary using the bundled-first order documented on
/// [`locate`]. Returns the first hit; `RUDIS_FFMPEG_DIR` (the manual/test
/// override) and the exe-adjacent dir (the shipped, publish-staged layout)
/// both win over PATH, so a bundled LGPL binary is always preferred whenever
/// one is present.
fn resolve_binary(name: &str) -> Result<PathBuf, EngineError> {
    // 1. Explicit bundled dir (manual/test override; nothing in the app sets it).
    if let Some(dir) = std::env::var_os("RUDIS_FFMPEG_DIR") {
        if let Some(hit) = executable_in(&Path::new(&dir).join(name)) {
            return Ok(hit);
        }
    }
    // 2. Beside the current executable (the shipped layout: `dotnet publish`
    //    stages runtime/binaries/** into $(PublishDir)\binaries\).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            for cand in [exe_dir.join(name), exe_dir.join("binaries").join(name)] {
                if let Some(hit) = executable_in(&cand) {
                    return Ok(hit);
                }
            }
        }
    }
    // 3. PATH (dev fallback).
    find_on_path(name)
}

/// Return `candidate` if it resolves to an executable file (handling the
/// Windows `.exe` extension via [`is_executable`]).
fn executable_in(candidate: &Path) -> Option<PathBuf> {
    if is_executable(candidate) {
        Some(candidate.to_path_buf())
    } else {
        None
    }
}

fn find_on_path(name: &str) -> Result<PathBuf, EngineError> {
    let path_var = std::env::var_os("PATH")
        .ok_or_else(|| EngineError::BinaryNotFound(name.to_string()))?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(EngineError::BinaryNotFound(name.to_string()))
}

#[cfg(unix)]
pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(windows)]
pub(crate) fn is_executable(path: &Path) -> bool {
    // On Windows the PATH entries are matched with an .exe extension.
    let with_exe = path.with_extension("exe");
    with_exe.is_file() || path.is_file()
}

// ---------------------------------------------------------------------------
// ffprobe JSON schema (only the fields we need)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    duration: Option<String>,
    pix_fmt: Option<String>,
    color_space: Option<String>,
    #[serde(default)]
    side_data_list: Vec<ProbeSideData>,
    tags: Option<ProbeStreamTags>,
    disposition: Option<ProbeDisposition>,
}

/// One side-data entry. We only care about the Display Matrix `rotation`
/// (ffprobe emits it as a JSON number, possibly negative, e.g. -90 for
/// typical phone portrait footage).
#[derive(Debug, Deserialize)]
struct ProbeSideData {
    rotation: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ProbeStreamTags {
    /// Legacy rotation tag (pre-display-matrix files), degrees as a string.
    rotate: Option<String>,
    /// WebM alpha side-channel marker (`alpha_mode=1`). VP9's native decoder
    /// reports pix_fmt yuv420p even when the container carries alpha, so this
    /// tag is the ONLY probe-time signal that a VP9/WebM stream has alpha.
    #[serde(rename = "alpha_mode")]
    alpha_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeDisposition {
    /// 1 when the "video" stream is really embedded cover art (e.g. album
    /// art inside an .m4a) — such a stream must not make the file a Video.
    attached_pic: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    format_name: Option<String>,
    /// Container bit rate in bits/second. ffprobe emits this as a JSON
    /// STRING (e.g. `"60123456"`), and omits the key entirely for containers
    /// that do not carry one — hence `Option<String>` plus a tolerant parse
    /// (see [`parse_format_bit_rate`]). Phase 58 D-31.
    bit_rate: Option<String>,
}

/// Parse ffprobe's container `bit_rate` (a JSON string) into bits/second.
///
/// Tolerant by design (Phase 58 D-31): a missing key, an empty string, a
/// non-numeric value (`"N/A"`), or no `format` object at all all yield `None`
/// rather than an error. A heaviness predicate that failed import on an
/// unusual container would be a defect, not a safety feature.
fn parse_format_bit_rate(format: Option<&ProbeFormat>) -> Option<u64> {
    format
        .and_then(|f| f.bit_rate.as_deref())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Probe a media file with `ffprobe` and return normalized [`MediaInfo`].
///
/// Handles video, audio-only, and still-image files. Returns `Err` (never
/// panics) if the file is missing, malformed, or contains no decodable
/// audio/video stream at all.
pub fn probe(path: &Path) -> Result<MediaInfo, EngineError> {
    bump_probe_spawn();
    let bins = locate()?;
    let output = ffmpeg_command(&bins.ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_streams", "-show_format"])
        .arg(path)
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        return Err(EngineError::SidecarFailed {
            tool: "ffprobe".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let parsed: ProbeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| EngineError::ProbeParse(format!("invalid ffprobe JSON: {e}")))?;

    let format_name = parsed
        .format
        .as_ref()
        .and_then(|f| f.format_name.as_deref())
        .unwrap_or("");

    // The real picture stream, if any: embedded cover art (attached_pic)
    // must not classify an audio file as Video.
    let video = parsed.streams.iter().find(|s| {
        s.codec_type.as_deref() == Some("video")
            && s.disposition
                .as_ref()
                .and_then(|d| d.attached_pic)
                .unwrap_or(0)
                == 0
    });
    let audio = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    // A numbered `image2` SEQUENCE is identified by the `%0Nd` printf token in
    // its path — the DEFINITIVE, cross-platform signal that the caller intends
    // a sequence. format_name alone is NOT reliable on Windows: given a Rust
    // `\\?\` verbatim absolute path, ffprobe reports `image2` even for a single
    // still PNG (it would otherwise report `png_pipe`), so the still-vs-sequence
    // distinction must key on the path pattern, not the demuxer name.
    let is_image_sequence = format_name.contains("image2") && has_image2_pattern(path);

    // Per-pixel alpha: an alpha-carrying pixel format, OR the WebM alpha_mode
    // side-channel tag (VP9's native decoder reports plain yuv420p even with
    // alpha, so pix_fmt alone is insufficient for the VP9 case).
    let has_alpha = video
        .map(|v| {
            let fmt_alpha = v
                .pix_fmt
                .as_deref()
                .map(|p| {
                    p.starts_with("yuva")
                        || p.starts_with("rgba")
                        || p.starts_with("bgra")
                        || p.starts_with("argb")
                        || p.starts_with("abgr")
                        || p.starts_with("ya")
                })
                .unwrap_or(false);
            let tag_alpha = v
                .tags
                .as_ref()
                .and_then(|t| t.alpha_mode.as_deref())
                .map(|m| m == "1")
                .unwrap_or(false);
            fmt_alpha || tag_alpha
        })
        .unwrap_or(false);

    let media_kind = match (video, audio) {
        // A numbered `image2` SEQUENCE (frame_%0Nd.png) carries a real
        // container duration/fps just like a video stream — route it through
        // the full Video path so its duration is preserved (Phase 28 Gap 2).
        (Some(_), _) if is_image_sequence => MediaKind::Video,
        // A true SINGLE-FILE still demuxes via a `*_pipe` demuxer (png_pipe,
        // webp_pipe, ...) or, under a Windows verbatim path, via `image2` with
        // NO numbering token: one frame, no real duration.
        (Some(_), _) if format_name.contains("_pipe") || format_name.contains("image2") => {
            MediaKind::Image
        }
        (Some(_), _) => MediaKind::Video,
        (None, Some(_)) => MediaKind::Audio,
        (None, None) => {
            return Err(EngineError::NoMediaStream(path.display().to_string()));
        }
    };

    let (width, height) = match video {
        Some(v) => (v.width.unwrap_or(0), v.height.unwrap_or(0)),
        None => (0, 0),
    };

    // Frame rates from the VIDEO stream only (audio streams report "0/0").
    let r_frame_rate = video
        .and_then(|v| v.r_frame_rate.as_deref())
        .and_then(parse_rational);
    let avg_frame_rate = video
        .and_then(|v| v.avg_frame_rate.as_deref())
        .and_then(parse_rational);

    // VFR detection: both rates must be real (parse_rational already guards
    // "0/0" / "N/A" / div-by-zero) and disagree. Only meaningful for Video.
    let is_vfr = media_kind == MediaKind::Video
        && match (r_frame_rate, avg_frame_rate) {
            (Some(r), Some(avg)) if r > 0.0 && avg > 0.0 => (r - avg).abs() > 1e-3,
            _ => false,
        };

    // NORMALIZED duration (MEDIA-03): CONTAINER duration preferred — never
    // frame_count x nominal fps, which drifts on VFR sources. Still images
    // have no duration; the import policy is 0.
    let duration_us = if media_kind == MediaKind::Image {
        0
    } else {
        let seconds = parsed
            .format
            .as_ref()
            .and_then(|f| f.duration.as_deref())
            .and_then(|d| d.parse::<f64>().ok())
            .or_else(|| {
                video
                    .or(audio)
                    .and_then(|s| s.duration.as_deref())
                    .and_then(|d| d.parse::<f64>().ok())
            })
            .unwrap_or(0.0);
        (seconds * 1_000_000.0).round() as i64
    };

    Ok(MediaInfo {
        media_kind,
        width,
        height,
        r_frame_rate: r_frame_rate.unwrap_or(0.0),
        avg_frame_rate: avg_frame_rate.unwrap_or(0.0),
        duration_us,
        vcodec: video.and_then(|v| v.codec_name.clone()),
        acodec: audio.and_then(|a| a.codec_name.clone()),
        has_audio: audio.is_some(),
        rotation_degrees: video.map(extract_rotation).unwrap_or(0),
        pix_fmt: video.and_then(|v| v.pix_fmt.clone()),
        color_space: video.and_then(|v| v.color_space.clone()),
        is_vfr,
        is_image_sequence,
        has_alpha,
        // D-31: rides the SAME `-show_format` JSON already parsed above — no
        // second ffprobe spawn, so import's probe budget is unchanged.
        bit_rate: parse_format_bit_rate(parsed.format.as_ref()),
    })
}

/// Rotation from the display-matrix side data (preferred; ffprobe emits a
/// signed `rotation` number) or the legacy `rotate` stream tag, normalized
/// into {0, 90, 180, 270}.
fn extract_rotation(stream: &ProbeStream) -> u32 {
    let raw = stream
        .side_data_list
        .iter()
        .find_map(|sd| sd.rotation)
        .or_else(|| {
            stream
                .tags
                .as_ref()
                .and_then(|t| t.rotate.as_deref())
                .and_then(|r| r.trim().parse::<f64>().ok())
        });
    match raw {
        Some(deg) => normalize_rotation(deg),
        None => 0,
    }
}

/// Normalize an arbitrary rotation in degrees (possibly negative or slightly
/// off-axis) to the nearest of {0, 90, 180, 270}.
fn normalize_rotation(deg: f64) -> u32 {
    let wrapped = (deg.round() as i64).rem_euclid(360);
    ((((wrapped + 45) / 90) * 90) % 360) as u32
}

/// Parse an ffprobe rational like `"30/1"` into an f64. Returns None for
/// `"0/0"`, `"N/A"`, or garbage.
fn parse_rational(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

/// Extract a single poster frame from `path` into `out_path` (image format
/// chosen by extension, e.g. `.png`), scaled to 320px wide, via sidecar
/// ffmpeg. ffmpeg auto-applies the display rotation on decode, so posters of
/// rotated footage come out upright. Not meaningful for audio-only files
/// (the caller skips them; the renderer shows a distinct audio tile).
pub fn generate_poster(path: &Path, out_path: &Path, at_seconds: f64) -> Result<(), EngineError> {
    let bins = locate()?;

    let output = ffmpeg_command(&bins.ffmpeg)
        .args(["-v", "error", "-y", "-ss", &format!("{at_seconds}")])
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf", "scale=320:-1"])
        .arg(out_path)
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (poster)".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Position-driven decode (Phase 4: real-frame preview)
// ---------------------------------------------------------------------------

/// Nominal duration of one frame in MICROSECONDS = round(1_000_000 / fps).
/// Returns 0 for non-positive fps (audio/image — no frame stepping).
///
/// NOTE: `rudis_core::model::frame_step_us` is an intentional twin of this
/// function (the pure domain core cannot depend on the engine crate). Keep
/// the two in sync.
pub fn frame_step_us(fps: f64) -> i64 {
    if fps <= 0.0 {
        return 0;
    }
    (1_000_000.0 / fps).round() as i64
}

/// Output dimensions after applying a display rotation: 90/270 swap w/h.
fn rotated_dims(width: u32, height: u32, rotation_degrees: u32) -> (u32, u32) {
    match rotation_degrees % 360 {
        90 | 270 => (height, width),
        _ => (width, height),
    }
}

/// The ffmpeg filter that uprights a frame carrying our NORMALIZED
/// `rotation_degrees` (Phase 3 probe: raw ffprobe display-matrix `rotation`
/// wrapped into {0, 90, 180, 270} — NOT negated).
///
/// Mapping verified empirically against ffmpeg's own autorotate (ffmpeg 8,
/// macOS): for `rotated_90.mp4` (display-matrix rotation = +90), the default
/// autorotated decode is BIT-IDENTICAL to `-noautorotate -vf transpose=cclock`.
/// We decode with `-noautorotate` + this explicit filter so orientation is
/// driven deterministically by the metadata normalized at import, not by
/// whatever the decoder build does by default.
fn upright_filter(rotation_degrees: u32) -> Option<&'static str> {
    match rotation_degrees % 360 {
        90 => Some("transpose=cclock"),
        180 => Some("hflip,vflip"),
        270 => Some("transpose=clock"),
        _ => None,
    }
}

/// True when a path's filename carries an `image2` numbering token (`%d`,
/// `%03d`, `%04d`, ...) — the definitive signal that the caller intends a
/// numbered image SEQUENCE rather than a single still. Robust across
/// platforms: on Windows a `\\?\` verbatim absolute path makes ffprobe report
/// even a single PNG as `image2`, so the demuxer name alone cannot distinguish
/// still from sequence; the `%0Nd` token in the path can.
fn has_image2_pattern(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let bytes = name.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'%' {
            // `%` then zero-or-more digits (the zero-pad width) then `d`.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'd' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// The decoder to FORCE for this source, or `None` for the default path.
/// Phase 28 Pitfall 1: a VP9 stream carrying alpha (WebM `alpha_mode=1`) only
/// surfaces its alpha plane through the `libvpx-vp9` decoder — the default
/// native `vp9` decoder decodes to plain yuv420p, silently dropping alpha.
/// Every other format decodes bit-identically through the default path.
fn needs_libvpx_vp9(info: &MediaInfo) -> Option<&'static str> {
    if info.vcodec.as_deref() == Some("vp9") && info.has_alpha {
        Some("libvpx-vp9")
    } else {
        None
    }
}

/// Format microseconds as an ffmpeg `-ss` seconds argument ("2.500000").
fn us_to_ss_arg(position_us: i64) -> String {
    let pos = position_us.max(0);
    format!("{}.{:06}", pos / 1_000_000, pos % 1_000_000)
}

/// Run one sidecar decode: seek to `position_us` (PTS-accurate: `-ss` before
/// `-i` fast-seeks to the prior keyframe, then decodes and DISCARDS frames
/// until the target timestamp — the first emitted frame is the frame at the
/// playhead), emit `frames` raw RGBA frames on stdout, uprighting via
/// [`upright_filter`]. `scale_to`, if set, appends a `scale=W:H` filter
/// AFTER the upright rotation (Phase 7 export: composite frames are scaled
/// to the chosen output resolution as part of the same decode).
fn run_rgba_decode(
    bins: &FfmpegBinaries,
    path: &Path,
    position_us: i64,
    rotation_degrees: u32,
    frames: usize,
    scale_to: Option<(u32, u32)>,
    input_framerate: Option<f64>,
    force_decoder: Option<&str>,
) -> Result<Vec<u8>, EngineError> {
    let mut cmd = ffmpeg_command(&bins.ffmpeg);
    cmd.args(["-v", "error", "-noautorotate", "-ss", &us_to_ss_arg(position_us)]);
    // Image2-sequence timing (Phase 28): the `image2` demuxer needs the
    // framerate specified at INPUT to establish per-frame timing — without it
    // it defaults to 25fps, so probe (import-time, at the project fps) and
    // decode would disagree. These input options MUST precede `-i`.
    if let Some(fps) = input_framerate {
        cmd.args(["-framerate", &fps.to_string()]).args(["-f", "image2"]);
    }
    // Forced decoder (Phase 28 Pitfall 1): VP9/WebM alpha only survives with
    // `-c:v libvpx-vp9` — the default native vp9 decoder silently drops the
    // alpha plane. MUST precede `-i` (it is an input/decoder option).
    if let Some(dec) = force_decoder {
        cmd.args(["-c:v", dec]);
    }
    cmd.arg("-i")
        .arg(path)
        .args(["-frames:v", &frames.to_string()]);
    let mut filters: Vec<String> = Vec::new();
    if let Some(filter) = upright_filter(rotation_degrees) {
        filters.push(filter.to_string());
    }
    if let Some((w, h)) = scale_to {
        filters.push(format!("scale={w}:{h}"));
    }
    if !filters.is_empty() {
        cmd.args(["-vf", &filters.join(",")]);
    }
    let output = cmd
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (decode)".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

/// Decode the frame AT `position_us` (microseconds) into tightly-packed,
/// UPRIGHT RGBA bytes.
///
///   * **PTS-accurate:** the returned frame is the first frame whose PTS
///     >= `position_us` (verified against independent frame-INDEX decodes in
///     `tests/preview_decode.rs`).
///   * **Orientation:** `rotation_degrees` is the value normalized at import
///     (Phase 3 probe); 90/270 outputs have swapped (upright) dimensions.
///   * **Color depth:** high-bit-depth sources (e.g. 10-bit HEVC) are
///     converted to 8-bit RGBA by swscale inside the sidecar.
///   * **End-of-stream fallback:** `position_us` at/past the last frame's PTS
///     (e.g. the transport clamps position to `duration_us`, where no frame
///     starts) steps back one nominal frame at a time (bounded) and returns
///     the FINAL frame instead of failing.
///
/// Negative positions clamp to 0. Errors (never panics) on malformed input,
/// non-video media, or a sidecar failure — crash isolation stays in the
/// child process.
pub fn decode_frame_rgba_at(
    path: &Path,
    position_us: i64,
    rotation_degrees: u32,
) -> Result<Frame, EngineError> {
    let info = probe(path)?; // malformed input fails here with Err, not a panic
    // Phase 28 Gap 1: accept a single-file still (MediaKind::Image) as well as
    // Video. A still has exactly ONE frame, always at time 0 — the `png_pipe`
    // demuxer reports a nominal 25fps for its 1-frame "video", so ANY nonzero
    // `-ss` seeks past the only frame and returns nothing. So for images we
    // clamp the position to 0 and skip the step-back backoff loop entirely.
    let is_image = info.media_kind == MediaKind::Image;
    if !(info.media_kind == MediaKind::Video || is_image) || info.width == 0 || info.height == 0 {
        return Err(EngineError::NoVideoStream(path.display().to_string()));
    }
    let bins = locate()?;
    let force = needs_libvpx_vp9(&info);

    let (out_w, out_h) = rotated_dims(info.width, info.height, rotation_degrees);
    let expected = out_w as usize * out_h as usize * 4;

    if is_image {
        // Single decode attempt at position 0 — no backoff loop (one frame).
        let stdout = run_rgba_decode(&bins, path, 0, rotation_degrees, 1, None, None, force)?;
        if stdout.len() == expected {
            return Ok(Frame {
                width: out_w,
                height: out_h,
                rgba: stdout,
            });
        }
        return Err(EngineError::BadOutputSize {
            got: stdout.len(),
            expected,
        });
    }

    let step = frame_step_us(if info.avg_frame_rate > 0.0 {
        info.avg_frame_rate
    } else {
        30.0 // defensive default; only reachable for streams without a rate
    });

    let mut pos = position_us.max(0);
    // 6 attempts x 1 frame back covers container-duration overshoot (duration
    // is often a few frames past the last video PTS) without masking real
    // decode failures mid-stream.
    for _ in 0..6 {
        let stdout = run_rgba_decode(&bins, path, pos, rotation_degrees, 1, None, None, force)?;
        if stdout.len() == expected {
            return Ok(Frame {
                width: out_w,
                height: out_h,
                rgba: stdout,
            });
        }
        if stdout.is_empty() && pos > 0 {
            pos = (pos - step).max(0);
            continue;
        }
        return Err(EngineError::BadOutputSize {
            got: stdout.len(),
            expected,
        });
    }
    Err(EngineError::BadOutputSize { got: 0, expected })
}

/// [`decode_frame_rgba_at`] PLUS a scale to an EXPLICIT output resolution
/// (`out_w` x `out_h`) — the Phase 7 EXPORT decode: identical PTS-accurate
/// seek + upright-rotation logic (export == preview at the pixel-source
/// level), with an additional `scale=` filter so every exported frame
/// matches the chosen export resolution regardless of the source's native
/// size. `out_w`/`out_h` are the UPRIGHT (already rotation-swapped, if
/// relevant) target dimensions — the caller decides the export resolution;
/// this function does not re-derive it from the source.
pub fn decode_frame_rgba_at_scaled(
    path: &Path,
    position_us: i64,
    rotation_degrees: u32,
    out_w: u32,
    out_h: u32,
) -> Result<Frame, EngineError> {
    let info = probe(path)?; // malformed input fails here with Err, not a panic
    if info.media_kind != MediaKind::Video || info.width == 0 || info.height == 0 {
        return Err(EngineError::NoVideoStream(path.display().to_string()));
    }
    let bins = locate()?;
    let force = needs_libvpx_vp9(&info);

    let expected = out_w as usize * out_h as usize * 4;

    let step = frame_step_us(if info.avg_frame_rate > 0.0 {
        info.avg_frame_rate
    } else {
        30.0 // defensive default; only reachable for streams without a rate
    });

    let mut pos = position_us.max(0);
    for _ in 0..6 {
        let stdout =
            run_rgba_decode(&bins, path, pos, rotation_degrees, 1, Some((out_w, out_h)), None, force)?;
        if stdout.len() == expected {
            return Ok(Frame {
                width: out_w,
                height: out_h,
                rgba: stdout,
            });
        }
        if stdout.is_empty() && pos > 0 {
            pos = (pos - step).max(0);
            continue;
        }
        return Err(EngineError::BadOutputSize {
            got: stdout.len(),
            expected,
        });
    }
    Err(EngineError::BadOutputSize { got: 0, expected })
}

/// Decode `count` CONSECUTIVE frames starting at `position_us` in ONE decoder
/// session (single sidecar invocation, frames streamed over the pipe).
///
/// This is how sustained playback pulls frames — one seek, then sequential
/// decode — and what the realtime throughput gate measures. The dev preview
/// path (per-position `decode_frame_rgba_at` + asset-protocol PNG) trades
/// per-frame process overhead for simplicity; the production play path feeds
/// frames from a session like this one straight to the native wgpu surface.
/// Native on-screen surface presentation is VERIFIED on Windows (Phase 9 Waves
/// 2-3, live + SC-3 MAD 0.0000); macOS runtime re-validation remains a
/// documented revisit trigger (retired dev machine), not a Windows gap.
///
/// Returns the frames actually decoded (fewer than `count` at end of stream;
/// errors if the pipe yields a non-whole number of frames).
pub fn decode_frames_rgba_seq(
    path: &Path,
    position_us: i64,
    count: usize,
    rotation_degrees: u32,
) -> Result<Vec<Frame>, EngineError> {
    let info = probe(path)?;
    if info.media_kind != MediaKind::Video || info.width == 0 || info.height == 0 {
        return Err(EngineError::NoVideoStream(path.display().to_string()));
    }
    let bins = locate()?;

    let (out_w, out_h) = rotated_dims(info.width, info.height, rotation_degrees);
    let frame_bytes = out_w as usize * out_h as usize * 4;

    let force = needs_libvpx_vp9(&info);
    let stdout = run_rgba_decode(&bins, path, position_us, rotation_degrees, count, None, None, force)?;
    if stdout.is_empty() || stdout.len() % frame_bytes != 0 {
        return Err(EngineError::BadOutputSize {
            got: stdout.len(),
            expected: frame_bytes * count,
        });
    }

    Ok(stdout
        .chunks_exact(frame_bytes)
        .map(|chunk| Frame {
            width: out_w,
            height: out_h,
            rgba: chunk.to_vec(),
        })
        .collect())
}

/// Decode the frame at `position_us` from a numbered `image2` SEQUENCE
/// (`frame_%0Nd.png`) using an EXPLICIT `framerate` (Phase 28 SC-4).
///
/// The `image2` demuxer establishes per-frame timing from the input
/// `-framerate`; passing the SAME framerate here that import used to probe the
/// sequence makes probe and decode agree on which frame lands at a given
/// timeline position (probe==decode timing). Rotation is not applicable to a
/// sequence, so this always decodes upright (rotation 0). Position is clamped
/// to >= 0. Errors (never panics) on a malformed member frame or a sidecar
/// failure — crash isolation stays in the child process.
///
/// This is the function the composite/import path calls for sequence clips
/// (it must pass the sequence's project framerate).
pub fn decode_frame_rgba_at_seq(
    pattern: &Path,
    position_us: i64,
    framerate: f64,
) -> Result<Frame, EngineError> {
    let info = probe(pattern)?; // malformed input fails here with Err, not a panic
    if info.width == 0 || info.height == 0 {
        return Err(EngineError::NoVideoStream(pattern.display().to_string()));
    }
    let bins = locate()?;

    // Rotation is not applicable to an image sequence — decode upright as-is.
    let (out_w, out_h) = (info.width, info.height);
    let expected = out_w as usize * out_h as usize * 4;

    let fps = framerate.max(1.0); // guard against a non-positive framerate arg
    // Map the requested time to a DISCRETE sequence frame index, then seek to a
    // point JUST BEFORE that frame's presentation time — the midpoint between
    // the previous frame and this one, `(index - 0.5) / fps`. `run_rgba_decode`
    // applies `-ss` on the OUTPUT side, which DROPS every decoded frame whose
    // pts is below the seek time; seeking at (or past) the target frame's own
    // boundary therefore drops it and returns an EMPTY buffer at a clip's tail
    // (the export loop's final output tick lands past the last frame's start).
    // Seeking half a frame EARLIER keeps exactly the intended frame as the first
    // surviving one — including the last frame of the sequence (SC-4 boundary).
    let raw = position_us.max(0);
    let frame_index = ((raw as f64) * fps / 1_000_000.0).floor().max(0.0);
    let pos = (((frame_index - 0.5) / fps) * 1_000_000.0).round().max(0.0) as i64;
    // A PNG/JPEG image sequence is never VP9 — no forced decoder needed.
    let stdout = run_rgba_decode(&bins, pattern, pos, 0, 1, None, Some(fps), None)?;
    if stdout.len() == expected {
        return Ok(Frame {
            width: out_w,
            height: out_h,
            rgba: stdout,
        });
    }
    Err(EngineError::BadOutputSize {
        got: stdout.len(),
        expected,
    })
}

/// Bounded lookahead for the streaming decode session: ~6 frames ≈ 200ms at
/// 30fps. Large enough to absorb decode/scheduler jitter, small enough that
/// the reader thread backpressures (blocks on `send`) instead of the ffmpeg
/// child racing arbitrarily far ahead into unbounded memory (research §5).
pub const STREAM_CHANNEL_CAPACITY: usize = 6;

/// Total ffmpeg streaming-session spawns since process start. ALWAYS compiled
/// (not `#[cfg(test)]`, so integration tests can read it) — cost is one atomic
/// increment per session start. `surface_present.rs` resets + asserts this to
/// prove SC-2 (a single long-lived process per session, no per-frame spawn).
pub static STREAM_SPAWN_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// ffprobe invocations — mirrors STREAM_SPAWN_COUNT so the WR-03 backoff
/// test can prove an unresolvable layer does NOT re-probe every tick.
pub static PROBE_SPAWN_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// PER-THREAD spawn accounting (quick task 260730-x2t, the SR-3 gap)
// ---------------------------------------------------------------------------
// [`STREAM_SPAWN_COUNT`]/[`PROBE_SPAWN_COUNT`] are PROCESS-GLOBAL, which makes
// them unusable as a *budget* assertion inside a parallel test harness: any
// sibling test that also drives ffmpeg inflates the delta, so the check fails
// whether or not the thing it guards regressed. That is strictly worse than no
// check — it cannot distinguish a regression from noise, so it gets ignored or
// deleted by the next engineer who hits it.
//
// These thread-local mirrors fix that WITHOUT serializing the suite and
// WITHOUT a new dependency: a caller measures the delta across its own
// operation and is immune to everything running on other threads. That is
// sound precisely because every spawn site below runs on the CALLING thread —
// the reader threads a streaming session starts are spawned AFTER the child,
// by the caller, and never spawn children of their own.
//
// The globals are untouched and still incremented at every site, so
// `surface_present.rs`'s SC-2 reset-and-assert and the WR-03 backoff test keep
// working exactly as they do today.

thread_local! {
    static THREAD_STREAM_SPAWNS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static THREAD_PROBE_SPAWNS: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

/// `(stream_spawns, probe_spawns)` issued BY THE CALLING THREAD since the
/// process started. Monotonic; callers take a delta around the operation they
/// are measuring.
///
/// Unlike [`STREAM_SPAWN_COUNT`]/[`PROBE_SPAWN_COUNT`] this is unaffected by
/// concurrent work on other threads, which is what makes a tight spawn BUDGET
/// assertable from inside a parallel test harness.
pub fn thread_spawn_counts() -> (usize, usize) {
    (
        THREAD_STREAM_SPAWNS.with(|c| c.get()),
        THREAD_PROBE_SPAWNS.with(|c| c.get()),
    )
}

fn bump_stream_spawn() {
    STREAM_SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    THREAD_STREAM_SPAWNS.with(|c| c.set(c.get() + 1));
}

fn bump_probe_spawn() {
    PROBE_SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    THREAD_PROBE_SPAWNS.with(|c| c.set(c.get() + 1));
}

/// Spawn ONE long-lived ffmpeg child that seeks to `position_us` and streams
/// upright raw RGBA frames on its piped stdout until killed or EOF. Same
/// arg-building conventions as [`run_rgba_decode`] but with NO `-frames:v` cap
/// and `Stdio::piped()` instead of `.output()` (research Pitfall A).
fn spawn_streaming_decode(
    bins: &FfmpegBinaries,
    path: &Path,
    position_us: i64,
    rotation_degrees: u32,
    force_decoder: Option<&str>,
) -> Result<std::process::Child, EngineError> {
    let mut cmd = ffmpeg_command(&bins.ffmpeg);
    cmd.args(["-v", "error", "-noautorotate", "-ss", &us_to_ss_arg(position_us)]);
    // Forced decoder (Phase 28 Pitfall 1): keep the streaming (preview) path in
    // lockstep with the per-frame (export) path — VP9 alpha needs libvpx-vp9 in
    // BOTH or preview and export diverge, breaking WYSIWYG. MUST precede `-i`.
    if let Some(dec) = force_decoder {
        cmd.args(["-c:v", dec]);
    }
    cmd.arg("-i").arg(path);
    if let Some(filter) = upright_filter(rotation_degrees) {
        cmd.args(["-vf", filter]);
    }
    cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    bump_stream_spawn();
    Ok(cmd.spawn()?)
}

/// A genuinely persistent streaming decode session: ONE long-lived ffmpeg
/// sidecar whose stdout is consumed incrementally by a reader thread that
/// pushes each complete `Frame` through a bounded channel (backpressure). This
/// is the playback path — replacing the fully-buffered `.output()` decode with
/// an incrementally-consumed stream (research §5 / Pitfall A). `decode_frames_
/// rgba_seq` is unchanged and remains available for batch callers.
pub struct StreamingDecodeSession {
    child: std::process::Child,
    frame_rx: std::sync::mpsc::Receiver<Frame>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
    width: u32,
    height: u32,
}

impl StreamingDecodeSession {
    /// Probe for upright dimensions, spawn the long-lived sidecar seeked to
    /// `position_us`, and start the reader thread. Errors (never panics) on
    /// non-video media or a spawn failure.
    pub fn start(
        path: &Path,
        position_us: i64,
        rotation_degrees: u32,
    ) -> Result<Self, EngineError> {
        let info = probe(path)?;
        if info.media_kind != MediaKind::Video || info.width == 0 || info.height == 0 {
            return Err(EngineError::NoVideoStream(path.display().to_string()));
        }
        let bins = locate()?;
        let force = needs_libvpx_vp9(&info);
        let (out_w, out_h) = rotated_dims(info.width, info.height, rotation_degrees);
        let frame_bytes = out_w as usize * out_h as usize * 4;

        let mut child = spawn_streaming_decode(&bins, path, position_us, rotation_degrees, force)?;
        let mut stdout = child.stdout.take().ok_or_else(|| EngineError::SidecarFailed {
            tool: "ffmpeg (stream)".to_string(),
            status: -1,
            stderr: "child stdout pipe unavailable".to_string(),
        })?;

        let (tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(STREAM_CHANNEL_CAPACITY);
        let reader_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = vec![0u8; frame_bytes];
            loop {
                // A full read == one complete frame. A partial/EOF read at end
                // of stream (or the child being killed) ends the thread cleanly.
                if stdout.read_exact(&mut buf).is_err() {
                    break;
                }
                let frame = Frame {
                    width: out_w,
                    height: out_h,
                    rgba: buf.clone(),
                };
                // Blocks when the channel is full (backpressure); errors when
                // the receiver (session) has been dropped — either way, stop.
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            frame_rx,
            reader_handle: Some(reader_handle),
            width: out_w,
            height: out_h,
        })
    }

    /// Pull the next decoded frame, blocking up to `timeout`. Returns `None` on
    /// timeout OR when the stream has ended (channel closed / process exited).
    pub fn try_next_frame(&self, timeout: std::time::Duration) -> Option<Frame> {
        self.frame_rx.recv_timeout(timeout).ok()
    }

    /// Upright output dimensions of this session's frames.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Like [`Self::start`], but additionally delivers ffmpeg's OWN per-frame
    /// accounting — `(n, pts_us)` — on an unbounded side channel, sourced from
    /// a `showinfo` filter running in the SAME process and filter graph as the
    /// rawvideo stdout pipe (SEEK-02, software half). A `-f rawvideo` pipe
    /// carries no timestamp channel at all, so this is the only same-process
    /// route to real per-frame PTS; the separate-`ffprobe` alternative was
    /// rejected (second subprocess per clip-open + a cross-process
    /// frame-order-correlation risk on exactly the timing SEEK-02 fixes).
    ///
    /// Pairing guarantee (Assumption A1, empirically pinned by
    /// `tests/stream_pts.rs` against the committed VFR fixtures): showinfo
    /// sits LAST in the filter chain and the output runs `-fps_mode
    /// passthrough` (see Deviation 4 below — the default fps_mode duplicates
    /// frames on VFR media, which would break the pairing), so record `n`
    /// describes exactly the N-th frame written to stdout, in display order. `pts_us` is the
    /// frame's `pts_time` in microseconds, relative to the `-ss` seek point
    /// (input-side `-ss` RESETS output timestamps — measured, and asserted by
    /// `ss_offset_semantics_are_measured`); it is `None` when ffmpeg reports
    /// no/unparseable PTS for that frame, so the consumer can fall back to a
    /// synthetic stamp OBSERVABLY instead of receiving a silently wrong value.
    ///
    /// [`Self::start`] and its spawn helper are byte-untouched: this method
    /// replicates `spawn_streaming_decode`'s command shape inline with three
    /// deliberate, commented deviations (log level, showinfo filter, piped
    /// stderr). The returned `Self` is field-for-field what `start` builds —
    /// `try_next_frame`, `dimensions` and `Drop` behave identically.
    pub fn start_with_pts(
        path: &Path,
        position_us: i64,
        rotation_degrees: u32,
    ) -> Result<(Self, std::sync::mpsc::Receiver<(u64, Option<i64>)>), EngineError> {
        let info = probe(path)?;
        if info.media_kind != MediaKind::Video || info.width == 0 || info.height == 0 {
            return Err(EngineError::NoVideoStream(path.display().to_string()));
        }
        let bins = locate()?;
        let force = needs_libvpx_vp9(&info);
        let (out_w, out_h) = rotated_dims(info.width, info.height, rotation_degrees);
        let frame_bytes = out_w as usize * out_h as usize * 4;

        // Command built INLINE, replicating spawn_streaming_decode's arg order
        // so that helper stays byte-untouched (the narrowed freeze guard).
        let mut cmd = ffmpeg_command(&bins.ffmpeg);
        // Deviation 1: `-v info`, not `-v error` — showinfo logs its per-frame
        // lines at AV_LOG_INFO, so `-v error` would suppress the very output
        // this side channel exists to read. `-hide_banner -nostats` remove the
        // banner and the `\r`-progress noise so stderr stays line-parseable.
        cmd.args(["-hide_banner", "-nostats", "-v", "info"]);
        cmd.args(["-noautorotate", "-ss", &us_to_ss_arg(position_us)]);
        // Forced decoder: same Phase 28 Pitfall 1 lockstep as the other decode
        // paths (VP9 alpha needs libvpx-vp9). MUST precede `-i`.
        if let Some(dec) = force {
            cmd.args(["-c:v", dec]);
        }
        cmd.arg("-i").arg(path);
        // Deviation 2: `showinfo` appended LAST in the filter chain, so it
        // reports the frames exactly as written to stdout (post-upright) —
        // this ordering is what makes the A1 pairing guarantee hold.
        let vf = match upright_filter(rotation_degrees) {
            Some(filter) => format!("{filter},showinfo"),
            None => "showinfo".to_string(),
        };
        cmd.arg("-vf").arg(vf);
        // Deviation 4: `-fps_mode passthrough` — MEASURED finding (Phase 49):
        // the default fps_mode CFR-conforms rawvideo output at r_frame_rate,
        // DUPLICATING frames on VFR media (vfr.mp4: 181 piped frames vs 120
        // decoded, dup=61) while showinfo only reports the 120 real frames —
        // which would break the A1 pairing this method exists to guarantee.
        // Passthrough emits exactly the decoded frames (120==120 measured;
        // byte-identical to the default on CFR media), so record N describes
        // stdout frame N by construction. NOTE: on VFR media this stream is
        // therefore REAL frames with REAL timing — not the duplicate-padded
        // 30fps-shaped stream `start` produces; consumers pace by the
        // delivered pts, not by a fixed frame_step grid.
        cmd.args(["-fps_mode", "passthrough"]);
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Deviation 3: stderr PIPED (spawn_streaming_decode nulls it) —
            // the showinfo records arrive on stderr.
            .stderr(Stdio::piped());

        // Bumped exactly as spawn_streaming_decode does (SC-2): the
        // spawn-count instrumentation counts this path identically.
        bump_stream_spawn();
        let mut child = cmd.spawn()?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| EngineError::SidecarFailed {
                tool: "ffmpeg (stream+pts)".to_string(),
                status: -1,
                stderr: "child stdout pipe unavailable".to_string(),
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| EngineError::SidecarFailed {
                tool: "ffmpeg (stream+pts)".to_string(),
                status: -1,
                stderr: "child stderr pipe unavailable".to_string(),
            })?;

        // Frame path: identical shape to `start`'s reader — bounded channel,
        // read_exact loop, backpressure via the sync_channel capacity.
        let (tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(STREAM_CHANNEL_CAPACITY);
        let reader_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = vec![0u8; frame_bytes];
            loop {
                if stdout.read_exact(&mut buf).is_err() {
                    break;
                }
                let frame = Frame {
                    width: out_w,
                    height: out_h,
                    rgba: buf.clone(),
                };
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });

        // PTS side channel: UNBOUNDED so this reader can never deadlock
        // against the bounded frame channel. Unbounded is safe here because
        // production is bounded by the frame channel's backpressure on the
        // SAME child process — ffmpeg cannot run ahead of the bounded stdout
        // pipe by more than the OS pipe buffer, and each record is 16 bytes.
        let (pts_tx, pts_rx) = std::sync::mpsc::channel::<(u64, Option<i64>)>();
        // DELIBERATELY detached (no JoinHandle stored — the struct keeps
        // `start`'s exact fields): the EXISTING `Drop` kills the child, which
        // closes its stderr write end, so this thread's read hits EOF and it
        // exits on its own. No `Drop` change is needed or wanted.
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let mut reader = BufReader::new(stderr);
            let mut raw: Vec<u8> = Vec::with_capacity(512);
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => break, // EOF (child exited/killed) or broken pipe
                    Ok(_) => {}
                }
                // Lossy conversion: hostile media can put arbitrary bytes in
                // log lines; a non-UTF8 line must degrade to "skipped", never
                // kill the whole side channel (V5).
                let line = String::from_utf8_lossy(&raw);
                if let Some(record) = Self::parse_showinfo_line(&line) {
                    // On a dropped receiver, keep READING (discarding) rather
                    // than breaking: abandoning stderr would let the pipe
                    // buffer fill and stall ffmpeg's frame output for a
                    // consumer that only dropped the pts side of the pair.
                    let _ = pts_tx.send(record);
                }
            }
        });

        Ok((
            Self {
                child,
                frame_rx,
                reader_handle: Some(reader_handle),
                width: out_w,
                height: out_h,
            },
            pts_rx,
        ))
    }

    /// Parse one ffmpeg stderr line into a `(n, pts_us)` record, or `None`
    /// for any line that is not a per-frame showinfo report. Narrow and
    /// explicit (threat T-49-03-01): only lines carrying both
    /// `Parsed_showinfo` and an ` n:` field are considered; the `n:` integer
    /// must parse or the line is skipped entirely; a missing / `NOPTS` /
    /// unparseable `pts_time:` yields `(n, None)`. Never panics on hostile
    /// input — every failure path is a skip or a `None`.
    ///
    /// Line shape on the vendored build (verified against the fixtures):
    /// `[Parsed_showinfo_0 @ 0x...] n:   0 pts:      0 pts_time:0 duration:...`
    fn parse_showinfo_line(line: &str) -> Option<(u64, Option<i64>)> {
        if !line.contains("Parsed_showinfo") {
            return None;
        }
        let n = Self::field_after(line, " n:")?.parse::<u64>().ok()?;
        let pts_us = Self::field_after(line, " pts_time:")
            .and_then(|token| token.parse::<f64>().ok())
            .filter(|seconds| seconds.is_finite())
            .map(|seconds| (seconds * 1_000_000.0).round() as i64);
        Some((n, pts_us))
    }

    /// The whitespace-delimited token following `key` in `line` (showinfo
    /// right-pads some fields, so leading whitespace after the key is
    /// trimmed). `None` when the key is absent or trails nothing.
    fn field_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let rest = &line[line.find(key)? + key.len()..];
        let token = rest.trim_start();
        let end = token.find(char::is_whitespace).unwrap_or(token.len());
        if end == 0 {
            None
        } else {
            Some(&token[..end])
        }
    }
}

impl Drop for StreamingDecodeSession {
    fn drop(&mut self) {
        // Pitfall F: ALWAYS pair kill() with wait() so the child is reaped and
        // never left a zombie/orphan.
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The reader thread may be PARKED in a full channel's `send()` (consumer
        // stopped pulling) — killing the child does NOT unblock that (it's in
        // send, not read_exact). Drain the channel so the send completes; the
        // reader then loops back to a now-broken read_exact and exits. Only then
        // is join() guaranteed not to deadlock. (`frame_rx` is dropped after this
        // body, but draining here is what lets us join deterministically.)
        while self.frame_rx.try_recv().is_ok() {}
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Streaming decoder for ONE contiguous EXPORT run: accurate-seek to
/// `source_start_us` in `path`, upright-rotate, `scale` to `out_w`x`out_h`, and
/// resample to `out_fps`, streaming exactly `dur_us` of timeline as RGBA frames
/// from ONE long-lived ffmpeg. This is the export twin of
/// [`StreamingDecodeSession`] and REPLACES the per-output-frame
/// [`decode_frame_rgba_at_scaled`] spawn-per-frame path (Phase-7 export was
/// thousands of ffmpeg spawns; this is one per contiguous clip run). Frame
/// selection matches the old path: `-ss` before `-i` is accurate (decode +
/// discard to the exact frame), and the `fps` filter emits output frames at the
/// SAME cadence the per-frame loop seeked to.
pub struct ExportRunDecoder {
    child: std::process::Child,
    frame_rx: std::sync::mpsc::Receiver<Frame>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
}

impl ExportRunDecoder {
    /// Spawn the run's ffmpeg and start the reader thread. `out_w`/`out_h` are the
    /// UPRIGHT target dims (rotation already applied by the filter). Errors (never
    /// panics) on locate/spawn failure.
    ///
    /// SELF-PROBING entry point, behaviour unchanged: this is a delegation to
    /// [`ExportRunDecoder::start_with_probed`] with `probed = None`, which runs the
    /// same `probe(path)` this function has always run. One body, so the two
    /// cannot drift. Callers that already hold a `probe()` result for `path` call
    /// the sibling directly and skip that child.
    pub fn start(
        path: &Path,
        source_start_us: i64,
        dur_us: i64,
        rotation_degrees: u32,
        out_w: u32,
        out_h: u32,
        out_fps: f64,
    ) -> Result<Self, EngineError> {
        Self::start_with_probed(
            path,
            source_start_us,
            dur_us,
            rotation_degrees,
            out_w,
            out_h,
            out_fps,
            None,
        )
    }

    /// [`ExportRunDecoder::start`] with the internal `ffprobe` made SKIPPABLE by a
    /// caller that already holds the answer.
    ///
    /// # What `probed` is for (and what it is NOT)
    ///
    /// It is NOT consulted for dimensions — `out_w`/`out_h` are already parameters
    /// and always win. The probe inside `start` exists for exactly TWO facts, both
    /// documented at the acquisition site below:
    ///
    /// 1. `needs_libvpx_vp9` — the VP9-alpha forced decoder (Phase 28 Pitfall 1).
    /// 2. `media_kind == MediaKind::Image` — the still's `-loop 1` (Phase 32).
    ///
    /// # Caller contract
    ///
    /// `probed` MUST be a `probe(path)` result for this SAME `path` — the caller's
    /// own cached copy of it. It is consumed for the two facts above only, and a
    /// wrong or stale `MediaInfo` therefore fails SILENTLY and visually: another
    /// file's info can drop a transparent overlay's alpha plane (the run exports
    /// FULLY OPAQUE — preview != export) or turn a still's whole run into one
    /// frame. Nothing validates the pairing at runtime; a caller that cannot
    /// guarantee it passes `None`, and the function then probes exactly as `start`
    /// always has.
    ///
    /// # Why it exists
    ///
    /// That probe is ONE `ffprobe` child per call — **~88 ms on this machine**
    /// (plan 59-15's measurement of the frame-source re-root arm; plan 59-18
    /// measured ~258 ms across a cold three-source start shape). It is the
    /// component plan 59-22's residual decomposition named. A caller holding a
    /// per-path probe cache pays it once per path instead of once per spawn.
    /// **This entry point alone claims no latency win** — the saving is the
    /// caller's, and only when the caller actually passes `Some`.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_probed(
        path: &Path,
        source_start_us: i64,
        dur_us: i64,
        rotation_degrees: u32,
        out_w: u32,
        out_h: u32,
        out_fps: f64,
        probed: Option<&MediaInfo>,
    ) -> Result<Self, EngineError> {
        let bins = locate()?;
        let frame_bytes = out_w as usize * out_h as usize * 4;

        // Phase 28 Pitfall 1: a VP9/WebM overlay carrying alpha only surfaces its
        // alpha plane through the `libvpx-vp9` decoder — the default native vp9
        // decoder silently drops it (the overlay would export FULLY OPAQUE). The
        // per-frame (`decode_frame_rgba_at`) and preview-streaming paths already
        // force it; the EXPORT run must too, or a transparent VP9 overlay
        // exports opaque and preview != export (SC-1/SC-2 break). Probe once at
        // spawn (mirrors `StreamingDecodeSession::start`); every non-VP9-alpha
        // source returns `None` and decodes bit-identically through the default.
        //
        // Phase 32 (still-image export fix): the SAME probe also tells us the
        // source is a single-file STILL. A still is ONE frame; without `-loop 1`
        // ffmpeg reads that frame then hits EOF, so the `fps` resample + `-t`
        // bound emit exactly ONE frame no matter the run duration. A 5s still
        // then exports as a ~1/30s video with no frame past the first tick
        // (decode_frame_rgba_at at 1s → BadOutputSize { got: 0 }). `-loop 1`
        // makes the demuxer re-emit the frame indefinitely; `-t dur_s` bounds it
        // to the run length and `fps` stamps the output cadence → exactly the
        // right frame count. (Video sources never loop — they carry their own
        // frames and EOF at the real stream end.)
        //
        // THE ONE CACHE-AWARE LINE (plan 59-23): when the caller already holds
        // this file's `MediaInfo` it hands it in and the `ffprobe` child above is
        // not spawned at all. `None` — every caller of `start` — probes exactly as
        // before. The owned slot exists so both arms yield an `Option<&MediaInfo>`
        // living long enough for the two reads below; nothing downstream of them
        // can tell which arm ran.
        let probed_owned: Option<MediaInfo>;
        let probed: Option<&MediaInfo> = match probed {
            Some(info) => Some(info),
            None => {
                probed_owned = probe(path).ok();
                probed_owned.as_ref()
            }
        };
        let force = probed.and_then(needs_libvpx_vp9);
        let is_image = probed.map(|i| i.media_kind == MediaKind::Image).unwrap_or(false);

        // Same filter chain as `run_rgba_decode` (upright THEN scale), plus an
        // `fps` resample so the run emits frames at the OUTPUT rate.
        let mut filters: Vec<String> = Vec::new();
        if let Some(f) = upright_filter(rotation_degrees) {
            filters.push(f.to_string());
        }
        filters.push(format!("scale={out_w}:{out_h}"));
        filters.push(format!("fps={out_fps}"));
        let dur_s = (dur_us.max(0) as f64) / 1_000_000.0;

        let mut cmd = ffmpeg_command(&bins.ffmpeg);
        cmd.args(["-v", "error", "-noautorotate"]);
        // A still loops so the single frame fills the whole run; `-loop 1` is an
        // input (demuxer) option, so it MUST precede `-i`.
        if is_image {
            cmd.args(["-loop", "1"]);
        }
        // A still has exactly one frame at time 0 — its `-ss` seek is always 0
        // (a nonzero seek into a looping 1-frame image is meaningless). A video
        // seeks to the run's real source start.
        let ss = if is_image { 0 } else { source_start_us };
        cmd.args(["-ss", &us_to_ss_arg(ss)]);
        // Forced decoder (Pitfall 1) MUST precede `-i` (a decoder/input option).
        if let Some(dec) = force {
            cmd.args(["-c:v", dec]);
        }
        cmd.arg("-i")
            .arg(path)
            .args(["-t", &format!("{dur_s}")])
            .args(["-vf", &filters.join(",")])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        bump_stream_spawn();
        let mut child = cmd.spawn()?;
        let mut stdout = child.stdout.take().ok_or_else(|| EngineError::SidecarFailed {
            tool: "ffmpeg (export run)".to_string(),
            status: -1,
            stderr: "child stdout pipe unavailable".to_string(),
        })?;

        let (tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(STREAM_CHANNEL_CAPACITY);
        let reader_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = vec![0u8; frame_bytes];
            loop {
                if stdout.read_exact(&mut buf).is_err() {
                    break; // EOF / kill / partial trailing bytes → done
                }
                let frame = Frame {
                    width: out_w,
                    height: out_h,
                    rgba: buf.clone(),
                };
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            frame_rx,
            reader_handle: Some(reader_handle),
        })
    }

    /// Pull the next frame, blocking until one is ready. Returns `None` when the
    /// run's stream has ended (ffmpeg closed stdout → reader thread exited →
    /// channel closed) — the caller's `while let Some(f)` loop then advances to
    /// the next run.
    pub fn next_frame(&self) -> Option<Frame> {
        self.frame_rx.recv().ok()
    }

    /// Pull the next frame, blocking up to `timeout`. Mirrors
    /// [`StreamingDecodeSession::try_next_frame`] but reports WHY no frame came:
    /// `RecvTimeoutError::Timeout` -> `Timedout`, `::Disconnected` -> `Ended`.
    /// A live-paced pool needs this distinction that `next_frame()`'s `Option`
    /// cannot express — a transient stall HOLDs the last frame, a genuine stream
    /// end drops the session + falls back to a precise single-frame decode.
    pub fn try_next_frame(&self, timeout: std::time::Duration) -> FramePull {
        use std::sync::mpsc::RecvTimeoutError;
        match self.frame_rx.recv_timeout(timeout) {
            Ok(f) => FramePull::Ready(f),
            Err(RecvTimeoutError::Timeout) => FramePull::Timedout,
            Err(RecvTimeoutError::Disconnected) => FramePull::Ended,
        }
    }
}

/// Outcome of a bounded pull from an [`ExportRunDecoder`] in a live-paced loop.
/// Distinguishes a transient stall (HOLD the last frame, keep the session) from
/// a genuine stream end (drop the session + precise-decode fallback).
/// `next_frame()`'s `Option` cannot express this distinction — a live pool needs
/// it.
pub enum FramePull {
    Ready(Frame),
    Timedout,
    Ended,
}

impl Drop for ExportRunDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        while self.frame_rx.try_recv().is_ok() {}
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Encode a decoded [`Frame`] as a PNG on disk (used by the app layer to
/// serve preview frames over the ASSET PROTOCOL — the decoded pixels that
/// tests verify are byte-for-byte the pixels that reach the renderer; no raw
/// frames ever cross the IPC command channel).
pub fn write_frame_png(frame: &Frame, out_path: &Path) -> Result<(), EngineError> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or(EngineError::BadOutputSize {
            got: frame.rgba.len(),
            expected: frame.width as usize * frame.height as usize * 4,
        })?;
    img.save(out_path)
        .map_err(|e| EngineError::PngEncode(format!("{}: {e}", out_path.display())))
}

/// Decode a single frame at `at_seconds` into tightly-packed RGBA bytes.
///
/// Phase 1 API, kept for existing callers/tests; now a thin wrapper over
/// [`decode_frame_rgba_at`] with rotation 0 (i.e. the frame is returned in
/// container orientation — rotation-aware callers use `decode_frame_rgba_at`
/// with the rotation normalized at import).
pub fn decode_frame_rgba(path: &Path, at_seconds: f64) -> Result<Frame, EngineError> {
    decode_frame_rgba_at(path, (at_seconds * 1_000_000.0).round() as i64, 0)
}

// ---------------------------------------------------------------------------
// Audio render (Phase 6: measurable PCM for volume/detach verification and
// the seed of Phase 7 export audio)
// ---------------------------------------------------------------------------

/// Canonical sample rate for rendered clip audio (48 kHz mono f32).
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// Render the audio of `path` over `[in_us, out_us)` as MONO f32 samples at
/// 48 kHz with a gain of `volume`, via sidecar ffmpeg:
/// `-ss <in> -i <path> -t <dur> -vn -af volume=<v> -f f32le -ac 1 -ar 48000 -`.
///
/// A source with NO audio stream (still image, video without audio) returns
/// an EMPTY vec — silence is data, not an error. Missing/malformed files and
/// sidecar failures return `Err`. `volume` is clamped >= 0.
pub fn render_audio_pcm(
    path: &Path,
    in_us: i64,
    out_us: i64,
    volume: f32,
) -> Result<Vec<f32>, EngineError> {
    render_audio_pcm_retimed(path, in_us, out_us, volume, 1.0)
}

/// Legal playback-tempo bounds for the audio path (quick task 260730-x2t,
/// WR-04/WR-05). MIRRORS `rudis_core::MIN_SPEED` / `MAX_SPEED` — the engine
/// deliberately does NOT depend on the domain crate, so the two are kept in
/// lock-step by hand; `engine/tests/audio_retime.rs` asserts they agree.
///
/// These are not stylistic: they are what BOUNDS the `atempo` stage count (see
/// [`atempo_chain`]) and the `-t` computation in [`render_audio_pcm_retimed`].
pub const MIN_TEMPO: f32 = 0.1;
/// See [`MIN_TEMPO`].
pub const MAX_TEMPO: f32 = 10.0;

/// The most `atempo` stages [`atempo_chain`] can ever emit, over the whole
/// `[MIN_TEMPO, MAX_TEMPO]` domain: `ceil(ln(10)/ln(2)) = 4` at the top and
/// `ceil(ln(0.1)/ln(0.5)) = 4` at the bottom. Asserted across the full sweep in
/// `engine/tests/audio_retime.rs` so an out-of-range input can never grow the
/// `-af` argument without a test failing.
pub const MAX_ATEMPO_STAGES: usize = 4;

/// `atempo` stages whose PRODUCT is `tempo`, each inside the filter's REAL
/// `[0.5, 100.0]` range (`libavfilter/af_atempo.c` — the widely-repeated "2.0
/// ceiling" is stale pre-4.2 documentation). Chaining is MANDATORY below 0.5x
/// (a single stage would be rejected) and ADVISABLE above 2x (above 2 the
/// filter starts skipping samples rather than blending them):
///
/// * `tempo > 2.0` -> `n = ceil(log(f) / log(2))` stages;
/// * `tempo < 0.5` -> `n = ceil(log(f) / log(0.5))` stages;
/// * otherwise a single stage.
///
/// Returns EMPTY for `tempo == 1.0`, so the un-retimed filter chain is
/// byte-identical to the pre-retime one.
///
/// The LAST stage is the exact f64 RESIDUAL rather than another copy of
/// `f^(1/n)`: n f32 roots multiplied together drift by ~n f32 ulps, which at
/// tempo 10 is bigger than the 1e-6 product tolerance this is asserted against.
pub fn atempo_chain(tempo: f32) -> Vec<f32> {
    if !tempo.is_finite() || tempo <= 0.0 || tempo == 1.0 {
        return Vec::new();
    }
    // BOUND the input at the function boundary (quick task 260730-x2t, WR-05).
    // The stage count is LOGARITHMIC in the tempo and had no floor below 0.5:
    // `1e-30` is 100 stages and `f32::MIN_POSITIVE` is 127, each appending
    // `,atempo=<value>` to a single `-af` argument — an unbounded filter graph
    // driven by one unvalidated float. `[MIN_TEMPO, MAX_TEMPO]` is the
    // domain-model bound the validators enforce; nothing outside it is a legal
    // retime, and inside it the chain is provably <= MAX_ATEMPO_STAGES.
    //
    // `render_audio_pcm_retimed` REJECTS an out-of-range tempo outright, so
    // this clamp is only ever reached by a direct caller of this `pub` fn (e.g.
    // an arbitrary `MixSource.tempo`), where a bounded chain is strictly better
    // than a 127-deep one.
    let f = (tempo as f64).clamp(MIN_TEMPO as f64, MAX_TEMPO as f64);
    let n = if f > 2.0 {
        (f.ln() / 2.0f64.ln()).ceil().max(1.0) as usize
    } else if f < 0.5 {
        (f.ln() / 0.5f64.ln()).ceil().max(1.0) as usize
    } else {
        1
    };
    if n <= 1 {
        // `f`, not `tempo`: the clamp above must not be undone here.
        return vec![f as f32];
    }
    let stage = f.powf(1.0 / n as f64);
    let mut out: Vec<f32> = Vec::with_capacity(n);
    let mut acc = 1.0f64;
    for _ in 0..n - 1 {
        let s = stage as f32;
        acc *= s as f64;
        out.push(s);
    }
    out.push((f / acc) as f32);
    out
}

/// The `-af` chain [`render_audio_pcm_retimed`] emits: `volume={v}` plus, when
/// `tempo != 1.0`, one `atempo=` stage per [`atempo_chain`] entry.
///
/// **LICENCE (CLAUDE.md rule 6, RT-06).** `volume` and `atempo` are both
/// LGPL-2.1+ native FFmpeg filters that appear NOWHERE in FFmpeg's `configure`
/// — no `_deps`, no `_select`, no GPL gate — so they are present in ANY
/// `--disable-gpl --disable-nonfree` build. `rubberband` is GPL
/// (`rubberband_filter_deps="librubberband"`, and `librubberband` is in
/// `EXTERNAL_LIBRARY_GPL_LIST`) and MUST NEVER appear here. Note that
/// [`resolve_binary`] falls through to PATH as a dev fallback, and a developer's
/// PATH ffmpeg may well be a GPL build with `--enable-librubberband` — so "it
/// worked locally" proves NOTHING. `emits_only_lgpl_filters` asserts the
/// ARGUMENT STRING across a full tempo sweep, and `resolved_binary_has_atempo`
/// probes the RESOLVED binary.
/// Margin, in OUTPUT µs, that a WINDOWED audio render adds beyond its window so
/// the returned PCM is never SHORTER than the window's expected sample count
/// and the caller can TRUNCATE rather than pad (quick task 260730-x2t, RT-05).
///
/// Two independent, MEASURED shortfalls make this necessary — both are fixed
/// startup costs, NOT proportional errors, which is why the margin is a
/// constant in the OUTPUT domain rather than a percentage:
///
/// * **codec frame granularity** — a compressed stream decodes in whole frames
///   (1024 samples ≈ 21.3 ms for AAC), so a 100 ms window returns 4064 of the
///   4800 samples asked for. Measured on `speech_en.mp4`: stitching a flat ramp
///   without a lead-out left a **736-sample silent hole at every boundary**.
///   RE-CONFIRMED on the shipped `N-125907` sidecar 2026-08-08: `-ss 1.0
///   -t 0.1` returns exactly 4064 of 4800. This is the bullet the margin rests
///   on. It is position-dependent, not constant — an input seek rounds the start
///   FORWARD to the next AAC frame boundary, so a window starting on a boundary
///   (`-ss 0.5`, `-ss 2.0`) loses nothing while `-ss 1.0` loses 736.
/// * ~~**`atempo`'s own filter latency**~~ — **MIS-ATTRIBUTED, corrected
///   2026-08-08** (debug session `waveform-aac-priming-trim-short`). The two
///   numbers this bullet used to quote (`-t 0.1` → 3056 of 4800, `-t 1.0` →
///   46064 of 48000, read as "a near-constant 36-40 ms head loss at every window
///   length") were both taken at window start **ZERO**, where the engine used to
///   emit a `-ss 0.000000` that discarded a whole 1024-sample AAC frame off the
///   head. `atempo` has no fixed head loss of its own: at tempo 2.0 the shipped
///   build returns EXACTLY 48000 of 48000 from `-ss 0.5` and from `-ss 2.0`, and
///   now also from `-ss 0` since [`input_seek_args`] stopped emitting the
///   zero-position seek.
///
/// **The constant does NOT move.** Its justification is now carried entirely by
/// the first bullet, which is real, still present on the shipped build, and worst
/// measured at 736 samples / 15.3 ms — so 150 ms remains ~10x the worst measured
/// loss rather than the ~4x this comment used to claim against the inflated
/// figure. It is discarded by the caller's truncation, so the only cost is a
/// little extra decode per window. The gate is
/// `engine/tests/audio_retime.rs::retimed_render_is_longer_than_the_expected_
/// window_and_must_be_truncated`, which sweeps window starts and asserts the
/// margin covers whatever shortfall each one has.
pub const RETIME_AUDIO_LEAD_OUT_US: i64 = 150_000;

/// SOURCE µs a windowed render must add beyond its window to gain
/// [`RETIME_AUDIO_LEAD_OUT_US`] of OUTPUT at `tempo`.
///
/// The margin has to be constant in the OUTPUT domain (the losses above are
/// fixed startup costs), and `render_audio_pcm_retimed`'s `-t` divides the
/// requested source span by `tempo` — so the SOURCE lead-out must be scaled UP
/// by `tempo`. ONE helper so the three windowed call sites (export mix, live
/// preview mix, the seam test) cannot each get the scaling wrong.
pub fn audio_lead_out_us(tempo: f32) -> i64 {
    let t = if tempo.is_finite() && tempo > 0.0 {
        tempo as f64
    } else {
        1.0
    };
    (RETIME_AUDIO_LEAD_OUT_US as f64 * t).ceil() as i64
}

pub fn audio_filter_chain(volume: f32, tempo: f32) -> String {
    let mut chain = format!("volume={}", volume.max(0.0));
    for stage in atempo_chain(tempo) {
        chain.push_str(&format!(",atempo={stage}"));
    }
    chain
}

/// The `-ss <pos>` INPUT-seek terms for a decode that begins at `position_us` —
/// and **NOTHING AT ALL when the position is 0**.
///
/// # Why the zero case is a special case (debug session
/// `waveform-aac-priming-trim-short`, 2026-08-08)
///
/// `-ss 0` reads as a no-op and is not one. On the SHIPPED sidecar
/// (`runtime/binaries`, `N-125907`, libavcodec 63) an input seek on AAC-in-MP4
/// quantises the start FORWARD to an AAC frame boundary, and at position 0 that
/// costs a WHOLE 1024-sample frame rather than nothing:
///
/// | speech_en.mp4 (3.720000 s container) | samples at 48 kHz mono |
/// |---|---|
/// | no `-ss` | **178560** = 3.720000 s exactly |
/// | `-ss 0.000000` | 176512 — **2048 samples / 42.67 ms short** |
///
/// It is a HEAD loss, not a short tail: the `-ss 0` output is bit-identical to
/// the no-`-ss` output with its first 2048 samples removed (max abs diff
/// 9.85e-10), so everything after t=0 arrives 42.67 ms EARLY. Measured on
/// `speech_delayed_700ms.mp4`, whose speech onset moves from 890 ms to 850 ms.
///
/// Four things were checked before choosing this fix rather than a tolerance:
///
/// * **It is not `-t`.** The 2x2 over {`-ss 0`, no `-ss`} x {`-t dur`, no `-t`}
///   moves only with `-ss`. `-t` never removes a sample.
/// * **It is not "the newer decoder is lossy".** The shipped build and the 2023
///   PATH build agree BIT-EXACTLY (max abs diff 0.0) on a full no-`-ss` decode,
///   and the shipped one is the more accurate of the two — the older build emits
///   384-1664 samples of trailing encoder padding it should have trimmed.
/// * **It is AAC-specific.** Five controls transcoded from the same source
///   (pcm_s16le, FLAC, MP3, Opus, and an AAC `-c:a copy` remux) lose samples
///   under `-ss 0` in the AAC case ONLY. MP3 and Opus both carry encoder delay
///   and are immune.
/// * **It is FILE-specific within AAC**, and not predictably so: 5 of the repo's
///   7 audio fixtures lose a frame, 2 lose nothing, and all 7 declare the same
///   `elst media_time = 1024` priming skip. So the size of the loss cannot be
///   computed and compensated for — the seek has to be avoided.
///
/// With the seek omitted at position 0, all THREE ffmpeg builds in this tree
/// (`runtime/binaries` N-125907, `crates/engine/ffmpeg-dev/bin` n8.0.1, and a
/// 2023 PATH 6.1) return exactly `round(container_duration * 48000)` samples for
/// all 7 fixtures — 21 of 21 cells, one 16-sample resampler-rounding exception.
///
/// NONZERO positions keep the seek and are byte-unchanged. They carry their own
/// forward quantisation of up to one AAC frame on the shipped build (measured
/// +12.00 / +24.00 / +5.33 ms at `-ss` 0.5 / 1.0 / 2.0), which the 2023 build
/// does not have. That is a SEPARATE, larger finding — it needs a decode-and-
/// discard or seek-behind-and-trim strategy with its own cost measurement — and
/// it is deliberately NOT addressed here. See the debug session's open items.
fn input_seek_args(position_us: i64) -> Vec<std::ffi::OsString> {
    if position_us <= 0 {
        Vec::new()
    } else {
        vec![
            std::ffi::OsString::from("-ss"),
            std::ffi::OsString::from(us_to_ss_arg(position_us)),
        ]
    }
}

/// The EXACT argument vector [`render_audio_pcm_retimed`] spawns, `path`
/// included as its own element (never interpolated into a string — threat
/// T-22-04, the same rule the rest of this module follows).
///
/// Extracted from the spawn site so the command line can be PINNED without a
/// subprocess, exactly as [`render_cache_encode_args`] is: the `-ss` rule this
/// builder implements is invisible to any real-output test run on
/// `crates/engine/ffmpeg-dev/bin` — the CI runner's only sidecar, which does not
/// have the defect — so an argv pin is the only gate that can catch a
/// reintroduction everywhere.
///
/// `out_dur_us` is the OUTPUT duration (already divided by `tempo` by the
/// caller), not the source span.
fn audio_render_args(
    path: &Path,
    in_us: i64,
    out_dur_us: i64,
    volume: f32,
    tempo: f32,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    let mut args: Vec<OsString> = Vec::with_capacity(16);
    args.push(OsString::from("-v"));
    args.push(OsString::from("error"));
    // INPUT half — every one of these must precede `-i`. `-ss` AFTER `-i` is an
    // OUTPUT seek (decode-from-zero-and-discard), a different operation with a
    // different cost; the pins assert the position, not merely the presence.
    args.extend(input_seek_args(in_us));
    args.push(OsString::from("-i"));
    args.push(path.as_os_str().to_os_string());
    // OUTPUT half.
    args.push(OsString::from("-t"));
    args.push(OsString::from(us_to_ss_arg(out_dur_us)));
    args.push(OsString::from("-vn"));
    args.push(OsString::from("-af"));
    args.push(OsString::from(audio_filter_chain(volume, tempo)));
    args.push(OsString::from("-f"));
    args.push(OsString::from("f32le"));
    args.push(OsString::from("-ac"));
    args.push(OsString::from("1"));
    args.push(OsString::from("-ar"));
    args.push(OsString::from(AUDIO_SAMPLE_RATE.to_string()));
    args.push(OsString::from("-"));
    args
}

/// [`render_audio_pcm`] plus a constant playback `tempo` (quick task
/// 260730-x2t, RT-05): the audio is TIME-STRETCHED with PITCH PRESERVED via the
/// LGPL `atempo` chain — never resampled (which would shift pitch) and never
/// through the GPL `rubberband` filter.
///
/// `tempo == 1.0` emits the identical argv `render_audio_pcm` always emitted,
/// so every one of its ~20 call sites is byte-unchanged.
///
/// `in_us`/`out_us` are SOURCE times; `out_us - in_us` of source becomes
/// `(out_us - in_us) / tempo` of output — APPROXIMATELY. `atempo`'s output
/// length is not exact (measured +0.12% at 2.0x, −0.36% at 0.25x), so a caller
/// placing this on a timeline MUST derive placement from the speed integral and
/// truncate to the expected sample count — NEVER from the returned length.
pub fn render_audio_pcm_retimed(
    path: &Path,
    in_us: i64,
    out_us: i64,
    volume: f32,
    tempo: f32,
) -> Result<Vec<f32>, EngineError> {
    render_audio_pcm_retimed_inner(path, in_us, out_us, volume, tempo, None)
}

/// [`render_audio_pcm_retimed`] for a caller that ALREADY knows whether the
/// media has an audio stream.
///
/// The public entry point spends one `probe()` — a whole ffprobe process — per
/// call, purely to answer `has_audio` ("silence is data, not an error"). That
/// was free when a call rendered ~2 s of audio, but the live preview mixer
/// calls it ONCE PER STAIRCASE WINDOW of a ramped contributor inside a
/// real-time budget, where the probe is ~50 % of the total cost (MEASURED:
/// probe ~110-130 ms, render ~90-120 ms). The mixer has already established
/// `has_audio` from the imported `MediaBinItem` before it ever builds a
/// `MixSource`, so re-probing per window is pure redundancy — see live-UAT bug
/// `retime-live-uat-frontend-mirror-undo-audio`, symptom 3.
///
/// `crate`-private on purpose: the invariant "only pass `Some` when you truly
/// know" is not one a public API can enforce, and a wrong `Some(true)` would
/// turn a silent source into an ffmpeg error instead of an empty vec. Every
/// out-of-crate caller keeps the probing entry point.
pub(crate) fn render_audio_pcm_retimed_known_audio(
    path: &Path,
    in_us: i64,
    out_us: i64,
    volume: f32,
    tempo: f32,
    has_audio: bool,
) -> Result<Vec<f32>, EngineError> {
    render_audio_pcm_retimed_inner(path, in_us, out_us, volume, tempo, Some(has_audio))
}

/// The one body. `known_has_audio: None` probes (the public contract);
/// `Some(_)` trusts the caller and skips the ffprobe spawn. EVERYTHING else —
/// the tempo contract, the `-t` division, the filter chain, the argv — is
/// shared, so the two entry points can never emit different commands.
fn render_audio_pcm_retimed_inner(
    path: &Path,
    in_us: i64,
    out_us: i64,
    volume: f32,
    tempo: f32,
    known_has_audio: Option<bool>,
) -> Result<Vec<f32>, EngineError> {
    // Tempo bounds the ffmpeg `-t` computation below AND the filter chain
    // (quick task 260730-x2t, WR-04). The two fail in OPPOSITE directions: a
    // non-finite or vanishingly small tempo makes `atempo_chain` return EMPTY
    // (so no time-stretch is applied at all) while `-t` is still divided by it
    // — `NaN.max(f32::MIN_POSITIVE)` is `MIN_POSITIVE` (Rust's `f32::max`
    // returns the other operand when self is NaN), so `dur_us / 1e-38`
    // saturates to `i64::MAX` on the float->int cast and `us_to_ss_arg` emits
    // `-t 9223372036854.775807`, i.e. "decode to EOF". The whole remaining file
    // is then buffered into a `Vec<f32>` at 48 kHz: an unbounded-memory read
    // driven by a single bad float. A tempo <= 0 fails the other way — an empty
    // chain plus a negative `-t` clamped to 0, so the contributor is silently
    // dropped from the mix.
    //
    // REJECT rather than sanitize (RT-08): a wrong tempo is a wrong export
    // length, and the caller must find out. This is a `pub` engine entry point
    // re-exported from `lib.rs`, so in-tree caller sanitization is not
    // sufficient — the invariant belongs HERE.
    if !tempo.is_finite() || tempo < MIN_TEMPO || tempo > MAX_TEMPO {
        return Err(EngineError::InvalidTempo {
            tempo,
            min: MIN_TEMPO,
            max: MAX_TEMPO,
        });
    }
    if out_us <= in_us {
        return Err(EngineError::InvalidWindow { in_us, out_us });
    }
    let has_audio = match known_has_audio {
        Some(known) => known,
        None => probe(path)?.has_audio, // malformed input fails here with Err
    };
    if !has_audio {
        return Ok(Vec::new());
    }
    let bins = locate()?;

    let dur_us = out_us - in_us.max(0);
    let volume = volume.max(0.0);
    // `-t` sits among the OUTPUT options here, so it bounds the STRETCHED
    // length, not the source window. At tempo T, `dur_us` of SOURCE becomes
    // `dur_us / T` of output — so the cap must be divided (quick task
    // 260730-x2t). Getting this wrong is not a subtle drift: with a plain
    // `-t dur`, a 2 s source window at 2.0x reads FOUR seconds of source and
    // returns ~2 s of audio (measured: 87424 samples where 48000 were wanted).
    // `tempo == 1.0` divides by one, so the un-retimed argv is byte-identical.
    let out_dur_us = if tempo == 1.0 {
        dur_us
    } else {
        (dur_us as f64 / tempo.max(f32::MIN_POSITIVE) as f64).ceil() as i64
    };
    let output = ffmpeg_command(&bins.ffmpeg)
        .args(audio_render_args(path, in_us, out_dur_us, volume, tempo))
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (audio render)".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    if output.stdout.len() % 4 != 0 {
        return Err(EngineError::BadOutputSize {
            got: output.stdout.len(),
            expected: output.stdout.len() / 4 * 4,
        });
    }

    Ok(output
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

/// The EXACT argument vector [`extract_wav_for_whisper`] spawns — `path` and
/// `wav_path` each their own element, never interpolated (threat T-22-04).
///
/// Extracted for the same reason [`audio_render_args`] is: this is where the
/// [`input_seek_args`] rule is actually applied, and an argv pin is the only gate
/// that catches its removal on a sidecar that does not exhibit the defect.
fn whisper_wav_args(
    path: &Path,
    in_us: i64,
    dur_us: i64,
    wav_path: &Path,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    let mut args: Vec<OsString> = Vec::with_capacity(16);
    args.push(OsString::from("-v"));
    args.push(OsString::from("error"));
    args.push(OsString::from("-y"));
    // INPUT half — before `-i`, and absent entirely at position 0.
    args.extend(input_seek_args(in_us));
    args.push(OsString::from("-i"));
    args.push(path.as_os_str().to_os_string());
    // OUTPUT half — whisper.cpp's required 16 kHz mono s16le.
    args.push(OsString::from("-t"));
    args.push(OsString::from(us_to_ss_arg(dur_us)));
    args.push(OsString::from("-vn"));
    args.push(OsString::from("-ar"));
    args.push(OsString::from("16000"));
    args.push(OsString::from("-ac"));
    args.push(OsString::from("1"));
    args.push(OsString::from("-c:a"));
    args.push(OsString::from("pcm_s16le"));
    args.push(wav_path.as_os_str().to_os_string());
    args
}

/// Extract the audio of `path` over `[in_us, out_us)` into a fresh 16 kHz MONO
/// signed-16-bit-LE WAV on disk — the EXACT input contract whisper.cpp requires
/// (Phase 22, TEXT-02). Reuses [`render_audio_pcm`]'s seek shape via the SAME
/// [`input_seek_args`] rule (so the zero-position AAC head loss documented there
/// cannot be reintroduced in one of the two and not the other) but writes a real
/// `.wav` FILE (whisper reads a file, not a pipe) at 16 kHz mono `pcm_s16le`
/// instead of 48 kHz f32.
///
/// This function had the SAME defect and it was measured, not assumed: with
/// `-ss 0.000000`, `speech_delayed_700ms.mp4` extracted at 16 kHz put its speech
/// onset at 850 ms and ran 4.412000 s; without it, 890 ms and 4.454688 s. Every
/// word timestamp whisper returns for an AAC source was 42.67 ms early, and
/// `transcribe_media_window` adds `in_us` back to them, so the error survived
/// into the caller's timeline coordinates.
///
/// Returns:
///   * `Ok(Some(wav_path))` — a unique temp WAV the caller MUST delete when done
///     (the `engine::whisper` layer wraps this in a scope-guard so the extracted
///     speech never lingers on disk, threat T-22-07).
///   * `Ok(None)` — the source has NO audio stream (still image / silent video):
///     silence is DATA, not an error, mirroring `render_audio_pcm`'s
///     `!has_audio => Ok(Vec::new())` contract (research Pitfall 4). The caller
///     returns an empty word list.
///   * `Err(..)` — missing/malformed input, an empty window, or a sidecar
///     failure (never a panic).
///
/// Every path is passed as a SEPARATE `.arg()` (no shell string interpolation —
/// threat T-22-04, Security V12), exactly like the rest of this module.
pub fn extract_wav_for_whisper(
    path: &Path,
    in_us: i64,
    out_us: i64,
) -> Result<Option<PathBuf>, EngineError> {
    if out_us <= in_us {
        return Err(EngineError::InvalidWindow { in_us, out_us });
    }
    // Resource-exhaustion cap (threat T-22-06) folded into the natural choke
    // point so EVERY caller — not just `transcribe_media_window` — is protected
    // against a pathologically long window before any WAV is written (WR-03).
    // The cap value AND its message live in a single source
    // (`whisper::check_whisper_window_cap`) so the two call sites can't drift.
    crate::whisper::check_whisper_window_cap(in_us, out_us)?;
    let info = probe(path)?; // malformed input fails here with Err, not a panic
    if !info.has_audio {
        return Ok(None); // silence is data (Pitfall 4)
    }
    let bins = locate()?;

    let dur_us = out_us - in_us.max(0);
    let wav_path = std::env::temp_dir().join(format!("rudis-whisper-{}.wav", unique_temp_stem()));

    let output = ffmpeg_command(&bins.ffmpeg)
        .args(whisper_wav_args(path, in_us, dur_us, &wav_path))
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        // Best-effort cleanup of any partial file before surfacing the error.
        let _ = std::fs::remove_file(&wav_path);
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (whisper wav extract)".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(Some(wav_path))
}

/// A process-unique, collision-resistant filename stem for temp artifacts,
/// built WITHOUT a new dependency (no `uuid` crate): pid + a monotonically
/// increasing counter + the current nanosecond clock. Used for the whisper WAV
/// / JSON temp files so concurrent transcriptions never clobber each other.
pub(crate) fn unique_temp_stem() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), n, nanos)
}

/// Root-mean-square level of a PCM buffer. Empty input (silence / no audio)
/// is 0.0. Pure math — the measurement side of the audio verification gate.
pub fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt()
}

/// Re-encode `src` into `dst` at `out_w` x `out_h` with video codec `vcodec`.
///
/// Proves a real re-encode (scaling forces a full decode -> filter -> encode
/// pipeline; the output cannot be a stream copy of the source).
///
/// NOTE (DEV ONLY): tests pass `libx264` here, which is GPL — acceptable for
/// the local Homebrew dev build only and NEVER shipped. The ship path on
/// Windows uses hardware / Media Foundation encoders (NVENC/QSV/VCE) through
/// an LGPL FFmpeg build. // PENDING WINDOWS VERIFICATION
pub fn encode_from_source(
    src: &Path,
    dst: &Path,
    out_w: u32,
    out_h: u32,
    vcodec: &str,
) -> Result<(), EngineError> {
    let bins = locate()?;

    let output = ffmpeg_command(&bins.ffmpeg)
        .args(["-v", "error", "-y"])
        .arg("-i")
        .arg(src)
        .args([
            "-vf",
            &format!("scale={out_w}:{out_h}"),
            "-c:v",
            vcodec,
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ])
        .arg(dst)
        .stdin(Stdio::null())
        .output()?;

    if !output.status.success() {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (encode)".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 58: proxy / optimized-media encoder (PROXY-01)
// ---------------------------------------------------------------------------

/// The Media Foundation quality target (0–100) for proxy encodes, driving
/// `-rate_control quality -quality N` on the `*_mf` encoders.
///
/// **MEASURED 2026-08-03** (debug session `proxy-bitrate-starved-all-intra`),
/// on the worst-case real source the defect was reported against — a 23 s
/// aerial Shibuya-crossing 4K 59.94 fps clip, encoded to a 960x540 all-intra
/// proxy and scored against the source downscaled losslessly to proxy dims:
///
/// | setting            | Mbps | SSIM-All | PSNR-Y | worst-frame PSNR |
/// |--------------------|------|----------|--------|------------------|
/// | flat 6 Mbps (old)  |  6.0 | 0.767    | 24.96  | 23.95 — macroblocked |
/// | `-b:v` 20 Mbps     | 19.7 | 0.928    | 31.67  | 31.48 |
/// | quality 30         | 15.1 | 0.907    | 30.31  | 31.37 |
/// | **quality 45**     | 23.2 | 0.941    | 32.68  | 33.34 |
/// | quality 55         | 30.8 | 0.957    | 34.49  | 34.77 |
///
/// 45 was chosen from real decoded frames: at 3x zoom (past the ~1.34x the UI
/// ever upscales a 960 px proxy) it shows zero macroblocking and no chroma
/// bleed, while quality 55 costs ~32% more bytes for a difference invisible at
/// display scale. Quality mode also holds a NEAR-FLAT per-frame floor
/// (min/max PSNR within ~1 dB) where a fixed `-b:v` swings ~8 dB across the
/// same clip, and it self-scales with fps and content: the same 45 spends
/// 23 Mbps on dense 4K59.94 but only ~8 Mbps on a typical 30 fps clip — the
/// two axes the old flat `PROXY_BITRATE_BPS = 6_000_000` constant ignored,
/// which is exactly why all-intra proxies of dense high-fps footage shipped
/// at 0.19 bits/pixel and macroblocked during playback.
///
/// The selected MFT is Windows' own software `H264 Encoder MFT`
/// (`hw_encoding` defaults to false in ffmpeg's mfenc), so the behaviour is
/// the same on every Windows 8+ machine, and the honored quality mode was
/// verified empirically: sizes scale monotonically with this value while an
/// explicit `-b:v` is ignored, and the all-intra census still reads
/// keyframes == frames under quality mode.
pub const PROXY_ENCODE_QUALITY: u32 = 45;

/// Is `encoder` one of ffmpeg's Media Foundation VIDEO encoders — the only
/// family whose private `-rate_control`/`-quality`/`-hw_encoding`/`-scenario`
/// options any argument builder in this file may emit? An explicit list, not a
/// suffix match, so a future foreign encoder name cannot accidentally receive
/// MF-only flags (an unknown private option is a hard spawn failure, not a
/// warning).
///
/// **Public since Phase 59 plan 59-11**, and the reason is not convenience:
/// `rendercache`'s segment meta has to record WHICH rate-control policy
/// produced a payload, and the writer, the reader and the argument builder must
/// all ask the SAME question. One discriminator with four callers
/// ([`proxy_encode_args`], [`export_rate_control_args`],
/// [`render_cache_encode_args`], and `rendercache::cache::effective_rate_control`)
/// — a second, forked list is exactly how a payload and its own meta come to
/// disagree about what produced it.
///
/// **These are NOT "the hardware encoders", and the file used to say they
/// were.** On Windows the MFT that ffmpeg's `h264_mf` actually selects is
/// Microsoft's own SOFTWARE `H264 Encoder MFT` — `hw_encoding` defaults to
/// **false** in ffmpeg's `mfenc`, verified against the shipped
/// `runtime/binaries/ffmpeg.exe`'s own `-h encoder=h264_mf`. That is why the
/// behaviour is identical on every Windows 8+ machine regardless of GPU, and
/// why `-hw_encoding true` is a real, measurable, currently-unexercised door
/// (see [`RENDER_CACHE_HW_ENCODING_ENV`]) rather than a no-op.
pub fn is_mf_video_encoder(encoder: &str) -> bool {
    matches!(encoder, "h264_mf" | "hevc_mf")
}

/// Is `encoder` one of ffmpeg's NVIDIA NVENC VIDEO encoders — the only family
/// whose private `-rc`/`-qp` options any argument builder in this file may
/// emit?
///
/// **An explicit list, not a suffix match**, for exactly the reason
/// [`is_mf_video_encoder`] is one: an unknown private option is a HARD SPAWN
/// FAILURE, not a warning. `-rc constqp` handed to `h264_mf` kills the child;
/// `-rate_control quality` handed to `h264_nvenc` kills it just as dead. A
/// `ends_with("_nvenc")` test would hand nvenc-private flags to any future
/// foreign name that happened to end that way, and the failure would surface as
/// "the render cache stopped working" three layers from the cause.
///
/// The two families are DISJOINT and both are cleared: `h264_mf` is the shipped
/// Windows Media Foundation path (Phase 58 D-03), `h264_nvenc` is the
/// nv-codec-headers dynamic-load path this build enables with
/// `--enable-ffnvcodec` and no `--enable-gpl`/`--enable-nonfree` (see
/// [`RENDER_CACHE_PREFERRED_ENCODERS`]).
pub fn is_nvenc_video_encoder(encoder: &str) -> bool {
    matches!(encoder, "h264_nvenc" | "hevc_nvenc")
}

/// The Phase-58 spelling of [`is_mf_video_encoder`], kept as a call-through
/// rather than renamed at its two elder call sites: a rename is a diff, and
/// Phase 59 D-29 freezes export's instrument and treats the shipped,
/// separately-verified proxy path the same way. One list, two names, zero
/// behaviour change.
fn is_media_foundation_video_encoder(encoder: &str) -> bool {
    is_mf_video_encoder(encoder)
}

/// Build the argument vector for one proxy encode.
///
/// This vector's all-intra shape was verified empirically against the real
/// `test-media/longgop_4k30_60s.mp4` fixture with the vendored LGPL binaries
/// (58-RESEARCH § RQ1): every output frame came back a keyframe, at 4K and at
/// 960 px, while an otherwise-identical `-g 300` control kept its Long-GOP
/// structure. Each flag is load-bearing:
///
/// * `-g 1` — **D-01's all-intra core.** Every frame is an I-frame, so entry /
///   scrub cost stops being linear in the SOURCE's (uncontrollable) GOP depth.
/// * `-bf 0` — no B-frames. All-intra with B-frames would be incoherent, and
///   B-frames add reorder latency for nothing on a playback proxy.
/// * `-an` — **D-06: proxies are video-only.** Audio always comes from the
///   ORIGINAL, so a second audio source (and its drift risk) never exists.
///   This is one of the three reasons the test-only file-to-file re-encoder
///   defined immediately above could not be extended for this job: it hardcodes
///   `-c:a aac` with no way to turn audio off.
/// * `-noautorotate` — matches the app's own decode convention (see
///   [`ExportRunDecoder`]): the proxy stores UNROTATED pixels at the CONTAINER
///   dimensions, so playback keeps applying the original's
///   [`MediaInfo::rotation_degrees`] exactly once. Without it a rotated source
///   would be baked-rotated in the proxy AND rotated again at present time.
/// * `-pix_fmt yuv420p` — the universally decodable 8-bit format; a proxy that
///   only some decoders can open is not a proxy.
///
/// # Rate control is per-encoder-family (debug `proxy-bitrate-starved-all-intra`)
///
/// * On the SHIPPED Media Foundation path (`h264_mf`/`hevc_mf`) the encode is
///   QUALITY-TARGETED: `-rate_control quality -quality` [`PROXY_ENCODE_QUALITY`],
///   and `bitrate_bps` is NOT emitted at all. A flat bitrate is the wrong
///   control for an ALL-INTRA stream — every frame costs a full I-frame, so a
///   long-GOP-shaped constant starves dense-motion/high-fps content into
///   macroblocks while over-spending on easy content. Quality mode was
///   verified honored by the Windows software `H264 Encoder MFT` (output size
///   tracks the quality value, ignores `-b:v`) and verified all-intra under
///   `-g 1` by a real decoded keyframe census.
/// * On any OTHER encoder (reachable only through the loud DEV-only
///   [`DEV_ENCODER_OVERRIDE_ENV`] door) the MF-private options would be a hard
///   spawn failure, so the caller-computed `bitrate_bps` is emitted as `-b:v`
///   instead — the caller derives it from pixel rate (bits-per-pixel x W x H
///   x fps), never from a flat constant.
///
/// Deliberately ABSENT: any fps, timebase, or duration flag. **D-04** — a
/// proxy has the same fps, the same timebase and the same duration as its
/// source; only the pixels and the GOP change. That is what makes
/// `DecodeSource::source_us` an identity mapping and deletes a whole class of
/// A/V-drift bugs before it can exist.
fn proxy_encode_args(
    src: &Path,
    dst: &Path,
    out_w: u32,
    out_h: u32,
    bitrate_bps: u64,
    encoder: &str,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    let mut args: Vec<OsString> = Vec::with_capacity(27);
    let mut push = |s: &str| args.push(OsString::from(s));

    push("-v");
    push("error");
    push("-y");
    // 71-01 (TRUST-03): publish the child's OWN progress as machine-readable
    // `key=value` lines on stderr (the stream the pump already drains), four
    // times a second, with the human stats line silenced. These are global
    // options, so they sit before every input option below. The pump parses
    // `out_time_us=` into `ProxyEncodeChild::progress_out_time_us` and keeps
    // every progress line OUT of the 64 KiB error capture.
    push("-nostats");
    push("-progress");
    push("pipe:2");
    push("-stats_period");
    push("0.25");
    // INPUT option — must precede `-i` to apply to the source.
    push("-noautorotate");
    push("-i");
    args.push(src.as_os_str().to_os_string());
    args.push(OsString::from("-vf"));
    args.push(OsString::from(format!("scale={out_w}:{out_h}")));
    args.push(OsString::from("-c:v"));
    args.push(OsString::from(encoder));
    if is_media_foundation_video_encoder(encoder) {
        // Shipped path: quality-targeted, content-adaptive, fps-adaptive.
        args.push(OsString::from("-rate_control"));
        args.push(OsString::from("quality"));
        args.push(OsString::from("-quality"));
        args.push(OsString::from(PROXY_ENCODE_QUALITY.to_string()));
    } else {
        // DEV-override door only: MF-private options would fail the spawn, so
        // fall back to the caller's pixel-rate-derived bitrate.
        args.push(OsString::from("-b:v"));
        args.push(OsString::from(bitrate_bps.to_string()));
    }
    args.push(OsString::from("-g"));
    args.push(OsString::from("1"));
    args.push(OsString::from("-bf"));
    args.push(OsString::from("0"));
    args.push(OsString::from("-pix_fmt"));
    args.push(OsString::from("yuv420p"));
    args.push(OsString::from("-an"));
    args.push(dst.as_os_str().to_os_string());
    args
}

/// A live proxy-encode sidecar process.
///
/// Returned by [`spawn_proxy_encode`]. The caller owns the child's lifetime:
/// poll it with [`try_wait`](Self::try_wait), block on it with
/// [`wait`](Self::wait), or **cancel it for real** with
/// [`kill_and_reap`](Self::kill_and_reap).
///
/// Phase 58 D-11 requires cancellation to be real, not best-effort. The
/// codebase's existing cancel precedent (`agent-gen`'s `Arc<AtomicBool>` latch)
/// cancels a *remote, polled* job, where "stop polling" is enough. A local
/// sidecar is different: one `ffmpeg` invocation runs to completion once
/// spawned and checks no flag, so the only real cancellation is killing the
/// process. That is what this handle exists to make possible.
///
/// # stderr is drained CONCURRENTLY, and that is load-bearing
///
/// The pipe is taken out of the child at spawn time and pumped by a dedicated
/// thread for the child's whole life. It is **not** read by
/// [`wait`](Self::wait), and it must never go back to being read there:
/// [`try_wait`](Self::try_wait) is the polling caller's only contact with the
/// process, so a pipe that is only drained *after* exit is a pipe that is never
/// drained while the child is alive.
///
/// `-v error` bounds the *verbosity* of that stream, not its *volume*.
/// MEASURED 2026-08-03 on this machine, bundled LGPL build: a corrupted 25 s 4K
/// source encoded with exactly these arguments emits **126 260 bytes** of
/// error-level output (`error while decoding MB …`, once per macroblock run),
/// against Rust's 64 KiB pipe capacity. With the pipe undrained the child blocks
/// on `write`, `try_wait` answers `Ok(None)` forever, and the caller's poll loop
/// spins until the process is killed — reproduced at exactly that volume, and
/// pinned by `a_chatty_encode_cannot_wedge_a_polling_caller`.
#[derive(Debug)]
pub struct ProxyEncodeChild {
    child: std::process::Child,
    /// What the pump has kept, capped at [`PROXY_STDERR_CAPTURE_BYTES`].
    stderr: Arc<Mutex<String>>,
    /// How many bytes the pump has READ, uncapped. The cap above bounds memory;
    /// this counts what actually crossed the pipe, which is the only honest
    /// measure of whether a run came anywhere near wedging it.
    stderr_bytes_seen: Arc<std::sync::atomic::AtomicU64>,
    /// The drain thread. Joined by [`wait`](Self::wait) /
    /// [`kill_and_reap`](Self::kill_and_reap); `None` once joined, and `None`
    /// from the start in the (unreachable) case where the child had no stderr
    /// handle to take.
    pump: Option<std::thread::JoinHandle<()>>,
    /// The latest `out_time_us=` the child reported on its own `-progress`
    /// stream (71-01, TRUST-03). `-1` until the first report.
    progress_out_time_us: Arc<std::sync::atomic::AtomicI64>,
}

impl ProxyEncodeChild {
    /// Non-blocking poll. `Ok(None)` while the encode is still running,
    /// `Ok(Some(status))` once it has exited.
    ///
    /// Safe to call in a loop **because a pump thread owns the stderr pipe and
    /// drains it continuously** (see the type's own doc). This method reads
    /// nothing itself, deliberately: a poll that had to drain a pipe would be a
    /// poll that blocks.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Block until the encode finishes, mapping a non-zero exit to
    /// [`EngineError::SidecarFailed`] with the captured stderr.
    ///
    /// The diagnostics come from the pump thread, which is joined here — so a
    /// genuinely-failing encode still reports what ffmpeg said, without this
    /// method ever having touched the pipe.
    pub fn wait(&mut self) -> Result<(), EngineError> {
        let status = self.child.wait()?;
        let stderr_text = self.join_pump();
        if !status.success() {
            return Err(EngineError::SidecarFailed {
                tool: "ffmpeg (proxy encode)".to_string(),
                status: status.code().unwrap_or(-1),
                stderr: stderr_text.trim().to_string(),
            });
        }
        Ok(())
    }

    /// Cancel the encode: kill the child, then REAP it (D-11).
    ///
    /// Best-effort by construction — a child that already exited makes
    /// `kill()` fail, and that is fine. The `wait()` after the kill is the
    /// non-optional half: without it the process stays a zombie in the table
    /// and its file handles are not guaranteed released, which would defeat the
    /// "a cancelled proxy leaves nothing behind" guarantee its caller builds on.
    ///
    /// The pump is joined LAST: the killed process closes its end of the pipe,
    /// the pump sees EOF, and the join is therefore bounded by the kill rather
    /// than by the encode. (A child wedged mid-`write` cannot exist here — the
    /// pump has been draining all along — but even one that did would be
    /// unblocked by the kill.)
    pub fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.join_pump();
    }

    /// Everything the pump kept from the child's stderr, capped at
    /// [`PROXY_STDERR_CAPTURE_BYTES`].
    ///
    /// Safe to call at any time: while the encode runs it is a snapshot of what
    /// has arrived so far, and after [`wait`](Self::wait) /
    /// [`kill_and_reap`](Self::kill_and_reap) it is the whole (capped) stream.
    /// Diagnostics and tests only — nothing in the app branches on it.
    pub fn stderr_snapshot(&self) -> String {
        self.stderr
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// Total bytes the pump has read off the child's stderr, UNCAPPED — i.e.
    /// how much really crossed the pipe, as opposed to how much
    /// [`stderr_snapshot`](Self::stderr_snapshot) kept. Diagnostics and tests
    /// only.
    pub fn stderr_bytes_seen(&self) -> u64 {
        self.stderr_bytes_seen
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Join the drain thread (once) and hand back what it captured.
    fn join_pump(&mut self) -> String {
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        self.stderr_snapshot()
    }

    /// The OS process id of the encode sidecar (diagnostics / tests).
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// How far the encode has got, in microseconds of OUTPUT time, as the
    /// child itself last reported it on its `-progress pipe:2` stream
    /// (71-01, TRUST-03). `-1` before the first report.
    ///
    /// Non-blocking: a relaxed atomic load of what the pump thread parsed.
    /// This is the child's own number, not an estimate — callers turn it into
    /// a fraction against the SOURCE's probed duration (a proxy keeps the
    /// source's timebase and duration, see [`proxy_encode_args`]). An `N/A`
    /// report (early in some encodes) is ignored rather than stored.
    pub fn progress_out_time_us(&self) -> i64 {
        self.progress_out_time_us
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// How much of a proxy encode's stderr is KEPT for diagnostics. Bytes past this
/// are still **read** (that is the half that prevents the deadlock) and then
/// discarded, so a pathologically chatty encode costs bounded memory rather
/// than a bounded pipe.
const PROXY_STDERR_CAPTURE_BYTES: usize = 64 * 1024;

/// Take `stderr` out of a freshly-spawned proxy encode and drain it on a
/// dedicated thread for the child's whole life.
///
/// Returns the sink the caller keeps, the uncapped byte counter, and the handle
/// it must join. The thread ends at EOF, which arrives when the child exits
/// (naturally or killed) and closes its write end — so it can outlive neither
/// the process nor the handle.
type StderrPump = (
    Arc<Mutex<String>>,
    Arc<std::sync::atomic::AtomicU64>,
    Option<std::thread::JoinHandle<()>>,
);

fn spawn_stderr_pump(child: &mut std::process::Child) -> StderrPump {
    use std::sync::atomic::{AtomicU64, Ordering};

    let sink = Arc::new(Mutex::new(String::new()));
    let seen = Arc::new(AtomicU64::new(0));
    let pump = child.stderr.take().map(|mut err| {
        let sink = Arc::clone(&sink);
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            use std::io::Read;
            let mut kept: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match err.read(&mut chunk) {
                    Ok(0) => break, // EOF: the child exited and closed the pipe.
                    Ok(n) => {
                        seen.fetch_add(n as u64, Ordering::Relaxed);
                        // Keep up to the cap; READ everything regardless — the
                        // reading is what keeps the pipe from filling, the
                        // keeping is only for the error message.
                        if kept.len() < PROXY_STDERR_CAPTURE_BYTES {
                            let room = PROXY_STDERR_CAPTURE_BYTES - kept.len();
                            kept.extend_from_slice(&chunk[..n.min(room)]);
                            // Publish incrementally so `stderr_snapshot` is
                            // useful while the encode is still running.
                            if let Ok(mut s) = sink.lock() {
                                *s = String::from_utf8_lossy(&kept).into_owned();
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if let Ok(mut s) = sink.lock() {
                *s = String::from_utf8_lossy(&kept).into_owned();
            }
        })
    });
    (sink, seen, pump)
}

/// The proxy encode's pump: [`spawn_stderr_pump`]'s contract (drained for
/// the child's whole life, capped capture, uncapped byte count) PLUS the parsed
/// `out_time_us=` of a child run with `-progress pipe:2` (71-01, TRUST-03).
///
/// A separate function rather than a change to [`spawn_stderr_pump`], whose
/// other caller (the render-cache segment encoder) runs no `-progress` stream
/// and keeps its raw-chunk capture byte-for-byte. The read loop lives in
/// [`pump_proxy_stderr`] so it can be pinned with a `Cursor` and no process.
#[allow(clippy::type_complexity)]
fn spawn_proxy_stderr_pump(
    child: &mut std::process::Child,
) -> (
    Arc<Mutex<String>>,
    Arc<std::sync::atomic::AtomicU64>,
    Arc<std::sync::atomic::AtomicI64>,
    Option<std::thread::JoinHandle<()>>,
) {
    use std::sync::atomic::{AtomicI64, AtomicU64};

    let sink = Arc::new(Mutex::new(String::new()));
    let seen = Arc::new(AtomicU64::new(0));
    let out_time_us = Arc::new(AtomicI64::new(-1));
    let pump = child.stderr.take().map(|err| {
        let sink = Arc::clone(&sink);
        let seen = Arc::clone(&seen);
        let out_time_us = Arc::clone(&out_time_us);
        std::thread::spawn(move || {
            pump_proxy_stderr(std::io::BufReader::new(err), &sink, &seen, &out_time_us);
        })
    });
    (sink, seen, out_time_us, pump)
}

/// Drain `reader` to EOF, line by line (71-01, TRUST-03).
///
/// Per line:
/// * `out_time_us=<i64>` stores `max(v, 0)` into `out_time_us` (`N/A` or any
///   unparsable value is ignored — never stored as progress);
/// * any other `-progress` key line ([`is_progress_key_line`]) is dropped;
/// * every other non-empty line is a genuine `-v error` diagnostic and is
///   appended (with a newline) to `kept`, up to [`PROXY_STDERR_CAPTURE_BYTES`].
///
/// `seen` counts every byte READ, uncapped. Reading continues past the cap —
/// the reading is what keeps the pipe from filling (the
/// `a_chatty_encode_cannot_wedge_a_polling_caller` guard); the keeping is only
/// for the error message. Progress lines are filtered BEFORE the cap
/// accounting, so a long encode's progress stream can never displace a real
/// diagnostic (T-71-01). `kept` is published line by line, so
/// `stderr_snapshot` stays useful mid-encode.
///
/// Lines are read as BYTES and decoded lossily: `read_line` into a `String`
/// errors on invalid UTF-8, and an error here would stop the drain — which is
/// exactly the wedge this pump exists to prevent.
///
/// **71-REVIEW WR-03: memory is bounded whatever the child writes.** Both `\n`
/// and `\r` end a line, so ffmpeg's `\r`-only stats line (should `-nostats` ever
/// be dropped, or a dev encoder override print one) is parsed line by line rather
/// than accumulated. A line longer than [`PROXY_STDERR_MAX_LINE`] is handled as
/// its first `PROXY_STDERR_MAX_LINE` bytes, and the rest of it, up to the next
/// terminator, is read and counted into `seen` but discarded. The line buffer
/// therefore never grows past that cap.
fn pump_proxy_stderr(
    mut reader: impl std::io::BufRead,
    kept: &Mutex<String>,
    seen: &std::sync::atomic::AtomicU64,
    out_time_us: &std::sync::atomic::AtomicI64,
) {
    use std::sync::atomic::Ordering;

    let handle_line = |bytes: &[u8]| {
        let line = String::from_utf8_lossy(bytes);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(v) = trimmed.strip_prefix("out_time_us=") {
            if let Ok(us) = v.trim().parse::<i64>() {
                out_time_us.store(us.max(0), Ordering::Relaxed);
            }
            return;
        }
        if is_progress_key_line(trimmed) {
            return;
        }
        let mut s = kept.lock().unwrap_or_else(|p| p.into_inner());
        if s.len() < PROXY_STDERR_CAPTURE_BYTES {
            let room = PROXY_STDERR_CAPTURE_BYTES - s.len();
            let mut piece = String::with_capacity(trimmed.len() + 1);
            piece.push_str(trimmed);
            piece.push('\n');
            if piece.len() > room {
                let mut cut = room;
                while cut > 0 && !piece.is_char_boundary(cut) {
                    cut -= 1;
                }
                piece.truncate(cut);
            }
            s.push_str(&piece);
        }
    };

    let mut buf: Vec<u8> = Vec::with_capacity(256);
    // True while skipping the tail of an over-long line, up to its terminator.
    let mut discarding = false;
    loop {
        let (take, ended) = {
            let chunk = match reader.fill_buf() {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if chunk.is_empty() {
                // EOF: the child exited and closed the pipe. A final line with
                // no terminator is still a line.
                if !discarding && !buf.is_empty() {
                    handle_line(&buf);
                }
                break;
            }
            let (take, ended) = match chunk.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(i) => (i + 1, true),
                None => (chunk.len(), false),
            };
            if !discarding {
                let room = PROXY_STDERR_MAX_LINE - buf.len();
                buf.extend_from_slice(&chunk[..take.min(room)]);
            }
            (take, ended)
        };
        reader.consume(take);
        seen.fetch_add(take as u64, Ordering::Relaxed);
        if ended {
            if !discarding {
                handle_line(&buf);
            }
            buf.clear();
            discarding = false;
        } else if !discarding && buf.len() >= PROXY_STDERR_MAX_LINE {
            handle_line(&buf);
            buf.clear();
            discarding = true;
        }
    }
}

/// The longest stderr line [`pump_proxy_stderr`] buffers (71-REVIEW WR-03).
/// Real `-v error` diagnostics and `-progress` key lines are far shorter.
const PROXY_STDERR_MAX_LINE: usize = 4096;

/// Spawn an all-intra proxy encode of `src` into `dst` at `out_w` x `out_h`
/// (PROXY-01). Returns immediately with a live [`ProxyEncodeChild`].
///
/// **There is no encoder parameter, and that omission is the point (D-03 /
/// D-32).** The test-only file-to-file re-encoder's open `vcodec: &str` is
/// exactly the GPL door this path must never inherit — its own caller
/// deliberately passes `libx264` (dev/test only). Here the encoder is resolved
/// the same way
/// [`VideoEncoder::new`] resolves it and no other way:
///
/// 1. [`DEFAULT_VIDEO_ENCODER`] — the already-cleared, already-shipped,
///    hardware, license-safe encoder (`h264_mf` on Windows).
/// 2. ...UNLESS [`DEV_ENCODER_OVERRIDE_ENV`] is set and non-empty, in which
///    case that value is used **with the same LOUD stderr warning** — never
///    silently. That override is the ONLY door to a GPL encoder, it is dev-only,
///    and it announces itself.
///
/// Availability is checked with [`encoder_available`] BEFORE spawning, so an
/// ffmpeg build without the cleared encoder fails **clearly**, never silently
/// and never by falling back to something we have not cleared.
///
/// Process discipline:
/// * below-normal priority on Windows ([`BELOW_NORMAL_PRIORITY_CLASS`], D-10) —
///   playback outranks proxy generation;
/// * `stdin` null (this is a file→file transcode, nothing is piped in);
/// * `stderr` piped **and drained by a dedicated pump thread from the moment
///   the child exists** ([`spawn_stderr_pump`]), so
///   [`try_wait`](ProxyEncodeChild::try_wait) polling cannot deadlock on a full
///   pipe buffer and [`wait`](ProxyEncodeChild::wait) still has real
///   diagnostics to report when something does go wrong. `-v error` bounds how
///   CHATTY the stream is, not how LARGE it can get — a corrupt source emits
///   error-level lines per macroblock run and overruns the 8 KiB pipe in well
///   under a second (measured; see [`ProxyEncodeChild`]).
///
/// The caller owns `dst`'s naming policy. This function writes exactly where it
/// is told — the temp-name-then-atomic-rename discipline that makes a killed
/// encode's partial output unreachable belongs to the proxy CACHE, not here.
///
/// # `bitrate_bps` is the NON-MF FALLBACK, not the shipped control
///
/// Since the `proxy-bitrate-starved-all-intra` fix, the shipped Media
/// Foundation path (`h264_mf`/`hevc_mf`) encodes QUALITY-TARGETED at
/// [`PROXY_ENCODE_QUALITY`] and does not emit `-b:v` at all — see
/// [`proxy_encode_args`] for the measurements. `bitrate_bps` is only emitted
/// when the DEV-only [`DEV_ENCODER_OVERRIDE_ENV`] door selects a non-MF
/// encoder that cannot accept the MF-private quality options. Callers passing
/// a bitrate here (e.g. the Phase 59 `segment_decode_spike`) should treat it
/// as a ceiling REQUEST that the shipped encoder deliberately ignores.
pub fn spawn_proxy_encode(
    src: &Path,
    dst: &Path,
    out_w: u32,
    out_h: u32,
    bitrate_bps: u64,
) -> Result<ProxyEncodeChild, EngineError> {
    let bins = locate()?;

    // Encoder resolution — identical discipline to VideoEncoder::new.
    let encoder_name = match std::env::var(DEV_ENCODER_OVERRIDE_ENV) {
        Ok(v) if !v.trim().is_empty() => {
            eprintln!(
                "!!! RUDIS DEV-ONLY ENCODER OVERRIDE ACTIVE: using '{v}' instead of \
                 {DEFAULT_VIDEO_ENCODER} for PROXY generation (set via \
                 {DEV_ENCODER_OVERRIDE_ENV}). This is NEVER valid in a shipped build — \
                 GPL/patent-encumbered encoders must not ship. !!!"
            );
            v
        }
        _ => DEFAULT_VIDEO_ENCODER.to_string(),
    };

    // Fail clearly (not silently) if the cleared encoder is genuinely absent.
    if !encoder_available(&bins, &encoder_name)? {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (proxy encoder probe)".to_string(),
            status: -1,
            stderr: format!(
                "encoder '{encoder_name}' not available in this ffmpeg build; proxy \
                 generation requires a hardware/license-safe H.264 encoder (see \
                 {DEV_ENCODER_OVERRIDE_ENV} for a DEV-ONLY fallback)"
            ),
        });
    }

    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut cmd = ffmpeg_command(&bins.ffmpeg);
    cmd.args(proxy_encode_args(
        src,
        dst,
        out_w,
        out_h,
        bitrate_bps,
        &encoder_name,
    ));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // NOTE: `creation_flags` REPLACES the flag word, it does not OR into
        // it — `ffmpeg_command` already set CREATE_NO_WINDOW, so the combined
        // mask must be passed here or the console window comes back.
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    // Before ANYTHING else touches the child: hand its stderr to a pump. The
    // window between `spawn` and this line is the only one in which the pipe is
    // undrained, and nothing in it blocks.
    let (stderr, stderr_bytes_seen, progress_out_time_us, pump) =
        spawn_proxy_stderr_pump(&mut child);
    Ok(ProxyEncodeChild {
        child,
        stderr,
        stderr_bytes_seen,
        pump,
        progress_out_time_us,
    })
}

// --- RENDER-CACHE-ENCODER REGION START (Phase 59, plan 59-03) --------------
//
// Everything between this marker and RENDER-CACHE-ENCODER REGION END is the
// render-cache segment encoder and nothing else. `crates/engine/tests/
// render_cache_encode.rs` reads the text between the two markers and asserts,
// as source text, that this region asks for `-an` and `-g`, resolves the
// encoder through the ONE cleared chokepoint, and names no GPL or
// patent-encumbered encoder — so a future edit cannot open a second licensing
// door here without the gate noticing (CLAUDE.md rule 6, threat T-59-03-01).
//
// Keep the markers. Keep the region self-contained.

/// How long [`RenderCacheEncoder::finish`] waits for the sidecar to exit after
/// stdin EOF before it gives up, kills the child and reports failure.
///
/// The sibling [`ProxyEncodeChild::wait`] blocks forever, which is right for a
/// caller that is already polling `try_wait` in its own loop. This encoder is
/// driven SYNCHRONOUSLY by the render worker's frame loop, so an ffmpeg that
/// never exits after EOF would wedge that worker for the life of the process
/// instead of just failing one segment. 60 s is two orders of magnitude above
/// the measured cost of muxing a 2 s segment.
const RENDER_CACHE_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The Media Foundation quality target (0–100) for render-cache SEGMENT
/// encodes, driving `-rate_control quality -quality N` on the `*_mf` encoders.
///
/// # Why this is a SEPARATE constant and not a reuse of [`PROXY_ENCODE_QUALITY`]
///
/// A proxy is allowed to look softer than its original — it is a disposable
/// 960 px playback stand-in, and the user never sees it at full canvas. **A
/// cache segment is not.** CACHE-02 / D-06 forbid a visible discontinuity where
/// playback crosses from cached pixels to live ones, and a segment IS that
/// tick's composite, presented at full canvas. So the cache constant is
/// calibrated against the boundary MAD the proxy constant never had to meet,
/// and it sits above the proxy's 45.
///
/// # MEASURED — 2026-08-04, 59-11 Task 3, `render_cache_quality_calibration`
///
/// One `SEG_US` (61 frames at 30 fps, 1920x1080) per arm, pushed through the
/// segment encoder's own command line, decoded back with the bundled LGPL
/// binaries, and scored as mean-absolute-difference against the pushed source
/// frames on the project's standing metric (the `EXPORT_MATCH_MAD = 12.0`
/// ruler, carried from `crates/ffi/tests/export_parity.rs`). Two sources: the
/// `testsrc2` high-entropy shape 59-01's A1 spike used, and REAL camera footage
/// (`ttt_cut180s.mov`, 1620x1080, centred un-scaled on the canvas; MAD taken
/// over the content window). The `b24` arm is the OLD shipped policy — a flat
/// `-b:v` 24 Mbps through the same encoder — so the "before" column is
/// MEASURED here, not quoted.
///
/// <!-- RENDER-CACHE-QUALITY-CAL — regenerate with the ignored test -->
/// | arm | source | MB / media-s | keyframes | MAD | worst-frame MAD |
/// |------|-----------|-------|-------|--------|--------|
/// | q55  | testsrc2  | 1.556 | 61/61 | 0.9193 | 1.0296 |
/// | q60  | testsrc2  | 1.786 | 61/61 | 0.8485 | 1.0255 |
/// | q65  | testsrc2  | 1.895 | 61/61 | 0.8232 | 1.0591 |
/// | b24  | testsrc2  | 2.136 | 61/61 | 0.7280 | 0.8413 |
/// | **q55** | **real**  | **1.462** | **61/61** | **1.7494** | **1.7862** |
/// | q60  | real      | 1.746 | 61/61 | 1.5952 | 1.6245 |
/// | q65  | real      | 1.884 | 61/61 | 1.5534 | 1.5844 |
/// | b24  | real      | 2.300 | 61/61 | 1.4276 | 1.5674 |
///
/// **Selection rule, stated before the run and applied mechanically:** the
/// LOWEST quality whose MAD is ≤ 2.0 on synthetic AND ≤ 6.0 on real footage —
/// 1/6 and 1/2 of the 12.0 bound, which is SHARED with export parity and may
/// not move (`59-CACHE-CALIBRATION.md` § 7). **q55 clears both by ~2x and 3.4x
/// and is the lowest arm, so 55 is the value.** It sits above the proxy's 45,
/// as CACHE-02 requires, and it is the row the proxy calibration measured at
/// SSIM 0.957 / PSNR-Y 34.49.
///
/// # What this ladder does NOT claim, stated because the honest reading matters
///
/// * **The mode change is not a measured QUALITY win on these two sources.**
///   The flat `b24` arm scores a marginally BETTER mean MAD than every quality
///   arm (0.7280 / 1.4276) while spending 22–57 % more bytes. What quality mode
///   buys here is measured in the byte column and in adaptivity, not in MAD.
/// * **`59-CACHE-CALIBRATION` § 2's 10.9x undershoot did NOT reproduce.** A
///   flat 24 Mbps request delivered 17.1 Mbps (testsrc2) and 18.4 Mbps (real)
///   in this harness, against 2.2 Mbps on 59-10's synthetic F5 composite. The
///   undershoot is real and is a property of THAT content (colour bars, large
///   flat regions), which is exactly the complexity-blindness the mode change
///   removes: one constant cannot be right for both.
/// * **The per-frame floor is flat, which is the property the proxy table
///   turned on.** Every quality arm's worst frame is within ~0.2 MAD of its
///   own mean; the encoder is not paying for one frame out of another's budget.
///
/// **What would move it:** a measured boundary MAD approaching the 12.0 bound
/// (raise it), or a measured `MAX_RENDER_CACHE_BYTES` eviction thrash traced to
/// segment size (lower it, and re-take the MAD). 59-14 re-takes the byte-budget
/// arithmetic against these figures.
pub const RENDER_CACHE_ENCODE_QUALITY: u32 = 55;

/// The encoders the render cache PREFERS over [`DEFAULT_VIDEO_ENCODER`], in
/// descending order, probed at runtime by [`render_cache_preferred_encoder`].
/// The first one this ffmpeg build actually offers wins; if none does, the
/// shipped default is used and behaviour is byte-identical to before this list
/// existed.
///
/// # Why `h264_nvenc`, and why it is ON POLICY rather than an exception
///
/// **Licensing (CLAUDE.md rule 6) — verified against the shipped binary's OWN
/// configuration string, not against documentation.**
/// `runtime/binaries/ffmpeg.exe -version` reports `--enable-version3`,
/// `--enable-cuda-llvm` and the nv-codec-headers enable flag; it explicitly
/// DISABLES both GPL software H.264/HEVC encoders; and it carries **no
/// `--enable-gpl` and no `--enable-nonfree`**. NVENC reaches the driver through
/// nv-codec-headers, which ffmpeg loads DYNAMICALLY — the LGPL-compatible
/// path, which is why enabling it does not pull in `--enable-nonfree`.
/// CLAUDE.md's own stack rule names NVENC explicitly ("H.264/HEVC via hardware
/// / Windows Media Foundation encoders (NVENC/QSV/VCE)"), so this is the policy
/// being followed, not bent.
///
/// Three configure flags are described above rather than SPELLED, and that is
/// the licence gate working rather than a stylistic choice: this region is
/// scanned as SOURCE TEXT by `tests/render_cache_encode.rs` for GPL encoder
/// names and for codec-selector shapes, and three of the flag spellings trip
/// that scan (two name a forbidden encoder outright; the nv-codec-headers flag
/// contains a codec-selector substring). The verbatim strings live in this
/// task's SUMMARY, where they are evidence rather than code. Re-verify with
/// `ffmpeg -hide_banner -version` if the bundled binary is ever refetched.
///
/// **Why it is worth a preference at all:** the background render worker
/// competes with LIVE PLAYBACK for CPU, and 59-14 measured that competition at
/// +17.98 ms of live-path stall against only 3.70 ms of margin. Measured with
/// `ffmpeg -benchmark` on identical rawvideo input, minus the 0.016 s
/// no-encode floor: `h264_mf` **3.515 s** user CPU / **809 MB** peak RSS versus
/// `h264_nvenc` **1.750 s** / **397 MB** — **2.0x less CPU and 2.0x less
/// memory**. Wall time is EQUAL (the pipe read dominates), so no wall-time win
/// is claimed anywhere in this file.
///
/// # What is deliberately ABSENT
///
/// * **`h264_qsv` (Intel) and `h264_amf` (AMD) — a recorded follow-up, not an
///   oversight.** Both exist in this build, but `REQUIREMENTS.md § Known
///   constraint` records cross-vendor behaviour as UNVALIDATED and the machine
///   this was measured on is NVIDIA-only. A ladder entry nobody can run is a
///   claim nobody can check.
/// * **Export and proxy never consult this list.** Phase 59 D-29 freezes
///   export's instrument ([`VideoEncoder::new`]) and the proxy path is a
///   separately-verified shipped gate ([`spawn_proxy_encode`]); both keep
///   resolving [`DEFAULT_VIDEO_ENCODER`] exactly as before. This is the RENDER
///   CACHE's encoder and nothing else's.
/// * **Any env door onto this list.** The names are a const, checked by
///   `rendercache/tests/encoder_license.rs` against the cleared families. The
///   only env door to an encoder name remains the loud
///   [`DEV_ENCODER_OVERRIDE_ENV`], which outranks this list.
pub const RENDER_CACHE_PREFERRED_ENCODERS: &[&str] = &["h264_nvenc"];

/// The constant-QP target for render-cache segment encodes on the NVENC path,
/// driving `-rc constqp -qp N`.
///
/// **Note the scale is INVERTED relative to [`RENDER_CACHE_ENCODE_QUALITY`]:**
/// a HIGHER QP means coarser quantisation and FEWER bytes. The two constants
/// are not comparable numbers and must never be swapped for one another.
///
/// # MEASURED — 2026-08-04, `render_cache_nvenc_qp_calibration`
///
/// 61 frames (one `SEG_US` + D-9's overshoot) at 1920x1080 @ 30 fps per arm,
/// pushed through the segment encoder's own command line, decoded back with the
/// bundled LGPL binaries, and scored as mean-absolute-difference against the
/// pushed source frames on the project's standing metric (the
/// `EXPORT_MATCH_MAD = 12.0` ruler, carried from
/// `crates/ffi/tests/export_parity.rs`). Two sources: the `testsrc2`
/// high-entropy shape 59-01's A1 spike used, and REAL camera footage
/// (`ttt_cut180s.mov`, 1620x1080, centred un-scaled on the canvas; MAD taken
/// over the content window). The `q55` row is the SHIPPED MEDIA FOUNDATION
/// FALLBACK measured in the same run — the splice partner, so the comparison
/// column is measured here rather than quoted from 59-11.
///
/// <!-- RENDER-CACHE-NVENC-CAL (2026-08-04) — regenerate with the ignored test -->
/// | arm | encoder | source | MB / media-s | Mbps | keyframes | MAD | worst-frame MAD |
/// |------|------|-----------|-------|-------|-------|--------|--------|
/// | **qp31** | nvenc | testsrc2 | **1.510** | 12.66 | 61/61 | **0.7329** | 0.7449 |
/// | qp27 | nvenc | testsrc2 | 1.785 | 14.98 | 61/61 | 0.6879 | 0.6978 |
/// | qp23 | nvenc | testsrc2 | 2.195 | 18.42 | 61/61 | 0.6397 | 0.6468 |
/// | qp19 | nvenc | testsrc2 | 2.539 | 21.30 | 61/61 | 0.6215 | 0.6287 |
/// | _q55_ | _mf_ | _testsrc2_ | _1.484_ | _12.45_ | _61/61_ | _0.9193_ | _1.0296_ |
/// | **qp31** | nvenc | **real** | **1.738** | 14.58 | 61/61 | **1.4263** | 1.4493 |
/// | qp27 | nvenc | real | 2.275 | 19.08 | 61/61 | 1.2921 | 1.3147 |
/// | qp23 | nvenc | real | **3.362** | 28.20 | 61/61 | 1.1857 | 1.2063 |
/// | qp19 | nvenc | real | **4.566** | 38.30 | 61/61 | 1.1274 | 1.1422 |
/// | _q55_ | _mf_ | _real_ | _1.394_ | _11.69_ | _61/61_ | _1.7494_ | _1.7862_ |
///
/// **Selection rule, stated before the run and applied mechanically:** the
/// HIGHEST qp in {31, 27, 23, 19} whose mean MAD is <= 2.0 on synthetic AND
/// <= 6.0 on real footage (the same 1/6 and 1/2 fractions of the shared 12.0
/// bound 59-11 used — the bound itself was NOT moved) AND whose bytes are
/// <= **3.15 MB per media-second**, the anchor
/// `rendercache::cache::MAX_RENDER_CACHE_BYTES` was sized against. Every arm
/// must also census 61/61 keyframes or it is disqualified outright, whatever
/// its MAD.
///
/// **qp31 is the highest arm clearing every bar, so 31 is the value.** It clears
/// the MAD bars by 2.7x (synthetic) and 4.2x (real), and the byte bar by 1.8x.
///
/// # The measurement CHANGED the answer, which is why it was taken
///
/// The exploratory spike suggested 23. **The ladder disqualifies 23** — on real
/// footage it spends **3.362 MB per media-second, over the 3.15 anchor**, and
/// qp19 is worse again at 4.566. Shipping the spike's number would have cut how
/// much program time the fixed 8 GiB budget holds, silently, in exchange for
/// 0.24 MAD nobody can see. The spike's own quality comparison was
/// non-discriminating anyway (PSNR flat at 29.267 across QP 19 -> 27 while bytes
/// moved 42 %: the 4:2:0 chroma floor swamped it), which is precisely why this
/// ladder scores MAD on real decoded frames instead.
///
/// # What this table does NOT claim
///
/// * **No CPU claim is made here.** That is this constant's whole reason to
///   exist and it is measured elsewhere (`h264_mf` 3.515 s user CPU / 809 MB
///   peak RSS vs `h264_nvenc` 1.750 s / 397 MB — 2.0x less of each at EQUAL
///   wall time). MAD and bytes are what this ladder measures.
/// * **qp31 does score a LOWER MAD than the MF fallback on both sources**
///   (0.7329 vs 0.9193 synthetic; 1.4263 vs 1.7494 real) while spending 2 %
///   more bytes on synthetic and 25 % more on real. That is what the numbers
///   say; it is not the argument for the change, and the two encoders'
///   quantisers are not the same instrument, so it is recorded rather than
///   promoted.
/// * **All-intra survives constant-QP mode**, proven by decoding: 61/61
///   keyframes on all ten arms, both encoders.
///
/// **What would move it:** a measured boundary MAD approaching the 12.0 bound
/// (lower the QP), or a measured `MAX_RENDER_CACHE_BYTES` eviction thrash traced
/// to segment size (raise it, and re-take the MAD). The boundary MAD under this
/// constant was re-taken when it landed and is recorded in the task summary.
pub const RENDER_CACHE_NVENC_QP: u32 = 31;

/// MEASUREMENT-ONLY door: set to `1`/`true` to append `-hw_encoding true` to
/// the segment encode, asking Media Foundation for a GPU MFT instead of the
/// Windows software `H264 Encoder MFT` it selects by default.
///
/// **Never set in a shipped build.** This is an instrument, not a feature: plan
/// 59-14 runs the arm matrix and decides whether the shipped default should
/// move. Same loud-discipline family as [`DEV_ENCODER_OVERRIDE_ENV`] but with
/// **no licence implication whatsoever** — THIS DOOR never changes the encoder
/// NAME, so the CLAUDE.md rule-6 chokepoint ([`resolve_cleared_encoder`]) is
/// untouched and this door cannot reach a GPL encoder even in principle.
///
/// Note since the preference ladder landed: the shipped render-cache encoder
/// name is now RESOLVED at runtime ([`render_cache_preferred_encoder`]), so it
/// is not always `h264_mf`. This door is MF-private and therefore applies only
/// when MF is what resolved; on the NVENC path it is structurally unreachable
/// (pinned by `the_measurement_doors_never_leak_onto_an_nvenc_command_line`).
/// The ladder is a const list of cleared names, not an env door, so the licence
/// statement above is unchanged.
///
/// A segment encoded under this door records the fact in its meta's
/// rate-control policy string (`rendercache::cache::effective_rate_control`),
/// so a door-encoded payload structurally cannot be served as a shipped-policy
/// one.
pub const RENDER_CACHE_HW_ENCODING_ENV: &str = "RUDIS_RENDER_CACHE_HW_ENCODING";

/// MEASUREMENT-ONLY door: set to one of `h264_mf`'s `-scenario` enum values
/// (`archive`, `live_streaming`, `camera_record`, `display_remoting`, …) to
/// append `-scenario {v}` to the segment encode.
///
/// Same discipline and same non-implication as [`RENDER_CACHE_HW_ENCODING_ENV`]:
/// an instrument for 59-14's matrix, never set in a shipped build, recorded in
/// the segment's policy string so it cannot masquerade.
///
/// The value is VALIDATED before it reaches the argument vector — see
/// [`render_cache_scenario`]. An env var that shapes a child's argv is a trust
/// boundary, and "it is dev-only" is not a reason to hand it an unfiltered
/// string.
pub const RENDER_CACHE_SCENARIO_ENV: &str = "RUDIS_RENDER_CACHE_SCENARIO";

/// Is the [`RENDER_CACHE_HW_ENCODING_ENV`] measurement door open?
///
/// Read per call rather than memoized, deliberately: the tests that pin every
/// branch of the policy string set and clear this variable mid-process, and a
/// `OnceLock` would make the first test to run decide the answer for all of
/// them. The cost class (one `getenv` per segment encode, and one per meta
/// build) is already recorded as 59-REVIEW IN-01 for the sibling
/// `effective_encoder`.
pub fn render_cache_hw_encoding_enabled() -> bool {
    matches!(
        std::env::var(RENDER_CACHE_HW_ENCODING_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true"
    )
}

/// The [`RENDER_CACHE_SCENARIO_ENV`] measurement door's value, if it is set to
/// something that may safely become an argv element.
///
/// **Validated, not passed through.** Accepted: 1..=32 ASCII characters, each
/// `[A-Za-z0-9_]` — the shape of every `-scenario` enum name in ffmpeg's
/// `mfenc`. Anything else (empty, over-long, or carrying `-`, `:`, `=`, a
/// path separator, whitespace…) answers `None` and the flag is simply not
/// emitted. This is what stops the door from being a general-purpose argv
/// injector: a value can never begin with `-`, so ffmpeg can never read it as a
/// second option, and it lands in exactly one slot — immediately after
/// `-scenario` — which is not a slot any codec is selected from. The codec
/// still arrives from [`resolve_cleared_encoder`] and nowhere else.
pub fn render_cache_scenario() -> Option<String> {
    let raw = std::env::var(RENDER_CACHE_SCENARIO_ENV).ok()?;
    let v = raw.trim();
    if v.is_empty() || v.len() > 32 {
        return None;
    }
    if !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        eprintln!(
            "!!! RUDIS render-cache {RENDER_CACHE_SCENARIO_ENV} measurement door IGNORED: \
             {v:?} is not a bare [A-Za-z0-9_] scenario name !!!"
        );
        return None;
    }
    Some(v.to_string())
}

/// The render cache's preferred encoder on THIS machine, or `None` if none of
/// [`RENDER_CACHE_PREFERRED_ENCODERS`] is available and the shipped
/// [`DEFAULT_VIDEO_ENCODER`] should be used instead.
///
/// This is the ONE ladder answer. `RenderCacheEncoder::new` passes it into
/// [`resolve_cleared_encoder`] (the writer) and
/// `rendercache::cache::effective_encoder` delegates to it (the reader), so the
/// two structurally cannot disagree — and they MUST not, because
/// `rendercache::generate`'s `finish` stamps the encoder that ACTUALLY RAN into
/// the meta and `write_meta` refuses a commit whose meta the reader would not
/// accept. A second, hand-copied probe here would not mislabel segments; it
/// would break every commit.
///
/// # Memoized, and UNLIKE the env doors that is correct here
///
/// [`render_cache_hw_encoding_enabled`] and [`render_cache_scenario`] refuse a
/// `OnceLock` on purpose: they read ENV, the branch tests toggle env
/// mid-process, and the first test to run would decide the answer for all of
/// them. **This value is machine truth, not env truth** — which encoders this
/// ffmpeg build offers on this GPU does not change while the process lives. The
/// DEV override remains a per-call env read, and it sits ABOVE this in the
/// ladder inside [`resolve_cleared_encoder`], so nothing memoized can outrank
/// it.
///
/// The memoization is also load-bearing for cost: `encoder_available` SPAWNS
/// `ffmpeg -encoders`, and `effective_encoder` is on the per-tick read path
/// (cost class recorded at 59-REVIEW IN-01). ANY error — `locate()` failing,
/// the probe failing — memoizes `None`, so the read path can never crash and
/// never spawns more than once per process.
pub fn render_cache_preferred_encoder() -> Option<&'static str> {
    static PREFERRED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *PREFERRED.get_or_init(|| {
        let bins = locate().ok()?;
        RENDER_CACHE_PREFERRED_ENCODERS
            .iter()
            .copied()
            .find(|name| encoder_available(&bins, name).unwrap_or(false))
    })
}

/// Resolve the encoder for a render-cache segment through the ONE cleared
/// licensing chokepoint (CLAUDE.md rule 6, Phase 58 D-03, Phase 59 D-05):
///
/// 1. [`DEV_ENCODER_OVERRIDE_ENV`], if set and non-empty — used **with a LOUD
///    stderr warning**, never silently, and it outranks everything below. That
///    override is the ONLY door to a GPL/patent-encumbered encoder, it is
///    dev-only, and it announces itself. An unavailable override is a hard
///    typed error, exactly as before.
/// 2. `preferred`, if the caller supplied one AND [`encoder_available`] finds
///    it in this build. A probe MISS here is **not an error** — it is the
///    normal answer on a machine without the hardware, and resolution falls
///    through silently to (3). A probe ERROR still propagates: a broken ffmpeg
///    is a real failure, not a preference question.
/// 3. [`DEFAULT_VIDEO_ENCODER`] — the already-cleared, already-shipped,
///    license-safe encoder (`h264_mf` on Windows), with the same availability
///    probe and the same byte-identical hard error as before.
///
/// So a machine with no NVIDIA GPU behaves EXACTLY as it did before the
/// preference existed, and that is pinned by
/// `render_cache_encode_arg_tests::an_unavailable_preferred_encoder_really_falls_back`,
/// which sends a name no build has through the real probe.
///
/// **`preferred` names must come from a cleared const list**
/// ([`RENDER_CACHE_PREFERRED_ENCODERS`]) — never from an env var and never from
/// a caller-computed string. There is deliberately NO new env door onto this
/// parameter; the licence gate in `rendercache/tests/encoder_license.rs` pins
/// every entry of that list to a cleared family.
///
/// # Why this exists as a function while its two elders are inlined
///
/// This is the THIRD site in this file that performs exactly this resolution
/// ([`VideoEncoder::new`] is the first, [`spawn_proxy_encode`] the second). Both
/// elders inline it, and both are deliberately left **byte-unchanged** by Phase
/// 59 — export's instrument is frozen (D-29) and the proxy path is a shipped,
/// separately-verified gate. So rather than hand-copy the discipline a third
/// time, the third instance is written once, here, next to its only caller.
///
/// **A FOURTH call site must call this function rather than copy it again**, and
/// a fourth hand-written copy anywhere in the tree is a review flag. The
/// preference ladder went INSIDE this function for that reason: a parallel
/// "which encoder do we prefer" resolution outside the chokepoint would be that
/// fourth copy, wearing a different name.
fn resolve_cleared_encoder(
    bins: &FfmpegBinaries,
    purpose: &str,
    preferred: Option<&str>,
) -> Result<String, EngineError> {
    let encoder_name = match std::env::var(DEV_ENCODER_OVERRIDE_ENV) {
        Ok(v) if !v.trim().is_empty() => {
            eprintln!(
                "!!! RUDIS DEV-ONLY ENCODER OVERRIDE ACTIVE: using '{v}' instead of \
                 {DEFAULT_VIDEO_ENCODER} for {purpose} (set via \
                 {DEV_ENCODER_OVERRIDE_ENV}). This is NEVER valid in a shipped build — \
                 GPL/patent-encumbered encoders must not ship. !!!"
            );
            v
        }
        _ => {
            // The preference rung. A miss falls through; only a broken probe
            // propagates.
            match preferred {
                Some(p) if encoder_available(bins, p)? => return Ok(p.to_string()),
                _ => DEFAULT_VIDEO_ENCODER.to_string(),
            }
        }
    };

    if !encoder_available(bins, &encoder_name)? {
        return Err(EngineError::SidecarFailed {
            tool: format!("ffmpeg ({purpose} encoder probe)"),
            status: -1,
            stderr: format!(
                "encoder '{encoder_name}' not available in this ffmpeg build; {purpose} \
                 requires a hardware/license-safe H.264 encoder (see \
                 {DEV_ENCODER_OVERRIDE_ENV} for a DEV-ONLY fallback)"
            ),
        });
    }
    Ok(encoder_name)
}

/// Build the argument vector for one render-cache segment encode.
///
/// The input half mirrors [`VideoEncoder::new`] (a raw RGBA frame stream on
/// stdin); the output half mirrors [`spawn_proxy_encode`]'s all-intra vector,
/// which 58-01 proved on real output. Each flag is load-bearing:
///
/// * `-f rawvideo -pix_fmt rgba -s {w}x{h}` — `rawvideo` carries no framing of
///   its own, so the geometry IS the framing: every `push_frame` must be exactly
///   `w * h * 4` bytes or every later frame is mis-strided. That is why
///   [`RenderCacheEncoder::push_frame`] length-checks rather than trusting.
/// * `-r {fps}` **before `-i -`, and nowhere else.** This is the D-07 identity
///   half of the command line. VERIFIED against [`VideoEncoder::new`]
///   (`ffmpeg.rs`: `.args(["-r", …]).args(["-i", "-"])`), which does the same:
///   the flag declares the rate of the INPUT stream. Repeating `-r` on the
///   OUTPUT side would insert a frame-rate conversion — a drop/dup term — and
///   program time -> segment time would stop being an identity. N frames pushed
///   at F fps therefore come back as N frames at F fps, which is exactly what
///   `rendercache::key::segment_frame_program_us` assumes.
/// * `-an` — **D-08: a cache segment is VIDEO-ONLY.** Audio always comes from
///   the live mix path, so a second audio source (and its drift risk) never
///   exists. This is one of the three reasons the test-only file-to-file
///   re-encoder earlier in this file could not be extended for this job: it
///   hardcodes an audio codec with no way to turn audio off (58 D-32).
/// * `-g` — **D-05's all-intra core, and the reason this cache is worth
///   having.** Every frame is an I-frame, so entering a cached range costs one
///   frame's decode instead of a GOP's worth of reconstruction. 59-01's A1 spike
///   measured a software `StreamingDecodeSession` over exactly this payload at
///   209.8 fps against 30 fps demand. **The VALUE is per encoder family and
///   getting it wrong is silent:** `1` everywhere except NVENC, which spells
///   all-intra `0`. `h264_nvenc -g 1` cannot open at all — NVENC enforces GOP
///   length > B-frames + 1, so with `-bf 0` the minimum legal GOP is 2 and the
///   spawn dies with `InitializeEncoder failed: invalid param (8)`. `-g 0` was
///   verified all-intra by keyframe census twice (60/60 at 2 s, 150/150 at 5 s)
///   while `-g 2` gave 30/60, i.e. NOT all-intra with no error anywhere. A
///   wrong `-g` here would produce a cache that still works and still seeks
///   badly, which is why the real-output census in
///   `tests/render_cache_encode.rs` gates it on the encoder that actually
///   resolved rather than trusting this vector.
/// * `-bf 0` — no B-frames. All-intra with B-frames would be incoherent, and
///   reorder latency buys nothing on a payload that is decoded strictly
///   forwards. Present in the vector 58-01 verified; kept here for the same
///   reason.
/// * `-pix_fmt yuv420p` (output) — the universally decodable 8-bit format; a
///   segment only some decoders can open is not a cache.
/// * `-movflags +faststart` — moov atom first. 59-01 measured **~180 ms of cold
///   session entry** per segment and named it the binding constraint on `SEG_US`
///   — a payload whose header is at the end makes that worse for nothing.
///
/// # Rate control is per-encoder-family (59-11; `59-CACHE-CALIBRATION` § 2)
///
/// This function used to push `-b:v {bitrate_bps}` unconditionally and its doc
/// called that "the quality floor". **It was not one, and the measurement said
/// so:** 59-10 asked for 24 Mbps and the Media Foundation encoder delivered
/// **2.2 Mbps** on the same content — 10.9x under the request — because a flat
/// bitrate is the wrong control for an ALL-INTRA stream. Every frame costs a
/// full I-frame, so a long-GOP-shaped constant starves dense content and
/// over-spends on easy content, and neither direction is the number anyone
/// asked for. The proxy path found the same defect first and fixed it the same
/// way; this is that fix, ported.
///
/// * On the SHIPPED Media Foundation path (`h264_mf`/`hevc_mf`) the encode is
///   QUALITY-TARGETED: `-rate_control quality -quality`
///   [`RENDER_CACHE_ENCODE_QUALITY`], and `bitrate_bps` is NOT emitted at all.
///   That constant is the shipped quality floor CACHE-02 needs — a segment may
///   not look softer than the live composite it is spliced against — and it is
///   calibrated on a measured MAD ladder, not chosen.
/// * On the NVENC path (`h264_nvenc`/`hevc_nvenc`, reached when
///   [`render_cache_preferred_encoder`] finds one) the encode is CONSTANT-QP:
///   `-rc constqp -qp` [`RENDER_CACHE_NVENC_QP`], with no `-b:v` and none of
///   the MF-private options — `-rate_control`/`-quality`/`-hw_encoding`/
///   `-scenario` are `mfenc` options and an unknown private option is a hard
///   spawn failure, in both directions. **What this branch buys is CPU, and
///   only CPU:** measured with `ffmpeg -benchmark` on identical rawvideo input,
///   minus the 0.016 s no-encode floor, `h264_mf` costs **3.515 s** user CPU
///   and **809 MB** peak RSS against `h264_nvenc`'s **1.750 s** and **397 MB**
///   — 2.0x less of each — at EQUAL wall time, because the raw pipe read
///   dominates. That CPU is what the background render worker was taking from
///   live playback (59-14: +17.98 ms of live-path stall against 3.70 ms of
///   margin).
/// * On any OTHER encoder (reachable only through the loud DEV-only
///   [`DEV_ENCODER_OVERRIDE_ENV`] door) the MF-private options would be a hard
///   spawn failure, so `bitrate_bps` is emitted as `-b:v` exactly as before.
///   That branch is also the one platform requirement behind the old bullet:
///   `h264_videotoolbox` refuses to open without an explicit bitrate (see
///   [`VideoEncoder::new`]).
/// * Two MEASUREMENT DOORS ride the MF branch and NOTHING else:
///   [`RENDER_CACHE_HW_ENCODING_ENV`] appends `-hw_encoding true` and
///   [`RENDER_CACHE_SCENARIO_ENV`] appends `-scenario {v}`. Both are instruments
///   for 59-14's arm matrix, never set in a shipped build, and both are stamped
///   into the segment's meta policy string so a door-encoded payload can never
///   be served as a shipped-policy one.
///
/// Deliberately ABSENT: any `-t`/`-ss`/`-vsync`/`-fps_mode` term (they would all
/// break the identity above), and **any caller-chosen codec parameter**. There
/// is no `codec: &str` on any public function here: the encoder arrives from
/// [`resolve_cleared_encoder`] and nowhere else.
///
/// Also deliberately absent: `-pix_fmt d3d11`. The shipped `h264_mf` DOES
/// accept a `d3d11` input pixel format — that is the zero-copy GPU encode path
/// (v7 GPU-07), where a composited frame would never leave VRAM. It is noted
/// here so a later phase can find it, and it is **out of scope**: this builder
/// feeds a rawvideo RGBA pipe from system memory.
fn render_cache_encode_args(
    dst: &Path,
    w: u32,
    h: u32,
    fps: f64,
    bitrate_bps: u64,
    encoder: &str,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    let mut args: Vec<OsString> = Vec::with_capacity(34);
    let push = |args: &mut Vec<OsString>, s: String| args.push(OsString::from(s));

    for flag in ["-v", "error", "-y"] {
        push(&mut args, flag.to_string());
    }
    // INPUT half — every one of these must precede `-i`.
    push(&mut args, "-f".to_string());
    push(&mut args, "rawvideo".to_string());
    push(&mut args, "-pix_fmt".to_string());
    push(&mut args, "rgba".to_string());
    push(&mut args, "-s".to_string());
    push(&mut args, format!("{w}x{h}"));
    push(&mut args, "-r".to_string());
    push(&mut args, format!("{fps}"));
    push(&mut args, "-i".to_string());
    push(&mut args, "-".to_string());
    // OUTPUT half.
    push(&mut args, "-an".to_string());
    push(&mut args, "-c:v".to_string());
    push(&mut args, encoder.to_string());
    push(&mut args, "-g".to_string());
    // ALL-INTRA, spelled per family. NVENC's minimum legal GOP is
    // B-frames + 2, so `-g 1` cannot open there at all; `0` is its all-intra
    // spelling (censused 60/60 and 150/150). Everything else takes `1`.
    push(
        &mut args,
        if is_nvenc_video_encoder(encoder) {
            "0".to_string()
        } else {
            "1".to_string()
        },
    );
    push(&mut args, "-bf".to_string());
    push(&mut args, "0".to_string());
    if is_mf_video_encoder(encoder) {
        // SHIPPED path: quality-targeted, content-adaptive. NO `-b:v` — that
        // flat constant is precisely what delivered 2.2 Mbps against a 24 Mbps
        // request on an all-intra stream.
        push(&mut args, "-rate_control".to_string());
        push(&mut args, "quality".to_string());
        push(&mut args, "-quality".to_string());
        push(&mut args, RENDER_CACHE_ENCODE_QUALITY.to_string());
        // MEASUREMENT DOORS (59-14's arm matrix). Never open in a shipped
        // build; both are stamped into the segment's meta policy string.
        if render_cache_hw_encoding_enabled() {
            push(&mut args, "-hw_encoding".to_string());
            push(&mut args, "true".to_string());
        }
        if let Some(scenario) = render_cache_scenario() {
            push(&mut args, "-scenario".to_string());
            push(&mut args, scenario);
        }
    } else if is_nvenc_video_encoder(encoder) {
        // PREFERRED path where the hardware exists: NVENC's own constant-QP
        // rate control. NO `-b:v` (same all-intra reasoning as the MF branch)
        // and NO measurement doors — those are MF-private and would hard-fail
        // the spawn here, which is why the branch order is a discriminator
        // rather than a fall-through.
        push(&mut args, "-rc".to_string());
        push(&mut args, "constqp".to_string());
        push(&mut args, "-qp".to_string());
        push(&mut args, RENDER_CACHE_NVENC_QP.to_string());
    } else {
        // DEV-override door only: MF-private options would hard-fail the spawn,
        // and `h264_videotoolbox` cannot open without an explicit bitrate.
        push(&mut args, "-b:v".to_string());
        push(&mut args, bitrate_bps.to_string());
    }
    push(&mut args, "-pix_fmt".to_string());
    push(&mut args, "yuv420p".to_string());
    push(&mut args, "-movflags".to_string());
    push(&mut args, "+faststart".to_string());
    args.push(dst.as_os_str().to_os_string());
    args
}

/// A live, frame-push, video-only, ALL-INTRA segment encoder: composited RGBA
/// frames are pushed on stdin, an mp4 cache segment comes out (Phase 59 D-05 /
/// D-06 / D-07 / D-08 / D-23).
///
/// # Why this is a new sibling and not an extension of anything
///
/// Three encode shapes already exist in this file and none of them fits
/// (59-RESEARCH § RQ7):
///
/// * [`spawn_proxy_encode`] is **file-to-file**. A cache segment has no input
///   file at all — it is N composited layers, decoded and blended on the fly.
/// * [`VideoEncoder`] is **export's frozen instrument**, with export's GOP and
///   quality policy, muxing audio, and doubling as the export-parity ruler.
///   Phase 59 D-29 requires export stay byte-unchanged.
/// * The test-only file-to-file re-encoder earlier in this file is the door
///   Phase 58 D-32 refused to widen: test-only, hardcoded audio codec, no GOP
///   parameter, and an open caller-chosen codec parameter.
///
/// So this is genuinely new — and deliberately DUMB. It knows a destination, a
/// geometry, a cadence and a bitrate. Everything smart — temp names, the
/// meta-last atomic commit that makes a killed segment un-trustable, the cancel
/// registry, encode admission shared with the proxy worker — lives one layer up
/// in `rendercache::generate` (59-04) and `app-core` (59-08).
///
/// # Lifecycle
///
/// ```ignore
/// let mut enc = RenderCacheEncoder::new(&dst, 1920, 1080, 30.0, 24_000_000)?;
/// for frame in composited_frames { enc.push_frame(&frame)?; }
/// let bytes = enc.finish()?;
/// ```
///
/// [`kill`](Self::kill) cancels for real (D-23): the child is terminated and
/// reaped, not asked politely. Dropping without [`finish`](Self::finish) does
/// the same, because a partial segment is worthless.
///
/// # stderr is drained CONCURRENTLY, and here it is not merely prudent
///
/// [`spawn_proxy_encode`] pumps stderr so a caller polling `try_wait` cannot
/// wedge on a full pipe (measured: 126 260 bytes of error-level output from one
/// corrupt source, against a 64 KiB pipe). On THIS path the hazard is strictly
/// worse: the caller is simultaneously *writing* megabytes to the child's stdin.
/// A child blocked writing stderr stops reading stdin, our `write_all` blocks
/// forever, and the render worker deadlocks with no poll loop to notice. The
/// pump ([`spawn_stderr_pump`]) is therefore load-bearing, not diagnostic.
///
/// # No PROVENANCE entry
///
/// Choosing parameters for our own already-cleared encoder is not a borrow from
/// anyone (Phase 58 D-01's reasoning). No third-party source is derived here.
pub struct RenderCacheEncoder {
    child: std::process::Child,
    /// `None` once stdin has been closed by `finish`/`kill` — which is also what
    /// makes a later `push_frame` a clean typed error instead of a panic.
    stdin: Option<std::process::ChildStdin>,
    /// What the stderr pump has kept, capped at [`PROXY_STDERR_CAPTURE_BYTES`].
    stderr: Arc<Mutex<String>>,
    pump: Option<std::thread::JoinHandle<()>>,
    dst: PathBuf,
    width: u32,
    height: u32,
    /// `width * height * 4` — the exact length every pushed frame must have.
    frame_bytes: usize,
    encoder_name: String,
    frames_pushed: u64,
    /// Set once the child has been waited on, so `Drop` does not try again.
    reaped: bool,
}

impl RenderCacheEncoder {
    /// Spawn the segment encoder sidecar and return it ready for frames.
    ///
    /// `w`/`h` are the FULL project canvas (D-06 — a dynamic-resolution-degraded
    /// composite may never be written to a cache file, so this encoder is never
    /// handed one), `fps` is the project cadence (D-07 — the same cadence comes
    /// back out).
    ///
    /// `bitrate_bps` (`rendercache::SEGMENT_BITRATE_BPS`) is **the DEV-override
    /// door's floor only**, since 59-11. On either shipped path the floor is a
    /// quality term — [`RENDER_CACHE_ENCODE_QUALITY`] on Media Foundation,
    /// [`RENDER_CACHE_NVENC_QP`] on NVENC — and this value never reaches the
    /// command line; see [`render_cache_encode_args`]. The
    /// parameter is kept (rather than dropped) because the non-MF branch still
    /// needs it and because `h264_videotoolbox` genuinely cannot open without
    /// an explicit bitrate; it is still validated non-zero below for the same
    /// reason.
    ///
    /// **There is no encoder parameter, and that omission is the point.** See
    /// [`resolve_cleared_encoder`].
    ///
    /// Geometry and cadence are validated HERE rather than left to ffmpeg: odd
    /// dimensions cannot be encoded as `yuv420p` and a non-finite fps produces a
    /// nonsense command line, and in both cases a typed error at the call site
    /// is worth more to the caller than a sidecar diagnostic two layers away.
    pub fn new(
        dst: &Path,
        w: u32,
        h: u32,
        fps: f64,
        bitrate_bps: u64,
    ) -> Result<Self, EngineError> {
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
            return Err(EngineError::SidecarFailed {
                tool: "render-cache segment encoder".to_string(),
                status: -1,
                stderr: format!(
                    "invalid segment geometry {w}x{h}: both dimensions must be non-zero \
                     and even (yuv420p is subsampled 2x2)"
                ),
            });
        }
        if !fps.is_finite() || fps <= 0.0 {
            return Err(EngineError::SidecarFailed {
                tool: "render-cache segment encoder".to_string(),
                status: -1,
                stderr: format!("invalid segment fps {fps}: must be finite and positive"),
            });
        }
        if bitrate_bps == 0 {
            return Err(EngineError::SidecarFailed {
                tool: "render-cache segment encoder".to_string(),
                status: -1,
                stderr: "invalid segment bitrate 0".to_string(),
            });
        }

        let bins = locate()?;
        let encoder_name = resolve_cleared_encoder(
            &bins,
            "render-cache segment generation",
            render_cache_preferred_encoder(),
        )?;

        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let mut cmd = ffmpeg_command(&bins.ffmpeg);
        cmd.args(render_cache_encode_args(
            dst,
            w,
            h,
            fps,
            bitrate_bps,
            &encoder_name,
        ));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // D-23: playback outranks background cache generation. NOTE:
            // `creation_flags` REPLACES the flag word rather than OR-ing into
            // it — `ffmpeg_command` already set CREATE_NO_WINDOW, so the
            // combined mask must be passed here or the console window returns.
            cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        // Before ANYTHING else touches the child: hand its stderr to a pump.
        // See the type's own doc for why this is load-bearing on a push path.
        let (stderr, _seen, pump) = spawn_stderr_pump(&mut child);
        let stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(p) = pump {
                    let _ = p.join();
                }
                return Err(EngineError::SidecarFailed {
                    tool: "render-cache segment encoder".to_string(),
                    status: -1,
                    stderr: "failed to open stdin pipe".to_string(),
                });
            }
        };

        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr,
            pump,
            dst: dst.to_path_buf(),
            width: w,
            height: h,
            frame_bytes: w as usize * h as usize * 4,
            encoder_name,
            frames_pushed: 0,
            reaped: false,
        })
    }

    /// The encoder name actually in use (post dev-override resolution) — the
    /// same accessor [`VideoEncoder::encoder_name`] offers, and what
    /// `rendercache`'s segment meta records so a segment encoded by a different
    /// encoder is never read back as fresh.
    pub fn encoder_name(&self) -> &str {
        &self.encoder_name
    }

    /// The canvas this encoder was opened at.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// How many frames have been accepted so far. The caller uses this to check
    /// its own D-07 identity (frames pushed == frames the segment must hold).
    pub fn frames_pushed(&self) -> u64 {
        self.frames_pushed
    }

    /// Push one tightly-packed RGBA frame (`w * h * 4` bytes) in presentation
    /// order.
    ///
    /// The length check is not defensive tidiness: `-f rawvideo` has no framing,
    /// so ONE short write silently mis-strides every frame after it and the
    /// segment would decode to garbage that still probes as a valid video.
    ///
    /// A dead child surfaces here as a broken pipe, and is reported as a typed
    /// [`EngineError::SidecarFailed`] carrying whatever the encoder said — the
    /// caller's answer to that is to abandon the segment and let the range play
    /// live (D-21), which it cannot do if it never finds out.
    pub fn push_frame(&mut self, rgba: &[u8]) -> Result<(), EngineError> {
        if rgba.len() != self.frame_bytes {
            return Err(EngineError::BadOutputSize {
                got: rgba.len(),
                expected: self.frame_bytes,
            });
        }
        let stderr_now = self.stderr_snapshot();
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| EngineError::SidecarFailed {
                tool: "render-cache segment encoder".to_string(),
                status: -1,
                stderr: "push_frame called after finish()/kill()".to_string(),
            })?;
        match stdin.write_all(rgba) {
            Ok(()) => {
                self.frames_pushed += 1;
                Ok(())
            }
            Err(e) => Err(EngineError::SidecarFailed {
                tool: "render-cache segment encoder".to_string(),
                status: -1,
                stderr: format!(
                    "writing frame {} to the encoder failed ({e}); the sidecar said: {}",
                    self.frames_pushed,
                    stderr_now.trim()
                ),
            }),
        }
    }

    /// Non-blocking poll of the sidecar. `Ok(None)` while it is still running.
    ///
    /// Safe to call in a loop because the pump owns the stderr pipe and drains
    /// it continuously (see [`ProxyEncodeChild::try_wait`], same reasoning). The
    /// render worker uses this to notice a child that died mid-segment; the kill
    /// gate uses it to prove the child was really reaped.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Close stdin (EOF), wait for the sidecar to finalize the mp4, and return
    /// the byte length of the finished file.
    ///
    /// The wait is BOUNDED ([`RENDER_CACHE_FINISH_TIMEOUT`]) because this is
    /// called from the render worker's own thread: a sidecar that never exits
    /// must cost one failed segment, not the worker. On timeout the child is
    /// killed and reaped and the error says so.
    ///
    /// Returning the length rather than `()` is deliberate: 59-04's commit
    /// protocol writes the payload first and the meta LAST, and the meta records
    /// `payload_bytes` so a truncated payload can never be read back as fresh.
    /// Handing the caller the number this function already had to stat removes
    /// its only reason to re-derive it.
    pub fn finish(mut self) -> Result<u64, EngineError> {
        // Drop stdin to send EOF, THEN wait — ffmpeg will not finalize the mp4
        // (moov atom, faststart rewrite) until it sees EOF on the video pipe.
        drop(self.stdin.take());

        let deadline = std::time::Instant::now() + RENDER_CACHE_FINISH_TIMEOUT;
        let status = loop {
            match self.child.try_wait()? {
                Some(status) => break status,
                None => {
                    if std::time::Instant::now() >= deadline {
                        self.child.kill().ok();
                        let _ = self.child.wait();
                        self.reaped = true;
                        let said = self.join_pump();
                        return Err(EngineError::SidecarFailed {
                            tool: "render-cache segment encoder".to_string(),
                            status: -1,
                            stderr: format!(
                                "the encoder did not exit within {:?} of stdin EOF after \
                                 {} frames; killed. It said: {}",
                                RENDER_CACHE_FINISH_TIMEOUT,
                                self.frames_pushed,
                                said.trim()
                            ),
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };
        self.reaped = true;
        let said = self.join_pump();

        if !status.success() {
            return Err(EngineError::SidecarFailed {
                tool: "render-cache segment encoder".to_string(),
                status: status.code().unwrap_or(-1),
                stderr: said.trim().to_string(),
            });
        }

        let len = std::fs::metadata(&self.dst)?.len();
        if len == 0 {
            return Err(EngineError::SidecarFailed {
                tool: "render-cache segment encoder".to_string(),
                status: 0,
                stderr: format!(
                    "the encoder exited cleanly but wrote an empty segment at {} \
                     ({} frames pushed)",
                    self.dst.display(),
                    self.frames_pushed
                ),
            });
        }
        Ok(len)
    }

    /// Cancel the encode for real (D-23): KILL the child, then REAP it.
    ///
    /// Best-effort by construction — a child that already exited makes `kill()`
    /// fail, and that is fine. The order matters: the kill comes BEFORE stdin is
    /// dropped, because dropping stdin first would signal EOF and race the
    /// sidecar into finalizing a *complete-looking* short segment, which is the
    /// one outcome a cancel must never produce. The `wait()` after the kill is
    /// the non-optional half: without it the process stays a zombie and its
    /// handle on the output file is not guaranteed released.
    ///
    /// What this does NOT do, by design: delete or hide the partial file.
    /// Un-trustability is 59-04's meta-last commit protocol (a payload with no
    /// `.seg.json` beside it is never a hit) — named here so the gap is owned
    /// rather than assumed away (threat T-59-03-02).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        drop(self.stdin.take());
        let _ = self.child.wait();
        self.reaped = true;
        let _ = self.join_pump();
    }

    /// Everything the pump has kept from the sidecar's stderr, capped at
    /// [`PROXY_STDERR_CAPTURE_BYTES`]. Diagnostics only.
    pub fn stderr_snapshot(&self) -> String {
        self.stderr
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// Join the drain thread (once) and hand back what it captured.
    fn join_pump(&mut self) -> String {
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        self.stderr_snapshot()
    }
}

/// A `RenderCacheEncoder` dropped without [`finish`](RenderCacheEncoder::finish)
/// — an early `?` in the render loop, a cancelled job, a panic — must not leave
/// a sidecar running, and must not let one finalize a short segment behind the
/// caller's back. So drop is a kill, for the same reason
/// [`kill`](RenderCacheEncoder::kill) is: a partial segment is worthless, and a
/// *plausible-looking* partial segment is worse than worthless.
impl Drop for RenderCacheEncoder {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            drop(self.stdin.take());
            let _ = self.child.wait();
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

// --- RENDER-CACHE-ENCODER REGION END ---------------------------------------

// ---------------------------------------------------------------------------
// Export frame-sink encoder (Phase 7)
// ---------------------------------------------------------------------------

use std::io::Write;
use std::sync::{Arc, Mutex};

/// The hardware/license-safe H.264 encoder name to pass as `-c:v` on THIS OS.
///
/// Per-OS seam (Phase 8 formalizes this; was previously a single constant
/// that incorrectly returned the macOS value on every platform):
///
/// - **macOS (dev target):** `h264_videotoolbox` (Apple hardware encoder,
///   license-safe — system framework, not GPL/patent-encumbered source).
/// - **Windows (shipped target):** `h264_mf` (Media Foundation encoder — and
///   note that the MFT ffmpeg actually SELECTS is Windows' own **software**
///   `H264 Encoder MFT`: `hw_encoding` defaults to false in ffmpeg's `mfenc`,
///   verified against the shipped `runtime/binaries/ffmpeg.exe`. Earlier
///   revisions of this comment called it "the hardware encoder"; it is not one
///   by default, which is exactly why its behaviour is identical on every
///   Windows 8+ machine regardless of GPU). VERIFIED on Windows (2026-07-05,
///   Phase 10): the export gate
///   (`crates/engine/tests/export_encode.rs`, no dev override) produces a real
///   H.264 file tagged `Lavc h264_mf` + AAC with a monotonic 0→100 progress
///   stream on this machine's FFmpeg build. NVENC/QSV/VCE alternates remain a
///   possible future optimization but are not required for a valid export.
/// - **Linux/other:** no license-safe hardware encoder is assumed available
///   by default, so this is intentionally left unset — callers must supply
///   [`DEV_ENCODER_OVERRIDE_ENV`] to encode on this platform. Rudis is not
///   shipped for Linux; this branch exists only so the crate compiles
///   everywhere. NEVER defaults to `libx264`/`libx265`/`openh264` (GPL /
///   patent-encumbered) on any platform.
///
/// The [`DEV_ENCODER_OVERRIDE_ENV`] loud override remains available on every
/// OS for local dev fallback (e.g. when the license-safe encoder is unavailable
/// in a CI/VM environment) and is never valid in a shipped build.
#[cfg(target_os = "macos")]
pub const DEFAULT_VIDEO_ENCODER: &str = "h264_videotoolbox";

#[cfg(target_os = "windows")]
pub const DEFAULT_VIDEO_ENCODER: &str = "h264_mf"; // Media Foundation (Windows' SOFTWARE H264 Encoder MFT by default) — VERIFIED on Windows (Phase 10, real export tagged Lavc h264_mf)

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const DEFAULT_VIDEO_ENCODER: &str = ""; // no license-safe default; use RUDIS_DEV_ENCODER

/// Env var that permits a DEV-ONLY fallback encoder (e.g. `libx264`, GPL —
/// never shipped) when the cleared license-safe encoder
/// ([`DEFAULT_VIDEO_ENCODER`]) is unavailable in a dev environment. Absent by
/// default; setting it logs LOUDLY (never silent).
pub const DEV_ENCODER_OVERRIDE_ENV: &str = "RUDIS_DEV_ENCODER";

/// The Media Foundation quality target (0–100) for EXPORT encodes, driving
/// `-rate_control quality -quality N` on the `*_mf` encoders — the export
/// twin of [`PROXY_ENCODE_QUALITY`], calibrated SEPARATELY because an export
/// is the user's deliverable at full canvas resolution, not a disposable
/// 960 px playback intermediate.
///
/// **MEASURED 2026-08-04** (debug session `proxy-bitrate-starved-all-intra`,
/// round 3), on a real worst-case composite: 20 s at canvas 1920x1080\@30 —
/// a fullscreen dense aerial Shibuya-crossing layer plus two hard-edged
/// 480x270 PiP overlays — scored frame-index-aligned against the lossless
/// ffv1 master it was encoded from (long-GOP defaults, exactly like a real
/// export):
///
/// | setting              | Mbps | SSIM-All | PSNR-Y | worst-frame PSNR-Y |
/// |----------------------|------|----------|--------|--------------------|
/// | flat `-b:v` 4.35M (old) | 4.7 | 0.847  | 27.49  | 21.23 — shredded   |
/// | quality 45           | 11.8 | 0.946    | 34.44  | 33.99 |
/// | quality 55           | 16.5 | 0.961    | 36.53  | 36.07 |
/// | quality 65           | 22.9 | 0.973    | 38.74  | 38.28 |
/// | **quality 75**       | 31.5 | 0.981    | 40.93  | 40.49 |
/// | quality 85           | 42.6 | 0.987    | 43.18  | 42.76 |
///
/// The old flat bitrate (`W*H*fps*0.07`, ~4.35 Mbps at 1080p30 regardless of
/// content) swung 22.9→40.4 dB per frame across the same file — adequate on
/// simple single-layer shots, catastrophic exactly where the timeline stacked
/// layers of dense footage: crowds and storefronts shredded into coloured
/// high-frequency noise while flat regions stayed clean, which is the defect
/// the owner reported from a real exported file. Quality mode holds a
/// ~2.4 dB per-frame band and self-scales with content and fps.
///
/// 75 was chosen from real decoded frames: at native size AND at 2x zoom (a
/// 1080p deliverable viewed fullscreen on a 4K display) the worst frame is
/// visually indistinguishable from the lossless master (worst-frame
/// 40.49 dB / SSIM 0.979 — the classic visually-clean delivery bar), while
/// quality 85 costs ~35% more bytes for a difference invisible at 2x. The
/// 31.5 Mbps spend is WORST-CASE-only: quality mode adapts ~3x downward on
/// typical content (round-1 measurement), landing typical 1080p30 exports in
/// the 8–12 Mbps range — comparable to professional NLE delivery presets.
/// Unlike the proxy cache there is no byte budget here; the export is a
/// one-shot deliverable. Encode wall time was flat across the whole sweep
/// (~4.5 s per 20 s clip), so the quality raise costs no export time.
///
/// Verified on the shipped Windows software `H264 Encoder MFT`: two encodes
/// of the same input are BYTE-IDENTICAL (the ffi export-parity and
/// proxy-isolation gates compare exports byte-for-byte), and the long-GOP
/// structure is preserved under quality mode (keyframe census: 20/600 — the
/// MFT's default ~1 s GOP; a deliverable must NEVER inherit the proxy's
/// `-g 1` all-intra shape).
pub const EXPORT_ENCODE_QUALITY: u32 = 75;

/// Bits-per-pixel-per-frame, in thousandths, for the NON-MF export bitrate
/// fallback: `200` = 0.2 bpp.
///
/// The fallback exists because `h264_videotoolbox` REJECTS opening without an
/// explicit bitrate ("Error setting bitrate property" — verified empirically,
/// ffmpeg 8.0.1 / macOS: `-c:v h264_videotoolbox` alone fails to open the
/// encoder regardless of resolution; `-b:v` fixes it), and MF-private quality
/// options would hard-fail any non-MF spawn. The SHIPPED Windows `h264_mf`
/// path never uses this value — see [`export_rate_control_args`].
///
/// 0.2 bpp errs deliberately high (12.4 Mbps at 1080p30, ~100 Mbps at 4K60)
/// because a flat ABR cannot adapt to content: the round-3 matrix measured
/// the quality-75 target spending ~0.5 bpp on worst-case stacked-overlay
/// content, so a lean flat constant WILL starve complex sections — that was
/// the shipped defect (0.07 bpp, described in its own comment as "~0.1",
/// starving stacked-overlay sections to 21 dB). If this constant changes,
/// keep this comment's numbers in agreement with it.
pub const EXPORT_FALLBACK_BPP_MILLI: u64 = 200;

/// The constant-quantizer target for EXPORT encodes on the NVENC path, driving
/// `-rc constqp -qp N`. The export twin of [`EXPORT_ENCODE_QUALITY`], and
/// **explicitly NOT** [`RENDER_CACHE_NVENC_QP`].
///
/// **Note the scale is INVERTED relative to [`EXPORT_ENCODE_QUALITY`]:** a
/// HIGHER QP means coarser quantisation and FEWER bytes. The two constants are
/// not comparable numbers and must never be swapped for one another.
///
/// # MEASURED — 2026-08-09, plan 60-01
/// (`.planning/phases/60-.../artifacts/60-HWENC-CALIBRATION.md`)
///
/// 20 s of 1920x1080\@30 worst-case composite (a dense `mandelbrot` bed plus
/// two hard-edged 480x270 PiP overlays — the round-3 shape
/// [`EXPORT_ENCODE_QUALITY`] itself was calibrated on), scored
/// frame-index-aligned against the lossless ffv1 master it was encoded from.
/// **Row 1 is the bar: the shipped MF export args.**
///
/// <!-- EXPORT-NVENC-CAL (2026-08-09) -->
/// | args | Mbps | SSIM-All | PSNR-Y | worst-frame PSNR-Y |
/// |---|---|---|---|---|
/// | **REF** `h264_mf -rate_control quality -quality 75` | 26.29 | 0.9927 | 44.28 | **43.22** |
/// | `-rc constqp -qp 18` | 23.15 | 0.9960 | 47.98 | 45.08 |
/// | `-rc constqp -qp 19` | 21.77 | 0.9957 | 47.29 | 44.45 |
/// | **`-rc constqp -qp 20`** | **19.90** | **0.9952** | **46.38** | **43.54** |
/// | `-rc constqp -qp 21` | 17.76 | 0.9945 | 45.27 | 42.08 — FAILS |
/// | `-rc constqp -qp 24` | 13.49 | 0.9923 | 42.68 | 39.09 |
/// | `-rc constqp -qp 31` (the CACHE's value) | 5.01 | 0.9792 | 35.79 | **30.83** |
///
/// **Selection rule, stated before the run and applied mechanically:** the
/// HIGHEST QP (fewest bytes) whose SSIM-All AND worst-frame PSNR-Y both meet
/// the `h264_mf @75` reference on the same master. `qp 21` fails worst-frame
/// by 1.14 dB; `qp 20` clears by 0.32 dB. A SECOND master of different
/// difficulty reproduced the same boundary independently (`qp 20` +1.35 dB,
/// `qp 21` −0.36 dB), which is why a 0.32 dB margin was accepted rather than
/// rounded up. If future content puts `qp 20` under the bar, **`qp 19` is the
/// pre-identified conservative neighbour** (+1.23 dB, 9 % more bytes).
///
/// At `qp 20` export spends **24 % fewer bytes than `h264_mf @75`** for
/// equal-or-better measured quality, and encodes ~3.6x faster. The saving is
/// real and structural, not a scoring artifact: MF emits **Constrained
/// Baseline with no B-frames** — the most compatible and least efficient H.264
/// configuration there is — while NVENC emits **High with B-frames**.
///
/// # `-rc vbr -cq` was the research's recommendation and the measurement killed it
///
/// 60-RESEARCH assumption A1 proposed `-rc vbr -cq N` as "NVENC's analogue of
/// MF's quality-targeted mode". Measured, it **SATURATES**: `cq 19`, `15`,
/// `11` and even `cq 1` — the strongest quality request the knob can express —
/// all land ~16.6 Mbps at ~38.4 dB worst-frame, **4.8 dB under the bar**, and
/// `-preset p7` moves it 0.05 dB. Seven further levers were measured before
/// concluding: `-maxrate 80M -bufsize 160M` recovers most of it (42.58 dB,
/// still 0.64 dB short); `-qmin 1 -qmax 20` clears the bar but spends
/// 25.65 Mbps (essentially MF's own 26.29) across three interacting knobs,
/// i.e. it reaches the bar by becoming approximately constant-QP, at
/// constant-QP's price. `-tune hq` and `-multipass fullres` do nothing;
/// `-spatial-aq` makes it worse; `-bf 0` recovers 1.84 dB, locating the defect
/// in B-frame bit redistribution. vbr's MEAN PSNR-Y (44.40) already beats
/// MF's (44.28) — only its worst frame collapses, a 5.97 dB per-frame band
/// against MF's 1.06 and `qp 20`'s 2.84. Mean-good/worst-bad is the same
/// defect class round 3 was convened to fix.
///
/// **What would move this constant:** a measured worst-frame PSNR-Y below the
/// `h264_mf @75` bar on real content (drop to `qp 19`), or an FFmpeg/driver
/// release that lifts the VBR ceiling — whose reopening condition is written
/// down in 60-01 § Decision (re-run the saturation probe and check whether
/// `-rc vbr -cq 1` clears 43.22 dB worst-frame on a worst-case master).
pub const EXPORT_NVENC_QP: u32 = 20;

/// Target keyframe interval, in SECONDS, for EXPORT encodes on the NVENC path.
///
/// # The decision, and why it is a decision at all (60-02)
///
/// 60-01 deliberately did not settle this and handed it forward: NVENC's
/// default GOP is ~250 frames, so switching export's encoder would have moved
/// keyframe spacing from the MF path's measured **~1.0 s** (19 keyframes in
/// 600) to **~6.7 s** (3 in 600) as a silent side effect. Keyframe spacing is
/// the seek granularity a user's OTHER tools get on the delivered file, so
/// that is a user-visible property change nobody asked for.
///
/// **Rule applied: an encoder swap changes the encoder, not the deliverable's
/// other observable properties.** `1.0 s` is the value that holds today's
/// behaviour still, and it is expressed in SECONDS (multiplied by fps at build
/// time) so the interval does not silently change with frame rate the way a
/// raw frame count would.
///
/// # MEASURED — 2026-08-09, plan 60-02, same master and same fixed harness as
/// the ladder above (guard: master-vs-master 0 non-inf of 600 ✅; the unpinned
/// row reproduced the published table byte-for-byte at 49 752 332 bytes)
///
/// | `-g` | keyframes / 600 | interval | Mbps | SSIM-All | PSNR-Y | worst-frame |
/// |---|---|---|---|---|---|---|
/// | unset (NVENC default) | 3 | ~6.7 s | 19.90 | 0.9952 | 46.38 | 43.54 |
/// | `-g 60` (2.0 s) | 10 | 2.0 s | 20.52 | 0.9953 | 46.47 | 43.57 |
/// | **`-g 30` (1.0 s)** | **20** | **1.0 s** | **21.50** | **0.9955** | **46.61** | **43.56** |
/// | _`h264_mf @75` (today)_ | _19_ | _~1.05 s_ | _26.29_ | _0.9927_ | _44.28_ | _43.22_ |
///
/// **The price is published rather than hidden: +8.0 % bytes against the
/// unpinned default.** The saving versus the encoder being replaced narrows
/// from 24 % to **18 %**, which is the cost of not moving a property the user
/// never asked to move. Quality does not pay: SSIM-All and worst-frame
/// PSNR-Y both come out marginally BETTER pinned than unpinned, and the
/// selected rung still clears the bar (+0.34 dB worst-frame). Both runs of the
/// pinned vector were byte-identical, so `export_parity.rs`'s Claim B is
/// unaffected by the pin.
///
/// 20 keyframes against MF's 19 is the closest match available on an integer
/// frame count, and it is emphatically still long-GOP — a deliverable must
/// NEVER inherit the proxy/cache's all-intra shape (round-3 rule, and
/// [`RENDER_CACHE_NVENC_QP`]'s `-g 0` is the opposite decision for the
/// opposite reason).
pub const EXPORT_NVENC_GOP_SECONDS: f64 = 1.0;

/// Floor on the emitted `-g`, in frames. NVENC refuses to open when GOP length
/// is not greater than B-frames + 1 (`InitializeEncoder failed: invalid param
/// (8)` — the same wall [`render_cache_encode_args`] hit from the other
/// direction with `-g 1`), and export's NVENC arm keeps B-frames on because
/// they are where its 24 % byte saving comes from. 12 frames is comfortably
/// clear of any B-frame count NVENC's presets choose, and only a degenerate
/// sub-12 fps export can reach it.
const EXPORT_NVENC_GOP_MIN_FRAMES: u32 = 12;

/// [`EXPORT_NVENC_GOP_SECONDS`] converted to the frame count `-g` takes.
fn export_nvenc_gop_frames(fps: f64) -> u32 {
    let frames = (fps.max(1.0) * EXPORT_NVENC_GOP_SECONDS).round();
    let frames = if frames.is_finite() && frames > 0.0 {
        frames as u32
    } else {
        EXPORT_NVENC_GOP_MIN_FRAMES
    };
    frames.max(EXPORT_NVENC_GOP_MIN_FRAMES)
}

/// Rate-control arguments for one EXPORT encode, per encoder family — the
/// export twin of the split inside [`proxy_encode_args`] (same debug session,
/// round 3; same discipline: family-private options are only ever emitted to
/// the family that owns them, via the same closed `matches!` lists).
///
/// * SHIPPED Media Foundation path (`h264_mf`/`hevc_mf`): QUALITY-TARGETED —
///   `-rate_control quality -quality` [`EXPORT_ENCODE_QUALITY`], and NO
///   `-b:v` at all (the MFT ignores `-b:v` under quality mode anyway; round-1
///   measurement). A flat bitrate is complexity-blind: it starves
///   stacked-overlay/dense sections while over-spending on easy ones.
/// * **NVENC (`h264_nvenc`/`hevc_nvenc`, HWENC-01, 60-02):** CONSTANT-QP —
///   `-rc constqp -qp` [`EXPORT_NVENC_QP`] `-preset p4 -g` (fps x
///   [`EXPORT_NVENC_GOP_SECONDS`]), and no `-b:v`, which `constqp` does not
///   read. **`-preset p4` is pinned explicitly even though it is today's
///   FFmpeg default**, because a default that silently moved would silently
///   move every user's export quality and every byte of `export_parity.rs`'s
///   Claim B.
/// * Any OTHER encoder (macOS dev `h264_videotoolbox`, or the loud DEV-only
///   [`DEV_ENCODER_OVERRIDE_ENV`] door): an explicit `-b:v` derived from pixel
///   rate — [`EXPORT_FALLBACK_BPP_MILLI`] x W x H x fps, floored at 500 kbps
///   so tiny/test frame sizes still produce a valid bitstream —
///   `h264_videotoolbox` genuinely cannot open without one.
///
/// # The branch is the control, and 60-02 measured why
///
/// This file has said in several places that a wrong-family option is "a HARD
/// SPAWN FAILURE… `-rate_control quality` handed to `h264_nvenc` kills it just
/// as dead". **Measured against the shipped `N-125907` build on 2026-08-09,
/// that is not what happens**: `-rate_control` and `-quality` are private
/// options of OTHER encoders, so ffmpeg's option parser ACCEPTS them, exits 0,
/// and silently ignores them — the NVENC output is byte-identical to the same
/// command without them, i.e. encoded at NVENC's uncalibrated default rather
/// than at the calibrated operating point. (A genuinely unknown option really
/// does kill the spawn, exit 8; so does an unknown encoder NAME.) The
/// conclusion strengthens the discipline rather than relaxing it: a
/// wrong-family flag here fails SILENTLY, at the wrong quality, in a
/// deliverable — so the dispatch must stay a closed-enum branch with no shared
/// code path, and the arg vectors stay unit-pinned.
fn export_rate_control_args(
    encoder: &str,
    width: u32,
    height: u32,
    fps: f64,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    if is_media_foundation_video_encoder(encoder) {
        vec![
            OsString::from("-rate_control"),
            OsString::from("quality"),
            OsString::from("-quality"),
            OsString::from(EXPORT_ENCODE_QUALITY.to_string()),
        ]
    } else if is_nvenc_video_encoder(encoder) {
        vec![
            OsString::from("-rc"),
            OsString::from("constqp"),
            OsString::from("-qp"),
            OsString::from(EXPORT_NVENC_QP.to_string()),
            OsString::from("-preset"),
            OsString::from("p4"),
            OsString::from("-g"),
            OsString::from(export_nvenc_gop_frames(fps).to_string()),
        ]
    } else {
        let bitrate_bps = (((EXPORT_FALLBACK_BPP_MILLI as f64) / 1000.0)
            * (width as u64 * height as u64) as f64
            * fps.max(1.0))
        .max(500_000.0) as u64;
        vec![OsString::from("-b:v"), OsString::from(bitrate_bps.to_string())]
    }
}

#[cfg(test)]
mod export_rate_control_arg_tests {
    //! Debug `proxy-bitrate-starved-all-intra` round 3 — pure arg pins for the
    //! EXPORT rate-control split, mirroring `proxy_encode_arg_tests`. The REAL
    //! proof (a decoded exported file scored against ground truth) is the ffi
    //! `export_parity` gate; these pin the *contract* so a future edit cannot
    //! quietly reintroduce the flat complexity-blind `-b:v` that shredded
    //! stacked-overlay sections of exported files.

    fn strings(encoder: &str, w: u32, h: u32, fps: f64) -> Vec<String> {
        super::export_rate_control_args(encoder, w, h, fps)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The shipped MF path is quality-targeted and NEVER carries a flat
    /// bitrate — that constant WAS the export-starvation bug.
    #[test]
    fn mf_export_is_quality_targeted_and_never_passes_a_flat_bitrate() {
        for mf in ["h264_mf", "hevc_mf"] {
            let args = strings(mf, 1920, 1080, 30.0);
            assert_eq!(
                args,
                vec![
                    "-rate_control".to_string(),
                    "quality".to_string(),
                    "-quality".to_string(),
                    super::EXPORT_ENCODE_QUALITY.to_string(),
                ],
                "{mf}: {args:?}"
            );
            assert!(
                !args.iter().any(|a| a == "-b:v"),
                "a flat bitrate must never reach the MF export encode again: {args:?}"
            );
        }
    }

    /// A non-MF encoder must NOT receive MF-private options (hard spawn
    /// failure) and gets the documented pixel-rate `-b:v` instead —
    /// `h264_videotoolbox` cannot open without one.
    #[test]
    fn non_mf_export_falls_back_to_the_documented_pixel_rate_bitrate() {
        let args = strings("h264_videotoolbox", 1920, 1080, 30.0);
        assert!(
            !args.iter().any(|a| a == "-rate_control" || a == "-quality"),
            "MF-private options would hard-fail a non-MF encoder: {args:?}"
        );
        // 0.2 bpp x 1920 x 1080 x 30 = 12,441,600 — the constant and this
        // number must agree (the shipped bug's comment said 0.1 while the
        // code computed 0.07).
        assert_eq!(args, vec!["-b:v".to_string(), "12441600".to_string()], "{args:?}");
    }

    /// The 500 kbps floor still guards tiny/test frame sizes.
    #[test]
    fn non_mf_fallback_floors_tiny_frames_at_a_valid_bitrate() {
        let args = strings("h264_videotoolbox", 16, 16, 5.0);
        assert_eq!(args, vec!["-b:v".to_string(), "500000".to_string()], "{args:?}");
    }

    // -----------------------------------------------------------------------
    // 60-02 (HWENC-01) — the NVENC arm, calibrated by 60-01
    // -----------------------------------------------------------------------

    /// **The whole 60-01 § Decision vector, verbatim.** Every term is a
    /// measured choice and the test names why, so a future edit that drops one
    /// has to argue with a number.
    #[test]
    fn nvenc_export_is_constqp_at_the_calibrated_operating_point() {
        for nv in ["h264_nvenc", "hevc_nvenc"] {
            assert!(
                super::is_nvenc_video_encoder(nv),
                "{nv} must be recognised by the nvenc discriminator"
            );
            let args = strings(nv, 1920, 1080, 30.0);
            assert_eq!(
                args,
                vec![
                    "-rc".to_string(),
                    "constqp".to_string(),
                    "-qp".to_string(),
                    super::EXPORT_NVENC_QP.to_string(),
                    "-preset".to_string(),
                    "p4".to_string(),
                    "-g".to_string(),
                    "30".to_string(),
                ],
                "{nv}: {args:?}"
            );
        }
    }

    /// `-b:v` is a vbr-only concept `constqp` does not read, and a flat
    /// bitrate is the complexity-blind control round 3 removed from export.
    /// The MF-private options are the subtler hazard: 60-02 MEASURED that
    /// `-rate_control quality -quality 75` handed to `h264_nvenc` does NOT
    /// kill the spawn on this build — ffmpeg accepts the options (they are
    /// private options of OTHER encoders), silently ignores them, and encodes
    /// at NVENC's uncalibrated default. Wrong-family flags fail SILENTLY here,
    /// which is exactly why the dispatch must be a closed-enum branch and not
    /// a shared code path.
    #[test]
    fn nvenc_export_never_carries_a_bitrate_or_an_mf_private_option() {
        for nv in ["h264_nvenc", "hevc_nvenc"] {
            let args = strings(nv, 1920, 1080, 30.0);
            for banned in ["-b:v", "-rate_control", "-quality", "-cq"] {
                assert!(
                    !args.iter().any(|a| a == banned),
                    "{nv} must never receive {banned}: {args:?}"
                );
            }
        }
    }

    /// **The frozen MF path.** Byte-identical to before 60-02 — no `-g`, no
    /// new flag, nothing. `export_parity.rs`'s Claim B and the whole
    /// `EXPORT_ENCODE_QUALITY = 75` calibration rest on this vector, and the
    /// NVENC arm was inserted BETWEEN the MF branch and the flat-bitrate
    /// fallthrough precisely so neither elder branch had to move.
    #[test]
    fn the_nvenc_arm_left_the_mf_and_videotoolbox_branches_untouched() {
        assert_eq!(
            strings("h264_mf", 1920, 1080, 30.0),
            vec!["-rate_control", "quality", "-quality", "75"],
            "the shipped MF vector must be byte-identical to today's"
        );
        assert_eq!(
            strings("h264_videotoolbox", 1920, 1080, 30.0),
            vec!["-b:v", "12441600"],
            "videotoolbox still gets the documented pixel-rate bitrate"
        );
    }

    /// **The counterfactual, pinned as code.** The render cache's own doc
    /// records a DISK-BUDGET selection rule; export is a one-shot deliverable
    /// with no byte budget. 60-01 measured `qp 31` at 30.83 dB worst-frame
    /// PSNR-Y — 12.39 dB BELOW the `h264_mf @75` bar, worse than the flat-`-b:v`
    /// defect round 3 was convened to fix.
    #[test]
    fn export_does_not_reuse_the_render_cache_operating_point() {
        assert_ne!(
            super::EXPORT_NVENC_QP,
            super::RENDER_CACHE_NVENC_QP,
            "export must carry its OWN calibrated QP; RENDER_CACHE_NVENC_QP is \
             disk-budget-shaped and measures 12.39 dB under the deliverable bar"
        );
        assert_eq!(super::EXPORT_NVENC_QP, 20, "60-01 § Decision, verbatim");
    }

    /// **The GOP decision, 60-02's own (handed forward unabsorbed by 60-01).**
    /// Keyframe spacing is the seek granularity a user's OTHER tools get on the
    /// exported file, so an encoder swap must not move it. `-g` tracks fps so
    /// the interval stays ~1.0 s at any frame rate, with a floor that keeps the
    /// GOP legal (NVENC refuses to open when GOP <= B-frames + 1).
    #[test]
    fn the_nvenc_gop_tracks_fps_so_seek_granularity_does_not_move() {
        for (fps, want_g) in [
            (30.0, "30"),
            (60.0, "60"),
            (25.0, "25"),
            (24.0, "24"),
            (23.976, "24"),
            (50.0, "50"),
            // Floor: a degenerate low fps must not produce a GOP NVENC refuses.
            (5.0, "12"),
            (1.0, "12"),
            (0.0, "12"),
        ] {
            let args = strings("h264_nvenc", 1920, 1080, fps);
            let g = args.iter().position(|a| a == "-g").expect("-g must be emitted");
            assert_eq!(args[g + 1], want_g, "fps={fps}: {args:?}");
        }
    }

    /// ...and the MF path never grew one, because pinning `-g` there would
    /// change bytes on the frozen instrument for no requirement at all.
    #[test]
    fn the_gop_pin_is_nvenc_only() {
        for other in ["h264_mf", "hevc_mf", "h264_videotoolbox", "mpeg4"] {
            let args = strings(other, 1920, 1080, 30.0);
            assert!(
                !args.iter().any(|a| a == "-g"),
                "{other} must not receive the NVENC GOP pin: {args:?}"
            );
        }
    }
}

#[cfg(test)]
mod export_encoder_resolution_pins {
    //! **The fourth call site, pinned as source text.**
    //!
    //! [`super::resolve_cleared_encoder`]'s own doc says: *"A FOURTH call site
    //! must call this function rather than copy it again, and a fourth
    //! hand-written copy anywhere in the tree is a review flag."* 60-02 is that
    //! fourth call site. What makes it real is not that the call exists but
    //! that the copy it replaced is GONE — so the property is pinned by
    //! reading `VideoEncoder::new`'s own body.

    use std::path::Path;

    /// This file's own source, with line endings normalized. The tree is
    /// checked out CRLF on Windows and LF elsewhere; a pin that depended on
    /// which one would be a pin on the developer's git config.
    fn own_source() -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ffmpeg.rs"))
            .expect("read src/ffmpeg.rs")
            .replace("\r\n", "\n")
    }

    /// The setup half of `VideoEncoder::new` — from its signature to the point
    /// where the child is already spawned and configured.
    fn video_encoder_new_body() -> String {
        let source = own_source();
        let start = source
            .find("    pub fn new(\n        out_path: &Path,")
            .expect("VideoEncoder::new must exist with its documented signature");
        let end = source[start..]
            .find("let mut cmd = ffmpeg_command(")
            .expect("VideoEncoder::new must build its command");
        source[start..start + end].to_string()
    }

    #[test]
    fn export_resolves_through_the_one_cleared_chokepoint_and_keeps_no_copy() {
        let body = video_encoder_new_body();
        println!("EXPORT-RESOLUTION body_bytes={}", body.len());

        assert!(
            body.contains("resolve_cleared_encoder("),
            "VideoEncoder::new must resolve through the ONE licensing chokepoint: {body}"
        );
        assert!(
            body.contains("export_encoder_preference("),
            "…and must offer it export's OWN preference (with the kill switch applied)"
        );
        assert!(
            body.contains("\"export\""),
            "…passing purpose=\"export\" so the typed error keeps its export wording"
        );

        // The copy is GONE. This is the half that makes the call site real:
        // a second inline DEV-override READ here would be the fourth
        // hand-written copy resolve_cleared_encoder's doc names as a review
        // flag, and it would be a second source of truth about which rung
        // fired. Both spellings of the read are banned; NAMING the constant in
        // a comment that explains why the read is absent is not (a pin that
        // punished the explanation would delete the record of the decision).
        for read in [
            "std::env::var(DEV_ENCODER_OVERRIDE_ENV)",
            "std::env::var_os(DEV_ENCODER_OVERRIDE_ENV)",
        ] {
            assert!(
                !body.contains(read),
                "the inline DEV-override read ({read}) must be gone from \
                 VideoEncoder::new — the chokepoint owns that rung: {body}"
            );
        }
        assert!(
            !body.contains("encoder_available("),
            "the inline availability probe must be gone too; resolve_cleared_encoder \
             performs it: {body}"
        );
    }

    /// Export's rate control must never reach for the render cache's constants.
    /// Two separate calibrations, two separate problems — and a shared constant
    /// is how a disk budget silently becomes a deliverable's quality.
    #[test]
    fn the_export_rate_control_builder_never_names_a_render_cache_constant() {
        let source = own_source();
        let start = source
            .find("fn export_rate_control_args(")
            .expect("export_rate_control_args must exist");
        let end = source[start..]
            .find("\n}\n")
            .expect("export_rate_control_args must have a body");
        let body = &source[start..start + end];
        for shape in ["RENDER_CACHE_NVENC_QP", "RENDER_CACHE_ENCODE_QUALITY"] {
            assert!(
                !body.contains(shape),
                "export_rate_control_args names {shape} — export has its own \
                 calibration and must not inherit the cache's disk budget: {body}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// HWENC (Phase 60 plan 60-02) — EXPORT's own preferred-encoder ladder
// ---------------------------------------------------------------------------

/// The encoders EXPORT prefers over [`DEFAULT_VIDEO_ENCODER`], most-preferred
/// first — export's twin of [`RENDER_CACHE_PREFERRED_ENCODERS`], and
/// **deliberately a SEPARATE list**.
///
/// The render cache's const says outright that "Export and proxy never consult
/// this list… This is the RENDER CACHE's encoder and nothing else's", and that
/// separation is load-bearing in both directions: Phase 59 owns the cache's
/// disk-budget calibration, and sharing one list would couple a deliverable's
/// quality to a disposable cache's byte budget. The two lists happen to hold
/// the same name today. That is a coincidence of this machine's hardware, not
/// a shared decision, and neither may be repointed at the other.
///
/// # Every entry MUST satisfy the closed-enum discriminators
///
/// [`is_nvenc_video_encoder`] or [`is_mf_video_encoder`] — checked at runtime
/// by [`export_probe_first_usable`] and pinned totally by
/// `export_encoder_ladder_tests`. **Presence in `ffmpeg -encoders` is NOT a
/// licence proof:** `libopenh264` is compiled into the shipped
/// `runtime/binaries/ffmpeg.exe` (verified 2026-08-09, 60-RESEARCH Pitfall 1
/// and again by 60-01), is a registered `D`-capable encoder, and would encode
/// a perfectly valid file if anything ever handed it to a spawn. The ONLY
/// thing standing between it and a shipped export is the closed `matches!`
/// allowlist — never [`encoder_available`], which is an AVAILABILITY probe and
/// is used here as one.
///
/// # Why `h264_nvenc`, and why the list is not empty
///
/// 60-01 measured (`artifacts/60-HWENC-CALIBRATION.md`) that `h264_nvenc` is
/// **run-to-run byte-deterministic** in both of its quality-targeted
/// rate-control modes — 12 of 12 `cmp` pairs identical across three
/// independent runs, on the elementary stream AND the mp4, and identical again
/// under a concurrent NVENC job saturating the same engine. That is what
/// `export_parity.rs`'s Claim B (`assert_eq!(diff_bytes, 0)`) needs, and it was
/// the plan's biggest single unknown. Had it failed, this list would ship
/// EMPTY (`&[]`) and export would stay on `h264_mf` — the machinery below is
/// written so that outcome costs exactly one const edit.
///
/// There is deliberately **no env door onto this list**. The only door to an
/// encoder NAME remains the loud [`DEV_ENCODER_OVERRIDE_ENV`], which outranks
/// it inside [`resolve_cleared_encoder`].
///
/// **Deliberately absent:** `h264_qsv` (Intel) and `h264_amf` (AMD). Both
/// exist in this build and both are plausible, but the reference machine is
/// NVIDIA-only — a ladder entry nobody can run is a claim nobody can check.
/// Same recorded follow-up the render cache's own list carries.
pub const EXPORT_PREFERRED_ENCODERS: &[&str] = &["h264_nvenc"];

/// The export hardware-encoder kill switch. Set it to ANYTHING and export
/// resolves exactly as it did before HWENC existed: the preference rung is
/// skipped and [`DEFAULT_VIDEO_ENCODER`] (`h264_mf` on Windows) runs.
///
/// 59-24's [`crate::render_cache_job`]-side `RUDIS_DISABLE_RENDER_CACHE` idiom,
/// mirrored deliberately: a `pub const` naming the variable once, read with
/// `var_os` **per export** so a flip takes effect in the field with no rebuild,
/// and **presence — not value — is the signal**. `RUDIS_EXPORT_DISABLE_HW_ENCODER=0`
/// disables the hardware encoder, because a variable someone bothered to set is
/// a variable someone meant.
///
/// The switch only ever DISABLES. There is no value of it that turns anything
/// on, its absence must never become an opt-out of the default, and — T-60-06 —
/// **its value is never parsed and never forwarded to a command line**, so
/// there is nothing to inject through it.
///
/// # It has two jobs
///
/// 1. **The honest-fallback proof door.** 60-03 needs to exercise the MF path
///    on a machine that HAS working NVENC, and the alternative (unplugging a
///    GPU) is not a test. It is also how a user with a misbehaving driver keeps
///    exporting while the bug is diagnosed.
/// 2. **A first-class DISABLED arm for every future benchmark**, exactly as
///    `RUDIS_DISABLE_HWDECODE` and `RUDIS_DISABLE_RENDER_CACHE` already give
///    their own subsystems.
pub const EXPORT_HW_ENCODER_DISABLE_ENV: &str = "RUDIS_EXPORT_DISABLE_HW_ENCODER";

/// Frame geometry and length of the functional pre-flight encode. Small enough
/// that the probe costs a few tens of milliseconds once per process, large
/// enough to be a real H.264 frame: both dimensions even, and comfortably
/// inside the surface limits recorded on [`EXPORT_NVENC_MIN_WIDTH`] (192x108
/// clears the measured 146x50 floor by 46x58 px).
const EXPORT_PROBE_WIDTH: u32 = 192;
const EXPORT_PROBE_HEIGHT: u32 = 108;
const EXPORT_PROBE_FPS: f64 = 30.0;
const EXPORT_PROBE_FRAMES: usize = 8;

/// The canvas sizes `h264_nvenc` can actually encode — **a hard hardware
/// limit, and one `h264_mf` does not share.**
///
/// # MEASURED 2026-08-09 (60-02), and it caught a real regression
///
/// The existing Phase 7 export gate (`crates/engine/tests/export_encode.rs`)
/// exports a **64x48** canvas. Under the new ladder it resolved `h264_nvenc`,
/// which then died at encoder-open with
///
/// ```text
/// InitializeEncoder failed: invalid param (8): Frame Dimension less than the
/// minimum supported value.
/// ```
///
/// — a WHOLE-EXPORT failure on a canvas that has always worked. An export has
/// no per-segment escape hatch (unlike the render cache's D-21), so this had to
/// be answered before the ladder could ship, and it is not something the
/// functional pre-flight can catch: the probe encodes at ONE geometry, and the
/// limit is a property of the geometry the USER asked for.
///
/// Bisected against the bundled `N-125907` build, driver `610.47`, RTX 3070,
/// with the exact shipped NVENC arg vector, and cross-checked against
/// `h264_mf` on every row:
///
/// | canvas | `h264_nvenc` | `h264_mf` |
/// |---|---|---|
/// | 32x32 | FAIL | FAIL (both — below H.264 itself) |
/// | 48x48 · 64x48 · 128x96 · 144x64 · 145x49 | **FAIL** | OK |
/// | 192x48 | **FAIL** | OK |
/// | **146x64 · 192x50 · 160x64 · 192x108** | OK | OK |
/// | 1920x1080 · 3840x2160 · 4096x4096 | OK | OK |
/// | **4098x2160 · 1920x4098 · 7680x4320** | **FAIL** (`No capable devices found`) | OK |
///
/// So the usable window is **146x50 … 4096x4096 inclusive**, and it agrees with
/// NVIDIA's own documented H.264 NVENC caps (minimum "greater than 145x49",
/// maximum 4096x4096 across every NVENC generation), which is the reason these
/// numbers are trusted to generalize off this one GPU rather than being treated
/// as a local quirk.
///
/// A canvas outside the window is not an error and not a probe failure — it is
/// simply not a canvas the preference applies to, so
/// [`export_encoder_preference`] withholds the preference and export runs
/// `h264_mf` exactly as it always has.
pub const EXPORT_NVENC_MIN_WIDTH: u32 = 146;
/// See [`EXPORT_NVENC_MIN_WIDTH`] for the measured table.
pub const EXPORT_NVENC_MIN_HEIGHT: u32 = 50;
/// See [`EXPORT_NVENC_MIN_WIDTH`] for the measured table.
pub const EXPORT_NVENC_MAX_WIDTH: u32 = 4096;
/// See [`EXPORT_NVENC_MIN_WIDTH`] for the measured table.
pub const EXPORT_NVENC_MAX_HEIGHT: u32 = 4096;

/// Can `h264_nvenc` open at this canvas? See [`EXPORT_NVENC_MIN_WIDTH`].
pub fn export_nvenc_supports_canvas(width: u32, height: u32) -> bool {
    (EXPORT_NVENC_MIN_WIDTH..=EXPORT_NVENC_MAX_WIDTH).contains(&width)
        && (EXPORT_NVENC_MIN_HEIGHT..=EXPORT_NVENC_MAX_HEIGHT).contains(&height)
}

/// Which encoder EXPORT would prefer on THIS machine, or `None` if no
/// candidate survives the ladder — memoized for the life of the process.
///
/// Mirrors [`render_cache_preferred_encoder`]'s shape and its rationale for
/// memoizing: **this is machine truth, not env truth.** Which encoders this
/// ffmpeg build offers, and whether this GPU can actually open one, does not
/// change while the process lives. The env doors that CAN change mid-process —
/// [`DEV_ENCODER_OVERRIDE_ENV`] and [`EXPORT_HW_ENCODER_DISABLE_ENV`] — are
/// both read per call, ABOVE this memo, so nothing memoized can outrank them
/// (see [`export_encoder_preference`] and [`resolve_cleared_encoder`]).
///
/// Any error — a broken probe, an unreadable file — memoizes `None`, so export
/// falls back to the always-present [`DEFAULT_VIDEO_ENCODER`] rather than
/// failing. A preference that cannot be established is not an error; it is the
/// normal answer on a machine without the hardware.
pub fn export_preferred_encoder(bins: &FfmpegBinaries) -> Option<&'static str> {
    static PREFERRED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *PREFERRED.get_or_init(|| export_probe_first_usable(bins, EXPORT_PREFERRED_ENCODERS))
}

/// [`export_preferred_encoder`] with the kill switch and the canvas limits
/// applied — the value [`VideoEncoder::new`] hands to
/// [`resolve_cleared_encoder`] as `preferred`, and the ONE place that decision
/// is made (tests and 60-03 call this rather than re-deriving it).
///
/// Two things sit outside the memo, for two different reasons:
///
/// * **The kill switch** is read here because the memo may only ever hold
///   machine truth; a latched env read is how a door becomes a one-way switch
///   that a test — or a user — cannot turn back off.
/// * **The canvas limits** are checked here because they are a property of the
///   export the user asked for, not of the machine, so they cannot be memoized
///   and the functional pre-flight (which encodes at ONE fixed geometry) cannot
///   see them. See [`EXPORT_NVENC_MIN_WIDTH`] for the measured window and the
///   real regression that produced it.
pub fn export_encoder_preference(
    bins: &FfmpegBinaries,
    width: u32,
    height: u32,
) -> Option<&'static str> {
    if std::env::var_os(EXPORT_HW_ENCODER_DISABLE_ENV).is_some() {
        return None;
    }
    let candidate = export_preferred_encoder(bins)?;
    if is_nvenc_video_encoder(candidate) && !export_nvenc_supports_canvas(width, height) {
        eprintln!(
            "EXPORT-ENCODER-SKIP encoder={candidate} canvas={width}x{height} \
             reason=outside-nvenc-surface-limits (usable window is \
             {EXPORT_NVENC_MIN_WIDTH}x{EXPORT_NVENC_MIN_HEIGHT} to \
             {EXPORT_NVENC_MAX_WIDTH}x{EXPORT_NVENC_MAX_HEIGHT}); falling back to \
             {DEFAULT_VIDEO_ENCODER}"
        );
        return None;
    }
    Some(candidate)
}

/// Walk `candidates` in preference order and return the first that clears all
/// THREE rungs. Logs one `EXPORT-ENCODER-PROBE` line per candidate either way.
///
/// 1. **The licence rung** — [`is_nvenc_video_encoder`] or
///    [`is_mf_video_encoder`]. A list entry that fails this is a BUG in the
///    list, not a machine fact, so it is skipped loudly rather than probed.
/// 2. **The availability rung** — [`encoder_available`], a LISTING probe. It
///    answers "does this build know the name", which is necessary and nowhere
///    near sufficient.
/// 3. **The functional rung** — a real, tiny encode. See
///    [`export_encoder_preflight`].
///
/// Rung 3 is the one 60-RESEARCH left open (open decision #5) and it is settled
/// as a REAL probe because of 59-14: `h264_mf -hw_encoding true` was measured
/// accepting its flag, spawning cleanly, taking frame 1, and then dying with
/// `os error 109` — 0 of 6 segments, six times out of six. "It is listed" and
/// even "it spawned" are not "it works", and an export is a one-shot,
/// possibly-multi-minute deliverable with no per-segment escape hatch.
fn export_probe_first_usable(
    bins: &FfmpegBinaries,
    candidates: &[&'static str],
) -> Option<&'static str> {
    for &candidate in candidates {
        if !(is_nvenc_video_encoder(candidate) || is_mf_video_encoder(candidate)) {
            eprintln!(
                "EXPORT-ENCODER-PROBE encoder={candidate} verdict=FAIL \
                 reason=not-a-cleared-encoder-family (this is a bug in \
                 EXPORT_PREFERRED_ENCODERS, not a property of this machine)"
            );
            continue;
        }
        match encoder_available(bins, candidate) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "EXPORT-ENCODER-PROBE encoder={candidate} verdict=FAIL \
                     reason=not-listed-by-this-ffmpeg-build"
                );
                continue;
            }
            Err(e) => {
                eprintln!(
                    "EXPORT-ENCODER-PROBE encoder={candidate} verdict=FAIL \
                     reason=listing-probe-error: {e}"
                );
                continue;
            }
        }
        match export_encoder_preflight(bins, candidate) {
            Ok(detail) => {
                eprintln!(
                    "EXPORT-ENCODER-PROBE encoder={candidate} verdict=PASS reason={detail}"
                );
                return Some(candidate);
            }
            Err(why) => {
                eprintln!("EXPORT-ENCODER-PROBE encoder={candidate} verdict=FAIL reason={why}");
            }
        }
    }
    None
}

/// Encode [`EXPORT_PROBE_FRAMES`] real frames with `encoder` and the EXACT
/// rate-control arguments a real export would give it, then verify the OUTPUT
/// FILE — not the exit status alone.
///
/// The verification is HWENC-01's own proof method, applied to a throwaway
/// file before the deliverable is attempted:
///
/// * ffmpeg exited 0, **and**
/// * `ffprobe` counts [`EXPORT_PROBE_FRAMES`] video packets in the result, so
///   every pushed frame really came out the other side (59-14's failure took
///   frame 1 and died at frame 2), **and**
/// * the video stream's `encoder` TAG names `encoder`. 60-01 established that
///   the **stream**-level tag is the one that discriminates
///   (`Lavc63.7.100 h264_nvenc` vs `Lavc63.7.100 h264_mf`); the FORMAT-level
///   tag reads only `Lavf63.5.101` and would pass identically for both.
///
/// A false FAIL costs the hardware preference and falls back to `h264_mf` —
/// today's shipped behaviour — so this rung is deliberately strict.
///
/// The temp file is removed on every path, including the failing ones.
fn export_encoder_preflight(bins: &FfmpegBinaries, encoder: &str) -> Result<String, String> {
    let out = std::env::temp_dir().join(format!(
        "rudis-export-encoder-probe-{}-{}.mp4",
        std::process::id(),
        encoder
    ));
    let _ = std::fs::remove_file(&out);
    let verdict = export_encoder_preflight_inner(bins, encoder, &out);
    let _ = std::fs::remove_file(&out);
    verdict
}

fn export_encoder_preflight_inner(
    bins: &FfmpegBinaries,
    encoder: &str,
    out: &Path,
) -> Result<String, String> {
    let rate_control = export_rate_control_args(
        encoder,
        EXPORT_PROBE_WIDTH,
        EXPORT_PROBE_HEIGHT,
        EXPORT_PROBE_FPS,
    );

    let mut cmd = ffmpeg_command(&bins.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .args(["-s", &format!("{EXPORT_PROBE_WIDTH}x{EXPORT_PROBE_HEIGHT}")])
        .args(["-r", &format!("{EXPORT_PROBE_FPS}")])
        .args(["-i", "-"])
        .args(["-c:v", encoder])
        .args(&rate_control)
        // `-an`: the probe asks one question — can this VIDEO encoder produce
        // real frames — and an audio input would only add a second way to fail.
        .args(["-an", "-pix_fmt", "yuv420p"])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn-failed: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "no-stdin-pipe".to_string())?;
        // A cheap, deterministic, NON-uniform pattern: a flat frame would let a
        // broken encoder look healthy. Writes stop at the FIRST error rather
        // than blocking forever on a pipe whose reader has died — which is
        // precisely 59-14's shape (the child exits, the next write returns
        // broken-pipe) and is why the status check below is still reached.
        let mut frame = vec![0u8; (EXPORT_PROBE_WIDTH * EXPORT_PROBE_HEIGHT * 4) as usize];
        for i in 0..EXPORT_PROBE_FRAMES {
            for (p, px) in frame.chunks_exact_mut(4).enumerate() {
                let x = (p as u32) % EXPORT_PROBE_WIDTH;
                let y = (p as u32) / EXPORT_PROBE_WIDTH;
                px[0] = (x.wrapping_mul(7).wrapping_add(i as u32 * 29) & 0xff) as u8;
                px[1] = (y.wrapping_mul(11).wrapping_add(i as u32 * 13) & 0xff) as u8;
                px[2] = ((x ^ y).wrapping_add(i as u32 * 51) & 0xff) as u8;
                px[3] = 0xff;
            }
            if stdin.write_all(&frame).is_err() {
                break;
            }
        }
    } // stdin dropped here => EOF

    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait-failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        return Err(format!(
            "encode-failed status={} stderr={first:?}",
            output.status.code().unwrap_or(-1)
        ));
    }

    // Verify REAL OUTPUT.
    let probe = ffmpeg_command(&bins.ffprobe)
        .args(["-v", "error", "-count_packets"])
        .args(["-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=codec_name,nb_read_packets:stream_tags=encoder",
        ])
        .args(["-of", "default=nw=1"])
        .arg(out)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ffprobe-failed: {e}"))?;
    if !probe.status.success() {
        return Err(format!(
            "ffprobe-rejected-output status={}",
            probe.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&probe.stdout);

    let packets: usize = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("nb_read_packets="))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if packets != EXPORT_PROBE_FRAMES {
        return Err(format!(
            "wrong-packet-count got={packets} want={EXPORT_PROBE_FRAMES}"
        ));
    }

    let tag = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("TAG:encoder="))
        .unwrap_or("")
        .trim()
        .to_string();
    if !tag.split_whitespace().any(|t| t == encoder) {
        return Err(format!("encoder-tag-mismatch tag={tag:?} want={encoder}"));
    }

    Ok(format!("packets={packets} tag={tag:?}"))
}

#[cfg(test)]
mod export_encoder_ladder_tests {
    //! **HWENC-01 / HWENC-02 (Phase 60 plan 60-02) — the EXPORT preference
    //! ladder's SHAPE, its licence guard, and its kill-switch door.**
    //!
    //! The real proof that export encodes on hardware is a decoded, probed
    //! output file (60-03, and the `ffi` export-parity gate). What is pinned
    //! HERE is the property no output file can prove: that a forbidden encoder
    //! is structurally unreachable no matter what this machine's ffmpeg build
    //! happens to offer.
    //!
    //! This module may — and must — NAME the forbidden encoders in order to
    //! assert their absence, exactly as `rendercache/tests/encoder_license.rs`
    //! does for the render cache's own ladder.

    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// `cargo test` runs a binary's tests as THREADS in one process and an
    /// environment variable is process-global, so every test below that reads
    /// or writes [`super::EXPORT_HW_ENCODER_DISABLE_ENV`] takes this lock
    /// first. Same pattern as `render_cache_encode_arg_tests::door_serial`;
    /// a SEPARATE mutex because this door and the render cache's doors are
    /// disjoint variables read by disjoint code.
    static LADDER_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ladder_serial() -> std::sync::MutexGuard<'static, ()> {
        LADDER_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set (or clear) one env var for a scope and restore whatever was there
    /// before — including "nothing", which a naive set/remove pair gets wrong
    /// on a machine that really does export the variable.
    struct EnvGuard {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> EnvGuard {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            EnvGuard { key, prev }
        }
        fn cleared(key: &'static str) -> EnvGuard {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            EnvGuard { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// GPL (`libx264`/`libx265`) and patent-scoped (`openh264`) names that may
    /// never reach a shipped spawn — CLAUDE.md rule 6. `x264`/`x265` catch the
    /// bare spellings too.
    const FORBIDDEN: [&str; 6] = [
        "libx264",
        "libx265",
        "libopenh264",
        "openh264",
        "x264",
        "x265",
    ];

    /// The BUNDLED LGPL binaries, addressed DIRECTLY rather than through
    /// [`super::locate`]. Deliberate: `locate()` falls back to PATH, and the
    /// PATH ffmpeg on the reference machine is a **GPL 6.1** build that has
    /// silently falsified this project's measurements before. A licence test
    /// that could accidentally run against a GPL binary is worse than no test.
    fn bundled_bins() -> Option<super::FfmpegBinaries> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())?
            .to_path_buf();
        let dir = root.join("runtime").join("binaries");
        let ffmpeg = dir.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));
        let ffprobe = dir.join(format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
        (ffmpeg.is_file() && ffprobe.is_file())
            .then_some(super::FfmpegBinaries { ffmpeg, ffprobe })
    }

    /// Binaries that cannot possibly spawn — used where the assertion is about
    /// the LADDER's logic and any spawn at all would be the bug.
    fn nowhere_bins() -> super::FfmpegBinaries {
        super::FfmpegBinaries {
            ffmpeg: PathBuf::from("rudis-no-such-ffmpeg-60-02"),
            ffprobe: PathBuf::from("rudis-no-such-ffprobe-60-02"),
        }
    }

    /// **The HWENC-02 shape guard.** Every entry of export's own ladder must
    /// be recognised by one of the two closed `matches!` allowlists. This is
    /// total rather than a sample, because the list is a `const` with no env
    /// door onto it.
    #[test]
    fn export_ladder_every_entry_is_a_cleared_hardware_family() {
        println!(
            "EXPORT-LADDER preferred={:?} default={:?}",
            super::EXPORT_PREFERRED_ENCODERS,
            super::DEFAULT_VIDEO_ENCODER
        );
        for name in super::EXPORT_PREFERRED_ENCODERS {
            assert!(
                super::is_nvenc_video_encoder(name) || super::is_mf_video_encoder(name),
                "{name:?} is in EXPORT_PREFERRED_ENCODERS but is neither an NVENC nor a \
                 Media Foundation encoder — the only two families cleared for shipped \
                 Rudis (CLAUDE.md rule 6)"
            );
            let lowered = name.to_lowercase();
            for forbidden in FORBIDDEN {
                assert!(
                    !lowered.contains(forbidden),
                    "{name:?} names {forbidden:?}; a preference ladder is not an \
                     exemption from rule 6"
                );
            }
        }
        // Non-vacuity. Under 60-01's shipped branch (b) the ladder is NOT
        // empty; an empty list would pass every clause above for free. If a
        // future measurement ever forces branch (c) (`&[]`), this assertion is
        // the one that must be consciously deleted, with the reason recorded.
        assert!(
            !super::EXPORT_PREFERRED_ENCODERS.is_empty(),
            "60-01 measured NVENC deterministic in both rate-control modes, so the \
             ladder ships non-empty (branch (b)); an empty ladder here means someone \
             disabled HWENC without recording why"
        );
    }

    /// **Pitfall 1, pinned in export's scope.** `libopenh264` is COMPILED INTO
    /// the shipped `runtime/binaries/ffmpeg.exe` and would pass any
    /// "is it in `-encoders`" check. Build presence is not a licence proof; the
    /// closed enums are.
    #[test]
    fn export_ladder_forbidden_encoders_fail_both_discriminators() {
        for name in ["libopenh264", "libx264", "libx265", "openh264", "mpeg4"] {
            assert!(
                !super::is_nvenc_video_encoder(name),
                "{name:?} must not be recognised as NVENC"
            );
            assert!(
                !super::is_mf_video_encoder(name),
                "{name:?} must not be recognised as Media Foundation"
            );
            assert!(
                !super::EXPORT_PREFERRED_ENCODERS.contains(&name),
                "{name:?} must never appear in export's preference ladder"
            );
        }
        // ...and the discriminators are not vacuously false for everything.
        assert!(super::is_nvenc_video_encoder("h264_nvenc"));
        assert!(super::is_mf_video_encoder("h264_mf"));
    }

    /// An empty candidate list answers None and probes nothing — the branch-(c)
    /// shape stays live so a future measurement can flip one const.
    #[test]
    fn export_ladder_an_empty_candidate_list_answers_none() {
        assert_eq!(
            super::export_probe_first_usable(&nowhere_bins(), &[]),
            None,
            "an empty ladder must answer None without touching the binaries"
        );
    }

    /// The encoder [`export_ladder_refuses_a_listed_working_encoder_outside_the_closed_set`]
    /// uses as its non-vacuity witness.
    ///
    /// **`libopenh264` was the original witness, and Phase 62 removed it on
    /// purpose.** Until 2026-08-30 the shipped `runtime/binaries/ffmpeg.exe` was
    /// built `--enable-libopenh264`, and that was exactly the point: a
    /// patent-encumbered encoder that WAS listed by `-encoders`, WOULD encode a
    /// valid file, and was refused anyway. `SHIP-04` (plan 62-01) then rebuilt the
    /// sidecar `--disable-libopenh264 --disable-libkvazaar --disable-libvvenc`, so
    /// the old witness is no longer in the binary and could not keep the assertion
    /// below non-vacuous. Per this test's own standing instruction the witness was
    /// RE-VERIFIED and MOVED, never deleted — the guard itself is unchanged.
    ///
    /// `mpeg4` (MPEG-4 part 2) is the replacement because it is strictly better at
    /// the job:
    /// - it is a **native** FFmpeg encoder, compiled into every build, so no future
    ///   licensing decision can remove it the way one just removed `libopenh264`
    ///   — this witness cannot rot the same way;
    /// - it is genuinely listed by `-encoders` and genuinely encodes, so a refusal
    ///   of it cannot be explained by absence or by brokenness;
    /// - it is neither an NVENC nor a Media Foundation name, so the ONLY thing that
    ///   rejects it is the closed-enum check.
    ///
    /// It is also already named in
    /// [`export_ladder_forbidden_encoders_fail_both_discriminators`], so both tests
    /// now agree on the same forbidden name.
    const LADDER_NON_VACUITY_WITNESS: &str = "mpeg4";

    /// **The load-bearing licence test.** [`LADDER_NON_VACUITY_WITNESS`] is present
    /// in this build, IS listed by `-encoders`, and WOULD encode a valid file if
    /// the ladder ever handed it to a spawn. It is rejected by the closed-enum
    /// check and by nothing else — so this test fails the moment someone
    /// "simplifies" the ladder into an availability probe.
    ///
    /// See [`LADDER_NON_VACUITY_WITNESS`] for why the witness is no longer
    /// `libopenh264`; the second arm below keeps asserting on `libopenh264` itself,
    /// so `SHIP-04` cannot silently regress either.
    #[test]
    fn export_ladder_refuses_a_listed_working_encoder_outside_the_closed_set() {
        let Some(bins) = bundled_bins() else {
            eprintln!("SKIPPING: no bundled LGPL ffmpeg in runtime/binaries");
            return;
        };
        let witness = LADDER_NON_VACUITY_WITNESS;

        // The witness must sit OUTSIDE both cleared families, or the refusal below
        // would be testing nothing at all.
        assert!(
            !super::is_nvenc_video_encoder(witness) && !super::is_mf_video_encoder(witness),
            "{witness:?} is inside a cleared encoder family, so it cannot witness a refusal"
        );

        // Non-vacuity: the name really is available on this build, so "None" below
        // cannot be explained by absence.
        let listed = super::encoder_available(&bins, witness).unwrap_or(false);
        println!("EXPORT-LADDER-LICENCE {witness} listed_by_encoders={listed}");
        assert!(
            listed,
            "the premise of this test is that {witness:?} IS in the shipped build; if the \
             bundled binary no longer carries it, re-verify Pitfall 1 and MOVE the witness \
             (as Phase 62 did when SHIP-04 removed libopenh264) rather than deleting this \
             assertion"
        );
        assert_eq!(
            super::export_probe_first_usable(&bins, &[LADDER_NON_VACUITY_WITNESS]),
            None,
            "a LISTED, WORKING, FORBIDDEN encoder must still be refused — the guard is \
             the closed enum, never availability"
        );

        // The other half of the same guard, kept on the ORIGINAL witness: the
        // encoder SHIP-04 removed must stay removed, and must be refused whether or
        // not the build lists it. Availability was never what rejected it.
        let openh264_listed = super::encoder_available(&bins, "libopenh264").unwrap_or(false);
        println!("EXPORT-LADDER-LICENCE libopenh264 listed_by_encoders={openh264_listed}");
        assert!(
            !openh264_listed,
            "libopenh264 is listed by the shipped build again. SHIP-04 rebuilt the sidecar \
             --disable-libopenh264 and scripts/windows/fetch-lgpl-ffmpeg.ps1 hard-fails on \
             its return, so a payload carrying it did not arrive through the sanctioned path"
        );
        assert_eq!(
            super::export_probe_first_usable(&bins, &["libopenh264"]),
            None,
            "libopenh264 must be refused whether or not the build lists it — the guard is \
             the closed enum, never availability"
        );
    }

    /// **T-60-06 / the kill switch.** Presence, not value: `0` and `false`
    /// disable it just as surely as `1`, because the door is read with
    /// `var_os`. Read per call and never latched into the memo.
    #[test]
    fn export_ladder_the_disable_door_skips_the_preference_rung() {
        let _serial = ladder_serial();

        for value in ["1", "0", "false", "no", "anything at all"] {
            let _door = EnvGuard::set(super::EXPORT_HW_ENCODER_DISABLE_ENV, value);
            assert_eq!(
                super::export_encoder_preference(&nowhere_bins(), 1920, 1080),
                None,
                "with {} set to {value:?} the preference rung must be skipped entirely",
                super::EXPORT_HW_ENCODER_DISABLE_ENV
            );
        }

        // With the door shut, the preference is the memoized machine truth —
        // and it comes back, so the door is a door and not a one-way latch.
        if let Some(bins) = bundled_bins() {
            let _door = EnvGuard::cleared(super::EXPORT_HW_ENCODER_DISABLE_ENV);
            assert_eq!(
                super::export_encoder_preference(&bins, 1920, 1080),
                super::export_preferred_encoder(&bins),
                "with the door shut, preference resolution is exactly the probe's answer"
            );
        }
    }

    /// **The canvas-limit guard, which a real export gate caught.** See
    /// [`super::EXPORT_NVENC_MIN_WIDTH`] for the bisected table; every row here
    /// is a measured `h264_nvenc` verdict, and `h264_mf` encodes ALL of them.
    #[test]
    fn export_ladder_withholds_nvenc_on_a_canvas_it_cannot_encode() {
        for (w, h, supported) in [
            // Measured FAIL on h264_nvenc, OK on h264_mf.
            (64, 48, false),
            (48, 48, false),
            (128, 96, false),
            (144, 64, false),
            (145, 49, false),
            (192, 48, false),
            (4098, 2160, false),
            (1920, 4098, false),
            (7680, 4320, false),
            // Measured OK.
            (146, 50, true),
            (146, 64, true),
            (192, 50, true),
            (192, 108, true),
            (1920, 1080, true),
            (3840, 2160, true),
            (4096, 4096, true),
        ] {
            assert_eq!(
                super::export_nvenc_supports_canvas(w, h),
                supported,
                "{w}x{h}: the guard must match the measured NVENC verdict"
            );
        }

        let _serial = ladder_serial();
        let _door = EnvGuard::cleared(super::EXPORT_HW_ENCODER_DISABLE_ENV);
        if let Some(bins) = bundled_bins() {
            // The Phase 7 export gate's own canvas. Before the guard, this
            // resolved h264_nvenc and the export DIED at encoder-open.
            assert_eq!(
                super::export_encoder_preference(&bins, 64, 48),
                None,
                "a 64x48 export must fall back to the default encoder, not fail"
            );
            assert_eq!(
                super::export_encoder_preference(&bins, 7680, 4320),
                None,
                "an 8K export must fall back too — NVENC H.264 caps at 4096x4096"
            );
            // ...and the guard is not a blanket refusal.
            assert_eq!(
                super::export_encoder_preference(&bins, 1920, 1080),
                super::export_preferred_encoder(&bins),
                "a normal canvas must still get the preference"
            );
        }
    }

    /// The ladder may only ever answer with a name from its own const list —
    /// and on this machine it runs the REAL functional pre-flight to get there.
    #[test]
    fn export_ladder_answers_only_from_its_own_const_list() {
        let _serial = ladder_serial();
        let _door = EnvGuard::cleared(super::EXPORT_HW_ENCODER_DISABLE_ENV);
        let Some(bins) = bundled_bins() else {
            eprintln!("SKIPPING: no bundled LGPL ffmpeg in runtime/binaries");
            return;
        };
        let answer = super::export_preferred_encoder(&bins);
        println!("EXPORT-LADDER-RESOLVED preferred={answer:?}");
        if let Some(name) = answer {
            assert!(
                super::EXPORT_PREFERRED_ENCODERS.contains(&name),
                "{name:?} is not in EXPORT_PREFERRED_ENCODERS"
            );
            assert!(
                super::is_nvenc_video_encoder(name) || super::is_mf_video_encoder(name),
                "{name:?} escaped the closed-enum gate"
            );
        }
    }
}

/// Progress callback: fired with a percentage in `[0.0, 100.0]` as ffmpeg's
/// `-progress` stream reports `out_time_us`. Called from the thread driving
/// `push_frame`/`finish` (the caller decides whether to hop threads further).
pub type ProgressFn = Box<dyn FnMut(f64) + Send>;

/// A frame-sink H.264+AAC encoder: raw RGBA frames pushed on `stdin`, muxed
/// against a pre-rendered audio wav, hardware-encoded via `-c:v
/// h264_videotoolbox` (dev) into a single mp4. This is the Phase 7 EXPORT
/// encoder — distinct from [`encode_from_source`] (Phase 1 spike: re-encodes
/// an existing FILE, not a live frame stream).
///
/// Usage: `VideoEncoder::new(...)` spawns the sidecar, then the caller pushes
/// EXACTLY `fps * duration` RGBA frames (tightly packed, `w*h*4` bytes each)
/// via [`push_frame`](Self::push_frame), in presentation order, then calls
/// [`finish`](Self::finish) to close stdin, wait for the process, and check
/// the exit status.
pub struct VideoEncoder {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    width: u32,
    height: u32,
    /// Progress-parser thread reading stderr (`-progress pipe:2`) and
    /// forwarding percentages to the caller's callback.
    progress_thread: Option<std::thread::JoinHandle<()>>,
    /// Non-progress stderr lines (ffmpeg errors/warnings), captured for a
    /// useful error message if the encode fails — `-v error` keeps this to
    /// genuine problems only.
    stderr_log: Arc<Mutex<String>>,
    encoder_name: String,
}

impl VideoEncoder {
    /// Spawn the encoder sidecar: `ffmpeg -f rawvideo -pix_fmt rgba -s WxH -r
    /// FPS -i - -i <audio_wav> -c:v <encoder> <rate control> -c:a aac
    /// -pix_fmt yuv420p -shortest -progress pipe:2 <out_path>`, where
    /// `<rate control>` comes from [`export_rate_control_args`] (quality-
    /// targeted on the shipped Media Foundation path; an explicit `-b:v` only
    /// for non-MF encoders that cannot open without one).
    ///
    /// Encoder selection (HWENC-01, 60-02): the THREE-rung ladder inside
    /// [`resolve_cleared_encoder`] — the loud dev-only
    /// [`DEV_ENCODER_OVERRIDE_ENV`], then export's OWN probed preference
    /// ([`export_encoder_preference`] over [`EXPORT_PREFERRED_ENCODERS`]),
    /// then the cleared [`DEFAULT_VIDEO_ENCODER`]. `total_us` is the expected
    /// output duration, used only to compute the progress percentage.
    ///
    /// **This is the fourth call site that function's own doc demands**, not a
    /// fourth hand-written copy: the inline two-rung resolution that lived here
    /// (a DEV-override read plus an [`encoder_available`] check) is gone, and
    /// `export_encoder_resolution_pins` fails if it comes back.
    pub fn new(
        out_path: &Path,
        width: u32,
        height: u32,
        fps: f64,
        audio_wav: &Path,
        total_us: i64,
        mut on_progress: Option<ProgressFn>,
    ) -> Result<Self, EngineError> {
        let bins = locate()?;

        // The preference rung. A miss (no hardware, a failed functional
        // pre-flight, or the kill switch) is NOT an error — it falls through
        // inside the chokepoint to DEFAULT_VIDEO_ENCODER, which is exactly
        // today's behaviour. Only a genuinely unavailable resolved encoder is
        // still a hard, typed error, with its export-specific wording carried
        // by purpose="export".
        let preferred = export_encoder_preference(&bins, width, height);
        let encoder_name = resolve_cleared_encoder(&bins, "export", preferred)?;

        // One line per export, the executor-facing twin of the render cache's
        // RCENC-SELECTED precedent (T-60-08: an encoder swap must never be
        // silent — the file on disk is the user's deliverable).
        //
        // The rung is derived from the ANSWER, never from a second env read:
        // resolve_cleared_encoder can only ever return the preference, the
        // cleared default, or the DEV override's value, so those three cases
        // are exhaustive. Re-reading DEV_ENCODER_OVERRIDE_ENV here to classify
        // would be a second copy of the chokepoint's own knowledge — and the
        // override already announces itself loudly on its own line.
        let via = if preferred.is_some_and(|p| p == encoder_name) {
            "preferred-probe"
        } else if encoder_name == DEFAULT_VIDEO_ENCODER {
            "default"
        } else {
            "dev-override"
        };
        eprintln!("EXPORT-ENCODER-SELECTED encoder={encoder_name} via={via}");

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Rate control is per-encoder-family — see [`export_rate_control_args`]
        // (debug `proxy-bitrate-starved-all-intra`, round 3, the export twin of
        // the proxy fix, plus 60-02's NVENC arm): the shipped `h264_mf` path is
        // QUALITY-TARGETED and NVENC is CONSTANT-QP at its own calibrated
        // operating point; the flat complexity-blind `-b:v` those replaced
        // starved stacked-overlay sections of real exported files into
        // macroblocks. Only encoders in neither family (macOS
        // `h264_videotoolbox`) still receive an explicit `-b:v`, because
        // videotoolbox refuses to open without one.
        let rate_control = export_rate_control_args(&encoder_name, width, height, fps);

        let mut cmd = ffmpeg_command(&bins.ffmpeg);
        cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-s", &format!("{width}x{height}")])
            .args(["-r", &format!("{fps}")])
            .args(["-i", "-"])
            .arg("-i")
            .arg(audio_wav)
            .args(["-c:v", &encoder_name])
            .args(&rate_control)
            .args(["-c:a", "aac"])
            .args(["-pix_fmt", "yuv420p"])
            .args(["-shortest", "-progress", "pipe:2"])
            .arg(out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            EngineError::SidecarFailed {
                tool: "ffmpeg (export)".to_string(),
                status: -1,
                stderr: "failed to open stdin pipe".to_string(),
            }
        })?;
        let stderr = child.stderr.take().ok_or_else(|| EngineError::SidecarFailed {
            tool: "ffmpeg (export)".to_string(),
            status: -1,
            stderr: "failed to open stderr pipe".to_string(),
        })?;

        // Parse `-progress pipe:2` key=value lines off stderr on a dedicated
        // thread so a slow/absent reader on our side never blocks ffmpeg.
        // `-v error` shares this SAME stream with genuine diagnostics (they
        // don't match the `key=value` progress shape), which we retain for a
        // useful error message if the encode fails.
        let stderr_log = Arc::new(Mutex::new(String::new()));
        let stderr_log_thread = stderr_log.clone();
        let total_us = total_us.max(1);
        let progress_thread = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut line = String::new();
            use std::io::BufRead;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: process exited / stderr closed
                    Ok(_) => {
                        let trimmed = line.trim();
                        if let Some(val) = trimmed.strip_prefix("out_time_us=") {
                            if let Ok(us) = val.parse::<i64>() {
                                let pct = (us.max(0) as f64 / total_us as f64 * 100.0).min(100.0);
                                if let Some(cb) = on_progress.as_mut() {
                                    cb(pct);
                                }
                            }
                        } else if trimmed == "progress=end" {
                            if let Some(cb) = on_progress.as_mut() {
                                cb(100.0);
                            }
                        } else if !trimmed.is_empty() && !is_progress_key_line(trimmed) {
                            // A genuine `-v error` diagnostic line.
                            let mut log = stderr_log_thread.lock().unwrap();
                            log.push_str(trimmed);
                            log.push('\n');
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            child,
            stdin: Some(stdin),
            width,
            height,
            progress_thread: Some(progress_thread),
            stderr_log,
            encoder_name,
        })
    }

    /// The encoder name actually in use (post dev-override resolution) —
    /// exposed for tests/logging to confirm the hardware path was taken.
    pub fn encoder_name(&self) -> &str {
        &self.encoder_name
    }

    /// Push one tightly-packed RGBA frame (`width * height * 4` bytes,
    /// presentation order) to the encoder's stdin.
    pub fn push_frame(&mut self, rgba: &[u8]) -> Result<(), EngineError> {
        let expected = self.width as usize * self.height as usize * 4;
        if rgba.len() != expected {
            return Err(EngineError::BadOutputSize {
                got: rgba.len(),
                expected,
            });
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| EngineError::SidecarFailed {
            tool: "ffmpeg (export)".to_string(),
            status: -1,
            stderr: "push_frame called after finish()".to_string(),
        })?;
        stdin.write_all(rgba)?;
        Ok(())
    }

    /// Close stdin (signals EOF to ffmpeg), wait for the process to exit, and
    /// check its exit status. Joins the progress-parsing thread.
    pub fn finish(mut self) -> Result<(), EngineError> {
        // Drop stdin to send EOF, THEN wait — ffmpeg won't finalize the mp4
        // (moov atom etc.) until it sees EOF on the video pipe.
        drop(self.stdin.take());

        let status = self.child.wait()?;
        if let Some(handle) = self.progress_thread.take() {
            let _ = handle.join();
        }

        if !status.success() {
            let stderr = self.stderr_log.lock().unwrap().clone();
            return Err(EngineError::SidecarFailed {
                tool: "ffmpeg (export)".to_string(),
                status: status.code().unwrap_or(-1),
                stderr,
            });
        }
        Ok(())
    }
}

/// True for a `-progress` machine-readable `key=value` line (as opposed to a
/// genuine `-v error` diagnostic line, which never looks like this). The
/// known progress keys are matched explicitly rather than a bare "contains
/// '='" heuristic, which could misclassify a diagnostic that happens to
/// contain an `=` (e.g. a filter description).
fn is_progress_key_line(line: &str) -> bool {
    const KEYS: &[&str] = &[
        "frame=", "fps=", "stream_", "bitrate=", "total_size=", "out_time_us=", "out_time_ms=",
        "out_time=", "dup_frames=", "drop_frames=", "speed=", "progress=",
    ];
    KEYS.iter().any(|k| line.starts_with(k))
}

/// Best-effort cleanup: if a `VideoEncoder` is dropped WITHOUT calling
/// `finish()` (e.g. an early error return mid-export), make sure the child
/// process is not left running / zombied.
impl Drop for VideoEncoder {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// `ffmpeg -encoders` lists `encoder_name` as an available encoder. Cheap
/// sidecar probe used to fail export CLEARLY (never silently) when the
/// hardware encoder is missing, instead of letting ffmpeg reject `-c:v` deep
/// inside the pipeline after frames are already flowing.
///
/// # This is an AVAILABILITY probe. It is NEVER a licence guard.
///
/// It answers exactly one question — *does this build know that name* — and a
/// `true` from it carries no licensing meaning whatever. `libopenh264` is
/// compiled into the shipped `runtime/binaries/ffmpeg.exe`
/// (`--enable-libopenh264`, verified 2026-08-09) and this function returns
/// `true` for it. The only thing standing between a forbidden encoder and a
/// spawn is the closed `matches!` allowlist ([`is_nvenc_video_encoder`] /
/// [`is_mf_video_encoder`]).
///
/// # Why it is `pub` (60-03)
///
/// So that the export-scope licence gate
/// (`crates/engine/tests/export_hw_encoder.rs`) can make the
/// listed-but-unreachable claim against **this** function — the one the
/// ladder's rung 2 really calls — instead of against a re-implemented
/// `-encoders` spawn that could drift away from it. The name never reaches a
/// command line: it is matched against the second whitespace-separated column
/// of `-encoders` output, so there is nothing to inject through the parameter.
pub fn encoder_available(bins: &FfmpegBinaries, encoder_name: &str) -> Result<bool, EngineError> {
    let output = ffmpeg_command(&bins.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (-encoders)".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().any(|line| {
        line.split_whitespace().nth(1) == Some(encoder_name)
    }))
}

/// Write a MONO f32 PCM buffer as a canonical 16-bit PCM WAV file at
/// [`AUDIO_SAMPLE_RATE`] (converted from f32 with clamping) — the format
/// ffmpeg's `-i audio.wav` input for export consumes. Silence (empty input)
/// still produces a valid (zero-length-samples) wav header.
pub fn write_wav_mono_f32(samples: &[f32], out_path: &Path) -> Result<(), EngineError> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(out_path)?;

    let num_samples = samples.len() as u32;
    let bits_per_sample: u16 = 16;
    let num_channels: u16 = 1;
    let byte_rate = AUDIO_SAMPLE_RATE * num_channels as u32 * (bits_per_sample as u32 / 8);
    let block_align: u16 = num_channels * (bits_per_sample / 8);
    let data_bytes = num_samples * (bits_per_sample as u32 / 8);
    let riff_size = 36 + data_bytes;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVE")?;

    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?; // fmt chunk size (PCM)
    file.write_all(&1u16.to_le_bytes())?; // audio format = PCM
    file.write_all(&num_channels.to_le_bytes())?;
    file.write_all(&AUDIO_SAMPLE_RATE.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&bits_per_sample.to_le_bytes())?;

    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    let mut buf = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = (s.max(-1.0).min(1.0) * i16::MAX as f32).round() as i16;
        buf.extend_from_slice(&clamped.to_le_bytes());
    }
    file.write_all(&buf)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 29 (OVL-03): license-clean ALPHA-ASSET encoders.
//
// These are DELIBERATELY SEPARATE from the frozen `VideoEncoder` /
// `DEFAULT_VIDEO_ENCODER` H.264/Media-Foundation timeline-export path (SC-4):
// they reuse ONLY the sidecar plumbing (`locate`, `ffmpeg_command`,
// `encoder_available`) and encode alpha-preserving assets through the native,
// license-clean FFmpeg codecs `png` (image2, `-c:v png`) and `prores_ks`
// (ProRes 4444, `-pix_fmt yuva444p10le`) — NEVER `VideoEncoder`, NEVER
// `DEFAULT_VIDEO_ENCODER`, NEVER a GPL/patent-encumbered software encoder. Used
// ONLY by `export_overlay_asset` (self-contained transparent overlay export),
// NEVER by timeline export. The frozen path stays byte-untouched — enforced by
// the durable `frozen_export_encoder_untouched` test (tests/overlay_asset_export.rs).
// ---------------------------------------------------------------------------

/// Validate that `frames` is non-empty and every frame shares frame 0's
/// dimensions with a tightly-packed (`w*h*4`) RGBA buffer — checked BEFORE any
/// sidecar spawn so a malformed input errors cleanly instead of mid-pipe.
fn validate_alpha_frames(frames: &[Frame]) -> Result<(u32, u32), EngineError> {
    let Some(first) = frames.first() else {
        return Err(EngineError::BadOutputSize { got: 0, expected: 1 });
    };
    let (w, h) = (first.width.max(1), first.height.max(1));
    let need = w as usize * h as usize * 4;
    for f in frames {
        if f.width != w || f.height != h || f.rgba.len() != need {
            return Err(EngineError::BadOutputSize {
                got: f.rgba.len(),
                expected: need,
            });
        }
    }
    Ok((w, h))
}

/// Pipe `frames` (tightly-packed RGBA) to an already-configured ffmpeg
/// `Command` on stdin, close stdin, and check the exit status — the shared
/// spawn/pipe body for both alpha-asset encoders. `stderr` is captured under
/// `-v error` for a useful failure message.
fn pipe_frames_to_ffmpeg(mut cmd: Command, frames: &[Frame], label: &str) -> Result<(), EngineError> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| EngineError::SidecarFailed {
            tool: label.to_string(),
            status: -1,
            stderr: "failed to open stdin pipe".to_string(),
        })?;
        for f in frames {
            stdin.write_all(&f.rgba)?;
        }
    }
    // Close stdin (EOF) so ffmpeg finalizes, then collect status + stderr.
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(EngineError::SidecarFailed {
            tool: label.to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Encode `frames` (RGBA, alpha-carrying — typically from
/// `Compositor::composite_layers_to_rgba_transparent`) to a real PNG image
/// SEQUENCE under `out_dir`, one lossless `frame_%04d.png` per frame starting at
/// index 0, and return the written paths in order.
///
/// License-clean by construction: uses the native `image2` muxer with the
/// `-c:v png` encoder and `-pix_fmt rgba` so per-pixel ALPHA is preserved on
/// decode (SC-3). Never touches `VideoEncoder` / `DEFAULT_VIDEO_ENCODER`.
pub fn encode_overlay_png_sequence(
    frames: &[Frame],
    fps: f64,
    out_dir: &Path,
) -> Result<Vec<PathBuf>, EngineError> {
    let (w, h) = validate_alpha_frames(frames)?;
    let bins = locate()?;
    if !encoder_available(&bins, "png")? {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (overlay png encoder probe)".to_string(),
            status: -1,
            stderr: "encoder 'png' not available in this ffmpeg build".to_string(),
        });
    }
    std::fs::create_dir_all(out_dir)?;
    let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };
    let pattern = out_dir.join("frame_%04d.png");

    let mut cmd = ffmpeg_command(&bins.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .args(["-s", &format!("{w}x{h}")])
        .args(["-r", &format!("{fps}")])
        .args(["-i", "-"])
        .args(["-frames:v", &frames.len().to_string()])
        // license-clean native PNG image encoder, alpha preserved (-c:v png)
        .args(["-c:v", "png"])
        .args(["-pix_fmt", "rgba"])
        .args(["-start_number", "0"])
        .arg(&pattern);

    pipe_frames_to_ffmpeg(cmd, frames, "ffmpeg (overlay png sequence)")?;

    Ok((0..frames.len())
        .map(|i| out_dir.join(format!("frame_{i:04}.png")))
        .collect())
}

/// Encode `frames` (RGBA, alpha-carrying) to a single real ProRes 4444 file at
/// `out_path` via the native `prores_ks` encoder (`-profile:v 4` = 4444) into
/// the alpha-carrying `yuva444p10le` pixel format — so per-pixel ALPHA survives
/// decode (SC-3). Returns a clean `Err` (never panics) if `prores_ks` is absent
/// from the ffmpeg build. Never touches `VideoEncoder` / `DEFAULT_VIDEO_ENCODER`.
pub fn encode_overlay_prores4444(
    frames: &[Frame],
    fps: f64,
    out_path: &Path,
) -> Result<(), EngineError> {
    let (w, h) = validate_alpha_frames(frames)?;
    let bins = locate()?;
    if !encoder_available(&bins, "prores_ks")? {
        return Err(EngineError::SidecarFailed {
            tool: "ffmpeg (overlay prores_ks encoder probe)".to_string(),
            status: -1,
            stderr: "encoder 'prores_ks' not available in this ffmpeg build".to_string(),
        });
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };

    let mut cmd = ffmpeg_command(&bins.ffmpeg);
    cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .args(["-s", &format!("{w}x{h}")])
        .args(["-r", &format!("{fps}")])
        .args(["-i", "-"])
        .args(["-frames:v", &frames.len().to_string()])
        // license-clean native ProRes 4444 encoder, alpha via yuva444p10le
        .args(["-c:v", "prores_ks"])
        .args(["-profile:v", "4"])
        .args(["-pix_fmt", "yuva444p10le"])
        .arg(out_path);

    pipe_frames_to_ffmpeg(cmd, frames, "ffmpeg (overlay prores 4444)")
}

// ---------------------------------------------------------------------------
// Phase 58 (PROXY-01): proxy-encode arg-vector pins
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proxy_encode_arg_tests {
    //! Pure arg-vector pins — no spawn, no encode. The REAL all-intra proof
    //! (decoded keyframe census on actual output) is
    //! `crates/engine/tests/proxy_encode.rs`; these tests pin the *contract* of
    //! the command line so a future edit cannot quietly reintroduce audio, a
    //! GOP, or a GPL codec name.

    use std::path::Path;

    fn args_as_strings(encoder: &str) -> Vec<String> {
        super::proxy_encode_args(
            Path::new("src.mp4"),
            Path::new("dst.mp4"),
            960,
            540,
            6_000_000,
            encoder,
        )
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
    }

    /// The index of `needle` in `args`, or a readable panic.
    fn index_of(args: &[String], needle: &str) -> usize {
        args.iter()
            .position(|a| a == needle)
            .unwrap_or_else(|| panic!("proxy args must contain `{needle}`: {args:?}"))
    }

    /// 71-01 (TRUST-03): the child publishes its own progress on stderr as
    /// `key=value` lines (`-progress pipe:2`) four times a second, with the
    /// human stats line silenced — and the new globals did not push
    /// `-noautorotate` past the input it must precede.
    #[test]
    fn args_publish_progress_to_stderr_and_silence_stats() {
        for encoder in [super::DEFAULT_VIDEO_ENCODER, "libfoo_dev_override"] {
            let args = args_as_strings(encoder);
            let p = index_of(&args, "-progress");
            assert_eq!(args[p + 1], "pipe:2", "-progress must write to stderr: {args:?}");
            let sp = index_of(&args, "-stats_period");
            assert_eq!(args[sp + 1], "0.25", "-stats_period must be 0.25 s: {args:?}");
            let ns = index_of(&args, "-nostats");
            let i = index_of(&args, "-i");
            assert!(
                p < i && sp < i && ns < i,
                "progress globals must precede the input: {args:?}"
            );
            assert!(
                index_of(&args, "-noautorotate") < i,
                "-noautorotate must still precede -i: {args:?}"
            );
        }
    }

    /// 71-01: the pump keeps ONLY genuine diagnostics, stores the latest
    /// numeric `out_time_us`, ignores `N/A`, and counts every byte read.
    #[test]
    fn the_proxy_pump_parses_out_time_and_keeps_only_diagnostics() {
        use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
        use std::sync::Mutex;

        let input = "frame=10\nfps=0.0\nout_time_us=500000\nprogress=continue\n\
                     [h264_mf @ 0x1] real diagnostic\nout_time_us=N/A\n\
                     out_time_us=1000000\nprogress=end\n";
        let kept = Mutex::new(String::new());
        let seen = AtomicU64::new(0);
        let out = AtomicI64::new(-1);
        super::pump_proxy_stderr(std::io::Cursor::new(input.as_bytes()), &kept, &seen, &out);

        assert_eq!(kept.lock().unwrap().as_str(), "[h264_mf @ 0x1] real diagnostic\n");
        assert_eq!(out.load(Ordering::Relaxed), 1_000_000);
        assert_eq!(seen.load(Ordering::Relaxed), input.len() as u64);
    }

    /// 71-01 / T-71-01: 200 KiB of progress lines cannot consume the 64 KiB
    /// capture — a diagnostic that arrives AFTER them is still kept.
    #[test]
    fn progress_lines_never_consume_the_error_capture() {
        use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
        use std::sync::Mutex;

        let mut input = String::new();
        let mut t = 0i64;
        while input.len() < 200 * 1024 {
            input.push_str(&format!(
                "frame={t}\nfps=30.0\nbitrate=N/A\nout_time_us={t}\nout_time=00:00:00\n\
                 speed=1.0x\nprogress=continue\n"
            ));
            t += 250_000;
        }
        input.push_str("[h264_mf @ 0x2] the diagnostic that must survive\n");

        let kept = Mutex::new(String::new());
        let seen = AtomicU64::new(0);
        let out = AtomicI64::new(-1);
        super::pump_proxy_stderr(std::io::Cursor::new(input.as_bytes()), &kept, &seen, &out);

        let kept = kept.into_inner().unwrap();
        assert!(kept.len() < super::PROXY_STDERR_CAPTURE_BYTES, "kept {} bytes", kept.len());
        assert!(kept.contains("the diagnostic that must survive"), "kept: {kept:?}");
        assert_eq!(seen.load(Ordering::Relaxed), input.len() as u64);
        assert_eq!(out.load(Ordering::Relaxed), t - 250_000);
    }

    /// 71-REVIEW WR-03: a `\r`-only stats stream is parsed line by line, and an
    /// unterminated flood is bounded: the diagnostic after it is still kept, the
    /// progress after it is still parsed, and every byte is still counted.
    #[test]
    fn the_proxy_pump_bounds_long_and_carriage_return_lines() {
        use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
        use std::sync::Mutex;

        let mut input = String::new();
        // `\r`-terminated stats, the shape `-nostats` normally suppresses.
        for i in 0..50 {
            input.push_str(&format!("frame={i} fps=30 size=N/A time=00:00:0{} \r", i % 10));
        }
        input.push_str("out_time_us=250000\r\n");
        // One line far past the cap, with no terminator for 1 MiB.
        input.push_str(&"x".repeat(1024 * 1024));
        input.push('\n');
        input.push_str("[h264_mf @ 0x3] the diagnostic after the flood\n");
        input.push_str("out_time_us=750000\n");

        let kept = Mutex::new(String::new());
        let seen = AtomicU64::new(0);
        let out = AtomicI64::new(-1);
        super::pump_proxy_stderr(std::io::Cursor::new(input.as_bytes()), &kept, &seen, &out);

        let kept = kept.into_inner().unwrap();
        assert!(kept.contains("the diagnostic after the flood"), "kept: {} bytes", kept.len());
        assert!(
            !kept.contains(&"x".repeat(super::PROXY_STDERR_MAX_LINE + 1)),
            "an over-long line must be truncated at the cap"
        );
        assert_eq!(out.load(Ordering::Relaxed), 750_000);
        assert_eq!(seen.load(Ordering::Relaxed), input.len() as u64);
    }

    /// D-01: `-g 1` (all-intra) with `-bf 0` (no B-frames), as flag/value pairs.
    #[test]
    fn args_request_all_intra_with_no_b_frames() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        let g = index_of(&args, "-g");
        assert_eq!(args[g + 1], "1", "-g must be 1 (every frame a keyframe): {args:?}");
        let bf = index_of(&args, "-bf");
        assert_eq!(args[bf + 1], "0", "-bf must be 0: {args:?}");
    }

    /// D-06: video-only. `-an` present, and NO audio codec flag at all — the
    /// concrete reason the test-only file-to-file re-encoder (which hardcodes
    /// `-c:a aac`) could not be extended for this job.
    #[test]
    fn args_are_video_only_and_never_name_an_audio_codec() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        assert!(args.iter().any(|a| a == "-an"), "-an required: {args:?}");
        assert!(
            !args.iter().any(|a| a == "-c:a" || a == "-acodec"),
            "a proxy must never encode audio: {args:?}"
        );
    }

    /// `-noautorotate` is an INPUT option: it only applies if it precedes
    /// `-i`. If it drifted after `-i`, a rotated source's proxy would be baked
    /// rotated AND rotated again at present time.
    #[test]
    fn noautorotate_precedes_the_input() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        let nar = index_of(&args, "-noautorotate");
        let i = index_of(&args, "-i");
        assert!(
            nar < i,
            "-noautorotate must precede -i to apply to the input: {args:?}"
        );
    }

    /// D-05's geometry is passed through verbatim as the scale filter.
    #[test]
    fn scale_filter_matches_the_requested_dimensions_exactly() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        let vf = index_of(&args, "-vf");
        assert_eq!(args[vf + 1], "scale=960:540", "{args:?}");
    }

    /// D-04: a proxy shares the source's fps, timebase and duration — no flag
    /// may touch any of them.
    #[test]
    fn args_never_touch_fps_timebase_or_duration() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        for forbidden in ["-r", "-t", "-vsync", "-fps_mode", "-video_track_timescale", "-ss"] {
            assert!(
                !args.iter().any(|a| a == forbidden),
                "`{forbidden}` would break D-04 (same fps/timebase/duration): {args:?}"
            );
        }
    }

    /// CLAUDE.md rule 6 / D-03 / T-58-01-01: no GPL or patent-encumbered
    /// encoder name can appear on the proxy command line by default.
    #[test]
    fn default_proxy_args_name_no_gpl_or_patent_encumbered_encoder() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        for banned in ["libx264", "libx265", "openh264", "libopenh264"] {
            assert!(
                !args.iter().any(|a| a == banned),
                "`{banned}` must be unreachable from the proxy path: {args:?}"
            );
        }
        let cv = index_of(&args, "-c:v");
        assert_eq!(
            args[cv + 1],
            super::DEFAULT_VIDEO_ENCODER,
            "the proxy path encodes with the cleared default encoder"
        );
    }

    /// Debug `proxy-bitrate-starved-all-intra`: the SHIPPED Media Foundation
    /// path is QUALITY-TARGETED. A flat `-b:v` on an all-intra encode is the
    /// exact defect that shipped 0.19 bits/pixel macroblocked proxies of dense
    /// 4K59.94 footage, so its absence here is load-bearing: `-rate_control
    /// quality -quality N` present, and NO `-b:v` at all.
    #[test]
    fn mf_args_are_quality_targeted_and_never_pass_a_bitrate() {
        let args = args_as_strings("h264_mf");
        let rc = index_of(&args, "-rate_control");
        assert_eq!(args[rc + 1], "quality", "{args:?}");
        let q = index_of(&args, "-quality");
        assert_eq!(
            args[q + 1],
            super::PROXY_ENCODE_QUALITY.to_string(),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "-b:v"),
            "a flat bitrate must never reach the MF proxy encode again — that \
             constant WAS the macroblocking bug: {args:?}"
        );
    }

    /// A NON-MF encoder (reachable only through the DEV-only override door)
    /// must NOT receive the MF-private options — an unknown private option is
    /// a hard spawn failure — and falls back to the caller's `-b:v`.
    #[test]
    fn non_mf_args_fall_back_to_the_callers_bitrate() {
        let args = args_as_strings("h264_videotoolbox");
        assert!(
            !args.iter().any(|a| a == "-rate_control" || a == "-quality"),
            "MF-private options would hard-fail a non-MF encoder: {args:?}"
        );
        let b = index_of(&args, "-b:v");
        assert_eq!(args[b + 1], "6000000", "{args:?}");
    }

    /// The output path is last, the input path immediately follows `-i` —
    /// getting these backwards would silently overwrite the SOURCE.
    #[test]
    fn input_and_output_paths_land_in_the_right_slots() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        let i = index_of(&args, "-i");
        assert_eq!(args[i + 1], "src.mp4", "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("dst.mp4"), "{args:?}");
    }
}

// ---------------------------------------------------------------------------
// Phase 58 (D-31): container bit-rate probe tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod probe_bit_rate_tests {
    //! Phase 58 D-31: `MediaInfo` gains an additive `bit_rate`, parsed from the
    //! `-show_format` JSON `probe()` ALREADY fetches — no second ffprobe spawn.
    //!
    //! It is the bitrate half of the D-07/D-08 heaviness predicate, which
    //! 58-RESEARCH § Pitfall 4 found does not exist today ("D-07's wording
    //! reads as already-true; `bit_rate` is in ffprobe's JSON output but not
    //! parsed into `MediaInfo`").

    use std::path::{Path, PathBuf};

    use super::{parse_format_bit_rate, ProbeFormat};

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    /// A real, COMMITTED 5 s h264 clip (~1.99 MB → ~3.2 Mbps).
    fn fixture() -> PathBuf {
        workspace_root().join("test-media/bars_720p30_5s.mp4")
    }

    /// CLAUDE.md rule 3: prove the field on a REAL probe of a REAL file, not on
    /// a hand-written JSON blob.
    #[test]
    fn probing_a_real_video_reports_a_real_container_bit_rate() {
        let path = fixture();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let info = super::probe(&path).expect("probe a real committed fixture");
        let bit_rate = info
            .bit_rate
            .expect("a real h264 mp4 must report a container bit_rate");
        assert!(
            bit_rate > 100_000,
            "bars_720p30_5s.mp4 is ~1.99 MB / 5 s (~3.2 Mbps); got {bit_rate} bps"
        );
    }

    /// The SAME probe call must still cost exactly ONE ffprobe spawn — the
    /// whole point of D-31 is that bit_rate is free (it rides the JSON the
    /// import path already pays for).
    #[test]
    fn bit_rate_costs_no_extra_probe_spawn() {
        let path = fixture();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let _ = super::locate();
        let (_, before) = super::thread_spawn_counts();
        let info = super::probe(&path).expect("probe");
        let (_, after) = super::thread_spawn_counts();
        assert!(info.bit_rate.is_some(), "the field must actually be populated");
        assert_eq!(
            after - before,
            1,
            "one probe() call must remain exactly one ffprobe spawn"
        );
    }

    /// A container that reports no `bit_rate` (or a non-numeric one) parses to
    /// `None` — never an error, never a panic. Some containers genuinely omit
    /// it, and a heaviness predicate that errored on them would break import.
    #[test]
    fn a_missing_or_unparsable_bit_rate_is_none_never_an_error() {
        let absent: ProbeFormat = serde_json::from_str(r#"{"duration":"5.0"}"#)
            .expect("a format object with no bit_rate key must still parse");
        assert_eq!(parse_format_bit_rate(Some(&absent)), None);

        let na: ProbeFormat = serde_json::from_str(r#"{"bit_rate":"N/A"}"#)
            .expect("a non-numeric bit_rate must still parse");
        assert_eq!(parse_format_bit_rate(Some(&na)), None);

        let empty: ProbeFormat =
            serde_json::from_str(r#"{"bit_rate":""}"#).expect("an empty bit_rate must still parse");
        assert_eq!(parse_format_bit_rate(Some(&empty)), None);

        assert_eq!(parse_format_bit_rate(None), None, "no format object at all");
    }

    /// ffprobe emits `bit_rate` as a JSON STRING, not a number — the parse must
    /// handle that shape (and tolerate surrounding whitespace).
    #[test]
    fn a_numeric_string_bit_rate_parses_to_the_number() {
        let ok: ProbeFormat =
            serde_json::from_str(r#"{"bit_rate":"60123456"}"#).expect("parse");
        assert_eq!(parse_format_bit_rate(Some(&ok)), Some(60_123_456));

        let padded: ProbeFormat =
            serde_json::from_str(r#"{"bit_rate":" 3187456 "}"#).expect("parse");
        assert_eq!(parse_format_bit_rate(Some(&padded)), Some(3_187_456));
    }
}

// ---------------------------------------------------------------------------
// Phase 8: per-OS encoder seam tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod live_mix_probe_budget_tests {
    //! LIVE-UAT REGRESSION (`retime-live-uat-frontend-mirror-undo-audio`,
    //! symptom 3): the live preview mixer's per-window ffprobe.
    //!
    //! `AudioOutput::start_mix`'s producer must deliver `CHUNK_US` (2 s) of
    //! audio per 2 s of WALL CLOCK. It renders once per staircase window, and
    //! the PUBLIC `render_audio_pcm_retimed` spends a whole ffprobe process per
    //! call just to answer `has_audio` — MEASURED at ~110-130 ms against the
    //! bundled binary, i.e. roughly HALF the per-window cost. That is free at
    //! one call per 2 s chunk (the un-retimed path) and ruinous at one call per
    //! 100-400 ms window.
    //!
    //! This asserts the SPAWN COUNT, not wall time — the same discipline the
    //! export throughput pins use. It is the second half of the real-time
    //! budget whose first half is
    //! `core/tests/retime.rs::a_live_preview_chunk_of_ramped_audio_stays_inside_
    //! the_real_time_spawn_budget` (which bounds the window COUNT): together
    //! they cap one chunk of ramped live mix at `<= 6` renders plus ONE probe
    //! per distinct path, instead of 6 renders plus 6 probes.

    use std::path::{Path, PathBuf};

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    /// The real 5 s h264+aac clip the live UAT used.
    fn fixture() -> PathBuf {
        workspace_root().join("test-media/bars_720p30_5s.mp4")
    }

    /// Simulate one 2 s live-mix chunk of a RAMPED contributor: N staircase
    /// windows rendered back to back on ONE thread, exactly as the producer
    /// does. The mixer already knows `has_audio` from the imported
    /// `MediaBinItem`, so the whole loop must cost ZERO probes.
    #[test]
    fn a_chunk_of_ramped_live_mix_windows_costs_no_per_window_probe() {
        let path = fixture();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        // Warm `locate()`'s resolution before measuring (it does not probe, but
        // keep the window around the renders only).
        let _ = super::locate();

        const WINDOWS: usize = 6; // the shipped per-chunk budget
        let (_, probes_before) = super::thread_spawn_counts();
        for i in 0..WINDOWS {
            let in_us = i as i64 * 400_000;
            let pcm = super::render_audio_pcm_retimed_known_audio(
                &path,
                in_us,
                in_us + 400_000,
                1.0,
                1.6,
                true,
            )
            .expect("a real windowed retimed render");
            assert!(!pcm.is_empty(), "window {i} produced no samples");
        }
        let (_, probes_after) = super::thread_spawn_counts();
        assert_eq!(
            probes_after - probes_before,
            0,
            "{WINDOWS} live-mix windows must cost ZERO ffprobe spawns — the \
             caller already knows has_audio"
        );
    }

    /// NON-VACUITY / negative control: the PUBLIC entry point still probes on
    /// every call. Without this the assertion above could pass against an
    /// engine that stopped probing everywhere (which would silently turn a
    /// video-without-audio into an ffmpeg ERROR instead of the documented empty
    /// vec), and it is what makes the "one probe per window" cost real.
    #[test]
    fn the_public_entry_point_still_probes_once_per_call() {
        let path = fixture();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let _ = super::locate();

        const CALLS: usize = 3;
        let (_, before) = super::thread_spawn_counts();
        for i in 0..CALLS {
            let in_us = i as i64 * 400_000;
            super::render_audio_pcm_retimed(&path, in_us, in_us + 400_000, 1.0, 1.6)
                .expect("a real windowed retimed render");
        }
        let (_, after) = super::thread_spawn_counts();
        assert_eq!(
            after - before,
            CALLS,
            "the probing contract of the public entry point is unchanged"
        );
    }

    /// The two entry points must return the SAME PCM — the hoist is a cost
    /// change, never a content change.
    #[test]
    fn skipping_the_probe_does_not_change_a_single_sample() {
        let path = fixture();
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let probed = super::render_audio_pcm_retimed(&path, 1_000_000, 1_400_000, 0.8, 1.6)
            .expect("probed render");
        let known =
            super::render_audio_pcm_retimed_known_audio(&path, 1_000_000, 1_400_000, 0.8, 1.6, true)
                .expect("known-audio render");
        assert!(!probed.is_empty(), "the control produced no samples");
        assert_eq!(probed, known, "the hoist must be sample-identical");
    }
}

#[cfg(test)]
mod encoder_seam_tests {
    use super::DEFAULT_VIDEO_ENCODER;

    /// On macOS (the dev target), the default export encoder MUST be Apple's
    /// hardware VideoToolbox encoder — license-safe (system framework, never
    /// GPL/patent-encumbered libx264/libx265/openh264).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_encoder_is_videotoolbox() {
        assert_eq!(DEFAULT_VIDEO_ENCODER, "h264_videotoolbox");
    }

    /// On Windows (the shipped target), the default export encoder is the
    /// Media Foundation encoder (Windows' SOFTWARE `H264 Encoder MFT` by
    /// default — `hw_encoding` is false unless asked for). RUNS on Windows
    /// (Phase 10) — this
    /// asserts the seam's value, and the real MF export is verified by the
    /// export gate (`tests/export_encode.rs`, output tagged `Lavc h264_mf`).
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_default_encoder_is_media_foundation() {
        assert_eq!(DEFAULT_VIDEO_ENCODER, "h264_mf");
    }

    /// Regression guard for the Phase 8 bug: the non-macOS branch must NEVER
    /// silently fall back to the macOS-only `h264_videotoolbox` name (that
    /// encoder does not exist outside Apple platforms, and shipping it as a
    /// "default" on Windows/Linux would make export fail confusingly instead
    /// of using the correct per-OS hardware encoder or a clear error).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_default_encoder_is_never_videotoolbox() {
        assert_ne!(DEFAULT_VIDEO_ENCODER, "h264_videotoolbox");
    }

    /// License guard, all platforms: the default must never be a
    /// GPL/patent-encumbered software encoder.
    #[test]
    fn default_encoder_is_never_gpl_or_patent_encumbered() {
        assert_ne!(DEFAULT_VIDEO_ENCODER, "libx264");
        assert_ne!(DEFAULT_VIDEO_ENCODER, "libx265");
        assert_ne!(DEFAULT_VIDEO_ENCODER, "libopenh264");
    }
}

// ---------------------------------------------------------------------------
// Phase 59 (CACHE-01): render-cache segment encoder arg-vector pins
// ---------------------------------------------------------------------------

#[cfg(test)]
mod render_cache_encode_arg_tests {
    //! Pure arg-vector pins — no spawn, no encode. The REAL proof (a decoded
    //! keyframe census and a stream census over actual output) is
    //! `crates/engine/tests/render_cache_encode.rs`; these pin the *contract* of
    //! the command line so a future edit cannot quietly reintroduce audio, a
    //! GOP, an output-side rate conversion, or a caller-chosen codec.
    //!
    //! Deliberately OUTSIDE the RENDER-CACHE-ENCODER source region: the
    //! integration gate's licence scan refuses the GPL encoder names as source
    //! text inside that region, and these tests must be able to name them in
    //! order to assert their absence.

    use std::path::Path;

    fn args_as_strings(encoder: &str) -> Vec<String> {
        super::render_cache_encode_args(
            Path::new("seg.mp4"),
            1920,
            1080,
            30.0,
            24_000_000,
            encoder,
        )
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
    }

    /// Binaries for the two pins that really spawn a probe: the BUNDLED LGPL
    /// build first (the one that ships, and the one whose `-encoders` list the
    /// ladder is about), falling back to whatever [`super::locate`] finds so a
    /// machine without a fetched `runtime/binaries` still runs the pin rather
    /// than silently skipping it.
    fn test_bins() -> Option<super::FfmpegBinaries> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());
        if let Some(root) = root {
            for dir in ["runtime/binaries", "crates/engine/ffmpeg-dev/bin"] {
                let d = root.join(dir);
                let ffmpeg = d.join("ffmpeg.exe");
                let ffprobe = d.join("ffprobe.exe");
                if ffmpeg.is_file() && ffprobe.is_file() {
                    return Some(super::FfmpegBinaries { ffmpeg, ffprobe });
                }
            }
        }
        super::locate().ok()
    }

    /// The index of `needle` in `args`, or a readable panic.
    fn index_of(args: &[String], needle: &str) -> usize {
        args.iter()
            .position(|a| a == needle)
            .unwrap_or_else(|| panic!("segment args must contain `{needle}`: {args:?}"))
    }

    /// D-05: `-g 1` (all-intra) with `-bf 0` (no B-frames), as flag/value pairs.
    #[test]
    fn args_request_all_intra_with_no_b_frames() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        let g = index_of(&args, "-g");
        assert_eq!(args[g + 1], "1", "-g must be 1 (every frame a keyframe): {args:?}");
        let bf = index_of(&args, "-bf");
        assert_eq!(args[bf + 1], "0", "-bf must be 0: {args:?}");
    }

    /// D-08: video-only. `-an` present, and NO audio codec flag at all — the
    /// concrete reason the test-only file-to-file re-encoder (which hardcodes
    /// `-c:a aac`) could not be extended for this job (58 D-32).
    #[test]
    fn args_are_video_only_and_never_name_an_audio_codec() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        assert!(args.iter().any(|a| a == "-an"), "-an required: {args:?}");
        assert!(
            !args.iter().any(|a| a == "-c:a" || a == "-acodec"),
            "a cache segment must never encode audio: {args:?}"
        );
    }

    /// The rawvideo input contract: `-f rawvideo`, `-pix_fmt rgba` and the
    /// geometry are INPUT options, so all of them must precede `-i`. If the
    /// geometry drifted after `-i`, ffmpeg would be reading an unframed byte
    /// stream with no idea how long a frame is.
    #[test]
    fn the_rawvideo_input_contract_precedes_the_input() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        let i = index_of(&args, "-i");
        assert_eq!(args[i + 1], "-", "the input must be stdin: {args:?}");
        let f = index_of(&args, "-f");
        assert!(f < i, "-f rawvideo must precede -i: {args:?}");
        assert_eq!(args[f + 1], "rawvideo");
        let s = index_of(&args, "-s");
        assert!(s < i, "-s WxH must precede -i: {args:?}");
        assert_eq!(args[s + 1], "1920x1080", "{args:?}");
        // The FIRST -pix_fmt is the input one (rgba); the LAST is the output
        // one (yuv420p). Both matter and they are not interchangeable.
        let first_pix = index_of(&args, "-pix_fmt");
        assert!(first_pix < i, "the input -pix_fmt must precede -i: {args:?}");
        assert_eq!(args[first_pix + 1], "rgba", "{args:?}");
        let last_pix = args.iter().rposition(|a| a == "-pix_fmt").unwrap();
        assert!(last_pix > i, "the output -pix_fmt must follow -i: {args:?}");
        assert_eq!(args[last_pix + 1], "yuv420p", "{args:?}");
    }

    /// **D-07's identity, as a command-line property.** `-r` appears EXACTLY
    /// ONCE and it is on the INPUT side. A second `-r` after `-i` would be an
    /// output frame-rate conversion — a drop/dup term — and N frames pushed at F
    /// fps would stop coming back as N frames at F fps.
    #[test]
    fn the_rate_flag_appears_once_and_only_on_the_input_side() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        let rs: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == "-r")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(rs.len(), 1, "-r must appear exactly once: {args:?}");
        let i = index_of(&args, "-i");
        assert!(rs[0] < i, "-r must precede -i (input rate): {args:?}");
        assert_eq!(args[rs[0] + 1], "30", "{args:?}");
    }

    /// D-07 again, from the other side: nothing may re-time, trim or re-stamp
    /// the stream.
    #[test]
    fn args_never_retime_trim_or_restamp() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        for forbidden in ["-t", "-ss", "-vsync", "-fps_mode", "-video_track_timescale", "-vf"] {
            assert!(
                !args.iter().any(|a| a == forbidden),
                "a cache segment must not carry `{forbidden}`: {args:?}"
            );
        }
    }

    /// CLAUDE.md rule 6 / threat T-59-03-01: whatever else changes, the built
    /// vector must never carry a GPL or patent-encumbered encoder name. The only
    /// way one can appear is the loud dev override, which is the caller's
    /// (`resolve_cleared_encoder`) business, not this builder's.
    #[test]
    fn the_default_vector_names_no_gpl_encoder() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        for banned in ["libx264", "libx265", "openh264", "libopenh264"] {
            assert!(
                !args.iter().any(|a| a == banned),
                "`{banned}` must be unreachable from the segment encoder: {args:?}"
            );
        }
        let c = index_of(&args, "-c:v");
        assert_eq!(
            args[c + 1],
            super::DEFAULT_VIDEO_ENCODER,
            "the default path must encode with the cleared default: {args:?}"
        );
    }

    /// The destination is the LAST argument and is passed through verbatim (no
    /// temp-name policy lives here — that is `rendercache::generate`'s job).
    #[test]
    fn the_destination_is_the_last_argument_verbatim() {
        let args = args_as_strings(super::DEFAULT_VIDEO_ENCODER);
        assert_eq!(args.last().map(String::as_str), Some("seg.mp4"), "{args:?}");
        let mov = index_of(&args, "-movflags");
        assert_eq!(args[mov + 1], "+faststart", "{args:?}");
    }

    // -----------------------------------------------------------------------
    // 59-11 — the rate-control MODE, and the two measurement doors
    // -----------------------------------------------------------------------

    /// `cargo test` runs a binary's tests as THREADS in one process and an
    /// environment variable is process-global, so every test below that reads
    /// or writes a door variable takes this lock first. Same pattern as
    /// `crates/rendercache/tests/encoder_license.rs`.
    static DOOR_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn door_serial() -> std::sync::MutexGuard<'static, ()> {
        DOOR_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set a door for a scope and restore whatever was there before —
    /// including "nothing", which a naive set/remove pair gets wrong on a
    /// machine that really does export the variable.
    struct DoorGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl DoorGuard {
        fn set(key: &'static str, value: &str) -> DoorGuard {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            DoorGuard { key, prev }
        }
        fn cleared(key: &'static str) -> DoorGuard {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            DoorGuard { key, prev }
        }
    }

    impl Drop for DoorGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// **The 59-11 port, and the absence is the load-bearing half.** The
    /// SHIPPED Media Foundation path is QUALITY-TARGETED; a flat `-b:v` on an
    /// all-intra stream is exactly what asked for 24 Mbps and delivered
    /// 2.2 Mbps (`59-CACHE-CALIBRATION` § 2), so `-b:v` must not appear at all.
    #[test]
    fn mf_segment_args_are_quality_targeted_and_never_pass_a_bitrate() {
        let _serial = door_serial();
        let _hw = DoorGuard::cleared(super::RENDER_CACHE_HW_ENCODING_ENV);
        let _sc = DoorGuard::cleared(super::RENDER_CACHE_SCENARIO_ENV);

        for mf in ["h264_mf", "hevc_mf"] {
            let args = args_as_strings(mf);
            let rc = index_of(&args, "-rate_control");
            assert_eq!(args[rc + 1], "quality", "{mf}: {args:?}");
            let q = index_of(&args, "-quality");
            assert_eq!(
                args[q + 1],
                super::RENDER_CACHE_ENCODE_QUALITY.to_string(),
                "{mf}: {args:?}"
            );
            assert!(
                !args.iter().any(|a| a == "-b:v"),
                "a flat bitrate must never reach the MF segment encode again — that \
                 constant is what starved the all-intra payload: {args:?}"
            );
            // The doors are CLOSED by default. A shipped build must never emit
            // either flag.
            assert!(
                !args.iter().any(|a| a == "-hw_encoding" || a == "-scenario"),
                "the measurement doors must be shut unless their env is set: {args:?}"
            );
        }
    }

    /// **The NVENC recipe, and every term of it is a measured correction.**
    ///
    /// * `-g 0` — NVENC's all-intra spelling. `-g 1` **cannot open**: NVENC
    ///   enforces GOP length > B-frames + 1, so with `-bf 0` the minimum legal
    ///   GOP is 2 and `-g 1` dies with `InitializeEncoder failed: invalid param
    ///   (8)` / "Gop Length should be greater than number of B frames + 1".
    ///   Verified by keyframe census: `-g 0` gives 60/60 at 2 s and 150/150 at
    ///   5 s, while `-g 2` gives 30/60 — NOT all-intra. Getting this wrong
    ///   silently destroys the seek property the whole cache exists for, which
    ///   is why the real-output census in `tests/render_cache_encode.rs` gates
    ///   it as well as this vector pin.
    /// * `-rc constqp -qp N` — NVENC's own rate control. The MF spelling
    ///   (`-rate_control quality -quality N`) is a private option `h264_nvenc`
    ///   has never heard of and would refuse to spawn against.
    /// * NO `-b:v` — a flat bitrate on an all-intra stream is the defect 59-11
    ///   removed from the MF path; it is not reintroduced here.
    #[test]
    fn nvenc_segment_args_are_all_intra_constqp_and_never_pass_a_bitrate() {
        let _serial = door_serial();
        let _hw = DoorGuard::cleared(super::RENDER_CACHE_HW_ENCODING_ENV);
        let _sc = DoorGuard::cleared(super::RENDER_CACHE_SCENARIO_ENV);

        for nv in ["h264_nvenc", "hevc_nvenc"] {
            assert!(
                super::is_nvenc_video_encoder(nv),
                "{nv} must be recognised by the nvenc discriminator"
            );
            let args = args_as_strings(nv);
            let c = index_of(&args, "-c:v");
            assert_eq!(args[c + 1], nv, "{args:?}");
            let g = index_of(&args, "-g");
            assert_eq!(
                args[g + 1], "0",
                "{nv}: -g must be 0 (NVENC's all-intra spelling; -g 1 cannot open \
                 because GOP must exceed B-frames + 1): {args:?}"
            );
            let bf = index_of(&args, "-bf");
            assert_eq!(args[bf + 1], "0", "{nv}: {args:?}");
            let rc = index_of(&args, "-rc");
            assert_eq!(args[rc + 1], "constqp", "{nv}: {args:?}");
            let qp = index_of(&args, "-qp");
            assert_eq!(
                args[qp + 1],
                super::RENDER_CACHE_NVENC_QP.to_string(),
                "{nv}: {args:?}"
            );
            assert!(
                !args
                    .iter()
                    .any(|a| a == "-b:v" || a == "-rate_control" || a == "-quality"),
                "{nv}: neither a flat bitrate nor the MF-private rate control may \
                 reach an NVENC command line: {args:?}"
            );
        }
    }

    /// The two MEASUREMENT DOORS are MF-private and must not leak onto an NVENC
    /// command line even when both are wide open. `-hw_encoding` and
    /// `-scenario` are `mfenc` options; ffmpeg rejects an unknown private option
    /// outright, so a leak here would not degrade the encode — it would stop the
    /// render cache dead on every NVIDIA machine.
    #[test]
    fn the_measurement_doors_never_leak_onto_an_nvenc_command_line() {
        let _serial = door_serial();
        let _hw = DoorGuard::set(super::RENDER_CACHE_HW_ENCODING_ENV, "1");
        let _sc = DoorGuard::set(super::RENDER_CACHE_SCENARIO_ENV, "archive");

        for nv in ["h264_nvenc", "hevc_nvenc"] {
            let args = args_as_strings(nv);
            assert!(
                !args
                    .iter()
                    .any(|a| a == "-hw_encoding" || a == "-scenario"),
                "{nv}: the MF measurement doors must be structurally unable to reach \
                 a non-MF encoder: {args:?}"
            );
            // ...and the doors being open must not have disturbed the recipe.
            let rc = index_of(&args, "-rc");
            assert_eq!(args[rc + 1], "constqp", "{nv}: {args:?}");
            let g = index_of(&args, "-g");
            assert_eq!(args[g + 1], "0", "{nv}: {args:?}");
        }
    }

    /// A NON-MF encoder (reachable only through the DEV-only override door)
    /// must NOT receive the MF-private options — an unknown private option is a
    /// hard spawn failure — and keeps the caller's `-b:v`, which is also the one
    /// platform requirement behind it (`h264_videotoolbox` refuses to open
    /// without an explicit bitrate).
    #[test]
    fn non_mf_segment_args_keep_the_callers_bitrate_and_no_mf_options() {
        let _serial = door_serial();
        // Both doors OPEN: they must still not leak onto a non-MF command line.
        let _hw = DoorGuard::set(super::RENDER_CACHE_HW_ENCODING_ENV, "1");
        let _sc = DoorGuard::set(super::RENDER_CACHE_SCENARIO_ENV, "archive");

        let args = args_as_strings("h264_videotoolbox");
        assert!(
            !args
                .iter()
                .any(|a| a == "-rate_control" || a == "-quality" || a == "-hw_encoding" || a == "-scenario"),
            "MF-private options would hard-fail a non-MF encoder: {args:?}"
        );
        let b = index_of(&args, "-b:v");
        assert_eq!(args[b + 1], "24000000", "{args:?}");
    }

    /// The `-hw_encoding` door appears ONLY under its env var, and its state is
    /// the thing 59-14's matrix varies. Set, assert, restore.
    #[test]
    fn the_hw_encoding_door_is_shut_unless_its_env_is_set() {
        let _serial = door_serial();
        {
            let _hw = DoorGuard::cleared(super::RENDER_CACHE_HW_ENCODING_ENV);
            let args = args_as_strings("h264_mf");
            assert!(
                !args.iter().any(|a| a == "-hw_encoding"),
                "closed door: {args:?}"
            );
        }
        for opener in ["1", "true", "TRUE"] {
            let _hw = DoorGuard::set(super::RENDER_CACHE_HW_ENCODING_ENV, opener);
            let args = args_as_strings("h264_mf");
            let i = index_of(&args, "-hw_encoding");
            assert_eq!(args[i + 1], "true", "{opener}: {args:?}");
        }
        for non_opener in ["0", "false", "", "  ", "yes"] {
            let _hw = DoorGuard::set(super::RENDER_CACHE_HW_ENCODING_ENV, non_opener);
            let args = args_as_strings("h264_mf");
            assert!(
                !args.iter().any(|a| a == "-hw_encoding"),
                "{non_opener:?} must not open the door: {args:?}"
            );
        }
    }

    /// **The forced-fallback pin: a machine without the preferred hardware must
    /// behave EXACTLY as it did before the preference existed.**
    ///
    /// This dev machine is NVIDIA-only, and `REQUIREMENTS.md § Known
    /// constraint` records cross-vendor behaviour as UNVALIDATED, so the
    /// fallback rung cannot be exercised by simply having different hardware
    /// here. It is exercised by sending a name NO ffmpeg build has through the
    /// REAL [`super::encoder_available`] probe: the probe really spawns, really
    /// misses, and resolution really falls through to
    /// [`super::DEFAULT_VIDEO_ENCODER`]. A miss on the preference rung is not
    /// an error — that is the whole property.
    ///
    /// Deliberately NOT a mocked probe: the thing under test IS the probe's
    /// answer feeding a fall-through, and a mock would pin the mock.
    #[test]
    fn an_unavailable_preferred_encoder_really_falls_back() {
        let _serial = door_serial();
        let _dev = DoorGuard::cleared(super::DEV_ENCODER_OVERRIDE_ENV);

        let Some(bins) = test_bins() else {
            eprintln!("SKIPPING an_unavailable_preferred_encoder_really_falls_back: no ffmpeg");
            return;
        };

        // Control: the probe is not a rubber stamp — it says YES to the shipped
        // default, so the NO below is a real answer rather than a broken probe.
        assert!(
            super::encoder_available(&bins, super::DEFAULT_VIDEO_ENCODER)
                .expect("the probe must not error"),
            "control: {} must be available, or the fall-through below proves nothing",
            super::DEFAULT_VIDEO_ENCODER
        );
        assert!(
            !super::encoder_available(&bins, "rudis_no_such_encoder")
                .expect("the probe must not error"),
            "control: a nonsense name must NOT be available"
        );

        let resolved =
            super::resolve_cleared_encoder(&bins, "test", Some("rudis_no_such_encoder"))
                .expect("an unavailable PREFERENCE must fall back, never fail");
        assert_eq!(
            resolved,
            super::DEFAULT_VIDEO_ENCODER,
            "a machine without the preferred encoder must resolve exactly what it \
             resolved before the ladder existed"
        );

        // And with no preference at all — the pre-ladder call shape.
        let resolved_none = super::resolve_cleared_encoder(&bins, "test", None)
            .expect("no preference must resolve the shipped default");
        assert_eq!(resolved_none, super::DEFAULT_VIDEO_ENCODER);
    }

    /// Whatever this machine's ladder answers, it must be one of the CLEARED
    /// names — never something a caller computed and never a GPL/patent-scoped
    /// encoder. The rule-6 statement of the ladder, at the engine boundary.
    #[test]
    fn the_preferred_encoder_ladder_only_ever_names_cleared_encoders() {
        for name in super::RENDER_CACHE_PREFERRED_ENCODERS {
            assert!(
                super::is_nvenc_video_encoder(name) || super::is_mf_video_encoder(name),
                "{name} is not one of the two cleared hardware families"
            );
            for banned in ["libx264", "libx265", "openh264", "libopenh264", "x264", "x265"] {
                assert_ne!(*name, banned, "CLAUDE.md rule 6");
            }
        }
        if let Some(resolved) = super::render_cache_preferred_encoder() {
            println!("RCENC-LADDER resolved={resolved:?}");
            assert!(
                super::RENDER_CACHE_PREFERRED_ENCODERS.contains(&resolved),
                "the ladder may only answer with a name from its own const list"
            );
        } else {
            println!("RCENC-LADDER resolved=None (falls back to the shipped default)");
        }
    }

    /// The `-scenario` door passes only a VALIDATED bare token. An env var that
    /// shapes a child's argv is a trust boundary (T-59-11-03), so anything
    /// carrying a `-`, a separator or whitespace is refused outright rather
    /// than handed to ffmpeg as a possible second option.
    #[test]
    fn the_scenario_door_emits_only_a_validated_bare_token() {
        let _serial = door_serial();
        {
            let _sc = DoorGuard::cleared(super::RENDER_CACHE_SCENARIO_ENV);
            let args = args_as_strings("h264_mf");
            assert!(
                !args.iter().any(|a| a == "-scenario"),
                "closed door: {args:?}"
            );
        }
        for good in ["archive", "live_streaming", "camera_record"] {
            let _sc = DoorGuard::set(super::RENDER_CACHE_SCENARIO_ENV, good);
            let args = args_as_strings("h264_mf");
            let i = index_of(&args, "-scenario");
            assert_eq!(args[i + 1], good, "{args:?}");
        }
        for bad in [
            "",
            "   ",
            "-c:v",
            "archive -c:v libx264",
            "h264_mf;rm",
            "../../etc",
            "a_very_long_scenario_name_that_is_over_thirty_two_chars",
        ] {
            let _sc = DoorGuard::set(super::RENDER_CACHE_SCENARIO_ENV, bad);
            let args = args_as_strings("h264_mf");
            assert!(
                !args.iter().any(|a| a == "-scenario"),
                "{bad:?} must be refused, never forwarded: {args:?}"
            );
            for banned in ["libx264", "libx265", "openh264", "libopenh264"] {
                assert!(
                    !args.iter().any(|a| a.contains(banned)),
                    "no door may smuggle `{banned}` onto the command line: {args:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Debug session `waveform-aac-priming-trim-short` (2026-08-08): the audio
// decode arg-vector pins.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod audio_decode_arg_tests {
    //! Pure arg-vector pins for the two audio-decode command lines — no spawn,
    //! no decode, no ffmpeg needed.
    //!
    //! # Why these exist AS WELL AS the real-output gate
    //!
    //! The real-output proof is
    //! `crates/engine/tests/audio_render.rs::{a_render_from_zero_spans_the_whole_
    //! source, a_render_from_zero_does_not_shift_the_audio_earlier}`, and it can
    //! only SEPARATE on the shipped sidecar. This tree contains three ffmpeg
    //! builds and only one of them exhibits the defect:
    //!
    //! | build | `-ss 0` head loss on `speech_en.mp4` |
    //! |---|---|
    //! | `runtime/binaries` `N-125907` (libavcodec 63) — **THE SHIPPED ONE** | 2048 samples / 42.67 ms |
    //! | `crates/engine/ffmpeg-dev/bin` `n8.0.1` (libavcodec 62) — **the CI runner's only sidecar** | none |
    //! | a 2023 PATH `6.1` | none |
    //!
    //! `ffi-gates.yml` fetches the middle one, so a real-output gate would have
    //! been GREEN on CI with the bug fully present — which is how this shipped in
    //! the first place. These pins have no such blind spot: they assert the
    //! command line itself, on every machine, with no media and no sidecar.

    use std::path::Path;

    fn audio_args(in_us: i64) -> Vec<String> {
        super::audio_render_args(Path::new("in.mp4"), in_us, 3_720_000, 1.0, 1.0)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn whisper_args(in_us: i64) -> Vec<String> {
        super::whisper_wav_args(Path::new("in.mp4"), in_us, 3_720_000, Path::new("out.wav"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The RULE, isolated: a zero (or clamped-to-zero) position emits NO seek
    /// terms at all, and a real position emits exactly two.
    #[test]
    fn a_zero_position_emits_no_input_seek_terms() {
        for position_us in [0i64, -1, -42_667, i64::MIN] {
            let terms: Vec<String> = super::input_seek_args(position_us)
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(
                terms.is_empty(),
                "position {position_us} must emit NO `-ss`: on the SHIPPED sidecar an input \
                 seek at zero costs a whole 1024-sample AAC frame (42.67 ms of HEAD, measured \
                 bit-exactly on speech_en.mp4), got {terms:?}"
            );
        }
        let terms: Vec<String> = super::input_seek_args(1_500_000)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            terms,
            vec!["-ss".to_string(), "1.500000".to_string()],
            "a real position keeps the seek, byte-unchanged"
        );
    }

    /// The argv both builders actually emit at position 0: no `-ss` ANYWHERE,
    /// and the rest of the line intact.
    ///
    /// Spelled as a whole-vector equality rather than a `!contains("-ss")`, so a
    /// future edit that drops the seek by also dropping `-t`, the filter chain or
    /// the sample rate fails here too.
    #[test]
    fn the_zero_offset_audio_argv_carries_no_seek_and_nothing_else_changed() {
        assert_eq!(
            audio_args(0).join(" "),
            "-v error -i in.mp4 -t 3.720000 -vn -af volume=1 -f f32le -ac 1 -ar 48000 -"
        );
        assert_eq!(
            whisper_args(0).join(" "),
            "-v error -y -i in.mp4 -t 3.720000 -vn -ar 16000 -ac 1 -c:a pcm_s16le out.wav"
        );
        // The joins above read like the command line, which is the point; this
        // keeps the ELEMENT structure pinned too, because a path folded into a
        // neighbouring argument would join to the same string (threat T-22-04,
        // "every path is a SEPARATE `.arg()`").
        for (args, path_at) in [(audio_args(0), 3usize), (whisper_args(0), 4)] {
            assert_eq!(args[path_at - 1], "-i");
            assert_eq!(args[path_at], "in.mp4", "the path is its own argv element");
        }
    }

    /// A NONZERO offset is byte-identical to what these two spawned before the
    /// fix — the whole point is that only the zero case changed.
    ///
    /// Also pins the seek's POSITION: `-ss` must sit before `-i`. After `-i` it
    /// is an output seek, which decodes from the file start and discards — a
    /// different operation with an O(position) cost.
    #[test]
    fn a_nonzero_offset_keeps_the_input_seek_immediately_before_i() {
        assert_eq!(
            audio_args(1_500_000).join(" "),
            "-v error -ss 1.500000 -i in.mp4 -t 3.720000 -vn -af volume=1 -f f32le -ac 1 \
             -ar 48000 -"
        );
        assert_eq!(
            whisper_args(1_500_000).join(" "),
            "-v error -y -ss 1.500000 -i in.mp4 -t 3.720000 -vn -ar 16000 -ac 1 -c:a pcm_s16le \
             out.wav"
        );
        for args in [audio_args(1_500_000), whisper_args(1_500_000)] {
            let ss = args.iter().position(|a| a == "-ss").expect("-ss present");
            let i = args.iter().position(|a| a == "-i").expect("-i present");
            assert_eq!(
                ss + 2,
                i,
                "`-ss <pos>` must be the last INPUT option before `-i`, not an output seek: {args:?}"
            );
        }
    }
}
