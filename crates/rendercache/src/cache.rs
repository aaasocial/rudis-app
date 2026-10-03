//! The render-cache on-disk substrate (Phase 59 — CACHE-04, decisions
//! D-05/D-06/D-18/D-21/D-25).
//!
//! # Copied in shape, not re-invented (D-25)
//!
//! `crates/proxy/src/cache.rs` is this file's reference implementation, and it
//! is itself a copy of `crates/filmstrip/src/cache.rs`, which copied
//! `crates/app-core/src/probe_cache.rs` and `crates/waveform/src/cache.rs`. One
//! codebase, one posture for a persisted artefact that a LATER RUN parses —
//! i.e. untrusted input by construction. Every property below is enforced by a
//! named test in `crates/rendercache/tests/cache.rs`:
//!
//! 1. **Versioned** — a four-character magic ([`RENDER_CACHE_MAGIC`]) plus
//!    [`RENDER_CACHE_VERSION`]. Any other stamp, older or newer, invalidates the
//!    entry outright, so two builds sharing one cache directory degrade to
//!    correct-but-slow, never to wrong.
//! 2. **Bounded** — [`MAX_META_FILE_BYTES`] is checked against the meta file's
//!    METADATA before a single byte is read, [`write_meta`] refuses a meta past
//!    that same cap, and [`prune_bytes`] bounds how many BYTES may accumulate.
//! 3. **Temp-then-rename** — every write lands on a uniquely-nonced temp
//!    sibling, is `sync_all`ed, and only then renamed over the final name.
//! 4. **Best-effort** — [`write_meta`] returns `bool` and [`prune_bytes`]
//!    returns nothing. A cache that cannot be written costs one re-render, never
//!    a playback failure.
//! 5. **Never errors, never panics** — [`read_fresh_segment`] returns `Option`,
//!    and every hostile input resolves to `None`. This file contains no
//!    panicking construct outside its comments, enforced by
//!    `no_unwrap_no_panic_in_cache`, a source scan that blanks comments before
//!    looking.
//!
//! # The two-file shape, and what "meta LAST" buys
//!
//! ```text
//! {stem}.seg.mp4     the payload: a real, playable, all-intra MP4
//! {stem}.seg.json    the meta:    magic + version + the FULL segment identity
//!                                 + the geometry + the generation params +
//!                                 payload_bytes
//! ```
//!
//! The meta is written **LAST**, as the commit marker. That single ordering rule
//! is the whole of this cache's partial-write protection (T-59-02-02):
//!
//! * a killed or crashed render leaves at most a payload with no meta, which is
//!   a MISS ([`read_fresh_segment`] stats the meta first) and is swept as an
//!   orphan by [`prune_bytes`];
//! * a meta only ever exists once its payload is complete, because
//!   [`write_meta`] refuses to commit a meta whose payload is absent or whose
//!   length disagrees with `payload_bytes`;
//! * and the meta itself lands atomically, so a reader sees the previous whole
//!   meta or the new whole meta, never a mixture.
//!
//! # One altitude up from the proxy cache
//!
//! The proxy cache's identity is ONE MEDIA FILE's `(path, mtime_ns,
//! size_bytes)`. A segment's identity is a hash of EVERYTHING A PROGRAM-TIME
//! WINDOW COMPOSITES FROM (see [`crate::key`]) — which is why the identity
//! arrives here as a `u64` the caller computed, not as something this module
//! derives. This module's job is to store it, compare it, and refuse anything
//! that does not match.
//!
//! [`read_fresh_segment`]'s `expect_hash` compare IS D-18's entire mechanism: a
//! stale segment is not deleted here, it is simply never a hit. Deletion is the
//! write path's business (a fresh render lands under a different name) and
//! [`prune_bytes`]'s (the byte budget reclaims what nobody asks for any more).
//! There is no "probably still fine" branch anywhere in this file.
//!
//! # Threat model
//!
//! * **T-59-02-01 (a corrupt or mismatched meta)** — properties 1, 2 and 5, plus
//!   [`SegmentMeta::is_sane`]'s geometry/fps clamps, the full identity compare
//!   and the `payload_bytes` cross-check against the MP4's real length.
//! * **T-59-02-02 (a partial file at the final path after a kill or crash)** —
//!   property 3, plus the meta-last commit rule and [`prune_bytes`]'s stale-temp
//!   sweep.
//! * **T-59-02-03 (unbounded disk growth)** — [`prune_bytes`] against
//!   [`MAX_RENDER_CACHE_BYTES`].
//! * **T-59-02-04 (a filename hash collision)** — the identity is stored INSIDE
//!   the meta and compared, so a collision degrades to a MISS. The FNV hash is a
//!   non-cryptographic identity mechanism and must never be treated as a
//!   security boundary.
//! * **T-59-02-05 (path traversal via a crafted media path)** — file names here
//!   are always self-derived `{hex}-k{int}.seg.*`. A raw media path appears only
//!   INSIDE hashed material and inside meta JSON, never in a filesystem name.
//!
//! # What the read path may cost
//!
//! [`read_fresh_segment`] runs on the producer's per-tick lookup path (D-17), so
//! it is **stats plus one bounded file read**, plus — on a HIT only, and at most
//! once per [`LRU_TOUCH_INTERVAL`] per entry — **one mtime write** to record the
//! use for [`prune_bytes`]. It never opens a video and never starts a child
//! process. `the_read_path_starts_no_child_process_and_opens_no_video` pins that
//! at the source level rather than by inspection, and
//! `a_hot_segment_is_not_re_touched_on_every_hit` pins the write half — 58's
//! WR-03 finding, inherited: an unconditional open-for-write on a per-tick path
//! is a per-frame filesystem write on the playback producer thread, which is
//! exactly the class of cost this phase exists to remove.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::key::SEG_US;

/// The directory, under the host's `AppCtx::app_cache_dir()`, that holds the
/// segment pairs (D-25). Same root as `filmstrips/`, `posters/` and `proxies/`.
/// Named here so the writer, the reader and the pruner all use one location
/// rather than each inventing its own.
pub const RENDER_CACHE_DIR_NAME: &str = "rendercache";

/// Bump when the meaning of ANY meta field changes, or when the payload's
/// expected shape does. A bump invalidates every existing segment by
/// construction: the version is in the KEY MATERIAL (so every segment hash, and
/// therefore every file name, changes) AND in the meta (so an old file that
/// somehow reaches a colliding name still misses).
///
/// * **v2** (gap pass, 2026-08): the rate-control MODE changed from a flat
///   `-b:v` to a quality target ([`engine::RENDER_CACHE_ENCODE_QUALITY`]), and
///   the meta's flat-bitrate term became [`SegmentMeta::rate_control`], a
///   policy string. Every v1 segment on disk was produced under a policy this
///   build does not use, so **stranding them is the deliberate purpose of this
///   bump, not a side effect of it.** Plan 59-11 lands before any
///   re-measurement (59-14) for exactly this reason: re-measuring first would
///   produce numbers this bump deletes.
/// * **v3** (the NVENC ladder, 2026-08-04): the segment encoder stopped being
///   one shipped name and became RUNTIME-RESOLVED — `h264_nvenc` where the
///   hardware exists, [`engine::DEFAULT_VIDEO_ENCODER`] where it does not (see
///   [`engine::render_cache_preferred_encoder`]) — and the policy string gained
///   the `constqp:{QP}` shape for the NVENC family. Every v2 segment on disk
///   was produced under a resolution policy this build does not use, so
///   **stranding them is the deliberate purpose of this bump, not a side effect
///   of it.** The encoder-name compare alone would already strand them on an
///   NVIDIA machine; the version bump is what strands them on EVERY machine,
///   including one where the ladder misses and the name happens to be
///   unchanged.
pub const RENDER_CACHE_VERSION: u32 = 3;

/// First field of every meta file. A file that does not carry this is not ours,
/// whatever its name says. (`RRCM` = Rudis Render Cache Meta.)
pub const RENDER_CACHE_MAGIC: &str = "RRCM";

/// Target video bitrate for a cached segment, bits per second — **the DEV
/// override door's `-b:v` floor, and nothing else, since 59-11.**
///
/// This constant used to be the shipped path's quality floor, and its own doc
/// used to say so. It was not one, and `59-CACHE-CALIBRATION` § 2 measured it:
/// 24 Mbps requested, **2.2 Mbps delivered** — 10.9x under — because a flat
/// bitrate is the wrong control for an ALL-INTRA stream. **The shipped floor is
/// now PER-FAMILY**: [`engine::RENDER_CACHE_ENCODE_QUALITY`] on the Media
/// Foundation encoder (a quality target it actually honours, self-scaling with
/// content and fps) and [`engine::RENDER_CACHE_NVENC_QP`] on the preferred
/// NVENC path (constant QP, its own equivalent). Both are quality terms, both
/// are measured, and neither is a bitrate.
///
/// The name and the value are KEPT, deliberately:
///
/// * `engine::RenderCacheEncoder::new`'s signature is unchanged, and
///   `generate.rs` still passes this value into it, so the non-MF branch
///   (reachable only through the loud `DEV_ENCODER_OVERRIDE_ENV` door) still has
///   the explicit bitrate `h264_videotoolbox` cannot open without;
/// * the value remains the right one for that door for the reason
///   `59-CEILING-VERDICT.md` § 4 gives — a real high-entropy 1080p all-intra
///   encode measured **25.24 Mbps** there, and its low-entropy arm 2.38 Mbps, so
///   24 Mbps is a cap real content approaches from below rather than a number
///   anything is padded up to.
///
/// A cache segment still needs a floor a proxy does not (D-05/D-06): a proxy may
/// look softer than its original, a cache segment may NOT — CACHE-02 forbids a
/// visible discontinuity at the boundary where playback crosses from cached
/// pixels to live ones. That requirement did not change; only the instrument
/// that meets it did.
pub const SEGMENT_BITRATE_BPS: u64 = 24_000_000;

/// The total-bytes budget for the whole `rendercache` directory (D-25).
///
/// **The byte math this number is SIZED against — an upper bound, not a
/// prediction.** A 1920x1080 all-intra `h264_mf` segment at
/// [`SEGMENT_BITRATE_BPS`] would cost about `24_000_000 / 8` = **3 MB per
/// second** of program time, and `59-CEILING-VERDICT.md` § 4 recorded
/// **3.16 MB per media-second** on its high-entropy arm. At
/// [`crate::key::SEG_US`] = 2 s that is **~6 MB per segment**, so 8 GiB holds
/// roughly **44 minutes** of cached program time (8 589 934 592 B /
/// 3 145 728 B per s ≈ 2 730 s).
///
/// Since 59-11 the shipped encode is quality-targeted rather than
/// bitrate-targeted, and since the NVENC ladder it is quality-targeted in one
/// of TWO per-family spellings ([`engine::RENDER_CACHE_ENCODE_QUALITY`] on MF,
/// [`engine::RENDER_CACHE_NVENC_QP`] on NVENC), so the arithmetic above is
/// explicitly an **upper-bound sizing anchor**: the measured rate is
/// content-dependent (`59-CACHE-CALIBRATION` § 3 recorded 275 kB/s on synthetic
/// composite against the 3.15 MB/s worst case — an order of magnitude of slack,
/// in the safe direction). The NVENC constant's own calibration ladder re-checks
/// the byte anchor per arm and the selection rule refuses any arm above
/// 3.15 MB per media-second, so the anchor constrains the constant rather than
/// merely describing it. 59-14 re-takes the budget arithmetic on the shipped
/// policy; until it does, this bound is known-conservative rather than known-
/// exact. Either way it is far more than any one editing session revisits, and
/// small enough that a laptop SSD never notices.
///
/// Twice `proxy::cache::MAX_PROXY_CACHE_BYTES`'s 4 GiB, deliberately: a proxy
/// is a DOWNSCALED stand-in for one file, a cache segment is a full-resolution
/// render of a whole composite, so the same disk buys much less program time
/// here. An entry-COUNT cap would be the wrong bound for the same reason it was
/// wrong for proxies — entries are not uniform.
///
/// This is a **FIXED** constant. Free-disk-space awareness (shrinking the budget
/// on a nearly-full volume) is explicitly **not** this phase's job (D-25,
/// inheriting 58 D-30) and is recorded as a deferred item rather than smuggled
/// in.
///
/// Dropping a segment costs exactly one re-render, never correctness.
pub const MAX_RENDER_CACHE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Refuse to hand an unbounded byte count to the JSON parser. A real meta is a
/// few hundred bytes; 64 KiB is enormous slack. Checked against the file's
/// METADATA before any read, in that order (T-53.2-06's discipline, inherited
/// through `proxy::cache`).
pub const MAX_META_FILE_BYTES: u64 = 64 * 1024;

/// Suffix of the payload file — a real, playable, all-intra MP4.
pub const SEGMENT_PAYLOAD_SUFFIX: &str = ".seg.mp4";

/// Suffix of the meta file — the commit marker.
pub const SEGMENT_META_SUFFIX: &str = ".seg.json";

/// Substring shared by both members of a pair, and by every temp sibling this
/// module writes. Only files carrying it are ever deleted by [`prune_bytes`], so
/// a foreign file that happens to live in the cache directory is skipped rather
/// than destroyed (filmstrip's "only its own extension" rule).
const SEGMENT_INFIX: &str = ".seg.";

/// Marker embedded in every in-flight temp sibling.
const TMP_MARKER: &str = ".tmp.";

/// Geometry sanity clamps. A meta violating ANY of them is a MISS, never a
/// crash and never a partially-trusted read (T-59-02-01). A cache segment is
/// rendered at the PROJECT canvas, so these are the canvas's own plausible
/// bounds; the exact canvas is compared separately, and these exist to reject a
/// meta that is nonsense before it is compared to anything.
pub const MIN_SEGMENT_DIM: u32 = 2;
/// See [`MIN_SEGMENT_DIM`].
pub const MAX_SEGMENT_DIM: u32 = 8192;

/// How long a temp sibling must have sat untouched before [`prune_bytes`]
/// treats it as an app-killed-mid-render leftover rather than as an in-flight
/// write belonging to a live writer.
pub const STALE_TMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How far behind reality a HIT's recorded use may drift before
/// [`read_fresh_segment`] pays a write to correct it.
///
/// 58-REVIEW **WR-03**, inherited whole and for a stronger reason: the proxy
/// cache's read path runs once per pooled layer per composited tick; THIS read
/// path runs once per composited tick, on the playback producer thread, on a
/// volume that may be network-backed or AV-scanned. An unconditional
/// `set_modified` there is a per-frame open-for-write.
///
/// The eviction order [`prune_bytes`] needs is a RANKING, not a timestamp: two
/// segments used within the same minute are interchangeable to it, and a segment
/// used within the last minute already sorts ahead of every segment that was
/// not. So quantizing "recently used" to this interval loses nothing the bound
/// actually consumes.
pub const LRU_TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// Monotonic per-write nonce so a temp file is unique per call — mirrors
/// `proxy::cache`'s `TMP_COUNTER` and its stated reason: two concurrent writers
/// must never share a `.tmp` handle and interleave-corrupt it before the rename.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Everything about a cached segment except its pixels. Written LAST, as the
/// commit marker for the `{stem}.seg.mp4` beside it.
///
/// The FULL identity lives here, not merely hashed into the name: the name
/// carries a 64-bit FNV hash, and a collision must degrade to a MISS rather than
/// to a confidently-served WRONG segment (T-59-02-04).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub magic: String,
    pub version: u32,
    /// Which segment on the global grid this file holds.
    pub seg_index: i64,
    /// The grid pitch this file was cut on. Stored as well as hashed, so a
    /// build with a different [`crate::key::SEG_US`] cannot read this file even
    /// if it somehow reached the right name.
    pub seg_us: i64,
    /// The segment identity — [`crate::key::segment_hash`]'s answer for the
    /// arrangement this file was rendered from.
    pub hash: u64,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub fps: f64,
    /// The encoder that actually produced the payload. In the meta AND compared
    /// on read, so a DEV-override segment can never masquerade as a
    /// shipped-encoder one (D-05, inheriting 58 D-03).
    pub encoder: String,
    /// The rate-control POLICY the payload was produced under — see
    /// [`effective_rate_control`]. Replaced the flat `bitrate_bps: u64` at
    /// [`RENDER_CACHE_VERSION`] 2, because the shipped path no longer emits a
    /// bitrate at all, and because the thing that has to be compared is the
    /// whole policy (mode, quality value, and which measurement doors were open)
    /// rather than one number of it.
    pub rate_control: String,
    /// The payload's exact length on disk. Cross-checked at read time, which is
    /// what turns a truncated killed-mid-render file into a MISS.
    pub payload_bytes: u64,
}

impl SegmentMeta {
    /// A meta describing a segment rendered by THIS build, under THIS build's
    /// policy. The render side (plan 59-04) is the only production caller.
    pub fn new(
        seg_index: i64,
        hash: u64,
        canvas_w: u32,
        canvas_h: u32,
        fps: f64,
        payload_bytes: u64,
    ) -> SegmentMeta {
        SegmentMeta {
            magic: RENDER_CACHE_MAGIC.to_string(),
            version: RENDER_CACHE_VERSION,
            seg_index,
            seg_us: SEG_US,
            hash,
            canvas_w,
            canvas_h,
            fps,
            encoder: effective_encoder(),
            rate_control: effective_rate_control(),
            payload_bytes,
        }
    }

    /// Every clamp that can be judged WITHOUT knowing what the caller asked
    /// for, in one place, so [`read_fresh_segment`] and [`write_meta`] cannot
    /// drift: a meta this rejects is never written, and never trusted if it
    /// appears on disk anyway.
    ///
    /// The IDENTITY (`hash`, `seg_index`, canvas, fps) is deliberately not
    /// checked here — it can only be judged against the segment the caller is
    /// asking about.
    pub fn is_sane(&self) -> bool {
        self.magic == RENDER_CACHE_MAGIC
            && self.version == RENDER_CACHE_VERSION
            && self.seg_us == SEG_US
            && self.rate_control == effective_rate_control()
            && self.encoder == effective_encoder()
            && dims_are_sane(self.canvas_w, self.canvas_h)
            && fps_is_sane(self.fps)
            && self.payload_bytes > 0
    }
}

/// What a cache HIT hands back: where the playable segment is, and the geometry
/// and cadence the caller should expect out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentHit {
    pub path: PathBuf,
    pub w: u32,
    pub h: u32,
    pub fps: f64,
}

fn dims_are_sane(w: u32, h: u32) -> bool {
    w % 2 == 0
        && h % 2 == 0
        && (MIN_SEGMENT_DIM..=MAX_SEGMENT_DIM).contains(&w)
        && (MIN_SEGMENT_DIM..=MAX_SEGMENT_DIM).contains(&h)
}

/// The project-fps ceiling `Command::SetProjectSettings` already enforces,
/// reused rather than re-invented: the cache must not accept a cadence the
/// domain model itself would reject.
fn fps_is_sane(fps: f64) -> bool {
    fps.is_finite() && fps > 0.0 && fps <= rudis_core::MAX_TIMEBASE_FPS
}

/// The encoder identity that goes into the meta and is compared on read.
///
/// Resolved EXACTLY as the engine's encode entry point resolves it, so the meta
/// always describes the encoder that actually ran: the DEV-only override if it
/// is set to a non-blank value, otherwise the engine's ONE preference ladder
/// answer, otherwise the cleared, shipped `DEFAULT_VIDEO_ENCODER`.
///
/// # Why this DELEGATES rather than re-deriving (the one-ladder property)
///
/// `engine::render_cache_preferred_encoder()` is the SAME memoized answer
/// `engine::RenderCacheEncoder::new` resolves through — that function is the
/// writer, this one is the reader, and they are two halves of one compare.
/// `generate.rs`'s `finish` stamps the encoder that **actually ran**, taken off
/// the child rather than re-resolved at commit time, and `write_meta` refuses a
/// commit whose meta this build could not read back. So a forked prediction
/// here would not mislabel segments — it would make EVERY segment commit fail
/// closed, and the cache would silently never fill. Delegation is the only
/// correct shape, and `tests/encoder_license.rs` pins it as source text (one
/// call site, zero hand-copied probe logic).
///
/// The ladder itself is memoized in the engine, so this stays cheap on the
/// per-tick read path (cost class recorded at 59-REVIEW IN-01) even though the
/// underlying question is answered by spawning `ffmpeg -encoders` once.
///
/// **No warning is printed here.** This is the READ path, called per tick; the
/// loud, unmissable override warning lives on the render path where the choice
/// is actually made (plan 59-04). Storing the encoder in the meta and comparing
/// it is what makes a DEV-override segment structurally unable to masquerade as
/// a shipped-encoder segment (D-05, inheriting 58 D-03): the two fail each
/// other's stored-params compare.
pub fn effective_encoder() -> String {
    match std::env::var(engine::DEV_ENCODER_OVERRIDE_ENV) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => engine::render_cache_preferred_encoder()
            .unwrap_or(engine::DEFAULT_VIDEO_ENCODER)
            .to_string(),
    }
}

/// The rate-control POLICY identity that goes into the meta and is compared on
/// read — the 59-11 sibling of [`effective_encoder`], and the reason the
/// rate-control mode change is a designed invalidation rather than a discovered
/// one.
///
/// **ONE builder, called by BOTH sides.** [`SegmentMeta::new`] (write) and
/// [`SegmentMeta::is_sane`] (read) call this same function, so a writer and a
/// reader structurally cannot disagree about what policy produced a payload.
/// Two spellings that happen to agree today is exactly the drift this shape
/// exists to prevent, and `tests/encoder_license.rs` pins the single-call-site
/// property as source text.
///
/// The string names every door that can change how the bits were spent:
///
/// | policy | produced by |
/// |---|---|
/// | `constqp:{QP}` | the PREFERRED path where the hardware exists — constant-QP on NVENC ([`engine::RENDER_CACHE_NVENC_QP`]). A different MODE, not a different number: NVENC cannot take the MF-private options at all |
/// | `quality:{Q}:sw` | the FALLBACK path — quality-targeted on the Media Foundation encoder, whose selected MFT is Windows' SOFTWARE `H264 Encoder MFT` |
/// | `quality:{Q}:hw` | the [`engine::RENDER_CACHE_HW_ENCODING_ENV`] measurement door |
/// | `quality:{Q}:{sw\|hw}+{scenario}` | the [`engine::RENDER_CACHE_SCENARIO_ENV`] measurement door |
/// | `bitrate:{SEGMENT_BITRATE_BPS}` | the loud DEV encoder override onto a FOREIGN encoder — it can take neither family's private options, so its rate control really is a flat `-b:v` |
///
/// **The two measurement doors do NOT stamp the NVENC branch, and could not
/// honestly do so.** `-hw_encoding` and `-scenario` are `mfenc`-private options
/// that `engine::render_cache_encode_args` emits only on the MF branch; they
/// are structurally unable to reach an NVENC command line (an unknown private
/// option is a hard spawn failure, which is why the branch is a discriminator
/// rather than a fall-through). A policy string that mentioned them on an NVENC
/// segment would be describing an encode that never happened.
///
/// That is 58 D-03's discipline extended from the encoder NAME to the encode
/// POLICY: a segment produced under a measurement door for 59-14's arm matrix
/// can never be served by a shipped-policy build, because the two fail each
/// other's stored-params compare.
///
/// **No warning is printed here**, for the same reason [`effective_encoder`]
/// prints none: this is the READ path, called per tick. The loud warnings live
/// where the choice is made (`engine::render_cache_scenario` for a refused door
/// value, `resolve_cleared_encoder` for the encoder override).
///
/// **Per-call env reads, deliberately not memoized.** The cost class is already
/// recorded (59-REVIEW IN-01, for `effective_encoder`), and a `OnceLock` here
/// would let the first test to run decide the answer for every later one — the
/// branch tests set and clear these variables mid-process.
pub fn effective_rate_control() -> String {
    // ONE encoder answer, shared by all three branches — asking twice would be
    // the same fork this whole design refuses.
    let encoder = effective_encoder();
    if engine::is_mf_video_encoder(&encoder) {
        format!(
            "quality:{}:{}{}",
            engine::RENDER_CACHE_ENCODE_QUALITY,
            if engine::render_cache_hw_encoding_enabled() {
                "hw"
            } else {
                "sw"
            },
            engine::render_cache_scenario()
                .map(|s| format!("+{s}"))
                .unwrap_or_default()
        )
    } else if engine::is_nvenc_video_encoder(&encoder) {
        format!("constqp:{}", engine::RENDER_CACHE_NVENC_QP)
    } else {
        format!("bitrate:{SEGMENT_BITRATE_BPS}")
    }
}

/// The filesystem-safe, deterministic file stem for one segment.
///
/// `{hash:016x}-k{seg_index}`. The hash already encodes every generation
/// parameter and the cache version, because they are all in the key material —
/// so a policy change invalidates BY CONSTRUCTION rather than by anyone
/// remembering to bump something. `seg_index` is appended anyway: it costs
/// nothing, and it makes a cache directory legible to a human debugging an
/// eviction or a miss.
///
/// A raw media path never reaches a file name (T-59-02-05); the name is
/// self-derived from a hex integer and a decimal integer, and nothing else.
pub fn file_stem_for(hash: u64, seg_index: i64) -> String {
    format!("{hash:016x}-k{seg_index}")
}

/// `{stem}.seg.mp4` — the playable payload.
pub fn payload_file_name(stem: &str) -> String {
    format!("{stem}{SEGMENT_PAYLOAD_SUFFIX}")
}

/// `{stem}.seg.json` — the meta, and the commit marker.
pub fn meta_file_name(stem: &str) -> String {
    format!("{stem}{SEGMENT_META_SUFFIX}")
}

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

/// Is there a usable, CURRENT cache file for segment `seg_index` under identity
/// `expect_hash`, rendered at exactly `canvas_w` x `canvas_h` at `fps`?
///
/// `None` covers absent, stale, corrupt, oversized, truncated, wrong-version,
/// wrong-geometry, wrong-encoder and never-written alike — the caller cannot
/// distinguish them and must not try. **D-21: the fallback is total, silent and
/// PER SEGMENT.** Playback never fails because a cache is bad, and one bad
/// segment never affects another: this function is called once per segment with
/// no shared mutable state, so a corrupt segment `k` is invisible to segment
/// `k+1`.
///
/// Deliberately `Option`, never `Result` — a `Result` on the per-tick lookup
/// path would be an error the producer has to handle, and there is nothing to
/// handle: the answer to every failure is the same, and it is "composite live".
///
/// Cost: two `metadata` calls plus one bounded file read, plus — on a HIT, and
/// at most once per [`LRU_TOUCH_INTERVAL`] per entry — one mtime write. No video
/// is opened and no child process is started.
///
/// The order of checks is deliberate: the meta's METADATA length is settled
/// BEFORE any allocation, so a hostile file cannot cost memory merely by
/// existing (T-59-02-01).
pub fn read_fresh_segment(
    dir: &Path,
    seg_index: i64,
    expect_hash: u64,
    canvas_w: u32,
    canvas_h: u32,
    fps: f64,
) -> Option<SegmentHit> {
    let stem = file_stem_for(expect_hash, seg_index);

    let meta_path = dir.join(meta_file_name(&stem));
    let meta_stat = std::fs::metadata(&meta_path).ok()?;
    if !meta_stat.is_file() {
        return None;
    }
    // The bound is settled on the METADATA, before a single byte is read, so a
    // hostile file cannot cost memory merely by existing.
    if meta_stat.len() > MAX_META_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&meta_path).ok()?;
    let meta: SegmentMeta = serde_json::from_slice(&bytes).ok()?;

    // The RRCM magic, the version, the grid pitch, the generation params and
    // the geometry/fps clamps — all of them, in one place shared with the write
    // path so the two can never drift.
    if !meta.is_sane() {
        return None;
    }
    // The name is a hash; the IDENTITY is the truth. A collision misses here,
    // and so does a stale segment — D-18 in one comparison.
    if meta.hash != expect_hash || meta.seg_index != seg_index {
        return None;
    }
    // Geometry and cadence must be what the CALLER is compositing at. A segment
    // rendered for a different canvas would be a visible discontinuity, and a
    // different fps would break D-07's identity mapping.
    if meta.canvas_w != canvas_w || meta.canvas_h != canvas_h {
        return None;
    }
    // Bit compare, not an epsilon: `is_sane` has already rejected non-finite
    // values, and serde_json round-trips an f64 exactly, so two cadences that
    // are not the same bits are not the same cadence.
    if meta.fps.to_bits() != fps.to_bits() {
        return None;
    }

    // The payload must exist and be EXACTLY as long as the meta committed it.
    // A killed-mid-render file is shorter; a re-used name is different.
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

    Some(SegmentHit {
        path: payload_path,
        w: meta.canvas_w,
        h: meta.canvas_h,
        fps: meta.fps,
    })
}

/// Commit the segment at `{stem}.seg.mp4` by publishing its meta. Returns
/// whether it landed.
///
/// **Call this LAST**, after the payload is complete on disk: it is the commit
/// marker, and it refuses to commit a payload that is absent or whose length
/// disagrees with `meta.payload_bytes`. That refusal is what makes the meta-last
/// rule an enforced invariant rather than a documented convention
/// (T-59-02-02).
///
/// It also refuses at write time exactly what [`read_fresh_segment`] refuses at
/// read time: a meta that would MISS forever is disk burned for nothing.
///
/// The meta itself lands on a uniquely-nonced temp sibling, is `sync_all`ed, and
/// only then renamed over the final name — so a concurrent reader observes the
/// previous whole meta or the new whole meta, never a mixture.
///
/// BEST-EFFORT: any failure is reported on stderr and returned as `false`.
/// Nothing here is an `Err`, and nothing here panics — losing the cache costs
/// one re-render, never a playback failure.
pub fn write_meta(dir: &Path, stem: &str, meta: &SegmentMeta) -> bool {
    if !meta.is_sane() {
        eprintln!(
            "render cache: refusing to persist an unreadable meta for segment {} \
             ({}x{} @ {}, {} bytes, encoder {}, v{}) — it could never be read back",
            meta.seg_index,
            meta.canvas_w,
            meta.canvas_h,
            meta.fps,
            meta.payload_bytes,
            meta.encoder,
            meta.version
        );
        return false;
    }

    // The commit marker may not commit a payload that is not there. THIS is
    // what makes "meta LAST" an enforced invariant rather than a documented
    // convention.
    let payload_path = dir.join(payload_file_name(stem));
    match std::fs::metadata(&payload_path) {
        Ok(stat) if stat.is_file() && stat.len() == meta.payload_bytes => {}
        Ok(stat) => {
            eprintln!(
                "render cache: refusing to commit {} — the payload is {} bytes, the \
                 meta claims {}",
                payload_path.display(),
                stat.len(),
                meta.payload_bytes
            );
            return false;
        }
        Err(e) => {
            eprintln!(
                "render cache: refusing to commit {} — its payload is unreadable: {e}",
                payload_path.display()
            );
            return false;
        }
    }

    let Ok(bytes) = serde_json::to_vec(meta) else {
        eprintln!(
            "render cache: meta for segment {} would not serialize",
            meta.seg_index
        );
        return false;
    };
    if bytes.len() as u64 > MAX_META_FILE_BYTES {
        eprintln!(
            "render cache: refusing a {}-byte meta for segment {} — past the \
             {MAX_META_FILE_BYTES}-byte cap the reader enforces",
            bytes.len(),
            meta.seg_index
        );
        return false;
    }

    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("render cache: cannot create {}: {e}", dir.display());
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
            // to ask for it (filmstrip's discipline, inherited through proxy).
            prune_bytes(dir, MAX_RENDER_CACHE_BYTES);
            true
        }
        Err(e) => {
            eprintln!(
                "render cache: write failed for {}: {e}",
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
/// # Why a byte budget and not an entry count (D-25)
///
/// Most caches in this codebase bound ENTRY COUNT, because their entries are
/// small and roughly uniform. Cache segments are neither uniform across
/// projects nor small: a 2 s segment of a static title costs a few hundred kB
/// and a 2 s segment of a six-layer 4K stack costs several MB, so `N` entries
/// could mean 200 MB or 20 GB. Only a byte total actually bounds the disk. The
/// eviction SHAPE below is `proxy::cache::prune_bytes`'s, copied deliberately —
/// sort newest-first, tie-break for determinism, best-effort, never panic.
///
/// # The rules, in order
///
/// 1. **Pairs.** A `{stem}.seg.mp4` and `{stem}.seg.json` together are one
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
/// 4. **Sweep.** An mp4 with no meta (a killed-mid-render leftover), a meta with
///    no mp4 (unusable, a permanent MISS), and any temp sibling older than
///    [`STALE_TMP_AGE`] all go. A FRESH temp sibling belongs to a live writer
///    and is left strictly alone.
/// 5. **Only our own files.** A name must carry [`SEGMENT_INFIX`] to be
///    considered at all, so a foreign file — including another cache's, since
///    `rendercache/` shares a root with `proxies/` and `filmstrips/` — is never
///    touched. Sub-directories are not walked.
///
/// BEST-EFFORT: every filesystem error is swallowed and nothing is returned.
/// Failing to prune costs disk, never correctness, so it must never become a
/// playback or render failure.
///
/// **Known edge, recorded rather than hidden:** a single pair larger than the
/// entire budget is evicted immediately, because rule 3 is a hard bound. At
/// [`MAX_RENDER_CACHE_BYTES`] that would take a single segment over ~8 GiB,
/// which at the [`SEGMENT_BITRATE_BPS`]-shaped UPPER BOUND (~3 MB per media
/// second — the shipped path is quality-targeted since 59-11, so the real rate
/// is content-dependent and measured lower) means a `SEG_US` of roughly 45
/// minutes — unreachable by three orders of magnitude from D-10's 2 s. The
/// bound being conservative only makes this edge further away. Recorded for the
/// same reason the proxy cache records its version of it: the edge exists, and
/// the answer if it is ever reached is to raise the budget.
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
        if !name.contains(SEGMENT_INFIX) {
            continue;
        }
        let Ok(stat) = entry.metadata() else {
            continue;
        };
        if !stat.is_file() {
            continue;
        }
        // An unreadable timestamp sorts as "oldest", so such a file is dropped
        // before any file whose age is actually known.
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

        let classified = match name.strip_suffix(SEGMENT_PAYLOAD_SUFFIX) {
            Some(stem) => Some((stem.to_string(), true)),
            None => name
                .strip_suffix(SEGMENT_META_SUFFIX)
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
