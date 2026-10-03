//! The playback-proxy cache (Phase 58 — PROXY-05, decisions D-13/D-14/D-15/D-30).
//!
//! # Copied in shape, not re-invented (D-13)
//!
//! `crates/filmstrip/src/cache.rs` settled how this codebase treats a persisted
//! cache that a later run must parse — itself a copy of
//! `crates/app-core/src/probe_cache.rs` and `crates/waveform/src/cache.rs`. This
//! module copies that posture property for property, and each one is enforced by
//! a named test in `crates/proxy/tests/cache.rs`:
//!
//! 1. **Versioned** — a four-character magic ([`PROXY_CACHE_MAGIC`]) plus
//!    [`PROXY_CACHE_VERSION`]. Any other stamp, older or newer, invalidates the
//!    entry outright, so two builds sharing one cache directory degrade to
//!    correct-but-slow, never to wrong.
//! 2. **Bounded** — [`MAX_META_FILE_BYTES`] is checked against the meta file's
//!    METADATA before a single byte is read, [`write_meta`] refuses a meta past
//!    that same cap, and [`prune_bytes`] bounds how many BYTES may accumulate.
//! 3. **Temp-then-rename** — every write lands on a uniquely-nonced temp
//!    sibling, is `sync_all`ed, and only then renamed over the final name.
//! 4. **Best-effort** — [`write_meta`] returns `bool` and [`prune_bytes`]
//!    returns nothing. A cache that cannot be written costs one re-encode, never
//!    a playback failure.
//! 5. **Never errors, never panics** — [`read_fresh`] returns `Option`, and
//!    every hostile input resolves to `None`. This file contains no panicking
//!    construct outside its comments, enforced by `no_unwrap_no_panic_in_cache`,
//!    a source scan that blanks comments before looking.
//!
//! # The two-file adaptation, stated explicitly
//!
//! Filmstrip puts its header INSIDE the payload, because its payload is a
//! private raw-RGBA blob it alone parses. A proxy's payload is an **MP4** — a
//! real video file the media engine opens directly — so a header cannot be
//! prepended to it without making it unopenable. The discipline therefore
//! adapts to a **two-file** shape sharing one stem:
//!
//! ```text
//! {stem}.proxy.mp4    the payload: a real, playable, all-intra MP4
//! {stem}.proxy.json   the meta:    magic + version + the FULL key + the
//!                                  generation params + the geometry +
//!                                  payload_bytes
//! ```
//!
//! and the meta is written **LAST**, as the commit marker. That single ordering
//! rule is what preserves filmstrip's whole-or-absent property across two files:
//!
//! * a killed or crashed encode leaves at most a payload with no meta, which is
//!   a MISS (`read_fresh` stats the meta first) and is swept as an orphan by
//!   [`prune_bytes`];
//! * a meta only ever exists once its payload is complete, because
//!   [`write_meta`] refuses to commit a meta whose payload is absent or whose
//!   length disagrees with `payload_bytes`;
//! * and the meta itself lands atomically, so a reader sees the previous whole
//!   meta or the new whole meta, never a mixture.
//!
//! The FULL key is stored inside the meta, not merely hashed into the name: the
//! name is a 64-bit FNV hash, and a collision must degrade to a MISS, never to a
//! confidently-served WRONG proxy (T-58-02-04).
//!
//! # Threat model
//!
//! * **T-58-02-01 (a corrupt or mismatched meta)** — properties 1, 2 and 5, plus
//!   [`ProxyMeta::is_sane`]'s geometry clamps and the `payload_bytes`
//!   cross-check against the MP4's real length.
//! * **T-58-02-02 (a partial file at the final path after a kill or crash)** —
//!   property 3, plus the meta-last commit rule and [`prune_bytes`]'s stale-temp
//!   sweep.
//! * **T-58-02-03 (unbounded disk growth)** — [`prune_bytes`] against
//!   [`MAX_PROXY_CACHE_BYTES`].
//! * **T-58-02-04 (a filename hash collision)** — the full-key-inside-the-file
//!   rule above.
//!
//! # What the read path may cost
//!
//! [`read_fresh`] runs on the resolve path (D-18), so it is **stats plus one
//! bounded file read**, plus — on a HIT only, and at most once per
//! [`LRU_TOUCH_INTERVAL`] per entry — **one mtime write** to record the use for
//! [`prune_bytes`]. It never opens a video and never starts a subprocess. A
//! resolve that cost more than that would spend the very time the proxy exists
//! to save. `the_read_path_starts_no_subprocess_and_decodes_nothing` pins the
//! no-subprocess/no-decode half at the source level rather than by inspection,
//! and `a_hot_entry_is_not_re_touched_on_every_hit` pins the write half.
//!
//! **That write used to be unconditional, and it was wrong** (58-REVIEW
//! WR-03). The resolve path was believed to run at session-open granularity;
//! 58-07 and 58-09 then MEASURED it at one call per pooled layer per composited
//! tick, on both decode arms (`deferred-items.md` §§ D-15, D-16). An
//! unconditional open-for-write at that granularity is a per-frame filesystem
//! write on the playback producer thread, which is exactly the class of cost
//! this phase exists to remove. Quantizing it costs nothing the byte budget
//! actually reads — see [`LRU_TOUCH_INTERVAL`].

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The directory, under the host's `AppCtx::app_cache_dir()`, that holds the
/// proxy pairs (D-14). Same root as `filmstrips/` and `posters/`. Named here so
/// the job side and the resolve side use one location rather than each
/// inventing its own.
pub const PROXY_CACHE_DIR_NAME: &str = "proxies";

/// Bump when the meaning of ANY meta field changes, or when the payload's
/// expected shape does. A bump invalidates every existing proxy by
/// construction: the version is in the key material (so the file name changes)
/// AND in the meta (so an old file at a colliding name still misses).
///
/// **v2** (debug `proxy-bitrate-starved-all-intra`, 2026-08-03): the encode
/// moved from a flat `-b:v 6_000_000` — a long-GOP-shaped constant that
/// starved ALL-INTRA encodes of dense/high-fps content down to 0.19
/// bits/pixel and macroblocked playback — to Media Foundation quality
/// targeting ([`PROXY_QUALITY`]). The meta's `bitrate_bps` field was replaced
/// by `quality` + `fallback_bpp_milli`, so every v1 proxy must MISS: the v1
/// name is no longer derivable (version and params are in the key material)
/// and a v1 meta at any name fails BOTH the version compare and serde (its
/// field set no longer matches). Orphaned v1 pairs are ordinary LRU fodder
/// for [`prune_bytes`] — never served, evicted first under byte pressure.
pub const PROXY_CACHE_VERSION: u32 = 2;

/// First field of every meta file. A file that does not carry this is not ours,
/// whatever its name says. (`RPXM` = Rudis ProXy Meta.)
pub const PROXY_CACHE_MAGIC: &str = "RPXM";

/// The one derived geometry policy (D-05): scale the LONG edge down to this
/// target, preserve the aspect ratio, snap both dimensions even. Sources already
/// at or below it are not proxied at all — there is nothing to win.
pub const PROXY_LONG_EDGE: u32 = 960;

/// The Media Foundation quality target driving the SHIPPED proxy encode —
/// one name for `engine::PROXY_ENCODE_QUALITY`, which owns the measured
/// rationale (debug `proxy-bitrate-starved-all-intra`, 2026-08-03).
///
/// This REPLACED `PROXY_BITRATE_BPS = 6_000_000`. That constant was
/// long-GOP-shaped and FLAT: applied to an ALL-INTRA encode it budgeted a
/// dense 4K 59.94 fps source at 0.19 bits/pixel (professional all-intra
/// proxies sit near 0.7), and because it never scaled with fps, per-frame
/// quality silently halved as source fps rose. Measured on the reporting
/// clip: SSIM 0.767 / 24.96 dB PSNR-Y — visible macroblocking and chroma
/// bleed during playback. Quality targeting adapts spend to content AND fps
/// (measured 23.2 Mbps on that worst case, ~7.8 Mbps on a typical 30 fps
/// clip, SSIM 0.941 / 32.68 dB at this value) — and, unlike a per-source
/// derived bitrate, it stays a BUILD CONSTANT, which is what lets it live in
/// the key material below without the resolve path ever probing a source
/// (D-18).
pub const PROXY_QUALITY: u32 = engine::PROXY_ENCODE_QUALITY;

/// Bits-per-pixel, in thousandths, for the NON-MF fallback bitrate — the
/// bitrate a DEV-encoder-override proxy encode receives, computed by the job
/// side as `bpp x proxy_w x proxy_h x source_fps` (0.650 bpp; the measured
/// `-b:v` 20 Mbps arm scored SSIM 0.928 at 0.63 bpp on the worst-case clip).
/// The shipped `h264_mf` path never reads it. It is key material anyway:
/// changing this policy must invalidate any dev-override proxy by
/// construction, exactly as every other generation param does.
pub const PROXY_FALLBACK_BPP_MILLI: u64 = 650;

/// The total-bytes budget for the whole proxy directory (D-15, D-30).
///
/// **The byte math this number comes from (re-measured 2026-08-03, debug
/// `proxy-bitrate-starved-all-intra`):** at [`PROXY_QUALITY`] the spend is
/// content- and fps-adaptive, so the budget buys a RANGE rather than one
/// number — measured on real footage: worst-case dense-motion 4K 59.94 fps
/// costs ~23 Mbps ≈ **~174 MB per minute** (4 GiB ≈ **~24 minutes** resident),
/// while a typical 30 fps clip costs ~8 Mbps ≈ **~58 MB per minute** (4 GiB ≈
/// **~70 minutes**). The old "90 minutes" figure belonged to the flat 6 Mbps
/// constant, which bought that headline by shipping macroblocked proxies of
/// exactly the footage proxies exist for. Still comfortably more than a
/// beginner's project holds, and an eviction costs one ~7 s re-encode, never
/// correctness. An entry-count cap (filmstrip's `MAX_CACHE_ENTRIES`) is the
/// WRONG bound here: filmstrip entries are megabytes and uniform, proxies are
/// hundreds of megabytes and wildly unequal, so 256 entries could mean 2 GB or
/// 200 GB.
///
/// This is a **FIXED** constant by decision **D-30**. Free-disk-space awareness
/// (shrinking the budget on a nearly-full volume) is explicitly **not** this
/// phase's job and is recorded as a deferred item rather than smuggled in.
///
/// Dropping an entry costs exactly one re-encode, never correctness.
pub const MAX_PROXY_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Refuse to hand an unbounded byte count to the JSON parser. A real meta is a
/// few hundred bytes; 64 KiB is enormous slack. Checked against the file's
/// METADATA before any read, in that order (T-53.2-06's discipline, inherited).
pub const MAX_META_FILE_BYTES: u64 = 64 * 1024;

/// Suffix of the payload file — a real, playable MP4.
pub const PROXY_PAYLOAD_SUFFIX: &str = ".proxy.mp4";

/// Suffix of the meta file — the commit marker.
pub const PROXY_META_SUFFIX: &str = ".proxy.json";

/// Substring shared by both members of a pair, and by every temp sibling this
/// module writes. Only files carrying it are ever deleted by [`prune_bytes`], so
/// a foreign file that happens to live in the cache directory is skipped rather
/// than destroyed (filmstrip's "only its own extension" rule).
const PROXY_INFIX: &str = ".proxy.";

/// Marker embedded in every in-flight temp sibling.
const TMP_MARKER: &str = ".tmp.";

/// Geometry sanity clamps. A meta violating ANY of them is a MISS, never a
/// crash and never a partially-trusted read (T-58-02-01). Deliberately wider
/// than [`proxy_dims`]'s own output, so a proxy written by a DIFFERENT build's
/// policy is rejected on its params rather than on its pixels.
pub const MIN_PROXY_DIM: u32 = 2;
/// See [`MIN_PROXY_DIM`].
pub const MAX_PROXY_DIM: u32 = 8192;

/// How long a temp sibling must have sat untouched before [`prune_bytes`]
/// treats it as an app-killed-mid-encode leftover (D-11) rather than as an
/// in-flight write belonging to a live writer.
pub const STALE_TMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Monotonic per-write nonce so a temp file is unique per call — mirrors
/// `filmstrip::cache::TMP_COUNTER` and its stated reason: two concurrent
/// writers must never share a `.tmp` handle and interleave-corrupt it before
/// the rename.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// D-13's identity: `(canonical path, mtime_ns, size_bytes)`.
///
/// `mtime_ns`, NOT `mtime_ms`. Windows file timestamps advance in coarse
/// (~15.6 ms) steps, so a millisecond-truncated stamp can compare EQUAL across
/// two genuinely different writes and serve a stale proxy forever. Same grain,
/// and the same reasoning, as `probe_cache::ProbeCacheKey`,
/// `waveform::cache::WaveformCacheKey` and `filmstrip::cache::FilmstripCacheKey`
/// — one codebase, one convention.
///
/// Keying on the MEDIA FILE rather than on a clip is what makes a trim, a
/// split, a duplicate and a move cost zero new encoding: every one of them
/// selects a window into the one proxy the file already has.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProxyCacheKey {
    pub path: String,
    pub mtime_ns: u64,
    pub size_bytes: u64,
}

/// Everything about a proxy except its pixels. Written LAST, as the commit
/// marker for the `{stem}.proxy.mp4` beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyMeta {
    pub magic: String,
    pub version: u32,
    /// The FULL key, so a filename hash collision degrades to a MISS.
    pub key: ProxyCacheKey,
    /// The generation params this proxy was produced under. They are ALSO in
    /// the file name (via the key material), so a policy change invalidates by
    /// construction; storing them here is the second, independent line of
    /// defence for a file that somehow reaches the right name anyway.
    pub long_edge: u32,
    /// The MF quality target the shipped encode ran under ([`PROXY_QUALITY`]).
    /// Replaced v1's `bitrate_bps` — a rename ON PURPOSE, so a v1 meta also
    /// fails serde, not merely the version compare.
    pub quality: u32,
    /// The non-MF fallback policy in force ([`PROXY_FALLBACK_BPP_MILLI`]).
    /// Constant policy, not the per-source derived bitrate: the derived value
    /// depends on source fps, which the resolve path must never probe (D-18).
    pub fallback_bpp_milli: u64,
    pub encoder: String,
    pub proxy_w: u32,
    pub proxy_h: u32,
    /// The payload's exact length on disk. Cross-checked at read time, which is
    /// what turns a truncated killed-mid-encode file into a MISS.
    pub payload_bytes: u64,
}

impl ProxyMeta {
    /// A meta describing a proxy generated by THIS build, under THIS build's
    /// policy. The job side (plan 58-03) is the only production caller.
    pub fn new(key: ProxyCacheKey, proxy_w: u32, proxy_h: u32, payload_bytes: u64) -> ProxyMeta {
        ProxyMeta {
            magic: PROXY_CACHE_MAGIC.to_string(),
            version: PROXY_CACHE_VERSION,
            key,
            long_edge: PROXY_LONG_EDGE,
            quality: PROXY_QUALITY,
            fallback_bpp_milli: PROXY_FALLBACK_BPP_MILLI,
            encoder: effective_encoder(),
            proxy_w,
            proxy_h,
            payload_bytes,
        }
    }

    /// Every clamp, in one place, so [`read_fresh`] and [`write_meta`] cannot
    /// drift: a meta this rejects is never written, and never trusted if it
    /// appears on disk anyway.
    ///
    /// The KEY is deliberately not checked here — it can only be judged against
    /// the source file the caller is asking about.
    pub fn is_sane(&self) -> bool {
        self.magic == PROXY_CACHE_MAGIC
            && self.version == PROXY_CACHE_VERSION
            && self.long_edge == PROXY_LONG_EDGE
            && self.quality == PROXY_QUALITY
            && self.fallback_bpp_milli == PROXY_FALLBACK_BPP_MILLI
            && self.encoder == effective_encoder()
            && dims_are_sane(self.proxy_w, self.proxy_h)
            && self.payload_bytes > 0
    }
}

/// What a cache HIT hands back: where the playable proxy is, and the geometry
/// the caller should expect out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyHit {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

fn dims_are_sane(w: u32, h: u32) -> bool {
    w % 2 == 0
        && h % 2 == 0
        && (MIN_PROXY_DIM..=MAX_PROXY_DIM).contains(&w)
        && (MIN_PROXY_DIM..=MAX_PROXY_DIM).contains(&h)
}

/// The cache key for a real file on disk.
///
/// `None` means "not cacheable" — a stat failure, a non-file, an unreadable or
/// absurd timestamp. `None` is NEVER a failure: the caller simply plays the
/// original. Deliberately `Option`, not `Result`, exactly as
/// `filmstrip::cache::key_for` is.
pub fn key_for(path: &Path) -> Option<ProxyCacheKey> {
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
    Some(ProxyCacheKey {
        path: canonical.to_string_lossy().into_owned(),
        mtime_ns,
        size_bytes: meta.len(),
    })
}

/// FNV-1a, 64-bit. Hand-rolled ON PURPOSE, for the reason
/// `crates/waveform/src/cache.rs` and `crates/filmstrip/src/cache.rs` both state
/// at length: the default hasher behind `std`'s `HashMap` is explicitly
/// documented as NOT stable across Rust releases, so deriving a FILENAME from it
/// would silently orphan every cached file on a toolchain bump — an invisible
/// cache wipe that looks like nothing at all. FNV-1a is fixed arithmetic, so the
/// same key yields the same name on every build, forever.
///
/// (Not a security hash, and it does not need to be: a collision degrades to a
/// MISS, because the full key is stored inside the meta and compared.)
///
/// In-house precedent, copied from our own crate — not a third-party borrow, so
/// no `PROVENANCE.md` entry is owed.
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

/// The encoder identity that goes into the key and into the meta.
///
/// Resolved EXACTLY as the engine's proxy-encode entry point resolves it, so
/// the key always describes the encoder that actually ran: the DEV-only
/// override if it is set to a non-blank value, otherwise the cleared, shipped
/// `DEFAULT_VIDEO_ENCODER`.
///
/// **No warning is printed here.** This is the READ path, called per resolve;
/// the loud, unmissable override warning lives on the encode path where the
/// choice is actually made (58-01's engine entry point). Including the
/// encoder in the key material is what makes a DEV-override proxy structurally
/// unable to masquerade as a shipped-encoder proxy (D-03): the two land under
/// different names AND fail each other's stored-params compare.
pub fn effective_encoder() -> String {
    match std::env::var(engine::DEV_ENCODER_OVERRIDE_ENV) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => engine::DEFAULT_VIDEO_ENCODER.to_string(),
    }
}

/// The exact string that is hashed into a file name. Kept as its own function
/// so `generation_params_and_version_are_all_in_the_key_material` can vary one
/// field at a time without touching a process-global environment variable.
///
/// Every param here is a BUILD CONSTANT, and must stay one: the stem is
/// computed on the resolve path, which may never probe a source (D-18), so a
/// per-source value (like v1's idea of a derived bitrate) can never be key
/// material. The `q`/`bpp` prefixes keep the two numeric policies from ever
/// colliding positionally with each other or with `long_edge`.
fn stem_material(
    key: &ProxyCacheKey,
    long_edge: u32,
    quality: u32,
    fallback_bpp_milli: u64,
    encoder: &str,
    version: u32,
) -> String {
    format!(
        "{}|{}|{}|{}|q{}|bpp{}|{}|v{}",
        key.path, key.mtime_ns, key.size_bytes, long_edge, quality, fallback_bpp_milli, encoder,
        version
    )
}

/// The filesystem-safe, deterministic file stem for a key under EXPLICIT
/// generation params.
///
/// The size is appended in hex as a cheap second discriminator, so two keys must
/// collide in BOTH the hash and the source file size to share a name.
pub fn file_stem_for_params(
    key: &ProxyCacheKey,
    long_edge: u32,
    quality: u32,
    fallback_bpp_milli: u64,
    encoder: &str,
    version: u32,
) -> String {
    let material = stem_material(key, long_edge, quality, fallback_bpp_milli, encoder, version);
    let hash = fnv1a64(material.as_bytes());
    format!("{hash:016x}-{size:016x}", size = key.size_bytes)
}

/// [`file_stem_for_params`] applied to THIS build's policy — the form every
/// production caller uses. D-13's "invalidate by construction, not by
/// remembering to" lives in this one line: change any constant it reads and
/// every existing proxy stops being findable.
pub fn file_stem_for(key: &ProxyCacheKey) -> String {
    file_stem_for_params(
        key,
        PROXY_LONG_EDGE,
        PROXY_QUALITY,
        PROXY_FALLBACK_BPP_MILLI,
        &effective_encoder(),
        PROXY_CACHE_VERSION,
    )
}

/// `{stem}.proxy.mp4` — the playable payload.
pub fn payload_file_name(stem: &str) -> String {
    format!("{stem}{PROXY_PAYLOAD_SUFFIX}")
}

/// `{stem}.proxy.json` — the meta, and the commit marker.
pub fn meta_file_name(stem: &str) -> String {
    format!("{stem}{PROXY_META_SUFFIX}")
}

/// Round up to the nearest even value, then hold it inside the clamps
/// [`read_fresh`] enforces. Encoders demand even dimensions for 4:2:0 chroma;
/// a zero-height "proxy" is not a proxy at all, hence the floor of
/// [`MIN_PROXY_DIM`].
fn snap_even(x: u32) -> u32 {
    let even = x.saturating_add(1) & !1;
    even.clamp(MIN_PROXY_DIM, MAX_PROXY_DIM)
}

/// D-05's one geometry policy, derived rather than configured.
///
/// `None` means **do not proxy this source at all**: it is already at or below
/// the target long edge, so a proxy would cost an encode and buy nothing. That
/// is a policy answer, not an error.
///
/// Orientation is preserved (a portrait source yields a portrait proxy), the
/// long edge lands exactly on [`PROXY_LONG_EDGE`], and both dimensions are
/// snapped even.
pub fn proxy_dims(src_w: u32, src_h: u32) -> Option<(u32, u32)> {
    if src_w == 0 || src_h == 0 {
        return None;
    }
    let long = src_w.max(src_h);
    if long <= PROXY_LONG_EDGE {
        return None;
    }
    let short = src_w.min(src_h);

    // Round-half-up in integer arithmetic, in u64 so a huge source cannot
    // overflow the multiply.
    let long_u = long as u64;
    let scaled = ((short as u64) * (PROXY_LONG_EDGE as u64) + long_u / 2) / long_u;
    let short_out = snap_even(u32::try_from(scaled).unwrap_or(MAX_PROXY_DIM));
    let long_out = snap_even(PROXY_LONG_EDGE);

    if src_w >= src_h {
        Some((long_out, short_out))
    } else {
        Some((short_out, long_out))
    }
}

/// How far behind reality a HIT's recorded use may drift before [`read_fresh`]
/// pays a write to correct it (58-REVIEW **WR-03**).
///
/// The eviction order [`prune_bytes`] needs is a ranking, not a timestamp: two
/// entries used within the same minute are interchangeable to it, and an entry
/// used within the last minute already sorts ahead of every entry that was not.
/// So quantizing "recently used" to this interval loses nothing the bound
/// actually consumes.
///
/// What it buys is the whole point. `read_fresh` runs on the RESOLVE path, and
/// 58-07/58-09 measured that path at one call per pooled layer per composited
/// tick on **both** decode arms (`deferred-items.md` §§ D-15, D-16) — so an
/// unconditional touch is an open-for-write on the playback producer thread,
/// per layer, per frame, on a volume that may be network-backed or
/// AV-scanned. With this interval a hot entry pays it once a minute instead.
///
/// **MEASURED 2026-08-03** on this machine by the instrument D-18 asked for
/// (`crates/preview/tests/proxy_attribution.rs`'s
/// `PROXY-RESOLVE-POOL-GRANULARITY` line, two 720p layers, software pool arm),
/// same run shape either side:
///
/// | touch policy | `mean_us` per resolve | `pct_of_wall` |
/// |---|---|---|
/// | unconditional (as reviewed) | **735.7** | **5.72** |
/// | quantized (this constant) | **523–557** | **4.07–4.33** |
///
/// i.e. about a quarter of the resolve's cost was the LRU write. The rest is
/// the `canonicalize`, which is D-15's separate finding and is NOT addressed
/// here — memoizing it breaks
/// `deleting_the_cache_mid_project_costs_only_speed` (V-15), which 58-07 proved
/// by mutation, so the hoist D-15 prefers is the shape that fixes it.
pub const LRU_TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// Best-effort mtime bump, so [`prune_bytes`] is LRU-on-USE rather than
/// LRU-on-WRITE — but only when the recorded use is actually stale by
/// [`LRU_TOUCH_INTERVAL`].
///
/// `recorded` is the mtime the caller ALREADY has in hand from the `metadata`
/// call it had to make anyway, so the skip costs zero syscalls: the cheap case
/// is a comparison, and only the rare case opens a file.
///
/// A mtime in the FUTURE (clock skew, a copied tree) answers `Err` from
/// `duration_since` and is treated as "just used" — such an entry already sorts
/// as newest, so correcting it would buy nothing.
///
/// Every error is swallowed: failing to record a use costs a slightly wrong
/// eviction order, never a wrong answer.
fn touch_if_stale(path: &Path, recorded: Option<SystemTime>) {
    if let Some(recorded) = recorded {
        if SystemTime::now()
            .duration_since(recorded)
            .unwrap_or_default()
            < LRU_TOUCH_INTERVAL
        {
            return;
        }
    }
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Is there a usable, CURRENT proxy for `source_path` in `dir`?
///
/// `None` covers absent, stale, corrupt, oversized, truncated, wrong-policy and
/// never-written alike — the caller cannot distinguish them and must not try
/// (D-17: fallback to the original is silent, per-clip and total).
///
/// Cost: `canonicalize` + two `metadata` calls + one bounded file read, plus —
/// on a HIT, and at most once per [`LRU_TOUCH_INTERVAL`] per entry — one mtime
/// write to record the use (58-REVIEW **WR-03**; it was unconditional before,
/// which put an open-for-write on the per-tick resolve path). No video is
/// opened and no subprocess is started, because this runs on the resolve path
/// (D-18).
///
/// The order of checks is deliberate: the meta's METADATA length is settled
/// BEFORE any allocation, so a hostile file cannot cost memory merely by
/// existing (T-58-02-01).
pub fn read_fresh(source_path: &Path, dir: &Path) -> Option<ProxyHit> {
    let key = key_for(source_path)?;
    let stem = file_stem_for(&key);

    let meta_path = dir.join(meta_file_name(&stem));
    let meta_stat = std::fs::metadata(&meta_path).ok()?;
    if !meta_stat.is_file() {
        return None;
    }
    if meta_stat.len() > MAX_META_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&meta_path).ok()?;
    let meta: ProxyMeta = serde_json::from_slice(&bytes).ok()?;

    // Magic, version, generation params and geometry clamps, in one place.
    if !meta.is_sane() {
        return None;
    }
    // The name is a hash; the KEY is the truth. A collision misses here.
    if meta.key != key {
        return None;
    }

    // The payload must exist and be EXACTLY as long as the meta committed it.
    // A killed-mid-encode file is shorter; a re-used name is different.
    let payload_path = dir.join(payload_file_name(&stem));
    let payload_stat = std::fs::metadata(&payload_path).ok()?;
    if !payload_stat.is_file() {
        return None;
    }
    if payload_stat.len() != meta.payload_bytes {
        return None;
    }

    // LRU-on-use, quantized. `meta_stat` is the metadata this function already
    // had to read, so the common (hot) case adds no syscall at all.
    touch_if_stale(&meta_path, meta_stat.modified().ok());

    Some(ProxyHit {
        path: payload_path,
        width: meta.proxy_w,
        height: meta.proxy_h,
    })
}

/// Commit the proxy at `{stem}.proxy.mp4` by publishing its meta. Returns
/// whether it landed.
///
/// **Call this LAST**, after the payload is complete on disk: it is the commit
/// marker, and it refuses to commit a payload that is absent or whose length
/// disagrees with `meta.payload_bytes`. That refusal is what makes the
/// meta-last rule an enforced invariant rather than a documented convention
/// (T-58-02-02).
///
/// It also refuses at write time exactly what [`read_fresh`] refuses at read
/// time (filmstrip's `write_refuses_a_payload_it_could_never_read_back`): a meta
/// that would MISS forever is disk burned for nothing.
///
/// The meta itself lands on a uniquely-nonced temp sibling, is `sync_all`ed, and
/// only then renamed over the final name — so a concurrent reader observes the
/// previous whole meta or the new whole meta, never a mixture.
///
/// BEST-EFFORT: any failure is reported on stderr and returned as `false`.
/// Nothing here is an `Err`, and nothing here panics — losing the cache costs
/// one re-encode, never a playback failure.
pub fn write_meta(dir: &Path, stem: &str, meta: &ProxyMeta) -> bool {
    if !meta.is_sane() {
        eprintln!(
            "proxy cache: refusing to persist an unreadable meta for {} \
             ({}x{}, {} bytes, encoder {}, v{}) — it could never be read back",
            meta.key.path, meta.proxy_w, meta.proxy_h, meta.payload_bytes, meta.encoder,
            meta.version
        );
        return false;
    }

    // The commit marker may not commit a payload that is not there.
    let payload_path = dir.join(payload_file_name(stem));
    match std::fs::metadata(&payload_path) {
        Ok(stat) if stat.is_file() && stat.len() == meta.payload_bytes => {}
        Ok(stat) => {
            eprintln!(
                "proxy cache: refusing to commit {} — the payload is {} bytes, the meta \
                 claims {}",
                payload_path.display(),
                stat.len(),
                meta.payload_bytes
            );
            return false;
        }
        Err(e) => {
            eprintln!(
                "proxy cache: refusing to commit {} — its payload is unreadable: {e}",
                payload_path.display()
            );
            return false;
        }
    }

    let Ok(bytes) = serde_json::to_vec(meta) else {
        eprintln!("proxy cache: meta for {} would not serialize", meta.key.path);
        return false;
    };
    if bytes.len() as u64 > MAX_META_FILE_BYTES {
        eprintln!(
            "proxy cache: refusing a {}-byte meta for {} — past the \
             {MAX_META_FILE_BYTES}-byte cap the reader enforces",
            bytes.len(),
            meta.key.path
        );
        return false;
    }

    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("proxy cache: cannot create {}: {e}", dir.display());
        return false;
    }

    let name = meta_file_name(stem);
    let final_path = dir.join(&name);
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!("{name}{TMP_MARKER}{}-{nonce:x}", std::process::id()));

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
            // to ask for it (filmstrip's discipline, verbatim).
            prune_bytes(dir, MAX_PROXY_CACHE_BYTES);
            true
        }
        Err(e) => {
            eprintln!(
                "proxy cache: write failed for {}: {e}",
                final_path.display()
            );
            // Never leak a stray temp sibling behind a failed write.
            let _ = std::fs::remove_file(&tmp_path);
            false
        }
    }
}

/// The two halves of one cache entry, as [`prune_bytes`] finds them on disk.
/// Either may be missing — that is what makes the other an ORPHAN.
#[derive(Default)]
struct PairHalves {
    payload: Option<(SystemTime, u64, PathBuf)>,
    meta: Option<(SystemTime, u64, PathBuf)>,
}

/// Bound the whole cache directory to `budget_bytes`, evicting
/// least-recently-used PAIRS first, and sweep everything that can never be a
/// usable entry again.
///
/// # Why a byte budget and not an entry count (D-15)
///
/// Every other cache in this codebase bounds ENTRY COUNT, because its entries
/// are small and roughly uniform. Proxies are neither: a 5-second clip and a
/// 40-minute one differ by three orders of magnitude, so `N` entries could mean
/// 200 MB or 200 GB. Only a byte total actually bounds the disk. The eviction
/// SHAPE below is `filmstrip::cache::prune`'s, copied deliberately — sort
/// newest-first, tie-break for determinism, best-effort, never panic. The stop
/// condition is the only thing that changes.
///
/// # The rules, in order
///
/// 1. **Pairs.** A `{stem}.proxy.mp4` and `{stem}.proxy.json` together are one
///    entry: `pair_bytes` is their sum, and `pair_mtime` is the LATER of the
///    two, so a read that bumped only the meta still counts as a use.
/// 2. **Newest first, ties broken by stem**, so two runs over the same
///    directory make the same decision.
/// 3. **Evict past the budget.** Walking newest-first, the first pair that
///    pushes the running total OVER `budget_bytes` is evicted along with every
///    older pair, so the survivors always fit. Each eviction removes the mp4
///    first and the json second — the big bytes go first, and either order of
///    interruption leaves only an orphan, which is a MISS and is swept on the
///    next run.
/// 4. **Sweep.** An mp4 with no meta (a killed-mid-encode leftover, D-11), a
///    meta with no mp4 (unusable, a permanent MISS), and any temp sibling older
///    than [`STALE_TMP_AGE`] all go. A FRESH temp sibling belongs to a live
///    writer and is left strictly alone.
/// 5. **Only our own files.** A name must carry [`PROXY_INFIX`] to be
///    considered at all, so a foreign file sharing the directory is never
///    touched. Sub-directories are not walked.
///
/// BEST-EFFORT: every filesystem error is swallowed and nothing is returned.
/// Failing to prune costs disk, never correctness, so it must never become a
/// playback or import failure.
///
/// **Known edge, recorded rather than hidden:** a single pair larger than the
/// entire budget is evicted immediately, because rule 3 is a hard bound. At
/// [`MAX_PROXY_CACHE_BYTES`] that means one proxy over ~90 minutes long, which
/// would then be regenerated and re-evicted. Raising the budget is the answer;
/// see this plan's deferred items.
pub fn prune_bytes(dir: &Path, budget_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let now = SystemTime::now();
    let mut halves: BTreeMap<String, PairHalves> = BTreeMap::new();
    let mut doomed: Vec<PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.contains(PROXY_INFIX) {
            continue;
        }
        let Ok(stat) = entry.metadata() else {
            continue;
        };
        if !stat.is_file() {
            continue;
        }
        // An unreadable timestamp sorts as "oldest", so such a file is dropped
        // before any file whose age is actually known (filmstrip's rule).
        let mtime = stat.modified().unwrap_or(UNIX_EPOCH);

        // Temp siblings are checked FIRST. They end in neither suffix, so this
        // is the only branch that can ever see one, and the only place that can
        // decide whether it is in flight or dead.
        if name.contains(TMP_MARKER) {
            let age = now.duration_since(mtime).unwrap_or_default();
            if age >= STALE_TMP_AGE {
                doomed.push(path);
            }
            continue;
        }

        let classified = match name.strip_suffix(PROXY_PAYLOAD_SUFFIX) {
            Some(stem) => Some((stem.to_string(), true)),
            None => name
                .strip_suffix(PROXY_META_SUFFIX)
                .map(|stem| (stem.to_string(), false)),
        };
        let Some((stem, is_payload)) = classified else {
            continue;
        };
        let slot = halves.entry(stem).or_default();
        if is_payload {
            slot.payload = Some((mtime, stat.len(), path));
        } else {
            slot.meta = Some((mtime, stat.len(), path));
        }
    }

    // (pair_mtime, stem, pair_bytes, payload, meta)
    let mut pairs: Vec<(SystemTime, String, u64, PathBuf, PathBuf)> = Vec::new();
    for (stem, half) in halves {
        match (half.payload, half.meta) {
            (Some(p), Some(m)) => {
                pairs.push((p.0.max(m.0), stem, p.1.saturating_add(m.1), p.2, m.2));
            }
            (Some(p), None) => doomed.push(p.2),
            (None, Some(m)) => doomed.push(m.2),
            (None, None) => {}
        }
    }

    pairs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut running: u64 = 0;
    let mut over_budget = false;
    for (_, _, bytes, payload, meta) in pairs {
        if !over_budget {
            running = running.saturating_add(bytes);
            if running <= budget_bytes {
                continue;
            }
            over_budget = true;
        }
        doomed.push(payload);
        doomed.push(meta);
    }

    for path in doomed {
        let _ = std::fs::remove_file(path);
    }
}
