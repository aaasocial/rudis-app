//! Phase 26 (LIB-02): project persistence primitives -- one .rud JSON file
//! per project under app_data_dir()/projects/. Pure app-layer I/O;
//! crates/core stays zero-I/O (offline_guard.rs). No agent-tool wiring here
//! (Plan 03) -- this module is the tested file-I/O primitive layer only.
//!
//! # Relocated by plan 45-06
//!
//! Moved here from `src-tauri/src/project_store.rs` VERBATIM except for one
//! function: [`projects_dir`] took `&tauri::AppHandle<R>` and now takes
//! `&impl AppCtx`. Every other item in this file -- including all 15
//! `#[cfg(test)]` tests -- is byte-identical to its `src-tauri` original; none
//! of them ever named a `tauri` type. `src-tauri` reaches this module
//! unchanged through `pub use app_core::project_store;`, so
//! `project_store::write_project_atomic(..)` and
//! `rudis_app_lib::project_store::scan_known_projects(..)` (used by
//! `src-tauri/tests/lat01_baseline.rs`) both keep resolving exactly as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use rudis_core::Project;
use serde::{Deserialize, Serialize};

use crate::AppCtx;

/// Max bytes for a project name -- mirrors crates/core's
/// MAX_FOLDER_SEGMENT_LEN / MAX_MEDIA_NAME_LEN (255) DoS/UX cap convention.
const MAX_PROJECT_NAME_LEN: usize = 255;

/// MS-DOS/Windows reserved device names. Windows recognizes these by matching
/// the path component up to the FIRST '.', regardless of any extension --
/// "NUL.rud" still addresses the NUL device (a silent /dev/null sink that
/// discards every byte while reporting write success). Case-insensitive.
const RESERVED_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Monotonic per-write nonce so `write_project_atomic`'s temp file is unique
/// per call (WR-02: two concurrent writers to the same project must never
/// share a `.tmp` handle and interleave-corrupt it before the rename).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Validate an agent-supplied project name BEFORE it becomes a real
/// filesystem path component (T-26-01: path traversal / injection). A
/// FRESH implementation mirroring crates/core's is_safe_folder_segment
/// character-class rules -- NOT imported cross-crate (that fn is
/// pub(crate) to rudis_core AND scoped to virtual, never-touches-disk
/// paths; this is a REAL filesystem path, a stricter risk profile).
pub fn sanitize_project_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("project name must not be empty".to_string());
    }
    if name.len() > MAX_PROJECT_NAME_LEN {
        return Err(format!("project name must be at most {MAX_PROJECT_NAME_LEN} bytes"));
    }
    if name == "." || name == ".." {
        return Err("project name must not be \".\" or \"..\"".to_string());
    }
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return Err("project name must not contain a path separator or colon".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("project name must not contain control characters".to_string());
    }
    // Windows matches a reserved device name against the component BEFORE the
    // first '.', regardless of extension -- so both "NUL" and "NUL.rud" hit the
    // null device. Reject the base (pre-'.') segment, case-insensitively.
    let base = name.split('.').next().unwrap_or(name);
    if RESERVED_WINDOWS_NAMES
        .iter()
        .any(|r| base.eq_ignore_ascii_case(r))
    {
        return Err(format!(
            "project name \"{name}\" is a reserved Windows device name"
        ));
    }
    Ok(())
}

/// Resolve `app_data_dir()/projects`, creating it if needed.
///
/// Plan 45-06: was `projects_dir<R: tauri::Runtime>(app: &tauri::AppHandle<R>)`,
/// whose body read `app.path().app_data_dir().map_err(|e| format!("resolve app
/// data dir: {e}"))`. `TauriAppCtx::app_data_dir` produces that EXACT error
/// string, so a failure here is byte-identical to the pre-move message.
pub fn projects_dir(ctx: &impl AppCtx) -> Result<PathBuf, String> {
    let dir = ctx.app_data_dir()?.join("projects");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create projects dir: {e}"))?;
    Ok(dir)
}

/// Atomic save (26-RESEARCH.md Pitfall 1): write to a sibling `.tmp` file,
/// then `std::fs::rename` over the real path -- atomic on the same volume,
/// so a crash/power-loss mid-write can never leave a truncated `.rud`.
pub fn write_project_atomic(path: &Path, project: &Project) -> Result<(), String> {
    let json = serde_json::to_string_pretty(project)
        .map_err(|e| format!("serialize project: {e}"))?;
    // WR-02: unique temp name (pid + monotonic counter) so concurrent writers
    // to the same target never share a `.tmp` handle. Kept in the SAME dir as
    // `path` (sibling) so the final rename stays a same-volume atomic replace.
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = PathBuf::from(format!(
        "{}.{}-{:x}.tmp",
        path.to_string_lossy(),
        std::process::id(),
        nonce
    ));
    std::fs::write(&tmp, &json).map_err(|e| {
        // IN-02: best-effort cleanup so a failed write never leaks a stray .tmp.
        let _ = std::fs::remove_file(&tmp);
        format!("write temp project file: {e}")
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        // IN-02: rename failed (dest locked/permissions) -- the temp file would
        // otherwise leak; remove it before returning (real `.rud` untouched).
        let _ = std::fs::remove_file(&tmp);
        format!("rename temp project file: {e}")
    })?;
    Ok(())
}

/// Load a `.rud` file back into a real `Project`.
///
/// A `.rud` is plain user-editable JSON that can be shared or downloaded, so
/// this is an UNTRUSTED boundary — not merely a deserialize. Derived state and
/// bounded fields are re-established here, applying the exact same rules the
/// `Command::apply` layer applies, before the project can reach the hot path.
pub fn load_project(path: &Path) -> Result<Project, String> {
    // T-60.1-02: bound the read BEFORE it happens, exactly as the sidecar index
    // path has done since T-43-04-01. `load_project` is now reachable from a
    // user-chosen path, so an unbounded `fs::read` here is an OOM on the single
    // interop worker thread.
    let meta = std::fs::metadata(path).map_err(|e| format!("read project file: {e}"))?;
    check_rud_size(meta.len())?;
    let bytes = std::fs::read(path).map_err(|e| format!("read project file: {e}"))?;
    let mut project: Project =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse project file: {e}"))?;
    // Derived state + untrusted curve (quick task 260730-x2t, CR-01 /
    // T-x2t-08): `Retime::timeline_len_us` is a CACHE, and the curve is bounded
    // (speed in [0.1, 10], <= MAX_RETIME_KEYS keys, no NaN). Rebuild both at the
    // load boundary — the same rule every command apply follows — or a
    // hand-edited file silently misplaces every later clip, overflows
    // `timeline_end_us`, uncaps the per-output-frame key walk, and breaks the
    // strict monotonicity both bisection inverses assume.
    project.sanitize_retime_after_load();
    // T-60.1-03: and the counts serde will happily accept but nothing else
    // bounds. Refuses -- never truncates -- so the file on disk survives.
    bound_project_after_load(&mut project)?;
    Ok(project)
}

// ---------------------------------------------------------------------------
// Phase 60.1 (PROJ-05): the untrusted `.rud` OPEN boundary, as a DELTA over
// T-26-01 -- extended, never duplicated, never weakened.
//
// [`sanitize_project_name`] above validates a *name* before it becomes ONE
// path component, and it still runs FIRST on every name-keyed route
// (`run_new_project` / `run_open_project`); nothing here changes a byte of it.
// What follows validates a *whole path the user chose through a file picker*,
// which is a surface the name-keyed route never had because it never left
// `projects_dir(ctx)`.
//
// The three new controls, named so later plans can cite them:
//   T-60.1-01  canonicalise + require an existing REGULAR FILE + require the
//              `.rud` extension  ([`RudProjectPath`])
//   T-60.1-02  bound the read by `metadata.len()` BEFORE `fs::read`
//              ([`check_rud_size`], called from BOTH doors)
//   T-60.1-03  refuse absurd post-parse counts ([`bound_project_after_load`])
//
// T-60.1-05 is deliberately ACCEPTED rather than mitigated: a shared `.rud`
// carries attacker-chosen `MediaBinItem.path` strings. That is
// read-amplification only -- Rudis already accepts arbitrary user-picked media
// paths through `rudis_import_media` with no allow-list, and `crates/engine`
// invokes FFmpeg as an argv subprocess and never a shell, so there is no
// injection channel. The real control is T-60.1-03's item cap. Recorded here
// rather than hidden.
// ---------------------------------------------------------------------------

/// The same convention as [`MAX_PROJECT_INDEX_BYTES`] (T-43-04-01): never hand
/// an unbounded byte count to `serde_json`. A `.rud` from a picker is a
/// caller-chosen file, so a 4 GiB "project" would otherwise be an OOM on the
/// single interop worker thread before anything could reject it.
///
/// 64 MiB is generous by four orders of magnitude: the 6-clip
/// `F6M Six Minimized 4K.rud` fixture is **8,753 bytes**. The cap exists to
/// bound a hostile file, not to bound a real one.
const MAX_RUD_BYTES: u64 = 64 * 1024 * 1024;

/// T-60.1-03 caps. These are **DoS bounds on a read-amplification surface, not
/// domain rules** -- nothing in the editor enforces them at edit time, and a
/// project is not "invalid" for approaching them. They exist because a
/// syntactically perfect `.rud` can carry counts that turn one open into
/// millions of `ffprobe`/poster/proxy probes or a multi-gigabyte snapshot
/// clone, and serde will happily accept every one of them.
///
/// 10 000 media items is ~40x the largest real bin anyone has built here and
/// still bounds the per-item work that `rearm_project_media` schedules on open.
const MAX_MEDIA_BIN_ITEMS: usize = 10_000;

/// 100 000 clips summed across ALL tracks -- counted as a total rather than
/// per-track, because a hostile file can spread the same load out thinly.
const MAX_TIMELINE_CLIPS: usize = 100_000;

/// 256 tracks. The compositor walks every track per output frame, so this one
/// bounds the hot path rather than the load path.
const MAX_TRACKS: usize = 256;

/// 4 096 bytes for a `MediaBinItem` id or path. Ids are generated short and a
/// real Windows path cannot exceed ~32 767 even extended, so this only ever
/// catches the megabyte-of-text shape -- which would otherwise be cloned into
/// every snapshot, undo entry and IPC delta.
const MAX_ID_BYTES: usize = 4_096;

/// T-60.1-02, in ONE place so both doors decide identically: the
/// [`RudProjectPath`] type boundary and [`load_project`] itself.
///
/// Takes a length rather than a path so it is decidable without touching the
/// filesystem -- which is what lets the cap be tested at its exact boundary
/// without writing 64 MiB.
fn check_rud_size(len: u64) -> Result<(), String> {
    if len > MAX_RUD_BYTES {
        return Err(format!(
            "project file is {len} bytes, over the {MAX_RUD_BYTES}-byte limit for a .rud"
        ));
    }
    Ok(())
}

/// A `.rud` path a USER chose through a file picker, already validated.
///
/// The inner `PathBuf` is **private** and the ONLY way in is the
/// [`Deserialize`] impl below -- the discipline `RunwayUploadedAsset` uses
/// (`crates/agent-gen/src/runway.rs`) to make a bad value structurally
/// unconstructible rather than runtime-checked. There is no `new`, no `From`,
/// no `FromStr`, no public field and no `Default`. A function that takes THIS
/// type cannot be called with an unvalidated path, so "the path was validated"
/// is a property of the SIGNATURE rather than a habit of the call site.
///
/// What a value of this type guarantees: the path is canonical, it exists, it
/// is a REGULAR FILE (junctions, symlinks and reparse points already resolved
/// by `canonicalize`), it ends in `.rud` case-insensitively, and it is small
/// enough to read.
///
/// ⚠ **It is deliberately NOT confined to any root.** Opening a project from
/// anywhere on disk is ROADMAP § 60.1 scope item 3 and the entire point of the
/// type -- the owner could not open a `.rud` sitting in the managed projects
/// folder itself. Do not "fix" this by adding a root check; the distinction
/// that keeps T-26-03 intact is WHO CHOSE THE PATH (a picker is the user, a
/// `name` argument may be the LLM), and that distinction is expressed by there
/// being two functions, not by confining this one.
///
/// This type is for OPENING. A Save As target does not exist yet and would be
/// refused by every check here -- see [`RudSaveTargetPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RudProjectPath(PathBuf);

impl RudProjectPath {
    /// Borrow the validated, canonical path. Read-only, and the only accessor.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RudProjectPath {
    /// Hand-written rather than derived, because a derive would accept any
    /// string and this type's whole value is that it does not.
    ///
    /// Order matters and is the mitigation, not an implementation detail:
    /// canonicalise FIRST (T-60.1-01 -- this is what resolves `..`, `\\?\`,
    /// UNC, junctions and reparse points, so every later check is made against
    /// the real target rather than the text), then `metadata` on the CANONICAL
    /// path, then regular-file, then the size cap, then the extension.
    ///
    /// No refusal echoes the rejected path back: a hostile string reflected
    /// into an error is an injection vector into every log that error reaches
    /// (the rule `RunwayUploadedAsset::deserialize` records).
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let raw = String::deserialize(deserializer)?;
        let canonical = std::fs::canonicalize(&raw)
            .map_err(|e| D::Error::custom(format!("cannot resolve project path: {e}")))?;
        let meta = std::fs::metadata(&canonical)
            .map_err(|e| D::Error::custom(format!("cannot read project path: {e}")))?;
        if !meta.is_file() {
            return Err(D::Error::custom(
                "that project path is not a regular file -- pick a .rud file, not a folder",
            ));
        }
        check_rud_size(meta.len()).map_err(D::Error::custom)?;
        let is_rud = canonical
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("rud"));
        if !is_rud {
            return Err(D::Error::custom(
                "a Rudis project file must have a .rud extension",
            ));
        }
        Ok(Self(canonical))
    }
}

/// T-60.1-03: refuse a syntactically valid `.rud` whose COUNTS are absurd.
///
/// **Refuse, never truncate.** A silently truncated project is a corrupted
/// project the user did not ask for, and it would be written back over the
/// original by the very next autosave. Returning `Err` leaves the file on disk
/// exactly as it was, which is the only outcome a user can recover from.
///
/// Takes `&mut` rather than `&` deliberately: this sits at the same load
/// boundary as `sanitize_retime_after_load`, which normalises in place, and
/// D-4's remaining half (clamping absurd SCALARS -- `duration_us = i64::MAX`,
/// `width` at `u32::MAX`) belongs here and does mutate. The seam is shaped for
/// the extension rather than needing to be re-plumbed for it.
fn bound_project_after_load(p: &mut Project) -> Result<(), String> {
    if p.media_bin.len() > MAX_MEDIA_BIN_ITEMS {
        return Err(format!(
            "project has {} media items, over the {MAX_MEDIA_BIN_ITEMS} limit",
            p.media_bin.len()
        ));
    }
    if p.timeline.tracks.len() > MAX_TRACKS {
        return Err(format!(
            "project has {} tracks, over the {MAX_TRACKS} limit",
            p.timeline.tracks.len()
        ));
    }
    let clips: usize = p.timeline.tracks.iter().map(|t| t.clips.len()).sum();
    if clips > MAX_TIMELINE_CLIPS {
        return Err(format!(
            "project has {clips} clips, over the {MAX_TIMELINE_CLIPS} limit"
        ));
    }
    for item in &p.media_bin {
        if item.id.len() > MAX_ID_BYTES || item.path.len() > MAX_ID_BYTES {
            return Err(format!(
                "a media item's id or path exceeds the {MAX_ID_BYTES}-byte limit"
            ));
        }
    }
    Ok(())
}

/// A `.rud` path a USER chose as a SAVE AS target, already validated
/// (T-60.1-04).
///
/// # Why this is a second type and not a flag on [`RudProjectPath`]
///
/// **An open target must already exist; a save target must not have to.**
/// `RudProjectPath` canonicalises the WHOLE path, which requires the file to
/// be there — so it would refuse every Save As target there has ever been.
/// Folding the two into one type means weakening the open check to admit
/// nonexistent paths, which is exactly the guarantee `run_open_project_at_path`
/// is built on. Two contracts, two types. Do not "simplify" them into one;
/// `the_open_and_save_target_types_are_not_interchangeable` drives both
/// directions on real files and will redden if anyone tries.
///
/// # What it validates, and where each rule comes from
///
/// The PARENT is canonicalised and must be an existing directory — the Save As
/// equivalent of T-60.1-01, resolving traversal and reparse points without
/// requiring the target file to exist. The extension must be `.rud`,
/// case-insensitively. **The final component is then handed to the existing
/// [`sanitize_project_name`]** rather than re-checked here: the 22 MS-DOS
/// device names, the 255-byte cap, the separator/colon rule and the
/// control-character rule all stay owned by the one function that has owned
/// them since Phase 26. That delegation is the ROADMAP's "extend T-26-01, do
/// not duplicate it" discharged concretely — and it is what keeps the `NUL.rud`
/// sink closed on this new door (Windows resolves it to the null device, a
/// writer that reports success and discards every byte).
///
/// An existing regular `.rud` IS accepted: Save As over an existing project is
/// legitimate, and `write_project_atomic` replaces it atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RudSaveTargetPath(PathBuf);

impl RudSaveTargetPath {
    /// Borrow the validated target. Read-only, and the only accessor.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RudSaveTargetPath {
    /// Hand-written for the same reason [`RudProjectPath`]'s is: a derive would
    /// accept any string, and this type's whole value is that it does not.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let raw = String::deserialize(deserializer)?;
        let requested = PathBuf::from(&raw);

        let file_name = requested
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| D::Error::custom("a save target must name a file, not just a folder"))?
            .to_string();
        let parent = requested.parent().ok_or_else(|| {
            D::Error::custom("a save target must name the folder to save into")
        })?;

        // The folder must already exist. Canonicalising it here is what makes
        // every later step reason about the real directory rather than the
        // text -- the same job `canonicalize` does for the open path, applied
        // to the only component that is allowed to exist yet.
        let canonical_parent = std::fs::canonicalize(parent).map_err(|e| {
            D::Error::custom(format!("cannot resolve the folder to save into: {e}"))
        })?;
        if !canonical_parent.is_dir() {
            return Err(D::Error::custom(
                "the folder to save into is not a folder",
            ));
        }

        let named = Path::new(&file_name);
        let is_rud = named
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("rud"));
        if !is_rud {
            return Err(D::Error::custom(
                "a Rudis project must be saved with a .rud extension",
            ));
        }
        let stem = named
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| D::Error::custom("a save target needs a name before the .rud"))?;

        // T-26-01, reused rather than re-implemented. Its message is returned
        // verbatim so the user reads the reason the ONE owner of these rules
        // gives -- and so a second, drifting copy of them here would be
        // visible immediately.
        sanitize_project_name(stem).map_err(D::Error::custom)?;

        let target = canonical_parent.join(&file_name);
        if let Ok(meta) = std::fs::metadata(&target) {
            if !meta.is_file() {
                return Err(D::Error::custom(
                    "that save target exists and is not a regular file",
                ));
            }
        }
        Ok(Self(target))
    }
}

/// Every media item whose backing file is not present, by **id**.
///
/// # The decision, and its reason
///
/// **The project OPENS regardless.** Premiere Pro, DaVinci Resolve, Final Cut
/// Pro and CapCut all open a project with missing media and offer a repair
/// path; not one of them refuses. A beginner's first encounter with a broken
/// link must be a fixable state inside a working app, never a locked door.
/// This is the ROADMAP's flagged hard-to-reverse item, decided deliberately
/// (60.1-RESEARCH § Finding E).
///
/// This is **slice 1: read-only detection only.** Nothing is rewritten,
/// nothing is removed, no heuristic guesses at a new location — a repair the
/// user did not ask for, on data they cannot undo, is the one shape that turns
/// a recoverable state into a lost project. Slice 2 (Resolve-style
/// folder-level repair: "where did you move them?" → one folder → match by
/// file name → rewrite the matching `MediaBinItem.path` as ONE undoable
/// command) is deferred and recorded rather than half-built.
///
/// **This slice makes no `.rud` schema change**, which is what keeps the
/// phase's reversibility budget unspent: `.rud` keeps absolute media paths and
/// gains no fields. Portability (relative paths, a bundle, a path-root
/// indirection table) would be a format commitment and must be decided
/// explicitly, not drifted into.
///
/// Returns **ids, not paths** — the minimal-disclosure posture
/// `run_get_projects` established at T-26-08. The shell already keys MediaBin
/// tiles by media id and holds the path in its own mirror, so an id is
/// everything the ⚠ "relink" state needs.
pub fn missing_media(project: &Project) -> Vec<String> {
    project
        .media_bin
        .iter()
        .filter(|item| !Path::new(&item.path).is_file())
        .map(|item| item.id.clone())
        .collect()
}

/// One entry in the known-projects registry -- always derived from a REAL
/// on-disk `.rud` file, never a separate index (26-RESEARCH.md "Don't
/// Hand-Roll": no manifest file that can drift from disk).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Phase 43 (LAT-04): the sidecar project-name index
// ---------------------------------------------------------------------------
// `scan_known_projects` runs on every `open_project` and `get_projects`, and
// used to `load_project` -- a FULL JSON deserialize into a complete `Project`,
// then drop it -- for every `.rud` in the directory purely to read one `name`
// string. Its cost therefore scaled with the number of OTHER known projects.
//
// D-12/D-13: a sidecar index file in the same directory, rebuilt lazily when
// any `.rud` is newer than it. The fast path is an OPTIMIZATION, never a
// correctness dependency -- a missing, wrong-version, corrupt or provably stale
// index falls straight back to the full-parse scan (the real `.rud` files are
// always the source of truth) and rewrites itself.

/// The sidecar index's filename. A leading underscore keeps it sorting before
/// ordinary project names and visually distinct; the `.json` extension means
/// the existing `.rud`-only filter already ignores it without an extra guard.
const PROJECT_INDEX_FILE: &str = "_index.json";

/// Bump when [`ProjectIndex`]'s shape changes -- any other version is treated
/// as absent (full scan + rewrite), so an older or newer build sharing a
/// projects directory degrades to correct-but-slow, never to wrong.
const PROJECT_INDEX_VERSION: u32 = 2;

/// T-43-04-01: the index is a new on-disk artifact, parsed BEFORE the window is
/// interactive, whose contents are attacker-influenceable (file names, project
/// names) by anything with write access to the projects dir. Refuse to hand an
/// unbounded byte count to `serde_json` on the startup path. At ~140 bytes per
/// entry this still admits ~60k projects -- far past any real registry -- and a
/// rejected index costs exactly one full scan.
const MAX_PROJECT_INDEX_BYTES: u64 = 8 * 1024 * 1024;

/// Below this many `.rud` files the full scan stays on the calling thread:
/// spawning workers would cost more than it saves.
const SCAN_PARALLEL_THRESHOLD: usize = 16;

/// Upper bound on full-scan worker threads. The work is dominated by per-file
/// `CreateFile`/`ReadFile` syscall latency, so it parallelizes well, but this
/// runs on the IPC thread of an app that is also decoding video -- 8 is the
/// measured knee (1000 files: 214ms serial -> 30ms at 8 -> 24ms at 16).
const SCAN_MAX_WORKERS: usize = 8;

/// One recorded project name plus the staleness key that says whether it is
/// still true. `(mtime, size)` mirrors D-17's cache key for LAT-08's probe
/// cache -- the same grain, one phase, one convention.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectIndexEntry {
    file_name: String,
    name: String,
    /// NANOseconds since the epoch, not milliseconds: a millisecond-truncated
    /// stamp can compare EQUAL across two genuinely different writes.
    mtime_ns: u64,
    size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectIndex {
    version: u32,
    entries: Vec<ProjectIndexEntry>,
}

/// One `.rud` file exactly as the directory enumeration saw it. The `(mtime,
/// size)` pair is harvested from the SAME [`std::fs::DirEntry`], which on
/// Windows is free -- `FindNextFile` already returned it. Re-`stat`ing each
/// path instead costs 46ms at 1000 files vs 1.5ms for the whole enumeration,
/// which is the difference between the fast path being worth having and not.
struct RudFile {
    path: PathBuf,
    file_name: String,
    mtime_ns: u64,
    size_bytes: u64,
}

/// Modification time as nanoseconds since the epoch. `0` means "unknown" (an
/// unreadable timestamp) and is never treated as a valid staleness key -- such
/// a file always forces the full scan rather than being trusted from cache.
fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Enumerate the directory's `.rud` files ONCE, harvesting each one's staleness
/// key as we go. Both the fast path and the full scan consume this same
/// listing, so a scan never enumerates the directory twice.
fn list_rud_files(dir: &Path) -> Vec<RudFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rud") {
            continue;
        }
        let (mtime, size) = match entry.metadata() {
            Ok(meta) => (mtime_ns(&meta), meta.len()),
            // Never panic on a metadata failure: `0` forces the full scan.
            Err(_) => (0, 0),
        };
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        out.push(RudFile {
            file_name: file_name.to_string(),
            path,
            mtime_ns: mtime,
            size_bytes: size,
        });
    }
    out
}

/// The registry IS a directory scan -- the single source of truth. A file
/// that fails to parse is silently skipped (never panics; a corrupt file
/// simply does not appear as "known" rather than crashing the scan).
///
/// Phase 43 (LAT-04): the sidecar index is consulted first and used ONLY when
/// it validates against the current directory listing; otherwise this is
/// exactly the scan it always was, plus a best-effort index rewrite.
pub fn scan_known_projects(dir: &Path) -> Vec<ProjectEntry> {
    let listing = list_rud_files(dir);
    if let Some(fast) = try_scan_from_index(dir, &listing) {
        return fast;
    }
    let full = scan_known_projects_full(&listing);
    let mut entries: Vec<ProjectEntry> = full.iter().map(|(e, _)| e.clone()).collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    // Best-effort: a write failure never fails the scan (D-13).
    write_project_index(dir, &full);
    entries
}

/// `None` = the index is missing, oversized, wrong-version, corrupt, OR stale
/// (the file set or any file's `(mtime, size)` differs from what was recorded)
/// -- the caller falls back to the full parse. `Some` = every recorded entry
/// was validated against the CURRENT directory listing before being trusted.
fn try_scan_from_index(dir: &Path, listing: &[RudFile]) -> Option<Vec<ProjectEntry>> {
    let idx_path = dir.join(PROJECT_INDEX_FILE);
    let idx_meta = std::fs::metadata(&idx_path).ok()?;
    // T-43-04-01: bound the read BEFORE it happens, and require a real file
    // (a directory named `_index.json` must not be `read` into memory).
    if !idx_meta.is_file() || idx_meta.len() > MAX_PROJECT_INDEX_BYTES {
        return None;
    }
    let idx_mtime_ns = mtime_ns(&idx_meta);
    if idx_mtime_ns == 0 {
        return None;
    }

    let bytes = std::fs::read(&idx_path).ok()?;
    let index: ProjectIndex = serde_json::from_slice(&bytes).ok()?;
    if index.version != PROJECT_INDEX_VERSION || index.entries.len() != listing.len() {
        return None;
    }

    // Map lookup, not a linear `contains` per file: a registry of N projects
    // would otherwise cost N^2 path comparisons on the startup path. Duplicate
    // `file_name`s in a hand-edited index collapse here and fail the count
    // check, so a doctored index cannot smuggle in an unmatched entry.
    let recorded: HashMap<&str, &ProjectIndexEntry> = index
        .entries
        .iter()
        .map(|e| (e.file_name.as_str(), e))
        .collect();
    if recorded.len() != listing.len() {
        return None;
    }

    let mut out = Vec::with_capacity(listing.len());
    for file in listing {
        // A recorded file vanished, was renamed, or changed on disk.
        let entry = recorded.get(file.file_name.as_str())?;
        if file.mtime_ns == 0
            || entry.mtime_ns != file.mtime_ns
            || entry.size_bytes != file.size_bytes
        {
            return None;
        }
        // D-12, literally: "rebuilt lazily when any `.rud` is NEWER than the
        // index". Windows file timestamps advance in coarse (~15.6ms) steps, so
        // a `.rud` rewritten in the same tick the index was built in can carry
        // an unchanged mtime AND an unchanged size. `>=` takes the conservative
        // side of that race: same tick means rescan, never serve a stale name.
        if file.mtime_ns >= idx_mtime_ns {
            return None;
        }
        out.push(ProjectEntry {
            name: entry.name.clone(),
            path: file.path.clone(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

/// Read + fully deserialize ONE `.rud` to recover its project name -- the exact
/// work the index exists to avoid, kept verbatim as the always-correct fallback.
fn scan_one(file: &RudFile) -> Option<(ProjectEntry, ProjectIndexEntry)> {
    let project = load_project(&file.path).ok()?;
    let name = if !project.name.is_empty() {
        project.name.clone()
    } else {
        file.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };
    Some((
        ProjectEntry {
            name: name.clone(),
            path: file.path.clone(),
        },
        ProjectIndexEntry {
            file_name: file.file_name.clone(),
            name,
            mtime_ns: file.mtime_ns,
            size_bytes: file.size_bytes,
        },
    ))
}

/// The pre-LAT-04 behavior: full-parse every listed `.rud`, skipping any that
/// fails, preserving directory-enumeration order (the caller sorts by name, and
/// a stable sort makes that order the tie-break -- so this must stay
/// order-preserving to keep the fast and full paths byte-identical).
///
/// Large registries fan the per-file work out across a bounded worker pool: the
/// files are independent, and the cost is dominated by per-file syscall latency.
/// Chunks are joined in order, so the output is deterministic and identical to
/// the serial result.
fn scan_known_projects_full(listing: &[RudFile]) -> Vec<(ProjectEntry, ProjectIndexEntry)> {
    let workers = if listing.len() < SCAN_PARALLEL_THRESHOLD {
        1
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, SCAN_MAX_WORKERS)
    };
    if workers <= 1 {
        return listing.iter().filter_map(scan_one).collect();
    }

    let chunk = listing.len().div_ceil(workers).max(1);
    let mut out = Vec::with_capacity(listing.len());
    let mut worker_panicked = false;
    std::thread::scope(|scope| {
        let handles: Vec<_> = listing
            .chunks(chunk)
            .map(|c| scope.spawn(move || c.iter().filter_map(scan_one).collect::<Vec<_>>()))
            .collect();
        for handle in handles {
            match handle.join() {
                Ok(part) => out.extend(part),
                // Every handle IS joined, so `scope` does not re-raise this.
                Err(_) => worker_panicked = true,
            }
        }
    });
    if worker_panicked {
        // A dropped chunk would silently hide real projects AND poison the
        // index with a short entry list. Redo the whole listing serially.
        return listing.iter().filter_map(scan_one).collect();
    }
    out
}

/// Best-effort. A write failure is swallowed -- the index is an optimization,
/// never a correctness dependency (D-13).
///
/// Written temp-then-rename, the module's established atomic-write convention:
/// two concurrent scans can then never leave a torn `_index.json`. A torn one
/// would only cost a full scan (T-43-04-01), but never producing one is cheaper
/// than relying on that.
///
/// Note: `.rud` files that failed to parse are absent from `scanned` and so
/// from the index, which makes the entry count disagree with the directory
/// listing and disables the fast path while such a file is present. That is a
/// deliberate degradation to correct-but-slow, not a correctness problem.
fn write_project_index(dir: &Path, scanned: &[(ProjectEntry, ProjectIndexEntry)]) {
    let index = ProjectIndex {
        version: PROJECT_INDEX_VERSION,
        entries: scanned.iter().map(|(_, e)| e.clone()).collect(),
    };
    let Ok(json) = serde_json::to_string(&index) else {
        return;
    };
    let target = dir.join(PROJECT_INDEX_FILE);
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = PathBuf::from(format!(
        "{}.{}-{:x}.tmp",
        target.to_string_lossy(),
        std::process::id(),
        nonce
    ));
    if std::fs::write(&tmp, json.as_bytes()).is_err() || std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Phase 26 (SC-3): refuse to delete the CURRENTLY ACTIVE project's `.rud`
/// file -- an internal guard only, deliberately NOT an agent-facing tool
/// (26-RESEARCH.md Open Question 3: v3 ships no delete_project/
/// close_project agent tool at all; a future UI "Delete Project" action
/// would call this exact function). Deleting the active project's backing
/// file out from under the running in-memory Store would leave it
/// operating on a now-orphaned snapshot, then autosave would either fail
/// or silently recreate an orphaned file -- refused before that can happen.
pub fn remove_known_project(meta: &ActiveProjectMeta, target: &Path) -> Result<(), String> {
    let active = meta.0.lock().map_err(|_| "project meta poisoned".to_string())?;
    if active.as_deref() == Some(target) {
        return Err("cannot delete the currently active project".to_string());
    }
    drop(active);
    std::fs::remove_file(target).map_err(|e| format!("delete project file: {e}"))
}

/// Tauri-managed state (mirrors `PlaybackMirror`'s "non-domain managed
/// struct alongside the real Store" precedent): which `.rud` file the
/// currently-active project should autosave to. `None` until the first
/// `new_project`/`open_project` call -- preserves today's exact
/// in-memory-only startup behavior (no project is auto-created/loaded).
#[derive(Default)]
pub struct ActiveProjectMeta(pub Mutex<Option<PathBuf>>);

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rudis-project-store-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Every `*.tmp` file currently in `dir` -- used to assert no temp file
    /// leaks after a write/rename (WR-02/IN-02).
    fn tmp_files(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("tmp"))
            .collect();
        out.sort();
        out
    }

    fn sample_project(name: &str) -> Project {
        let mut p = Project::new();
        p.name = name.to_string();
        p
    }

    #[test]
    fn sanitize_project_name_rejects_path_traversal_and_unsafe_chars() {
        // Rejected: empty, "."/"..", traversal, path separators, drive/ADS colon,
        // control char, and a >255-byte name.
        let too_long = "a".repeat(256);
        let rejected = [
            "",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            "C:evil",
            "a\0b",
            too_long.as_str(),
        ];
        for name in rejected {
            assert!(
                sanitize_project_name(name).is_err(),
                "expected Err for {name:?}"
            );
        }

        // Accepted: ordinary names, and a name at exactly the 255-byte cap.
        let at_cap = "a".repeat(255);
        let accepted = ["My Project", "proj-1", at_cap.as_str()];
        for name in accepted {
            assert!(
                sanitize_project_name(name).is_ok(),
                "expected Ok for {name:?} (len {})",
                name.len()
            );
        }
    }

    /// CR-01: every Windows reserved device name must be rejected -- bare, with
    /// an extension (Windows matches up to the first '.', so "NUL.rud" still
    /// hits the null device), and in mixed case. Rejecting BEFORE any path join
    /// means new_project/write_project_atomic never touch the device at all.
    #[test]
    fn sanitize_project_name_rejects_windows_reserved_device_names() {
        let bases = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM5", "COM9", "LPT1", "LPT5", "LPT9",
        ];
        // Bare, with a .rud extension, with another extension, and mixed case.
        let suffixes = ["", ".rud", ".txt"];
        for base in bases {
            let variants = [base.to_string(), base.to_lowercase(), {
                // Mixed case: lowercase the first char only (e.g. "nUL", "cOM1").
                let mut c: Vec<char> = base.chars().collect();
                c[0] = c[0].to_ascii_lowercase();
                c.into_iter().collect::<String>()
            }];
            for v in &variants {
                for suffix in suffixes {
                    let name = format!("{v}{suffix}");
                    assert!(
                        sanitize_project_name(&name).is_err(),
                        "expected Err for reserved device name {name:?}"
                    );
                }
            }
        }

        // Names that merely CONTAIN a reserved token but whose base differs must
        // still be accepted (e.g. "CONtacts", "my-COM1", "NULl-and-void").
        for ok in ["CONtacts", "my-COM1", "NULl-and-void", "CONCERT"] {
            assert!(
                sanitize_project_name(ok).is_ok(),
                "expected Ok for non-reserved {ok:?}"
            );
        }
    }

    /// CR-01: new_project must never write to the NUL device. Because the
    /// rejection happens in sanitize (before any path join), no file OR device
    /// write occurs -- assert the projects dir stays empty for every reserved
    /// name variant. (Exercises the primitive layer directly: sanitize gate +
    /// the fact that no .rud/.tmp is produced.)
    #[test]
    fn reserved_device_names_produce_no_disk_write() {
        let dir = temp_dir("reserved-no-write");
        for name in ["NUL", "nul", "Nul", "NUL.rud", "CON", "COM1.rud", "LPT9"] {
            assert!(
                sanitize_project_name(name).is_err(),
                "reserved name {name:?} must be rejected before any write"
            );
        }
        // Nothing was ever written for a rejected name.
        let contents: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .collect();
        assert!(
            contents.is_empty(),
            "no file/device write must occur for reserved names, found {contents:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_project_atomic_produces_valid_json_and_leaves_no_tmp_file() {
        let dir = temp_dir("atomic-write");
        let path = dir.join("demo.rud");
        let project = sample_project("Demo Project");

        write_project_atomic(&path, &project).expect("atomic write");

        // The real target file exists...
        assert!(path.exists(), "target .rud file must exist after write");

        // ...deserializes back to an equal Project...
        let loaded = load_project(&path).expect("load written project");
        assert_eq!(loaded, project, "round-tripped project must equal the original");

        // ...and no sibling `.tmp` file remains anywhere in the dir (the unique
        // per-write temp name was consumed by the rename).
        assert_eq!(tmp_files(&dir), Vec::<PathBuf>::new(), "no .tmp may remain");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WR-02 + IN-02: two sequential writes to the SAME target (as rapid
    /// autosaves do) both succeed, the final file holds the LAST content, and
    /// no `.tmp` is leaked -- proving the per-write unique temp name does not
    /// break the last-writer-wins atomic replace.
    #[test]
    fn sequential_writes_keep_last_content_and_leave_no_tmp() {
        let dir = temp_dir("sequential-writes");
        let path = dir.join("proj.rud");

        write_project_atomic(&path, &sample_project("First")).expect("first write");
        write_project_atomic(&path, &sample_project("Second")).expect("second write");

        let loaded = load_project(&path).expect("load");
        assert_eq!(loaded.name, "Second", "final file must hold the last content");
        assert_eq!(
            tmp_files(&dir),
            Vec::<PathBuf>::new(),
            "no leftover .tmp after either write"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Quick task 260730-x2t, CR-01: the `.rud` LOAD boundary is UNTRUSTED.
    // -----------------------------------------------------------------------
    // A `.rud` is plain user-editable JSON that can also be shared or
    // downloaded. Before the fix, `load_project` was a bare
    // `serde_json::from_slice` and every guarantee `Command::apply` establishes
    // for a `Retime` was simply absent for anything off disk. These fixtures are
    // written as RAW JSON on purpose — a hand-crafted hostile file, not a
    // `Project` this crate serialized (which by construction can never carry
    // these values).

    /// A whole `.rud` document around one video clip whose `retime` object is
    /// supplied verbatim as JSON. `media_bin` and `timeline` are the only
    /// required Project keys; everything else takes its serde default.
    fn hostile_rud(retime_json: &str) -> String {
        format!(
            r#"{{
              "media_bin": [],
              "timeline": {{
                "tracks": [
                  {{"kind":"video","clips":[
                    {{"id":"c1","media_id":"m","start_us":1000000,
                      "in_us":0,"out_us":5000000,
                      "retime":{retime_json}}}
                  ]}},
                  {{"kind":"audio","clips":[]}}
                ]
              }},
              "fps": 30.0
            }}"#
        )
    }

    /// Load `retime_json` as the only clip's retime and return that clip.
    fn load_hostile_clip(dir: &Path, tag: &str, retime_json: &str) -> rudis_core::Clip {
        let path = dir.join(format!("{tag}.rud"));
        std::fs::write(&path, hostile_rud(retime_json)).expect("plant hostile .rud");
        let project = load_project(&path).expect("a hostile file must LOAD, not error");
        project.timeline.tracks[0].clips[0].clone()
    }

    /// CR-01 bypass 1: a stored `timeline_len_us` that DISAGREES with the curve.
    /// `Clip::timeline_len_us()` reads the stored value unconditionally, so a
    /// wrong one misplaces the clip's own end and — through `timeline_end_us`
    /// -> `active_at` -> `active_layers_at` — every downstream overlap, snap,
    /// export length and preview extent (threat T-x2t-08), reachable without
    /// touching a single command.
    #[test]
    fn load_project_rebuilds_a_retime_cache_that_disagrees_with_its_curve() {
        let dir = temp_dir("x2t-load-stale-cache");

        // 5 s of source at 2x MUST occupy 2.5 s. The file claims 30 s.
        let clip = load_hostile_clip(
            &dir,
            "stale",
            r#"{"curve":{"constant":2.0},"timeline_len_us":30000000,"timebase_fps":30.0}"#,
        );

        assert_eq!(
            clip.timeline_len_us(),
            2_500_000,
            "the loaded occupancy must be REBUILT from the curve + span \
             (5 s at 2x = 2.5 s), not read from the file's 30 s claim"
        );
        // The load-path twin of `assert_retime_cache_consistent`.
        assert_eq!(
            clip.retime.as_ref().map(|r| r.timeline_len_us),
            clip.recomputed_retime().map(|r| r.timeline_len_us),
            "the loaded clip's cached occupancy must equal recomputed_retime()'s"
        );
        assert_eq!(clip.timeline_end_us(), 1_000_000 + 2_500_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CR-01 bypass 2: `timeline_len_us: i64::MAX` overflowed
    /// `Clip::timeline_end_us`'s plain `start_us + len` — a debug-build PANIC
    /// on the per-output-frame hot path, and in release a wrap to NEGATIVE,
    /// which makes the clip INVISIBLE to `active_at`'s half-open test.
    #[test]
    fn load_project_defuses_an_i64_max_retime_occupancy() {
        let dir = temp_dir("x2t-load-overflow");

        let clip = load_hostile_clip(
            &dir,
            "overflow",
            r#"{"curve":{"constant":2.0},"timeline_len_us":9223372036854775807,"timebase_fps":30.0}"#,
        );

        assert_eq!(
            clip.timeline_len_us(),
            2_500_000,
            "an i64::MAX occupancy must be REPLACED by the curve's real answer"
        );
        // Would panic in debug / wrap negative in release before the fix.
        assert_eq!(clip.timeline_end_us(), 3_500_000);
        assert!(
            clip.timeline_end_us() > clip.start_us,
            "a clip's end must never wrap behind its own start"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CR-01 bypass 3: the `MAX_RETIME_KEYS` cap exists because
    /// `retime_source_offset` walks `keys.windows(2)` on the PER-OUTPUT-FRAME
    /// hot path (per frame, per track, per clip, from BOTH export and preview)
    /// — "the key count IS the per-frame cost". A file with hundreds of keys
    /// was accepted verbatim.
    #[test]
    fn load_project_rejects_a_ramp_past_the_key_cap() {
        let dir = temp_dir("x2t-load-keycap");

        let keys: Vec<String> = (0..(rudis_core::MAX_RETIME_KEYS as u32 + 40))
            .map(|f| format!(r#"{{"frame":{f},"value":1.5,"interp":"linear"}}"#))
            .collect();
        let clip = load_hostile_clip(
            &dir,
            "keycap",
            &format!(
                r#"{{"curve":{{"ramp":[{}]}},"timeline_len_us":3333333,"timebase_fps":30.0}}"#,
                keys.join(",")
            ),
        );

        assert!(
            clip.retime.is_none(),
            "a ramp past the {}-key cap must LOSE its retime (play 1:1) rather \
             than uncap the per-output-frame key walk; got {:?}",
            rudis_core::MAX_RETIME_KEYS,
            clip.retime
        );
        assert_eq!(
            clip.timeline_len_us(),
            5_000_000,
            "a dropped retime falls back to the 1:1 source span"
        );

        // A ramp AT the cap is legal and must survive with a rebuilt cache.
        let ok_keys: Vec<String> = (0..rudis_core::MAX_RETIME_KEYS as u32)
            .map(|f| format!(r#"{{"frame":{f},"value":2.0,"interp":"hold"}}"#))
            .collect();
        let ok = load_hostile_clip(
            &dir,
            "keycap-ok",
            &format!(
                r#"{{"curve":{{"ramp":[{}]}},"timeline_len_us":77,"timebase_fps":30.0}}"#,
                ok_keys.join(",")
            ),
        );
        assert!(ok.retime.is_some(), "a ramp AT the cap must still load");
        assert_ne!(
            ok.retime.as_ref().unwrap().timeline_len_us,
            77,
            "the at-cap ramp's bogus cached occupancy must still be rebuilt"
        );
        assert_eq!(
            ok.retime.as_ref().map(|r| r.timeline_len_us),
            ok.recomputed_retime().map(|r| r.timeline_len_us),
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CR-01 bypass 4: `validate_retime` deliberately REJECTS rather than
    /// clamps `speed <= 0` / non-finite (RT-08) — but it never ran on this
    /// path. `Constant(0.0)` freezes `source_offset_at` at 0 forever, and a
    /// NEGATIVE speed produces monotonically DECREASING source offsets, which
    /// breaks the strict-monotonicity precondition BOTH bisection inverses
    /// (`retimed_timeline_len_us` and `pts_map::map_source_pts_to_timeline`)
    /// assume: they return arbitrary answers rather than erroring.
    #[test]
    fn load_project_drops_out_of_range_and_non_finite_speeds() {
        let dir = temp_dir("x2t-load-speed");

        for (tag, curve) in [
            ("zero", r#"{"constant":0.0}"#),
            ("negative", r#"{"constant":-5.0}"#),
            ("above-max", r#"{"constant":50.0}"#),
            ("below-min", r#"{"constant":0.001}"#),
            (
                "ramp-negative",
                r#"{"ramp":[{"frame":0,"value":1.0,"interp":"linear"},
                            {"frame":30,"value":-2.0,"interp":"linear"}]}"#,
            ),
            (
                "ramp-duplicate-frames",
                r#"{"ramp":[{"frame":5,"value":1.0,"interp":"linear"},
                            {"frame":5,"value":2.0,"interp":"linear"}]}"#,
            ),
        ] {
            let clip = load_hostile_clip(
                &dir,
                tag,
                &format!(r#"{{"curve":{curve},"timeline_len_us":1,"timebase_fps":30.0}}"#),
            );
            assert!(
                clip.retime.is_none(),
                "{tag}: a curve validate_retime REJECTS must not survive the load \
                 boundary, got {:?}",
                clip.retime
            );
            assert_eq!(
                clip.timeline_len_us(),
                5_000_000,
                "{tag}: a dropped retime plays 1:1"
            );
            // The monotonicity the bisections depend on, restated as an
            // assertion rather than as prose.
            assert!(
                clip.source_offset_at(1_000_000) > clip.source_offset_at(0),
                "{tag}: the timeline->source map must stay strictly increasing"
            );
        }

        // A degenerate `timebase_fps` falls back to the project's, and the
        // curve itself survives (this is sanitize, not reject).
        let clip = load_hostile_clip(
            &dir,
            "bad-timebase",
            r#"{"curve":{"constant":2.0},"timeline_len_us":1,"timebase_fps":0.0}"#,
        );
        let r = clip.retime.as_ref().expect("a legal curve survives");
        assert_eq!(r.timebase_fps, 30.0, "a degenerate timebase falls back to project fps");
        assert_eq!(r.timeline_len_us, 2_500_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of CR-01: sanitizing must be a NO-OP for every file this
    /// app itself writes. A legitimately retimed project must survive
    /// `write_project_atomic` -> `load_project` completely unchanged, and an
    /// UN-retimed project must be byte-identical to its pre-retime self.
    #[test]
    fn load_project_round_trips_a_legitimate_retime_unchanged() {
        let dir = temp_dir("x2t-load-roundtrip");
        let path = dir.join("legit.rud");

        let mut project = sample_project("Legit");
        let mut clip = rudis_core::Clip {
            id: "c1".into(),
            media_id: "m".into(),
            start_us: 250_000,
            in_us: 0,
            out_us: 4_000_000,
            volume: 1.0,
            audio_detached: false,
            transform: Default::default(),
            opacity: 1.0,
            crop: Default::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: Some(rudis_core::Retime {
                curve: rudis_core::RetimeCurve::Ramp(vec![
                    rudis_core::Keyframe {
                        frame: 0,
                        value: 1.0,
                        interp: rudis_core::Interpolation::Smooth,
                    },
                    rudis_core::Keyframe {
                        frame: 60,
                        value: 0.4,
                        interp: rudis_core::Interpolation::Smooth,
                    },
                ]),
                // Built the way a command builds it: the real derived value.
                timeline_len_us: 0,
                timebase_fps: 30.0,
            }),
        };
        clip.retime = clip.recomputed_retime();
        project.timeline.tracks[0].clips.push(clip);

        write_project_atomic(&path, &project).expect("write");
        let loaded = load_project(&path).expect("load");
        assert_eq!(
            loaded, project,
            "a legitimately retimed project must round-trip byte-for-byte — \
             sanitizing is a no-op on everything this app writes"
        );

        // And an UN-retimed project is untouched (no `retime` key at all).
        let plain = sample_project("Plain");
        let plain_path = dir.join("plain.rud");
        write_project_atomic(&plain_path, &plain).expect("write plain");
        let json = std::fs::read_to_string(&plain_path).expect("read plain");
        assert!(
            !json.contains("retime"),
            "an un-retimed project must still emit NO retime key"
        );
        assert_eq!(load_project(&plain_path).expect("load plain"), plain);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_known_projects_finds_every_written_rud_file() {
        let dir = temp_dir("scan");

        write_project_atomic(&dir.join("alpha.rud"), &sample_project("Alpha"))
            .expect("write alpha");
        write_project_atomic(&dir.join("beta.rud"), &sample_project("Beta"))
            .expect("write beta");

        // A non-.rud file dropped in the same dir must be ignored by the scan.
        std::fs::write(dir.join("notes.txt"), b"not a project").expect("write stray file");

        let mut found = scan_known_projects(&dir);
        found.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(found.len(), 2, "exactly the 2 .rud files must be discovered");
        assert_eq!(found[0].name, "Alpha");
        assert_eq!(found[0].path, dir.join("alpha.rud"));
        assert_eq!(found[1].name, "Beta");
        assert_eq!(found[1].path, dir.join("beta.rud"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Phase 43 (LAT-04): the sidecar project-name index
    // -----------------------------------------------------------------------
    // These tests deliberately poke the index as OPAQUE JSON (`serde_json::Value`,
    // literal `"_index.json"`) rather than through the private `ProjectIndex`
    // types. The index is an internal optimization whose field names are free to
    // change; what must not change is the OBSERVABLE contract -- `scan_known_projects`
    // returns the same answer whether the index is absent, fresh, stale or hostile.
    // Only `index_value`'s `entries[].name` path is assumed, and only by the one
    // test that must prove the fast path really ran.

    /// The sidecar index's filename, restated literally so a rename of the
    /// private const cannot silently make these tests vacuous.
    const INDEX_FILE: &str = "_index.json";

    fn index_path(dir: &Path) -> PathBuf {
        dir.join(INDEX_FILE)
    }

    /// Parse the sidecar index as opaque JSON. `None` when it is absent or not
    /// valid JSON at all.
    fn index_value(dir: &Path) -> Option<serde_json::Value> {
        let bytes = std::fs::read(index_path(dir)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The index's `entries` array, however the struct around it is shaped.
    fn index_entries(dir: &Path) -> Vec<serde_json::Value> {
        index_value(dir)
            .as_ref()
            .and_then(|v| v.get("entries"))
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// Windows file timestamps advance in coarse (~15.6ms) steps, so a `.rud`
    /// written and an index built in the SAME tick are indistinguishable by
    /// mtime. The fast path resolves that race conservatively (same-tick =>
    /// treat as stale), so a test that needs the fast path to actually engage
    /// must put the project writes in an EARLIER tick than the index write.
    fn advance_filesystem_clock_tick() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    /// The degenerate case: an empty directory must return no projects, must
    /// not panic, and must still record an index (of zero entries) so the next
    /// call has something to validate against.
    #[test]
    fn scan_known_projects_on_an_empty_dir_returns_empty_and_writes_an_index() {
        let dir = temp_dir("idx-empty");

        let found = scan_known_projects(&dir);
        assert_eq!(found, Vec::<ProjectEntry>::new(), "an empty dir has no projects");

        assert!(
            index_path(&dir).exists(),
            "the scan must write a sidecar index even for zero projects"
        );
        assert!(
            index_value(&dir).is_some(),
            "the written index must be valid JSON"
        );
        assert_eq!(
            index_entries(&dir).len(),
            0,
            "an empty dir's index records zero entries"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The core equivalence: the fast (index) path and the full-parse path must
    /// agree EXACTLY. Call twice in a row -- the first builds the index, the
    /// second is served from it -- and demand an identical `Vec<ProjectEntry>`.
    #[test]
    fn scan_known_projects_second_call_matches_the_first_exactly() {
        let dir = temp_dir("idx-agree");
        write_project_atomic(&dir.join("a.rud"), &sample_project("Alpha")).expect("write a");
        write_project_atomic(&dir.join("b.rud"), &sample_project("Beta")).expect("write b");
        write_project_atomic(&dir.join("c.rud"), &sample_project("Gamma")).expect("write c");
        advance_filesystem_clock_tick();

        let first = scan_known_projects(&dir);
        assert_eq!(first.len(), 3, "all 3 .rud files are found on the cold scan");
        assert!(
            index_path(&dir).exists(),
            "the cold scan must leave an index behind"
        );
        assert_eq!(index_entries(&dir).len(), 3, "the index records all 3 files");

        let second = scan_known_projects(&dir);
        assert_eq!(
            second, first,
            "the index-served scan must be byte-identical to the full-parse scan"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Staleness by CONTENT: one `.rud` is rewritten under a different name
    /// WITHOUT going through `scan_known_projects`. The next scan must notice
    /// and report the NEW name -- a cached-forever wrong answer is the whole
    /// failure mode the validation exists to prevent.
    #[test]
    fn scan_known_projects_detects_a_rewritten_file_and_reports_the_new_name() {
        let dir = temp_dir("idx-stale-mtime");
        write_project_atomic(&dir.join("a.rud"), &sample_project("Alpha")).expect("write a");
        write_project_atomic(&dir.join("b.rud"), &sample_project("Beta")).expect("write b");
        write_project_atomic(&dir.join("c.rud"), &sample_project("Gamma")).expect("write c");
        advance_filesystem_clock_tick();

        let first = scan_known_projects(&dir);
        assert_eq!(
            first.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Beta", "Gamma"]
        );

        // Rewrite a.rud behind the scan's back, under a longer, different name.
        write_project_atomic(&dir.join("a.rud"), &sample_project("Alpha Renamed"))
            .expect("rewrite a");

        let third = scan_known_projects(&dir);
        assert_eq!(
            third.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha Renamed", "Beta", "Gamma"],
            "a rewritten .rud must invalidate the index, not be served stale"
        );
        assert!(
            index_entries(&dir)
                .iter()
                .filter_map(|e| e.get("name")?.as_str())
                .any(|n| n == "Alpha Renamed"),
            "the rebuilt index must record the NEW name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Staleness by FILE SET: a fourth `.rud` appears after the index was
    /// built. Every recorded file is still byte-identical, so only a file-set
    /// check (not an mtime check) can catch this.
    #[test]
    fn scan_known_projects_detects_a_file_added_behind_its_back() {
        let dir = temp_dir("idx-stale-fileset");
        write_project_atomic(&dir.join("a.rud"), &sample_project("Alpha")).expect("write a");
        write_project_atomic(&dir.join("b.rud"), &sample_project("Beta")).expect("write b");
        write_project_atomic(&dir.join("c.rud"), &sample_project("Gamma")).expect("write c");
        advance_filesystem_clock_tick();

        assert_eq!(scan_known_projects(&dir).len(), 3);
        assert_eq!(index_entries(&dir).len(), 3);

        write_project_atomic(&dir.join("d.rud"), &sample_project("Delta")).expect("write d");

        let found = scan_known_projects(&dir);
        assert_eq!(
            found.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Beta", "Delta", "Gamma"],
            "a NEW .rud must appear even though every indexed file is unchanged"
        );
        assert_eq!(
            index_entries(&dir).len(),
            4,
            "the rebuilt index must record all 4 files"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-43-04-01 (Tampering): the index is a NEW on-disk artifact parsed on
    /// the startup path and is writable by anything with access to the projects
    /// dir. Garbage in it must never panic, never be trusted, and never change
    /// the answer -- the REAL `.rud` files stay the source of truth, and a fresh
    /// correct index is written back.
    #[test]
    fn scan_known_projects_survives_a_corrupt_index_and_rewrites_it() {
        let dir = temp_dir("idx-corrupt");
        write_project_atomic(&dir.join("a.rud"), &sample_project("Alpha")).expect("write a");
        write_project_atomic(&dir.join("b.rud"), &sample_project("Beta")).expect("write b");

        // A range of hostile shapes: raw binary garbage, valid-JSON-wrong-type,
        // valid-JSON-right-shape-wrong-version, and an empty file.
        let corruptions: [&[u8]; 5] = [
            b"\x00\x01\x02 not json at all \xff\xfe",
            b"[]",
            b"{\"version\":999,\"entries\":[]}",
            b"{\"entries\":\"not an array\"}",
            b"",
        ];

        for corrupt in corruptions {
            std::fs::write(index_path(&dir), corrupt).expect("plant a corrupt index");

            let found = scan_known_projects(&dir);
            assert_eq!(
                found.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
                vec!["Alpha", "Beta"],
                "a corrupt index must fall back to the full parse, not change the answer"
            );
            assert_eq!(
                index_entries(&dir).len(),
                2,
                "the corrupt index must be replaced with a correct one"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fast path must ACTUALLY be taken -- otherwise every test above passes
    /// vacuously against the old always-full-parse implementation and LAT-04
    /// would be unproven. Plant a name in the index that appears in NO `.rud`
    /// file; seeing it come back is only possible if the name was read from the
    /// index instead of from a full deserialize of the project file.
    #[test]
    fn scan_known_projects_actually_reads_names_from_the_index() {
        let dir = temp_dir("idx-fastpath");
        write_project_atomic(&dir.join("a.rud"), &sample_project("Alpha")).expect("write a");
        write_project_atomic(&dir.join("b.rud"), &sample_project("Beta")).expect("write b");
        advance_filesystem_clock_tick();

        // Cold scan builds a valid index.
        assert_eq!(
            scan_known_projects(&dir)
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha", "Beta"]
        );

        // Substitute ONE recorded name, leaving every staleness key untouched.
        let mut value = index_value(&dir).expect("the cold scan wrote a valid index");
        let entries = value
            .get_mut("entries")
            .and_then(|e| e.as_array_mut())
            .expect("the index exposes an `entries` array");
        let mut substituted = false;
        for entry in entries.iter_mut() {
            if entry.get("name").and_then(|n| n.as_str()) == Some("Alpha") {
                entry["name"] = serde_json::Value::String("SENTINEL-FROM-INDEX".to_string());
                substituted = true;
            }
        }
        assert!(substituted, "the index must record Alpha's name to substitute");
        std::fs::write(index_path(&dir), serde_json::to_vec(&value).unwrap())
            .expect("write the doctored index");

        let found = scan_known_projects(&dir);
        assert_eq!(
            found.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["Beta", "SENTINEL-FROM-INDEX"],
            "the name must have come from the index -- no .rud file contains this string, \
             so a full parse could not have produced it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 43 (LAT-04): a reproducible COLD-vs-WARM number for the
    /// registry-size axis.
    ///
    /// The LAT-01 ruler (`src-tauri/tests/lat01_baseline.rs`) builds a fresh
    /// directory and calls `scan_known_projects` exactly ONCE, so it can only
    /// ever measure the COLD (index-absent) path -- by construction it cannot
    /// show what the index itself buys. This records both halves under
    /// identical conditions so the ratio is honest.
    ///
    /// NOT a comparison against the committed baseline artifact: the projects
    /// here are bare `Project::new()` documents, not the ruler's fixture-backed
    /// ones, so the absolute microseconds are NOT interchangeable with it. The
    /// printed `stub_file_bytes` makes that difference visible.
    ///
    /// `#[ignore]`d (it writes ~1000 files). Run with:
    ///   powershell -NoProfile -File scripts\windows\run-tauri-lib-tests.ps1 \
    ///     -TestName lat04_registry -Ignored
    #[test]
    #[ignore]
    fn lat04_registry_scan_cold_vs_warm_measurement() {
        use std::time::Instant;

        let registry_size = 1000usize;
        let dir = temp_dir("lat04-measure");
        write_project_atomic(&dir.join("target.rud"), &sample_project("Target")).expect("target");
        for i in 0..registry_size {
            write_project_atomic(&dir.join(format!("other-{i}.rud")), &sample_project(&format!("Other {i}")))
                .expect("stub");
        }
        let stub_bytes = std::fs::metadata(dir.join("other-0.rud"))
            .map(|m| m.len())
            .unwrap_or(0);
        // The index must land in a later filesystem clock tick than the project
        // writes, or the same-tick rule (correctly) refuses the fast path.
        advance_filesystem_clock_tick();

        let t0 = Instant::now();
        let cold = scan_known_projects(&dir);
        let cold_us = t0.elapsed().as_micros();
        assert_eq!(cold.len(), registry_size + 1, "the cold scan sees every .rud");

        let t1 = Instant::now();
        let warm = scan_known_projects(&dir);
        let warm_us = t1.elapsed().as_micros();
        assert_eq!(warm, cold, "the warm scan must agree with the cold scan exactly");

        println!(
            "LAT04_SCAN: {{\"registry_size\":{registry_size},\"cold_us\":{cold_us},\
             \"warm_us\":{warm_us},\"stub_file_bytes\":{stub_bytes},\
             \"note\":\"cold = index absent (full parse + index write); warm = served \
             from the validated index. Bare Project::new() documents, NOT the LAT-01 \
             ruler's fixture-backed projects -- absolute us are not interchangeable \
             with the committed baseline artifact.\"}}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 26 (SC-3): the guard REFUSES deleting the currently-active
    /// project and leaves its `.rud` file untouched on disk.
    #[test]
    fn remove_known_project_refuses_the_active_project() {
        let dir = temp_dir("remove-refuses-active");
        let path = dir.join("active.rud");
        write_project_atomic(&path, &sample_project("Active")).expect("write active");

        // ActiveProjectMeta points AT this file — deletion must be refused.
        let meta = ActiveProjectMeta(Mutex::new(Some(path.clone())));
        let result = remove_known_project(&meta, &path);
        assert!(result.is_err(), "removing the active project must be Err, got {result:?}");
        assert!(
            path.exists(),
            "the refused active project's .rud must still exist untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 26 (SC-3): the guard DELETES a non-active project's file while
    /// leaving the active one untouched.
    #[test]
    fn remove_known_project_deletes_a_non_active_project() {
        let dir = temp_dir("remove-deletes-nonactive");
        let active_path = dir.join("active.rud");
        let other_path = dir.join("other.rud");
        write_project_atomic(&active_path, &sample_project("Active")).expect("write active");
        write_project_atomic(&other_path, &sample_project("Other")).expect("write other");

        // Active is "active.rud"; deleting the OTHER, non-active file succeeds.
        let meta = ActiveProjectMeta(Mutex::new(Some(active_path.clone())));
        remove_known_project(&meta, &other_path).expect("deleting a non-active project is Ok");
        assert!(!other_path.exists(), "the non-active project's file must be deleted");
        assert!(active_path.exists(), "the active project's file must be untouched");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Phase 60.1 (PROJ-05): the untrusted `.rud` boundary -- T-60.1-01 (the
    // validated open path), T-60.1-02 (the bounded read) and T-60.1-03 (the
    // post-load count caps). Every case below drives a REAL refusal rather
    // than only exercising the happy path: a guard nobody has watched fire is
    // not a guard.
    // -----------------------------------------------------------------------

    /// The ONLY door into a [`RudProjectPath`] is its `Deserialize`, so the
    /// tests go through serde exactly as a host would.
    fn open_target(p: &Path) -> Result<RudProjectPath, serde_json::Error> {
        serde_json::from_value(serde_json::Value::String(
            p.to_string_lossy().into_owned(),
        ))
    }

    fn media_item(id: &str, path: &str) -> rudis_core::MediaBinItem {
        rudis_core::MediaBinItem {
            id: id.to_string(),
            path: path.to_string(),
            media_kind: rudis_core::MediaKind::Video,
            duration_us: 1_000_000,
            width: 1920,
            height: 1080,
            fps: 30.0,
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

    #[test]
    fn rud_project_path_accepts_a_real_rud_and_yields_the_canonical_path() {
        let dir = temp_dir("open-happy");
        let path = dir.join("Opened.rud");
        write_project_atomic(&path, &sample_project("Opened")).expect("write project");

        let validated = open_target(&path).expect("a real .rud written by write_project_atomic");
        let canonical = std::fs::canonicalize(&path).expect("canonicalize");
        assert_eq!(
            validated.as_path(),
            canonical.as_path(),
            "as_path() must return the CANONICAL form, not the string handed in"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rud_project_path_refuses_a_traversal_to_a_system_file() {
        // The classic shape: climb out of wherever we are and grab the SAM
        // hive. It is refused because it does not resolve to an existing
        // `.rud` regular file -- not because anything string-matched "..".
        let err = open_target(Path::new("..\\..\\..\\Windows\\System32\\config\\SAM"))
            .expect_err("a traversal to a system file must be refused");
        assert!(
            format!("{err}").contains("cannot resolve project path"),
            "unexpected refusal message: {err}"
        );
    }

    #[test]
    fn rud_project_path_refuses_a_directory() {
        let dir = temp_dir("open-dir");
        let as_rud = dir.join("looks-like-a-project.rud");
        std::fs::create_dir_all(&as_rud).expect("create dir named like a project");

        // The extension is right and it canonicalizes fine -- only the
        // regular-file check stands between this and a `fs::read` of a folder.
        let err = open_target(&as_rud).expect_err("a directory must be refused");
        assert!(
            format!("{err}").contains("not a regular file"),
            "unexpected refusal message: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rud_project_path_matches_the_extension_case_insensitively() {
        let dir = temp_dir("open-ext");

        // `.RUD` is the SAME extension on Windows and must be accepted.
        let upper = dir.join("Shouty.RUD");
        write_project_atomic(&upper, &sample_project("Shouty")).expect("write .RUD");
        assert!(
            open_target(&upper).is_ok(),
            "an uppercase .RUD is the same extension and must be accepted"
        );

        // `.rud.txt` is NOT a `.rud` -- the extension is `txt`. A suffix
        // "contains" check would have let this through.
        let decoy = dir.join("decoy.rud.txt");
        std::fs::write(&decoy, b"{}").expect("write decoy");
        let err = open_target(&decoy).expect_err("a .rud.txt must be refused");
        assert!(
            format!("{err}").contains(".rud"),
            "unexpected refusal message: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rud_project_path_refuses_a_path_that_does_not_exist() {
        let dir = temp_dir("open-missing");
        let err = open_target(&dir.join("never-written.rud"))
            .expect_err("a nonexistent path must be refused");
        assert!(
            format!("{err}").contains("cannot resolve project path"),
            "unexpected refusal message: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-60.1-02, at the unit the plan asked for: the size decision itself,
    /// exactly at the boundary. `MAX_RUD_BYTES` is allowed; one byte more is
    /// not.
    #[test]
    fn check_rud_size_refuses_only_past_the_cap() {
        assert!(
            check_rud_size(0).is_ok(),
            "an empty file is not a size failure"
        );
        assert!(
            check_rud_size(MAX_RUD_BYTES).is_ok(),
            "exactly at the cap must be allowed"
        );
        let err = check_rud_size(MAX_RUD_BYTES + 1).expect_err("one byte over must be refused");
        assert!(
            err.contains(&MAX_RUD_BYTES.to_string()),
            "the refusal must name the cap so a user can act on it: {err}"
        );
    }

    /// A 65 MiB file of zeros. If the size cap did NOT fire, `serde_json`
    /// would reject the zeros and the message would say "parse project file" --
    /// so asserting on WHICH message comes back proves the cap ran BEFORE the
    /// read, behaviourally, rather than proving it is merely wired up.
    #[test]
    fn load_project_refuses_an_oversized_file_before_it_parses() {
        let dir = temp_dir("open-oversize");
        let path = dir.join("huge.rud");
        let f = std::fs::File::create(&path).expect("create huge.rud");
        f.set_len(MAX_RUD_BYTES + 1).expect("grow past the cap");
        drop(f);

        let err = load_project(&path).expect_err("an oversized .rud must be refused");
        assert!(
            !err.contains("parse project file"),
            "the parse must never have been reached: {err}"
        );
        assert!(
            err.contains(&MAX_RUD_BYTES.to_string()),
            "the refusal must name the cap: {err}"
        );

        // ...and the same file is refused at the type boundary too.
        let type_err = open_target(&path).expect_err("an oversized .rud must be refused");
        assert!(
            format!("{type_err}").contains(&MAX_RUD_BYTES.to_string()),
            "unexpected refusal message: {type_err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-60.1-03: a syntactically PERFECT `.rud` carrying an absurd media-bin
    /// count is refused, and refusing means the file on disk is left exactly
    /// as it was -- a truncated project is a corrupted project the user never
    /// asked for.
    #[test]
    fn load_project_refuses_an_absurd_media_bin_count_and_leaves_the_file_unchanged() {
        let dir = temp_dir("load-bin-cap");
        let path = dir.join("absurd.rud");

        let mut p = sample_project("Absurd");
        p.media_bin = (0..=MAX_MEDIA_BIN_ITEMS)
            .map(|i| media_item(&format!("m{i}"), "C:/nope.mp4"))
            .collect();
        write_project_atomic(&path, &p).expect("write an oversized-but-valid project");

        let before = std::fs::read(&path).expect("read before");
        let err = load_project(&path).expect_err("an absurd media-bin count must be refused");
        assert!(
            err.contains(&MAX_MEDIA_BIN_ITEMS.to_string()),
            "the refusal must name the cap: {err}"
        );
        let after = std::fs::read(&path).expect("read after");
        assert_eq!(before, after, "a refusal must not rewrite the file on disk");

        // One under the cap is a perfectly ordinary project.
        p.media_bin.pop();
        write_project_atomic(&path, &p).expect("rewrite at the cap");
        assert!(
            load_project(&path).is_ok(),
            "exactly at the cap must still load"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bound_project_after_load_refuses_absurd_tracks_clips_and_ids() {
        // Tracks.
        let mut tracks = sample_project("T");
        tracks.timeline.tracks = (0..=MAX_TRACKS)
            .map(|_| rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: Vec::new(),
            })
            .collect();
        let err = bound_project_after_load(&mut tracks).expect_err("track cap must fire");
        assert!(err.contains(&MAX_TRACKS.to_string()), "message: {err}");

        // Clips, summed ACROSS tracks -- a hostile file can spread them out.
        let clip = rudis_core::Clip {
            id: "c".into(),
            media_id: "m".into(),
            start_us: 0,
            in_us: 0,
            out_us: 1_000,
            volume: 1.0,
            audio_detached: false,
            transform: Default::default(),
            opacity: 1.0,
            crop: Default::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        };
        let mut clips = sample_project("C");
        let per_track = (MAX_TIMELINE_CLIPS / 2) + 1;
        clips.timeline.tracks = (0..2)
            .map(|_| rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: vec![clip.clone(); per_track],
            })
            .collect();
        let err = bound_project_after_load(&mut clips).expect_err("clip cap must fire");
        assert!(err.contains(&MAX_TIMELINE_CLIPS.to_string()), "message: {err}");

        // A megabyte of id, and a megabyte of path.
        let mut fat_id = sample_project("I");
        fat_id.media_bin = vec![media_item(&"i".repeat(MAX_ID_BYTES + 1), "C:/ok.mp4")];
        assert!(
            bound_project_after_load(&mut fat_id).is_err(),
            "an over-long media id must be refused"
        );
        let mut fat_path = sample_project("P");
        fat_path.media_bin = vec![media_item("m0", &"p".repeat(MAX_ID_BYTES + 1))];
        assert!(
            bound_project_after_load(&mut fat_path).is_err(),
            "an over-long media path must be refused"
        );

        // And an ordinary project passes untouched.
        let mut ordinary = sample_project("Ordinary");
        ordinary.media_bin = vec![media_item("m0", "C:/ok.mp4")];
        assert!(bound_project_after_load(&mut ordinary).is_ok());
    }

    // -----------------------------------------------------------------------
    // Phase 60.1 -- the SAVE AS target (T-60.1-04) and read-only missing-media
    // detection (relink slice 1).
    // -----------------------------------------------------------------------

    /// As with the open type, the ONLY door is `Deserialize`.
    fn save_target(p: &Path) -> Result<RudSaveTargetPath, serde_json::Error> {
        serde_json::from_value(serde_json::Value::String(
            p.to_string_lossy().into_owned(),
        ))
    }

    #[test]
    fn rud_save_target_path_accepts_a_file_that_does_not_exist_yet() {
        let dir = temp_dir("save-new");
        let target = dir.join("Brand New.rud");
        assert!(!target.exists(), "the fixture must NOT pre-create the target");

        let validated = save_target(&target).expect("a new .rud in a real folder");
        let canonical_parent = std::fs::canonicalize(&dir).expect("canonicalize parent");
        assert_eq!(
            validated.as_path(),
            canonical_parent.join("Brand New.rud").as_path(),
            "the stored path must be the CANONICAL parent joined with the file name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rud_save_target_path_refuses_a_parent_folder_that_does_not_exist() {
        let dir = temp_dir("save-no-parent");
        let err = save_target(&dir.join("nope").join("x.rud"))
            .expect_err("a missing parent folder must be refused");
        assert!(
            format!("{err}").contains("folder"),
            "unexpected refusal message: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rud_save_target_path_refuses_a_target_that_is_already_a_directory() {
        let dir = temp_dir("save-dir");
        // Named `.rud` on purpose, so the EXTENSION check cannot be what
        // refuses it -- only the regular-file check can.
        let as_dir = dir.join("dir-target.rud");
        std::fs::create_dir_all(&as_dir).expect("create dir named like a project");

        let err = save_target(&as_dir).expect_err("a directory target must be refused");
        assert!(
            format!("{err}").contains("not a regular file"),
            "unexpected refusal message: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rud_save_target_path_refuses_a_non_rud_extension() {
        let dir = temp_dir("save-ext");
        let err = save_target(&dir.join("project.txt"))
            .expect_err("a non-.rud save target must be refused");
        assert!(
            format!("{err}").contains(".rud"),
            "unexpected refusal message: {err}"
        );
        // ...and the uppercase form is the same extension, so it is accepted.
        assert!(
            save_target(&dir.join("SHOUTY.RUD")).is_ok(),
            "an uppercase .RUD save target is the same extension"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reason T-26-01 exists, arriving through the Save As door instead of
    /// the `name` door: Windows resolves `NUL.rud` to the null device, a sink
    /// that swallows every byte and reports success -- a "saved" project that
    /// was never written. `RudSaveTargetPath` does not re-implement that check;
    /// it DELEGATES the final component to `sanitize_project_name`, which has
    /// owned the 22 device names since Phase 26.
    #[test]
    fn rud_save_target_path_refuses_the_windows_null_device_via_sanitize_project_name() {
        let dir = temp_dir("save-nul");
        for stem in ["NUL", "nul", "CON", "aux", "COM1", "LPT9"] {
            let target = dir.join(format!("{stem}.rud"));
            let err = save_target(&target)
                .err()
                .unwrap_or_else(|| panic!("{stem}.rud must be refused"));
            // The message is `sanitize_project_name`'s OWN wording, which is
            // the proof that the check was delegated rather than duplicated:
            // a second, hand-rolled device-name list here would phrase it
            // differently and this assertion would redden.
            assert!(
                format!("{err}").contains("reserved Windows device name"),
                "{stem}.rud must be refused by sanitize_project_name's own message, got: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rud_save_target_path_accepts_an_existing_rud_because_save_as_may_overwrite() {
        let dir = temp_dir("save-overwrite");
        let target = dir.join("Existing.rud");
        write_project_atomic(&target, &sample_project("Existing")).expect("write project");

        assert!(
            save_target(&target).is_ok(),
            "Save As over an existing project is legitimate -- write_project_atomic \
             replaces it atomically"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The trap this phase's security core turns on: the two types are NOT one
    /// stricter than the other, and neither can stand in for the other. Each
    /// refuses something the other accepts, and this test drives BOTH
    /// directions on real files.
    #[test]
    fn the_open_and_save_target_types_are_not_interchangeable() {
        let dir = temp_dir("save-vs-open");

        // Direction 1 -- a Save As target does not exist yet. `RudProjectPath`
        // canonicalises the WHOLE path, so it refuses every save target there
        // has ever been; `RudSaveTargetPath` canonicalises only the PARENT.
        let fresh = dir.join("Not Written Yet.rud");
        assert!(
            save_target(&fresh).is_ok(),
            "the SAVE type must accept a target that does not exist yet"
        );
        assert!(
            open_target(&fresh).is_err(),
            "the OPEN type must refuse a target that does not exist yet"
        );

        // Direction 2 -- the save target delegates the final component to
        // `sanitize_project_name`, and the open type deliberately does not. A
        // 100-character CJK stem is a legal Windows file name (100 UTF-16 units,
        // well under 255) but 300 UTF-8 BYTES, over T-26-01's 255-byte cap.
        let long_stem = "\u{5b57}".repeat(100);
        assert_eq!(long_stem.len(), 300, "the fixture must exceed the 255-byte cap");
        let long = dir.join(format!("{long_stem}.rud"));
        write_project_atomic(&long, &sample_project("Long")).expect("write a long-named project");
        assert!(
            open_target(&long).is_ok(),
            "the OPEN type accepts it: it is a real, small, regular .rud file"
        );
        assert!(
            save_target(&long).is_err(),
            "the SAVE type must refuse it, because sanitize_project_name does"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_media_returns_exactly_the_absent_ids_in_bin_order() {
        let dir = temp_dir("missing-media");
        let present_a = dir.join("a.mp4");
        let present_b = dir.join("b.mp4");
        std::fs::write(&present_a, b"not really a video").expect("write a");
        std::fs::write(&present_b, b"not really a video either").expect("write b");
        let gone = dir.join("gone.mp4");
        std::fs::write(&gone, b"about to be deleted").expect("write gone");
        std::fs::remove_file(&gone).expect("delete gone");

        let mut p = sample_project("Relink");
        p.media_bin = vec![
            media_item("m-present-a", &present_a.to_string_lossy()),
            media_item("m-gone", &gone.to_string_lossy()),
            media_item("m-present-b", &present_b.to_string_lossy()),
            // A folder is not a file, so it counts as missing too.
            media_item("m-is-a-folder", &dir.to_string_lossy()),
        ];

        assert_eq!(
            missing_media(&p),
            vec!["m-gone".to_string(), "m-is-a-folder".to_string()],
            "exactly the absent ids, in media_bin order, and IDS not paths"
        );

        // Every file present => empty, not None-shaped emptiness by accident.
        let mut all_there = sample_project("AllThere");
        all_there.media_bin = vec![
            media_item("m-present-a", &present_a.to_string_lossy()),
            media_item("m-present-b", &present_b.to_string_lossy()),
        ];
        assert!(
            missing_media(&all_there).is_empty(),
            "a project whose media is all present has nothing missing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Relink slice 1 is READ-ONLY by design: the one shape that turns a
    /// recoverable state into a lost project is a repair the user did not ask
    /// for. Nothing is rewritten and nothing is removed.
    #[test]
    fn missing_media_mutates_nothing() {
        let dir = temp_dir("missing-media-pure");
        let present = dir.join("here.mp4");
        std::fs::write(&present, b"real bytes").expect("write present");

        let mut p = sample_project("Pure");
        p.media_bin = vec![
            media_item("m-here", &present.to_string_lossy()),
            media_item("m-not-here", &dir.join("absent.mp4").to_string_lossy()),
        ];

        let before = p.clone();
        let missing = missing_media(&p);
        assert_eq!(missing, vec!["m-not-here".to_string()]);
        assert_eq!(p, before, "missing_media must not touch the Project");
        assert!(present.is_file(), "and it must not touch the files either");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
