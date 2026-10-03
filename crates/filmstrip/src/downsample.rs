//! D-08's aspect-preserving fit into a fixed tile cell.
//!
//! A 9:16 phone clip shows as a narrow thumbnail with gaps beside it rather than
//! being centre-cropped: the filmstrip visibly differs for vertical footage,
//! which is information, not a defect.
//!
//! The padding is **alpha = 0**, not a colour. Nothing in this crate knows the
//! Timeline's design tokens and nothing here should: the renderer draws its
//! token-coloured clip body underneath and the transparent gaps let it through,
//! so colour decisions stay on the C# side where the token dictionary lives
//! (CLAUDE.md convention 7).

use image::imageops::FilterType;
use image::{ImageBuffer, Rgba};

/// Fit `rgba` into a `cell_w` x `cell_h` RGBA cell, preserving aspect, centered,
/// with TRANSPARENT padding.
///
/// Returns exactly `cell_w * cell_h * 4` bytes. NEVER panics: the input crosses
/// a process boundary from an ffmpeg sidecar, so a zero dimension, a
/// length/geometry mismatch, or an overflowing product all resolve to a fully
/// transparent cell -- the same "a bad frame costs one blank tile, never a
/// crashed import" posture the cache takes toward a bad file.
///
/// `FilterType::Triangle` is deliberate and low-stakes: thumbnail resampling is
/// squarely in the "getting it subtly wrong just looks slightly off" category,
/// unlike the atlas/eviction bookkeeping downstream. Triangle is the cheap
/// separable filter that still averages rather than point-samples, so a 1080p
/// frame reduced ~20x does not alias into noise the way nearest-neighbour would.
pub fn downsample_into_cell(
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    cell_w: u32,
    cell_h: u32,
) -> Vec<u8> {
    let cell_px = (cell_w as usize).saturating_mul(cell_h as usize);
    let mut cell = vec![0u8; cell_px.saturating_mul(4)];
    if cell_w == 0 || cell_h == 0 || src_w == 0 || src_h == 0 {
        return cell;
    }

    // The declared geometry must account for exactly the bytes handed over.
    let expected = (src_w as usize)
        .checked_mul(src_h as usize)
        .and_then(|px| px.checked_mul(4));
    if expected != Some(rgba.len()) {
        return cell;
    }

    // Scale to FIT (the smaller of the two ratios), never to fill.
    let scale = f64::min(
        cell_w as f64 / src_w as f64,
        cell_h as f64 / src_h as f64,
    );
    let dst_w = ((src_w as f64 * scale).round() as u32).clamp(1, cell_w);
    let dst_h = ((src_h as f64 * scale).round() as u32).clamp(1, cell_h);

    let Some(view) = ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(src_w, src_h, rgba) else {
        return cell;
    };
    let resized = image::imageops::resize(&view, dst_w, dst_h, FilterType::Triangle);

    // Blit centered. Integer division biases a leftover odd pixel to the RIGHT
    // side, consistently, so a strip of same-aspect tiles stays visually aligned.
    let x0 = (cell_w - dst_w) / 2;
    let y0 = (cell_h - dst_h) / 2;
    let src_row_bytes = dst_w as usize * 4;
    let cell_row_bytes = cell_w as usize * 4;
    let src = resized.as_raw();
    for y in 0..dst_h as usize {
        let src_start = y * src_row_bytes;
        let dst_start = (y + y0 as usize) * cell_row_bytes + x0 as usize * 4;
        let (Some(from), Some(to)) = (
            src.get(src_start..src_start + src_row_bytes),
            cell.get_mut(dst_start..dst_start + src_row_bytes),
        ) else {
            // Unreachable given the clamps above, and still handled: a blit that
            // cannot land leaves the cell transparent instead of panicking.
            break;
        };
        to.copy_from_slice(from);
    }
    cell
}
