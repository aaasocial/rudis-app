//! Media import: the whole shared import-walk closure, relocated here from
//! `src-tauri/src/lib.rs` by plan 45-07.
//!
//! # Why the WHOLE closure had to move, not just the two `run_*`/`handle_*` pairs
//!
//! 45-07's brief was `run_import_media`/`handle_import_media`. Those two reach
//! `walk_import_roots` -> `walk_import_dir` -> `import_one_path` ->
//! `detect_image_sequence` / `build_sequence_item` / `claim_sequence_members` /
//! `poster_cache_dir` / `next_id` / `map_kind` / `cached_from_probe`, and that
//! same closure is ALSO called by the out-of-scope, Phase-43-added ASYNC UI
//! commands (`import_media`, `import_media_folder`, `import_media_folder_capped`
//! — none of which is one of the 45 `run_*`/`handle_*` pairs). `app-core` cannot
//! depend back on `src-tauri`, so a half-move would not compile. The UI commands
//! stayed in `src-tauri` and now delegate in here through a [`crate::AppCtx`].
//! Plan 47-03 later finished the split: the UI commands' own command-level logic
//! (the per-file loop, and the folder turn bracket that WAS
//! `import_media_folder_capped`) lives here as [`run_import_media_ui`] /
//! [`run_import_media_folder_ui`], leaving thin `#[tauri::command]` wrappers.
//!
//! # The conversion, item by item — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `fn f<R: tauri::Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` | `fn f<C: AppCtx>(ctx: &C, ..)` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first line |
//! | `app.path().app_cache_dir()` | `ctx.app_cache_dir()` |
//! | `emit_changed(app, &patch, base_seq, seq)` | `ctx.emit_patch(&patch, base_seq, seq)` |
//! | `tauri::async_runtime::spawn_blocking(..)` | `tokio::task::spawn_blocking(..)` |
//! | `tauri::async_runtime::block_on(..)` | `ctx.block_on(..)` |
//!
//! # The two runtime-primitive swaps (registered threat T-45-05)
//!
//! `tauri::async_runtime` is a THIN WRAPPER over the same Tokio runtime the app
//! already runs on, so neither swap introduces a runtime:
//!
//! * **`spawn_blocking`.** Tauri's version dispatches to its process-global
//!   runtime; `tokio::task::spawn_blocking` dispatches to the AMBIENT runtime —
//!   which, at every call path into [`import_one_path`], *is* that same global
//!   runtime (a `#[tauri::command] async fn` body is polled by
//!   `tauri::async_runtime::spawn`, and [`run_import_media`] enters it via
//!   [`crate::AppCtx::block_on`] below). Same pool, same thread. The ONE
//!   observable difference is the `Display` text of the join error in the two
//!   `eprintln!`s on the task-PANIC path (`tokio::task::JoinError` instead of
//!   `tauri::Error`), which no caller and no test reads.
//! * **`block_on`.** NOT swapped for a different primitive at all. It became the
//!   [`crate::AppCtx::block_on`] trait method, whose one production
//!   implementation (`TauriAppCtx`) is literally `tauri::async_runtime::block_on`.
//!   That is deliberate: `pollster::block_on` (this plan's first suggestion)
//!   would enter NO Tokio context, so the `spawn_blocking` calls nested inside
//!   [`walk_import_roots`] would panic; and `tokio::runtime::Handle::current()`
//!   panics outright when no runtime is entered, which is exactly the situation
//!   [`run_import_media`] runs in today (its `run_agent_turn` caller is driven by
//!   `pollster::block_on` in `src-tauri`'s own tests).
//!
//! # What did NOT change
//!
//! Every DoS clamp and ordering property is byte-identical: `MAX_FOLDER_IMPORT_
//! FILES`/`MAX_FOLDER_IMPORT_DEPTH`/`MAX_IMAGE_SEQUENCE_FRAMES`, the
//! canonicalize -> claim-check -> detect-sequence order in [`import_one_path`],
//! the symlink skip and pre-order folder registration in `walk_import_dir`, and
//! the "one bad file is a logged SKIP, never a batch abort" contract. Event
//! emission is unchanged too: the progressive one-`project:changed`-per-file
//! loop (Phase 43, D-19) stayed in `src-tauri`'s `import_media`, because that
//! loop is the COMMAND's behavior, not this closure's.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::MutexGuard;

use rudis_core::{Command, MediaBinItem, MediaKind, Patch, Store};

use crate::{probe_cache, AppCtx, SharedStore};

/// `lock` for callers that hold the `SharedStore` DIRECTLY rather than through
/// Tauri's `State` wrapper (quick task 260726-k3n).
///
/// The agent-side Pattern-C interceptions take `&SharedStore` (that is what
/// `run_agent_turn` threads through), while the `#[tauri::command]` handlers hold
/// `State<'_, SharedStore>`. `State` derefs to `SharedStore`, so a `&SharedStore`
/// signature accepts BOTH (`&store` / `store.inner()`) — which is what lets the
/// folder walk be shared by the UI command and the agent tool instead of being
/// implemented twice.
fn lock_shared(store: &SharedStore) -> Result<MutexGuard<'_, Store>, String> {
    store.lock().map_err(|_| "backend store mutex poisoned".to_string())
}

/// Monotonic suffix so ids stay unique even within one millisecond.
pub static ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Backend-generated stable ids: `{prefix}-{unix_millis}-{seq}`. Used for
/// media bin items (`media-`) and timeline clips (`clip-`).
pub fn next_id(prefix: &str) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis}-{seq}")
}

pub fn map_kind(kind: engine::MediaKind) -> MediaKind {
    match kind {
        engine::MediaKind::Video => MediaKind::Video,
        engine::MediaKind::Audio => MediaKind::Audio,
        engine::MediaKind::Image => MediaKind::Image,
    }
}

/// Phase 43 (LAT-08): project a fresh [`engine::MediaInfo`] down to the EXACT
/// fields the import path consumes — eight for the [`MediaBinItem`] it builds,
/// plus (Phase 58, plan 58-05) `bit_rate` and `vcodec`, which no bin item
/// carries and which exist purely so the proxy trigger below can be decided
/// from the probe already in hand.
///
/// This is the ONE mapping site: both `engine::probe` call sites in this crate
/// (`import_one_path`'s UI batch path and `run_import_media`'s agent-tool
/// single-file path) build their item from a [`probe_cache::CachedProbeResult`],
/// whether it came from the cache or from a fresh probe — so a cache HIT and a
/// cache MISS cannot produce different metadata for the same file, and the two
/// surfaces cannot drift apart. That property is exactly why the two proxy
/// inputs go through here rather than being read off a `MediaInfo` on one path
/// only: a cached import must reach the same proxy decision as a cold one.
fn cached_from_probe(info: &engine::MediaInfo) -> probe_cache::CachedProbeResult {
    probe_cache::CachedProbeResult {
        media_kind: map_kind(info.media_kind),
        duration_us: info.duration_us,
        width: info.width,
        height: info.height,
        avg_frame_rate: info.avg_frame_rate,
        is_vfr: info.is_vfr,
        rotation_degrees: info.rotation_degrees,
        has_audio: info.has_audio,
        bit_rate: info.bit_rate,
        vcodec: info.vcodec.clone(),
        has_alpha: probed_alpha(info),
    }
}

/// Phase 60 (OCCL-01, plan 60-06): narrow `engine::MediaInfo::has_alpha` — a
/// plain `bool` — into the TRI-STATE
/// [`rudis_core::MediaBinItem::reports_alpha`] carries.
///
/// # Why a narrowing is needed at all
///
/// The engine's `has_alpha` is `false` in three materially different
/// situations, because the flag exists there for a different job (deciding
/// whether to force `-c:v libvpx-vp9`, where guessing "no" costs nothing):
///
/// 1. a video stream with a readable pixel format that has no alpha channel —
///    genuinely opaque, and the ONLY one of the three that is evidence;
/// 2. a video stream whose `pix_fmt` ffprobe did not report — nothing was read,
///    so nothing is known;
/// 3. no video stream at all (audio-only) — there is no visual surface to have
///    an opinion about.
///
/// The occlusion predicate reads this to decide whether a layer may HIDE what
/// is behind it. A `false` from case 2 or 3, taken as "opaque", would cull a
/// layer on the strength of an absence of data — and if that layer really did
/// carry alpha, the culled frame differs from the un-culled one, which is
/// OCCL-01's stated failure mode. So cases 2 and 3 become `None` ("unknown",
/// structurally never cullable) and only case 1 becomes `Some(false)`.
///
/// A positive finding is definitive and needs no such care: `has_alpha == true`
/// comes either from an alpha-carrying pixel format or from the WebM
/// `alpha_mode` side-channel tag, and both are evidence.
///
/// Deliberately narrowed HERE rather than in `crates/engine`: the engine's flag
/// has a second, older consumer whose meaning must not shift under it, and this
/// crate is where the field's occlusion meaning is defined.
pub fn probed_alpha(info: &engine::MediaInfo) -> Option<bool> {
    if info.has_alpha {
        return Some(true); // a positive finding is evidence in itself
    }
    match info.media_kind {
        // No video stream was examined, so `false` means "not asked".
        engine::MediaKind::Audio => None,
        // A video/image stream with no reported pixel format: the alpha test
        // had nothing to run on. Unknown, never "opaque".
        _ if info.pix_fmt.is_none() => None,
        _ => Some(false),
    }
}

/// Phase 43 (LAT-08): the poster timestamp policy, shared by both import
/// surfaces — video takes a mid-duration frame, an image takes t=0, and audio
/// never gets here (the caller skips posters for audio entirely).
fn poster_at_seconds(kind: MediaKind, duration_us: i64) -> f64 {
    if kind == MediaKind::Video {
        (duration_us as f64 / 2_000_000.0).max(0.0)
    } else {
        0.0
    }
}

/// Phase 28 (OVL-01, T-28-13): hard cap on how many frames one imported image
/// sequence may enumerate. Mirrors the `MAX_TEXT_BATCH` / `MAX_ORGANIZE_MEDIA_
/// BATCH` DoS-cap precedent (`crates/core/src/tools.rs`): a numbered set larger
/// than this is REJECTED before ffmpeg is ever spawned on the pattern
/// (unbounded-glob DoS guard).
pub const MAX_IMAGE_SEQUENCE_FRAMES: usize = 10_000;

/// A numbered image sequence detected from the user's first-file selection,
/// resolved ENTIRELY from server-validated existing sibling files (T-28-12
/// path-traversal guard: NO raw user pattern is ever accepted — the `%0Nd`
/// pattern is rebuilt from the common prefix/suffix of real files that live in
/// the selected file's own canonicalized parent directory).
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedSequence {
    /// The CONFINED image2 pattern path (e.g. `.../frame_%04d.png`) — a plain
    /// filename joined to the canonical parent dir, never a user string.
    pub pattern: PathBuf,
    /// Count of consecutive existing frames from the first detected number.
    pub frame_count: usize,
    /// The first real frame file (probed for pixel dims + used for the poster).
    pub first_frame: PathBuf,
}

/// Split a basename into `(prefix, digit_width, start_number, suffix)` when it
/// ends — before any file extension — in a contiguous run of ASCII digits, e.g.
/// `frame_0001.png` → `("frame_", 4, 1, ".png")`. Returns `None` when there is
/// no trailing numeric run (not a numbered file → not a sequence).
pub fn split_numbered_basename(basename: &str) -> Option<(String, usize, u64, String)> {
    // Suffix = the file extension from the LAST '.' (if any, and not a leading
    // dot); the numeric run sits immediately before it.
    let (stem, suffix) = match basename.rfind('.') {
        Some(dot) if dot > 0 => (&basename[..dot], &basename[dot..]),
        _ => (basename, ""),
    };
    // Walk back over the trailing ASCII-digit run.
    let digits_start = stem
        .bytes()
        .rposition(|b| !b.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if digits_start >= stem.len() {
        return None; // no trailing digits
    }
    let digits = &stem[digits_start..];
    let width = digits.len();
    let start = digits.parse::<u64>().ok()?;
    Some((stem[..digits_start].to_string(), width, start, suffix.to_string()))
}

/// Detect whether `first_file` (an existing, canonicalized path) is the first
/// frame of a numbered image sequence and, if so, build a CONFINED image2
/// `%0Nd` pattern from a server-validated common prefix/suffix over its REAL
/// existing siblings. `cap` bounds enumeration (T-28-13). Returns:
///   * `Ok(None)`   — not a numbered set, NOT A STILL IMAGE (a numbered video /
///     audio / unprobeable file), or a lone still → normal single-file import,
///   * `Ok(Some(_))`— a ≥2-frame confined sequence,
///   * `Err(_)`     — the run exceeds `cap`, or a member escapes the parent dir.
pub fn detect_image_sequence_capped(
    first_file: &Path,
    cap: usize,
) -> Result<Option<DetectedSequence>, String> {
    let Some(parent) = first_file.parent() else {
        return Ok(None);
    };
    let Some(basename) = first_file.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    let Some((prefix, width, start, suffix)) = split_numbered_basename(basename) else {
        return Ok(None);
    };

    // MEDIA-TYPE GATE: a sequence frame must itself be a STILL IMAGE. Without
    // this, EVERY numbered file qualified, so a folder of numbered VIDEO clips
    // (`1.mp4` .. `14.mp4` — an extremely common footage convention) collapsed
    // into ONE bogus "image sequence" whose `%01d.mp4` pattern the image2
    // demuxer resolves to `codec_name=unknown, width=0, height=0` with NO
    // decoder at all -> a silently BLACK clip and poster, no error surfaced.
    //
    // The classifier is `engine::probe`'s EXISTING `MediaKind` — the single
    // source of truth already used by the import path below, decided from the
    // ffprobe DEMUXER (a true single-file still demuxes via a `*_pipe` demuxer,
    // or via `image2` with NO `%0Nd` token under a Windows verbatim path).
    // Deliberately NOT a hand-written extension list: a deny-list would rot the
    // moment someone imports `1.mov`/`1.mkv`/`1.webm`/`1.avi`/`1.wav`, and a
    // hard-coded allow-list would rot on every still format ffmpeg gains.
    //
    // This gate — NOT the digit width — is the whole fix. Rust's `{:0width$}`
    // is a MINIMUM width, and that is CORRECT: it mirrors ffmpeg image2's own
    // `%0Nd` matching (measured: `ffprobe %01d.png` over `1.png`..`12.png`
    // reports nb_read_frames=12; `%02d.png` over `01.png`..`100.png` reports
    // 100). Tightening the counter to an exact width while still emitting the
    // same pattern would make `frame_count` DISAGREE with what ffmpeg actually
    // decodes and corrupt `duration_us = frame_count / fps`.
    //
    // Placed BEFORE enumeration on purpose: the frame cap below returns `Err`,
    // which `import_one_path` turns into a per-file SKIP, so letting a non-image
    // type reach it would silently DROP a >`cap` numbered video set instead of
    // importing it. Cost is one ffprobe per numbered file whose type we must
    // establish; the import path immediately below probes anyway, and poster
    // generation dominates either way. An unprobeable file returns `Ok(None)`
    // and falls through to the ordinary single-file path, which surfaces the
    // probe error itself rather than duplicating it here.
    match engine::probe(first_file) {
        Ok(info) if info.media_kind == engine::MediaKind::Image => {}
        // A real video/audio container, or a file we cannot classify: never a
        // sequence frame. Fall through to ordinary single-file import.
        _ => return Ok(None),
    }

    // The confinement boundary: every enumerated member MUST canonicalize to a
    // real file whose parent is EXACTLY this directory (T-28-12).
    let canon_parent = std::fs::canonicalize(parent)
        .map_err(|e| format!("cannot canonicalize sequence dir {}: {e}", parent.display()))?;

    let mut frame_count = 0usize;
    let mut n = start;
    loop {
        // A PURE filename (prefix + zero-padded digits + suffix) — the digits are
        // formatted, never user text, so this can carry no path separator.
        let name = format!("{}{:0width$}{}", prefix, n, suffix, width = width);
        let candidate = canon_parent.join(&name);
        if !candidate.is_file() {
            break; // first gap ends the run (bounded, no wildcard glob)
        }
        // Defense-in-depth: re-canonicalize the member and reject anything that
        // escapes the selected file's directory (crafted symlink / `..`).
        let canon = std::fs::canonicalize(&candidate)
            .map_err(|e| format!("cannot canonicalize sequence frame {}: {e}", candidate.display()))?;
        if canon.parent() != Some(canon_parent.as_path()) {
            return Err(format!(
                "image sequence frame {} escapes the import directory",
                candidate.display()
            ));
        }
        frame_count += 1;
        if frame_count > cap {
            return Err(format!(
                "image sequence exceeds the {cap}-frame import cap (DoS guard)"
            ));
        }
        n += 1;
    }

    // A lone first frame with no consecutive sibling is an ordinary still, not a
    // sequence — fall through to single-file import.
    if frame_count < 2 {
        return Ok(None);
    }

    let pattern = canon_parent.join(format!("{}%0{}d{}", prefix, width, suffix));
    let first_frame = canon_parent.join(format!("{}{:0width$}{}", prefix, start, suffix, width = width));
    Ok(Some(DetectedSequence { pattern, frame_count, first_frame }))
}

/// Production wrapper: [`detect_image_sequence_capped`] at the real
/// [`MAX_IMAGE_SEQUENCE_FRAMES`] cap.
pub fn detect_image_sequence(first_file: &Path) -> Result<Option<DetectedSequence>, String> {
    detect_image_sequence_capped(first_file, MAX_IMAGE_SEQUENCE_FRAMES)
}

/// Build the single [`MediaBinItem`] for a detected numbered image sequence:
/// `media_kind = Video` (rides the whole existing placement/decode/export
/// path), `is_image_sequence = true`, `fps = project fps` (COMP-01 timebase,
/// Q2 resolved default), `duration_us = round(frame_count / fps)` and pixel
/// dims from the REAL first frame. The `path` is the confined `%0Nd` pattern.
pub fn build_sequence_item(
    ctx: &impl AppCtx,
    poster_dir: &Path,
    seq: &DetectedSequence,
    project_fps: f64,
) -> Result<MediaBinItem, String> {
    let _ = ctx; // poster generation needs only the cache dir + first frame
    let fps = if project_fps.is_finite() && project_fps > 0.0 {
        project_fps
    } else {
        30.0
    };
    // Pixel dims from the REAL first frame (a single PNG probes cleanly). A
    // corrupt/undecodable first frame fails here via the sidecar Result path
    // (T-28-14), never a panic.
    let info = engine::probe(&seq.first_frame)
        .map_err(|e| format!("probe first sequence frame {}: {e}", seq.first_frame.display()))?;
    if info.width == 0 || info.height == 0 {
        return Err(format!(
            "sequence first frame has zero dimensions: {}",
            seq.first_frame.display()
        ));
    }
    let duration_us = ((seq.frame_count as f64 / fps) * 1_000_000.0).round() as i64;
    let id = next_id("media");
    let poster_path = {
        let out = poster_dir.join(format!("{id}.png"));
        match engine::generate_poster(&seq.first_frame, &out, 0.0) {
            Ok(()) => Some(out.to_string_lossy().into_owned()),
            Err(e) => {
                eprintln!(
                    "import_media: no poster for sequence {}: {e}",
                    seq.pattern.display()
                );
                None
            }
        }
    };
    Ok(MediaBinItem {
        id,
        path: seq.pattern.to_string_lossy().into_owned(),
        media_kind: MediaKind::Video,
        duration_us,
        width: info.width,
        height: info.height,
        fps,
        is_vfr: false,
        rotation_degrees: 0,
        has_audio: false,
        poster_path,
        folder: String::new(),
        display_name: None,
        is_image_sequence: true,
        // Phase 60 (OCCL-01): UNKNOWN, deliberately. `path` here is an image2
        // `%0Nd` PATTERN, not one media file — `info` above probed the pattern
        // as a synthetic container, which says nothing dependable about the
        // per-frame alpha of the SET, and the frames need not agree with each
        // other. The occlusion predicate excludes sequences from its scope
        // anyway (research Pitfall 6), so `None` costs nothing and a guessed
        // `Some(false)` would risk culling behind a transparent PNG run.
        reports_alpha: None,
    })
}

/// Resolve (and create) the poster cache dir — `app_cache_dir()/posters`, the
/// ONE canonical poster location.
///
/// ORIGIN (history, not a live constraint): under the retired Tauri/WebView
/// shell this was a SANDBOX rule — `tauri.conf.json`'s `assetProtocol.scope`
/// allowlisted `$APPCACHE/posters/**`, so a poster written anywhere else (e.g.
/// beside a generated asset under `$APPDATA/<identifier>/generated`) was blocked
/// by the asset protocol and the MediaBin `<img>` never rendered (live-UAT
/// MEDIABIN-STILL-THUMBNAIL fix). The native WinUI 3 shell has no such sandbox
/// and opens the stored path directly — see D-06 in
/// `shell/Rudis.Shell/Regions/MediaBin/MediaBinPoster.cs`, which states
/// deliberately that NO allowlist is re-implemented shell-side.
///
/// The convention still stands and every handler that mints a `poster_path`
/// must still write through this dir — one predictable, cache-cleanable home
/// for derived thumbnails — but it is now a convention, not a sandbox gate.
/// The asset file itself may live elsewhere.
pub fn poster_cache_dir(ctx: &impl AppCtx) -> Result<PathBuf, String> {
    let dir = ctx.app_cache_dir()?.join("posters");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create poster cache dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Resolve (and create) the waveform peak cache dir — `app_cache_dir()/waveforms`
/// (Phase 52, SHELL-09 / D-18), the WRITE-path twin of [`poster_cache_dir`]
/// above and the one location both the import-time extraction job and the FFI
/// read path use.
///
/// The directory NAME is `waveform::cache::WAVEFORM_CACHE_DIR_NAME`, not a
/// string literal, so the producer crate owns it; the join itself lives in
/// [`crate::waveform_job::waveform_dir`], which this wraps with the
/// `create_dir_all`. Unlike a poster, a peak file is never served to a
/// renderer over an asset protocol — it crosses the C ABI as base64 inside a
/// command envelope — so no scope allowlist constrains where it may live.
pub fn waveform_cache_dir(ctx: &impl AppCtx) -> Result<PathBuf, String> {
    let dir = crate::waveform_job::waveform_dir(ctx)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create waveform cache dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Resolve (and create) the filmstrip strip cache dir — `app_cache_dir()/filmstrips`
/// (Phase 53.2, D-11), the third member of the family above and the one location
/// both the import-time extraction job and the FFI read path use.
///
/// Same construction as [`waveform_cache_dir`], for the same reason: the
/// directory NAME is `filmstrip::cache::FILMSTRIP_CACHE_DIR_NAME`, not a string
/// literal, so the producer crate owns it, and the join itself lives in
/// [`crate::filmstrip_job::filmstrip_dir`], which this wraps with the
/// `create_dir_all` the READ path must not have. Like a peak file and unlike a
/// poster, a strip is never served to a renderer over an asset protocol — it
/// crosses the C ABI as base64 inside a command envelope — so no scope allowlist
/// constrains where it may live.
pub fn filmstrip_cache_dir(ctx: &impl AppCtx) -> Result<PathBuf, String> {
    let dir = crate::filmstrip_job::filmstrip_dir(ctx)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create filmstrip cache dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Import ONE real path into the MediaBin at virtual folder `folder`, returning
/// the created item together with the `Patch` its dispatch produced.
///
/// The single per-file import path shared by `import_media` (one call per
/// selected file, `folder = ""`, event emitted per file) and
/// `import_media_folder` (one call per walked file, `folder` = the mirrored
/// virtual path, ONE event for the whole gesture).
///
/// * `Ok(Some(..))` — imported (caller decides whether/when to emit).
/// * `Ok(None)`     — LOGGED SKIP: unresolvable path, an already-claimed image
///   sequence member, a rejected/failed sequence detection, an unprobeable
///   file, or a rejected dispatch. A skip NEVER aborts the caller's batch.
/// * `Err(..)`      — only a poisoned store mutex (unrecoverable, surfaced).
///
/// Lock discipline (lib.rs invariant): `SharedStore` is locked ONLY for the
/// short synchronous `dispatch` span — NEVER across the `engine::probe` /
/// `engine::generate_poster` subprocess spawns above it. Phase 43 (LAT-08)
/// hardens that further: those two spawns now run on Tokio's blocking pool and
/// are `.await`ed, so no store guard can even be alive at that point (a guard
/// held across an `.await` would make this future non-`Send` and fail to
/// compile — the invariant is now enforced by the type system, not by review).
///
/// Phase 43 (LAT-08): `probe_cache` is the caller's once-per-batch map. A HIT
/// on `(canonical path, mtime, size)` skips the `ffprobe` subprocess entirely;
/// a MISS probes fresh and inserts the result. The poster is NEVER cached — it
/// is written to a path derived from the freshly-minted item id.
pub async fn import_one_path<C: AppCtx>(
    ctx: &C,
    poster_dir: &Path,
    project_fps: f64,
    raw_path: &str,
    folder: &str,
    claimed: &mut std::collections::HashSet<PathBuf>,
    probe_cache: &mut probe_cache::ProbeCache,
    // Phase 43 (LAT-02): this is a CARRIER — it dispatches internally but its
    // CALLER is the one that calls `emit_changed`, so the `(base_seq, seq)`
    // pair captured inside `Store::dispatch` has to ride all the way up
    // rather than being re-read from a second store lock (T-43-02-01).
) -> Result<Option<(MediaBinItem, Patch, u64, u64)>, String> {
    let store = ctx.store();
    // Canonicalize to a REAL absolute path; a dangling path is a per-file
    // failure, not a batch failure.
    let abs = match std::fs::canonicalize(raw_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("import_media: skipping {raw_path}: {e}");
            return Ok(None);
        }
    };

    // ---- Sequence-member claiming (quick task 260726-hgu) -------------------
    // This check MUST happen HERE — immediately after canonicalization and
    // BEFORE `detect_image_sequence` is ever called. `detect_image_sequence`
    // scans FORWARD ONLY from the handed file's number, so a run's LAST member
    // yields `frame_count = 1 < 2` -> `Ok(None)` and would fall through to a
    // standalone still import without ever redetecting the pattern. Comparing
    // patterns AFTER detection therefore cannot dedupe; claiming member PATHS
    // before detection is the one mechanism that does.
    if claimed.contains(&abs) {
        eprintln!(
            "import_media: skipping {} (already consumed by an imported image sequence)",
            abs.display()
        );
        return Ok(None);
    }

    // Phase 28 (OVL-01, SC-4): a numbered first-file selection (e.g.
    // frame_0001.png) imports as ONE animated sequence clip, not N separate
    // stills. Detection + the %0Nd pattern are server-validated (confined to
    // the file's own canonical dir, frame-capped) — see
    // `detect_image_sequence`. Non-numbered files fall through to the normal
    // single-file import below.
    match detect_image_sequence(&abs) {
        Ok(Some(seq)) => {
            let mut item = match build_sequence_item(ctx, poster_dir, &seq, project_fps) {
                Ok(it) => it,
                Err(e) => {
                    eprintln!("import_media: sequence import failed for {}: {e}", abs.display());
                    return Ok(None);
                }
            };
            item.folder = folder.to_string();
            let (patch, base_seq, dispatch_seq) =
                match lock_shared(store)?.dispatch(Command::AddMediaBinItem(item.clone())) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("import_media: dispatch failed for sequence {}: {e}", item.path);
                        return Ok(None);
                    }
                };
            // Only a SUCCESSFUL import claims: a failed one leaves its members
            // free for a later (e.g. retried) attempt.
            claim_sequence_members(&seq, claimed);
            return Ok(Some((item, patch, base_seq, dispatch_seq)));
        }
        Ok(None) => {} // not a numbered sequence: normal single-file import
        Err(e) => {
            // A confinement/DoS-cap rejection: skip this file, never abort
            // the batch, never spawn ffmpeg on an unbounded/escaping pattern.
            eprintln!("import_media: rejected image sequence {}: {e}", abs.display());
            return Ok(None);
        }
    }

    // Phase 43 (LAT-08): consult the cache BEFORE spawning ffprobe. `key_for`
    // returning `None` (an unreadable stat/timestamp) means "not cacheable" —
    // probe fresh and do not record the result. It is never an import failure:
    // the cache must stay an optimization, never a correctness dependency.
    let key = probe_cache::key_for(&abs);
    let probed = match key.as_ref().and_then(|k| probe_cache.get(k)).cloned() {
        Some(hit) => hit,
        None => {
            let abs_for_probe = abs.clone();
            let joined =
                tokio::task::spawn_blocking(move || engine::probe(&abs_for_probe)).await;
            let fresh = match joined {
                Ok(Ok(info)) => info,
                Ok(Err(e)) => {
                    eprintln!("import_media: skipping {}: {e}", abs.display());
                    return Ok(None);
                }
                Err(e) => {
                    eprintln!("import_media: probe task panicked for {}: {e}", abs.display());
                    return Ok(None);
                }
            };
            let mapped = cached_from_probe(&fresh);
            if let Some(k) = key {
                probe_cache.insert(k, mapped.clone());
            }
            mapped
        }
    };

    let id = next_id("media");

    // Poster policy: video -> frame at mid-duration; image -> the image
    // itself (downscaled); audio -> none (renderer draws a distinct
    // audio tile). Poster failure downgrades to None, never aborts.
    let poster_path = if probed.media_kind == MediaKind::Audio {
        None
    } else {
        let at_seconds = poster_at_seconds(probed.media_kind, probed.duration_us);
        let out = poster_dir.join(format!("{id}.png"));
        let abs_for_poster = abs.clone();
        let out_for_poster = out.clone();
        let joined = tokio::task::spawn_blocking(move || {
            engine::generate_poster(&abs_for_poster, &out_for_poster, at_seconds)
        })
        .await;
        match joined {
            Ok(Ok(())) => Some(out.to_string_lossy().into_owned()),
            Ok(Err(e)) => {
                eprintln!("import_media: no poster for {}: {e}", abs.display());
                None
            }
            Err(e) => {
                eprintln!(
                    "import_media: poster task panicked for {}: {e}",
                    abs.display()
                );
                None
            }
        }
    };

    // Phase 52 (SHELL-09, D-19): the audio peak envelope is produced by a
    // BACKGROUND job, parallel to the poster block above — but DETACHED, and
    // that difference is the whole point. `spawn_extraction` returns
    // immediately and the import path never suspends on it, so the item below is
    // dispatched (and the caller's `project:changed` pushed) without waiting
    // for a decode that costs ~11 s and ~134 MiB per hour of audio. The item
    // appears in the bin and on the timeline at once; its waveform fills in
    // later, read back through `rudis_get_waveform_peaks` on the shell's
    // existing 100 ms cold-path poll.
    //
    // D-20: gated on `has_audio`, NOT on `media_kind == Audio`. An
    // audio-bearing VIDEO clip needs a waveform fill too.
    //
    // The cache dir is resolved HERE, from the `ctx` this function already
    // holds, rather than being threaded in as a parameter beside `poster_dir`:
    // `import_one_path` has exactly two real call sites and widening its
    // signature for an idempotent `create_dir_all` that costs nothing next to
    // the `ffprobe` spawn above is churn for nothing.
    //
    // Lock discipline (this function's stated invariant): no `SharedStore`
    // guard is alive at this point, and the spawn takes OWNED values only — no
    // `ctx`, no store, no borrow — so it adds NO SUSPENSION POINT to the import
    // path and cannot extend any guard's lifetime. (Said that way on purpose:
    // this plan's acceptance gate counts occurrences of the suspension
    // operator in this file to prove the trigger added none, and naming it in
    // a comment explaining that it is absent would blunt the very check —
    // 52-02's recorded rule about use-detector greps.)
    if probed.has_audio {
        match waveform_cache_dir(ctx) {
            Ok(waveform_dir) => {
                crate::waveform_job::spawn_extraction(waveform_dir, abs.clone(), probed.duration_us)
            }
            // Best-effort, exactly like the poster: no waveform is a redraw
            // with no fill, never a failed import.
            Err(e) => eprintln!("import_media: no waveform peaks for {}: {e}", abs.display()),
        }
    }

    // Phase 53.2 (D-09): filmstrip extraction — the SAME detached shape as the
    // peaks block above, for the same reason, hooked in beside the poster and the
    // peaks rather than anywhere else. `spawn_extraction` returns immediately, so
    // the item below is dispatched (and the caller's `project:changed` pushed)
    // without waiting for a decode that walks the whole file.
    //
    // D-15: gated on `media_kind == Video`. An audio-only source and a still
    // image both keep D-13's poster placeholder PERMANENTLY — for a still that is
    // the correct terminal state rather than a failure, because its poster
    // already IS its filmstrip. Note the contrast with the peaks gate directly
    // above, which is `has_audio`: the two pipelines fill two different halves of
    // a clip and their gates are genuinely different predicates, not a copy that
    // drifted.
    //
    // `frame_step_us` is derived from the probe's `avg_frame_rate` because
    // `crates/filmstrip` deliberately never probes again; a wrong step costs a
    // slightly-off frame CHOICE inside a chunk, never a panic or an
    // out-of-range read, so the 33_333 fallback (30fps) is a safe default for a
    // container that reports no rate at all.
    //
    // Lock discipline, exactly as stated for the peaks block: no `SharedStore`
    // guard is alive here and the spawn takes OWNED values only, so this adds NO
    // SUSPENSION POINT to the import path.
    if probed.media_kind == MediaKind::Video {
        match filmstrip_cache_dir(ctx) {
            Ok(dir) => {
                let step = if probed.avg_frame_rate > 0.0 {
                    (1_000_000.0 / probed.avg_frame_rate) as i64
                } else {
                    33_333
                };
                crate::filmstrip_job::spawn_extraction(
                    dir,
                    abs.clone(),
                    probed.duration_us,
                    probed.width,
                    probed.height,
                    probed.rotation_degrees,
                    step,
                )
            }
            // Best-effort, exactly like the poster and the peaks: no filmstrip is
            // a clip drawn with its placeholder, never a failed import.
            Err(e) => eprintln!("import_media: no filmstrip for {}: {e}", abs.display()),
        }
    }

    // Phase 58 (PROXY-02, D-07): the playback proxy — the FOURTH detached
    // background job on this path, and the same shape as the three above for the
    // same reasons.
    //
    // D-07: AUTOMATIC and PREDICATE-GATED. There is no prompt, no codec
    // question, no quality ladder and no place for the word "proxy" to reach a
    // beginner. The heaviness predicate below is a pure function of numbers this
    // probe already produced, so a source that fails it costs exactly one
    // comparison — a 720p clip pays nothing at all — while a 4K Long-GOP source
    // gets a proxy without anyone deciding anything. (Named only in the call
    // itself, deliberately: this plan's acceptance gate counts occurrences of
    // that identifier in this file to prove there is exactly ONE gate, and a
    // comment echoing it would blunt the check — 52-02's recorded rule about
    // use-detector greps, now five phases old.)
    //
    // D-09: DETACHED. `spawn_generation` returns immediately, so the item below
    // is dispatched (and the caller's `project:changed` pushed) without waiting
    // for a transcode that costs seconds. The timeline simply starts resolving
    // the proxy on its NEXT resolve; nothing is invalidated and nothing waits.
    //
    // The `configure_proxy_cache_dir` call beside the spawn is not redundant
    // with `transport::run_transport`'s: it makes a proxy generated during THIS
    // session usable without a transport command having happened first. Both
    // calls are idempotent overwrites of one process-global, by design.
    //
    // Lock discipline, exactly as stated for the peaks and filmstrip blocks: no
    // `SharedStore` guard is alive here and the spawn takes OWNED values only,
    // so this adds NO SUSPENSION POINT to the import path.
    if probed.media_kind == MediaKind::Video
        && proxy::heaviness::needs_proxy(
            probed.width,
            probed.height,
            probed.bit_rate,
            probed.vcodec.as_deref(),
        )
    {
        match crate::proxy_job::proxy_dir(ctx) {
            Ok(dir) => {
                preview::decode_source::configure_proxy_cache_dir(dir.clone());
                crate::proxy_job::spawn_generation(dir, abs.clone());
            }
            // Best-effort, exactly like the poster, the peaks and the filmstrip:
            // no proxy is a clip that plays from its original (D-17), never a
            // failed import.
            Err(e) => eprintln!("import_media: no proxy for {}: {e}", abs.display()),
        }
    }

    let item = MediaBinItem {
        id,
        path: abs.to_string_lossy().into_owned(),
        media_kind: probed.media_kind,
        duration_us: probed.duration_us,
        width: probed.width,
        height: probed.height,
        fps: probed.avg_frame_rate,
        is_vfr: probed.is_vfr,
        rotation_degrees: probed.rotation_degrees,
        has_audio: probed.has_audio,
        poster_path,
        folder: folder.to_string(),
        display_name: None,
        is_image_sequence: false,
        // Phase 60 (OCCL-01): straight off the ONE mapping (`cached_from_probe`
        // -> `probed_alpha`), so a cache HIT and a cold probe agree.
        reports_alpha: probed.has_alpha,
    };

    // Backend-owned + undoable: the finished item goes through the same
    // dispatch path as every other mutation.
    let (patch, base_seq, seq) =
        match lock_shared(store)?.dispatch(Command::AddMediaBinItem(item.clone())) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("import_media: dispatch failed for {}: {e}", item.path);
                return Ok(None);
            }
        };
    Ok(Some((item, patch, base_seq, seq)))
}

/// Mark EVERY real file consumed by a just-imported [`DetectedSequence`] as
/// claimed, so a later file handed to [`import_one_path`] in the SAME gesture
/// can never be re-imported (neither as a redetected sequence nor — the case
/// pattern-comparison misses — as a degraded standalone still).
///
/// The member paths are reconstructed with the SAME formatting the detector
/// itself enumerates with: `split_numbered_basename(seq.first_frame)` gives
/// `(prefix, width, start, suffix)` and the parent is taken from
/// `seq.first_frame.parent()` — which is CANONICAL BY CONSTRUCTION (the
/// detector built `first_frame` by joining its own `canonicalize`d parent).
/// Taking the parent from anywhere else (the raw user path, or a separate
/// `canonicalize` call) would produce keys that silently fail to match on
/// Windows (`\\?\` prefixing / case) and the dedupe would quietly no-op.
///
/// Documented consequence: a GAPPED run (`0001, 0002, 0005`) claims only the
/// contiguous run it actually imported, so `0005` still imports as its own
/// sequence/still — exactly what single-file imports of each run's first frame
/// do today.
fn claim_sequence_members(seq: &DetectedSequence, claimed: &mut std::collections::HashSet<PathBuf>) {
    let Some(parent) = seq.first_frame.parent() else {
        return;
    };
    let Some(basename) = seq.first_frame.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let Some((prefix, width, start, suffix)) = split_numbered_basename(basename) else {
        return;
    };
    for n in start..start.saturating_add(seq.frame_count as u64) {
        claimed.insert(parent.join(format!("{}{:0width$}{}", prefix, n, suffix, width = width)));
    }
}

/// Server-side cap on how many files ONE folder-import gesture may ATTEMPT
/// (T-hgu-02). The DoS resource is the ffprobe/ffmpeg subprocess spawn per
/// file, so the cap counts ATTEMPTS, not successes. Reaching it CLAMPS the walk
/// (logged) — it never turns the gesture into an error.
pub const MAX_FOLDER_IMPORT_FILES: usize = 500;

/// Server-side cap on how deep ONE folder-import gesture may descend
/// (T-hgu-02). The SELECTED root dir is depth 0, its child dirs depth 1; a dir
/// whose depth would EXCEED this is skipped whole (subtree AND registry entry),
/// logged.
pub const MAX_FOLDER_IMPORT_DEPTH: usize = 12;

/// Map one REAL disk directory name to its candidate VIRTUAL folder path under
/// `parent_virtual` ("" = library root), or `None` when the result would not be
/// a legal virtual path.
///
/// Threat T-hgu-01: the name comes from untrusted disk, so it is pre-validated
/// with the SAME authoritative reject list the Command layer applies
/// ([`rudis_core::is_valid_folder_path`]) — colon (drive letter / NTFS ADS),
/// `.`/`..`, separators, control chars, the 255-byte segment cap and the
/// 2048-byte whole-path cap. `None` means SKIP-WITH-LOG at the call site.
///
/// Sanitizing was rejected deliberately: a renamed segment can collide with a
/// sibling and the bin would no longer MIRROR disk. Only DIRECTORY names become
/// virtual segments — a file keeps its real disk path in `MediaBinItem.path`
/// and needs no mapping.
pub fn child_virtual_path(parent_virtual: &str, name: &std::ffi::OsStr) -> Option<String> {
    let decoded = name.to_string_lossy();

    // A directory name is exactly ONE virtual segment. Checking only the JOINED
    // path is NOT sufficient: `is_valid_folder_path` accepts "a/b" as a legal
    // MULTI-segment path, so a name carrying a separator would silently expand
    // one disk directory into a NESTED virtual path (a path-injection under
    // T-hgu-01). Reject the separator here, then validate the name on its own —
    // a single-segment path IS the segment rule (non-empty, <= 255 bytes, not
    // "."/"..", no colon, no control characters).
    if decoded.contains('/') || decoded.contains('\\') {
        return None;
    }
    if !rudis_core::is_valid_folder_path(&decoded, false) {
        return None;
    }

    let joined = if parent_virtual.is_empty() {
        decoded.into_owned()
    } else {
        format!("{parent_virtual}/{decoded}")
    };
    // The JOINED path must be legal too — this is what enforces the whole-path
    // length cap as the walk descends (and re-checks the inherited parent).
    if rudis_core::is_valid_folder_path(&joined, false) {
        Some(joined)
    } else {
        None
    }
}

/// Mutable state threaded through one folder-import walk.
///
/// Plan 45-07: `pub` with exactly TWO `pub` fields, because the UI folder
/// bracket (then `src-tauri`'s `import_media_folder_capped`, since 47-03
/// [`run_import_media_folder_ui`] in here) reads `imported` and `last_patch`
/// off the value [`walk_import_roots`] returns to it. Every other field is
/// written only in here and stays private — that is the minimum widening, and
/// 45-14's re-privatization worklist needs only these two names.
pub struct FolderWalk {
    poster_dir: PathBuf,
    project_fps: f64,
    max_files: usize,
    max_depth: usize,
    /// Every virtual folder path known to EXIST: seeded from the project
    /// snapshot (so a pre-existing name MERGES) and grown as the walk creates
    /// more.
    known_folders: std::collections::HashSet<String>,
    /// Real file paths consumed by an already-imported image sequence.
    claimed: std::collections::HashSet<PathBuf>,
    /// Files HANDED to `import_one_path` so far (the capped resource).
    attempts: usize,
    cap_logged: bool,
    pub imported: Vec<MediaBinItem>,
    /// Virtual folders this walk actually CREATED, in creation (pre-)order — a
    /// pre-existing name MERGES and is therefore absent. Additive (quick task
    /// 260726-k3n): the agent tool_result reports what was mirrored so the model
    /// can name the new folders back to the user; the UI path ignores it.
    created_folders: Vec<String>,
    /// The most recent successful dispatch's patch — the payload of the ONE
    /// `project:changed` emitted after `end_turn`.
    ///
    /// Phase 43 (LAT-02): carries that dispatch's own `(base_seq, seq)`.
    /// A folder walk deliberately emits ONE event for MANY mutations
    /// (T-hgu-04), so this envelope's `base_seq` will NOT chain to the
    /// renderer's last-applied seq — which is correct and intended: the
    /// renderer genuinely missed the intervening patches and takes D-09's
    /// single full `get_snapshot` resync, exactly the cost this path already
    /// pays today.
    pub last_patch: Option<(Patch, u64, u64)>,
}

/// Recursively import `dir` (already canonical) into virtual folder
/// `virtual_path`, at walk depth `depth`.
///
/// Ordering is PRE-ORDER: this dir's virtual folder is registered BEFORE any of
/// its files or subdirs are touched, which is what makes both core invariants
/// hold at every step — `CreateMediaFolder` requires the PARENT to already be
/// registered (no mkdir -p), and `AddMediaBinItem` rejects an item whose
/// `folder` is not registered. Because `Store::end_turn` REVERSES the group,
/// undo then runs the inverses in reverse dispatch order (items removed first,
/// then child folders, then parents) so `DeleteMediaFolder`'s empty-check
/// passes automatically.
///
/// Phase 43 (LAT-08): `async` because [`import_one_path`] is. The recursive
/// call below is `Box::pin`ned — an `async fn` that awaits itself has an
/// infinitely-sized future otherwise; boxing at the ONE recursion site is the
/// standard, allocation-per-directory fix (a directory descent, not a hot loop).
async fn walk_import_dir<C: AppCtx>(
    ctx: &C,
    walk: &mut FolderWalk,
    probe_cache: &mut probe_cache::ProbeCache,
    dir: &Path,
    virtual_path: &str,
    depth: usize,
) -> Result<(), String> {
    let store = ctx.store();
    // T-hgu-02 (depth): root = 0, its children = 1. A dir BEYOND the cap is
    // skipped whole — no registry entry, no descent, no file attempts.
    if depth > walk.max_depth {
        eprintln!(
            "import_media_folder: skipping {} (depth {depth} exceeds the {}-level cap)",
            dir.display(),
            walk.max_depth
        );
        return Ok(());
    }

    // MERGE on collision: a name already in `media_folders` is REUSED (no
    // duplicate registry entry, items land in the existing folder).
    if !walk.known_folders.contains(virtual_path) {
        match lock_shared(store)?.dispatch(Command::CreateMediaFolder {
            path: virtual_path.to_string(),
        }) {
            Ok(dispatched) => {
                walk.last_patch = Some(dispatched);
                walk.known_folders.insert(virtual_path.to_string());
                walk.created_folders.push(virtual_path.to_string());
            }
            Err(e) => {
                // Without the folder no item here can be dispatched, so skip
                // the whole subtree rather than half-mirror it.
                eprintln!(
                    "import_media_folder: skipping {} (cannot create virtual folder {virtual_path}: {e})",
                    dir.display()
                );
                return Ok(());
            }
        }
    }

    let read = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            eprintln!("import_media_folder: cannot read {}: {e}", dir.display());
            return Ok(());
        }
    };
    // Deterministic order (tests + a predictable MediaBin), by file name.
    let mut entries: Vec<std::fs::DirEntry> = read.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    let mut subdirs: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries {
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "import_media_folder: skipping {} ({e})",
                    entry.path().display()
                );
                continue;
            }
        };

        // T-hgu-03/T-hgu-05: skip EVERY symlink (file or dir) — a symlink cycle
        // can then never hang the walk, and no member can escape the selected
        // tree. Acknowledged untested gap: creating symlinks on Windows needs
        // elevation or Developer Mode, so this single branch ships with code +
        // this comment rather than an elevation-dependent (flaky) fixture test.
        if file_type.is_symlink() {
            eprintln!(
                "import_media_folder: skipping symlink {}",
                entry.path().display()
            );
            continue;
        }

        if file_type.is_dir() {
            match child_virtual_path(virtual_path, &entry.file_name()) {
                Some(child) => subdirs.push((entry.path(), child)),
                None => eprintln!(
                    "import_media_folder: skipping {} (name is not a legal virtual folder segment)",
                    entry.path().display()
                ),
            }
            continue;
        }
        if !file_type.is_file() {
            continue; // device / fifo / other: not media
        }

        // T-hgu-02 (files): CLAMP, never error.
        if walk.attempts >= walk.max_files {
            if !walk.cap_logged {
                eprintln!(
                    "import_media_folder: reached the {}-file import cap; remaining files are skipped",
                    walk.max_files
                );
                walk.cap_logged = true;
            }
            continue;
        }
        walk.attempts += 1;

        let raw = entry.path().to_string_lossy().into_owned();
        if let Some((item, patch, base_seq, seq)) = import_one_path(
            ctx,
            &walk.poster_dir,
            walk.project_fps,
            &raw,
            virtual_path,
            &mut walk.claimed,
            probe_cache,
        )
        .await?
        {
            walk.imported.push(item);
            walk.last_patch = Some((patch, base_seq, seq));
        }
    }

    for (path, child_virtual) in subdirs {
        // Boxed: a recursive `async fn` needs its self-referential future on
        // the heap. One `Box` per subdirectory, not per file.
        Box::pin(walk_import_dir(
            ctx,
            walk,
            probe_cache,
            &path,
            &child_virtual,
            depth + 1,
        ))
        .await?;
    }
    Ok(())
}

/// Walk every selected root directory into the media library and return the
/// finished [`FolderWalk`] state — the ONE shared implementation behind BOTH the
/// UI's `import_media_folder` command and the agent's `import_media` tool
/// (quick task 260726-k3n). There is deliberately no second walk: one engine
/// means the two surfaces cannot drift.
///
/// **TURN MANAGEMENT AND EVENT EMISSION ARE THE CALLER'S RESPONSIBILITY.** This
/// function contains NO `begin_turn`/`end_turn` and emits NO `project:changed`,
/// and that is load-bearing, not an oversight: [`rudis_core::Store::end_turn`] is
/// NOT refcounted — it unconditionally `take()`s the open turn and pushes the
/// group. If this walk owned its own bracket, calling it from inside
/// `run_agent_turn`'s whole-turn bracket (AGENT-03) would close the AGENT's turn
/// early and silently, with no error: every later dispatch in that turn would
/// push its OWN 1-member undo group, so one agent turn would need N Ctrl+Z
/// instead of one. Callers therefore bracket it themselves:
/// * [`run_import_media_folder_ui`] (UI, the relocated
///   `import_media_folder_capped` — 47-03) opens/closes its own turn and emits
///   once;
/// * `run_import_media`'s directory branch (agent) calls this BARE, inheriting
///   `run_agent_turn`'s existing bracket, and emits once itself.
///
/// Semantics (each a documented decision, unchanged from quick task 260726-hgu):
/// * **Mirror the nesting** — a selected `C:\footage\vacation` becomes root-level
///   virtual folder `vacation`; its child dir `day1` becomes `vacation/day1`;
///   each file's `folder` is its containing dir's virtual path.
/// * **Empty dirs still register** — mirroring means the bin matches disk, and
///   the model represents an empty folder by design (its registry entry).
/// * **Invalid disk names skip-with-log** (T-hgu-01, see [`child_virtual_path`]).
/// * **Collision merges** — an existing `media_folders` entry is reused.
/// * **Non-dir / unreadable selections and non-media files are logged and
///   skipped** — the per-file batch precedent; the rest still imports.
///
/// Phase 43 (LAT-08): `async`, and it owns the probe cache's load/save for the
/// whole gesture — one read before the walk, one write after it, for BOTH the
/// UI command and the agent tool, so neither surface can forget it and no
/// caller pays a per-file file read.
pub async fn walk_import_roots<C: AppCtx>(
    ctx: &C,
    paths: &[String],
    max_files: usize,
    max_depth: usize,
) -> Result<FolderWalk, String> {
    let store = ctx.store();
    let poster_dir = poster_cache_dir(ctx)?;

    // One short lock for both pieces of pre-walk state: the sequence timebase
    // and the EXISTING folder registry (the merge check's snapshot).
    let (project_fps, known_folders) = {
        let snap = lock_shared(store)?.snapshot();
        let folders: std::collections::HashSet<String> =
            snap.media_folders.iter().cloned().collect();
        (snap.fps, folders)
    };

    let mut walk = FolderWalk {
        poster_dir,
        project_fps,
        max_files,
        max_depth,
        known_folders,
        claimed: std::collections::HashSet::new(),
        attempts: 0,
        cap_logged: false,
        imported: Vec::new(),
        last_patch: None,
        created_folders: Vec::new(),
    };

    // Phase 43 (LAT-08): one read for the whole walk, one write at the end.
    let mut probe_cache_map = probe_cache::load(ctx);

    for raw in paths {
        let root = match std::fs::canonicalize(raw) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("import_media_folder: skipping {raw}: {e}");
                continue;
            }
        };
        if !root.is_dir() {
            eprintln!(
                "import_media_folder: skipping {} (not a directory)",
                root.display()
            );
            continue;
        }
        // T-hgu-05: the walk starts from the CANONICAL root and only ever
        // descends `read_dir` children, so it cannot escape the selection.
        let Some(name) = root.file_name() else {
            eprintln!(
                "import_media_folder: skipping {} (no directory name to mirror)",
                root.display()
            );
            continue;
        };
        let Some(virtual_root) = child_virtual_path("", name) else {
            eprintln!(
                "import_media_folder: skipping {} (name is not a legal virtual folder segment)",
                root.display()
            );
            continue;
        };
        walk_import_dir(
            ctx,
            &mut walk,
            &mut probe_cache_map,
            &root,
            &virtual_root,
            0,
        )
        .await?;
    }

    probe_cache::save(ctx, &probe_cache_map);
    Ok(walk)
}

/// The UI import command's whole per-call loop (Phase 47, plan 47-03).
/// NOT the agent tool's [`run_import_media`] below — that path has its own
/// turn/emit contract. This one preserves the UI command's pinned semantics:
/// per-call claimed set (260726-hgu), ONE probe-cache load/save per batch
/// (LAT-08), per-FILE emit (the pinned per-file undo/event contract,
/// `import_media_round_trip`).
///
/// The `#[tauri::command] async fn import_media` in `src-tauri` is now exactly
/// this call plus a `TauriAppCtx` construction.
pub async fn run_import_media_ui<C: AppCtx>(
    ctx: &C,
    paths: Vec<String>,
) -> Result<Vec<MediaBinItem>, String> {
    // Poster cache lives OUTSIDE the repo (e.g. ~/Library/Caches/<identifier>
    // on macOS, %LocalAppData% on Windows) — nothing to gitignore/commit.
    let poster_dir = poster_cache_dir(ctx)?;

    // Phase 28 (OVL-01): the PROJECT fps is the canonical timebase for an
    // imported numbered image sequence (Q2 resolved default) — read once, under
    // one lock, before the per-file loop.
    let project_fps = lock_shared(ctx.store())?.snapshot().fps;

    // A per-CALL claimed set (quick task 260726-hgu): if the user hands
    // `frame_0001.png` AND `frame_0002.png` in one multi-select, the second is
    // already consumed by the first's detected sequence and must not import
    // again. Behaviour for any OTHER selection is byte-identical to before.
    let mut claimed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // Phase 43 (LAT-08): read the persistent probe cache ONCE for the whole
    // batch, and write it back once at the end — never per file.
    let mut probe_cache_map = probe_cache::load(ctx);

    let mut imported: Vec<MediaBinItem> = Vec::new();
    for raw_path in paths {
        // `folder = ""` (library root) preserves the pre-existing placement of
        // every file-level import; the per-file `ctx.emit_patch` below preserves
        // the pinned per-file undo/event semantics (`import_media_round_trip`).
        if let Some((item, patch, base_seq, seq)) = import_one_path(
            ctx,
            &poster_dir,
            project_fps,
            &raw_path,
            "",
            &mut claimed,
            &mut probe_cache_map,
        )
        .await?
        {
            ctx.emit_patch(&patch, base_seq, seq)?;
            imported.push(item);
        }
    }

    probe_cache::save(ctx, &probe_cache_map);
    Ok(imported)
}

/// The UI folder-import command's turn bracket (Phase 47, plan 47-03) —
/// the ONLY walk caller that owns one: `begin_turn` once, `end_turn` ALWAYS
/// (even on a failed walk — T-12-14), ONE emit for the whole gesture
/// (T-hgu-04). The agent path ([`run_import_media`]'s directory branch) calls
/// [`walk_import_roots`] directly and must keep doing so (`end_turn` is not
/// idempotent).
///
/// * **One undo for the whole gesture** — `begin_turn()` once before ANY dir is
///   walked, `end_turn()` once after all of them, so one user action (even a
///   multi-dir selection) reverts with ONE Ctrl+Z.
/// * **ONE event** — `project:changed` is emitted ONCE after `end_turn` (never
///   per file: N files would otherwise cost N full-snapshot refetches in the
///   renderer). Nothing dispatched ⇒ nothing emitted.
///
/// The `#[tauri::command] async fn import_media_folder` in `src-tauri` is now
/// exactly this call at the production [`MAX_FOLDER_IMPORT_FILES`] /
/// [`MAX_FOLDER_IMPORT_DEPTH`] clamps, plus a `TauriAppCtx` construction; the
/// caps stay parameters so BOTH DoS clamps remain directly testable (the same
/// internal-fn + production-wrapper pattern as [`detect_image_sequence_capped`]
/// / [`detect_image_sequence`]).
pub async fn run_import_media_folder_ui<C: AppCtx>(
    ctx: &C,
    paths: Vec<String>,
    max_files: usize,
    max_depth: usize,
) -> Result<Vec<MediaBinItem>, String> {
    let store = ctx.store();
    // ONE undo group for the WHOLE gesture (short lock span, same as dispatch).
    lock_shared(store)?.begin_turn();

    let walk = walk_import_roots(ctx, &paths, max_files, max_depth).await;

    // ALWAYS close the group, even if the walk bailed on a poisoned mutex — an
    // errored gesture must never leave an open turn behind (the T-12-14 posture).
    lock_shared(store)?.end_turn();
    let walk = walk?;

    // T-hgu-04: exactly ONE renderer refresh for the whole gesture.
    if let Some((patch, base_seq, seq)) = &walk.last_patch {
        ctx.emit_patch(patch, *base_seq, *seq)?;
    }
    Ok(walk.imported)
}

/// What ONE agent `import_media` call actually did (quick task 260726-k3n).
///
/// `path` accepts a FILE (the original Phase-27 behavior) OR a DIRECTORY, and the
/// two produce genuinely different results, so ONE return type carries both
/// rather than a second entry point that could drift.
#[derive(Debug)]
pub enum ImportOutcome {
    /// A single real file was probed and dispatched — the unchanged Phase-27
    /// behavior.
    File(MediaBinItem),
    /// A real directory tree was walked through the SAME engine the UI's
    /// `import_media_folder` uses, mirroring its on-disk nesting into
    /// `Project.media_folders`.
    Folder {
        /// The canonicalized directory that was walked.
        root_path: String,
        /// The root-level virtual folder the directory was mirrored into.
        virtual_root: String,
        /// Every item genuinely imported by the walk (may be empty — an empty
        /// directory still mirrors its structure, which is a success).
        items: Vec<MediaBinItem>,
        /// Every virtual folder the walk CREATED (pre-existing names merge and
        /// are therefore absent).
        created_folders: Vec<String>,
        /// The walk hit [`MAX_FOLDER_IMPORT_FILES`] and clamped.
        file_cap_hit: bool,
    },
}

/// Phase 27 (LIB-03): the `import_media` interception — the agent-callable,
/// SINGLE-PATH narrowing of the batch UI `import_media` Tauri command. It probes
/// ONE real external file with the SAME `engine::probe` + poster-by-kind +
/// `Command::AddMediaBinItem` dispatch pipeline, optionally placing it in an
/// existing media-library folder (validated by Plan 27-01's `AddMediaBinItem`
/// folder-existence check). Needs `engine::probe` + `app_cache_dir`, which live
/// ONLY here — a Pattern-C interception, NOT a core Tool (mirrors
/// `run_generate_image`'s shape).
///
/// Deliberate behavior difference from the batch UI command: THIS agent tool is
/// single-call, so a canonicalize/probe/dispatch failure is a hard `Err`
/// surfaced to the agent — NOT silently skipped. The UI batch command's
/// skip-on-failure semantics make sense for a multi-file drag-drop; a single
/// explicit agent call should fail loudly so the agent can react/inform the user.
///
/// T-27-03 (accept): `path` is model-authored but reaches only a local
/// filesystem READ the OS user already has access to (offline-core, no network
/// egress) — reusing the shipped UI command's `canonicalize` + probe-must-succeed
/// posture with no additional path confinement. T-27-05 (mitigate): the poster
/// WRITE path is `app_cache_dir()/posters/{server-generated id}.png` — no
/// agent-supplied string becomes a write path component.
///
/// # `path` may also be a DIRECTORY (quick task 260726-k3n)
///
/// A directory routes through [`walk_import_roots`] — the SAME recursive walk the
/// UI's `import_media_folder` command uses — mirroring the on-disk nesting into
/// `Project.media_folders` under a root-level folder named after the directory,
/// at the production [`MAX_FOLDER_IMPORT_FILES`] / [`MAX_FOLDER_IMPORT_DEPTH`]
/// clamps (T-k3n-02: zero new walk logic to get wrong). It is called BARE — no
/// `begin_turn`/`end_turn` — so the whole tree joins `run_agent_turn`'s open
/// AGENT-03 turn and reverts with ONE Ctrl+Z (T-k3n-01).
///
/// ## Error semantics
///
/// | `path` is | Failure | Result |
/// |---|---|---|
/// | a FILE | anything (unresolvable / unprobeable / rejected dispatch) | hard `Err` — the unchanged single-call doctrine |
/// | a DIRECTORY | unresolvable path | hard `Err` (the shared `canonicalize`) |
/// | a DIRECTORY | non-empty `folder` argument | hard `Err`, ZERO mutation — a directory always mirrors from the library root |
/// | a DIRECTORY | unreadable (`read_dir` fails) | hard `Err` — there is nothing to walk |
/// | a DIRECTORY | its own name is not a legal virtual segment | hard `Err` — the tree has nowhere to be mirrored |
/// | INSIDE a readable DIRECTORY | one bad/non-media file, an unmappable subdir name, a depth/file cap | SKIP-WITH-LOG — the walk's existing batch semantics; the rest still imports |
///
/// An EMPTY (but readable) directory is a SUCCESS with zero items: the folder
/// structure is still mirrored, which is the point of mirroring.
///
/// Documented divergence: the FILE branch below does NO image-sequence detection
/// (unchanged Phase-27 behavior — a numbered still imports as one still). The
/// DIRECTORY branch DOES, because it goes through `import_one_path` like the UI
/// walk, so a numbered frame run inside an imported tree becomes ONE sequence item.
pub fn run_import_media<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<ImportOutcome, String> {
    let store = ctx.store();
    let raw_path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "path is required".to_string())?
        .to_string();
    let folder = input
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Canonicalize to a REAL absolute path; a dangling path is a hard Err (the
    // single-call divergence from the batch command's skip-on-failure).
    let abs = std::fs::canonicalize(&raw_path)
        .map_err(|e| format!("cannot resolve path {raw_path}: {e}"))?;

    // ---- DIRECTORY branch (quick task 260726-k3n) ---------------------------
    // Everything below this block is the byte-unchanged Phase-27 FILE path.
    if abs.is_dir() {
        // A directory import ALWAYS mirrors its nesting from the LIBRARY ROOT
        // (locked decision), so `folder` has no meaning here. Reject BEFORE any
        // mutation so the model gets a loud, actionable error rather than a
        // silently-ignored argument.
        if !folder.is_empty() {
            return Err(format!(
                "the `folder` argument applies to single files only, but {} is a directory — \
                 a directory import always mirrors its on-disk nesting from the library root. \
                 Retry without `folder`, or pass one file's path.",
                abs.display()
            ));
        }
        // Probe readability ONCE up front: an unreadable directory is a hard Err
        // (nothing to walk), unlike the per-file failures INSIDE a readable one.
        std::fs::read_dir(&abs)
            .map_err(|e| format!("cannot read directory {}: {e}", abs.display()))?;
        // The root's own name must map to a legal virtual segment, or the whole
        // tree has nowhere to be mirrored (T-hgu-01 / T-k3n-03).
        let virtual_root = abs
            .file_name()
            .and_then(|name| child_virtual_path("", name))
            .ok_or_else(|| {
                format!(
                    "directory name of {} cannot become a media-library folder \
                     (empty, \".\"/\"..\", over 255 bytes, or containing \":\" or a separator)",
                    abs.display()
                )
            })?;

        // The ONE shared walk — turn-free and emit-free, so it inherits
        // `run_agent_turn`'s open AGENT-03 bracket instead of closing it (see
        // `walk_import_roots`' doc comment: `Store::end_turn` is not refcounted).
        //
        // Phase 43 (LAT-08): the walk is now `async`, so this ONE call site is
        // wrapped in a `block_on(async { .. })` — mirrors
        // `handle_generate_image`'s block_on(async {..}); this Pattern-C
        // interception stays synchronous by design (43-06), because async
        // offers no benefit to a caller that is itself already synchronous.
        //
        // Phase 45 (45-07): the primitive is `ctx.block_on`, whose Tauri
        // implementation IS `tauri::async_runtime::block_on` — so the runtime
        // this enters, and therefore the one `tokio::task::spawn_blocking`
        // resolves against inside the walk, is byte-for-byte the one used
        // before the move (see this module's doc, T-45-05).
        let walk = ctx.block_on(async {
            walk_import_roots(
                ctx,
                std::slice::from_ref(&raw_path),
                MAX_FOLDER_IMPORT_FILES,
                MAX_FOLDER_IMPORT_DEPTH,
            )
            .await
        })?;

        // ONE renderer refresh for the whole directory gesture, matching the UI
        // path (never one per file).
        if let Some((patch, base_seq, seq)) = &walk.last_patch {
            ctx.emit_patch(patch, *base_seq, *seq)?;
        }
        return Ok(ImportOutcome::Folder {
            root_path: abs.to_string_lossy().into_owned(),
            virtual_root,
            items: walk.imported,
            created_folders: walk.created_folders,
            file_cap_hit: walk.attempts >= MAX_FOLDER_IMPORT_FILES,
        });
    }

    // Phase 43 (LAT-08): the agent tool's OWN single-file probe goes through
    // the SAME shared cache as the UI batch path — `must_haves` claims cache
    // reuse on EVERY import surface, and this is the second (and last)
    // `engine::probe` call site in this file. Load/save per call is fine here:
    // this branch handles exactly ONE file, never a batch. The cache is
    // consulted (and written) ONLY when the file is cacheable; an unreadable
    // stat degrades to an ordinary uncached probe rather than to an error.
    let cache_key = probe_cache::key_for(&abs);
    let mut cache = probe_cache::load(ctx);
    let probed = match cache_key.as_ref().and_then(|k| cache.get(k)).cloned() {
        Some(hit) => hit,
        None => {
            let fresh =
                engine::probe(&abs).map_err(|e| format!("probe {}: {e}", abs.display()))?;
            let mapped = cached_from_probe(&fresh);
            if let Some(k) = cache_key {
                cache.insert(k, mapped.clone());
                probe_cache::save(ctx, &cache);
            }
            mapped
        }
    };

    // Poster cache lives OUTSIDE the repo — same dir the UI import command uses.
    let poster_dir = poster_cache_dir(ctx)?;
    let id = next_id("media");
    // Poster policy mirrors import_media: video -> mid-duration frame; image ->
    // t=0; audio -> none. Poster failure downgrades to None, never aborts.
    let poster_path = if probed.media_kind == MediaKind::Audio {
        None
    } else {
        let at_seconds = poster_at_seconds(probed.media_kind, probed.duration_us);
        let out = poster_dir.join(format!("{id}.png"));
        match engine::generate_poster(&abs, &out, at_seconds) {
            Ok(()) => Some(out.to_string_lossy().into_owned()),
            Err(e) => {
                eprintln!("import_media: no poster for {}: {e}", abs.display());
                None
            }
        }
    };

    let item = MediaBinItem {
        id,
        path: abs.to_string_lossy().into_owned(),
        media_kind: probed.media_kind,
        duration_us: probed.duration_us,
        width: probed.width,
        height: probed.height,
        fps: probed.avg_frame_rate,
        is_vfr: probed.is_vfr,
        rotation_degrees: probed.rotation_degrees,
        has_audio: probed.has_audio,
        poster_path,
        folder,
        display_name: None,
        is_image_sequence: false,
        // Phase 60 (OCCL-01): the SAME mapping the UI batch surface uses — both
        // build from a `CachedProbeResult`, so the two cannot drift.
        reports_alpha: probed.has_alpha,
    };

    // Backend-owned + undoable: the SAME dispatch path import_media/generate_image
    // use, so the new item auto-joins the open agent turn and emits
    // project:changed. A nonexistent `folder` is rejected here by Plan 27-01's
    // AddMediaBinItem::apply folder check (the Err propagates; the bin is
    // unchanged). IN-01: on dispatch failure the just-extracted poster is
    // orphaned (no MediaBinItem references it), so best-effort remove it.
    let dispatched = store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())
        .and_then(|mut g| {
            g.dispatch(Command::AddMediaBinItem(item.clone()))
                .map_err(|e| e.to_string())
        });
    let (patch, base_seq, seq) = match dispatched {
        Ok(dispatched) => dispatched,
        Err(e) => {
            if let Some(pp) = &item.poster_path {
                let _ = std::fs::remove_file(pp);
            }
            return Err(e);
        }
    };
    ctx.emit_patch(&patch, base_seq, seq)?;
    Ok(ImportOutcome::File(item))
}

/// Phase 27 (LIB-03): the `import_media` interception. Never panics: a missing
/// `path` / unresolvable path / unprobeable file / rejected folder becomes an
/// `is_error` text tool_result so the agent can react, never a partial mutation.
///
/// Two success shapes (quick task 260726-k3n): a FILE keeps its original,
/// byte-identical one-line text; a DIRECTORY gets a summary the model can act on
/// — the item count, the mirrored root folder, which folders were created, and a
/// per-item line (id / file name / kind / dimensions+duration / folder) CAPPED at
/// 20 items with a `get_media` pointer for the rest, so a 500-file import cannot
/// flood the conversation (T-k3n-04).
pub fn handle_import_media<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_import_media(ctx, input) {
        Ok(ImportOutcome::File(item)) => result(
            format!(
                "Imported {} as a {:?} media bin item ({}x{}), id {}.",
                item.path, item.media_kind, item.width, item.height, item.id
            ),
            None,
        ),
        Ok(ImportOutcome::Folder {
            root_path,
            virtual_root,
            items,
            created_folders,
            file_cap_hit,
        }) => {
            let mut text = format!(
                "Imported {} item(s) from the directory {root_path} into the media-library folder \"{virtual_root}\", mirroring its on-disk nesting.",
                items.len()
            );
            if created_folders.is_empty() {
                text.push_str("\nNo new folders were created (they already existed).");
            } else {
                text.push_str(&format!(
                    "\nFolders created: {}.",
                    created_folders.join(", ")
                ));
            }
            if file_cap_hit {
                text.push_str(&format!(
                    "\nNOTE: the {MAX_FOLDER_IMPORT_FILES}-file import cap was reached, so some files in this tree were skipped."
                ));
            }
            if items.is_empty() {
                text.push_str(
                    "\nNo media files were found, so 0 items were imported; the folder structure was still mirrored.",
                );
            } else {
                text.push_str("\nItems:");
                // T-k3n-04: a 500-file import must not flood the conversation —
                // list at most SUMMARY_ITEM_CAP and point at get_media for the rest.
                const SUMMARY_ITEM_CAP: usize = 20;
                for item in items.iter().take(SUMMARY_ITEM_CAP) {
                    let name = Path::new(&item.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| item.path.clone());
                    let shape = if item.media_kind == MediaKind::Audio {
                        format!("{:.2}s", item.duration_us as f64 / 1_000_000.0)
                    } else {
                        format!(
                            "{}x{}, {:.2}s",
                            item.width,
                            item.height,
                            item.duration_us as f64 / 1_000_000.0
                        )
                    };
                    text.push_str(&format!(
                        "\n- {} ({name}, {:?}, {shape}) in \"{}\"",
                        item.id, item.media_kind, item.folder
                    ));
                }
                if items.len() > SUMMARY_ITEM_CAP {
                    text.push_str(&format!(
                        "\n… and {} more (use get_media to list all).",
                        items.len() - SUMMARY_ITEM_CAP
                    ));
                }
            }
            result(text, None)
        }
        Err(e) => result(format!("import_media failed: {e}"), Some(true)),
    }
}

/// Migrated from `src-tauri/src/lib.rs`'s `import_media_gate` by plan 45-07 —
/// the ONE test in that module that pins a function this batch moved AND needs
/// nothing from the Tauri shell. Its two siblings stayed: they drive
/// `place_clip` / `export_timeline` / `dispatch_command`, which are
/// `#[tauri::command]`s, so what they pin is the shell integration (45-05's
/// rule: a test travels with the function it PINS).
///
/// `build_app_isolated("app.rudis.test.probe-cache-agent")` became
/// [`TestAppCtx`], whose per-INSTANCE `TempDir` gives strictly stronger
/// isolation than the per-identifier `app_data_dir` it replaces — which matters
/// here, because the whole test asserts on the exact contents of ONE cache file.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture, TestAppCtx};

    /// LAT-08 on the AGENT-TOOL single-file path. `run_import_media`'s FILE
    /// branch does not go through `import_one_path` — it has its own inline
    /// `engine::probe` call — so the cache has to be proven on this surface
    /// SEPARATELY, or `must_haves`' "every import surface" claim is untrue.
    #[test]
    fn run_import_media_reuses_the_persistent_probe_cache() {
        const SENTINEL_WIDTH: u32 = 4242;
        let ctx = TestAppCtx::new();
        let cache_file = probe_cache::cache_path(&ctx).expect("app data dir resolves");
        let _ = std::fs::remove_file(&cache_file);

        // Opaque-JSON pokes (43-03's convention), local so the two surfaces'
        // tests stay independently readable.
        let entries = |path: &Path| -> Vec<serde_json::Value> {
            let bytes = std::fs::read(path)
                .unwrap_or_else(|e| panic!("probe cache must exist at {}: {e}", path.display()));
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("valid JSON")["entries"]
                .as_array()
                .expect("entries list")
                .clone()
        };

        let bars = fixture("bars_720p30_5s.mp4");
        let import = |p: &str| -> MediaBinItem {
            match run_import_media(&ctx, &serde_json::json!({ "path": p }))
                .expect("the agent tool imports the real file")
            {
                ImportOutcome::File(item) => item,
                _ => panic!("expected a File outcome for a file path"),
            }
        };

        // COLD: probes for real, writes exactly one entry.
        let first = import(&bars);
        assert_eq!((first.width, first.height), (1280, 720));
        assert_eq!(entries(&cache_file).len(), 1, "one file -> one cache entry");

        // WARM: identical metadata, still exactly ONE entry.
        let second = import(&bars);
        assert_eq!(
            (second.width, second.height, second.duration_us),
            (first.width, first.height, first.duration_us),
            "the cached result matches the first probe"
        );
        assert!((second.fps - first.fps).abs() < 1e-9, "fps matches");
        assert_eq!(
            entries(&cache_file).len(),
            1,
            "reuse, not duplicate accumulation"
        );

        // NON-VACUOUS control: a sentinel width no fixture carries can only
        // come back out if `engine::probe` was genuinely skipped.
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache_file).expect("read")).expect("valid JSON");
        for entry in value["entries"].as_array_mut().expect("entries") {
            entry[1]["width"] = serde_json::json!(SENTINEL_WIDTH);
        }
        std::fs::write(&cache_file, serde_json::to_vec(&value).expect("reserialize"))
            .expect("write doctored cache");

        let third = import(&bars);
        assert_eq!(
            third.width, SENTINEL_WIDTH,
            "the agent-tool path SERVED the cached value (a re-probe would return 1280)"
        );
        assert_eq!(entries(&cache_file).len(), 1);

        let _ = std::fs::remove_file(&cache_file);
    }

    // -----------------------------------------------------------------------
    // Phase 58 (PROXY-02): the D-07 trigger, and the D-09 detachment it needs
    // -----------------------------------------------------------------------

    /// D-07 + D-09, on the REAL UI import path with REAL media.
    ///
    /// Two claims in one gate, because they are only meaningful together:
    ///
    /// 1. **The trigger fires for a heavy source.** A 4K fixture passes the
    ///    heaviness predicate's resolution rung, so importing it registers a
    ///    proxy job — the status getter answers `queued`, not `none`.
    /// 2. **Import never waits for it.** DETERMINISTIC, not a race: the test
    ///    holds the pool's only permit for the whole import, so the encode
    ///    provably cannot have started. `queued` after `run_import_media_ui`
    ///    returned is therefore proof that the import path did not await the
    ///    transcode, rather than evidence that it happened to be fast.
    ///
    /// Then the permit is released and the REAL `h264_mf` sidecar produces a
    /// REAL proxy, verified as a committed payload on disk through the cache's
    /// own read — never "the status said ready".
    #[test]
    fn importing_a_heavy_source_queues_a_detached_proxy() {
        let _lease = crate::proxy_job::encoder_lease();
        let ctx = TestAppCtx::new();
        let heavy = fixture("bars_4k30_5s.mp4");
        crate::proxy_job::test_forget(Path::new(&heavy));

        let permit = crate::proxy_job::try_take_permit()
            .expect("the pool is idle while this test holds the lease");

        let items = ctx
            .block_on(run_import_media_ui(&ctx, vec![heavy.clone()]))
            .expect("the real import path succeeds on a real fixture");
        let item = items.first().expect("one item imported").clone();
        assert_eq!(
            (item.width, item.height),
            (3840, 2160),
            "the fixture really is the heavy one the predicate is being asked about"
        );

        assert_eq!(
            crate::proxy_job::test_state_of(&ctx, &item.id),
            "queued",
            "import returned with the proxy still queued behind a permit it \
             cannot get — D-09's 'generation never blocks import', proven by \
             construction rather than by timing"
        );

        // Release the pool and let the real encode run.
        drop(permit);
        let dir = ctx
            .app_cache_dir()
            .expect("cache dir")
            .join(proxy::cache::PROXY_CACHE_DIR_NAME);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            if crate::proxy_job::test_state_of(&ctx, &item.id) == "ready" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            crate::proxy_job::test_state_of(&ctx, &item.id),
            "ready",
            "the queued proxy runs once the pool frees up"
        );

        // Verify on OUTPUT (CLAUDE.md rule 3): a real committed proxy, reached
        // through the cache's own lookup rather than a path the test built.
        let hit = proxy::cache::read_fresh(Path::new(&heavy), &dir)
            .expect("the cache reports a fresh proxy for the imported source");
        assert_eq!(
            (hit.width, hit.height),
            (960, 540),
            "and it carries the derived geometry, not the source's"
        );
        assert!(hit.path.is_file(), "the payload is a real file on disk");

        crate::proxy_job::test_forget(Path::new(&heavy));
    }

    /// D-07's other half: a source that FAILS the predicate costs nothing.
    ///
    /// The 720p fixture clears neither rung, so importing it must register no
    /// job, spawn no encode and leave the proxy cache directory unborn. Without
    /// this control, "heavy sources get proxies" would pass just as happily on
    /// an implementation that proxies everything.
    #[test]
    fn importing_a_light_source_queues_no_proxy_at_all() {
        let _lease = crate::proxy_job::encoder_lease();
        let ctx = TestAppCtx::new();
        let light = fixture("bars_720p30_5s.mp4");
        crate::proxy_job::test_forget(Path::new(&light));

        let items = ctx
            .block_on(run_import_media_ui(&ctx, vec![light.clone()]))
            .expect("the real import path succeeds on a real fixture");
        let item = items.first().expect("one item imported").clone();
        assert_eq!((item.width, item.height), (1280, 720));

        assert_eq!(
            crate::proxy_job::test_state_of(&ctx, &item.id),
            "none",
            "a light source registers no job — the predicate gate, not a \
             cancelled encode"
        );
        let dir = ctx
            .app_cache_dir()
            .expect("cache dir")
            .join(proxy::cache::PROXY_CACHE_DIR_NAME);
        assert!(
            !dir.exists(),
            "and nothing ever touched the proxy cache directory: {dir:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 60 (OCCL-01, plan 60-06): the pre-decode alpha the occlusion
    // predicate reads at gather time
    // -----------------------------------------------------------------------

    /// The tri-state, filled from a REAL probe of REAL media, on BOTH import
    /// surfaces — and the same answer from a cache HIT as from a cold probe.
    ///
    /// Three fixtures, three verdicts, and the middle one is the whole reason
    /// the plumbing exists:
    ///
    /// * an ordinary opaque H.264 mp4 → `Some(false)`, occluder-eligible;
    /// * a VP9/WebM carrying alpha → `Some(true)`, NEVER an occluder. This is
    ///   the case a naive predicate gets wrong: VP9's native decoder reports a
    ///   plain `yuv420p` pix_fmt, so the alpha is only visible through the WebM
    ///   `alpha_mode` side-channel tag `engine::probe` already reads. If this
    ///   assertion ever flips to `Some(false)`, the predicate would cull a
    ///   genuinely transparent layer — OCCL-01's exact failure mode;
    ///   * an audio-only source → `None`, because nothing visual was probed at
    ///   all and "no video stream" is not evidence of opacity.
    #[test]
    fn import_records_the_probed_source_alpha_on_both_surfaces() {
        let ctx = TestAppCtx::new();

        let agent_import = |p: &str| -> MediaBinItem {
            match run_import_media(&ctx, &serde_json::json!({ "path": p }))
                .expect("the agent tool imports the real file")
            {
                ImportOutcome::File(item) => item,
                _ => panic!("expected a File outcome for a file path"),
            }
        };

        for (name, expected) in [
            ("bars_720p30_5s.mp4", Some(false)),
            ("overlay_vp9_alpha.webm", Some(true)),
            ("tone.m4a", None),
        ] {
            let path = fixture(name);

            // COLD: a real ffprobe decides.
            let cold = agent_import(&path);
            assert_eq!(
                cold.reports_alpha, expected,
                "{name}: the cold probe's verdict"
            );
            assert_eq!(
                cold.source_alpha(),
                rudis_core::SourceAlpha::from_reports_alpha(expected),
                "{name}: and it reads back through the tri-state"
            );

            // WARM: the persistent probe cache must reach the SAME verdict —
            // `cached_from_probe` is the one mapping site precisely so a hit
            // and a miss cannot disagree.
            let warm = agent_import(&path);
            assert_eq!(
                warm.reports_alpha, cold.reports_alpha,
                "{name}: a cache HIT reports the same alpha as the cold probe"
            );

            // The OTHER surface (the UI batch walk) shares the mapping too.
            let ui = ctx
                .block_on(run_import_media_ui(&ctx, vec![path.clone()]))
                .expect("the UI batch path imports the real file");
            assert_eq!(
                ui.first().expect("one item imported").reports_alpha,
                expected,
                "{name}: the UI batch surface agrees with the agent-tool surface"
            );
        }
    }

    /// An imported numbered image SEQUENCE is deliberately `None`.
    ///
    /// Its `path` is an image2 `%0Nd` PATTERN, never one media file, so the
    /// per-frame alpha of the set is not something a single container probe
    /// answers — and a sequence is excluded from the occlusion predicate's scope
    /// anyway. `None` is the honest value and the safe one; a `Some(false)`
    /// guessed from the pattern probe would be exactly the confident-and-wrong
    /// verdict this field exists to avoid (the tree's own sequence fixture is
    /// RGBA PNG — the guess would have been wrong on the first try).
    ///
    /// Runs on the UI batch surface because that is the only one that detects
    /// sequences at all (`import_one_path` → `build_sequence_item`; the
    /// agent-tool single-file path imports a numbered PNG as a lone still).
    #[test]
    fn an_imported_image_sequence_reports_an_unknown_source_alpha() {
        let ctx = TestAppCtx::new();
        let first_frame = fixture("seq/frame_0001.png");

        let items = ctx
            .block_on(run_import_media_ui(&ctx, vec![first_frame]))
            .expect("the UI batch path imports the sequence's first frame");
        let item = items.first().expect("one item imported").clone();
        assert!(
            item.is_image_sequence,
            "non-vacuous: the fixture really did resolve to a sequence"
        );
        assert_eq!(
            item.reports_alpha, None,
            "a sequence carries no single-file alpha verdict"
        );
        assert!(
            !item.source_alpha().is_proven_opaque(),
            "and is therefore never occluder-eligible"
        );
    }
}
