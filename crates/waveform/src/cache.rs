//! The per-media-item peak cache (SHELL-09, D-18).
//!
//! # The five properties, copied rather than re-invented
//!
//! `crates/app-core/src/probe_cache.rs` already settled how this codebase
//! treats a persisted cache that a later run must parse. This module copies
//! that posture property for property, and each one is enforced by a named
//! test in `crates/waveform/tests/cache.rs`:
//!
//! 1. **Versioned** -- a four-byte magic plus [`CACHE_VERSION`]. Any other
//!    stamp, older or newer, invalidates the file outright, so two builds
//!    sharing one cache directory degrade to correct-but-slow, never to
//!    wrong. (`a_wrong_magic_file_is_a_miss`,
//!    `a_future_version_file_is_a_miss`)
//! 2. **Bounded** -- [`MAX_CACHE_FILE_BYTES`] is checked against the file's
//!    METADATA before a single byte is read, [`write`] refuses a payload past
//!    that same cap, and [`prune`] bounds how many files may accumulate.
//!    (`an_oversized_file_is_a_miss_without_reading_it`,
//!    `write_refuses_a_payload_it_could_never_read_back`,
//!    `prune_keeps_the_directory_bounded`)
//! 3. **Temp-then-rename** -- every write lands on a uniquely-nonced temp
//!    sibling, is `sync_all`ed, and only then renamed over the final name, so
//!    a reader never observes a torn file and two concurrent writers never
//!    share a handle. (`write_is_atomic_temp_then_rename`)
//! 4. **Best-effort** -- [`write`] returns `bool` and [`prune`] returns
//!    nothing. A cache that cannot be written costs one recomputation, never
//!    an import failure. (`write_failure_is_swallowed_never_propagated`)
//! 5. **Never errors, never panics** -- [`read`] returns `Option`, and every
//!    hostile input (truncated, garbage, wrong magic, wrong version,
//!    key-mismatched, oversized, a directory) resolves to `None`. This file
//!    contains no `unwrap`, no `expect`, no explicit panic, and no indexing
//!    that can go out of bounds; all three are grep-enforced by the plan's
//!    acceptance criteria. (every `*_is_a_miss*` test)
//!
//! # Threat model
//!
//! * **T-52-07 (tampering with a cache file)** -- property 2 and 5 above. The
//!   size bound is checked on the METADATA, so a hostile file cannot cause an
//!   allocation merely by existing.
//! * **T-52-08 (stale peaks for a changed file)** -- the key; see
//!   [`WaveformCacheKey`].
//! * **T-52-09 (interleaved concurrent writes)** -- property 3.
//! * **T-52-10 (unbounded directory growth)** -- property 2's [`prune`], run
//!   after every successful write so the bound belongs to the cache rather
//!   than to whichever caller remembers to ask for it.
//!
//! # Storage shape
//!
//! ONE FILE PER MEDIA ITEM, following the POSTER cache
//! (`crates/app-core/src/import.rs`'s `poster_dir.join(format!("{id}.png"))`)
//! rather than the probe cache's single combined JSON. Peak arrays are sized
//! like posters, not like tiny metadata structs, and a combined file would be
//! rewritten in full on every import.
//!
//! On-disk layout, little-endian throughout:
//!
//! ```text
//! magic(4) | version(u32) | block_us(i64) | sample_rate(u32) |
//! key_len(u32) | key_json(key_len) | peak_count(u32) | peaks(peak_count)
//! ```
//!
//! The FULL key is stored inside the file, not merely hashed into its name:
//! the filename is a 64-bit hash, and a collision must degrade to a MISS, not
//! to a confidently-served WRONG envelope.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The directory, under the host's `app_cache_dir()`, that holds one file per
/// media item. Named here so the import-side job and the FFI read path use the
/// same location rather than each inventing one.
pub const WAVEFORM_CACHE_DIR_NAME: &str = "waveforms";

/// Bump when the on-disk layout OR the meaning of any field in it changes --
/// including a change to the peak quantisation (u8 today, D-17; widening to
/// u16 is exactly this bump).
pub const CACHE_VERSION: u32 = 1;

/// First four bytes of every cache file. A file that does not start with this
/// is not ours, whatever its name says.
pub const CACHE_MAGIC: &[u8; 4] = b"RWF1";

/// Refuse to hand an unbounded byte count to the parser. ~8 MiB of peaks is
/// ~22 hours at 100 peaks/second, past every real source; a rejected file
/// costs exactly one recomputation.
pub const MAX_CACHE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Hard bound on how many entries the directory may hold. Without it the cache
/// grows for the lifetime of the install: every distinct
/// `(path, mtime_ns, size_bytes)` ever imported is a new key, so re-importing
/// the SAME file after an edit adds one. A dropped entry costs exactly one
/// recomputation.
pub const MAX_CACHE_ENTRIES: usize = 1024;

/// Extension of a final (renamed-into-place) cache file.
pub const CACHE_FILE_EXT: &str = "wfm";

/// Monotonic per-write nonce so a temp file is unique per call -- mirrors
/// `probe_cache::TMP_COUNTER` and its stated reason: two concurrent writers
/// must never share a `.tmp` handle and interleave-corrupt it before the
/// rename.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// D-18's key: `(canonical path, mtime_ns, size_bytes)`.
///
/// `mtime_ns`, NOT `mtime_ms`. Windows file timestamps advance in coarse
/// (~15.6 ms) steps, so a millisecond-truncated stamp can compare EQUAL across
/// two genuinely different writes and serve a stale envelope forever
/// (threat T-52-08). Same grain, and the same reasoning, as
/// `probe_cache::ProbeCacheKey` -- one codebase, one convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WaveformCacheKey {
    pub path: String,
    pub mtime_ns: u64,
    pub size_bytes: u64,
}

/// What a cache HIT hands back.
///
/// `block_us` and `sample_rate` travel WITH the peaks rather than being
/// assumed: a consumer that draws them must scale by the resolution the file
/// was actually written at, not by whatever the current constants happen to
/// be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPeaks {
    pub block_us: i64,
    pub sample_rate: u32,
    pub peaks: Vec<u8>,
}

/// The cache key for a real file on disk.
///
/// `None` means "not cacheable" -- a stat failure, a non-file, an unreadable
/// or absurd timestamp. `None` is NEVER a failure: the caller extracts peaks
/// fresh and simply does not cache them. Deliberately `Option`, not `Result`,
/// exactly as `probe_cache::key_for` is: the import path skips one bad file
/// and never aborts the batch, and a `?`-propagated stat error here would turn
/// a per-file filesystem hiccup into a whole-batch abort.
pub fn key_for(path: &Path) -> Option<WaveformCacheKey> {
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
    Some(WaveformCacheKey {
        path: canonical.to_string_lossy().into_owned(),
        mtime_ns,
        size_bytes: meta.len(),
    })
}

/// FNV-1a, 64-bit. Hand-rolled ON PURPOSE.
///
/// The obvious shortcut -- the default hasher behind `std`'s `HashMap`, in
/// `std::collections::hash_map` -- is explicitly documented as NOT stable
/// across Rust releases. Deriving a FILENAME from it would silently orphan
/// every cached file on a toolchain bump: a slow, invisible cache wipe that
/// looks like nothing at all. FNV-1a is fixed arithmetic, so the same key
/// yields the same name on every build, forever.
///
/// (Not a security hash, and it does not need to be: a collision degrades to a
/// MISS, because the full key is stored inside the file and compared.)
///
/// Nothing in this file may name that unstable hasher, not even in a comment
/// -- the plan's acceptance grep for it is a use-detector, and a mention here
/// would blunt it.
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
/// The size is appended in hex as a cheap second discriminator, so two keys
/// must collide in BOTH the hash and the file size to share a name.
pub fn file_name_for(key: &WaveformCacheKey) -> String {
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
/// Every field of the layout is read through this. It is the reason a
/// truncated or lying file is a MISS rather than an out-of-bounds index.
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

/// Parse cache bytes, requiring them to describe exactly `key`.
///
/// Every failure -- and there is no other kind of outcome here -- is `None`.
fn parse(bytes: &[u8], key: &WaveformCacheKey) -> Option<CachedPeaks> {
    let mut cursor = 0usize;

    if take(bytes, &mut cursor, 4)? != CACHE_MAGIC.as_slice() {
        return None;
    }
    if read_u32(bytes, &mut cursor)? != CACHE_VERSION {
        return None;
    }
    let block_us = read_i64(bytes, &mut cursor)?;
    let sample_rate = read_u32(bytes, &mut cursor)?;

    let key_len = read_u32(bytes, &mut cursor)? as usize;
    let key_json = take(bytes, &mut cursor, key_len)?;
    let stored: WaveformCacheKey = serde_json::from_slice(key_json).ok()?;
    // The name is a hash; the KEY is the truth. A collision misses here.
    if &stored != key {
        return None;
    }

    let peak_count = read_u32(bytes, &mut cursor)? as usize;
    let peaks = take(bytes, &mut cursor, peak_count)?;
    // Trailing bytes mean the file is not what it declares itself to be.
    if cursor != bytes.len() {
        return None;
    }

    Some(CachedPeaks {
        block_us,
        sample_rate,
        peaks: peaks.to_vec(),
    })
}

/// Read the cached envelope for `key` from `dir`, or `None`.
///
/// The order of checks matters and is deliberate: `is_file` and the METADATA
/// length are both settled BEFORE any allocation, so a hostile file cannot
/// cost memory merely by existing (T-52-07).
pub fn read(dir: &Path, key: &WaveformCacheKey) -> Option<CachedPeaks> {
    let path = dir.join(file_name_for(key));
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_CACHE_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    parse(&bytes, key)
}

/// Serialize one cache file's bytes, or `None` if it could never be read back.
fn encode(
    key: &WaveformCacheKey,
    peaks: &[u8],
    block_us: i64,
    sample_rate: u32,
) -> Option<Vec<u8>> {
    let key_json = serde_json::to_vec(key).ok()?;
    let key_len = u32::try_from(key_json.len()).ok()?;
    let peak_count = u32::try_from(peaks.len()).ok()?;

    // 4 magic + 4 version + 8 block_us + 4 sample_rate + 4 key_len
    //   + key_json + 4 peak_count + peaks
    let total = 24usize
        .checked_add(key_json.len())?
        .checked_add(peaks.len())?;
    // Refuse to persist what `read` would refuse to load: such a file would
    // burn disk on bytes guaranteed to MISS forever.
    if total as u64 > MAX_CACHE_FILE_BYTES {
        return None;
    }

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    out.extend_from_slice(&block_us.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&key_len.to_le_bytes());
    out.extend_from_slice(&key_json);
    out.extend_from_slice(&peak_count.to_le_bytes());
    out.extend_from_slice(peaks);
    Some(out)
}

/// Persist `peaks` for `key` under `dir`. Returns whether it landed.
///
/// BEST-EFFORT: any failure is reported on stderr and returned as `false`.
/// Nothing here is an `Err`, and nothing here panics -- losing the cache costs
/// one recomputation, never an import.
pub fn write(
    dir: &Path,
    key: &WaveformCacheKey,
    peaks: &[u8],
    block_us: i64,
    sample_rate: u32,
) -> bool {
    let Some(bytes) = encode(key, peaks, block_us, sample_rate) else {
        eprintln!(
            "waveform cache: refusing to persist {} peaks for {} (past the {MAX_CACHE_FILE_BYTES}-byte cap)",
            peaks.len(),
            key.path
        );
        return false;
    };

    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("waveform cache: cannot create {}: {e}", dir.display());
        return false;
    }

    let name = file_name_for(key);
    let final_path = dir.join(&name);
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!("{name}.tmp.{}-{nonce:x}", std::process::id()));

    // Temp-then-rename: a reader either sees the previous complete file or the
    // new complete file, never a half-written one. `sync_all` before the
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
            // Property 2 belongs to the cache, not to whichever caller
            // remembers to ask (T-52-10).
            prune(dir, MAX_CACHE_ENTRIES);
            true
        }
        Err(e) => {
            eprintln!(
                "waveform cache: write failed for {}: {e}",
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
/// files with this cache's own extension are ever considered, let alone
/// removed.
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

    // Newest first; ties broken by path so two runs over the same directory
    // make the same decision.
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (_, path) in found.iter().skip(max_entries) {
        let _ = std::fs::remove_file(path);
    }
}
