//! ベクターレンダラー。[`Page`] のパスを lyon でテッセレーションし、wgpu の
//! オフスクリーンレンダリング（ネイティブ解像度・MSAA）で RGBA8 バッファに描画する。
//!
//! 「ベクター品質」を保つため、ズーム等で解像度が変わるたびに [`Renderer::render`] を
//! 呼び直す（ビットマップ拡大ではなく毎回テッセレーション→ラスタライズし直す）想定。
//! Kitty/Framebuffer どちらの出力でも、ここが生成する RGBA をそのまま流す。

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

/// ページ外（letterbox）の背景色。ビューア背景として暗色にし、ページ境界が見えるようにする。
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

/// 画像（テクスチャ付き矩形）の頂点。位置はページ座標、uv はテクスチャ座標。
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImageVertex {
    position: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    /// 表示するページ内矩形（ビューポート）= [原点x, 原点y, 幅, 高さ]（ページ座標）。
    /// この矩形が出力テクスチャ全体にマッピングされる。ズーム＝矩形を小さく、パン＝原点移動。
    viewport: [f32; 4],
}

/// lyon の頂点に塗り色を付与するコンストラクタ。
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
    /// 画像（テクスチャ付き矩形）描画用パイプラインとレイアウト・サンプラ。
    image_pipeline: wgpu::RenderPipeline,
    image_bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// 選択されたアダプタ名（デバッグ表示用）。
    pub adapter_info: String,
}

impl Renderer {
    /// wgpu をヘッドレス初期化する。bare TTY でも動くよう Vulkan を優先し、
    /// 取得できなければソフトウェアフォールバック（lavapipe 等）に切り替える。
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
            // ハードウェアアダプタが取れない場合はソフトウェアにフォールバック。
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                force_fallback_adapter: true,
                compatible_surface: None,
            }))
        })
        .map_err(|e| anyhow!("GPU アダプタを取得できませんでした: {e}"))?;

        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vecview-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| anyhow!("wgpu デバイス生成に失敗: {e}"))?;

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

        // 画像用：uniform(viewport, 頂点) + テクスチャ + サンプラ（フラグメント）。
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

    /// `page` の `viewport`（[x, y, w, h] ページ座標）を `width`×`height` ピクセルの
    /// RGBA8 バッファに描画して返す。viewport をページ全体にすれば全体表示、小さくすれば
    /// その領域を高解像度で拡大描画する（ズーム/パン）。歪みを避けるため viewport の
    /// アスペクト比は `width`/`height` に合わせること。
    pub fn render(
        &self,
        page: &Page,
        width: u32,
        height: u32,
        viewport: [f32; 4],
    ) -> Result<Vec<u8>> {
        let width = width.max(1);
        let height = height.max(1);

        let geometry = tessellate(page)?;

        // MSAA 用マルチサンプルテクスチャと、その resolve 先（コピー元）テクスチャ。
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

        // 画像 GPU リソースをパス開始前に用意（テクスチャ・頂点・バインドグループ）。
        // テクスチャはバインドグループが内部で参照を保持するが、念のため生存させておく。
        let image_draws = self.prepare_images(page, &uniform_buf);

        // 読み戻しバッファ（256バイト行アライン必須）。
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
                        // ページ外は暗色（letterbox）。ページ範囲は下記の白シート＋内容で塗る。
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

            // ラスター画像をパスの上に合成する。z 順は「全パス→全画像」と単純化しており、
            // ベクター注釈を画像の上に重ねる文書では前後関係が崩れる（PDF の図では実害なし）。
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

        // 同期読み戻し。
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv()
            .map_err(|_| anyhow!("map_async コールバック消失"))?
            .map_err(|e| anyhow!("バッファ map に失敗: {e:?}"))?;

        let data = slice.get_mapped_range();
        // パディングを除去して密な RGBA に詰め直す。
        let row_bytes = (width * 4) as usize;
        let mut out = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let start = row * bytes_per_row as usize;
            out.extend_from_slice(&data[start..start + row_bytes]);
        }
        drop(data);
        readback.unmap();

        Ok(out)
    }

    /// ページ内の各画像について、テクスチャ・バインドグループ・頂点バッファを用意する。
    /// `uniform_buf` は viewport（パスと共通の座標変換）。
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
            // write_texture は 256 バイト行アライン不要（copy_buffer_to_texture と違う）。
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

/// 1枚の画像を描くための GPU リソース束。`_texture` はバインドグループが内部参照を保持する
/// ため直接は使わないが、明示的に生存させて安全側に倒す。
struct ImageDraw {
    bind_group: wgpu::BindGroup,
    vertices: wgpu::Buffer,
    _texture: wgpu::Texture,
}

/// ページ座標の矩形 [x, y, w, h] を、テクスチャ全体を貼る2三角形（6頂点）に展開する。
fn image_quad(rect: [f32; 4]) -> [ImageVertex; 6] {
    let [x, y, w, h] = rect;
    let tl = ImageVertex { position: [x, y], uv: [0.0, 0.0] };
    let tr = ImageVertex { position: [x + w, y], uv: [1.0, 0.0] };
    let br = ImageVertex { position: [x + w, y + h], uv: [1.0, 1.0] };
    let bl = ImageVertex { position: [x, y + h], uv: [0.0, 1.0] };
    [tl, tr, br, tl, br, bl]
}

/// ページ内の全パスを1つの頂点/インデックスバッファにテッセレーションする（画像は別経路）。
fn tessellate(page: &Page) -> Result<VertexBuffers<Vertex, u32>> {
    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let mut fill_tess = FillTessellator::new();
    let mut stroke_tess = StrokeTessellator::new();

    // ページ背景の白シート。letterbox（暗色）の上にページ範囲だけ白を敷くことで、
    // 背景を持たない透明 SVG でもページ境界が分かり、横長/縦長スライドの余白が黒帯になる。
    // 自前で背景を塗る文書（Typst スライド等）はこの上から塗りつぶす。
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
            .map_err(|e| anyhow!("背景テッセレーション失敗: {e:?}"))?;
    }

    for cmd in &page.commands {
        let DrawCommand::Path(path) = cmd else {
            continue; // 画像は prepare_images / image_pipeline で別途描く。
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
                .map_err(|e| anyhow!("fill テッセレーション失敗: {e:?}"))?;
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
                .map_err(|e| anyhow!("stroke テッセレーション失敗: {e:?}"))?;
        }
    }

    Ok(buffers)
}

/// core のセグメント列から lyon のパスを構築する。
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
    // viewport = [x, y, w, h]。viewport 矩形を NDC [-1,1] にマッピング（Y反転）。
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

// --- 画像（テクスチャ付き矩形）---
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

    /// 赤い矩形でページ全体を塗り、中心ピクセルが赤く描画されることを確認する。
    /// wgpu のヘッドレス GPU 経路がこの環境で動くことの検証も兼ねる。
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

        let renderer = Renderer::new().expect("wgpu 初期化");
        eprintln!("adapter: {}", renderer.adapter_info);
        let (w, h) = (64u32, 64u32);
        let rgba = renderer
            .render(&page, w, h, [0.0, 0.0, page.width, page.height])
            .expect("render");
        assert_eq!(rgba.len(), (w * h * 4) as usize);

        // 中心ピクセル。
        let idx = ((h / 2 * w + w / 2) * 4) as usize;
        let (r, g, b, a) = (rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]);
        eprintln!("center pixel = ({r}, {g}, {b}, {a})");
        assert!(r > 200 && g < 60 && b < 60, "中心が赤でない: ({r},{g},{b},{a})");
    }
}
