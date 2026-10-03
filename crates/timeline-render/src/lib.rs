//! `rudis_timeline` — the Timeline region's renderer (Phase 52, SHELL-05).
//!
//! # What this crate is, and what it deliberately is NOT
//!
//! It is the GPU surface behind the C# shell's `Timeline` region: a second
//! `SwapChainPanel` hosting its own DX12 `wgpu` device, on which clip rectangles, the
//! ruler, the playhead and `glyphon` text are drawn. Plan 52-01 built the hosting core and
//! the D-05 tripwire; plan 52-04 built the renderer proper on top of a GREEN verdict.
//!
//! It is a **SEPARATE cdylib** (`rudis_timeline.dll`) from `rudis_ffi.dll`, on purpose:
//!
//! * Two `wgpu` majors are live in the shell process at once — the frozen 26 that the
//!   shipping video path pins, and the 29 this crate pins so a maintained, licence-safe
//!   text renderer (`glyphon`) is available at all. Separate dynamic libraries mean those
//!   two majors never share a symbol table.
//! * The shell's 23-export C ABI stays exactly what it was. Nothing about rendering
//!   widens it.
//!
//! It links **nothing** from the frozen crates. No `use`, no path dependency, no
//! re-export. The engine-axis freeze (tag `engine-axis-freeze`) allows those crates to be
//! CALLED, never MODIFIED — and this crate does not even call them, because the Timeline
//! draws from the shell's mirrored project state, not from the video pipeline.
//!
//! # Modules
//!
//! * [`surface`] — the `ISwapChainPanelNative` QI, the independent device/swapchain, the
//!   thread rule, and the minimal clear-and-present. Everything else stands on it.
//! * [`frame`] — THE CONTRACT: the `#[repr(C)]` frame/clip/lane/tick/palette/stats structs
//!   both languages compile against, byte-pinned by `tests/layout_canary.rs`.
//! * [`quads`] — the instanced-quad pass and the whole frame's assembly order. Everything
//!   that is not a glyph or a filmstrip tile is an instance in one buffer through one
//!   pipeline.
//! * [`atlas`] — the filmstrip atlas: one fixed 4096² texture, an `etagere` shelf
//!   allocator, and a bounded LRU (53.2 D-16). The renderer owns the texture and the
//!   upload; the C# side only decides which strips it wants (D-12's correction).
//! * [`texquad`] — the textured instanced-quad pipeline. Draws at the seam `quads.rs` cuts
//!   between its fill and overlay ranges.
//! * [`text`] — the `glyphon` wrapper. One font system, one atlas, one renderer, and a
//!   bounded cache of shaped buffers.
//! * [`waveform`] — the audio fill's SKELETON. Plan 52-08 fills it; the seam is cut here so
//!   that plan adds no ABI field and no second pipeline.
//! * [`smoke`] — plan 52-01's tripwire ABI: attach two panels, present continuously,
//!   report per-panel present counters and inter-present delta percentiles so D-05 is
//!   answered by measurement rather than by extrapolating from "one panel worked".
//!
//! # ERROR CODES — the exact values plan 52-06's C# side switches on
//!
//! Every export returns `i32`. Never a bool: "it failed" is not an answer anyone can act
//! on, and the difference between a lost surface and a malformed frame is the difference
//! between reattaching and fixing a bug.
//!
//! | Code | Name | Meaning | What the C# side should do |
//! |------|------|---------|----------------------------|
//! | `0`  | ok | the call succeeded (including a deliberately skipped clean frame) | nothing |
//! | `-1` | null handle | the `RudisTimeline*` was null | a bug in the caller's lifetime handling; log |
//! | `-2` | null/oversized buffer | a pointer was null with a non-zero length, or a length exceeded its array's bound | a bug in the frame builder; log, do not retry the same frame |
//! | `-3` | surface lost | the swapchain is lost or outdated | detach and re-attach; recoverable, never an unhandled exception (T-52-19) |
//! | `-4` | panic caught | a Rust panic was contained at the boundary | log with the stats; the renderer remains usable |
//! | `-5` | not on the UI thread | an attach/resize was attempted off the panel's own thread | dispatch to the panel's `DispatcherQueue` and retry |
//!
//! The table is duplicated in `artifacts/52-04-renderer.md` so the C# side has a source
//! that is not "go read the Rust".

pub mod abi;
pub mod atlas;
pub mod filmstrip;
pub mod frame;
pub mod quads;
pub mod smoke;
pub mod surface;
pub mod text;
pub mod texquad;
pub mod waveform;

/// Success.
pub const RC_OK: i32 = 0;
/// A null `RudisTimeline*`.
pub const RC_NULL_HANDLE: i32 = -1;
/// A null pointer with a non-zero length, or a length past its array's bound.
pub const RC_BAD_BUFFER: i32 = -2;
/// The surface is lost or outdated. Recoverable by re-attaching.
pub const RC_SURFACE_LOST: i32 = -3;
/// A panic was caught at the FFI boundary.
pub const RC_PANIC: i32 = -4;
/// A thread-affine call was made off the panel's UI thread.
pub const RC_WRONG_THREAD: i32 = -5;

/// Version string for the tripwire's transcript, so an artifact can say which build it
/// was measured against.
pub fn version() -> &'static str {
    concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"))
}

/// Opt-in stage trace: set this to a file path and every attach/teardown step is appended
/// and FLUSHED as it happens.
///
/// This exists because the failure a tripwire most needs to diagnose is the one it cannot
/// survive. When a native teardown step access-violates, the managed caller dies inside
/// the P/Invoke and logs nothing — the last line in this file is then the exact call that
/// faulted. Plan 52-01 used it to move "it crashes somewhere in detach" to "it crashes in
/// ISwapChainPanelNative::Release", which is the difference between attributing a fault
/// and guessing at one.
pub const TRACE_ENV: &str = "RUDIS_TIMELINE_SMOKE_TRACE";

/// Append one trace line, flushed immediately. A no-op unless [`TRACE_ENV`] is set.
pub fn trace(stage: &str) {
    let Some(path) = std::env::var_os(TRACE_ENV) else {
        return;
    };
    if path.is_empty() {
        return;
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{stage}");
        let _ = file.flush();
    }
}

/// A whole frame, with every pointer already turned into a bounded, validated slice.
///
/// This type is the T-52-15 mitigation expressed in the TYPE SYSTEM rather than in a
/// convention. [`RudisTimeline::render`] cannot be called without one, and a `FrameView`
/// cannot be built from raw pointers anywhere except `abi.rs`'s single validating
/// constructor. So "no export can skip validation" is not a discipline anyone has to
/// remember — it is what the compiler allows. `crates/ffi` achieves the same property by
/// funnelling every export through `call_out`/`call_json`; this is that idea with the
/// funnel made mandatory.
pub struct FrameView<'a> {
    /// The scalar half of the frame — sizes, scale, the gutter/header/ruler bands, the
    /// playhead x, and `dirty`. Pointers in here are NOT to be read; the slices below are
    /// the validated form of every one of them.
    pub geom: &'a frame::RudisTimelineFrame,
    pub lanes: &'a [frame::RudisTimelineLane],
    pub clips: &'a [frame::RudisTimelineClip],
    pub ticks: &'a [frame::RudisTimelineTick],
    pub ghosts: &'a [frame::RudisTimelineClip],
    pub snap_guides: &'a [f32],
}

/// Where a rendered frame goes.
///
/// A `SwapChainPanel` in production; an owned offscreen texture in the ABI validation
/// suite. The headless arm exists so `tests/abi_validation.rs` can drive the REAL assembly,
/// upload, render-pass and draw path without a window — proving the validation on the code
/// that ships rather than on a mock of it. It is deliberately not a trait: two arms of an
/// enum in one `match` are easier to read than a trait whose only two implementors are
/// "the real one" and "the test one", and impossible to accidentally implement a third
/// time.
enum Target {
    Panel(surface::TimelineSurface),
    Headless(HeadlessTarget),
}

impl Target {
    fn device(&self) -> &wgpu::Device {
        match self {
            Target::Panel(s) => s.device(),
            Target::Headless(h) => &h.device,
        }
    }
    fn queue(&self) -> &wgpu::Queue {
        match self {
            Target::Panel(s) => s.queue(),
            Target::Headless(h) => &h.queue,
        }
    }
    fn format(&self) -> wgpu::TextureFormat {
        match self {
            Target::Panel(s) => s.format(),
            Target::Headless(h) => h.format,
        }
    }
}

/// A real DX12 device with a real colour attachment and no swapchain.
struct HeadlessTarget {
    #[allow(dead_code)] // owns the device's lifetime; never called directly
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    #[allow(dead_code)] // the view below borrows from it
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// One attached Timeline: a target, the two passes, a palette and the counters.
///
/// Owned by the C# side as an opaque `RudisTimeline*` from `rudis_timeline_attach` until
/// `rudis_timeline_detach`.
pub struct RudisTimeline {
    target: Target,
    quads: quads::QuadPass,
    text: text::TextPass,
    waveform: waveform::WaveformPass,
    /// The filmstrip's 64 MiB texture and its bounded-LRU book (53.2 D-16). Allocated ONCE
    /// here, never grown, released by `Drop` at detach — so this feature's VRAM cost is a
    /// structural ceiling rather than a budget somebody has to keep checking.
    atlas: atlas::FilmstripAtlas,
    /// The textured pipeline that samples [`Self::atlas`]. Built AFTER it, because its bind
    /// group names the atlas's view and sampler.
    texquad: texquad::TexQuadPass,
    /// The tile emission and its per-frame counters (D-05/D-06/D-07/D-13).
    filmstrip: filmstrip::FilmstripPass,
    /// `None` until `rudis_timeline_set_palette`. A frame that arrives first draws nothing
    /// rather than inventing colours — this crate has none of its own (CLAUDE.md rule 7).
    palette: Option<quads::Palette>,
    stats: frame::RudisTimelineStats,
    /// Test-only fault injection. See [`Self::inject_panic_for_test`].
    panic_next_render: bool,
}

impl RudisTimeline {
    /// Attach to a `SwapChainPanel`. **Call on the panel's UI thread** — see
    /// [`surface`]'s THREAD RULE.
    ///
    /// `scale` is the panel's `CompositionScaleX`, threaded down to the swapchain's
    /// inverse-scale matrix transform — see
    /// [`surface::TimelineSurface::attach`]. It is load-bearing, not decoration.
    ///
    /// # Safety
    /// `panel` must be a valid COM pointer to a `Microsoft.UI.Xaml.Controls.SwapChainPanel`,
    /// or null.
    pub fn attach(
        panel: *mut std::os::raw::c_void,
        width_px: u32,
        height_px: u32,
        scale: f32,
    ) -> Result<Self, surface::TimelineError> {
        let surface = surface::TimelineSurface::attach(panel, width_px, height_px, scale)?;
        Ok(Self::with_target(Target::Panel(surface)))
    }

    /// **TEST-ONLY.** A renderer with no panel, drawing into an owned offscreen texture.
    ///
    /// Public because integration tests link the rlib and can reach only the public API.
    /// It is NOT an export: no `#[no_mangle] extern "C"` wrapper exists for it, so it is
    /// unreachable from the shell — asserted by
    /// `the_headless_constructor_is_not_reachable_from_the_c_abi`.
    pub fn attach_headless(
        width_px: u32,
        height_px: u32,
    ) -> Result<Self, surface::TimelineError> {
        // Same DX12 pin and the same explicit-fields discipline as `TimelineSurface::attach`
        // (52-01 deviation 5): the backend must not be able to hide inside a
        // `..Default::default()`, and the validation suite must exercise the same backend
        // the shell does or it is testing a different renderer.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|_| surface::TimelineError::NoAdapter)?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rudis-timeline-headless-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|_| surface::TimelineError::NoDevice)?;

        // The same format a real panel negotiates, so the pipelines and the glyphon atlas
        // under test are byte-identical to the shipping ones.
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rudis-timeline-headless-target"),
            size: wgpu::Extent3d {
                width: width_px.max(1),
                height: height_px.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        Ok(Self::with_target(Target::Headless(HeadlessTarget {
            instance,
            device,
            queue,
            format,
            texture,
            view,
        })))
    }

    fn with_target(target: Target) -> Self {
        let format = target.format();
        let quads = quads::QuadPass::new(target.device(), format);
        let text = text::TextPass::new(target.device(), target.queue(), format);
        // ORDER IS LOAD-BEARING: the atlas owns the texture view and sampler the texquad
        // bind group names, so it must exist first. Both are dropped with the renderer at
        // detach, which is where the 64 MiB goes back.
        let atlas = atlas::FilmstripAtlas::new(target.device());
        let texquad = texquad::TexQuadPass::new(target.device(), format, &atlas);
        Self {
            target,
            quads,
            text,
            waveform: waveform::WaveformPass::new(),
            atlas,
            texquad,
            filmstrip: filmstrip::FilmstripPass::new(),
            palette: None,
            stats: frame::RudisTimelineStats::default(),
            panic_next_render: false,
        }
    }

    /// **TEST-ONLY.** Make the next [`Self::render`] panic, so the `catch_unwind` in every
    /// export can be proven to contain it rather than assumed to.
    ///
    /// Per-INSTANCE rather than a global or an environment variable, deliberately: the test
    /// binary runs its cases on parallel threads, and a process-wide switch would make the
    /// panic land in whichever test happened to be rendering. Like
    /// [`Self::attach_headless`], it has no `extern "C"` wrapper and is unreachable from
    /// the shell.
    pub fn inject_panic_for_test(&mut self, on: bool) {
        self.panic_next_render = on;
    }

    /// Reconfigure for a new panel size. **UI thread.**
    ///
    /// A headless target is fixed-size: it has no swapchain to reconfigure, and silently
    /// accepting the call is correct rather than lenient — resize is about the swapchain,
    /// and the validation suite's subject is the boundary, not the swapchain.
    ///
    /// `scale` is threaded through because a DPI change (a monitor move) arrives as a
    /// RESIZE carrying a NEW composition scale, never as a re-attach.
    pub fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) {
        if let Target::Panel(s) = &mut self.target {
            s.resize(width_px, height_px, scale);
        }
    }

    /// Count a frame refused by the dirty gate.
    ///
    /// Lives here rather than in `abi.rs` because the counter is the renderer's state.
    /// `abi.rs` calls it BEFORE building a `FrameView`, so an idle Timeline never walks its
    /// arrays — see `rudis_timeline_render`'s ordering note.
    pub fn note_clean_frame(&mut self) {
        self.stats.skipped_clean_frames = self.stats.skipped_clean_frames.saturating_add(1);
    }

    /// Cache the palette, converted once. See [`quads::Palette`].
    pub fn set_palette(&mut self, p: &frame::RudisTimelinePalette) {
        self.palette = Some(quads::Palette::from_abi(p));
    }

    pub fn stats(&self) -> frame::RudisTimelineStats {
        self.stats
    }

    /// The underlying panel surface, for the detach sequence's steps 2 and 4. `None` for a
    /// headless renderer, which has no panel to unbind.
    pub fn surface(&self) -> Option<&surface::TimelineSurface> {
        match &self.target {
            Target::Panel(s) => Some(s),
            Target::Headless(_) => None,
        }
    }

    /// Render one frame.
    ///
    /// # The dirty gate comes FIRST, before anything touches the GPU
    ///
    /// `dirty == 0` returns immediately, having acquired no surface texture, encoded no
    /// commands, submitted nothing and presented nothing. An idle Timeline therefore costs
    /// one comparison and one increment per tick — which is the phase's criterion-1 redraw
    /// clause implemented where it can actually be honoured, rather than a C#-side promise
    /// not to call.
    ///
    /// `skipped_clean_frames` counts these, so "the Timeline is idle" is a number anyone can
    /// read out of `rudis_timeline_stats` instead of a claim.
    pub fn render(&mut self, view: &FrameView<'_>) -> i32 {
        if view.geom.dirty == 0 {
            self.note_clean_frame();
            return RC_OK;
        }

        // TEST-ONLY fault injection, placed where a real panic would most plausibly
        // originate: inside the frame path, past the cheap gates, with GPU objects live.
        // Proving `catch_unwind` contains a panic from HERE is the property that matters,
        // because unwinding out of an `extern "C"` frame into the CLR is undefined
        // behaviour rather than an exception the managed side can catch (T-52-17).
        if self.panic_next_render {
            panic!("rudis_timeline: injected test panic inside render");
        }

        let started = std::time::Instant::now();

        let Some(palette) = self.palette else {
            // No palette yet: there is nothing legitimate to draw, and picking a colour
            // here would be this crate inventing one. Counted as a clean skip rather than
            // an error — the very first frame can legitimately race the palette upload.
            self.note_clean_frame();
            return RC_OK;
        };

        // ASSEMBLE — no allocation in the steady state. Both passes clear and refill
        // buffers they have owned since the first frame at this size; `QuadPass::upload`
        // grows its GPU buffer by doubling and never shrinks it.
        self.waveform.begin();
        // The atlas's frame clock: bumps the LRU generation and refills the per-frame
        // upload allowance. Must happen before any `ensure_resident`, and exactly once per
        // rendered frame — a clean frame returns above and legitimately does not tick it.
        self.atlas.begin_frame();
        self.texquad.begin();
        self.filmstrip.begin();
        self.quads.assemble(
            view.geom,
            view.lanes,
            view.clips,
            view.ticks,
            view.ghosts,
            view.snap_guides,
            &palette,
            &self.waveform,
        );
        // THE TILES. Emitted here rather than inside `quads::assemble` because they are a
        // different pipeline's instances — but from the SAME clip array, with the SAME
        // inset (`hairline_px`) and the SAME band height the flat anatomy used, so a tile
        // cannot drift off the body rect the label and the waveform agree on.
        //
        // This is also where the atlas's uploads happen, which is why it runs before the
        // surface is acquired: `Queue::write_texture` between an acquire and a present is
        // work inside the frame's own critical section for no reason.
        {
            let hairline = quads::hairline_px(view.geom);
            let band_h = view.geom.band_h_px;
            let (atlas, filmstrip) = (&mut self.atlas, &mut self.filmstrip);
            filmstrip.assemble(
                self.texquad.instances_mut(),
                atlas,
                self.target.queue(),
                view.clips,
                hairline,
                band_h,
            );
        }

        self.text.prepare(
            self.target.device(),
            self.target.queue(),
            view.geom,
            view.lanes,
            view.clips,
            view.ticks,
            &palette,
        );
        self.quads.upload(
            self.target.device(),
            self.target.queue(),
            view.geom.surface_w_px,
            view.geom.surface_h_px,
        );
        self.texquad.upload(
            self.target.device(),
            self.target.queue(),
            view.geom.surface_w_px,
            view.geom.surface_h_px,
        );

        // ACQUIRE. A panel has a swapchain to acquire from and every wgpu outcome to
        // classify; a headless target already owns its colour attachment.
        let acquired = match &self.target {
            Target::Panel(s) => match s.acquire() {
                Ok(frame) => Some(frame),
                Err(surface::TimelineError::Occluded) => {
                    // Minimised or fully covered. wgpu's own guidance is to skip and retry,
                    // and it is not an error — but it is not a presented frame either, so it
                    // is neither counted as one nor logged as a failure.
                    return RC_OK;
                }
                Err(surface::TimelineError::DeviceLost) => {
                    self.stats.device_lost = self.stats.device_lost.saturating_add(1);
                    return RC_SURFACE_LOST;
                }
                Err(_) => {
                    self.stats.present_errors = self.stats.present_errors.saturating_add(1);
                    return RC_SURFACE_LOST;
                }
            },
            Target::Headless(_) => None,
        };

        let owned_view = acquired
            .as_ref()
            .map(|f| f.texture.create_view(&wgpu::TextureViewDescriptor::default()));
        let attachment = match (&owned_view, &self.target) {
            (Some(v), _) => v,
            (None, Target::Headless(h)) => &h.view,
            // Unreachable: a panel target always produced an `acquired`, and every failure
            // above returned. Written as a code rather than an `unwrap` so a future edit
            // that adds a target cannot turn this into a panic inside the render path.
            (None, Target::Panel(_)) => return RC_SURFACE_LOST,
        };

        let mut encoder =
            self.target
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("rudis-timeline-frame"),
                });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rudis-timeline-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: attachment,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Clear to bg-app, the Timeline's own base. Any pixel the lane
                        // bands do not cover is the region's background, not black.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: palette.bg_app[0] as f64,
                            g: palette.bg_app[1] as f64,
                            b: palette.bg_app[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // TWO draw calls over ONE buffer and ONE pipeline, with a seam between them.
            //
            // Range A is the flat fills: lane bands, lane separators, and every clip's body
            // plus D-01's title band. Range B is everything that must paint OVER a clip's
            // content — waveform bars, selection outlines, trim handles, the ruler, snap
            // guides, ghosts, the playhead, and the sticky gutter last.
            self.quads.draw_fills(&mut pass);

            // 53.2-05: the filmstrip textured pass draws HERE — and now it does.
            //
            // This gap is the whole point of the split. Tiles land above the body fills (or
            // the flat colour would cover the frames) and below every overlay (or the frames
            // would cover the selection ring, the waveform, the playhead and the sticky
            // gutter). Both hold by construction: the only place the call FITS is the only
            // place it is correct. It is a THIRD draw call per FRAME, not per clip.
            self.texquad.draw(&mut pass);

            self.quads.draw_overlays(&mut pass);

            // Text strictly after every quad, in the SAME pass: labels are then above their
            // own backgrounds by construction rather than by ordering discipline. That now
            // also means a clip label sits above its own title band and above any tile.
            self.text.render(&mut pass);
        }
        self.target.queue().submit(Some(encoder.finish()));
        if let Some(frame) = acquired {
            frame.present();
        }
        self.text.trim();

        self.stats.frames_rendered = self.stats.frames_rendered.saturating_add(1);
        self.stats.quads_drawn = self.quads.len() as u32;
        self.stats.glyphs_drawn = self.text.glyphs_drawn();
        // Plan 52-08: the audio fill's own two counters. Per-FRAME like `quads_drawn`,
        // not cumulative like `frames_rendered` — the question they answer is "did this
        // frame's waveform draw, and did the budget cut it short", and a running total
        // could not answer either.
        self.stats.waveform_quads_drawn = self.waveform.bars_emitted();
        self.stats.waveform_truncated_clips = self.waveform.truncated_clips();
        // Plan 53.2-05's four. The first two are per-FRAME like `quads_drawn`, because the
        // question they answer is "did this frame's filmstrip draw, and did the budget cut
        // it short". The atlas pair is different in kind and deliberately so: residency is
        // a LEVEL (how many strips are in VRAM right now) and evictions are a CUMULATIVE
        // total, because "the atlas is thrashing" is a question about a trend rather than
        // about one frame, and a per-frame eviction count could not answer it.
        self.stats.filmstrip_quads_drawn = self.filmstrip.quads_drawn();
        self.stats.filmstrip_truncated_clips = self.filmstrip.truncated_clips();
        self.stats.atlas_resident_strips = self.atlas.resident_strips();
        self.stats.atlas_evictions = self.atlas.evictions();
        self.stats.last_render_us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        RC_OK
    }
}

#[cfg(test)]
mod tests {
    /// glyphon is a declared dependency AT TRIPWIRE TIME, not at 52-04 time, so the
    /// dual-major link is proven by the compiler now (assumption A2's build half). This
    /// test exists so that fact is enforced rather than merely intended: if glyphon were
    /// dropped from Cargo.toml the crate would stop compiling here.
    #[test]
    fn glyphon_is_linked_against_this_crates_own_wgpu_major() {
        // Naming a glyphon type that is generic over nothing and needs no device: if
        // glyphon and wgpu disagreed on their wgpu major, this would not compile.
        let cache = glyphon::Cache::new;
        let _ = &cache;
        assert!(super::version().starts_with("timeline-render "));
    }
}
