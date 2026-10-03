//! SHELL-09's producer half (Phase 52, plan 52-02).
//!
//! Turns a real media file's audio into a `u8`-quantised block-RMS peak
//! envelope, at BOUNDED memory whatever the source's length.
//!
//! One property is the whole point of this module; everything else is
//! bookkeeping: **extraction is CHUNKED.** `engine::render_audio_pcm`
//! materialises a `Vec<f32>` for the WHOLE span it is asked for -- ~58 MB of
//! `f32` per 304-second span. One unbounded call against a multi-hour import
//! would allocate gigabytes on a background import thread (threat T-52-06).
//! [`extract_peaks_with`] therefore walks the source in [`EXTRACT_CHUNK_US`]
//! slices and holds exactly ONE of them at a time.
//!
//! ## What this crate is NOT allowed to do
//!
//! `crates/engine` is under the annotated tag `engine-axis-freeze`: its files
//! may be CALLED and never MODIFIED. This crate calls exactly
//! `render_audio_pcm` and `AUDIO_SAMPLE_RATE`, both already `pub` at that
//! crate's root, and touches nothing else there -- not its sources, not its
//! manifest.

pub mod cache;

use std::path::Path;

pub use cache::{CachedPeaks, WaveformCacheKey};

/// Re-export of the engine's own canonical rendered-audio sample rate
/// (48 kHz mono `f32`), so a consumer of this crate does not need its own
/// dependency edge on the frozen engine merely to read one integer.
pub use engine::AUDIO_SAMPLE_RATE;

/// Peak block size: one output peak per 10 ms of audio.
///
/// Deliberately EQUAL to `ENVELOPE_BLOCK_US` in
/// `crates/engine/src/audio_sync.rs` (D-15). Two different envelope
/// resolutions in one codebase -- one for waveform display, one for
/// sync-by-waveform -- would be an unforced inconsistency, so the number is
/// copied rather than re-chosen.
pub const PEAK_BLOCK_US: i64 = 10_000;

/// The DoS cap (threat T-52-06): the longest span this crate will ever ask
/// `engine::render_audio_pcm` to materialise in one call.
///
/// Mirrors the cap SHAPE of `SYNC_MAX_WINDOW_US` in
/// `crates/engine/src/audio_sync.rs` -- and its exact value, five minutes.
/// The difference in kind: that constant REJECTS an over-long window, while
/// this one SUBDIVIDES it, because a waveform must cover the whole source.
/// At 48 kHz mono `f32` one chunk is 57.6 MB, and peak resident memory is one
/// chunk regardless of whether the source is 5 minutes or 5 hours.
pub const EXTRACT_CHUNK_US: i64 = 5 * 60 * 1_000_000;

/// Upper bound on the peak vector this crate will PRE-reserve.
///
/// `duration_us` reaches this crate from `ffprobe` metadata, i.e. from
/// attacker-influenceable input. A capacity computed straight from it would
/// turn a bogus multi-century duration into an instant allocation failure, so
/// the reservation is an optimisation that is clamped, never a promise.
/// 8 MiB of peaks is ~22 hours at 100 peaks/second -- past every real source.
const MAX_RESERVED_PEAKS: usize = 8 * 1024 * 1024;

/// Everything that can go wrong while producing an envelope.
#[derive(Debug, thiserror::Error)]
pub enum WaveformError {
    /// `engine::render_audio_pcm` failed for this chunk. Carried as a string
    /// so this crate's error type does not leak the frozen engine's error
    /// enum into every consumer's match arms.
    #[error("audio decode failed: {0}")]
    Decode(String),

    /// Filesystem failure while reading the source. Currently unreachable
    /// from [`extract_peaks`] (the engine owns all file access) and present
    /// for callers that inject their own decode function.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Block-RMS downsample, quantised to `u8`: one output byte per `block_us` of
/// input, each the RMS of its block clamped to `[0, 1]` and scaled to 0-255.
///
/// **This REIMPLEMENTS the technique of `crates/engine/src/audio_sync.rs`'s
/// private `envelope()` -- it does not, and cannot, import it.** That function
/// is private, and its file is under the `engine-axis-freeze` tag regardless,
/// so making it public is not an option either. The shape is therefore copied
/// deliberately: same 10 ms blocks, same "RMS per block" contour. The only
/// intended difference is the return type -- `audio_sync.rs` keeps `f32`
/// because it feeds a cross-correlation; this returns `u8` because it feeds a
/// cached, drawn waveform (D-17).
///
/// Do not let the two diverge without stating a reason here.
pub fn block_rms_u8(pcm: &[f32], sample_rate: u32, block_us: i64) -> Vec<u8> {
    let block_len = ((sample_rate as i64 * block_us) / 1_000_000).max(1) as usize;
    pcm.chunks(block_len)
        .map(|chunk| {
            let sum_sq: f64 = chunk.iter().map(|&s| (s as f64) * (s as f64)).sum();
            let rms = (sum_sq / chunk.len().max(1) as f64).sqrt();
            (rms.clamp(0.0, 1.0) * 255.0) as u8
        })
        .collect()
}

/// [`extract_peaks`] with the decode step injected.
///
/// `decode(in_us, out_us)` stands in for `engine::render_audio_pcm`. This form
/// exists so the chunk bound can be PROVEN by a test that records every span
/// it is asked for, rather than claimed by reading the loop.
pub fn extract_peaks_with<F>(
    mut decode: F,
    duration_us: i64,
    has_audio: bool,
) -> Result<Vec<u8>, WaveformError>
where
    F: FnMut(i64, i64) -> Result<Vec<f32>, WaveformError>,
{
    // A source with no audio stream would make `render_audio_pcm` return
    // `Ok(vec![])`, and a non-positive duration would make it return
    // `Err(InvalidWindow)`. Neither is worth an ffmpeg subprocess, and neither
    // is an error here.
    if !has_audio || duration_us <= 0 {
        return Ok(Vec::new());
    }

    // Reserve ONCE, so the `extend` below does not repeatedly reallocate (and
    // memcpy) a multi-hour vector. Clamped -- see MAX_RESERVED_PEAKS.
    let reserve = ((duration_us / PEAK_BLOCK_US) as usize)
        .saturating_add(1)
        .min(MAX_RESERVED_PEAKS);
    let mut peaks: Vec<u8> = Vec::with_capacity(reserve);

    let mut cursor = 0i64;
    while cursor < duration_us {
        let chunk_end = (cursor + EXTRACT_CHUNK_US).min(duration_us);
        // ONE chunk's f32 buffer -- at 48 kHz mono that is 57.6 MB, and it is
        // the largest live allocation this function ever makes.
        let pcm = decode(cursor, chunk_end)?;
        peaks.extend(block_rms_u8(&pcm, AUDIO_SAMPLE_RATE, PEAK_BLOCK_US));
        cursor = chunk_end;
        // `pcm` goes out of scope and is freed HERE, at the end of this
        // iteration, BEFORE the next chunk is requested. That single fact is
        // the entire mitigation for T-52-06: hoisting `pcm` out of the loop,
        // or collecting chunks before reducing them, silently restores the
        // unbounded behaviour this crate exists to avoid.
    }
    Ok(peaks)
}

/// Extract the full peak envelope of `path`'s audio.
///
/// `duration_us` and `has_audio` come from the caller's already-performed
/// probe (the import path has both in hand); this crate never probes again.
pub fn extract_peaks(
    path: &Path,
    duration_us: i64,
    has_audio: bool,
) -> Result<Vec<u8>, WaveformError> {
    extract_peaks_with(
        |in_us, out_us| {
            // volume 1.0: the envelope describes the SOURCE, not a clip's
            // current gain. A clip's volume is a render-time property and must
            // not be baked into a cache keyed by the file on disk.
            engine::render_audio_pcm(path, in_us, out_us, 1.0)
                .map_err(|e| WaveformError::Decode(e.to_string()))
        },
        duration_us,
        has_audio,
    )
}
