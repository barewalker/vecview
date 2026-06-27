//! Vector renderer. Tessellates the paths of a [`Page`] with lyon and draws them
//! into an RGBA8 buffer via wgpu offscreen rendering (native resolution, MSAA).
//!
//! To preserve "vector quality", [`Renderer::render`] is meant to be called again
//! every time the resolution changes (e.g. on zoom): instead of scaling up a bitmap,
//! it re-tessellates and re-rasterizes each time. For both Kitty and Framebuffer
//! output, the RGBA produced here is passed through as-is.

use anyhow::{anyhow, Result};
use bytemuck::{Pod, Zeroable};
use lyon::path::FillRule as LyonFillRule;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, FillVertexConstructor, StrokeOptions,
    StrokeTessellator, StrokeVertex, StrokeVertexConstructor, VertexBuffers,
};
use vecview_core::{DrawCommand, FillRule, Page, PathSegment};
use wgpu::util::DeviceExt;

const SAMPLE_COUNT: u32 = 4;
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Internal supersampling factor for anti-aliasing: the vector scene is rendered at this integer
/// multiple of the requested size and then box-downsampled back down, sharpening text/curve edges
/// to roughly pdfium quality WITHOUT enlarging the transferred image. MSAA alone (4x) only gives a
/// few coverage levels on edges; supersampling effectively yields `(ss*ss * SAMPLE_COUNT)` levels.
/// Override with `VECVIEW_AA_SS` (1 disables; clamped to 1..=4). Combines on top of `-s`/VECVIEW_SCALE.
fn aa_supersample() -> u32 {
    use std::sync::OnceLock;
    static SS: OnceLock<u32> = OnceLock::new();
    *SS.get_or_init(|| {
        std::env::var("VECVIEW_AA_SS")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|v| v.clamp(1, 4))
            .unwrap_or(2)
    })
}

/// Average each `ss`x`ss` block of `src` (`sw`=`dw*ss` wide) into one pixel, producing a tightly
/// packed `dw`x`dh` RGBA buffer. This is the downsample step of internal supersampling.
fn box_downsample(src: &[u8], sw: u32, dw: u32, dh: u32, ss: u32) -> Vec<u8> {
    let (sw, dw, dh, ss) = (sw as usize, dw as usize, dh as usize, ss as usize);
    let n = (ss * ss) as u32;
    let mut dst = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        for dx in 0..dw {
            let mut acc = [0u32; 4];
            for oy in 0..ss {
                let row = (dy * ss + oy) * sw * 4;
                for ox in 0..ss {
                    let i = row + (dx * ss + ox) * 4;
                    acc[0] += src[i] as u32;
                    acc[1] += src[i + 1] as u32;
                    acc[2] += src[i + 2] as u32;
                    acc[3] += src[i + 3] as u32;
                }
            }
            let o = (dy * dw + dx) * 4;
            dst[o] = (acc[0] / n) as u8;
            dst[o + 1] = (acc[1] / n) as u8;
            dst[o + 2] = (acc[2] / n) as u8;
            dst[o + 3] = (acc[3] / n) as u8;
        }
    }
    dst
}

/// Background color outside the page (letterbox). Kept dark as the viewer background so the page boundary is visible.
const LETTERBOX: wgpu::Color = wgpu::Color {
    r: 0.10,
    g: 0.10,
    b: 0.10,
    a: 1.0,
};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

/// Vertex of an image (a textured rectangle). Position is in page coordinates, uv is in texture coordinates.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImageVertex {
    position: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    /// The rectangle within the page to display (the viewport) = [origin x, origin y, width, height] (page coordinates).
    /// This rectangle is mapped onto the entire output texture. Zooming shrinks the rectangle; panning moves its origin.
    viewport: [f32; 4],
}

/// Constructor that attaches a fill color to lyon vertices.
struct WithColor {
    color: [f32; 4],
}

impl FillVertexConstructor<Vertex> for WithColor {
    fn new_vertex(&mut self, vertex: FillVertex) -> Vertex {
        Vertex {
            position: vertex.position().to_array(),
            color: self.color,
        }
    }
}

impl StrokeVertexConstructor<Vertex> for WithColor {
    fn new_vertex(&mut self, vertex: StrokeVertex) -> Vertex {
        Vertex {
            position: vertex.position().to_array(),
            color: self.color,
        }
    }
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    /// Pipeline, layout, and sampler for drawing images (textured rectangles).
    image_pipeline: wgpu::RenderPipeline,
    image_bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Name of the selected adapter (for debug display).
    pub adapter_info: String,
}

impl Renderer {
    /// Initializes wgpu headlessly. Prefers Vulkan so it works even on a bare TTY,
    /// and falls back to a software adapter (e.g. lavapipe) if none can be acquired.
    pub fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .or_else(|_| {
            // Fall back to software if no hardware adapter is available.
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                force_fallback_adapter: true,
                compatible_surface: None,
            }))
        })
        .map_err(|e| anyhow!("failed to acquire a GPU adapter: {e}"))?;

        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vecview-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| anyhow!("failed to create wgpu device: {e}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vecview-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("vecview-bgl"),
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vecview-pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vecview-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TEXTURE_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: SAMPLE_COUNT,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        // For images: uniform (viewport, vertices) + texture + sampler (fragment).
        let image_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("vecview-image-bgl"),
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

        let image_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("vecview-image-pl"),
                bind_group_layouts: &[Some(&image_bind_group_layout)],
                immediate_size: 0,
            });

        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vecview-image-pipeline"),
            layout: Some(&image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_img"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImageVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_img"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TEXTURE_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: SAMPLE_COUNT,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vecview-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            image_pipeline,
            image_bind_group_layout,
            sampler,
            adapter_info,
        })
    }

    /// Renders the `viewport` of `page` ([x, y, w, h] in page coordinates) into a
    /// `width`x`height` pixel RGBA8 buffer and returns it. Setting the viewport to the
    /// whole page shows everything; making it smaller draws that region enlarged at high
    /// resolution (zoom/pan). To avoid distortion, match the viewport's aspect ratio to
    /// `width`/`height`.
    pub fn render(
        &self,
        page: &Page,
        width: u32,
        height: u32,
        viewport: [f32; 4],
    ) -> Result<Vec<u8>> {
        let target_w = width.max(1);
        let target_h = height.max(1);

        // Internal supersampling: render at an integer multiple, then box-downsample to the
        // requested size before returning (sharper edges, same transferred size). Reduce the factor
        // if the supersampled texture would exceed the GPU's max 2D dimension.
        let max_dim = self.device.limits().max_texture_dimension_2d.max(1);
        let mut ss = aa_supersample();
        while ss > 1 && (target_w.saturating_mul(ss) > max_dim || target_h.saturating_mul(ss) > max_dim)
        {
            ss -= 1;
        }
        let width = target_w * ss;
        let height = target_h * ss;

        let geometry = tessellate(page)?;

        // The multisample texture for MSAA and its resolve target (copy source) texture.
        let msaa = self.create_texture(width, height, SAMPLE_COUNT, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let resolve = self.create_texture(
            width,
            height,
            1,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let msaa_view = msaa.create_view(&Default::default());
        let resolve_view = resolve.create_view(&Default::default());

        let uniforms = Uniforms {
            viewport: [
                viewport[0],
                viewport[1],
                viewport[2].max(1.0),
                viewport[3].max(1.0),
            ],
        };
        let uniform_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vecview-uniform"),
                contents: bytemuck::bytes_of(&uniforms),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vecview-bg"),
            layout: &self.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let vertex_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vecview-vertices"),
                contents: bytemuck::cast_slice(&geometry.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let index_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vecview-indices"),
                contents: bytemuck::cast_slice(&geometry.indices),
                usage: wgpu::BufferUsages::INDEX,
            });

        // Prepare image GPU resources before the pass begins (textures, vertices, bind groups).
        // The bind group holds an internal reference to each texture, but we keep them alive just in case.
        let image_draws = self.prepare_images(page, &uniform_buf);

        // Readback buffer (rows must be aligned to 256 bytes).
        let bytes_per_row = align_up(width * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vecview-readback"),
            size: (bytes_per_row * height) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vecview-encoder"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vecview-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &msaa_view,
                    resolve_target: Some(&resolve_view),
                    depth_slice: None,
                    ops: wgpu::Operations {
                        // Outside the page is dark (letterbox). The page area is filled by the white sheet plus content below.
                        load: wgpu::LoadOp::Clear(LETTERBOX),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if !geometry.indices.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.set_vertex_buffer(0, vertex_buf.slice(..));
                pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..geometry.indices.len() as u32, 0, 0..1);
            }

            // Composite raster images on top of the paths. The z order is simplified to "all paths, then all images",
            // so documents that overlay vector annotations on top of images get the ordering wrong (no real harm for PDF figures).
            if !image_draws.is_empty() {
                pass.set_pipeline(&self.image_pipeline);
                for img in &image_draws {
                    pass.set_bind_group(0, &img.bind_group, &[]);
                    pass.set_vertex_buffer(0, img.vertices.slice(..));
                    pass.draw(0..6, 0..1);
                }
            }
        }

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &resolve,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        self.queue.submit(Some(encoder.finish()));

        // Synchronous readback.
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv()
            .map_err(|_| anyhow!("map_async callback lost"))?
            .map_err(|e| anyhow!("failed to map buffer: {e:?}"))?;

        let data = slice.get_mapped_range();
        // Strip the padding and repack into tightly packed RGBA.
        let row_bytes = (width * 4) as usize;
        let mut out = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let start = row * bytes_per_row as usize;
            out.extend_from_slice(&data[start..start + row_bytes]);
        }
        drop(data);
        readback.unmap();

        // Downsample the supersampled render back to the requested size.
        if ss > 1 {
            return Ok(box_downsample(&out, width, target_w, target_h, ss));
        }
        Ok(out)
    }

    /// Prepares a texture, bind group, and vertex buffer for each image in the page.
    /// `uniform_buf` is the viewport (the same coordinate transform used by paths).
    fn prepare_images(&self, page: &Page, uniform_buf: &wgpu::Buffer) -> Vec<ImageDraw> {
        let mut draws = Vec::new();
        for cmd in &page.commands {
            let DrawCommand::Image(img) = cmd else {
                continue;
            };
            if img.px_width == 0 || img.px_height == 0 {
                continue;
            }
            let size = wgpu::Extent3d {
                width: img.px_width,
                height: img.px_height,
                depth_or_array_layers: 1,
            };
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vecview-image-texture"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            // write_texture does not require 256-byte row alignment (unlike copy_buffer_to_texture).
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &img.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(img.px_width * 4),
                    rows_per_image: Some(img.px_height),
                },
                size,
            );
            let view = texture.create_view(&Default::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vecview-image-bg"),
                layout: &self.image_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            let vertices = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vecview-image-verts"),
                    contents: bytemuck::cast_slice(&image_quad(img.rect)),
                    usage: wgpu::BufferUsages::VERTEX,
                });
            draws.push(ImageDraw {
                bind_group,
                vertices,
                _texture: texture,
            });
        }
        draws
    }

    fn create_texture(
        &self,
        width: u32,
        height: u32,
        sample_count: u32,
        usage: wgpu::TextureUsages,
    ) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vecview-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count,
            dimension: wgpu::TextureDimension::D2,
            format: TEXTURE_FORMAT,
            usage,
            view_formats: &[],
        })
    }
}

/// Bundle of GPU resources for drawing a single image. `_texture` is held alive explicitly
/// to be safe; it is not used directly since the bind group keeps an internal reference.
struct ImageDraw {
    bind_group: wgpu::BindGroup,
    vertices: wgpu::Buffer,
    _texture: wgpu::Texture,
}

/// Expands a page-coordinate rectangle [x, y, w, h] into 2 triangles (6 vertices) that map the entire texture.
fn image_quad(rect: [f32; 4]) -> [ImageVertex; 6] {
    let [x, y, w, h] = rect;
    let tl = ImageVertex { position: [x, y], uv: [0.0, 0.0] };
    let tr = ImageVertex { position: [x + w, y], uv: [1.0, 0.0] };
    let br = ImageVertex { position: [x + w, y + h], uv: [1.0, 1.0] };
    let bl = ImageVertex { position: [x, y + h], uv: [0.0, 1.0] };
    [tl, tr, br, tl, br, bl]
}

/// Tessellates all paths in the page into a single vertex/index buffer (images go through a separate path).
fn tessellate(page: &Page) -> Result<VertexBuffers<Vertex, u32>> {
    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let mut fill_tess = FillTessellator::new();
    let mut stroke_tess = StrokeTessellator::new();

    // The white sheet for the page background. Laying down white only over the page area on top of the
    // letterbox (dark) makes the page boundary visible even for transparent SVGs that have no background,
    // and turns the margins of wide/tall slides into black bars. Documents that paint their own background
    // (e.g. Typst slides) just paint over this.
    {
        use lyon::math::point;
        let mut builder = lyon::path::Path::builder();
        builder.begin(point(0.0, 0.0));
        builder.line_to(point(page.width, 0.0));
        builder.line_to(point(page.width, page.height));
        builder.line_to(point(0.0, page.height));
        builder.end(true);
        let bg = builder.build();
        fill_tess
            .tessellate_path(
                &bg,
                &FillOptions::default(),
                &mut BuffersBuilder::new(&mut buffers, WithColor { color: [1.0, 1.0, 1.0, 1.0] }),
            )
            .map_err(|e| anyhow!("background tessellation failed: {e:?}"))?;
    }

    for cmd in &page.commands {
        let DrawCommand::Path(path) = cmd else {
            continue; // Images are drawn separately via prepare_images / image_pipeline.
        };
        let lyon_path = build_path(&path.segments);

        if let Some(fill) = &path.fill {
            let options = FillOptions::tolerance(0.1).with_fill_rule(match fill.rule {
                FillRule::NonZero => LyonFillRule::NonZero,
                FillRule::EvenOdd => LyonFillRule::EvenOdd,
            });
            let color = fill.color.to_f32();
            fill_tess
                .tessellate_path(
                    &lyon_path,
                    &options,
                    &mut BuffersBuilder::new(&mut buffers, WithColor { color }),
                )
                .map_err(|e| anyhow!("fill tessellation failed: {e:?}"))?;
        }

        if let Some(stroke) = &path.stroke {
            let options = StrokeOptions::tolerance(0.1).with_line_width(stroke.width.max(0.1));
            let color = stroke.color.to_f32();
            stroke_tess
                .tessellate_path(
                    &lyon_path,
                    &options,
                    &mut BuffersBuilder::new(&mut buffers, WithColor { color }),
                )
                .map_err(|e| anyhow!("stroke tessellation failed: {e:?}"))?;
        }
    }

    Ok(buffers)
}

/// Builds a lyon path from core's segment list.
fn build_path(segments: &[PathSegment]) -> lyon::path::Path {
    use lyon::math::point;
    let mut builder = lyon::path::Path::builder();
    let mut open = false;
    for seg in segments {
        match *seg {
            PathSegment::MoveTo(p) => {
                if open {
                    builder.end(false);
                }
                builder.begin(point(p[0], p[1]));
                open = true;
            }
            PathSegment::LineTo(p) => {
                if open {
                    builder.line_to(point(p[0], p[1]));
                }
            }
            PathSegment::QuadTo(c, p) => {
                if open {
                    builder.quadratic_bezier_to(point(c[0], c[1]), point(p[0], p[1]));
                }
            }
            PathSegment::CubicTo(c1, c2, p) => {
                if open {
                    builder.cubic_bezier_to(
                        point(c1[0], c1[1]),
                        point(c2[0], c2[1]),
                        point(p[0], p[1]),
                    );
                }
            }
            PathSegment::Close => {
                if open {
                    builder.end(true);
                    open = false;
                }
            }
        }
    }
    if open {
        builder.end(false);
    }
    builder.build()
}

fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

const SHADER: &str = r#"
struct Uniforms {
    viewport: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VsOut {
    var out: VsOut;
    // viewport = [x, y, w, h]. Map the viewport rectangle into NDC [-1,1] (Y flipped).
    let ndc = vec2<f32>(
        (position.x - u.viewport.x) / u.viewport.z * 2.0 - 1.0,
        1.0 - (position.y - u.viewport.y) / u.viewport.w * 2.0,
    );
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}

// --- Images (textured rectangles) ---
@group(0) @binding(1) var img_tex: texture_2d<f32>;
@group(0) @binding(2) var img_samp: sampler;

struct ImgOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_img(@location(0) position: vec2<f32>, @location(1) uv: vec2<f32>) -> ImgOut {
    var out: ImgOut;
    let ndc = vec2<f32>(
        (position.x - u.viewport.x) / u.viewport.z * 2.0 - 1.0,
        1.0 - (position.y - u.viewport.y) / u.viewport.w * 2.0,
    );
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_img(in: ImgOut) -> @location(0) vec4<f32> {
    return textureSample(img_tex, img_samp, in.uv);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use vecview_core::{Color, Fill, PathData};

    /// Fills the whole page with a red rectangle and checks that the center pixel is drawn red.
    /// Also serves to verify that wgpu's headless GPU path works in this environment.
    #[test]
    fn renders_red_rect() {
        let page = Page {
            width: 10.0,
            height: 10.0,
            commands: vec![DrawCommand::Path(PathData {
                segments: vec![
                    PathSegment::MoveTo([0.0, 0.0]),
                    PathSegment::LineTo([10.0, 0.0]),
                    PathSegment::LineTo([10.0, 10.0]),
                    PathSegment::LineTo([0.0, 10.0]),
                    PathSegment::Close,
                ],
                fill: Some(Fill {
                    color: Color::rgba(255, 0, 0, 255),
                    rule: FillRule::NonZero,
                }),
                stroke: None,
            })],
        };

        let renderer = Renderer::new().expect("wgpu init");
        eprintln!("adapter: {}", renderer.adapter_info);
        let (w, h) = (64u32, 64u32);
        let rgba = renderer
            .render(&page, w, h, [0.0, 0.0, page.width, page.height])
            .expect("render");
        assert_eq!(rgba.len(), (w * h * 4) as usize);

        // Center pixel.
        let idx = ((h / 2 * w + w / 2) * 4) as usize;
        let (r, g, b, a) = (rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]);
        eprintln!("center pixel = ({r}, {g}, {b}, {a})");
        assert!(r > 200 && g < 60 && b < 60, "center is not red: ({r},{g},{b},{a})");
    }
}
