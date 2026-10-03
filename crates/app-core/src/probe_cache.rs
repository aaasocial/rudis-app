//! Phase 43 (LAT-08): a persistent, versioned probe-result cache so
//! re-importing an unchanged file never re-shells to `ffprobe`.
//!
//! Keyed by `(canonical path, mtime, size)` -- NOT a content hash (D-17:
//! hashing a multi-GB video costs more than the probe it replaces). Shared by
//! EVERY `engine::probe` call site on the import path: the UI batch path
//! (`import_media` / `import_media_folder` -> [`crate::import::import_one_path`])
//! AND the agent-tool single-file path ([`crate::import::run_import_media`]).
//! There is deliberately one module, not two caches that can drift.
//!
//! Relocated verbatim from `src-tauri/src/probe_cache.rs` by plan 45-07. The
//! ONLY change: `cache_path` / `load` / `save` took a
//! `&tauri::AppHandle<R>` and now take a `&impl `[`AppCtx`], resolving the same
//! `app_data_dir()` through the trait. `key_for` / `load_from` / `save_to` and
//! both public types were already host-agnostic and are byte-identical, as are
//! all seven tests.
//!
//! **The cache is an OPTIMIZATION, never a correctness dependency** (D-13's
//! discipline, applied identically here): a missing, oversized, unreadable,
//! corrupt, over-long or version-mismatched cache file yields an EMPTY map and
//! every file is probed fresh. Nothing in this module returns an error to its
//! caller, and nothing in it panics.
//!
//! Threat model (T-43-08-01 / T-43-08-02): this is a NEW on-disk artifact that
//! gets deserialized, and its contents are attacker-influenceable via file
//! paths and media metadata. It is therefore treated as untrusted input --
//! size-bounded and `is_file`-checked BEFORE it is read, entry-count-bounded
//! before it is trusted, and written temp-then-rename so a race cannot leave a
//! torn file behind.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use rudis_core::MediaKind;
use serde::{Deserialize, Serialize};

use crate::AppCtx;

/// The cache file's name inside `app_data_dir()`.
pub const PROBE_CACHE_FILE: &str = "probe_cache.json";

/// Bump when [`CachedProbeResult`]'s shape (or `engine::probe`'s meaning for
/// any field mirrored into it) changes -- any other version invalidates every
/// entry (D-18), so an older or newer build sharing an app data dir degrades to
/// correct-but-slow, never to wrong.
///
/// **1 -> 2 (Phase 58, plan 58-05):** [`CachedProbeResult`] gained `bit_rate`
/// and `vcodec`, the two inputs `proxy::heaviness::needs_proxy` needs that the
/// import path was not carrying. The bump is the WHOLE mechanism by which every
/// stale on-disk entry re-probes and gains them: [`load_from`]'s version gate
/// discards the old file wholesale, so the next import of a
/// previously-imported file spends one `ffprobe` and comes back with real
/// values.
///
/// `#[serde(default)]` on the two new fields would have been the cheaper edit
/// and would have been WRONG: it deserializes a v1 entry into `bit_rate: None`,
/// which the heaviness predicate reads as "no bitrate evidence" and which
/// therefore silently starves the bitrate rung for exactly the media a user
/// already has in their library. A cache miss costs one probe; a poisoned hit
/// costs a proxy that is never generated and never explained.
///
/// **2 -> 3 (Phase 60, plan 60-06):** [`CachedProbeResult`] gained `has_alpha`,
/// the pre-decode input the OCCL-01 occlusion predicate reads at gather time
/// (`engine::MediaInfo::has_alpha`, narrowed to a tri-state by
/// `crate::import::probed_alpha`). The bump is again the WHOLE mechanism by
/// which stale entries re-probe and gain it.
///
/// The same `#[serde(default)]` shortcut is refused for the same reason, and
/// here the damage is STICKIER than `bit_rate`'s. `has_alpha: None` means
/// "unknown", and unknown is carried into `rudis_core::MediaBinItem::
/// reports_alpha` as PERMANENTLY-NEVER-CULLABLE. A defaulted v2 hit would
/// therefore pin every already-imported source out of the occlusion payoff
/// forever — silently, unexplainably, and not even fixable by re-importing the
/// file, because the re-import would hit the same poisoned entry. Phases 58 and
/// 59 both lost time to a stale cache serving a schema it did not have; the
/// version term exists for exactly this.
const PROBE_CACHE_VERSION: u32 = 3;

/// T-43-08-01: refuse to hand an unbounded byte count to `serde_json`. At ~200
/// bytes per entry this still admits far more than [`MAX_PROBE_CACHE_ENTRIES`],
/// and a rejected cache costs exactly one cold probe per imported file.
const MAX_PROBE_CACHE_BYTES: u64 = 8 * 1024 * 1024;

/// Hard bound on how many entries the cache may hold. Without it the file grows
/// for the lifetime of the install (every distinct `(path, mtime, size)` ever
/// imported is a new key, so even re-importing the SAME file after an edit adds
/// one). Over the cap the entries are truncated in a deterministic order; a
/// dropped entry costs exactly one fresh probe.
const MAX_PROBE_CACHE_ENTRIES: usize = 4096;

/// Monotonic per-write nonce so the temp file is unique per call -- mirrors
/// `project_store::TMP_COUNTER` (WR-02): two concurrent writers must never
/// share a `.tmp` handle and interleave-corrupt it before the rename.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The in-memory cache a caller loads once, consults per file, and saves once.
pub type ProbeCache = HashMap<ProbeCacheKey, CachedProbeResult>;

/// D-17's key. `mtime_ns`, NOT `mtime_ms`: Windows file timestamps advance in
/// coarse (~15.6 ms) steps, so a millisecond-truncated stamp can compare EQUAL
/// across two genuinely different writes and serve a stale entry forever. Same
/// grain as `project_store::ProjectIndexEntry` -- one phase, one convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProbeCacheKey {
    pub path: String,
    pub mtime_ns: u64,
    pub size_bytes: u64,
}

/// Mirrors ONLY the `engine::MediaInfo` fields the import path actually
/// consumes -- when building a `MediaBinItem`, and (since Phase 58) when
/// deciding whether the source deserves a playback proxy. `media_kind` is
/// stored ALREADY MAPPED to the wire/domain [`MediaKind`] (which is
/// serde-derived), so no serde dependency is added to `crates/engine`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedProbeResult {
    pub media_kind: MediaKind,
    pub duration_us: i64,
    pub width: u32,
    pub height: u32,
    pub avg_frame_rate: f64,
    pub is_vfr: bool,
    pub rotation_degrees: u32,
    pub has_audio: bool,
    /// Container bitrate in bits per second, `None` when the container does not
    /// report one. Phase 58 (plan 58-05): NOT consumed by `MediaBinItem` -- it
    /// exists solely so `proxy::heaviness::needs_proxy` can be evaluated at
    /// import time from the probe already in hand, with no second `ffprobe`.
    /// Cached rather than recomputed for the same reason every other field here
    /// is: a cache HIT and a cache MISS must not be able to produce different
    /// import decisions for the same file.
    pub bit_rate: Option<u64>,
    /// The video codec name as `ffprobe` reports it (`"h264"`, `"hevc"`,
    /// `"prores"`, ...), `None` for audio-only media or an unparsable stream.
    /// The predicate's other new input: it separates inter-frame codecs, where
    /// a high bitrate really does signal expensive decode, from intra-frame
    /// ones where it does not.
    pub vcodec: Option<String>,
    /// Phase 60 (plan 60-06, OCCL-01): whether the SOURCE can carry real
    /// per-pixel transparency, as a TRI-STATE — `None` = the probe established
    /// nothing (audio-only, or a video stream with no readable pixel format),
    /// `Some(false)` = probed opaque, `Some(true)` = probed alpha-carrying.
    ///
    /// Narrowed from `engine::MediaInfo::has_alpha` (a plain `bool`, which
    /// collapses "no alpha" and "nothing to read" into the same `false`) by
    /// [`crate::import::probed_alpha`], and cached rather than re-derived for
    /// the same reason every other field here is: a cache HIT and a cache MISS
    /// must not be able to produce different import decisions for one file.
    /// This one is the difference between a layer being occluder-eligible and
    /// not, so a drift between the two paths would be a PIXEL difference.
    pub has_alpha: Option<bool>,
}

/// The on-disk shape. Entries are a `Vec` of pairs, not a JSON object: the key
/// is a struct, which has no JSON-object-key representation.
#[derive(Debug, Serialize, Deserialize)]
struct ProbeCacheFile {
    version: u32,
    entries: Vec<(ProbeCacheKey, CachedProbeResult)>,
}

/// The cache key for a REAL, already-canonicalized path.
///
/// `None` means "not cacheable" -- a stat failure, a non-file, or an unreadable
/// timestamp. A `None` is NEVER an import failure: the caller probes fresh and
/// simply does not cache the result. (Deliberately `Option`, not `Result`: the
/// batch import path documents that one bad file is skipped and never aborts
/// the batch, and a `?`-propagated stat error here would convert a per-file
/// filesystem hiccup into a whole-batch abort.)
pub fn key_for(path: &Path) -> Option<ProbeCacheKey> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_ns = u64::try_from(
        meta.modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    )
    .unwrap_or(u64::MAX);
    // `0` means "unknown" and is never a trustworthy staleness key.
    if mtime_ns == 0 {
        return None;
    }
    Some(ProbeCacheKey {
        path: path.to_string_lossy().into_owned(),
        mtime_ns,
        size_bytes: meta.len(),
    })
}

/// `app_data_dir()/probe_cache.json` -- `None` when the platform dir cannot be
/// resolved (the caller then runs entirely uncached).
pub fn cache_path(ctx: &impl AppCtx) -> Option<PathBuf> {
    ctx.app_data_dir().ok().map(|dir| dir.join(PROBE_CACHE_FILE))
}

/// Best-effort load from an explicit file path. A missing file, a non-file, an
/// oversized file, unreadable bytes, corrupt JSON, a version mismatch or an
/// over-long entry list ALL return an EMPTY map. Never errors, never panics.
pub fn load_from(path: &Path) -> ProbeCache {
    let Ok(meta) = std::fs::metadata(path) else {
        return ProbeCache::new();
    };
    // T-43-08-01: bound the read BEFORE it happens, and require a real file
    // (a directory named `probe_cache.json` must not be `read` into memory).
    if !meta.is_file() || meta.len() > MAX_PROBE_CACHE_BYTES {
        return ProbeCache::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return ProbeCache::new();
    };
    let Ok(file) = serde_json::from_slice::<ProbeCacheFile>(&bytes) else {
        return ProbeCache::new();
    };
    // D-18 version gate + the entry-count bound (a small-in-bytes file can
    // still describe an absurd number of entries).
    if file.version != PROBE_CACHE_VERSION || file.entries.len() > MAX_PROBE_CACHE_ENTRIES {
        return ProbeCache::new();
    }
    file.entries.into_iter().collect()
}

/// Best-effort save to an explicit file path, written temp-then-rename so two
/// concurrent importers can never leave a torn cache file. A write failure is
/// swallowed: losing the cache costs a slower next import, never correctness.
pub fn save_to(path: &Path, cache: &ProbeCache) {
    // The app data dir may not exist yet on a first run (unlike the poster
    // cache dir, nothing else has created it at this point).
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let mut entries: Vec<(ProbeCacheKey, CachedProbeResult)> =
        cache.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    // Deterministic order (a `HashMap` iterates arbitrarily) so the truncation
    // below is reproducible and two runs over the same cache write the same
    // bytes. A dropped entry costs exactly one fresh probe.
    entries.sort_by(|a, b| {
        (&a.0.path, a.0.mtime_ns, a.0.size_bytes).cmp(&(&b.0.path, b.0.mtime_ns, b.0.size_bytes))
    });
    entries.truncate(MAX_PROBE_CACHE_ENTRIES);

    let file = ProbeCacheFile {
        version: PROBE_CACHE_VERSION,
        entries,
    };
    let Ok(json) = serde_json::to_string(&file) else {
        return;
    };
    // Temp-then-rename, `project_store`'s established atomic-write convention:
    // two concurrent importers can then never leave a torn cache file.
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = PathBuf::from(format!(
        "{}.{}-{:x}.tmp",
        path.to_string_lossy(),
        std::process::id(),
        nonce
    ));
    if std::fs::write(&tmp, json.as_bytes()).is_err() || std::fs::rename(&tmp, path).is_err() {
        // Best-effort cleanup so a failed write never leaks a stray `.tmp`.
        let _ = std::fs::remove_file(&tmp);
    }
}

/// [`load_from`] at the app's own cache location.
pub fn load(ctx: &impl AppCtx) -> ProbeCache {
    match cache_path(ctx) {
        Some(path) => load_from(&path),
        None => ProbeCache::new(),
    }
}

/// [`save_to`] at the app's own cache location.
pub fn save(ctx: &impl AppCtx, cache: &ProbeCache) {
    if let Some(path) = cache_path(ctx) {
        save_to(&path, cache);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rudis-probe-cache-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample(width: u32) -> CachedProbeResult {
        CachedProbeResult {
            media_kind: MediaKind::Video,
            duration_us: 5_000_000,
            width,
            height: 720,
            avg_frame_rate: 29.97,
            is_vfr: false,
            rotation_degrees: 90,
            has_audio: true,
            bit_rate: Some(8_000_000),
            vcodec: Some("h264".to_string()),
            has_alpha: Some(false),
        }
    }

    fn key(path: &str, mtime_ns: u64, size_bytes: u64) -> ProbeCacheKey {
        ProbeCacheKey {
            path: path.to_string(),
            mtime_ns,
            size_bytes,
        }
    }

    /// D-17's key must be STABLE for an unchanged file and must CHANGE the
    /// moment the file's bytes do -- otherwise a stale entry is served forever.
    #[test]
    fn key_for_is_stable_for_an_unchanged_file_and_changes_when_the_file_does() {
        let dir = temp_dir("key-for");
        let path = dir.join("media.bin");
        std::fs::write(&path, b"first").expect("write");

        let first = key_for(&path).expect("a real file has a key");
        let again = key_for(&path).expect("a real file has a key");
        assert_eq!(first, again, "an untouched file keeps the same key");

        // Change BOTH size and mtime: on Windows the timestamp alone advances
        // in ~15.6ms steps, so a fast rewrite can keep the same mtime.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, b"second-and-longer").expect("rewrite");
        let after = key_for(&path).expect("a real file has a key");
        assert_ne!(first, after, "a rewritten file must produce a DIFFERENT key");

        assert!(
            key_for(&dir.join("nope.bin")).is_none(),
            "a nonexistent path is not cacheable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A save/load round trip must preserve every entry field-for-field --
    /// otherwise a "hit" would hand the importer different metadata than a
    /// fresh probe would.
    #[test]
    fn save_then_load_round_trips_every_field_of_every_entry() {
        let dir = temp_dir("round-trip");
        let file = dir.join(PROBE_CACHE_FILE);

        let k1 = key("C:/media/a.mp4", 111, 222);
        let k2 = key("C:/media/b.mov", 333, 444);
        let mut cache = ProbeCache::new();
        cache.insert(k1.clone(), sample(1280));
        cache.insert(
            k2.clone(),
            CachedProbeResult {
                media_kind: MediaKind::Audio,
                duration_us: 3_000_000,
                width: 0,
                height: 0,
                avg_frame_rate: 0.0,
                is_vfr: true,
                rotation_degrees: 270,
                has_audio: true,
                bit_rate: None,
                vcodec: None,
                has_alpha: None,
            },
        );

        save_to(&file, &cache);
        assert!(file.is_file(), "save must produce a real file");
        let loaded = load_from(&file);
        assert_eq!(loaded.len(), 2, "both entries survive the round trip");
        assert_eq!(loaded.get(&k1), cache.get(&k1), "entry 1 field-for-field");
        assert_eq!(loaded.get(&k2), cache.get(&k2), "entry 2 field-for-field");

        // No `.tmp` leak from the atomic write.
        let leaked: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("tmp"))
            .collect();
        assert!(leaked.is_empty(), "atomic write leaked {leaked:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// First run on a fresh machine: no cache file exists yet.
    #[test]
    fn load_on_a_missing_cache_file_returns_an_empty_map() {
        let dir = temp_dir("missing");
        let loaded = load_from(&dir.join(PROBE_CACHE_FILE));
        assert!(loaded.is_empty(), "a missing cache file yields an empty map");
        // A DIRECTORY sitting where the cache file should be must not be read
        // into memory either (T-43-08-01's `is_file` half).
        std::fs::create_dir_all(dir.join(PROBE_CACHE_FILE)).expect("mkdir");
        assert!(
            load_from(&dir.join(PROBE_CACHE_FILE)).is_empty(),
            "a non-file at the cache path yields an empty map"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-43-08-01: a corrupt/hand-edited cache must fail CLOSED to a cold probe.
    #[test]
    fn load_on_a_corrupt_cache_file_returns_an_empty_map() {
        let dir = temp_dir("corrupt");
        let file = dir.join(PROBE_CACHE_FILE);
        // The two structurally-hostile cases carry the CURRENT version stamp so
        // they are refused for their SHAPE rather than incidentally by the
        // version gate. Built from the constant rather than hardcoded, because
        // a hardcoded stamp turns silently vacuous at the next version bump
        // (it did: these read `"version":2` until 60-06 bumped to 3).
        let cur = PROBE_CACHE_VERSION;
        let hostiles: [Vec<u8>; 6] = [
            b"\x00\x01\x02not json at all\xff\xfe".to_vec(),
            b"[]".to_vec(),
            b"{}".to_vec(),
            format!("{{\"version\":{cur},\"entries\":\"not-a-list\"}}").into_bytes(),
            format!("{{\"version\":{cur},\"entries\":[[{{\"path\":\"a\"}},{{}}]]}}").into_bytes(),
            Vec::new(),
        ];
        for hostile in &hostiles {
            std::fs::write(&file, hostile).expect("write hostile cache");
            assert!(
                load_from(&file).is_empty(),
                "hostile cache bytes must yield an empty map: {hostile:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D-18: a probe-schema version change invalidates EVERY entry.
    #[test]
    fn load_on_a_wrong_version_cache_file_returns_an_empty_map() {
        let dir = temp_dir("version");
        let file = dir.join(PROBE_CACHE_FILE);

        let k = key("C:/media/a.mp4", 111, 222);
        let mut cache = ProbeCache::new();
        cache.insert(k.clone(), sample(1280));
        save_to(&file, &cache);
        assert_eq!(load_from(&file).len(), 1, "the freshly written cache loads");

        // Rewrite the SAME entries under a different version stamp.
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("valid json");
        value["version"] = serde_json::json!(PROBE_CACHE_VERSION + 1);
        std::fs::write(&file, serde_json::to_vec(&value).expect("reserialize")).expect("write");
        assert!(
            load_from(&file).is_empty(),
            "a version mismatch invalidates every entry"
        );

        value["version"] = serde_json::json!(PROBE_CACHE_VERSION - 1);
        std::fs::write(&file, serde_json::to_vec(&value).expect("reserialize")).expect("write");
        assert!(
            load_from(&file).is_empty(),
            "an OLDER version also invalidates every entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 58 (plan 58-05): a real v1 file -- the exact eight-field entry
    /// shape shipped before this plan -- must be DISCARDED, not partially
    /// deserialized, so every previously-imported source re-probes once and
    /// gains `bit_rate`/`vcodec`.
    ///
    /// This is the gate that would fail if someone "helpfully" added
    /// `#[serde(default)]` to the two new fields: with defaults the entry below
    /// loads, `bit_rate` reads `None` forever, and the heaviness predicate's
    /// bitrate rung silently never fires for media already in the library.
    #[test]
    fn a_v1_cache_file_is_discarded_rather_than_partially_deserialized() {
        let dir = temp_dir("v1-schema");
        let file = dir.join(PROBE_CACHE_FILE);
        let v1 = serde_json::json!({
            "version": 1,
            "entries": [[
                { "path": "C:/media/a.mp4", "mtime_ns": 111u64, "size_bytes": 222u64 },
                { "media_kind": "video", "duration_us": 5_000_000, "width": 3840,
                  "height": 2160, "avg_frame_rate": 30.0, "is_vfr": false,
                  "rotation_degrees": 0, "has_audio": true }
            ]]
        });
        std::fs::write(&file, serde_json::to_vec(&v1).expect("serialize")).expect("write v1");
        assert!(
            load_from(&file).is_empty(),
            "a v1 entry must be discarded wholesale, so the file re-probes"
        );

        // Non-vacuity: the SAME entry under the current stamp, with the two new
        // fields present, loads and round-trips them.
        let mut cache = ProbeCache::new();
        let k = key("C:/media/a.mp4", 111, 222);
        cache.insert(k.clone(), sample(3840));
        save_to(&file, &cache);
        let loaded = load_from(&file);
        assert_eq!(
            loaded.get(&k).and_then(|v| v.bit_rate),
            Some(8_000_000),
            "bit_rate survives the round trip under the current version"
        );
        assert_eq!(
            loaded.get(&k).and_then(|v| v.vcodec.clone()),
            Some("h264".to_string()),
            "vcodec survives the round trip under the current version"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 60 (plan 60-06): the SAME discipline, one bump later. A real v2
    /// file -- the exact ten-field entry shape shipped between plans 58-05 and
    /// 60-06 -- must be DISCARDED, not partially deserialized, so every
    /// previously-imported source re-probes once and gains `has_alpha`.
    ///
    /// The stakes are higher here than they were for `bit_rate`, and in a
    /// sticky direction. `has_alpha` is an `Option<bool>` whose `None` means
    /// "unknown", and unknown is the state `MediaBinItem::reports_alpha` carries
    /// into the occlusion predicate as NEVER-CULLABLE. Under
    /// `#[serde(default)]` a v2 hit would deserialize to `None` and PIN that
    /// media as permanently un-cullable -- not wrong pixels, but a silent,
    /// unexplained, unrecoverable loss of the whole OCCL payoff for exactly the
    /// library a user already has. A cache miss costs one probe.
    #[test]
    fn a_v2_cache_file_is_discarded_rather_than_partially_deserialized() {
        let dir = temp_dir("v2-schema");
        let file = dir.join(PROBE_CACHE_FILE);
        let v2 = serde_json::json!({
            "version": 2,
            "entries": [[
                { "path": "C:/media/a.mp4", "mtime_ns": 111u64, "size_bytes": 222u64 },
                { "media_kind": "video", "duration_us": 5_000_000, "width": 3840,
                  "height": 2160, "avg_frame_rate": 30.0, "is_vfr": false,
                  "rotation_degrees": 0, "has_audio": true,
                  "bit_rate": 8_000_000, "vcodec": "h264" }
            ]]
        });
        std::fs::write(&file, serde_json::to_vec(&v2).expect("serialize")).expect("write v2");
        assert!(
            load_from(&file).is_empty(),
            "a v2 entry must be discarded wholesale, so the file re-probes"
        );

        // Non-vacuity: the SAME entry under the current stamp, with the new
        // field present, loads and round-trips the whole tri-state.
        let k = key("C:/media/a.mp4", 111, 222);
        for has_alpha in [None, Some(false), Some(true)] {
            let mut cache = ProbeCache::new();
            cache.insert(
                k.clone(),
                CachedProbeResult {
                    has_alpha,
                    ..sample(3840)
                },
            );
            save_to(&file, &cache);
            assert_eq!(
                load_from(&file).get(&k).and_then(|v| v.has_alpha),
                has_alpha,
                "has_alpha={has_alpha:?} survives the round trip under the current version"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-43-08-01: the read is bounded BEFORE it happens, and the entry list is
    /// bounded before it is trusted -- neither an oversized file nor an
    /// entry-count blow-up may allocate unboundedly on the import path.
    #[test]
    fn load_rejects_an_oversized_or_over_long_cache() {
        let dir = temp_dir("bounds");
        let file = dir.join(PROBE_CACHE_FILE);

        // Oversized: valid JSON padded past the byte cap must still be refused.
        let mut cache = ProbeCache::new();
        cache.insert(key("C:/media/a.mp4", 1, 2), sample(1280));
        save_to(&file, &cache);
        let mut bytes = std::fs::read(&file).expect("read");
        let pad = vec![b' '; (MAX_PROBE_CACHE_BYTES as usize) + 1];
        bytes.extend_from_slice(&pad);
        std::fs::write(&file, &bytes).expect("write oversized");
        assert!(
            load_from(&file).is_empty(),
            "an oversized cache file is refused before it is read"
        );

        // Over-long entry list (still small in bytes) is refused too.
        let entries: Vec<serde_json::Value> = (0..=MAX_PROBE_CACHE_ENTRIES)
            .map(|i| {
                serde_json::json!([
                    { "path": format!("p{i}"), "mtime_ns": 1, "size_bytes": 1 },
                    { "media_kind": "video", "duration_us": 0, "width": 0, "height": 0,
                      "avg_frame_rate": 0.0, "is_vfr": false, "rotation_degrees": 0,
                      "has_audio": false, "bit_rate": null, "vcodec": null,
                      "has_alpha": null }
                ])
            })
            .collect();
        std::fs::write(
            &file,
            serde_json::to_vec(&serde_json::json!({
                "version": PROBE_CACHE_VERSION, "entries": entries
            }))
            .expect("serialize"),
        )
        .expect("write over-long");
        assert!(
            load_from(&file).is_empty(),
            "an over-long entry list is refused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Saving more than the cap must WRITE at most the cap, so the artifact
    /// cannot grow without bound across an install's lifetime.
    #[test]
    fn save_bounds_the_number_of_persisted_entries() {
        let dir = temp_dir("save-cap");
        let file = dir.join(PROBE_CACHE_FILE);

        let mut cache = ProbeCache::new();
        for i in 0..(MAX_PROBE_CACHE_ENTRIES + 25) {
            cache.insert(key(&format!("C:/media/{i:06}.mp4"), 1, 2), sample(64));
        }
        save_to(&file, &cache);

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("valid json");
        assert_eq!(
            value["entries"].as_array().expect("entries is a list").len(),
            MAX_PROBE_CACHE_ENTRIES,
            "save must truncate to the entry cap"
        );
        assert_eq!(
            load_from(&file).len(),
            MAX_PROBE_CACHE_ENTRIES,
            "and the capped file still loads"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
