//! Phase 53.2's producer half: one media file in, one packed strip of
//! downsampled RGBA thumbnails out.
//!
//! The structural sibling of `crates/waveform` (Phase 52's SHELL-09 producer),
//! generalised from `u8` scalar peaks to RGBA image tiles. Everything that crate
//! settled -- the `(canonical path, mtime_ns, size_bytes)` cache key, the five
//! cache properties, the injected-decode form that lets a test PROVE the chunk
//! bound instead of reading the loop and believing it -- is copied here rather
//! than re-decided.
//!
//! One property is the whole point of this module and everything else is
//! bookkeeping: **extraction is CHUNKED, and the chunk is sized in BYTES.**
//!
//! # Why the audio precedent's number would have been catastrophic
//!
//! `engine::render_audio_pcm` materialises a `Vec<f32>` for whatever span it is
//! asked for, so `crates/waveform` walks the source in five-minute slices --
//! ~58 MB of mono `f32` per slice, held one at a time.
//! `engine::decode_frames_rgba_seq` has the same materialise-the-whole-run
//! shape, and NO scale parameter: it decodes `count` consecutive frames at the
//! source's NATIVE resolution and buffers the entire sidecar stdout before
//! returning. A single 1080p RGBA frame is 1920 x 1080 x 4 = 7.9 MiB, so five
//! minutes at 30fps is ~9,000 frames, i.e. roughly **69 GiB for one chunk**.
//!
//! The discipline is inherited; the number is not. [`FILMSTRIP_CHUNK_BUDGET_BYTES`]
//! is a BYTE budget and [`frames_per_chunk`] divides it by the real per-frame
//! cost. The audio literal must never appear in this crate, and a plan-time grep
//! for it is one of this work's acceptance criteria.
//!
//! ## What this crate is NOT allowed to do
//!
//! `crates/engine` is under the annotated tag `engine-axis-freeze`: its files
//! may be CALLED and never MODIFIED. This crate calls exactly
//! `decode_frames_rgba_seq` and reads the `Frame` struct it returns, both
//! already `pub use`d at that crate's root, so this dependency edge requires no
//! change whatsoever to its manifest or its sources.

pub mod cache;
pub mod downsample;

use std::path::Path;

pub use cache::{CachedStrip, FilmstripCacheKey, StripHeader};
pub use downsample::downsample_into_cell;

/// Tile cell width, in pixels.
///
/// A 16:9 cell at 96 x 54, drawn at roughly 60 x 34 logical pixels inside the
/// 34px frame body a 48px video lane leaves under D-01's 14px title band. The
/// ~1.6x headroom covers a 150% DPI scale without resampling up from a
/// lower-resolution tile.
pub const TILE_W: u32 = 96;
/// Tile cell height, in pixels. See [`TILE_W`].
pub const TILE_H: u32 = 54;

/// How many tiles sit side by side in one sheet row.
///
/// D-11 names "a very wide sheet" as the thing to avoid; 16 tiles per row makes
/// the sheet 1,536 px wide -- comfortably inside every GPU's maximum 2D texture
/// dimension -- and wraps the rest downward.
pub const TILES_PER_ROW: u32 = 16;

/// The hard ceiling on tiles per media file.
///
/// Bounds two different things at once: the strip's bytes (256 tiles at 96 x 54
/// RGBA is 5.06 MiB, well inside [`cache::MAX_STRIP_FILE_BYTES`]) and the
/// extraction WORK per file. An hour-long import gets a ~14-second interval
/// rather than 3,600 tiles; see [`plan_tiles`].
pub const FILMSTRIP_MAX_TILES: u32 = 256;

/// D-06's fixed source interval: one tile per second of media.
///
/// ONE cached density, chosen once and never re-extracted. Zooming the Timeline
/// selects a nearer or farther tile from the same strip -- zero decode, zero
/// I/O -- exactly as Phase 52's D-17 decided for peaks. Media longer than
/// [`FILMSTRIP_MAX_TILES`] seconds coarsens past this floor.
pub const FILMSTRIP_MIN_INTERVAL_US: i64 = 1_000_000;

/// D-10's memory cap, derived in BYTES.
///
/// `frames_per_chunk = FILMSTRIP_CHUNK_BUDGET_BYTES / (width * height * 4)`.
/// At 1080p that is 32 frames, a ~253 MiB FRAME BUFFER held for the length of
/// one `decode_frames_rgba_seq` call and freed before the next one is requested.
///
/// This is emphatically NOT the audio precedent's five-minute figure. Applied to
/// 1080p RGBA that duration would attempt roughly **69 GiB** per chunk
/// (53.2-RESEARCH.md, Pitfall 1) -- the exact warning sign that research names.
///
/// # MEASURED, plan 53.2-04 Task 3 -- and the arithmetic above UNDERSTATES the
/// # real cost by ~1.9x
///
/// Against a real 720p source the single-job process working set grew to
/// **476.6 MiB** where this budget predicts 253.1 MiB. The gap is not slack in
/// the bound: the FROZEN `engine::decode_frames_rgba_seq` holds the sidecar's
/// stdout `Vec<u8>` AND the converted `Vec<Frame>` SIMULTANEOUSLY, so the real
/// peak is roughly twice the frame buffer this constant sizes. That is exactly
/// the correction `crates/waveform` already had to make for audio (~124 MiB
/// measured against 57.6 MiB of arithmetic, a 2.15x factor); it was not carried
/// into this constant when it was derived, and it is carried now.
///
/// **The value was nevertheless NOT reduced, and that is a measured decision.**
/// Halving it to land the real peak near 253 MiB would collapse D-10's whole
/// premise at the most common resolution: at 720p/30fps against
/// [`FILMSTRIP_MIN_INTERVAL_US`], any budget below ~211 MiB drives
/// `tiles_in_chunk` to 1, i.e. **one seek per thumbnail** -- the single design
/// choice D-10 names and rejects. 256 MiB is therefore already AT its floor,
/// and the process-memory bound this constant cannot carry alone is carried
/// instead by `app_core::filmstrip_job::MAX_CONCURRENT_FILMSTRIP_JOBS` (NOT an
/// intra-doc link: `app-core` depends on THIS crate, never the reverse), whose
/// own ablation independently landed on 1. The two constants argue for each
/// other; neither is meaningful alone.
///
/// Full numbers:
/// `.planning/phases/53.2-*/artifacts/53.2-04-extraction-measurement.md`.
pub const FILMSTRIP_CHUNK_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// D-14's publish cadence: republish the strip every this many completed tiles.
///
/// At most `ceil(256 / 16) = 16` cache rewrites per file, which bounds the I/O
/// churn progressive fill costs while still letting frames march in visibly on
/// long media.
pub const PUBLISH_EVERY_TILES: u32 = 16;

/// Upper clamp on [`frames_per_chunk`], for resolutions small enough that the
/// byte budget would license a run of thousands.
///
/// A tiny frame is cheap in MEMORY but not in TIME -- every frame in a chunk is
/// fully decoded and most are discarded -- so the cap bounds the second cost
/// too.
pub const MAX_FRAMES_PER_CHUNK: usize = 240;

/// Everything that can go wrong while producing a strip.
#[derive(Debug, thiserror::Error)]
pub enum FilmstripError {
    /// `engine::decode_frames_rgba_seq` failed for this chunk. Carried as a
    /// string so this crate's error type does not leak the frozen engine's error
    /// enum into every consumer's match arms.
    #[error("frame decode failed: {0}")]
    Decode(String),

    /// Filesystem failure. Currently unreachable from [`extract_strip`] (the
    /// engine owns all file access) and present for callers that inject their
    /// own decode function.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Sheet width in pixels: [`TILES_PER_ROW`] cells side by side.
pub fn sheet_width() -> u32 {
    TILES_PER_ROW * TILE_W
}

/// How many whole tile-rows `tiles` occupy.
pub fn sheet_rows_for(tiles: u32) -> u32 {
    tiles.div_ceil(TILES_PER_ROW)
}

/// The byte length of a sheet covering `tiles`, i.e. the exact payload the cache
/// expects for `completed_tiles == tiles`.
pub fn sheet_bytes_for(tiles: u32) -> usize {
    sheet_rows_for(tiles) as usize * TILE_H as usize * sheet_width() as usize * 4
}

/// How many tiles a source of `duration_us` gets, and how far apart they sit.
///
/// Below the cap this is D-06's flat one-tile-per-second. Above it the interval
/// coarsens so the tile COUNT stays at [`FILMSTRIP_MAX_TILES`] -- which is the
/// mitigation for T-53.2-05, not merely a tidiness rule: `duration_us` reaches
/// this crate from ffprobe container metadata, i.e. from attacker-influenceable
/// input, and a count computed straight from it would turn a bogus multi-century
/// duration into an instant allocation failure.
///
/// A non-positive duration is `(0, 0)` -- nothing to draw is not an error.
pub fn plan_tiles(duration_us: i64) -> (u32, i64) {
    if duration_us <= 0 {
        return (0, 0);
    }
    // i128 throughout: `i64::MAX` microseconds is a legal (if absurd) probe
    // result, and a ceil computed as `(d + step - 1) / step` in i64 would
    // overflow on exactly the input this clamp exists to survive.
    let d = duration_us as i128;
    let base = FILMSTRIP_MIN_INTERVAL_US as i128;
    let max = FILMSTRIP_MAX_TILES as i128;

    let ideal = (d + base - 1) / base;
    if ideal <= max {
        // `ideal` is at most `max`, so the cast is lossless.
        return (ideal as u32, FILMSTRIP_MIN_INTERVAL_US);
    }
    // Coarsen: spread FILMSTRIP_MAX_TILES tiles across the whole source. The
    // ceil keeps the last tile inside the source rather than past its end.
    let interval = (d + max - 1) / max;
    // `interval <= d <= i64::MAX` because `max >= 1`, so this cannot truncate.
    (FILMSTRIP_MAX_TILES, interval as i64)
}

/// How many frames one `decode_frames_rgba_seq` call may ask for at this source
/// resolution -- D-10's cap, derived in BYTES (T-53.2-04).
///
/// `FILMSTRIP_CHUNK_BUDGET_BYTES / (src_w * src_h * 4)`, clamped to
/// `[1, MAX_FRAMES_PER_CHUNK]`. A degenerate or absurd resolution yields 1: one
/// frame per call is slow, and slow is the correct failure mode for a background
/// job whose alternative is an allocation the machine cannot satisfy.
pub fn frames_per_chunk(src_w: u32, src_h: u32) -> usize {
    let frame_bytes = (src_w as u64)
        .checked_mul(src_h as u64)
        .and_then(|px| px.checked_mul(4))
        .unwrap_or(u64::MAX);
    if frame_bytes == 0 {
        // A zero dimension means the probe is not to be trusted about size
        // either. Take the conservative end of the clamp.
        return 1;
    }
    let n = (FILMSTRIP_CHUNK_BUDGET_BYTES as u64 / frame_bytes) as usize;
    n.clamp(1, MAX_FRAMES_PER_CHUNK)
}

/// Copy one downsampled cell into the packed sheet at `tile_index`.
fn blit_tile(sheet: &mut [u8], tile_index: u32, cell: &[u8]) {
    let sheet_w = sheet_width() as usize;
    let row = (tile_index / TILES_PER_ROW) as usize;
    let col = (tile_index % TILES_PER_ROW) as usize;
    let cell_row_bytes = TILE_W as usize * 4;
    for y in 0..TILE_H as usize {
        let src_start = y * cell_row_bytes;
        let dst_start = ((row * TILE_H as usize + y) * sheet_w + col * TILE_W as usize) * 4;
        let (Some(from), Some(to)) = (
            cell.get(src_start..src_start + cell_row_bytes),
            sheet.get_mut(dst_start..dst_start + cell_row_bytes),
        ) else {
            // Unreachable while `tile_index < total` and the sheet was allocated
            // from the same `total`; handled anyway so a future caller cannot
            // turn an arithmetic slip into a panic on a background thread.
            return;
        };
        to.copy_from_slice(from);
    }
}

/// [`extract_strip`] with the decode step injected.
///
/// `decode(position_us, count)` stands in for
/// `engine::decode_frames_rgba_seq(path, position_us, count, rotation)`. This
/// form exists so the chunk bound can be PROVEN by a test that records every
/// `(position, count)` it is asked for and measures what is live when it is
/// asked, rather than claimed by reading the loop -- exactly why
/// `waveform::extract_peaks_with` exists.
///
/// `publish(sheet_rows_so_far, completed_tiles)` is called once per
/// [`PUBLISH_EVERY_TILES`] batch and once at the end; the caller owns writing
/// the cache revision. Returning `false` ABORTS extraction cleanly -- the cache
/// is best-effort, so a failing disk stops the job and never panics.
///
/// `frame_step_us` is the source's inter-frame spacing (`engine::frame_step_us`
/// at the caller). It is only ever used to pick WHICH decoded frame stands
/// closest to a tile's timestamp; a wrong value costs a slightly-off frame
/// choice, never a panic or an out-of-range read.
pub fn extract_strip_with<D, P>(
    mut decode: D,
    mut publish: P,
    duration_us: i64,
    src_w: u32,
    src_h: u32,
    frame_step_us: i64,
) -> Result<(), FilmstripError>
where
    D: FnMut(i64, usize) -> Result<Vec<engine::Frame>, FilmstripError>,
    P: FnMut(&[u8], u32) -> bool,
{
    let (total, interval) = plan_tiles(duration_us);
    if total == 0 || interval <= 0 {
        return Ok(());
    }

    let fpc = frames_per_chunk(src_w, src_h);
    let step = frame_step_us.max(1);

    // Bounded by FILMSTRIP_MAX_TILES at 5.06 MiB, so the sheet is safe to hold
    // whole -- it is three orders of magnitude smaller than ONE chunk of decoded
    // native-resolution frames, which is the allocation that actually matters.
    let mut sheet = vec![0u8; sheet_bytes_for(total)];

    let mut k: u32 = 0;
    let mut last_published: u32 = 0;
    while k < total {
        // Centre sampling: tile k stands for the middle of its interval, not its
        // leading edge, so the first tile is not always the (often black) first
        // frame of the file.
        let t_k = (k as i64)
            .saturating_mul(interval)
            .saturating_add(interval / 2)
            .clamp(0, duration_us - 1);

        // How much source time one chunk's worth of consecutive frames spans,
        // and therefore how many whole tiles ONE seek can serve.
        let span_us = (fpc as i64).saturating_mul(step);
        let tiles_in_chunk = (span_us / interval).clamp(1, (total - k) as i64) as u32;

        // When the interval exceeds the chunk span -- high resolution, or a
        // coarse density on long media -- decoding a full run would throw all but
        // one frame away. Ask for exactly the one frame that tile needs instead.
        // When the interval is fine, one seek amortises over `tiles_in_chunk`
        // kept tiles, which is what D-10's "no seek per thumbnail" buys.
        let frames_needed = if tiles_in_chunk == 1 { 1 } else { fpc };

        let frames = decode(t_k, frames_needed)?;
        if !frames.is_empty() {
            for j in 0..tiles_in_chunk {
                let target = (j as i64).saturating_mul(interval);
                // Nearest, not floor: at a coarse step the frame AFTER the
                // target can be materially closer to it than the one before.
                let idx = ((target + step / 2) / step).clamp(0, frames.len() as i64 - 1) as usize;
                let Some(frame) = frames.get(idx) else {
                    continue;
                };
                // The DECODED frame's own dimensions, not the probe's: the
                // engine has already applied rotation by this point, so a 90
                // degree phone clip arrives upright with swapped dimensions.
                let cell = downsample_into_cell(
                    &frame.rgba,
                    frame.width,
                    frame.height,
                    TILE_W,
                    TILE_H,
                );
                blit_tile(&mut sheet, k + j, &cell);
            }
        }
        // `frames` is freed HERE, at the end of this iteration, BEFORE the next
        // chunk is requested. That single fact is the entire mitigation for
        // T-53.2-04: hoisting it out of the loop, or collecting chunks before
        // reducing them, silently restores the unbounded behaviour this crate
        // exists to avoid. Explicit rather than implicit for exactly that reason.
        drop(frames);

        k += tiles_in_chunk;
        if k - last_published >= PUBLISH_EVERY_TILES || k == total {
            let covered = sheet_bytes_for(k).min(sheet.len());
            let Some(slice) = sheet.get(..covered) else {
                return Ok(());
            };
            if !publish(slice, k) {
                // Best-effort: a refused publish stops the job. Not an error --
                // losing the cache costs one re-extraction, never an import.
                return Ok(());
            }
            last_published = k;
        }
    }
    Ok(())
}

/// Extract the full strip of `path`, publishing revisions as chunks complete.
///
/// `duration_us`, `src_w`, `src_h`, `rotation_degrees` and `frame_step_us` come
/// from the caller's already-performed probe (the import path has all of them in
/// hand); this crate never probes again.
pub fn extract_strip(
    path: &Path,
    duration_us: i64,
    src_w: u32,
    src_h: u32,
    rotation_degrees: u32,
    frame_step_us: i64,
    publish: impl FnMut(&[u8], u32) -> bool,
) -> Result<(), FilmstripError> {
    extract_strip_with(
        |position_us, count| {
            // The FROZEN engine, CALLED and never modified.
            engine::decode_frames_rgba_seq(path, position_us, count, rotation_degrees)
                .map_err(|e| FilmstripError::Decode(e.to_string()))
        },
        publish,
        duration_us,
        src_w,
        src_h,
        frame_step_us,
    )
}
