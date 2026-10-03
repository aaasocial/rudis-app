//! The instanced-quad pass: one flat array in, one draw call out.
//!
//! Every non-text pixel the Timeline draws — lane bands, separators, clip bodies,
//! selection outlines, trim handles, the ruler band and its graduations, snap guides, drag
//! ghosts, the playhead and its diamond cap, and (from plan 52-08) waveform bars — is a
//! [`QuadInstance`] in one buffer, drawn by one `draw_indexed`-free instanced call through
//! one pipeline. There is no object per clip and no per-clip draw call, which is SHELL-05's
//! actual mechanism rather than its description.
//!
//! # No allocation in the steady state
//!
//! The instance `Vec` is cleared (not dropped) each frame and its GPU buffer grows by
//! DOUBLING, never shrinking and never being reallocated per frame. After the first few
//! frames at a given clip count, assembling and uploading a frame allocates nothing at all:
//! `Vec::clear` keeps capacity, `push` into spare capacity does not allocate, and
//! `Queue::write_buffer` into an existing buffer does not either. Growth is the only
//! allocating path and it is bounded by the largest frame ever seen.

use wgpu::util::DeviceExt;

use crate::frame::{
    split_band, RudisTimelineClip, RudisTimelineFrame, RudisTimelineLane, RudisTimelinePalette,
    RudisTimelineTick, FLAG_SELECTED,
};

/// One rectangle. 64 bytes, `bytemuck::Pod`, uploaded verbatim.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct QuadInstance {
    /// `x, y, w, h` in physical px, top-left origin.
    pub rect: [f32; 4],
    /// Interior colour, straight (un-premultiplied) alpha.
    pub fill: [f32; 4],
    /// Border colour. Ignored when `border_px == 0`.
    pub border: [f32; 4],
    /// Corner radius in px. `2` for clip bodies (`radius-clip`, README:229), `0` otherwise.
    pub radius_px: f32,
    /// INNER border width in px. `0` for a plain fill.
    pub border_px: f32,
    /// Padding to a 16-byte-aligned 64-byte stride. Present so the vertex attribute at
    /// offset 48 can be a single `Float32x4` rather than two scalars.
    pub _pad: [f32; 2],
}

/// Fully transparent — the value that means "no border" and "no fill" without a branch.
const TRANSPARENT: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

/// Trim handles are drawn at 60% opacity over the clip body, per this plan's spec.
const TRIM_HANDLE_ALPHA: f32 = 0.6;
/// Drag/trim ghosts are drawn at 50% opacity over the real clips (52-07 fills them).
const GHOST_ALPHA: f32 = 0.5;
/// `SelectionOutlineWidth`, README:160 "2px `accent` outline" — the same number
/// `TimelineMetrics.SelectionOutlineWidth` carries on the C# side (52-03 §1).
const SELECTION_OUTLINE_PX: f32 = 2.0;
/// `PlayheadLineWidth`, README:136 "2px `accent` vertical line + diamond cap".
const PLAYHEAD_LINE_PX: f32 = 2.0;
/// The playhead's diamond cap, drawn in the ruler band. A square this wide rotated 45°
/// would need a second pipeline or a per-instance rotation; a small axis-aligned cap the
/// full height of the ruler band reads identically at 8px and costs one more instance.
const PLAYHEAD_CAP_PX: f32 = 7.0;
/// Snap guides and lane separators are one LOGICAL px, scaled at the call site.
const HAIRLINE_LOGICAL_PX: f32 = 1.0;
/// A major ruler graduation runs the full ruler height; a minor one this fraction of it.
const MINOR_TICK_FRACTION: f32 = 0.45;

/// A palette with every colour converted ONCE, at upload time.
///
/// The conversion is trivial arithmetic, so the point of doing it here is not the cycles —
/// it is that there is exactly one place in the crate where a `u32` becomes a colour, and
/// therefore exactly one place where the sRGB decision documented in `quads.wgsl` can be
/// got wrong. Per-CLIP colours (`fill`/`border`) necessarily convert per clip: they arrive
/// per clip and differ per clip, so there is no earlier moment at which to do it. That is
/// not the redundant per-quad work the rule is about.
#[derive(Clone, Copy, Debug, Default)]
pub struct Palette {
    pub bg_bar: [f32; 4],
    pub bg_app: [f32; 4],
    pub bg_panel: [f32; 4],
    pub border_subtle: [f32; 4],
    pub border_hairline: [f32; 4],
    pub text_primary: [f32; 4],
    pub text_secondary: [f32; 4],
    pub text_faint: [f32; 4],
    pub accent: [f32; 4],
    pub accent_bright: [f32; 4],
    pub clip_audio_fill: [f32; 4],
    pub clip_waveform_line: [f32; 4],
    pub clip_poster: [[f32; 4]; 8],
    /// The raw `u32` form, kept because glyphon wants a packed colour, not a float vector.
    pub raw: RudisTimelinePalette,
}

impl Palette {
    /// Convert the ABI palette once.
    pub fn from_abi(p: &RudisTimelinePalette) -> Self {
        Self {
            bg_bar: unpack(p.bg_bar),
            bg_app: unpack(p.bg_app),
            bg_panel: unpack(p.bg_panel),
            border_subtle: unpack(p.border_subtle),
            border_hairline: unpack(p.border_hairline),
            text_primary: unpack(p.text_primary),
            text_secondary: unpack(p.text_secondary),
            text_faint: unpack(p.text_faint),
            accent: unpack(p.accent),
            accent_bright: unpack(p.accent_bright),
            clip_audio_fill: unpack(p.clip_audio_fill),
            clip_waveform_line: unpack(p.clip_waveform_line),
            clip_poster: [
                unpack(p.clip_poster[0]),
                unpack(p.clip_poster[1]),
                unpack(p.clip_poster[2]),
                unpack(p.clip_poster[3]),
                unpack(p.clip_poster[4]),
                unpack(p.clip_poster[5]),
                unpack(p.clip_poster[6]),
                unpack(p.clip_poster[7]),
            ],
            raw: *p,
        }
    }
}

/// `0xAARRGGBB` → straight-alpha RGBA in the surface's own (sRGB-encoded, NON-linear)
/// space. See `quads.wgsl`'s header for why there is no linearisation step here.
#[inline]
pub fn unpack(argb: u32) -> [f32; 4] {
    const INV: f32 = 1.0 / 255.0;
    [
        ((argb >> 16) & 0xFF) as f32 * INV,
        ((argb >> 8) & 0xFF) as f32 * INV,
        (argb & 0xFF) as f32 * INV,
        ((argb >> 24) & 0xFF) as f32 * INV,
    ]
}

/// The frame's DPI scale, sanitised — non-finite or non-positive degrades to `1.0`.
///
/// Factored out at plan 53.2-05 because a THIRD pass now needs it. The filmstrip's tiles
/// are inset by the same hairline the waveform's bars are, and a filmstrip that computed
/// its own inset would eventually paint over the 1px rule that separates one clip from the
/// next — on some scales only, which is the kind of drift nobody files a bug about.
#[inline]
pub fn sanitised_scale(frame: &RudisTimelineFrame) -> f32 {
    if frame.scale.is_finite() && frame.scale > 0.0 {
        frame.scale
    } else {
        1.0
    }
}

/// One LOGICAL px at this frame's scale, never below one PHYSICAL px. The clip border's
/// width, the lane separator's, and every pass's inset — one definition.
#[inline]
pub fn hairline_px(frame: &RudisTimelineFrame) -> f32 {
    (HAIRLINE_LOGICAL_PX * sanitised_scale(frame)).max(1.0)
}

/// The same colour at a different opacity, for the handles/ghosts/guides that are drawn
/// translucent over what is beneath them.
#[inline]
fn with_alpha(mut c: [f32; 4], alpha: f32) -> [f32; 4] {
    c[3] *= alpha;
    c
}

/// Is every component of this rectangle usable?
///
/// # WHICH LAYER OWNS WHICH CHECK — stated once, so neither layer skips it assuming the
/// other did
///
/// * **`abi.rs` owns POINTERS and LENGTHS.** Null checks, per-array maximum bounds, and
///   UTF-8 validity happen at the boundary, BEFORE any slice exists, because a bad pointer
///   or length is a memory-safety problem and there is no safe way to observe it later.
/// * **This module owns GEOMETRY.** Non-finite and non-positive `f32`s are filtered HERE,
///   where quads are built, because a `NaN` width is not a memory-safety problem — it is a
///   drawing problem, and the correct response is a skipped quad in an otherwise complete
///   frame rather than a rejected frame. Rejecting the whole frame would turn one bad clip
///   into a blank Timeline.
///
/// The skip is visible in the stats: `quads_drawn` counts what was emitted, so a filtered
/// quad shows up as a smaller number rather than as nothing at all.
#[inline]
fn rect_is_drawable(x: f32, y: f32, w: f32, h: f32) -> bool {
    x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0
}

/// Push one rectangle into a CALLER-OWNED instance list — **the single place in this crate
/// where a rectangle becomes a [`QuadInstance`]**.
///
/// Everything else funnels here: [`push_fill_into`], [`push_clip_into`], and all three of
/// [`QuadPass`]'s own push methods. That is deliberate rather than tidy — the geometry
/// filter and the radius/border sanitisation below are the crate's only defence against a
/// `NaN` reaching a vertex buffer, and a second construction site is a second place for
/// that defence to be forgotten.
pub fn push_into(
    out: &mut Vec<QuadInstance>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fill: [f32; 4],
    border: [f32; 4],
    radius_px: f32,
    border_px: f32,
) -> bool {
    if !rect_is_drawable(x, y, w, h) {
        return false;
    }
    let radius_px = if radius_px.is_finite() && radius_px > 0.0 {
        radius_px
    } else {
        0.0
    };
    let border_px = if border_px.is_finite() && border_px > 0.0 {
        border_px
    } else {
        0.0
    };
    out.push(QuadInstance {
        rect: [x, y, w, h],
        fill,
        border,
        radius_px,
        border_px,
        _pad: [0.0, 0.0],
    });
    true
}

/// Push a plain filled rectangle into a CALLER-OWNED instance list, applying exactly the
/// geometry filter [`QuadPass::push`] applies.
///
/// Exists so the waveform pass (plan 52-08) can emit bars into the same buffer and the same
/// draw call without owning a `QuadPass` — its bar loop is pure math over a byte slice and is
/// unit-tested with no GPU device at all. [`QuadPass::push_fill`] delegates here, so there is
/// exactly ONE place a plain fill becomes an instance and the two cannot drift.
#[inline]
pub fn push_fill_into(
    out: &mut Vec<QuadInstance>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fill: [f32; 4],
) -> bool {
    push_into(out, x, y, w, h, fill, TRANSPARENT, 0.0, 0.0)
}

/// Push one clip's FLAT ANATOMY — the body fill and D-01's title band — into a CALLER-OWNED
/// instance list. Returns the number of instances emitted (0, 1 or 2).
///
/// # The two quads, and why the first one is the WHOLE clip rather than the body alone
///
/// 1. **The clip quad**: the full rect, with `fill`, the `border` ring and the clip radius —
///    byte-for-byte what this crate emitted before Phase 53.2. It carries the clip's
///    silhouette: the rounded corners and the 1px rule that separates one clip from the next.
/// 2. **The band quad**: the top `band_h_px` of that same rect, in `band_fill`, at the same
///    radius and with no border of its own.
///
/// Emitting the body as `body_rect` instead would have meant giving BOTH rectangles their
/// own border and radius, which puts a rounded notch in the middle of every clip and a
/// doubled rule at the band seam. Painting the band OVER the top of the intact clip quad
/// costs one overdrawn 14px strip and keeps the silhouette exactly as the handoff draws it.
/// The body region is still addressable — [`split_band`] returns it, and the waveform, the
/// label and plan 53.2-05's tiles all take it from there rather than from this function.
///
/// # `band_h_px <= 0` is the back-compat path and emits exactly ONE quad
///
/// Not as a special case bolted on, but because [`split_band`] returns a zero-height band
/// and a zero-height rect is filtered by the same geometry check every other quad passes
/// through. An older C# build sending a zeroed struct tail gets the pre-phase picture.
///
/// # The band is NOT inset by the border, deliberately
///
/// A 2px-wide clip at a deep zoom-out would otherwise have a band `2 - 2*border` px wide —
/// which is to say none at all, exactly at the width where D-04 says the band is the last
/// thing standing between a clip and invisibility. So the band spans the clip's full width
/// and paints over the top of its border, which is what a title bar looks like anyway.
pub fn push_clip_into(
    out: &mut Vec<QuadInstance>,
    clip: &RudisTimelineClip,
    band_h_px: f32,
    radius_px: f32,
    border_px: f32,
) -> u32 {
    let mut emitted = 0u32;

    if push_into(
        out,
        clip.x_px,
        clip.y_px,
        clip.w_px,
        clip.h_px,
        unpack(clip.fill),
        unpack(clip.border),
        radius_px,
        border_px,
    ) {
        emitted += 1;
    }

    let (band, _body) = split_band(clip.x_px, clip.y_px, clip.w_px, clip.h_px, band_h_px);
    // `0` means "no band colour was resolved" rather than "transparent black": the C# side
    // sends a token or it sends nothing, and a band that merges with the body is a far
    // better unwired state than a hole punched through the clip.
    let band_colour = if clip.band_fill != 0 {
        unpack(clip.band_fill)
    } else {
        unpack(clip.fill)
    };
    if push_into(
        out,
        band[0],
        band[1],
        band[2],
        band[3],
        band_colour,
        TRANSPARENT,
        radius_px,
        0.0,
    ) {
        emitted += 1;
    }

    emitted
}

/// The instanced-quad pipeline, its static unit quad, and its reused instance buffer.
pub struct QuadPass {
    pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    unit_quad: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    /// Capacity of `instance_buffer`, in instances. Grown by doubling.
    instance_capacity: usize,
    /// The CPU-side instance list. Cleared per frame, capacity retained forever.
    instances: Vec<QuadInstance>,
    /// Where the FILLS end and the OVERLAYS begin — the boundary plan 53.2-05's textured
    /// filmstrip pass draws between. See [`QuadPass::draw_fills`].
    fills_end: usize,
}

/// The unit quad as a triangle strip: (0,0) (1,0) (0,1) (1,1).
const UNIT_QUAD: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]];

/// Instances the buffer starts with. Small enough to cost nothing, large enough that a
/// typical frame never grows it more than a couple of times.
const INITIAL_INSTANCE_CAPACITY: usize = 512;

impl QuadPass {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rudis-timeline-quads"),
            source: wgpu::ShaderSource::Wgsl(include_str!("quads.wgsl").into()),
        });

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rudis-timeline-quad-globals-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rudis-timeline-quad-globals"),
            size: 16, // vec2<f32> resolution + vec2<f32> pad
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rudis-timeline-quad-globals-bind-group"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        // wgpu 29 API drift, same class as the four 52-01 recorded: `bind_group_layouts`
        // takes `Option<&BindGroupLayout>` (so a gap in the binding indices is expressible)
        // and `push_constant_ranges` became `immediate_size`. Named explicitly rather than
        // defaulted, for the reason 52-01 gave about `InstanceDescriptor`: a pinned value
        // that can hide inside `..Default::default()` is a pinned value that can be lost.
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rudis-timeline-quad-pipeline-layout"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rudis-timeline-quad-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[
                    // 0: the static unit quad, per VERTEX.
                    wgpu::VertexBufferLayout {
                        array_stride: 8,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        }],
                    },
                    // 1: the dynamic instance buffer, per INSTANCE.
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<QuadInstance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 0,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 16,
                                shader_location: 2,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 32,
                                shader_location: 3,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 48,
                                shader_location: 4,
                            },
                        ],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // STRAIGHT alpha, matching what the fragment stage returns. See
                    // quads.wgsl's header for the sRGB-space blending decision this pairs
                    // with.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // No culling: the quad's winding depends on nothing the caller controls,
                // and a culled UI rectangle is a blank region nobody can debug.
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            // wgpu 29 renamed `multiview` to `multiview_mask` — the same drift 52-01 hit on
            // `RenderPassDescriptor`.
            multiview_mask: None,
            cache: None,
        });

        let unit_quad = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rudis-timeline-unit-quad"),
            contents: bytemuck::cast_slice(&UNIT_QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rudis-timeline-quad-instances"),
            size: (INITIAL_INSTANCE_CAPACITY * std::mem::size_of::<QuadInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            globals_buffer,
            globals_bind_group,
            unit_quad,
            instance_buffer,
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
            instances: Vec::with_capacity(INITIAL_INSTANCE_CAPACITY),
            fills_end: 0,
        }
    }

    /// Number of instances the last assembly produced.
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Start a frame. Keeps capacity — this is the whole reason the steady state does not
    /// allocate.
    pub fn begin(&mut self) {
        self.instances.clear();
        self.fills_end = 0;
    }

    /// The index at which the FILL range ends and the OVERLAY range begins.
    ///
    /// Exposed so plan 53.2-05 can address the seam it draws into, and so a test can assert
    /// the split is where the frame assembly says it is rather than where it looks like it
    /// should be.
    pub fn fills_end(&self) -> usize {
        self.fills_end
    }

    /// Push one rectangle, filtering non-finite and degenerate geometry.
    ///
    /// Returns `true` if the quad was emitted, so callers that need to know (the waveform
    /// pass) can, and so the stats counter is the truth rather than the intent.
    #[inline]
    pub fn push(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        fill: [f32; 4],
        border: [f32; 4],
        radius_px: f32,
        border_px: f32,
    ) -> bool {
        push_into(
            &mut self.instances,
            x,
            y,
            w,
            h,
            fill,
            border,
            radius_px,
            border_px,
        )
    }

    /// Push a plain filled rectangle with no border and square corners — the common case.
    #[inline]
    pub fn push_fill(&mut self, x: f32, y: f32, w: f32, h: f32, fill: [f32; 4]) -> bool {
        push_fill_into(&mut self.instances, x, y, w, h, fill)
    }

    /// The CPU-side instance list, for a pass that builds its own quads.
    ///
    /// The waveform fill is the one caller (plan 52-08) and it is why this exists: bars are
    /// quads, so they belong in this buffer and this draw call rather than in a second
    /// pipeline. Exposed as the list rather than as a per-bar method so the bar loop can be
    /// a free function with no GPU handle in sight — which is what makes it testable.
    pub fn instances_mut(&mut self) -> &mut Vec<QuadInstance> {
        &mut self.instances
    }

    /// Push an OUTLINE — a border with no fill. Used for the selection ring, which must not
    /// repaint over the clip body it surrounds.
    #[inline]
    pub fn push_outline(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        border: [f32; 4],
        radius_px: f32,
        border_px: f32,
    ) -> bool {
        self.push(x, y, w, h, TRANSPARENT, border, radius_px, border_px)
    }

    /// Upload the assembled instances, growing the GPU buffer by DOUBLING if needed.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_w: u32,
        surface_h: u32,
    ) {
        let globals: [f32; 4] = [surface_w.max(1) as f32, surface_h.max(1) as f32, 0.0, 0.0];
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::cast_slice(&globals));

        if self.instances.is_empty() {
            return;
        }

        if self.instances.len() > self.instance_capacity {
            // Double until it fits. Never shrinks: a Timeline that once showed 1,000 clips
            // will show them again, and giving the memory back only to re-take it next
            // frame is the reallocation this design exists to avoid.
            let mut cap = self.instance_capacity.max(1);
            while cap < self.instances.len() {
                cap *= 2;
            }
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rudis-timeline-quad-instances"),
                size: (cap * std::mem::size_of::<QuadInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = cap;
        }

        queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&self.instances));
    }

    /// Bind the pipeline and both vertex buffers. Cheap, and called once per draw range.
    fn bind(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind_group, &[]);
        pass.set_vertex_buffer(0, self.unit_quad.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
    }

    /// Draw range A: lane bands, lane separators, and every clip's body fill + title band.
    ///
    /// # This is one half of a STRUCTURAL ordering guarantee, not a convention
    ///
    /// Plan 53.2-05's textured filmstrip tiles must land ABOVE the flat body fills and
    /// BELOW every overlay — above, or the body colour paints over the frames; below, or
    /// the tiles paint over the selection ring, the trim handles, the ruler, the playhead
    /// and the sticky gutter. A comment saying "draw the tiles here" would be a comment.
    /// Two draw calls with a documented seam between them is a place.
    ///
    /// The instances are ONE buffer through ONE pipeline either way — the split is a pair
    /// of ranges into the same `instance_buffer`, not a second pass, so SHELL-05's
    /// "no object per clip, no draw call per clip" is untouched: this is two draw calls per
    /// FRAME, whether the frame carries one clip or a thousand.
    pub fn draw_fills(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.fills_end == 0 {
            return;
        }
        self.bind(pass);
        pass.draw(0..4, 0..self.fills_end as u32);
    }

    /// Draw range B: the waveform bars, selection outlines, trim handles, the ruler band and
    /// its graduations, snap guides, drag ghosts, the playhead, and the gutter LAST.
    ///
    /// Everything here paints over the filmstrip on purpose. The gutter in particular is
    /// why the ruler and the gutter could not simply move into range A: `TrackHeader` is
    /// sticky horizontally (README:138), so a clip scrolled underneath it — and therefore
    /// that clip's tiles — must disappear beneath it rather than beside it.
    pub fn draw_overlays(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.instances.len() <= self.fills_end {
            return;
        }
        self.bind(pass);
        pass.draw(0..4, self.fills_end as u32..self.instances.len() as u32);
    }

    /// Assemble a whole Timeline frame, BACK TO FRONT, from the flat arrays.
    ///
    /// The order below is the drawing order and it is load-bearing in three places:
    ///
    /// * **Selection outlines after clip bodies** — an outline drawn first would be
    ///   overpainted by its own clip.
    /// * **The ruler band after the lanes** — the ruler is sticky (README:138) and must
    ///   cover anything that scrolls under it.
    /// * **The gutter LAST** — `TrackHeader` is sticky horizontally (README:138), so a
    ///   horizontally-scrolled clip must disappear beneath it rather than beside it.
    ///
    /// # The FILL / OVERLAY seam (plan 53.2-03)
    ///
    /// Steps 1-3 are the FILLS and steps 4-12 are the OVERLAYS; `fills_end` is recorded
    /// between them and [`Self::draw_fills`] / [`Self::draw_overlays`] turn that index into
    /// two draw calls with a documented gap for 53.2-05's textured tiles.
    ///
    /// Step 3b (the waveform) moved OUT of the clip loop to land on the overlay side of
    /// that seam, and that is a real ordering decision rather than a tidy-up: bars are clip
    /// CONTENT, not body fill, so a filmstrip drawn over them would erase the waveform of
    /// every audio clip. The relocation is safe because clips on one lane cannot overlap
    /// and lanes cannot overlap each other, so a bar can only ever land on its own clip's
    /// body whichever order the two loops run in — the emitted quad COUNT is identical.
    ///
    /// Text is not here: glyphon renders in the same pass but strictly after every quad,
    /// so labels are always on top of their own backgrounds by construction rather than by
    /// ordering discipline. See `text.rs`.
    pub fn assemble(
        &mut self,
        frame: &RudisTimelineFrame,
        lanes: &[RudisTimelineLane],
        clips: &[RudisTimelineClip],
        ticks: &[RudisTimelineTick],
        ghosts: &[RudisTimelineClip],
        snap_guides: &[f32],
        palette: &Palette,
        waveform: &crate::waveform::WaveformPass,
    ) {
        self.begin();

        let surface_w = frame.surface_w_px.max(1) as f32;
        let surface_h = frame.surface_h_px.max(1) as f32;
        let scale = sanitised_scale(frame);
        let hairline = hairline_px(frame);
        let gutter_w = if frame.gutter_w_px.is_finite() {
            frame.gutter_w_px.max(0.0)
        } else {
            0.0
        };
        let ruler_h = if frame.ruler_h_px.is_finite() {
            frame.ruler_h_px.max(0.0)
        } else {
            0.0
        };
        let header_h = if frame.header_h_px.is_finite() {
            frame.header_h_px.max(0.0)
        } else {
            0.0
        };

        // 1. LANE BANDS — alternating bg-app / bg-panel by kind, so a video lane and the
        //    audio lane beneath it are separable without a heavy rule between them.
        for lane in lanes {
            let band = if lane.kind == 1 {
                palette.bg_panel
            } else {
                palette.bg_app
            };
            self.push_fill(0.0, lane.y_px, surface_w, lane.h_px, band);
        }

        // 2. LANE SEPARATORS — border-subtle, the handoff's stated use (README:197).
        for lane in lanes {
            self.push_fill(
                0.0,
                lane.y_px + lane.h_px - hairline,
                surface_w,
                hairline,
                palette.border_subtle,
            );
        }

        // 3. CLIP BODIES + their borders + D-01's TITLE BAND, radius-clip = 2
        //    (README:229), scaled. The band is drawn on EVERY clip regardless of lane kind
        //    (D-03) — one clip anatomy across both. What differs between a video clip and
        //    an audio clip is only what fills the body BELOW the band, and that is entirely
        //    the C# side's choice of field values, not a branch in here.
        let radius_clip = 2.0 * scale;
        let band_h = frame.band_h_px;
        for clip in clips {
            push_clip_into(&mut self.instances, clip, band_h, radius_clip, hairline);
        }

        // ── THE SEAM. Everything above is a FILL; everything below is an OVERLAY. ──
        //
        // 53.2-05: the filmstrip textured pass draws HERE — between these two draw ranges,
        // so tiles sit above the body fills and below every overlay. It is a real index
        // into a real buffer (`draw_fills` stops at it, `draw_overlays` starts from it),
        // not a comment marking a spot someone will have to find again.
        self.fills_end = self.instances.len();

        // 3b. WAVEFORM FILL (plan 52-08) — into THIS buffer and this pipeline, which is
        //     why it cost no ABI change and no second pass. Now on the OVERLAY side of the
        //     seam (plan 53.2-03): bars are clip CONTENT, so a filmstrip drawn over them
        //     would erase the waveform of every audio clip. Still before the selection
        //     outline below, so bars never paint over the selection ring, and still inset
        //     by the clip's own 1px border so a bar cannot bleed onto the line that
        //     separates one clip from the next. `band_h` starts the bars BELOW the band.
        for clip in clips {
            waveform.push_bars(self, clip, palette, hairline, band_h);
        }

        // 4. SELECTION OUTLINE — accent, 2px, only for flags & FLAG_SELECTED.
        for clip in clips {
            if clip.flags & FLAG_SELECTED != 0 {
                self.push_outline(
                    clip.x_px,
                    clip.y_px,
                    clip.w_px,
                    clip.h_px,
                    palette.accent,
                    radius_clip,
                    SELECTION_OUTLINE_PX * scale,
                );
            }
        }

        // 5. TRIM HANDLES — both edges, `trim_handle_px` wide, accent-bright at 60%.
        //    The width comes across the ABI rather than being recomputed here, so the
        //    DRAWN handle and the C# hit tester's ZONE cannot disagree (52-03 §3.4).
        let handle_colour = with_alpha(palette.accent_bright, TRIM_HANDLE_ALPHA);
        for clip in clips {
            let hw = clip.trim_handle_px;
            if !hw.is_finite() || hw <= 0.0 {
                continue;
            }
            // Never wider than half the clip: two handles that met in the middle would
            // paint over the whole body.
            let hw = hw.min(clip.w_px * 0.5);
            self.push_fill(clip.x_px, clip.y_px, hw, clip.h_px, handle_colour);
            self.push_fill(
                clip.x_px + clip.w_px - hw,
                clip.y_px,
                hw,
                clip.h_px,
                handle_colour,
            );
        }

        // 6. RULER BAND — bg-bar, sticky, drawn over whatever scrolled under it.
        self.push_fill(0.0, header_h, surface_w, ruler_h, palette.bg_bar);

        // 7. RULER GRADUATIONS — border-strong for major, border-hairline for minor.
        //    `border-strong` has no field of its own in the palette: the handoff uses it
        //    for outlined controls and slider tracks (README:196), none of which the
        //    Timeline draws, and inventing a thirteenth palette slot for one tick colour
        //    would put a token in the ABI that the C# side has no reason to resolve.
        //    `text-faint` is the tag/graduation colour the handoff already assigns to this
        //    region (README:201), so a major tick takes it and a minor one takes
        //    `border-hairline`.
        let minor_h = ruler_h * MINOR_TICK_FRACTION;
        for tick in ticks {
            let (colour, h) = if tick.major != 0 {
                (palette.text_faint, ruler_h)
            } else {
                (palette.border_hairline, minor_h)
            };
            self.push_fill(tick.x_px, header_h + ruler_h - h, hairline, h, colour);
        }

        // 8. SNAP GUIDES — accent-bright hairlines spanning the lane stack (52-07 fills).
        let lane_top = header_h + ruler_h;
        let lane_h = (surface_h - lane_top).max(0.0);
        for x in snap_guides {
            self.push_fill(*x, lane_top, hairline, lane_h, palette.accent_bright);
        }

        // 9. GHOST CLIPS — the drag/trim preview, 50% alpha over the real clips (52-07).
        //
        //    Deliberately UNCHANGED by plan 53.2-03: a ghost gets no band and no tiles.
        //    Whether the filmstrip survives inside the drag ghost is a question
        //    53.2-CONTEXT parks explicitly ("raised at the close of discussion and NOT
        //    discussed"), and a ghost previews a POSITION rather than a content. Giving it
        //    a band here would answer a parked question by accident.
        for ghost in ghosts {
            self.push(
                ghost.x_px,
                ghost.y_px,
                ghost.w_px,
                ghost.h_px,
                with_alpha(unpack(ghost.fill), GHOST_ALPHA),
                with_alpha(unpack(ghost.border), GHOST_ALPHA),
                radius_clip,
                hairline,
            );
        }

        // 10. PLAYHEAD — a 2px accent line spanning the lanes plus a diamond cap in the
        //     ruler band (README:136). A non-finite x draws nothing at all rather than a
        //     plausible-looking line at zero.
        if frame.playhead_x_px.is_finite() {
            let line_w = PLAYHEAD_LINE_PX * scale;
            let x = frame.playhead_x_px - line_w * 0.5;
            self.push_fill(x, lane_top, line_w, lane_h, palette.accent);

            let cap = PLAYHEAD_CAP_PX * scale;
            self.push(
                frame.playhead_x_px - cap * 0.5,
                header_h + ruler_h - cap,
                cap,
                cap,
                palette.accent,
                TRANSPARENT,
                cap * 0.5,
                0.0,
            );
        }

        // 11. THE GUTTER, LAST — bg-bar over the lane area, plus its right-hand rule.
        //     `TrackHeader` is sticky horizontally (README:138), so this must cover
        //     everything that scrolled beneath it.
        if gutter_w > 0.0 {
            self.push_fill(0.0, lane_top, gutter_w, lane_h, palette.bg_bar);
            self.push_fill(
                gutter_w - hairline,
                lane_top,
                hairline,
                lane_h,
                palette.border_subtle,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_instance_is_sixty_four_bytes_with_the_params_vec4_at_forty_eight() {
        // The vertex attribute table above hard-codes offsets 0/16/32/48 and a 64-byte
        // stride. If the struct ever drifts from that, the shader silently reads the wrong
        // fields — a class of bug that renders SOMETHING, which is worse than rendering
        // nothing.
        assert_eq!(std::mem::size_of::<QuadInstance>(), 64);
        assert_eq!(std::mem::offset_of!(QuadInstance, rect), 0);
        assert_eq!(std::mem::offset_of!(QuadInstance, fill), 16);
        assert_eq!(std::mem::offset_of!(QuadInstance, border), 32);
        assert_eq!(std::mem::offset_of!(QuadInstance, radius_px), 48);
        assert_eq!(std::mem::offset_of!(QuadInstance, border_px), 52);
    }

    #[test]
    fn unpack_reads_argb_and_does_not_linearise() {
        // Opaque white and fully transparent black are the two ends; the middle value
        // proves the channel ORDER (a linearising conversion would also move 0.5 to ~0.21,
        // which is exactly the silent-gamma bug quads.wgsl's header is about).
        assert_eq!(unpack(0xFFFF_FFFF), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(unpack(0x0000_0000), [0.0, 0.0, 0.0, 0.0]);
        let mid = unpack(0x8040_2010);
        assert!((mid[0] - 0x40 as f32 / 255.0).abs() < 1e-6, "red");
        assert!((mid[1] - 0x20 as f32 / 255.0).abs() < 1e-6, "green");
        assert!((mid[2] - 0x10 as f32 / 255.0).abs() < 1e-6, "blue");
        assert!((mid[3] - 0x80 as f32 / 255.0).abs() < 1e-6, "alpha");
    }

    #[test]
    fn non_finite_and_degenerate_geometry_is_filtered_not_drawn() {
        assert!(!rect_is_drawable(f32::NAN, 0.0, 10.0, 10.0));
        assert!(!rect_is_drawable(0.0, f32::NAN, 10.0, 10.0));
        assert!(!rect_is_drawable(0.0, 0.0, f32::INFINITY, 10.0));
        assert!(!rect_is_drawable(0.0, 0.0, 10.0, f32::NEG_INFINITY));
        assert!(!rect_is_drawable(0.0, 0.0, 0.0, 10.0), "zero width");
        assert!(!rect_is_drawable(0.0, 0.0, 10.0, -1.0), "negative height");
        assert!(rect_is_drawable(-50.0, -50.0, 10.0, 10.0), "offscreen is fine");
    }

    #[test]
    fn with_alpha_scales_rather_than_replaces() {
        // A half-transparent accent at 60% must land at 30%, not at 60% — the handles are
        // drawn over a clip body and a replace would make a translucent token opaque.
        let c = with_alpha([1.0, 0.5, 0.25, 0.5], 0.6);
        assert_eq!(c[0], 1.0);
        assert!((c[3] - 0.3).abs() < 1e-6);
    }

    // ========================================================================
    // D-01/D-03/D-04 — the clip anatomy, with no GPU anywhere in it
    // ========================================================================

    /// 14 LOGICAL px at a 1.25 display scale — D-01's real number, not a sentinel, so the
    /// geometry these tests assert is the geometry the shell actually sends.
    const BAND_H: f32 = 17.5;

    /// A clip carrying only the fields the flat anatomy reads. Zeroed, so any field these
    /// tests do not name is provably not consulted.
    fn anatomy_clip(x: f32, y: f32, w: f32, h: f32) -> RudisTimelineClip {
        let mut c: RudisTimelineClip = unsafe { std::mem::zeroed() };
        c.x_px = x;
        c.y_px = y;
        c.w_px = w;
        c.h_px = h;
        c.fill = 0x1122_3344;
        c.border = 0x5566_7788;
        c.band_fill = 0x99AA_BBCC;
        c
    }

    #[test]
    fn the_band_always_draws_at_any_width() {
        // 2px is BELOW any plausible D-04 body floor, and that is the point: the body is
        // what degrades, and the band is what is still there when it has. A clip the user
        // has zoomed down to a sliver must still read as a clip.
        for w in [2.0f32, 10.0, 24.0, 1000.0] {
            let clip = anatomy_clip(100.0, 200.0, w, 48.0);
            let mut out = Vec::new();
            let emitted = push_clip_into(&mut out, &clip, BAND_H, 2.5, 1.25);

            assert_eq!(emitted, 2, "width {w}: a body quad AND a band quad");
            assert_eq!(out.len(), 2);

            let band = out[1].rect;
            assert_eq!(band[0], 100.0, "width {w}: band starts at the clip's left edge");
            assert_eq!(band[1], 200.0, "width {w}: band starts at the clip's top edge");
            assert_eq!(
                band[2], w,
                "width {w}: the band spans the clip's FULL width — inset by the border it \
                 would vanish at exactly the width D-04 exists for"
            );
            assert_eq!(band[3], BAND_H, "width {w}: full band height");

            // The colour is the C#-resolved token, never anything this crate invented.
            assert_eq!(out[1].fill, unpack(clip.band_fill), "width {w}");
            assert_eq!(out[1].border_px, 0.0, "width {w}: the band carries no border");
        }
    }

    #[test]
    fn a_degenerate_clip_is_all_band() {
        // A clip shorter than the band itself — reachable through a lane height the C#
        // side scaled down, and the case where a naive `h - band_h` produces a NEGATIVE
        // body and a rectangle drawn upwards through the lane above.
        let clip = anatomy_clip(0.0, 0.0, 120.0, 10.0);
        let mut out = Vec::new();
        let emitted = push_clip_into(&mut out, &clip, BAND_H, 2.0, 1.0);

        assert_eq!(emitted, 2);
        assert_eq!(
            out[1].rect[3], 10.0,
            "the band clamps to the clip's OWN height, it does not overhang it"
        );

        let (band, body) = split_band(clip.x_px, clip.y_px, clip.w_px, clip.h_px, BAND_H);
        assert_eq!(band[3], 10.0);
        assert_eq!(body[3], 0.0, "no body height is left");

        // ...and therefore ZERO body content: the waveform's inner rect is not drawable,
        // so nothing is emitted into a body that does not exist. Asserted through the
        // renderer's own geometry rather than by inspection.
        let inner = crate::waveform::WaveformPass::body_inner_rect(&clip, 1.0, BAND_H);
        assert!(
            !inner.is_drawable(),
            "a body of height 0 must not be drawable, got {inner:?}"
        );

        let peaks = [200u8; 32];
        let mut bars = Vec::new();
        let mut budget = crate::waveform::MAX_WAVEFORM_QUADS_PER_FRAME;
        let outcome = crate::waveform::emit_bars(
            &clip,
            &peaks,
            inner,
            [0.0, 0.0, 0.0, 1.0],
            &mut bars,
            &mut budget,
        );
        assert_eq!(outcome.bars, 0, "an all-band clip draws no body content");
        assert!(bars.is_empty());
    }

    #[test]
    fn zero_band_height_is_byte_for_byte_backcompat() {
        let clip = anatomy_clip(100.0, 200.0, 300.0, 48.0);
        let mut out = Vec::new();
        let emitted = push_clip_into(&mut out, &clip, 0.0, 2.0, 1.0);

        // ONE quad — the pre-53.2 count, pinned as a literal so a later plan that adds to
        // this function has to come here and change a number on purpose.
        assert_eq!(emitted, 1);
        assert_eq!(out.len(), 1);

        // ...and it is the instance the pre-phase clip loop pushed, field for field.
        let mut expected = Vec::new();
        push_into(
            &mut expected,
            100.0,
            200.0,
            300.0,
            48.0,
            unpack(clip.fill),
            unpack(clip.border),
            2.0,
            1.0,
        );
        assert_eq!(out[0].rect, expected[0].rect);
        assert_eq!(out[0].fill, expected[0].fill);
        assert_eq!(out[0].border, expected[0].border);
        assert_eq!(out[0].radius_px, expected[0].radius_px);
        assert_eq!(out[0].border_px, expected[0].border_px);

        // A resolved band COLOUR must not leak a band into a frame that asked for none —
        // the height is the switch, and `anatomy_clip` deliberately sets `band_fill`.
        assert_ne!(clip.band_fill, 0);

        // The label rect is the whole clip, because `text.rs` falls back whenever the band
        // has no height. Asserted through the condition it actually branches on rather
        // than through a screenshot nobody can diff.
        let (band, body) = split_band(clip.x_px, clip.y_px, clip.w_px, clip.h_px, 0.0);
        assert_eq!(band[3], 0.0);
        assert_eq!(body, [100.0, 200.0, 300.0, 48.0]);

        // ...and the waveform's rect is byte-identical to plan 52-08's, which is what
        // makes "the audio lane renders exactly as before" a fact rather than a hope.
        let inner = crate::waveform::WaveformPass::body_inner_rect(&clip, 1.0, 0.0);
        assert_eq!(inner.x, 101.0);
        assert_eq!(inner.y, 201.0);
        assert_eq!(inner.w, 298.0);
        assert_eq!(inner.h, 46.0);
    }
}
