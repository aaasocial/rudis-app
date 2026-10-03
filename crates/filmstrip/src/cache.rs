//! The per-media-file strip cache (Phase 53.2, D-11 + D-14).
//!
//! # The five properties, copied rather than re-invented
//!
//! `crates/app-core/src/probe_cache.rs` settled how this codebase treats a
//! persisted cache that a later run must parse, and `crates/waveform/src/cache.rs`
//! re-applied it one phase ago for peak envelopes. This module copies that
//! posture property for property, and each one is enforced by a named test in
//! `crates/filmstrip/tests/cache.rs`:
//!
//! 1. **Versioned** -- a four-byte magic plus [`CACHE_VERSION`]. Any other
//!    stamp, older or newer, invalidates the file outright, so two builds
//!    sharing one cache directory degrade to correct-but-slow, never to wrong.
//!    (`a_wrong_magic_file_is_a_miss`, `a_wrong_version_file_is_a_miss`)
//! 2. **Bounded** -- [`MAX_STRIP_FILE_BYTES`] is checked against the file's
//!    METADATA before a single byte is read, [`write`] refuses a payload past
//!    that same cap, and [`prune`] bounds how many files may accumulate.
//!    (`an_oversized_file_is_a_miss_before_any_read`,
//!    `write_refuses_a_payload_it_could_never_read_back`,
//!    `prune_keeps_the_directory_bounded`)
//! 3. **Temp-then-rename** -- every write lands on a uniquely-nonced temp
//!    sibling, is `sync_all`ed, and only then renamed over the final name.
//!    (`write_is_atomic_temp_then_rename`)
//! 4. **Best-effort** -- [`write`] returns `bool` and [`prune`] returns nothing.
//!    A cache that cannot be written costs one re-extraction, never an import
//!    failure. (`write_failure_is_swallowed_never_propagated`)
//! 5. **Never errors, never panics** -- [`read`] returns `Option`, and every
//!    hostile input resolves to `None`. This file contains no panicking
//!    construct outside its comments, and that is enforced by
//!    `no_unwrap_no_panic_in_cache`, a source scan that blanks comments before
//!    looking -- a grep living in an archived plan document stops being run.
//!
//! # What D-14 adds that neither precedent has
//!
//! Phase 52's peaks were written ONCE, at the end of extraction, so the file was
//! whole or absent. A filmstrip fills PROGRESSIVELY -- each decode chunk
//! publishes its slice so frames march in left to right on long media -- which
//! fights the whole-or-absent property rather than following it. The gap is
//! closed here, deliberately, in three places:
//!
//! 1. the header carries `completed_tiles` beside `total_tiles`, so a reader can
//!    tell a strip that is STILL FILLING from one that is DONE;
//! 2. temp-then-rename applies **per published revision** rather than once at
//!    the end -- [`write`] already computes a fresh nonce per call and renames
//!    over the same final path, so this is a REUSE of an existing capability,
//!    not a new mechanism;
//! 3. retrieval therefore has three states -- absent (`None`), partial
//!    (`is_complete() == false`), complete -- rather than the flat `None`/`Some`
//!    the peaks pipeline chose.
//!
//! The all-at-once fallback that `53.2-CONTEXT.md` D-14 permits was NOT taken
//! and no fallback argument was needed: the per-revision publish is the same
//! `write` call the one-shot design would have made, invoked more than once.
//!
//! # Threat model
//!
//! * **T-53.2-06 (a corrupted or hostile header)** -- properties 2 and 5. Every
//!   field is read through the bounds-checked [`take`] helper, every geometry
//!   field is range-clamped, and the declared payload length is cross-checked
//!   against BOTH the geometry and the bytes actually present. The size bound is
//!   checked on the METADATA, so a hostile file cannot cause an allocation
//!   merely by existing.
//! * **T-53.2-07 (a torn read of a progressive revision)** -- property 3, now
//!   invoked once per published revision.
//! * **stale frames for a changed file** -- the key; see [`FilmstripCacheKey`].
//! * **unbounded directory growth** -- [`prune`], run after every successful
//!   write so the bound belongs to the cache rather than to whichever caller
//!   remembers to ask for it.
//!
//! # Storage shape, and why the payload is RAW RGBA rather than PNG
//!
//! ONE FILE PER MEDIA ITEM, following the POSTER cache rather than the probe
//! cache's single combined JSON: strips are sized like posters, and a combined
//! file would be rewritten in full on every published revision.
//!
//! The payload is **uncompressed RGBA**, not a PNG sheet, for three reasons:
//!
//! * (a) the read path stays a pure bounded file read with ZERO decode, which is
//!   what lets a retrieval export promise "returns bytes or `null`, never
//!   errors, never triggers computation";
//! * (b) D-14's partial reads need row-level validity tracked in the HEADER --
//!   PNG is whole-or-nothing, so a partially-written sheet would reintroduce the
//!   torn-file problem *inside* the payload, exactly where the rename cannot
//!   help;
//! * (c) the size is already bounded by the extractor's tile cap, so raw's disk
//!   cost has a hard ceiling (256 tiles at 96x54 is 5.06 MiB) rather than an
//!   open-ended one.
//!
//! PNG (via the `image` dependency this crate already carries) remains available
//! for debug dumps; it is deliberately not on the cache path.
//!
//! On-disk layout, little-endian throughout:
//!
//! ```text
//! magic(4) | version(u32) | key_len(u32) | key_json(key_len) |
//! tile_w(u32) | tile_h(u32) | tiles_per_row(u32) | total_tiles(u32) |
//! completed_tiles(u32) | interval_us(i64) | src_w(u32) | src_h(u32) |
//! payload_len(u64) | payload
//! ```
//!
//! `payload` is `ceil(completed_tiles / tiles_per_row)` whole SHEET ROWS, each
//! `tiles_per_row * tile_w * 4` bytes wide and `tile_h` rows tall. Whole rows,
//! never a partial row: a consumer slicing tile `n` out of the sheet must never
//! have to ask whether the bytes under it are real.
//!
//! The FULL key is stored inside the file, not merely hashed into its name: the
//! filename is a 64-bit hash, and a collision must degrade to a MISS, not to a
//! confidently-served WRONG strip.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The directory, under the host's `app_cache_dir()`, that holds one file per
/// media item. Named here so the import-side job and the retrieval path use the
/// same location rather than each inventing one.
pub const FILMSTRIP_CACHE_DIR_NAME: &str = "filmstrips";

/// Bump when the on-disk layout OR the meaning of any field in it changes --
/// including a change to the tile cell size or the sheet's row width, since a
/// consumer slices tiles out by that geometry.
pub const CACHE_VERSION: u32 = 1;

/// First four bytes of every cache file. A file that does not start with this is
/// not ours, whatever its name says. (`RFS` = Rudis FilmStrip.)
pub const CACHE_MAGIC: &[u8; 4] = b"RFS1";

/// Extension of a final (renamed-into-place) cache file.
pub const CACHE_FILE_EXT: &str = "rfs";

/// Refuse to hand an unbounded byte count to the parser.
///
/// The same 8 MiB bound the peak cache uses, and it has real slack here rather
/// than being copied for symmetry: the extractor's ceiling is 256 tiles at
/// 96x54 RGBA = 5.06 MiB, so a legitimate strip cannot approach this. Checked
/// against file METADATA before any read, in that order.
pub const MAX_STRIP_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Hard bound on how many entries the directory may hold.
///
/// A quarter of the peak cache's 1024, because a strip is roughly a thousand
/// times heavier than a peak array: 256 entries at the 5.06 MiB worst case is a
/// 1.3 GB disk ceiling, and the typical entry is far smaller. Without a bound
/// the cache grows for the life of the install -- every distinct
/// `(path, mtime_ns, size_bytes)` ever imported is a new key. A dropped entry
/// costs exactly one re-extraction.
pub const MAX_CACHE_ENTRIES: usize = 256;

/// Header sanity clamps. A file violating ANY of them is a MISS, never a crash
/// and never a partially-trusted read (T-53.2-06).
///
/// These are deliberately wider than the extractor's own constants: the cache
/// must survive a strip written by a DIFFERENT build of this crate with a
/// different cell size, and reject only what could not describe a real sheet.
pub const MIN_TILE_DIM: u32 = 1;
/// See [`MIN_TILE_DIM`].
pub const MAX_TILE_DIM: u32 = 512;
/// See [`MIN_TILE_DIM`].
pub const MIN_TILES_PER_ROW: u32 = 1;
/// See [`MIN_TILE_DIM`].
pub const MAX_TILES_PER_ROW: u32 = 64;
/// See [`MIN_TILE_DIM`].
pub const MIN_TOTAL_TILES: u32 = 1;
/// See [`MIN_TILE_DIM`].
pub const MAX_TOTAL_TILES: u32 = 4096;

/// Monotonic per-write nonce so a temp file is unique per call -- mirrors
/// `probe_cache::TMP_COUNTER` and its stated reason: two concurrent writers must
/// never share a `.tmp` handle and interleave-corrupt it before the rename.
/// Under D-14 this fires once per PUBLISHED REVISION, not once per file.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// D-07's key: `(canonical path, mtime_ns, size_bytes)`.
///
/// `mtime_ns`, NOT `mtime_ms`. Windows file timestamps advance in coarse
/// (~15.6 ms) steps, so a millisecond-truncated stamp can compare EQUAL across
/// two genuinely different writes and serve a stale strip forever. Same grain,
/// and the same reasoning, as `probe_cache::ProbeCacheKey` and
/// `waveform::cache::WaveformCacheKey` -- one codebase, one convention.
///
/// Keying on the MEDIA FILE rather than on a clip is what makes a trim, a split,
/// a duplicate and a move cost zero new decode: they all select a window into
/// the one strip the file already has.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FilmstripCacheKey {
    pub path: String,
    pub mtime_ns: u64,
    pub size_bytes: u64,
}

/// Everything about a strip except its pixels -- what [`write`] is handed and
/// what [`read`] hands back alongside the payload.
///
/// The geometry travels WITH the strip rather than being assumed: a consumer
/// slicing tiles out must use the grid the file was actually written at, not
/// whatever this build's constants happen to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripHeader {
    pub tile_w: u32,
    pub tile_h: u32,
    pub tiles_per_row: u32,
    /// How many tiles the finished strip will hold.
    pub total_tiles: u32,
    /// D-14: how many of them are VALID in this revision. Equal to
    /// `total_tiles` once extraction finishes; smaller while it is still
    /// running.
    pub completed_tiles: u32,
    /// Source microseconds between adjacent tiles (D-06's one fixed density).
    pub interval_us: i64,
    pub src_w: u32,
    pub src_h: u32,
}

/// What a cache HIT hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedStrip {
    pub tile_w: u32,
    pub tile_h: u32,
    pub tiles_per_row: u32,
    pub total_tiles: u32,
    pub completed_tiles: u32,
    pub interval_us: i64,
    pub src_w: u32,
    pub src_h: u32,
    /// `sheet_rows() * tile_h` rows of `sheet_w()` RGBA pixels.
    pub rgba: Vec<u8>,
}

impl CachedStrip {
    /// D-14's third state. `false` means "this strip is real and drawable, and
    /// more tiles are still coming" -- NOT "this strip failed".
    pub fn is_complete(&self) -> bool {
        self.completed_tiles >= self.total_tiles
    }

    /// Sheet width in pixels.
    pub fn sheet_w(&self) -> u32 {
        self.tiles_per_row.saturating_mul(self.tile_w)
    }

    /// How many whole tile-rows of the sheet the payload covers.
    pub fn sheet_rows(&self) -> u32 {
        if self.tiles_per_row == 0 {
            return 0;
        }
        self.completed_tiles.div_ceil(self.tiles_per_row)
    }

    fn header(&self) -> StripHeader {
        StripHeader {
            tile_w: self.tile_w,
            tile_h: self.tile_h,
            tiles_per_row: self.tiles_per_row,
            total_tiles: self.total_tiles,
            completed_tiles: self.completed_tiles,
            interval_us: self.interval_us,
            src_w: self.src_w,
            src_h: self.src_h,
        }
    }
}

impl StripHeader {
    /// Every clamp, in one place, so [`read`] and [`encode`] cannot drift: a
    /// header this rejects is never written, and never trusted if it appears on
    /// disk anyway.
    fn is_sane(&self) -> bool {
        (MIN_TILE_DIM..=MAX_TILE_DIM).contains(&self.tile_w)
            && (MIN_TILE_DIM..=MAX_TILE_DIM).contains(&self.tile_h)
            && (MIN_TILES_PER_ROW..=MAX_TILES_PER_ROW).contains(&self.tiles_per_row)
            && (MIN_TOTAL_TILES..=MAX_TOTAL_TILES).contains(&self.total_tiles)
            && self.completed_tiles <= self.total_tiles
    }

    /// How many whole tile-rows `completed_tiles` covers. Only meaningful once
    /// [`Self::is_sane`] has passed, which is why every caller checks first.
    fn sheet_rows(&self) -> u32 {
        if self.tiles_per_row == 0 {
            return 0;
        }
        self.completed_tiles.div_ceil(self.tiles_per_row)
    }

    /// The ONLY payload length this geometry can describe, or `None` on
    /// overflow. Saturating arithmetic would silently agree with a hostile
    /// header, so this is `checked_*` throughout.
    fn expected_payload_len(&self) -> Option<u64> {
        let row_bytes = (self.tiles_per_row as u64)
            .checked_mul(self.tile_w as u64)?
            .checked_mul(4)?;
        (self.sheet_rows() as u64)
            .checked_mul(self.tile_h as u64)?
            .checked_mul(row_bytes)
    }
}

/// The cache key for a real file on disk.
///
/// `None` means "not cacheable" -- a stat failure, a non-file, an unreadable or
/// absurd timestamp. `None` is NEVER a failure: the caller extracts the strip
/// fresh and simply does not cache it. Deliberately `Option`, not `Result`,
/// exactly as `probe_cache::key_for` is: the import path skips one bad file and
/// never aborts the batch.
pub fn key_for(path: &Path) -> Option<FilmstripCacheKey> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let meta = std::fs::metadata(&canonical).ok()?;
    if !meta.is_file() {
        return None;
    }
    let nanos = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let mtime_ns = u64::try_from(nanos.as_nanos()).ok()?;
    // `0` means "unknown" and is never a trustworthy staleness key.
    if mtime_ns == 0 {
        return None;
    }
    Some(FilmstripCacheKey {
        path: canonical.to_string_lossy().into_owned(),
        mtime_ns,
        size_bytes: meta.len(),
    })
}

/// FNV-1a, 64-bit. Hand-rolled ON PURPOSE, for the reason
/// `crates/waveform/src/cache.rs` states at length: the default hasher behind
/// `std`'s `HashMap` is explicitly documented as NOT stable across Rust
/// releases, so deriving a FILENAME from it would silently orphan every cached
/// file on a toolchain bump -- an invisible cache wipe that looks like nothing
/// at all. FNV-1a is fixed arithmetic, so the same key yields the same name on
/// every build, forever.
///
/// (Not a security hash, and it does not need to be: a collision degrades to a
/// MISS, because the full key is stored inside the file and compared.)
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The filesystem-safe, deterministic file name for a key.
///
/// The size is appended in hex as a cheap second discriminator, so two keys must
/// collide in BOTH the hash and the file size to share a name.
pub fn file_name_for(key: &FilmstripCacheKey) -> String {
    let material = format!("{}|{}|{}", key.path, key.mtime_ns, key.size_bytes);
    let hash = fnv1a64(material.as_bytes());
    format!(
        "{hash:016x}-{size:016x}.{CACHE_FILE_EXT}",
        size = key.size_bytes
    )
}

/// Advance `cursor` by `n` bytes and hand back that slice, or `None` if the
/// buffer is shorter than the file claims.
///
/// Every field of the layout is read through this. It is the reason a truncated
/// or lying file is a MISS rather than an out-of-bounds index.
fn take<'a>(bytes: &'a [u8], cursor: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(n)?;
    let out = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(out)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let raw: [u8; 4] = take(bytes, cursor, 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn read_i64(bytes: &[u8], cursor: &mut usize) -> Option<i64> {
    let raw: [u8; 8] = take(bytes, cursor, 8)?.try_into().ok()?;
    Some(i64::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let raw: [u8; 8] = take(bytes, cursor, 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

/// Parse cache bytes, requiring them to describe exactly `key`.
///
/// Every failure -- and there is no other kind of outcome here -- is `None`.
fn parse(bytes: &[u8], key: &FilmstripCacheKey) -> Option<CachedStrip> {
    let mut cursor = 0usize;

    if take(bytes, &mut cursor, 4)? != CACHE_MAGIC.as_slice() {
        return None;
    }
    if read_u32(bytes, &mut cursor)? != CACHE_VERSION {
        return None;
    }

    let key_len = read_u32(bytes, &mut cursor)? as usize;
    let key_json = take(bytes, &mut cursor, key_len)?;
    let stored: FilmstripCacheKey = serde_json::from_slice(key_json).ok()?;
    // The name is a hash; the KEY is the truth. A collision misses here.
    if &stored != key {
        return None;
    }

    let header = StripHeader {
        tile_w: read_u32(bytes, &mut cursor)?,
        tile_h: read_u32(bytes, &mut cursor)?,
        tiles_per_row: read_u32(bytes, &mut cursor)?,
        total_tiles: read_u32(bytes, &mut cursor)?,
        completed_tiles: read_u32(bytes, &mut cursor)?,
        interval_us: read_i64(bytes, &mut cursor)?,
        src_w: read_u32(bytes, &mut cursor)?,
        src_h: read_u32(bytes, &mut cursor)?,
    };
    // The clamps run BEFORE any geometry arithmetic, so a hostile
    // `tiles_per_row = 0` cannot reach a division and a hostile
    // `completed_tiles = u32::MAX` cannot reach an allocation.
    if !header.is_sane() {
        return None;
    }

    let declared = read_u64(bytes, &mut cursor)?;
    // Two independent cross-checks, and a file must pass BOTH: the declared
    // length must be the ONLY one this grid can have, and it must be exactly the
    // bytes actually present. Checking only the second would serve a payload
    // sliced against the wrong rows; checking only the first would serve a
    // truncated one.
    if declared != header.expected_payload_len()? {
        return None;
    }
    let payload = take(bytes, &mut cursor, usize::try_from(declared).ok()?)?;
    // Trailing bytes mean the file is not what it declares itself to be.
    if cursor != bytes.len() {
        return None;
    }

    Some(CachedStrip {
        tile_w: header.tile_w,
        tile_h: header.tile_h,
        tiles_per_row: header.tiles_per_row,
        total_tiles: header.total_tiles,
        completed_tiles: header.completed_tiles,
        interval_us: header.interval_us,
        src_w: header.src_w,
        src_h: header.src_h,
        rgba: payload.to_vec(),
    })
}

/// Read the cached strip for `key` from `dir`, or `None`.
///
/// `None` covers absent, stale, corrupt, oversized and never-written alike --
/// the caller cannot distinguish them and must not try. A `Some` whose
/// [`CachedStrip::is_complete`] is `false` is D-14's third state: a real,
/// drawable, still-growing strip.
///
/// The order of checks matters and is deliberate: `is_file` and the METADATA
/// length are both settled BEFORE any allocation, so a hostile file cannot cost
/// memory merely by existing (T-53.2-06).
pub fn read(dir: &Path, key: &FilmstripCacheKey) -> Option<CachedStrip> {
    let path = dir.join(file_name_for(key));
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_STRIP_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    parse(&bytes, key)
}

/// Serialize one revision's bytes, or `None` if it could never be read back.
fn encode(key: &FilmstripCacheKey, header: &StripHeader, rgba_rows: &[u8]) -> Option<Vec<u8>> {
    // Refuse at write time exactly what `read` refuses at read time. A file that
    // would MISS forever is disk burned for nothing.
    if !header.is_sane() {
        return None;
    }
    let expected = header.expected_payload_len()?;
    if expected != rgba_rows.len() as u64 {
        return None;
    }

    let key_json = serde_json::to_vec(key).ok()?;
    let key_len = u32::try_from(key_json.len()).ok()?;

    // 4 magic + 4 version + 4 key_len + key_json
    //   + 5*4 grid + 8 interval_us + 4 src_w + 4 src_h + 8 payload_len + payload
    let total = 56usize
        .checked_add(key_json.len())?
        .checked_add(rgba_rows.len())?;
    if total as u64 > MAX_STRIP_FILE_BYTES {
        return None;
    }

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    out.extend_from_slice(&key_len.to_le_bytes());
    out.extend_from_slice(&key_json);
    out.extend_from_slice(&header.tile_w.to_le_bytes());
    out.extend_from_slice(&header.tile_h.to_le_bytes());
    out.extend_from_slice(&header.tiles_per_row.to_le_bytes());
    out.extend_from_slice(&header.total_tiles.to_le_bytes());
    out.extend_from_slice(&header.completed_tiles.to_le_bytes());
    out.extend_from_slice(&header.interval_us.to_le_bytes());
    out.extend_from_slice(&header.src_w.to_le_bytes());
    out.extend_from_slice(&header.src_h.to_le_bytes());
    out.extend_from_slice(&expected.to_le_bytes());
    out.extend_from_slice(rgba_rows);
    Some(out)
}

/// Publish one revision of the strip for `key` under `dir`. Returns whether it
/// landed.
///
/// D-14 calls this once per completed batch of tiles, always against the SAME
/// final path, with a monotonically larger `header.completed_tiles`. Each call
/// takes its own nonce, writes a fresh temp sibling, `sync_all`s it and renames
/// over whatever is there -- so a concurrent reader observes the previous whole
/// revision or the new whole revision, never a mixture (T-53.2-07).
///
/// BEST-EFFORT: any failure is reported on stderr and returned as `false`.
/// Nothing here is an `Err`, and nothing here panics -- losing the cache costs
/// one re-extraction, never an import.
pub fn write(
    dir: &Path,
    key: &FilmstripCacheKey,
    header: &StripHeader,
    rgba_rows: &[u8],
) -> bool {
    let Some(bytes) = encode(key, header, rgba_rows) else {
        eprintln!(
            "filmstrip cache: refusing to persist a {}-byte payload for {} \
             (grid {}x{} @ {}/row, {}/{} tiles -- past the {MAX_STRIP_FILE_BYTES}-byte \
             cap, or inconsistent with the grid)",
            rgba_rows.len(),
            key.path,
            header.tile_w,
            header.tile_h,
            header.tiles_per_row,
            header.completed_tiles,
            header.total_tiles
        );
        return false;
    };

    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("filmstrip cache: cannot create {}: {e}", dir.display());
        return false;
    }

    let name = file_name_for(key);
    let final_path = dir.join(&name);
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!("{name}.tmp.{}-{nonce:x}", std::process::id()));

    // Temp-then-rename: a reader either sees the previous complete revision or
    // the new complete revision, never a half-written one. `sync_all` before the
    // rename so the rename cannot beat the data to disk.
    let landed = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, &final_path)
    })();

    match landed {
        Ok(()) => {
            // The bound belongs to the cache, not to whichever caller remembers
            // to ask.
            prune(dir, MAX_CACHE_ENTRIES);
            true
        }
        Err(e) => {
            eprintln!(
                "filmstrip cache: write failed for {}: {e}",
                final_path.display()
            );
            // Never leak a stray `.tmp` behind a failed write.
            let _ = std::fs::remove_file(&tmp_path);
            false
        }
    }
}

/// Newest-wins prune down to `max_entries` cache files.
///
/// BEST-EFFORT: every filesystem error is swallowed. Failing to prune costs
/// disk, never correctness, so it must never become an import failure. Only
/// files with this cache's own extension are ever considered, let alone removed.
pub fn prune(dir: &Path, max_entries: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(CACHE_FILE_EXT) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = match meta.modified() {
            Ok(t) => t,
            // An unreadable timestamp sorts as "oldest", so such a file is
            // dropped before any file whose age is actually known.
            Err(_) => UNIX_EPOCH,
        };
        found.push((mtime, path));
    }

    if found.len() <= max_entries {
        return;
    }

    // Newest first; ties broken by path so two runs over the same directory make
    // the same decision.
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (_, path) in found.iter().skip(max_entries) {
        let _ = std::fs::remove_file(path);
    }
}

/// Re-encode a strip exactly as [`write`] would, without touching the disk.
///
/// Exposed for the retrieval path a later plan builds: handing the SAME bytes
/// over an ABI that a re-read would have produced keeps one encoder, not two.
pub fn encode_strip(key: &FilmstripCacheKey, strip: &CachedStrip) -> Option<Vec<u8>> {
    encode(key, &strip.header(), &strip.rgba)
}
