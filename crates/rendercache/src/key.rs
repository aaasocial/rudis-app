//! Segment identity — the fixed program-time grid, and the hash of everything a
//! segment composites from (Phase 59, CACHE-03; decisions D-07, D-09, D-10,
//! D-14, D-15, D-17, D-37).
//!
//! # The rule this module exists to obey (D-37)
//!
//! **The key material is enumerated from the TYPES, not from D-15's prose.**
//! D-15's bullet list is the *intent*; `rudis_core::Clip` is the *truth*.
//! [`ClipKeyMaterial::from_clip`] therefore destructures the whole `Clip`
//! exhaustively and classifies every single field as INCLUDED or EXCLUDED —
//! which means the day a future phase adds a pixel-affecting field to `Clip`,
//! this file stops compiling until someone decides which side it falls on. That
//! compile break is the mechanism. A prose list cannot fail a build, and D-15's
//! prose list has already been wrong once: it named "track enable/mute" (a field
//! that does not exist) and omitted `Clip.keyframes` (a field that changes every
//! animated pixel).
//!
//! # Bit-stability of the material string
//!
//! Float fields go in as `to_bits()` hex, never as a formatted decimal, so the
//! material is bit-exact and two distinct floats can never collapse onto one
//! string. Composite sub-structures whose contents are pixel-affecting *in their
//! entirety* (`keyframes`, `text`, `retime`) go in as `serde_json` of THAT FIELD
//! ALONE — never a whole-`Clip` serialize, which would silently re-admit the
//! audio fields D-08 excludes (research Pitfall 5's over-inclusion trap).
//!
//! `serde_json` renders floats with a shortest-round-trip algorithm, so two
//! *different* values always produce two different strings — which is the
//! property the hash needs. It is not a guarantee that the *same* value renders
//! identically across serde versions forever; if that ever changed, the effect
//! would be a one-time cache miss (correct, slower), never a wrong frame, and
//! [`crate::cache::RENDER_CACHE_VERSION`] is the deliberate lever for it.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::cache::RENDER_CACHE_VERSION;

/// Deterministic JSON for ONE pixel-affecting composite field.
///
/// A field that will not serialize is replaced by a fixed sentinel rather than
/// by a panic: this runs on the producer's lookup path, and D-21 forbids a
/// playback failure caused by a cache. The sentinel is CONSTANT, which means
/// two unserializable values would hash alike — that degrades to serving one
/// segment's pixels for another's, so the sentinel is deliberately something a
/// grep will find if it ever appears in a material dump. No `Clip` field
/// reachable today can produce it (`KeyframeTracks`, `TextPayload` and `Retime`
/// are all plain derived `Serialize`).
fn json_material<T: Serialize>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(s) => s,
        Err(_) => "<RENDERCACHE-UNSERIALIZABLE>".to_string(),
    }
}

/// The fixed segment length in microseconds — segment `k` covers
/// `[k*SEG_US, (k+1)*SEG_US)` on a global grid anchored at program time zero
/// (D-09).
///
/// # Why this number, and what moves it (D-10)
///
/// The trade is stated rather than assumed. **Longer** segments amortize the
/// encoder's startup and the decode session's cold open over more program time,
/// and make fewer, bigger files. **Shorter** segments make invalidation
/// finer-grained (an edit dirties less cached work) and make the cache useful
/// sooner (the first segment lands earlier). ~2 s is the starting point
/// 59-CONTEXT names; 59-10's calibration artifact records the measured
/// judgement that keeps or moves it.
///
/// The binding constraint is NOT decode throughput. `59-CEILING-VERDICT.md`
/// § 4 measured a software all-intra 1080p segment decoding at ~210 fps against
/// a 30 fps demand (6.99x headroom on the hard arm) but **~180 ms of cold
/// session entry** (`session_open_ms ~75` + `first_pull_ms ~97-105`). Segment
/// length must be calibrated against that entry cost, which is why the number
/// lives here as a named constant with a rationale instead of inline at a call
/// site.
///
/// `SEG_US` is IN the hash material ([`SegmentKeyInputs::material`]), so
/// changing it invalidates every existing segment by construction rather than
/// by anyone remembering to bump the version.
///
/// **Deliberate consequence, recorded:** the grid is anchored in TIME, not in frames.
/// At 30 fps a segment holds 60 frames and 60 * 33 333 us = 1 999 980 us
/// — a segment boundary is NOT generally a frame boundary. That is fine and is
/// pinned by a test: what D-07 requires is that program time maps to
/// cache-file time by IDENTITY (see [`segment_frame_program_us`]), not that the
/// grid divides evenly by any particular cadence.
pub const SEG_US: i64 = 2_000_000;

/// Which segment a program-time position falls in.
///
/// `div_euclid`, NOT `/`: truncating division sends both `-1` and `+1` to
/// segment `0`, which would let a negative position alias onto segment zero's
/// cached pixels. Negative program time is not reachable through the transport
/// today, but a grid function that is wrong for half its domain is a trap for
/// the next caller, not a saved instruction.
pub fn segment_index_for(pos_us: i64) -> i64 {
    pos_us.div_euclid(SEG_US)
}

/// Segment `k`'s half-open program-time bounds, `[start, end)`.
///
/// Saturating throughout: a nonsense `k` yields a clamped range rather than an
/// overflow panic, because this is called from the producer's per-tick path
/// where a debug-build panic would be a playback failure caused by a *cache*
/// (D-21 forbids exactly that).
pub fn segment_bounds(k: i64) -> (i64, i64) {
    let start = k.saturating_mul(SEG_US);
    let end = k.saturating_add(1).saturating_mul(SEG_US);
    (start, end)
}

/// **D-07's identity mapping, as a function rather than as a comment.**
///
/// Program time of frame `frame_in_segment` of segment `seg_index` at project
/// `fps` — which is just `k*SEG_US + i*frame_step`. Same fps, same timebase,
/// same duration; only the container changes. There is no offset term, no
/// rounding correction and no drift accumulator here, and there must never be
/// one: the whole point of D-07 is to delete an entire class of A/V-drift and
/// stamp-desync bugs before it can exist, exactly as Phase 58 D-04 made proxy
/// `source_us` an identity.
///
/// The step comes from [`engine::frame_step_us`] rather than from a local
/// `1_000_000.0 / fps`, so the cache can never disagree with the engine about
/// what a frame is worth. (`rudis_core::frame_step_us` is that function's
/// documented pure-crate twin; a test in `tests/identity_mapping.rs` pins the
/// two together so twin drift is caught here rather than in a drift gate.)
pub fn segment_frame_program_us(seg_index: i64, frame_in_segment: i64, fps: f64) -> i64 {
    seg_index
        .saturating_mul(SEG_US)
        .saturating_add(frame_in_segment.saturating_mul(engine::frame_step_us(fps)))
}

/// One media file's identity, computed by the CALLER the same way
/// `proxy::cache::key_for` computes it: canonical path + `mtime_ns` +
/// `size_bytes`.
///
/// `mtime_ns`, NOT `mtime_ms` — the reasoning `probe_cache`, `waveform`,
/// `filmstrip` and `proxy` all state: Windows file timestamps advance in coarse
/// (~15.6 ms) steps, so a millisecond-truncated stamp can compare EQUAL across
/// two genuinely different writes.
///
/// It arrives as plain data because this crate is a LEAF (D-39) and must not
/// stat the filesystem on behalf of a domain object it cannot see.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaIdentity {
    pub canonical_path: String,
    pub mtime_ns: u64,
    pub size_bytes: u64,
}

impl MediaIdentity {
    /// The identity of a clip that has NO media file — a TEXT clip, whose
    /// `media_id` is the empty sentinel and which rasterizes its own payload
    /// instead of decoding (`rudis_core::Clip::text`). Distinct from "we failed
    /// to stat it": a caller that cannot stat a real media file must not cache
    /// the segment at all, because it cannot prove the file is unchanged.
    pub fn absent() -> MediaIdentity {
        MediaIdentity {
            canonical_path: String::new(),
            mtime_ns: 0,
            size_bytes: 0,
        }
    }
}

/// The resolved decode-source answer for one clip, as PLAIN DATA (D-14).
///
/// `crates/preview/src/decode_source.rs` owns the knowledge of what a proxy is
/// and answers "what media should this clip's session open?". That answer is
/// part of the segment's identity, because a cached range rendered from
/// originals while the surrounding live playback decodes proxies would be a
/// visible sharpness discontinuity at the boundary — precisely what CACHE-02
/// forbids. So: **a proxy landing, or being evicted, changes this tag and
/// therefore invalidates exactly the segments that clip appears in.**
///
/// # Why `(kind, w, h)` and not the proxy's own cache key (RQ5)
///
/// `DecodeSourceKind::Proxy` deliberately does not carry the proxy's own cache
/// key. Hashing `(kind, w, h)` is coarser than D-15's "resolved decode-source
/// kind AND proxy key" wording, and it is NOT a correctness gap: a proxy
/// regenerated from a CHANGED source also changes that clip's own
/// [`MediaIdentity`] term, which is already in the material. The only thing the
/// coarser tag under-distinguishes is two DIFFERENT proxies that share
/// dimensions for the SAME unchanged source — which cannot happen, because the
/// proxy policy is one proxy per source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DecodeAnswerTag {
    /// The clip decodes its original media.
    Original,
    /// The clip decodes a proxy of the stated dimensions.
    Proxy { w: u32, h: u32 },
}

impl DecodeAnswerTag {
    fn material(&self) -> String {
        match self {
            DecodeAnswerTag::Original => "orig".to_string(),
            DecodeAnswerTag::Proxy { w, h } => format!("proxy:{w}x{h}"),
        }
    }
}

/// One clip's contribution to a segment's identity — an opaque, canonical
/// material string.
///
/// Deliberately opaque and deliberately constructible ONLY through
/// [`ClipKeyMaterial::from_clip`]: a public field would let a caller hand-build
/// material that skipped the exhaustive destructure, which is the one thing
/// keeping this material honest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClipKeyMaterial {
    material: String,
}

impl ClipKeyMaterial {
    /// Build one clip's key material by EXPLICIT ALLOWLIST, field by field.
    ///
    /// **Never `serde_json::to_string(&clip)` and never a derived `Hash` on
    /// `Clip`.** Both would sweep in `volume` and `audio_detached`, and a pure
    /// volume edit — which cannot change a single pixel — would then invalidate
    /// and re-render the segment, burning exactly the background-render budget
    /// D-23 exists to protect (research Pitfall 5).
    ///
    /// `track_index` and `clip_index` are in the material because the ORDERED
    /// walk is the z-order term: the compositor's contract is index-0-top,
    /// track-ordered, so moving a clip between tracks changes its pixels without
    /// changing any of its own fields.
    pub fn from_clip(
        clip: &rudis_core::Clip,
        track_index: usize,
        clip_index: usize,
        media: MediaIdentity,
        decode: DecodeAnswerTag,
    ) -> ClipKeyMaterial {
        // ===================================================================
        // THE D-37 TRIPWIRE. This destructure is EXHAUSTIVE on purpose.
        //
        // Do not add `..` to this pattern, and do not silence a future compile
        // error by binding a new field to `_` without deciding, in writing,
        // which of the two lists below it belongs to. The compile break IS the
        // mechanism that keeps the key material honest — it is the only thing
        // in this codebase that can notice a new pixel-affecting field.
        // ===================================================================
        let rudis_core::Clip {
            // ---- EXCLUDED, each classified rather than silently dropped ----
            //
            // `id`: a stable string identity, not a pixel. Split/duplicate/undo
            // all mint new ids for clips that composite identically; hashing it
            // would invalidate segments for arrangements that produce the exact
            // same frame.
            id: _id,
            // `media_id`: a MediaBin key, not the media. What actually decodes
            // is the file behind it, and that arrives as `media` below —
            // canonical path + mtime + size. Re-importing the same file under a
            // new bin id must not invalidate a segment; a file that CHANGED
            // must, and the media identity term is what catches it.
            media_id: _media_id,
            // `volume`: audio gain. D-08 makes this cache video-only, so a
            // volume edit — including `SetClipMuted`, which is just
            // `volume = 0.0` — cannot change one pixel. Research Pitfall 5.
            volume: _volume,
            // `audio_detached`: DetachAudio moves audio to its own track and
            // leaves every pixel exactly where it was.
            audio_detached: _audio_detached,

            // ---- INCLUDED: every field below changes output pixels ----
            start_us,
            in_us,
            out_us,
            transform,
            opacity,
            crop,
            keyframes,
            text,
            alpha_mode,
            retime,
        } = clip;

        let mut m = String::with_capacity(512);
        // `write!` into a `String` is infallible; the results are discarded
        // rather than unwrapped so this file stays free of panicking
        // constructs.
        let _ = write!(m, "t{track_index}.c{clip_index}");
        let _ = write!(m, "|start={start_us}|in={in_us}|out={out_us}");
        // Floats go in as raw bits, never as a formatted decimal: bit-exact,
        // and two distinct floats can never collapse onto one string.
        let _ = write!(m, "|op={:08x}", opacity.to_bits());
        let _ = write!(m, "|alpha={alpha_mode:?}");
        let _ = write!(
            m,
            "|pos={:08x}:{:08x}",
            transform.position.0.to_bits(),
            transform.position.1.to_bits()
        );
        let _ = write!(
            m,
            "|scale={:08x}:{:08x}",
            transform.scale.0.to_bits(),
            transform.scale.1.to_bits()
        );
        let _ = write!(m, "|rot={:08x}", transform.rotation_deg.to_bits());
        let _ = write!(
            m,
            "|crop={:08x}:{:08x}:{:08x}:{:08x}",
            crop.left.to_bits(),
            crop.top.to_bits(),
            crop.right.to_bits(),
            crop.bottom.to_bits()
        );
        // `keyframes` is D-15's own gap, found by reading the type rather than
        // the prose: a NON-EMPTY track OVERRIDES its static field at sample
        // time (`Clip::sample_at`, used by `multilayer.rs`'s per-frame layer
        // build), so an animated clip's pixels vary across the segment by
        // construction. Hashing only the static fallbacks would make a fade-in
        // and a fade-out that share them ONE cache entry.
        //
        // `keyframes`/`text`/`retime` go in as `serde_json` of THAT FIELD
        // ALONE — each is pixel-affecting in its entirety, so an allowlist
        // inside them would buy nothing and cost a second enumeration to keep
        // in sync. What is forbidden is serializing the whole `Clip`, which
        // would re-admit the four excluded fields above through the back door.
        let _ = write!(m, "|kf={}", json_material(keyframes));
        let _ = write!(m, "|text={}", json_material(text));
        let _ = write!(m, "|retime={}", json_material(retime));
        let _ = write!(
            m,
            "|media={}:{}:{}",
            media.canonical_path, media.mtime_ns, media.size_bytes
        );
        let _ = write!(m, "|dec={}", decode.material());

        ClipKeyMaterial { material: m }
    }

    /// The canonical material string. Exposed read-only so a test can print
    /// what actually went into a hash when an assertion fails.
    pub fn as_str(&self) -> &str {
        &self.material
    }
}

/// Everything segment `k` composites from, ready to hash (D-15).
///
/// # THE RECORDED OMISSION (D-37)
///
/// **Track enable/mute is NOT in this material because `rudis_core::Track` has
/// no such field** — it is `{ kind, clips }` only, verified against the type on
/// 2026-08-03. The only mute-shaped tool, `SetClipMuted`, writes `Clip.volume`
/// — an audio property, excluded by D-08 regardless. D-15's prose assumed a
/// feature that was never built.
///
/// **A future phase that adds track enable/mute MUST add it here at that
/// moment.** The omission is written down rather than silently dropped
/// precisely so that phase has something to find; `tests/key_material.rs`
/// carries an exhaustive `let rudis_core::Track { kind: _, clips: _ } = track;`
/// destructure whose sole job is to STOP COMPILING on the day the field
/// appears, and to point here when it does.
///
/// Two further `Clip` fields are excluded, each classified rather than
/// forgotten — see [`ClipKeyMaterial::from_clip`]'s destructure for the full
/// per-field accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentKeyInputs {
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub fps: f64,
    pub seg_index: i64,
    /// The clips visible anywhere in the segment, in (track_index, clip_index)
    /// order. That ORDER is itself key material — the ordered walk is the
    /// z-order term.
    pub clips: Vec<ClipKeyMaterial>,
}

impl SegmentKeyInputs {
    /// The exact string that is hashed. Kept as its own function so a test can
    /// vary one field at a time and print the material that produced a hash.
    pub fn material(&self) -> String {
        let mut m = format!(
            "v{RENDER_CACHE_VERSION}|{}x{}|fps{:016x}|seg{SEG_US}|k{}|n{}",
            self.canvas_w,
            self.canvas_h,
            self.fps.to_bits(),
            self.seg_index,
            self.clips.len(),
        );
        for clip in &self.clips {
            m.push('\n');
            m.push_str(&clip.material);
        }
        m
    }
}

/// The segment's identity (D-15) — FNV-1a over the canonical material string.
///
/// Hand-rolled ON PURPOSE, for the reason `crates/waveform`, `crates/filmstrip`
/// and `crates/proxy` all state at length: the default hasher behind `std`'s
/// `HashMap` is explicitly documented as NOT stable across Rust releases, so
/// deriving a FILENAME from it would silently orphan every cached file on a
/// toolchain bump — an invisible cache wipe that looks like nothing at all.
/// FNV-1a is fixed arithmetic, so the same material yields the same name on
/// every build, forever. In-house precedent copied from our own crate, not a
/// third-party borrow, so no `PROVENANCE.md` entry is owed.
///
/// **Not a security hash, and it does not need to be** (threat T-59-02-04):
/// the full identity — hash, geometry, fps, encoder, payload length — is stored
/// INSIDE the meta and compared on read, so a collision degrades to a MISS,
/// never to confidently-served wrong content.
pub fn segment_hash(inputs: &SegmentKeyInputs) -> u64 {
    fnv1a64(inputs.material().as_bytes())
}

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
