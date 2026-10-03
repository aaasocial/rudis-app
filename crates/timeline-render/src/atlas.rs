//! The filmstrip atlas — one fixed texture, a shelf allocator, and a bounded LRU (D-16).
//!
//! # What this module owns, and why it is HERE rather than on the C# side
//!
//! 53.2-CONTEXT's D-12 was authored as *"C# uploads into a bounded `wgpu` texture atlas"*
//! and then CORRECTED: no `wgpu::Device` is reachable from C# anywhere in this codebase.
//! So the split is — **C# decides WHICH strips it wants** (the `PeakCache` residency-policy
//! shape it already owns, plan 53.2-06), and **this crate owns the texture, the packing and
//! the upload**. The bytes ride the per-frame `RudisTimelineClip` pointer/length pair that
//! `peaks_ptr` already proves, so the renderer's export surface stays at exactly six
//! (T-53.2-22).
//!
//! # Three bounds, and each one is structural rather than a budget check
//!
//! 1. **[`ATLAS_SIZE`] — the VRAM ceiling.** One 4096×4096 `Rgba8Unorm` texture, allocated
//!    ONCE at attach and never grown. 64 MiB is the whole cost of this feature, on every
//!    machine, forever (T-53.2-21). There is no code path that can allocate a second one.
//! 2. **[`MAX_RESIDENT_STRIPS`] — the entry ceiling**, beside the area ceiling. Two bounds
//!    because they fail differently: area runs out on a few huge sheets, entries run out on
//!    many small ones, and a design that only checks one of them is a design that has only
//!    thought about one of them.
//! 3. **[`MAX_STRIP_UPLOADS_PER_FRAME`] — the per-frame work ceiling.** A project-open
//!    burst wants dozens of strips at once; uploading them all in one frame is a visible
//!    hitch. Past the cap a strip is simply not resident this frame and the caller falls
//!    back — which is D-16's own stated contract, not a degradation invented here.
//!
//! # The correctness class this file is actually about
//!
//! 53.2-RESEARCH's *"Don't Hand-Roll"* table names atlas bookkeeping specifically: the
//! failure is not a crash, it is **the wrong pixels drawn silently** — a UV rect that
//! outlives its allocation and samples whatever strip landed in that rectangle next. So
//! two invariants are enforced by construction rather than by care:
//!
//! * **Exactly one `deallocate` per `AllocId`.** The only path to `deallocate` is
//!   [`AtlasBook::evict`], and it deallocates only what `HashMap::remove` actually handed
//!   back. `HashMap::remove` cannot return the same entry twice, so the allocator cannot be
//!   double-freed. (etagere `assert!`s on a freed id, so a slip is a panic rather than
//!   corruption — but relying on that would be relying on someone else's debug assertion.)
//! * **No uv survives its allocation.** [`AtlasBook::uv_for`] reads the map, and eviction
//!   removes from the map, so "evicted" and "has no uv" are the same fact rather than two
//!   facts that have to be kept in step.
//!
//! # The PLAIN [`AtlasAllocator`], not etagere's bucketed variant — an ARGUED deviation
//!
//! 53.2-RESEARCH suggested the bucketed one. It is the right choice for MANY SMALL
//! SAME-SIZED rectangles, which is glyphon's case (one bucket per glyph size class) and is
//! why glyphon takes it. This allocator's clients are the opposite: a handful of LARGE,
//! VARIABLE-sized sheets — up to 1536×864 each — allocated per MEDIA (D-07), not per tile.
//! Bucketing rounds every allocation up to a bucket boundary, which on a 1536×864 sheet
//! wastes far more than the shelf allocator's own rounding. The full argument is in
//! `artifacts/53.2-05-atlas-decisions.md`.
//!
//! The rejected type's NAME is deliberately not written anywhere in this crate's sources.
//! This plan's acceptance gate is a grep pinning it at zero, and that grep is a
//! USE-detector: prose saying "we do not take it" trips it exactly as an import would, and
//! a gate that reddens on its own rationale gets the rationale deleted rather than the gate
//! fixed. `abi.rs`'s module header records the same trap for the unchecked UTF-8 decoder;
//! 52-02 hit it twice and this file was written naming the type before the gate was run.

use std::collections::HashMap;

use etagere::{size2, AllocId, AtlasAllocator};

/// The atlas's side, in texels. `4096 × 4096 × 4 B` = **64 MiB of VRAM**, allocated once.
///
/// DERIVATION (recorded in `artifacts/53.2-05-atlas-decisions.md`): the worst realistic
/// sheet is `crates/filmstrip`'s full grid — `TILES_PER_ROW(16) × TILE_W(96) = 1536` wide
/// by `ceil(FILMSTRIP_MAX_TILES(256) / 16) × TILE_H(54) = 864` tall. 4096 fits two of
/// those across and four down, so ~8 worst-case strips before shelf fragmentation and
/// dozens of the short-media strips that dominate a real project. 8192 would be 256 MiB
/// for a Timeline decoration, which is not a trade this feature gets to make.
///
/// RE-CHECKED AT 1,000 CLIPS (plan 53.2-07) — **unchanged, and the check found the one
/// case where it binds.** The measured per-frame demand is 24 distinct sheets (see
/// [`MAX_RESIDENT_STRIPS`]). At the SHORT-media sizes that dominate a real project all 24
/// fit easily; at the 1536x864 worst case 24 sheets would want 31.9 M texels against this
/// atlas's 16.8 M, so roughly half of them would not be admitted. That is not thrashing —
/// [`AtlasBook`] refuses to evict anything drawn this generation, so a full atlas STOPS
/// ADMITTING and the surplus clips render their flat body fill, which is D-16's own stated
/// fallback and is byte-identical to D-04's degraded state. Doubling to 8192 would close
/// that case at a cost of 256 MiB of VRAM held permanently for a Timeline decoration, and
/// the trade is still refused: the failure mode is a graceful, self-correcting one that
/// resolves as the viewport moves.
pub const ATLAS_SIZE: u32 = 4096;

/// Resident strips, as an ENTRY count beside the area bound above.
///
/// DERIVATION: `PeakCache.MaxEntries = 256` is the SHAPE this borrows, not the number — a
/// packed strip sheet is ~5 MiB against a peak array's few KiB, three orders of magnitude.
/// The number that matters is not the clip count but the **distinct media count**, because
/// D-07 keys residency per MEDIA: 1,000 clips cut from 20 files need 20 entries, not 1,000.
/// 32 covers that with room, and is also more strips than [`ATLAS_SIZE`] can physically
/// hold at the worst case — so for large sheets the area bound binds first and this one is
/// the backstop for many small ones.
///
/// # RE-CHECKED AT 1,000 CLIPS (plan 53.2-07) — the value HELD, and its basis got better
///
/// 53.2-05 set this from an estimate of a typical project and recorded that plan 53.2-07
/// owed it a measurement. The measurement exists now, in
/// `TimelineHotPathGateTests::filmstrip_bearing_clips_do_not_regress_the_1000_clip_build`,
/// and it replaces the estimate with a BOUND:
///
/// * 1,000 clips, 200 of them visible at once, cut from 24 distinct media →
///   **24 distinct `strip_key`s in one frame**, never 200 and never 1,000. D-07's per-MEDIA
///   keying is what collapses the one number into the other, and that is now measured
///   rather than argued.
/// * 24 is not an arbitrary fixture size: it is `FilmstripCache.MaxEntries`, the C# side's
///   own bound, which plan 53.2-06 chose AFTER this constant was written. The client cache
///   is the thing that decides how many distinct strips can reach this crate in one frame
///   at all, so **no frame can ever demand more than 24 entries here**, whatever the
///   project holds.
///
/// So the per-frame demand ceiling is 24 and this bound is 32 — 8 entries of headroom, which
/// is exactly what the two things that outlive a single frame need: a media's PLACEHOLDER
/// key and its STRIP key differ (`^ 0x1`), so a media crossing D-13's placeholder→strip
/// transition briefly occupies two entries, and a scrolling viewport churns the resident set
/// faster than one frame. The property that matters — a frame's whole working set fits, so
/// [`AtlasBook::evict`]'s "never evict what was drawn this generation" rule never has to
/// refuse an admission for a clip that is on screen — holds with room. Lowering this to 24
/// would put the two long-lived cases straight into that refusal; raising it buys nothing
/// the area bound would not veto first.
pub const MAX_RESIDENT_STRIPS: usize = 32;

/// Sheet uploads one frame may perform.
///
/// DERIVATION: `Queue::write_texture` of a 5 MiB sheet happens on the render path. Opening
/// a project makes every visible clip want its strip at once; without a cap that is one
/// frame doing tens of megabytes of PCIe traffic and a visible stall. At 2 per frame a
/// 20-media project is fully resident within 10 frames — under 200 ms at 60 Hz — and no
/// single frame pays more than ~10 MiB. Strips past the cap are simply not resident this
/// frame, which is D-16's stated fallback rather than a new failure mode.
pub const MAX_STRIP_UPLOADS_PER_FRAME: usize = 2;

/// Where one resident sheet lives in the atlas, and how much of it is REAL.
///
/// `completed_tiles` is the count that was actually UPLOADED, which is deliberately not
/// the count the current frame's struct carries: under [`MAX_STRIP_UPLOADS_PER_FRAME`] a
/// grown strip can be told "not this frame", and a caller that clamped tile indices against
/// the frame's newer number would sample rows of the atlas nobody has written yet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SheetUv {
    /// `u0, v0, u1, v1` — the WHOLE sheet's rect, normalised over [`ATLAS_SIZE`].
    pub uv: [f32; 4],
    pub sheet_w: u32,
    pub sheet_h_total: u32,
    /// Tiles actually uploaded into the atlas. **Never the frame's newer number.**
    pub completed_tiles: u32,
}

/// What [`AtlasBook::ensure`] decided for one strip this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Residency {
    /// Resident and current. Sample `uv`; upload nothing.
    Ready(SheetUv),
    /// Resident, but `rows_valid_h` rows must be written at `(dest_x, dest_y)` first.
    Upload {
        sheet: SheetUv,
        dest_x: u32,
        dest_y: u32,
        rows_valid_h: u32,
    },
    /// **Not resident this frame.** The atlas is full of strips already drawn, or the
    /// per-frame upload cap is spent, or the sheet is larger than the atlas. The caller
    /// draws no tiles — D-16's contract, and the same picture as D-04's degraded state.
    Absent,
}

impl Residency {
    pub fn sheet(&self) -> Option<SheetUv> {
        match self {
            Residency::Ready(s) => Some(*s),
            Residency::Upload { sheet, .. } => Some(*sheet),
            Residency::Absent => None,
        }
    }

    pub fn needs_upload(&self) -> bool {
        matches!(self, Residency::Upload { .. })
    }
}

#[derive(Clone, Copy, Debug)]
struct ResidentEntry {
    /// The allocator's handle. Freed EXACTLY ONCE, by [`AtlasBook::evict`], and only via
    /// the value `HashMap::remove` returns.
    alloc_id: AllocId,
    rect_x: u32,
    rect_y: u32,
    uv: [f32; 4],
    /// The dims the allocation was made for. A frame whose geometry disagrees with these
    /// is not the same sheet, so the entry is evicted rather than re-used at a wrong size.
    sheet_w: u32,
    sheet_h_total: u32,
    completed_tiles: u32,
    last_drawn_generation: u64,
}

/// The DEVICE-FREE half of the atlas: who is resident, where, and who goes next.
///
/// Split out from [`FilmstripAtlas`] so every D-16 property is testable with no GPU at
/// all. etagere is pure CPU; the only thing in this feature that genuinely needs a device
/// is `Queue::write_texture`, and that is the ONE thing the wrapper adds.
pub struct AtlasBook {
    allocator: AtlasAllocator,
    resident: HashMap<u64, ResidentEntry>,
    size: u32,
    max_resident: usize,
    max_uploads: usize,
    /// Bumped per frame. An entry's `last_drawn_generation` compared against this is both
    /// the LRU key AND the "was this drawn THIS frame" test that stops intra-frame thrash.
    generation: u64,
    uploads_this_frame: usize,
    evictions: u32,
}

impl AtlasBook {
    pub fn new(size: u32, max_resident: usize, max_uploads: usize) -> Self {
        let size = size.max(1);
        Self {
            allocator: AtlasAllocator::new(size2(size as i32, size as i32)),
            resident: HashMap::new(),
            size,
            max_resident: max_resident.max(1),
            max_uploads,
            generation: 1,
            uploads_this_frame: 0,
            evictions: 0,
        }
    }

    /// Start a frame: bump the LRU clock and refill the upload allowance.
    pub fn begin_frame(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.uploads_this_frame = 0;
    }

    /// Resolve one strip's residency, allocating and evicting as needed.
    ///
    /// `sheet_h_total` is the height of the WHOLE grid, not of the valid rows — the
    /// allocation is sized for the finished strip so D-14's progressive growth re-uploads
    /// into the rectangle it already has instead of re-allocating (and therefore
    /// re-fragmenting) the atlas every time another chunk lands.
    pub fn ensure(
        &mut self,
        key: u64,
        sheet_w: u32,
        sheet_h_total: u32,
        completed_tiles: u32,
        rows_valid_h: u32,
    ) -> Residency {
        // Degenerate or impossible geometry is refused BEFORE the LRU is touched. An
        // oversized sheet can never fit however much is evicted, and a loop that keeps
        // evicting for it would empty the atlas on every frame — a bound that destroys
        // what it is protecting.
        if sheet_w == 0 || sheet_h_total == 0 || sheet_w > self.size || sheet_h_total > self.size {
            return Residency::Absent;
        }

        if let Some(entry) = self.resident.get(&key).copied() {
            if entry.sheet_w == sheet_w && entry.sheet_h_total == sheet_h_total {
                // Touch FIRST: a strip drawn this frame must not become this frame's own
                // eviction victim further down the clip loop.
                if let Some(e) = self.resident.get_mut(&key) {
                    e.last_drawn_generation = self.generation;
                }

                if entry.completed_tiles >= completed_tiles {
                    return Residency::Ready(self.sheet_of(&entry));
                }

                // D-14 growth. Subject to the per-frame cap: if it is spent, hand back the
                // EXISTING uv — stale-but-consistent tiles this frame, refreshed the next.
                // Returning `Absent` here would make a clip that already has good frames
                // flicker back to its body fill, which is worse than a frame of old tiles.
                if self.uploads_this_frame >= self.max_uploads {
                    return Residency::Ready(self.sheet_of(&entry));
                }
                self.uploads_this_frame += 1;

                let grown = {
                    let e = self
                        .resident
                        .get_mut(&key)
                        .expect("the entry was present one statement ago");
                    e.completed_tiles = completed_tiles;
                    *e
                };
                return Residency::Upload {
                    sheet: self.sheet_of(&grown),
                    dest_x: grown.rect_x,
                    dest_y: grown.rect_y,
                    rows_valid_h,
                };
            }

            // The geometry changed under this key. The rectangle is the wrong shape, so
            // the entry is not a cache hit at a different size — it is a stale allocation.
            self.evict(key);
        }

        if self.uploads_this_frame >= self.max_uploads {
            return Residency::Absent;
        }

        // The ENTRY bound, before the area bound: a full map with free area is still full.
        while self.resident.len() >= self.max_resident {
            if !self.evict_least_recently_drawn() {
                return Residency::Absent;
            }
        }

        let want = size2(sheet_w as i32, sheet_h_total as i32);
        let allocation = loop {
            if let Some(a) = self.allocator.allocate(want) {
                break a;
            }
            if !self.evict_least_recently_drawn() {
                return Residency::Absent;
            }
        };

        let rect_x = allocation.rectangle.min.x.max(0) as u32;
        let rect_y = allocation.rectangle.min.y.max(0) as u32;
        let entry = ResidentEntry {
            alloc_id: allocation.id,
            rect_x,
            rect_y,
            // The RETURNED rectangle can be larger than the request — etagere rounds a
            // shelf's height up and may hand back a whole item when the remainder is below
            // its split threshold. The uv must describe the SHEET, not the slot it sits in,
            // or every tile samples a scaled-down copy of the strip.
            uv: uv_of(rect_x, rect_y, sheet_w, sheet_h_total, self.size),
            sheet_w,
            sheet_h_total,
            completed_tiles,
            last_drawn_generation: self.generation,
        };
        self.resident.insert(key, entry);
        self.uploads_this_frame += 1;

        Residency::Upload {
            sheet: self.sheet_of(&entry),
            dest_x: rect_x,
            dest_y: rect_y,
            rows_valid_h,
        }
    }

    /// The resident sheet's uv rect, or `None`. Reads the SAME map eviction removes from,
    /// so a stale uv is not something this type can hold.
    pub fn uv_for(&self, key: u64) -> Option<[f32; 4]> {
        self.resident.get(&key).map(|e| e.uv)
    }

    /// Free one strip. **The only path to `deallocate` in this crate.**
    ///
    /// Returns whether anything was actually freed. The `AllocId` comes out of the value
    /// `remove` returned, so it is deallocated exactly once by construction: a second call
    /// finds nothing to remove and never reaches the allocator.
    pub fn evict(&mut self, key: u64) -> bool {
        match self.resident.remove(&key) {
            Some(entry) => {
                self.allocator.deallocate(entry.alloc_id);
                self.evictions = self.evictions.saturating_add(1);
                true
            }
            None => false,
        }
    }

    /// Drop every strip. Resets the allocator wholesale rather than deallocating id by id —
    /// which is not a shortcut but the safer form: `AtlasAllocator::clear` cannot be a
    /// double free, where a loop over ids can be made into one by a later edit.
    pub fn clear(&mut self) {
        self.resident.clear();
        self.allocator.clear();
    }

    pub fn resident_count(&self) -> usize {
        self.resident.len()
    }

    pub fn evictions(&self) -> u32 {
        self.evictions
    }

    pub fn allocated_area(&self) -> i64 {
        self.allocator.allocated_space() as i64
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn uploads_this_frame(&self) -> usize {
        self.uploads_this_frame
    }

    /// Evict the least-recently-DRAWN strip that was NOT drawn this frame.
    ///
    /// The "not this frame" filter is what keeps a full atlas from thrashing: without it, a
    /// frame carrying more distinct media than the atlas holds would evict clip 1's strip
    /// to make room for clip 40's, then evict clip 40's for clip 41's, uploading megabytes
    /// per frame forever and drawing nothing stable. With it, the frame simply stops
    /// admitting new strips once it is full and the surplus clips fall back — which is
    /// visible, bounded, and self-correcting as the viewport moves.
    ///
    /// Ties break on the key so the choice is deterministic; `HashMap` iteration order is
    /// not, and a test that passed on iteration order would be a test that passes sometimes.
    fn evict_least_recently_drawn(&mut self) -> bool {
        let victim = self
            .resident
            .iter()
            .filter(|(_, e)| e.last_drawn_generation < self.generation)
            .min_by_key(|(k, e)| (e.last_drawn_generation, **k))
            .map(|(k, _)| *k);

        match victim {
            Some(k) => self.evict(k),
            None => false,
        }
    }

    fn sheet_of(&self, entry: &ResidentEntry) -> SheetUv {
        SheetUv {
            uv: entry.uv,
            sheet_w: entry.sheet_w,
            sheet_h_total: entry.sheet_h_total,
            completed_tiles: entry.completed_tiles,
        }
    }
}

/// A pixel rect in the atlas as a normalised uv rect. One place, so the divisor cannot
/// drift between the allocation site and the sampling site.
#[inline]
fn uv_of(x: u32, y: u32, w: u32, h: u32, size: u32) -> [f32; 4] {
    let s = size.max(1) as f32;
    [
        x as f32 / s,
        y as f32 / s,
        (x + w) as f32 / s,
        (y + h) as f32 / s,
    ]
}

/// The atlas proper: [`AtlasBook`] plus the one thing that needs a GPU.
pub struct FilmstripAtlas {
    #[allow(dead_code)] // the view below borrows from it; it owns the VRAM
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    book: AtlasBook,
}

impl FilmstripAtlas {
    /// Allocate the one texture. ONCE per attached renderer, never grown, never a second.
    pub fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rudis-timeline-filmstrip-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Rgba8Unorm and NOT its sRGB sibling, for `quads.wgsl`'s stated reason: the
            // surface is non-sRGB, nothing is linearised anywhere in this crate, and a
            // sampler that decoded here would make every thumbnail darker than the frame
            // the user will export.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rudis-timeline-filmstrip-sampler"),
            // CLAMP, not repeat: a uv rounding error at a tile's edge must show that
            // tile's own edge texel, never wrap to the neighbouring tile in the sheet.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Linear: a 96px-wide cell drawn at ~60px is a downscale, and nearest sampling
            // on a downscale aliases badly on exactly the high-detail frames people notice.
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // wgpu 29 API drift, the same class 52-01 recorded four of: the mipmap filter
            // is its own two-variant type now, not the shared `FilterMode`.
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            texture,
            view,
            sampler,
            book: AtlasBook::new(ATLAS_SIZE, MAX_RESIDENT_STRIPS, MAX_STRIP_UPLOADS_PER_FRAME),
        }
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    pub fn begin_frame(&mut self) {
        self.book.begin_frame();
    }

    /// Make one strip resident, uploading if needed, and hand back what may be sampled.
    ///
    /// `None` means "draw no tiles this frame" — the atlas is full of strips already drawn,
    /// the per-frame upload cap is spent, or the sheet does not fit. Every one of those is
    /// D-16's stated fallback rather than an error.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_resident(
        &mut self,
        queue: &wgpu::Queue,
        key: u64,
        sheet_bytes: &[u8],
        sheet_w: u32,
        sheet_h_total: u32,
        completed_tiles: u32,
        rows_valid_h: u32,
    ) -> Option<SheetUv> {
        match self
            .book
            .ensure(key, sheet_w, sheet_h_total, completed_tiles, rows_valid_h)
        {
            Residency::Absent => None,
            Residency::Ready(sheet) => Some(sheet),
            Residency::Upload {
                sheet,
                dest_x,
                dest_y,
                rows_valid_h,
            } => {
                let need = (sheet_w as usize)
                    .checked_mul(4)
                    .and_then(|row| row.checked_mul(rows_valid_h as usize));

                let Some(need) = need.filter(|n| *n > 0 && *n <= sheet_bytes.len()) else {
                    // The book has already recorded these rows as valid, so leaving the
                    // entry resident would let the next frame sample atlas texels nobody
                    // wrote. Evicting is the only answer that keeps "resident implies
                    // uploaded" true. Unreachable from a validated strip — `filmstrip.rs`
                    // proves the length from the geometry before calling — but the atlas
                    // does not get to assume its caller validated.
                    self.book.evict(key);
                    return None;
                };

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: dest_x,
                            y: dest_y,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &sheet_bytes[..need],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(sheet_w * 4),
                        rows_per_image: Some(rows_valid_h),
                    },
                    wgpu::Extent3d {
                        width: sheet_w,
                        height: rows_valid_h,
                        depth_or_array_layers: 1,
                    },
                );
                Some(sheet)
            }
        }
    }

    /// Drop every resident strip. The GPU texture itself is released by `Drop` at detach —
    /// this resets the BOOKKEEPING without giving up the 64 MiB, which is what a re-attach
    /// or a project close wants.
    pub fn evict_all(&mut self) {
        self.book.clear();
    }

    pub fn resident_strips(&self) -> u32 {
        self.book.resident_count() as u32
    }

    pub fn evictions(&self) -> u32 {
        self.book.evictions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fill until `ensure` refuses. Returns the keys that landed.
    fn fill(book: &mut AtlasBook, w: u32, h: u32) -> Vec<u64> {
        let mut keys = Vec::new();
        for k in 0..512u64 {
            match book.ensure(k, w, h, 1, h) {
                Residency::Absent => return keys,
                _ => keys.push(k),
            }
        }
        panic!("the allocator never filled — a bound that no input reaches is not a bound");
    }

    #[test]
    fn allocation_and_eviction_are_lru() {
        // 1024² holding 512² sheets: the AREA bound binds, not the entry bound.
        let mut book = AtlasBook::new(1024, 32, 64);
        let keys = fill(&mut book, 512, 512);
        assert!(keys.len() >= 2, "fixture must actually fill the atlas");

        // Frame 2: draw everything EXCEPT the first key.
        book.begin_frame();
        for k in &keys[1..] {
            assert!(book.ensure(*k, 512, 512, 1, 512).sheet().is_some());
        }

        // Frame 3: a newcomer. The least-recently-DRAWN key is the one that goes.
        book.begin_frame();
        let newcomer = 9_999u64;
        assert!(
            book.ensure(newcomer, 512, 512, 1, 512).needs_upload(),
            "an eviction must actually make room"
        );
        assert_eq!(book.uv_for(keys[0]), None, "the LRU victim is gone");
        for k in &keys[1..] {
            assert!(book.uv_for(*k).is_some(), "a recently-drawn strip must survive");
        }
        assert_eq!(book.evictions(), 1);

        // Deallocated EXACTLY once: a second evict must not reach the allocator at all.
        // etagere's `deallocate` asserts on a freed id, so a double free is a panic here.
        assert!(!book.evict(keys[0]));

        // ...and the allocator is still sane afterwards: everything frees and refills.
        for k in keys[1..].iter().chain(std::iter::once(&newcomer)) {
            assert!(book.evict(*k));
        }
        assert_eq!(book.resident_count(), 0);
        assert_eq!(book.allocated_area(), 0);
        book.begin_frame();
        assert_eq!(fill(&mut book, 512, 512).len(), keys.len());
    }

    #[test]
    fn a_stale_uv_is_never_returned_after_eviction() {
        let mut book = AtlasBook::new(4096, 32, 8);
        const K: u64 = 0xABCD;
        assert!(book.ensure(K, 512, 512, 1, 512).needs_upload());
        assert!(book.uv_for(K).is_some());

        assert!(book.evict(K));
        assert_eq!(book.uv_for(K), None, "an evicted key has no uv, ever");
        assert!(!book.evict(K), "and evicting it again is a no-op, not a double free");

        book.begin_frame();
        assert!(
            book.ensure(K, 512, 512, 1, 512).needs_upload(),
            "it must be RE-UPLOADED, never resurrected from a stale rect"
        );
    }

    #[test]
    fn resident_count_and_bytes_are_bounded() {
        // Small entry bound, big atlas, tiny sheets: the ENTRY bound binds.
        let mut book = AtlasBook::new(4096, 3, 64);
        for k in 0..3u64 {
            assert!(book.ensure(k, 64, 64, 1, 64).needs_upload());
        }
        assert_eq!(book.resident_count(), 3);

        book.begin_frame();
        assert!(book.ensure(99, 64, 64, 1, 64).needs_upload());
        assert_eq!(book.resident_count(), 3, "the entry bound is a bound");
        assert!(book.evictions() >= 1);

        // The AREA bound holds too, at the real size.
        let mut real = AtlasBook::new(ATLAS_SIZE, MAX_RESIDENT_STRIPS, 4096);
        let keys = fill(&mut real, 1536, 864);
        assert!(real.resident_count() <= MAX_RESIDENT_STRIPS);
        assert!(
            real.allocated_area() <= (ATLAS_SIZE as i64) * (ATLAS_SIZE as i64),
            "allocated area {} exceeds the atlas after {} strips",
            real.allocated_area(),
            keys.len()
        );
    }

    #[test]
    fn growth_triggers_reupload() {
        let mut book = AtlasBook::new(4096, 32, 4);
        const K: u64 = 7;
        let first = book.ensure(K, 1536, 864, 32, 108);
        assert!(first.needs_upload(), "a first sight always uploads");

        book.begin_frame();
        let same = book.ensure(K, 1536, 864, 32, 108);
        assert!(!same.needs_upload(), "the SAME completed count must not re-upload");
        assert!(matches!(same, Residency::Ready(_)));

        book.begin_frame();
        let grown = book.ensure(K, 1536, 864, 64, 216);
        assert!(grown.needs_upload(), "D-14 growth re-uploads the valid rows");
        assert_eq!(
            grown.sheet().unwrap().uv,
            first.sheet().unwrap().uv,
            "growth REUSES the allocation — the rect was sized for the TOTAL grid"
        );
        assert_eq!(grown.sheet().unwrap().completed_tiles, 64);
        assert_eq!(book.evictions(), 0, "growth is not an eviction");
    }

    #[test]
    fn upload_cap_per_frame() {
        let mut book = AtlasBook::new(4096, 32, MAX_STRIP_UPLOADS_PER_FRAME);
        assert_eq!(MAX_STRIP_UPLOADS_PER_FRAME, 2, "this test reads the real constant");

        book.begin_frame();
        assert!(book.ensure(1, 256, 256, 1, 256).needs_upload());
        assert!(book.ensure(2, 256, 256, 1, 256).needs_upload());
        assert_eq!(
            book.ensure(3, 256, 256, 1, 256),
            Residency::Absent,
            "past the cap a strip stays NON-RESIDENT this frame — bounded frame cost"
        );
        assert_eq!(book.resident_count(), 2, "the refused strip took no space either");

        book.begin_frame();
        assert!(
            book.ensure(3, 256, 256, 1, 256).needs_upload(),
            "and it lands on the very next frame"
        );
    }

    #[test]
    fn the_discretion_values_are_the_ones_the_artifact_derives() {
        // Pinned as literals so changing one is a decision that has to come here, and
        // so artifacts/53.2-05-atlas-decisions.md has something to be checked against.
        assert_eq!(ATLAS_SIZE, 4096);
        assert_eq!(MAX_RESIDENT_STRIPS, 32);
        assert_eq!(MAX_STRIP_UPLOADS_PER_FRAME, 2);
    }

    #[test]
    fn a_sheet_larger_than_the_atlas_is_refused_without_emptying_it() {
        // The thrash guard, stated as a test because the failure it prevents looks like
        // a performance mystery rather than a bug: one hostile oversized strip that
        // evicted its way to an empty atlas every frame would blank every OTHER clip.
        let mut book = AtlasBook::new(1024, 32, 64);
        let keys = fill(&mut book, 256, 256);
        let before = book.resident_count();
        assert!(before > 0);

        book.begin_frame();
        assert_eq!(book.ensure(7777, 2048, 2048, 1, 2048), Residency::Absent);
        assert_eq!(book.ensure(7778, 0, 512, 1, 512), Residency::Absent);
        assert_eq!(
            book.resident_count(),
            before,
            "an impossible request must evict NOTHING"
        );
        assert_eq!(book.evictions(), 0);
        assert!(book.uv_for(keys[0]).is_some());
    }

    #[test]
    fn a_strip_drawn_this_frame_is_never_this_frames_eviction_victim() {
        // Without the "not drawn this generation" filter, a frame carrying more distinct
        // media than the atlas holds would evict clip 1's strip for clip 40's and clip
        // 40's for clip 41's, uploading megabytes per frame and drawing nothing stable.
        let mut book = AtlasBook::new(1024, 32, 64);
        let keys = fill(&mut book, 512, 512);
        // Everything in `keys` was ensured in THIS generation, so the atlas is full of
        // strips that must not move. The surplus is refused, not swapped in.
        assert_eq!(book.ensure(4242, 512, 512, 1, 512), Residency::Absent);
        assert_eq!(book.evictions(), 0);
        for k in &keys {
            assert!(book.uv_for(*k).is_some());
        }
    }

    #[test]
    fn the_uv_describes_the_sheet_and_not_the_slot_it_landed_in() {
        // etagere rounds a shelf's height up and may hand back a whole item when the
        // remainder is below its split threshold, so `allocation.rectangle` can be BIGGER
        // than the request. A uv built from the rect's size would scale every tile.
        let mut book = AtlasBook::new(4096, 32, 8);
        let sheet = book.ensure(1, 1536, 864, 256, 864).sheet().unwrap();
        let s = ATLAS_SIZE as f32;
        assert!((sheet.uv[2] - sheet.uv[0] - 1536.0 / s).abs() < 1e-6, "u span is the sheet width");
        assert!((sheet.uv[3] - sheet.uv[1] - 864.0 / s).abs() < 1e-6, "v span is the sheet height");
        assert_eq!(sheet.sheet_w, 1536);
        assert_eq!(sheet.sheet_h_total, 864);
    }

    #[test]
    fn a_capped_growth_reports_the_uploaded_count_not_the_frames_count() {
        // The silent-corruption case this whole `completed_tiles` round trip exists for:
        // if a grown strip is told "not this frame" and the caller clamped tile indices
        // against the FRAME's newer number, it would sample atlas rows nobody has written.
        let mut book = AtlasBook::new(4096, 32, 1);
        book.begin_frame();
        assert!(book.ensure(1, 512, 512, 16, 256).needs_upload());
        // The cap is now spent. The same key, grown.
        let capped = book.ensure(1, 512, 512, 64, 512);
        assert!(!capped.needs_upload(), "the cap holds");
        assert_eq!(
            capped.sheet().unwrap().completed_tiles,
            16,
            "the resident count is what was UPLOADED, never what the frame claims"
        );

        book.begin_frame();
        let landed = book.ensure(1, 512, 512, 64, 512);
        assert!(landed.needs_upload());
        assert_eq!(landed.sheet().unwrap().completed_tiles, 64);
    }

    #[test]
    fn a_geometry_change_under_one_key_evicts_rather_than_reusing_the_wrong_rect() {
        let mut book = AtlasBook::new(4096, 32, 8);
        let a = book.ensure(1, 512, 512, 1, 512).sheet().unwrap();
        book.begin_frame();
        let b = book.ensure(1, 1024, 512, 1, 512).sheet().unwrap();
        assert_eq!(b.sheet_w, 1024);
        assert!(
            (b.uv[2] - b.uv[0]) > (a.uv[2] - a.uv[0]),
            "the new uv must span the new width, not the old allocation"
        );
        assert_eq!(book.evictions(), 1);
        assert_eq!(book.resident_count(), 1);
    }

    #[test]
    fn clear_returns_the_allocator_to_empty() {
        let mut book = AtlasBook::new(1024, 32, 64);
        let keys = fill(&mut book, 512, 512);
        assert!(!keys.is_empty());
        book.clear();
        assert_eq!(book.resident_count(), 0);
        assert_eq!(book.allocated_area(), 0);
        assert_eq!(book.uv_for(keys[0]), None);
        // ...and it is usable again, which a botched reset would not be.
        book.begin_frame();
        assert_eq!(fill(&mut book, 512, 512).len(), keys.len());
    }
}
