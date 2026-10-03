//! The textured instanced-quad pass — this crate's first pipeline that samples anything.
//!
//! Structurally a sibling of [`crate::quads::QuadPass`]: one static unit quad, one dynamic
//! instance buffer grown by DOUBLING and never shrunk, one `draw` per frame regardless of
//! how many tiles it carries. What it adds over that pass is a bind group with a texture
//! and a sampler in it, which is the whole reason it cannot simply be more instances in the
//! flat-quad buffer — a different bind group layout is a different pipeline layout is a
//! different pipeline.
//!
//! # Where it draws, and why that is a place rather than a convention
//!
//! Between `QuadPass::draw_fills` and `QuadPass::draw_overlays` — the seam plan 53.2-03 cut
//! as two draw ranges over one buffer. Tiles must land ABOVE the flat body fills (or the
//! body colour paints over the frames) and BELOW every overlay (or the frames paint over
//! the selection ring, the trim handles, the waveform, the playhead and the sticky gutter).
//! Both hold by construction because that gap is the only place the call fits.
//!
//! # No allocation in the steady state
//!
//! Same discipline as `quads.rs`, for the same reason: the instance `Vec` is cleared rather
//! than dropped, the GPU buffer only ever grows, and `Queue::write_buffer` into an existing
//! buffer allocates nothing. A Timeline that once drew a thousand tiles will draw them
//! again, and giving the memory back only to re-take it next frame is the reallocation this
//! design exists to avoid.

use crate::atlas::FilmstripAtlas;

/// One textured rectangle. 32 bytes, `bytemuck::Pod`, uploaded verbatim.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TexQuadInstance {
    /// `x, y, w, h` in physical px, top-left origin.
    pub rect: [f32; 4],
    /// `u0, v0, u1, v1`, normalised over the atlas. A SUB-rect of one resident sheet.
    pub uv: [f32; 4],
}

/// Is every component of this instance usable?
///
/// The same layer split `quads::rect_is_drawable` states: `abi.rs` owns pointers and
/// lengths, and geometry is filtered HERE, where instances are built, because a `NaN`
/// coordinate is a drawing problem rather than a memory-safety one and the right answer is
/// a skipped tile inside an otherwise complete frame.
///
/// The uv rect is checked too, and that is not symmetry: a non-finite uv reaching the
/// vertex buffer makes `textureSample` read an undefined texel — which on a shared atlas
/// means *some other strip's pixels*, drawn confidently, with nothing to notice.
#[inline]
fn instance_is_drawable(x: f32, y: f32, w: f32, h: f32, uv: [f32; 4]) -> bool {
    x.is_finite()
        && y.is_finite()
        && w.is_finite()
        && h.is_finite()
        && w > 0.0
        && h > 0.0
        && uv.iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c))
}

/// Push one textured rectangle into a CALLER-OWNED instance list — **the single place in
/// this crate where a tile becomes a [`TexQuadInstance`]**.
///
/// Returns whether it was emitted, so the stats counter is the truth rather than the
/// intent (`quads::push_into`'s own rule).
#[inline]
pub fn push_tex_into(
    out: &mut Vec<TexQuadInstance>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    uv: [f32; 4],
) -> bool {
    if !instance_is_drawable(x, y, w, h, uv) {
        return false;
    }
    out.push(TexQuadInstance {
        rect: [x, y, w, h],
        uv,
    });
    true
}

/// The textured pipeline, its static unit quad, its bind group, and its reused instances.
pub struct TexQuadPass {
    pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    unit_quad: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    instances: Vec<TexQuadInstance>,
}

/// The unit quad as a triangle strip: (0,0) (1,0) (0,1) (1,1).
const UNIT_QUAD: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]];

/// Instances the buffer starts with. `MAX_TILES_PER_CLIP_DRAW * 8` — eight fully-tiled
/// clips before the first growth, which covers a typical viewport without over-reserving.
const INITIAL_INSTANCE_CAPACITY: usize = 512;

impl TexQuadPass {
    /// Build the pipeline against a LIVE atlas.
    ///
    /// The atlas is borrowed rather than owned because the bind group has to name its view
    /// and sampler, and both are stable for the renderer's whole life — the texture is
    /// allocated once at attach and never grown or replaced, which is exactly what makes a
    /// single bind group built here correct forever rather than something to rebuild per
    /// frame.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, atlas: &FilmstripAtlas) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rudis-timeline-texquads"),
            source: wgpu::ShaderSource::Wgsl(include_str!("texquad.wgsl").into()),
        });

        // Globals in the VERTEX stage, texture + sampler in the FRAGMENT stage. Stated per
        // entry rather than as `ShaderStages::all()`: a uniform visible to a stage that
        // does not read it is a validation surface for no benefit. The shape is
        // reimplemented from `crates/engine/src/compositor.rs`'s texture+sampler layout —
        // READ ONLY, that file is under the engine-axis freeze, and nothing is imported
        // from it (this crate links nothing from the frozen crates at all).
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rudis-timeline-texquad-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rudis-timeline-texquad-globals"),
            size: 16, // vec2<f32> resolution + vec2<f32> pad
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rudis-timeline-texquad-bind-group"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(atlas.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(atlas.sampler()),
                },
            ],
        });

        // wgpu 29 API drift, named explicitly for the reason `quads.rs` gives: a pinned
        // value that can hide inside `..Default::default()` is a pinned value that can be
        // lost.
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rudis-timeline-texquad-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rudis-timeline-texquad-pipeline"),
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
                        array_stride: std::mem::size_of::<TexQuadInstance>() as u64,
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
                    // D-08: STRAIGHT alpha, so the downsampler's alpha-0 letterbox padding
                    // shows the clip's own body fill beneath rather than black bars.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let unit_quad = {
            use wgpu::util::DeviceExt;
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("rudis-timeline-texquad-unit-quad"),
                contents: bytemuck::cast_slice(&UNIT_QUAD),
                usage: wgpu::BufferUsages::VERTEX,
            })
        };

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rudis-timeline-texquad-instances"),
            size: (INITIAL_INSTANCE_CAPACITY * std::mem::size_of::<TexQuadInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            globals_buffer,
            bind_group,
            unit_quad,
            instance_buffer,
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
            instances: Vec::with_capacity(INITIAL_INSTANCE_CAPACITY),
        }
    }

    /// Start a frame. Keeps capacity.
    pub fn begin(&mut self) {
        self.instances.clear();
    }

    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// The CPU-side instance list, for the pass that builds the tiles (`filmstrip.rs`).
    ///
    /// Exposed as the list rather than as a per-tile method for `quads.rs`'s own reason:
    /// the emission loop is then a free function with no GPU handle in sight, which is what
    /// makes it testable on a machine with no adapter.
    pub fn instances_mut(&mut self) -> &mut Vec<TexQuadInstance> {
        &mut self.instances
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
            let mut cap = self.instance_capacity.max(1);
            while cap < self.instances.len() {
                cap *= 2;
            }
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rudis-timeline-texquad-instances"),
                size: (cap * std::mem::size_of::<TexQuadInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = cap;
        }

        queue.write_buffer(
            &self.instance_buffer,
            0,
            bytemuck::cast_slice(&self.instances),
        );
    }

    /// Draw every tile — ONE call per frame, whether the frame carries one tile or 8,192.
    ///
    /// SHELL-05's "no draw call per clip" is untouched: this is a third draw call per
    /// FRAME beside `draw_fills` and `draw_overlays`, not a call per clip and not a call
    /// per tile.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.instances.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.unit_quad.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.draw(0..4, 0..self.instances.len() as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_UV: [f32; 4] = [0.0, 0.0, 0.5, 0.5];

    #[test]
    fn tex_quad_instance_is_thirty_two_bytes_with_uv_at_sixteen() {
        // The vertex attribute table above hard-codes offsets 0 and 16 and a 32-byte
        // stride. Drift renders SOMETHING — the wrong region of the atlas — which is worse
        // than rendering nothing, because nothing is obviously broken.
        assert_eq!(std::mem::size_of::<TexQuadInstance>(), 32);
        assert_eq!(std::mem::offset_of!(TexQuadInstance, rect), 0);
        assert_eq!(std::mem::offset_of!(TexQuadInstance, uv), 16);
    }

    #[test]
    fn degenerate_geometry_is_filtered_exactly_as_the_flat_quad_pass_filters_it() {
        let mut out = Vec::new();
        assert!(!push_tex_into(&mut out, f32::NAN, 0.0, 10.0, 10.0, OK_UV));
        assert!(!push_tex_into(&mut out, 0.0, f32::NAN, 10.0, 10.0, OK_UV));
        assert!(!push_tex_into(&mut out, 0.0, 0.0, f32::INFINITY, 10.0, OK_UV));
        assert!(!push_tex_into(&mut out, 0.0, 0.0, 0.0, 10.0, OK_UV), "zero width");
        assert!(!push_tex_into(&mut out, 0.0, 0.0, 10.0, -1.0, OK_UV), "negative height");
        assert!(out.is_empty(), "nothing degenerate reached the buffer");

        assert!(push_tex_into(&mut out, -50.0, -50.0, 10.0, 10.0, OK_UV), "offscreen is fine");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rect, [-50.0, -50.0, 10.0, 10.0]);
        assert_eq!(out[0].uv, OK_UV);
    }

    #[test]
    fn a_uv_outside_the_atlas_is_refused_rather_than_sampled() {
        // On a SHARED atlas an out-of-range uv does not render blank — it renders some
        // OTHER media's frames, confidently, with nothing to notice. That is T-53.2-19's
        // silent-visual-corruption class, so the filter is here rather than trusted upstream.
        let mut out = Vec::new();
        for bad in [
            [f32::NAN, 0.0, 0.5, 0.5],
            [0.0, f32::NAN, 0.5, 0.5],
            [0.0, 0.0, f32::INFINITY, 0.5],
            [-0.01, 0.0, 0.5, 0.5],
            [0.0, 0.0, 1.01, 0.5],
            [0.0, 0.0, 0.5, 2.0],
        ] {
            assert!(
                !push_tex_into(&mut out, 0.0, 0.0, 10.0, 10.0, bad),
                "uv {bad:?} must not reach a vertex buffer"
            );
        }
        assert!(out.is_empty());

        // The exact bounds ARE allowed: a sheet at the atlas's far edge is legitimate.
        assert!(push_tex_into(&mut out, 0.0, 0.0, 10.0, 10.0, [0.0, 0.0, 1.0, 1.0]));
    }
}
