//! Phase 27 (LIB-04): hand-rolled audio cross-correlation — NO new crate
//! (research confirmed neither rustfft nor realfft is in Cargo.lock). Operates
//! on the SAME 48kHz mono f32 PCM shape [`crate::render_audio_pcm`] already
//! produces. Downsamples to a coarse block-RMS envelope BEFORE correlating
//! (Pitfall 3: raw-sample correlation over hundreds of thousands of samples is
//! slow AND fragile to differing mic frequency response).

use crate::EngineError;

/// Envelope block size (research Assumption A3, 5-10ms suggested — picked the
/// upper bound for speed while staying well under SC-2's "aligned at start/
/// middle/end" tolerance).
const ENVELOPE_BLOCK_US: i64 = 10_000;

/// Normalized-correlation floor below which a match is declined. Re-tuned in
/// Plan 27-04 against REAL fixtures (27-01 explicitly deferred this calibration
/// to 27-04's integration tests). The correlation runs over a block-RMS
/// *envelope*, whose samples are all NON-NEGATIVE, so its normalized score has a
/// naturally high baseline — two genuinely UNRELATED real speech recordings
/// (speech_en vs speech_es) still correlate at ~0.77, while a true match
/// (speech vs its own delayed copy) scores ~1.0. Research Assumption A2's
/// 0.15-0.3 suggestion assumed a mean-centered metric and does NOT hold for this
/// envelope-cosine measure; the floor therefore sits ABOVE the empirical
/// unrelated-speech baseline (0.9), cleanly separating the two regimes
/// (proven by `sync_audio_declines_low_confidence_match`).
pub const SYNC_CONFIDENCE_FLOOR: f64 = 0.9;

/// DoS cap on a sync_audio correlation window (mirrors
/// [`crate::whisper::WHISPER_MAX_WINDOW_US`]'s precedent) — 5 minutes is far
/// beyond any real alignment use case and keeps the O(n·m) envelope search
/// (n,m = window_len / ENVELOPE_BLOCK_US) comfortably sub-second.
pub const SYNC_MAX_WINDOW_US: i64 = 5 * 60 * 1_000_000;

/// Reject a sync_audio correlation window longer than [`SYNC_MAX_WINDOW_US`]
/// BEFORE any `render_audio_pcm` call (resource-exhaustion cap, mirrors
/// `check_whisper_window_cap`).
pub fn check_sync_window_cap(in_us: i64, out_us: i64) -> Result<(), EngineError> {
    if out_us - in_us > SYNC_MAX_WINDOW_US {
        return Err(EngineError::SidecarFailed {
            tool: "sync_audio".to_string(),
            status: -1,
            stderr: format!(
                "sync_audio correlation window {}us exceeds the {}us ({}min) ceiling",
                out_us - in_us,
                SYNC_MAX_WINDOW_US,
                SYNC_MAX_WINDOW_US / 60_000_000
            ),
        });
    }
    Ok(())
}

/// Block-RMS downsample: one output sample per `block_us` of input, each the
/// RMS of its block (loudness-contour matching — the "Based on Waveform"
/// technique real sync-by-waveform tools use, not raw-sample correlation).
fn envelope(pcm: &[f32], sample_rate: u32, block_us: i64) -> Vec<f32> {
    let block_len = ((sample_rate as i64 * block_us) / 1_000_000).max(1) as usize;
    pcm.chunks(block_len)
        .map(|chunk| {
            let sum_sq: f64 = chunk.iter().map(|&s| (s as f64) * (s as f64)).sum();
            ((sum_sq / chunk.len() as f64).sqrt()) as f32
        })
        .collect()
}

/// Find the lag (in MICROSECONDS) that maximizes the NORMALIZED cross-
/// correlation between `a` and `b`'s block-RMS envelopes, searching every
/// possible overlap. Returns `(lag_us, score)`, `score` in `[0, 1]` — the
/// Cauchy-Schwarz upper bound is 1, and the lower bound is 0 (NOT -1) because
/// both envelopes are RMS magnitudes (`envelope()` values are always `>= 0`), so
/// every product term is non-negative and the score can never go negative; this
/// non-negativity is why unrelated content still scores well above 0. `0.0` also
/// when either envelope is all-zero/silent.
///
/// SIGN CONVENTION: a POSITIVE `lag_us` means `b`'s content lags `a`'s (the
/// same acoustic event appears LATER in `b`) — the caller must move the clip
/// owning `b` EARLIER on the timeline by `lag_us` to align it under `a`.
pub fn best_lag_us(a_pcm: &[f32], b_pcm: &[f32], sample_rate: u32) -> (i64, f64) {
    let a_env = envelope(a_pcm, sample_rate, ENVELOPE_BLOCK_US);
    let b_env = envelope(b_pcm, sample_rate, ENVELOPE_BLOCK_US);
    let max_lag = a_env.len().max(b_env.len()) as i64;
    let mut best_lag_blocks = 0i64;
    let mut best_score = 0.0f64;
    for lag in -max_lag..=max_lag {
        let mut num = 0.0f64;
        let mut ea = 0.0f64;
        let mut eb = 0.0f64;
        for i in 0..a_env.len() {
            let j = i as i64 + lag;
            if j < 0 || j as usize >= b_env.len() {
                continue;
            }
            let (x, y) = (a_env[i] as f64, b_env[j as usize] as f64);
            num += x * y;
            ea += x * x;
            eb += y * y;
        }
        let denom = (ea * eb).sqrt();
        let score = if denom > 1e-9 { num / denom } else { 0.0 };
        if score > best_score {
            best_score = score;
            best_lag_blocks = lag;
        }
    }
    (best_lag_blocks * ENVELOPE_BLOCK_US, best_score)
}
