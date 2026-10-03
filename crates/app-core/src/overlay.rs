//! The three overlay agent tools (Phase 29, OVL-02/OVL-03) — `get_overlay_
//! library`, `place_overlay` and `export_overlay_asset` — relocated here from
//! `src-tauri/src/lib.rs` by plan 45-08, together with the private helpers only
//! they reach.
//!
//! # What moved, and why exactly this set
//!
//! The batch is a SINGLE contiguous 1,022-line region of `src-tauri/src/lib.rs`
//! (18 items). The reachability closure was measured in both directions before
//! anything was cut, per this phase's standing rule, and it closes cleanly:
//! every one of the 18 items had **zero** callers anywhere outside that region
//! except through the three `run_*`/`handle_*` entry points and four
//! `#[cfg(test)]` modules. Nothing else in `src-tauri`, `generation.rs` or
//! `native_surface.rs` names any of them.
//!
//! Everything these functions call that is NOT in this module was already
//! resident: [`crate::compose`]'s `timeline_clip` / `decode_clip_frame` /
//! `engine_alpha_mode` (45-05) and [`crate::import`]'s `next_id` / `ID_SEQ` /
//! `map_kind` / `poster_cache_dir` / `detect_image_sequence` /
//! `build_sequence_item` / `MAX_IMAGE_SEQUENCE_FRAMES` (45-07). **No leaf had to
//! be duplicated and no new module was needed** — which is the thing this batch
//! was meant to demonstrate about the two largest non-`run_agent_turn` bodies.
//!
//! # ⚠ One thing the batch DID need: [`crate::AppCtx::resolve_resource`]
//!
//! `run_get_overlay_library` and `resolve_overlay_library_asset` both resolve
//! the BUNDLED overlay catalog out of the app's read-only **resource** root:
//!
//! ```text
//! app.path().resolve("overlay-library", tauri::path::BaseDirectory::Resource)
//! ```
//!
//! That is a third host directory, distinct from `app_data_dir` and
//! `app_cache_dir`, and `AppCtx` had no method for it — so the trait grew a
//! seventh, `resolve_resource(&str)`. It is the same move `block_on` was at
//! 45-07: name the HOST capability rather than let `app-core` pick a
//! replacement, because there is no shell-agnostic way to find a bundled
//! resource.
//!
//! Unlike `app_data_dir`/`app_cache_dir`, `resolve_resource` returns the host's
//! RAW error text and the two call sites below re-wrap it into their own
//! `"resolve bundled overlay-library dir: {e}"`. That is deliberate: baking one
//! consumer's message into a general accessor is what made `app_cache_dir`'s
//! string wrong for two batches (45-07's Rule-1 fix), and the message here is
//! specific to the overlay library, not to "resources" in general. The surfaced
//! string is byte-identical to the pre-move one either way.
//!
//! # The conversion, item by item — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `fn f<R: tauri::Runtime>(app: &AppHandle<R>, store: &SharedStore, ..)` | `fn f<C: AppCtx>(ctx: &C, ..)` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first body line |
//! | `app.path().resolve("..", BaseDirectory::Resource)` | `ctx.resolve_resource("..")` |
//! | `app.path().app_data_dir()` | `ctx.app_data_dir()` |
//! | `emit_changed(app, &patch, base_seq, seq)` | `ctx.emit_patch(&patch, base_seq, seq)` |
//! | `poster_cache_dir(&TauriAppCtx::new(app, store))` | `poster_cache_dir(ctx)` |
//! | `build_sequence_item(&TauriAppCtx::new(app, store), ..)` | `build_sequence_item(ctx, ..)` |
//!
//! # What did NOT change
//!
//! Every guard the trust boundary depends on is byte-identical, and each is
//! still pinned by a test that runs unmodified from where it always lived:
//!
//! * **T-29-08** — `overlay_component_is_safe`'s single-component confinement,
//!   applied to catalog ids/files and user filenames alike.
//! * **T-29-06** — the `max_entries` manifest rejection AND the two truncating
//!   `entries.len() >= max_entries` breaks in `scan_overlay_library`.
//! * **T-29-09** — `resolve_overlay_library_asset` resolves an opaque id ONLY
//!   against a server-built root; a `libraryAssetId` is never a path.
//! * **T-29-11** — `parse_overlay_transform`'s NaN/Inf rejection and the
//!   `opacity.clamp(0.0, 1.0)`.
//! * **T-29-10/12** — the SCRATCH-clone validation in `run_place_overlay`: an
//!   unknown track/media/clip id aborts BEFORE any live dispatch.
//! * **T-29-13 / SC-1** — the one-turn-one-undo property, which comes from
//!   dispatching under the caller's ALREADY-open agent turn. There is still no
//!   `begin_turn`/`end_turn` in this file.
//! * **T-29-15/16/17** — the server-built `overlay-exports` path (no agent value
//!   reaches it), the duration + `MAX_IMAGE_SEQUENCE_FRAMES` caps enforced
//!   BEFORE any decode/ffmpeg spawn, and live-state-only target resolution.
//! * **SC-4** — nothing here references the frozen H.264/MF export encoder;
//!   `encode_overlay_prores4444` / `encode_overlay_png_sequence` are the only
//!   encoders named, exactly as before.
//!
//! The `tokio::runtime::Handle::try_current()` multi-thread probe and the
//! `block_in_place` branch around the render loop are also verbatim — this
//! module needed no `AppCtx::block_on`, because that code inspects the AMBIENT
//! runtime rather than entering one.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use rudis_core::{Command, MediaBinItem};

use crate::compose::{decode_clip_frame, engine_alpha_mode, timeline_clip};
use crate::import::{
    build_sequence_item, detect_image_sequence, map_kind, next_id, poster_cache_dir, ID_SEQ,
    MAX_IMAGE_SEQUENCE_FRAMES,
};
use crate::AppCtx;

/// Phase 29 (OVL-03, T-29-06): the maximum number of overlay-library assets a
/// single `get_overlay_library` scan will enumerate. Enforced BEFORE any
/// per-entry filesystem cost (reading the catalog array's length, and bounding
/// the user-dir directory walk), mirroring `MAX_IMAGE_SEQUENCE_FRAMES` — a
/// malicious/oversized catalog or user dir cannot DoS the scan.
const MAX_OVERLAY_LIBRARY_ENTRIES: usize = 1000;

/// Phase 29 (T-29-08): a catalog `file` / user filename must be a SINGLE safe
/// path component — no separator, no `..`, no absolute/drive prefix — so a
/// crafted manifest entry can never escape the confined overlay-library root.
fn overlay_component_is_safe(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains("..")
        && s != "."
        && !s.contains(':')
}

/// Read a PNG's real pixel dimensions straight from its IHDR chunk (bytes
/// 16..24 of a valid PNG), with ZERO image-decode dependency. Returns `None`
/// for a non-PNG / truncated file — the caller falls back to width/height 0.
fn overlay_png_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 24 {
        return None;
    }
    // PNG signature + first chunk must be IHDR.
    if &bytes[0..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

/// Phase 29 (OVL-03): the pure, `AppHandle`-free catalog scan behind
/// `get_overlay_library`. Enumerates the BUNDLED catalog (a `catalog.json`
/// manifest in `bundled_dir`, `source:"bundled"`) then any user-imported PNGs
/// in `user_dir` (`source:"user"`), returning a JSON array of
/// `{id, name, category, mediaKind, isImageSequence, hasAudio, width, height,
/// source}`. Each asset's real path is resolved SERVER-SIDE, keyed by the
/// opaque `id` — the returned JSON carries NO filesystem path (T-29-05/07). The
/// scan stops once `max_entries` is reached (T-29-06 truncation), and every
/// resolved file is confined to its own root by `overlay_component_is_safe`
/// (T-29-08). Split out from the handle/app layer so it is unit-testable
/// against real directories (plain temp dirs — no app shell at all).
fn scan_overlay_library(
    bundled_dir: &std::path::Path,
    user_dir: &std::path::Path,
    max_entries: usize,
) -> Result<String, String> {
    let mut entries: Vec<serde_json::Value> = Vec::new();

    // --- Bundled catalog (catalog.json manifest) ---
    let catalog_path = bundled_dir.join("catalog.json");
    if catalog_path.is_file() {
        let raw = std::fs::read_to_string(&catalog_path)
            .map_err(|e| format!("read overlay catalog.json: {e}"))?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&raw)
            .map_err(|e| format!("parse overlay catalog.json: {e}"))?;
        // T-29-06: reject an oversized manifest BEFORE any per-entry file I/O.
        if items.len() > max_entries {
            return Err(format!(
                "overlay catalog too large ({} entries > cap {max_entries})",
                items.len()
            ));
        }
        for item in items {
            if entries.len() >= max_entries {
                break;
            }
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let file = item.get("file").and_then(|v| v.as_str()).unwrap_or("");
            // T-29-08: skip any entry whose id/file is not a safe component.
            if !overlay_component_is_safe(id) || !overlay_component_is_safe(file) {
                continue;
            }
            let path = bundled_dir.join(file);
            if !path.is_file() {
                continue;
            }
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or(id);
            let category = item
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("overlay");
            let (w, h) = overlay_png_dimensions(&path).unwrap_or((0, 0));
            entries.push(serde_json::json!({
                "id": id,
                "name": name,
                "category": category,
                "mediaKind": "image",
                "isImageSequence": false,
                "hasAudio": false,
                "width": w,
                "height": h,
                "source": "bundled",
            }));
        }
    }

    // --- User-imported assets (a flat dir of PNGs, no manifest) ---
    if user_dir.is_dir() {
        let rd = std::fs::read_dir(user_dir)
            .map_err(|e| format!("read overlay user dir: {e}"))?;
        for de in rd {
            // T-29-06: stop the walk once the cap is hit (truncation), BEFORE
            // any further per-entry filesystem cost.
            if entries.len() >= max_entries {
                break;
            }
            let de = match de {
                Ok(de) => de,
                Err(_) => continue,
            };
            let path = de.path();
            if !path.is_file() {
                continue;
            }
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            // Only .png assets; skip anything unsafe.
            if !overlay_component_is_safe(&file_name)
                || !file_name.to_ascii_lowercase().ends_with(".png")
            {
                continue;
            }
            let id = file_name.trim_end_matches(".png").trim_end_matches(".PNG");
            let (w, h) = overlay_png_dimensions(&path).unwrap_or((0, 0));
            entries.push(serde_json::json!({
                "id": id,
                "name": id,
                "category": "user",
                "mediaKind": "image",
                "isImageSequence": false,
                "hasAudio": false,
                "width": w,
                "height": h,
                "source": "user",
            }));
        }
    }

    serde_json::to_string(&entries).map_err(|e| e.to_string())
}

/// Phase 29 (OVL-03): resolve the two confined overlay-library roots from the
/// [`AppCtx`] — the BUNDLED resource dir (`resource_dir()/overlay-library`,
/// the SAME `BaseDirectory::Resource` discovery `point_engine_at_bundled_ffmpeg`
/// uses) and the persistent user dir (`app_data_dir()/overlay-library`) — then
/// hand both to the pure [`scan_overlay_library`]. Never accepts a client path.
fn run_get_overlay_library<C: AppCtx>(ctx: &C) -> Result<String, String> {
    let bundled_dir = ctx
        .resolve_resource("overlay-library")
        .map_err(|e| format!("resolve bundled overlay-library dir: {e}"))?;
    // The user dir is best-effort: if app_data_dir is unavailable, scan bundled
    // only (an empty/nonexistent user dir is not an error).
    let user_dir = ctx
        .app_data_dir()
        .map(|d| d.join("overlay-library"))
        .unwrap_or_else(|_| std::path::PathBuf::from("overlay-library-missing"));
    scan_overlay_library(&bundled_dir, &user_dir, MAX_OVERLAY_LIBRARY_ENTRIES)
}

/// Phase 29: the `get_overlay_library` interception. Read-only (no input, no
/// mutation); never panics — any failure becomes an `is_error` tool_result.
pub fn handle_get_overlay_library<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_get_overlay_library(ctx) {
        Ok(json) => result(json, None),
        Err(e) => result(format!("get_overlay_library failed: {e}"), Some(true)),
    }
}

/// Phase 29 (OVL-02): the project-local media-bin folder a library overlay
/// imports into. Created iff absent before `AddMediaBinItem` (Pitfall 3 —
/// `AddMediaBinItem::apply` REJECTS a nonexistent folder).
const OVERLAYS_FOLDER: &str = "Overlays";

/// Outcome of a successful [`run_place_overlay`] — the placed clip id, its
/// resolved media id, and whether a library asset was imported this call.
pub struct PlaceOverlayOutcome {
    clip_id: String,
    media_id: String,
    imported_from_library: bool,
}

/// Phase 29 (OVL-02, T-29-09): resolve an opaque overlay-library asset id to its
/// confined real path SERVER-SIDE — against the bundled `catalog.json` manifest
/// (id -> file) and the user dir (`{id}.png`), the SAME two roots
/// [`run_get_overlay_library`] scans. A `libraryAssetId` is therefore NEVER
/// interpolated as a client-supplied path (Pitfall 5): every id/file component
/// is confined by [`overlay_component_is_safe`] (T-29-08), and the returned path
/// is always a child of a server-resolved root. Returns Err if the id matches no
/// catalog entry / user asset.
fn resolve_overlay_library_asset<C: AppCtx>(
    ctx: &C,
    asset_id: &str,
) -> Result<std::path::PathBuf, String> {
    if !overlay_component_is_safe(asset_id) {
        return Err(format!("invalid overlay library asset id: {asset_id}"));
    }
    // --- bundled catalog (catalog.json manifest: id -> file) ---
    let bundled_dir = ctx
        .resolve_resource("overlay-library")
        .map_err(|e| format!("resolve bundled overlay-library dir: {e}"))?;
    let catalog_path = bundled_dir.join("catalog.json");
    if catalog_path.is_file() {
        let raw = std::fs::read_to_string(&catalog_path)
            .map_err(|e| format!("read overlay catalog.json: {e}"))?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&raw)
            .map_err(|e| format!("parse overlay catalog.json: {e}"))?;
        for item in items {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let file = item.get("file").and_then(|v| v.as_str()).unwrap_or("");
            if id == asset_id && overlay_component_is_safe(id) && overlay_component_is_safe(file) {
                let path = bundled_dir.join(file);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }
    // --- user-imported assets (a flat dir of PNGs; id == filename stem) ---
    let file = format!("{asset_id}.png");
    if overlay_component_is_safe(&file) {
        if let Ok(user_dir) = ctx.app_data_dir().map(|d| d.join("overlay-library")) {
            let path = user_dir.join(&file);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(format!("unknown overlay library asset id: {asset_id}"))
}

/// Phase 29 (OVL-02, T-29-11): parse the optional `transform` object into a
/// [`rudis_core::ClipTransform`], REJECTING any non-finite (NaN/Inf) component —
/// the SAME finite guard `SetClipTransform` applies at apply-time, enforced HERE
/// before any dispatch. Absent/`null` transform -> `None` (place at identity).
fn parse_overlay_transform(
    v: Option<&serde_json::Value>,
) -> Result<Option<rudis_core::ClipTransform>, String> {
    let Some(t) = v else {
        return Ok(None);
    };
    if t.is_null() {
        return Ok(None);
    }
    let vec2 = |key: &str| -> Result<(f32, f32), String> {
        let arr = t
            .get(key)
            .and_then(|x| x.as_array())
            .ok_or_else(|| format!("transform.{key} must be a [x, y] array"))?;
        if arr.len() != 2 {
            return Err(format!("transform.{key} must have exactly 2 numbers"));
        }
        let a = arr[0]
            .as_f64()
            .ok_or_else(|| format!("transform.{key}[0] must be a number"))?;
        let b = arr[1]
            .as_f64()
            .ok_or_else(|| format!("transform.{key}[1] must be a number"))?;
        if !a.is_finite() || !b.is_finite() {
            return Err(format!("transform.{key} must be finite (no NaN/Inf)"));
        }
        Ok((a as f32, b as f32))
    };
    let position = vec2("position")?;
    let scale = vec2("scale")?;
    let rot = t
        .get("rotation_deg")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| "transform.rotation_deg must be a number".to_string())?;
    if !rot.is_finite() {
        return Err("transform.rotation_deg must be finite (no NaN/Inf)".to_string());
    }
    Ok(Some(rudis_core::ClipTransform {
        position,
        scale,
        rotation_deg: rot as f32,
    }))
}

/// Phase 29 (OVL-02): the `place_overlay` interception — import-if-needed +
/// place + style a REUSABLE OVERLAY asset (from `get_overlay_library`) OR an
/// already-imported media item as ONE composited layer, composing EXISTING
/// Commands only (`CreateMediaFolder` / `AddMediaBinItem` / `AddClip` — ZERO new
/// `Command` variants). The one-turn-one-undo guarantee (SC-1 / T-29-13) is FREE
/// from the whole-turn `begin_turn()`/`end_turn()` bracket: every dispatch below
/// is a plain `store.dispatch()` under the ALREADY-open agent turn (no nested
/// transaction), so all 1-3 commands auto-join ONE undo group.
///
/// Security: a `libraryAssetId` resolves ONLY against the server-built catalog
/// (`resolve_overlay_library_asset`, T-29-09 — never a client path); numeric
/// transform/opacity are finite-checked and opacity clamped 0-1 (T-29-11);
/// unknown track/media/clip-id are rejected against a SCRATCH clone BEFORE any
/// live dispatch (T-29-10/12), so a rejected call mutates nothing.
///
/// Reuses `Tool::PlaceClip::resolve` to build the AddClip (Pattern C: a complete
/// `Clip` literal with the REAL `media_fps`/`placement_len_us`), then overrides
/// its transform/opacity/alpha_mode with the caller's — the ONE place
/// `SetClipAlphaMode`'s `AlphaMode` becomes agent-reachable (Phase 28 shipped the
/// command but never wired it to the agent).
pub fn run_place_overlay<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<PlaceOverlayOutcome, String> {
    // Plan 45-08 (45-06's recipe): the `store: &SharedStore` parameter became
    // `ctx.store()`, bound on the first body line so every `store.lock()`
    // below is byte-identical to the pre-move code. At every call site the
    // ctx is built from the SAME `&SharedStore` the old call passed.
    let store = ctx.store();

    // 1. Parse + validate EVERYTHING before any dispatch.
    let library_asset_id = input
        .get("libraryAssetId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let media_id_arg = input
        .get("mediaId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    match (library_asset_id, media_id_arg) {
        (Some(_), Some(_)) => {
            return Err("supply EITHER libraryAssetId OR mediaId, not both".to_string())
        }
        (None, None) => {
            return Err("supply exactly one of libraryAssetId or mediaId".to_string())
        }
        _ => {}
    }
    let clip_id = input
        .get("clipId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "clipId is required".to_string())?
        .to_string();
    let track = input
        .get("track")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "track is required".to_string())?
        .to_string();
    let start_frame = input
        .get("startFrame")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "startFrame is required (integer)".to_string())?;

    let transform = parse_overlay_transform(input.get("transform"))?;
    let opacity = match input.get("opacity") {
        Some(v) if !v.is_null() => {
            let o = v
                .as_f64()
                .ok_or_else(|| "opacity must be a number".to_string())?;
            if !o.is_finite() {
                return Err("opacity must be finite (no NaN/Inf)".to_string());
            }
            Some((o as f32).clamp(0.0, 1.0))
        }
        _ => None,
    };
    let alpha_mode = match input.get("alphaMode").and_then(|v| v.as_str()) {
        None | Some("straight") => rudis_core::AlphaMode::Straight,
        Some("premultiplied") => rudis_core::AlphaMode::Premultiplied,
        Some(other) => {
            return Err(format!(
                "unknown alphaMode: {other} (expected straight|premultiplied)"
            ))
        }
    };

    // 2. Snapshot; build the validated command list against a SCRATCH clone so a
    //    bad track/media/clip-id aborts BEFORE any live dispatch (all-or-none,
    //    T-29-10/12). No core::Command::apply is needed: pending media/folder are
    //    pushed onto the scratch, and Tool::PlaceClip::resolve validates the
    //    placement against it.
    let mut scratch = {
        store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?
            .snapshot()
    };
    let mut pending: Vec<Command> = Vec::new();
    let mut created_poster: Option<String> = None;

    let media_id: String = if let Some(lib_id) = library_asset_id {
        // Resolve id -> confined path SERVER-SIDE (never a client path, T-29-09).
        let src_path = resolve_overlay_library_asset(ctx, lib_id)?;
        let info = engine::probe(&src_path).map_err(|e| format!("probe overlay asset: {e}"))?;
        let id = next_id("media");
        // Best-effort poster (image -> t=0; video -> mid). Failure downgrades to
        // None, never aborts (mirrors run_import_media).
        let poster_dir = poster_cache_dir(ctx)?;
        let poster_path = match info.media_kind {
            engine::MediaKind::Audio => None,
            kind => {
                let at_seconds = if kind == engine::MediaKind::Video {
                    (info.duration_us as f64 / 2_000_000.0).max(0.0)
                } else {
                    0.0
                };
                let out = poster_dir.join(format!("{id}.png"));
                match engine::generate_poster(&src_path, &out, at_seconds) {
                    Ok(()) => {
                        let p = out.to_string_lossy().into_owned();
                        created_poster = Some(p.clone());
                        Some(p)
                    }
                    Err(e) => {
                        eprintln!("place_overlay: no poster for {}: {e}", src_path.display());
                        None
                    }
                }
            }
        };
        let item = MediaBinItem {
            id: id.clone(),
            path: src_path.to_string_lossy().into_owned(),
            media_kind: map_kind(info.media_kind),
            duration_us: info.duration_us,
            width: info.width,
            height: info.height,
            fps: info.avg_frame_rate,
            is_vfr: info.is_vfr,
            rotation_degrees: info.rotation_degrees,
            has_audio: info.has_audio,
            poster_path,
            folder: OVERLAYS_FOLDER.to_string(),
            display_name: None,
            is_image_sequence: false,
            // Phase 60 (OCCL-01): overlays are the population most likely to be
            // genuinely transparent, so this comes from the source's own probe
            // through the shared narrowing — never assumed opaque.
            reports_alpha: crate::import::probed_alpha(&info),
        };
        // Guarded folder create (Pitfall 3 / T-29-12): AddMediaBinItem REJECTS a
        // nonexistent folder, so create "Overlays" FIRST iff absent.
        if !scratch.media_folders.iter().any(|f| f == OVERLAYS_FOLDER) {
            pending.push(Command::CreateMediaFolder {
                path: OVERLAYS_FOLDER.to_string(),
            });
            scratch.media_folders.push(OVERLAYS_FOLDER.to_string());
        }
        pending.push(Command::AddMediaBinItem(item.clone()));
        scratch.media_bin.push(item);
        id
    } else {
        media_id_arg
            .expect("exactly-one check guarantees mediaId is present")
            .to_string()
    };

    // 3. Build the AddClip via the SAME core placement logic placeClip uses
    //    (Pattern C: a complete Clip literal with the real media_fps /
    //    placement_len_us). Unknown track/media/clip-id is rejected HERE against
    //    the scratch — nothing has been dispatched yet.
    let place = rudis_core::tools::Tool::PlaceClip(rudis_core::tools::PlaceClipArgs {
        clip_id: clip_id.clone(),
        media_id: media_id.clone(),
        track: track.clone(),
        start_frame,
    });
    let resolved = place.resolve(&scratch).map_err(|e| e.to_string())?;
    let add_clip = match resolved.into_iter().next() {
        Some(Command::AddClip {
            track: track_idx,
            mut clip,
        }) => {
            if let Some(t) = transform {
                clip.transform = t;
            }
            if let Some(o) = opacity {
                clip.opacity = o;
            }
            clip.alpha_mode = alpha_mode;
            Command::AddClip {
                track: track_idx,
                clip,
            }
        }
        _ => return Err("place_overlay: PlaceClip did not resolve to an AddClip".to_string()),
    };
    pending.push(add_clip);

    // 4. Dispatch every pending command under the ALREADY-open agent turn (no
    //    nested begin/end_turn — they auto-join ONE undo group, SC-1/T-29-13),
    //    emitting project:changed per patch. On a dispatch failure remove any
    //    just-written poster (the only file this tool writes).
    for cmd in pending {
        let dispatched = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())
            .and_then(|mut g| g.dispatch(cmd).map_err(|e| e.to_string()));
        match dispatched {
            Ok((patch, base_seq, seq)) => ctx.emit_patch(&patch, base_seq, seq)?,
            Err(e) => {
                if let Some(p) = &created_poster {
                    let _ = std::fs::remove_file(p);
                }
                return Err(e);
            }
        }
    }

    Ok(PlaceOverlayOutcome {
        clip_id,
        media_id,
        imported_from_library: library_asset_id.is_some(),
    })
}

/// Phase 29 (OVL-02): the `place_overlay` interception. Never panics — a parse /
/// resolve / probe / dispatch failure becomes an `is_error` text tool_result so
/// the agent can react, never a partial mutation the agent can't see.
pub fn handle_place_overlay<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_place_overlay(ctx, input) {
        Ok(o) => {
            let src = if o.imported_from_library {
                format!("imported from the overlay library into \"{OVERLAYS_FOLDER}\" and ")
            } else {
                String::new()
            };
            result(
                format!(
                    "Placed overlay clip {} (media {}) — {}composited as a new layer.",
                    o.clip_id, o.media_id, src
                ),
                None,
            )
        }
        Err(e) => result(format!("place_overlay failed: {e}"), Some(true)),
    }
}

/// Phase 29 (OVL-03, T-29-15): the CONFINED, server-built output dir for
/// exported overlay assets — `app_data_dir()/overlay-exports`. The
/// `export_overlay_asset` schema carries NO path arg, so no agent value ever
/// reaches the write path (mirrors create_matte's `generated/` confinement).
const OVERLAY_EXPORTS_DIR: &str = "overlay-exports";

/// Outcome of a successful [`run_export_overlay_asset`] — the re-imported
/// asset's media id + confined path, the format used, how many frames were
/// rendered, and whether it re-imported as a numbered image sequence.
pub struct ExportOverlayOutcome {
    pub media_id: String,
    pub path: String,
    // `format` is the ONLY field with no reader outside this module, so it is
    // the only one that did not need widening (45-14's re-privatization
    // worklist therefore covers the other four, not all five).
    format: String,
    pub frame_count: usize,
    pub is_image_sequence: bool,
}

/// The source an overlay-asset export renders — resolved from a single timeline
/// clip OR a MediaBin item under a SHORT lock (D-07), reduced to exactly what
/// the transparent composite + license-clean encode need. The lock is dropped
/// before ANY decode/GPU/ffmpeg work.
struct ResolvedOverlaySource {
    path: PathBuf,
    rotation: u32,
    is_seq: bool,
    seq_fps: f64,
    /// A still (probed `duration_us <= 0`): decode+composite ONCE and replicate
    /// across the window instead of spawning one ffmpeg decode per output tick.
    is_still: bool,
    start_us: i64,
    end_us: i64,
    opacity: f32,
    transform: engine::LayerTransform,
    crop: engine::LayerCrop,
    alpha_mode: engine::AlphaMode,
}

/// Probe a REAL just-written file (ProRes `.mov`, or a single PNG) into a
/// `MediaBinItem` with a poster (folder empty; the caller sets it). Mirrors
/// `import_media`'s single-file branch — never panics; a probe failure is a
/// clean `Err`.
fn build_probed_overlay_item(
    poster_dir: &Path,
    path: &Path,
) -> Result<MediaBinItem, String> {
    let info = engine::probe(path).map_err(|e| format!("probe exported overlay asset: {e}"))?;
    let id = next_id("media");
    let poster_path = match info.media_kind {
        engine::MediaKind::Audio => None,
        kind => {
            let at_seconds = if kind == engine::MediaKind::Video {
                (info.duration_us as f64 / 2_000_000.0).max(0.0)
            } else {
                0.0
            };
            let out = poster_dir.join(format!("{id}.png"));
            match engine::generate_poster(path, &out, at_seconds) {
                Ok(()) => Some(out.to_string_lossy().into_owned()),
                Err(e) => {
                    eprintln!(
                        "export_overlay_asset: no poster for {}: {e}",
                        path.display()
                    );
                    None
                }
            }
        }
    };
    Ok(MediaBinItem {
        id,
        path: path.to_string_lossy().into_owned(),
        media_kind: map_kind(info.media_kind),
        duration_us: info.duration_us,
        width: info.width,
        height: info.height,
        fps: info.avg_frame_rate,
        is_vfr: info.is_vfr,
        rotation_degrees: info.rotation_degrees,
        has_audio: info.has_audio,
        poster_path,
        folder: String::new(),
        display_name: None,
        is_image_sequence: false,
        // Phase 60 (OCCL-01): a baked overlay asset is a TRANSPARENT asset by
        // intent (PNG sequence / ProRes 4444), so the probe's answer matters
        // here more than anywhere — and it is the probe's, not ours.
        reports_alpha: crate::import::probed_alpha(&info),
    })
}

/// Phase 29 (OVL-03): the `export_overlay_asset` interception body — bake ONE
/// overlay clip (or MediaBin item) into a self-contained REUSABLE TRANSPARENT
/// asset (PNG image sequence OR ProRes 4444) and re-import it as a MediaBinItem,
/// mirroring `create_matte`'s produce-then-re-import shape.
///
/// SC-3: renders each output frame through the alpha-preserving
/// [`engine::Compositor::composite_layers_to_rgba_transparent`] (Plan 29-01) —
/// the clip's own transform/opacity/alpha_mode/crop baked in — then encodes via
/// the LICENSE-CLEAN native [`engine::encode_overlay_png_sequence`] /
/// [`engine::encode_overlay_prores4444`]. It NEVER references the frozen
/// license-safe H.264/MF export encoder, so that export path stays
/// byte-untouched (SC-4, T-29-14).
///
/// Security: EXACTLY ONE of `clipId`/`mediaId`, resolved against LIVE
/// timeline/media_bin only (never a raw path, T-29-17). The output dir/filename
/// are SERVER-BUILT under `app_data_dir()/overlay-exports` — no agent path arg
/// exists (T-29-15). A duration/frame-count cap is enforced BEFORE any decode or
/// ffmpeg spawn (T-29-16). ZERO new `Command` variants: it composes the existing
/// `CreateMediaFolder`/`AddMediaBinItem` under the already-open agent turn.
pub fn run_export_overlay_asset<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<ExportOverlayOutcome, String> {
    // Plan 45-08 (45-06's recipe): the `store: &SharedStore` parameter became
    // `ctx.store()`, bound on the first body line so every `store.lock()`
    // below is byte-identical to the pre-move code. At every call site the
    // ctx is built from the SAME `&SharedStore` the old call passed.
    let store = ctx.store();

    // 1. Parse EXACTLY ONE target + the required alpha-preserving format.
    let clip_id = input
        .get("clipId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let media_id_arg = input
        .get("mediaId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    match (clip_id, media_id_arg) {
        (Some(_), Some(_)) => {
            return Err("supply EITHER clipId OR mediaId, not both".to_string())
        }
        (None, None) => return Err("supply exactly one of clipId or mediaId".to_string()),
        _ => {}
    }
    let format = input
        .get("format")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "format is required (png_sequence|prores4444)".to_string())?;
    if format != "png_sequence" && format != "prores4444" {
        return Err(format!(
            "unknown format: {format} (expected png_sequence|prores4444)"
        ));
    }
    let folder = input
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // 2. Resolve the source + project res/fps under a SHORT lock, then DROP it
    //    (the resolved clip/media is a plain-path lookup against LIVE state —
    //    never a raw caller path, T-29-17).
    let (out_w, out_h, fps, src) = {
        let guard = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        let project = guard.snapshot();
        let media_of = |mid: &str| project.media_bin.iter().find(|m| m.id == mid).cloned();
        let src = if let Some(cid) = clip_id {
            let clip = timeline_clip(&project.timeline, cid)
                .ok_or_else(|| format!("clip not found on the timeline: {cid}"))?
                .clone();
            let media = media_of(&clip.media_id).ok_or_else(|| {
                format!("clip {cid}'s media is missing from the bin: {}", clip.media_id)
            })?;
            ResolvedOverlaySource {
                path: PathBuf::from(&media.path),
                rotation: media.rotation_degrees,
                is_seq: media.is_image_sequence,
                seq_fps: media.fps,
                is_still: media.duration_us <= 0,
                start_us: clip.in_us,
                end_us: clip.out_us,
                opacity: clip.opacity,
                transform: engine::LayerTransform {
                    position: clip.transform.position,
                    scale: clip.transform.scale,
                    rotation_deg: clip.transform.rotation_deg,
                },
                crop: engine::LayerCrop {
                    left: clip.crop.left,
                    top: clip.crop.top,
                    right: clip.crop.right,
                    bottom: clip.crop.bottom,
                },
                alpha_mode: engine_alpha_mode(clip.alpha_mode),
            }
        } else {
            let mid = media_id_arg.expect("exactly-one check guarantees mediaId is present");
            let media = media_of(mid).ok_or_else(|| format!("media bin item not found: {mid}"))?;
            let is_still = media.duration_us <= 0;
            let end_us = if is_still { 0 } else { media.duration_us };
            ResolvedOverlaySource {
                path: PathBuf::from(&media.path),
                rotation: media.rotation_degrees,
                is_seq: media.is_image_sequence,
                seq_fps: media.fps,
                is_still,
                start_us: 0,
                end_us,
                // A bare media item carries no on-timeline styling: identity
                // transform, full opacity, straight alpha.
                opacity: 1.0,
                transform: engine::LayerTransform::default(),
                crop: engine::LayerCrop::default(),
                alpha_mode: engine::AlphaMode::Straight,
            }
        };
        (project.width, project.height, project.fps, src)
    };

    // 3. DoS cap (T-29-16): frame count + duration, rejected BEFORE any decode /
    //    ffmpeg spawn (mirrors MAX_SCENE_DURATION_SECONDS / MAX_IMAGE_SEQUENCE_
    //    FRAMES). The window is the clip's own [in_us, out_us) (a still held for
    //    its clip duration is that many frames) or the media's full duration.
    let step_us = engine::frame_step_us(fps).max(1);
    let window_us = (src.end_us - src.start_us).max(0);
    let frame_count: usize = if window_us <= 0 {
        1
    } else {
        (((window_us + step_us - 1) / step_us).max(1)) as usize
    };
    if (window_us as f64 / 1_000_000.0) > rudis_core::MAX_SCENE_DURATION_SECONDS {
        return Err(format!(
            "overlay export duration {:.2}s exceeds the {}s cap",
            window_us as f64 / 1_000_000.0,
            rudis_core::MAX_SCENE_DURATION_SECONDS
        ));
    }
    if frame_count > MAX_IMAGE_SEQUENCE_FRAMES {
        return Err(format!(
            "overlay export frame count {frame_count} exceeds the {MAX_IMAGE_SEQUENCE_FRAMES}-frame cap (DoS guard)"
        ));
    }

    // 4. Render every output frame through the ALPHA-PRESERVING transparent-clear
    //    compositor (Plan 29-01) — the clip's own transform/opacity/alpha_mode/
    //    crop baked in. A still is decoded+composited ONCE and replicated (its
    //    per-tick output is provably identical), avoiding one ffmpeg decode spawn
    //    per frame. Run under block_in_place on a multi-thread runtime (the
    //    create_matte guard); inline under the tokio-less MockRuntime.
    let compositor =
        engine::Compositor::new().map_err(|e| format!("start overlay compositor: {e}"))?;
    let render = || -> Result<Vec<engine::Frame>, String> {
        let build_layer = |frame: engine::Frame| engine::Layer {
            frame,
            opacity: src.opacity,
            transform: src.transform,
            crop: src.crop,
            alpha_mode: src.alpha_mode,
        };
        let mut frames: Vec<engine::Frame> = Vec::with_capacity(frame_count);
        if src.is_still || window_us <= 0 {
            let frame =
                decode_clip_frame(&src.path, src.start_us, src.rotation, src.is_seq, src.seq_fps)
                    .map_err(|e| {
                        format!("decode overlay source at {}us: {e}", src.start_us)
                    })?;
            let rgba = compositor
                .composite_layers_to_rgba_transparent(&[build_layer(frame)], out_w, out_h)
                .map_err(|e| format!("composite transparent overlay frame: {e}"))?;
            for _ in 0..frame_count {
                frames.push(engine::Frame {
                    width: out_w,
                    height: out_h,
                    rgba: rgba.clone(),
                });
            }
        } else {
            let mut t = src.start_us;
            while t < src.end_us {
                let frame =
                    decode_clip_frame(&src.path, t, src.rotation, src.is_seq, src.seq_fps)
                        .map_err(|e| format!("decode overlay source at {t}us: {e}"))?;
                let rgba = compositor
                    .composite_layers_to_rgba_transparent(&[build_layer(frame)], out_w, out_h)
                    .map_err(|e| format!("composite transparent overlay frame at {t}us: {e}"))?;
                frames.push(engine::Frame {
                    width: out_w,
                    height: out_h,
                    rgba,
                });
                t += step_us;
            }
        }
        if frames.is_empty() {
            return Err("overlay export produced no frames".to_string());
        }
        Ok(frames)
    };
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let frames = if on_multi_thread {
        tokio::task::block_in_place(render)
    } else {
        render()
    }?;

    // 5. Encode via the LICENSE-CLEAN native encoders ONLY (png / prores_ks) into
    //    a CONFINED, server-built path — NEVER the frozen H.264/MF export encoder
    //    (SC-4). No agent value reaches the write path (T-29-15).
    // `TauriAppCtx::app_data_dir` returns this call's EXACT former error
    // string (`"resolve app data dir: {e}"`), so the surfaced message is
    // byte-identical — checked per 45-07's carry-forward.
    let dir = ctx.app_data_dir()?.join(OVERLAY_EXPORTS_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create overlay-exports dir: {e}"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let base = format!("overlay-{millis}-{seq}");

    // `probe_path` is the file we probe/re-import; `cleanup_dir`/`cleanup_file`
    // is what a later failure best-effort removes (a subdir for a sequence, one
    // file for prores).
    let (probe_path, is_png_output, cleanup_file, cleanup_dir): (
        PathBuf,
        bool,
        Option<PathBuf>,
        Option<PathBuf>,
    ) = if format == "prores4444" {
        let out_path = dir.join(format!("{base}.mov"));
        engine::encode_overlay_prores4444(&frames, fps, &out_path).map_err(|e| {
            let _ = std::fs::remove_file(&out_path);
            format!("encode ProRes 4444 overlay: {e}")
        })?;
        (out_path.clone(), false, Some(out_path), None)
    } else {
        let out_subdir = dir.join(&base);
        let paths = engine::encode_overlay_png_sequence(&frames, fps, &out_subdir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&out_subdir);
            format!("encode PNG-sequence overlay: {e}")
        })?;
        let first = paths.into_iter().next().ok_or_else(|| {
            let _ = std::fs::remove_dir_all(&out_subdir);
            "PNG-sequence export produced no frames".to_string()
        })?;
        (first, true, None, Some(out_subdir))
    };
    let remove_output = || {
        if let Some(d) = &cleanup_dir {
            let _ = std::fs::remove_dir_all(d);
        }
        if let Some(f) = &cleanup_file {
            let _ = std::fs::remove_file(f);
        }
    };

    // 6. Re-import the produced asset. A multi-frame PNG output re-imports as a
    //    numbered image sequence (build_sequence_item, is_image_sequence=true); a
    //    single PNG re-imports as a still image; prores re-imports as a Video.
    let poster_dir = match poster_cache_dir(ctx) {
        Ok(d) => d,
        Err(e) => {
            remove_output();
            return Err(e);
        }
    };
    let mut item: MediaBinItem = if is_png_output {
        match detect_image_sequence(&probe_path) {
            Ok(Some(det)) => match build_sequence_item(ctx, &poster_dir, &det, fps) {
                Ok(it) => it,
                Err(e) => {
                    remove_output();
                    return Err(format!("re-import overlay PNG sequence: {e}"));
                }
            },
            Ok(None) => match build_probed_overlay_item(&poster_dir, &probe_path) {
                Ok(it) => it,
                Err(e) => {
                    remove_output();
                    return Err(e);
                }
            },
            Err(e) => {
                remove_output();
                return Err(format!("detect overlay PNG sequence: {e}"));
            }
        }
    } else {
        match build_probed_overlay_item(&poster_dir, &probe_path) {
            Ok(it) => it,
            Err(e) => {
                remove_output();
                return Err(e);
            }
        }
    };
    item.folder = folder.clone();

    // 7. Dispatch under the ALREADY-open agent turn (ZERO new Command variants):
    //    guard-create the target folder iff a non-empty folder is absent (Add
    //    MediaBinItem rejects a nonexistent folder), then AddMediaBinItem. On a
    //    dispatch failure the orphaned output + poster are best-effort removed.
    let mut pending: Vec<Command> = Vec::new();
    if !folder.is_empty() {
        let exists = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?
            .snapshot()
            .media_folders
            .iter()
            .any(|f| f == &folder);
        if !exists {
            pending.push(Command::CreateMediaFolder {
                path: folder.clone(),
            });
        }
    }
    pending.push(Command::AddMediaBinItem(item.clone()));

    for cmd in pending {
        let dispatched = store
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())
            .and_then(|mut g| g.dispatch(cmd).map_err(|e| e.to_string()));
        match dispatched {
            Ok((patch, base_seq, seq)) => ctx.emit_patch(&patch, base_seq, seq)?,
            Err(e) => {
                remove_output();
                if let Some(pp) = &item.poster_path {
                    let _ = std::fs::remove_file(pp);
                }
                return Err(e);
            }
        }
    }

    Ok(ExportOverlayOutcome {
        media_id: item.id.clone(),
        path: item.path.clone(),
        format: format.to_string(),
        frame_count,
        is_image_sequence: item.is_image_sequence,
    })
}

/// Phase 29 (OVL-03): the `export_overlay_asset` interception. Never panics — a
/// bad target/format, a decode/encode/probe failure, or a rejected folder
/// becomes an `is_error` text tool_result (with the orphaned output cleaned up),
/// never a partial mutation.
pub fn handle_export_overlay_asset<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_export_overlay_asset(ctx, input) {
        Ok(o) => {
            let kind = if o.is_image_sequence {
                "PNG image sequence"
            } else if o.format == "prores4444" {
                "ProRes 4444 video"
            } else {
                "transparent PNG image"
            };
            result(
                format!(
                    "Exported the overlay as a reusable transparent {kind} ({} frame(s)) and \
                     re-imported it into the media bin as {} ({}).",
                    o.frame_count, o.media_id, o.path
                ),
                None,
            )
        }
        Err(e) => result(format!("export_overlay_asset failed: {e}"), Some(true)),
    }
}
/// Phase 29 (OVL-03, SC-2 listing half): `get_overlay_library` lists the
/// reusable overlay-asset catalog (bundled + user-imported) by OPAQUE server-
/// resolved id, never a raw filesystem path, and is bounded against a DoS scan.
#[cfg(test)]
mod overlay_library {
    use super::*;
    use crate::test_support::TestAppCtx;

    /// The REAL shipped bundled catalog dir, so the test exercises the genuine
    /// product assets, not a synthetic fixture. Plan 45-08 changed only the
    /// relative hop (`crates/app-core` walks back up two levels); Phase 55 plan
    /// 55-01 then relocated the catalog itself out of
    /// `src-tauri/resources/overlay-library` to `runtime/resources/overlay-library`,
    /// so the bundled assets survive GATE-07's deletion of the Tauri shell.
    fn real_bundled_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../runtime/resources/overlay-library")
    }

    /// A throwaway, per-test temp dir (removed + recreated fresh).
    fn fresh_temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rudis-ovl-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Copy a real bundled PNG into `dir/name` so user-dir tests use genuine
    /// alpha-carrying PNG bytes (real IHDR dimensions), not fabricated files.
    fn seed_real_png(dir: &std::path::Path, name: &str) {
        let src = real_bundled_dir().join("sticker-badge.png");
        std::fs::copy(&src, dir.join(name)).expect("copy real png fixture");
    }

    /// SC-2: the bundled catalog lists >= 2 real assets, each carrying an opaque
    /// id + metadata + real PNG dimensions, all `source:"bundled"` — and the
    /// JSON exposes NO `path` field and NO raw filesystem path (T-29-05/07).
    #[test]
    fn bundled_catalog_lists_real_assets_by_opaque_id() {
        let bundled = real_bundled_dir();
        let empty_user = fresh_temp("bundled-empty-user");
        let json = scan_overlay_library(&bundled, &empty_user, MAX_OVERLAY_LIBRARY_ENTRIES)
            .expect("scan ok");
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("valid JSON array");

        assert!(
            entries.len() >= 2,
            "the bundled catalog must list >= 2 assets, got {}: {json}",
            entries.len()
        );
        for e in &entries {
            assert_eq!(e["source"], serde_json::json!("bundled"), "bundled source");
            assert!(e["id"].as_str().is_some_and(|s| !s.is_empty()), "opaque id present");
            assert!(e["name"].as_str().is_some(), "name present");
            assert!(e["category"].as_str().is_some(), "category present");
            // Real IHDR dimensions parsed straight from the PNG header.
            assert!(e["width"].as_u64().unwrap_or(0) > 0, "real width: {e}");
            assert!(e["height"].as_u64().unwrap_or(0) > 0, "real height: {e}");
            // T-29-05: the model NEVER receives a raw path.
            assert!(e.get("path").is_none(), "no path field leaked: {e}");
        }
        // T-29-07: no absolute-path substring (drive prefix / the real dir) in
        // the whole serialized result.
        let bundled_str = bundled.to_string_lossy().to_string();
        assert!(
            !json.contains(&bundled_str) && !json.contains(":\\") && !json.contains(":/"),
            "get_overlay_library JSON must not disclose any filesystem path: {json}"
        );
    }

    /// T-29-06: the entry cap truncates enumeration — seeding more user assets
    /// than the cap yields exactly `max_entries` results, never more.
    #[test]
    fn entry_cap_truncates_the_scan() {
        let empty_bundled = fresh_temp("cap-empty-bundled");
        let user = fresh_temp("cap-user");
        for i in 0..5 {
            seed_real_png(&user, &format!("asset_{i}.png"));
        }
        let json = scan_overlay_library(&empty_bundled, &user, 3).expect("scan ok");
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("valid JSON array");
        assert_eq!(entries.len(), 3, "cap of 3 must truncate 5 user assets to 3: {json}");
    }

    /// T-29-06: an oversized MANIFEST is rejected BEFORE any per-entry file I/O
    /// (the array-length guard), rather than enumerated.
    #[test]
    fn oversized_manifest_is_rejected() {
        let bundled = fresh_temp("oversized-bundled");
        let big: Vec<serde_json::Value> = (0..8)
            .map(|i| serde_json::json!({"id":format!("a{i}"),"name":"x","category":"c","file":"x.png"}))
            .collect();
        std::fs::write(
            bundled.join("catalog.json"),
            serde_json::to_string(&big).unwrap(),
        )
        .expect("write catalog");
        let err = scan_overlay_library(&bundled, &fresh_temp("oversized-user"), 2)
            .expect_err("oversized manifest must be rejected");
        assert!(err.contains("too large"), "cap error message: {err}");
    }

    /// T-29-08: a catalog entry whose `file` is a traversal component is SKIPPED,
    /// never resolved outside the confined bundled root.
    #[test]
    fn traversal_file_component_is_skipped() {
        let bundled = fresh_temp("traversal-bundled");
        // A real, safe asset + a malicious traversal entry.
        seed_real_png(&bundled, "safe.png");
        let catalog = serde_json::json!([
            {"id":"safe","name":"Safe","category":"c","file":"safe.png"},
            {"id":"evil","name":"Evil","category":"c","file":"../../secret.png"},
        ]);
        std::fs::write(
            bundled.join("catalog.json"),
            serde_json::to_string(&catalog).unwrap(),
        )
        .expect("write catalog");
        let json = scan_overlay_library(&bundled, &fresh_temp("traversal-user"), MAX_OVERLAY_LIBRARY_ENTRIES)
            .expect("scan ok");
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("valid JSON array");
        assert_eq!(entries.len(), 1, "only the safe entry survives: {json}");
        assert_eq!(entries[0]["id"], serde_json::json!("safe"));
    }

    /// SC-2 end-to-end via the real [`AppCtx`] path: user-imported assets in
    /// `app_data_dir()/overlay-library` are listed with `source:"user"`, ids
    /// resolve server-side, and no raw path leaks.
    #[test]
    fn run_get_overlay_library_lists_user_assets_via_app() {
        let ctx = TestAppCtx::new();
        let user_dir = ctx
            .app_data_dir()
            .expect("app_data_dir")
            .join("overlay-library");
        let _ = std::fs::remove_dir_all(&user_dir);
        std::fs::create_dir_all(&user_dir).expect("create user overlay dir");
        seed_real_png(&user_dir, "my-logo.png");

        let json = run_get_overlay_library(&ctx).expect("get_overlay_library ok");
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("valid JSON array");

        let logo = entries
            .iter()
            .find(|e| e["id"] == serde_json::json!("my-logo"))
            .unwrap_or_else(|| panic!("user asset my-logo missing in {json}"));
        assert_eq!(logo["source"], serde_json::json!("user"), "user source");
        assert!(logo["width"].as_u64().unwrap_or(0) > 0, "real dims: {logo}");
        assert!(logo.get("path").is_none(), "no path field leaked: {logo}");
        assert!(
            !json.contains(":\\") && !json.contains(":/"),
            "no absolute path disclosed: {json}"
        );

        let _ = std::fs::remove_dir_all(&user_dir);
    }
}
