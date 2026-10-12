//! Gridfall — the desktop falls into a light-cycle grid. A screensaver by lAvArt Studio.
//! Tron: Ares–style grid world: GPU-rendered, CPU physics, ~one draw call per layer.

mod brand;
mod feed;
mod scene;
mod sim;

use bytemuck::{Pod, Zeroable};
use scene::{Scene, Vertex};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId};

const MAX_QUADS: usize = 40_000;
const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const APP_ID: &str = "org.omarchy.screensaver";

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    inv_vp: [[f32; 4]; 4],
    eye: [f32; 4],
    params: [f32; 4],
    pulse: [[f32; 4]; 4],
    pcol: [[f32; 4]; 4],
    extra: [f32; 4],
    /// the opening camera: pins the wallpaper to the floor exactly where it filled the first frame
    wall_vp: [[f32; 4]; 4],
    /// x: wallpaper opacity, y: wallpaper aspect (w/h), z: lAvArt hairline grid on (1/0), w: unused
    wall: [f32; 4],
}

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    floor: wgpu::RenderPipeline,
    add: wgpu::RenderPipeline,
    alpha: wgpu::RenderPipeline,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    ubuf: wgpu::Buffer,
    bind: wgpu::BindGroup,
    post: Post,
    /// recording: frames go to this texture instead of the window, so a covered or hidden window can't stall them
    offscreen: Option<wgpu::Texture>,
}

fn offscreen_target(device: &wgpu::Device, format: wgpu::TextureFormat, w: u32, h: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("recording"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Size-dependent post-processing resources (HDR scene + two-level bloom chain).
struct Targets {
    scene: wgpu::TextureView,
    depth: wgpu::TextureView,
    passes: Vec<wgpu::BindGroup>, // down1, blurH1, blurV1, down2, blurH2, blurV2, comp
    views: [wgpu::TextureView; 4], // L1A, L1B, L2A, L2B
    /// composite settings: w = bloom suppression (while the real wallpaper is on screen)
    u_comp: wgpu::Buffer,
}

struct Post {
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    down: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    comp: wgpu::RenderPipeline,
    t: Targets,
}

fn make_tex(device: &wgpu::Device, w: u32, h: u32) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: HDR,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn build_targets(device: &wgpu::Device, bgl: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, w: u32, h: u32) -> Targets {
    let (w1, h1) = ((w / 4).max(1), (h / 4).max(1));
    let (w2, h2) = ((w1 / 4).max(1), (h1 / 4).max(1));
    let scene = make_tex(device, w, h);
    let depth = device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default());
    let l1a = make_tex(device, w1, h1);
    let l1b = make_tex(device, w1, h1);
    let l2a = make_tex(device, w2, h2);
    let l2b = make_tex(device, w2, h2);
    let uni = |v: [f32; 4]| {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&v),
            usage: wgpu::BufferUsages::UNIFORM,
        })
    };
    let bg = |src: &wgpu::TextureView, u: &wgpu::Buffer, t1: &wgpu::TextureView, t2: &wgpu::TextureView| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(t1) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(t2) },
            ],
        })
    };
    let (ux, uy) = (1.0 / w as f32, 1.0 / h as f32);
    let u_down1 = uni([ux, uy, 0.85, 0.0]);
    let u_bh1 = uni([1.0 / w1 as f32, 0.0, 0.0, 0.0]);
    let u_bv1 = uni([0.0, 1.0 / h1 as f32, 0.0, 0.0]);
    let u_down2 = uni([1.0 / w1 as f32, 1.0 / h1 as f32, 0.0, 0.0]);
    let u_bh2 = uni([1.0 / w2 as f32, 0.0, 0.0, 0.0]);
    let u_bv2 = uni([0.0, 1.0 / h2 as f32, 0.0, 0.0]);
    let u_comp = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(&[0.0f32; 4]),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let passes = vec![
        bg(&scene, &u_down1, &scene, &scene),
        bg(&l1a, &u_bh1, &scene, &scene),
        bg(&l1b, &u_bv1, &scene, &scene),
        bg(&l1a, &u_down2, &scene, &scene),
        bg(&l2a, &u_bh2, &scene, &scene),
        bg(&l2b, &u_bv2, &scene, &scene),
        bg(&scene, &u_comp, &l1a, &l2a),
    ];
    Targets { scene, depth, passes, views: [l1a, l1b, l2a, l2b], u_comp }
}

impl Gfx {
    fn new(window: Arc<Window>, atlas: &scene::Atlas, capture: bool) -> Self {
        // Vulkan only: probing OpenGL too would wake every GPU in the machine just to list it
        let open = |backends: wgpu::Backends| {
            let mut d = wgpu::InstanceDescriptor::new_without_display_handle();
            d.backends = backends;
            let instance = wgpu::Instance::new(d);
            let surface = instance.create_surface(window.clone()).expect("surface");
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: if want_fast_gpu() { wgpu::PowerPreference::HighPerformance } else { wgpu::PowerPreference::LowPower },
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            }))
            .ok();
            adapter.map(|a| (instance, surface, a))
        };
        let (_instance, surface, adapter) = open(wgpu::Backends::VULKAN).or_else(|| open(wgpu::Backends::all())).expect("no GPU adapter");
        if debug() {
            let i = adapter.get_info();
            eprintln!("gridfall: adapter {} ({:?}, {:?})", i.name, i.device_type, i.backend);
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        if debug() {
            eprintln!("gridfall: surface formats {:?}", caps.formats);
        }
        // Gamma-space blending gives the punchy, filmic additive glow we want.
        // 8-bit sRGB: the output is linear light, encoded for the panel by the GPU. Matches the wallpaper exactly (a
        // float surface leaves the encoding to the compositor, which crushes the near-blacks), costs half the
        // bandwidth, and is exactly what a recording reads back.
        let srgb = caps.formats.iter().copied().find(|f| matches!(f, wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb));
        let format = if std::env::var("GRIDFALL_SURFACE").as_deref() == Ok("float") {
            caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0])
        } else {
            srgb.unwrap_or(caps.formats[0])
        };
        // a recording reads the frames back as 8-bit BGRA/RGBA for ffmpeg
        let format = if capture { srgb.unwrap_or(format) } else { format };
        let mut config = surface.get_default_config(&adapter, size.width.max(1), size.height.max(1)).expect("config");
        config.format = format;
        config.present_mode = wgpu::PresentMode::Fifo;
        config.desired_maximum_frame_latency = 1;
        if capture && caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
            config.usage |= wgpu::TextureUsages::COPY_SRC;
        }
        surface.configure(&device, &config);
        if debug() {
            eprintln!("gridfall: surface {:?} {}x{}", config.format, config.width, config.height);
        }

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

        let tex = device.create_texture_with_data(
            &queue,
            &wgpu::TextureDescriptor {
                label: Some("glyphs"),
                size: wgpu::Extent3d { width: atlas.w, height: atlas.h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &atlas.data,
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        // the desktop wallpaper the opening starts from (1×1 black when there is none)
        let wp = crate::brand::get().wallpaper.as_ref();
        let (ww, wh, wdata) = wp.map(|w| (w.w, w.h, w.rgba.as_slice())).unwrap_or((1, 1, &[0, 0, 0, 255]));
        let wtex = device.create_texture_with_data(
            &queue,
            &wgpu::TextureDescriptor {
                label: Some("wallpaper"),
                size: wgpu::Extent3d { width: ww, height: wh, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb, // decoded to linear: the output is linear light (see the surface format)
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            wdata,
        );
        let wview = wtex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&wview) },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x2, 3 => Float32];
        let vlayout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attrs,
        };
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
            alpha: wgpu::BlendComponent::OVER,
        };
        let make = |label: &str, vs: &str, fs: &str, buffers: &[Option<wgpu::VertexBufferLayout>], blend: Option<wgpu::BlendState>, write: bool, always: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState { module: &shader, entry_point: Some(vs), compilation_options: Default::default(), buffers },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: HDR, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH,
                    depth_write_enabled: Some(write),
                    depth_compare: Some(if always { wgpu::CompareFunction::Always } else { wgpu::CompareFunction::LessEqual }),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let floor = make("floor", "vs_full", "fs_floor", &[], None, false, true);
        let add = make("add", "vs_main", "fs_main", &[Some(vlayout.clone())], Some(additive), false, false);
        let alpha = make("alpha", "vs_main", "fs_main", &[Some(vlayout)], Some(wgpu::BlendState::ALPHA_BLENDING), true, false);

        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("verts"),
            size: (MAX_QUADS * 4 * 5 * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let indices: Vec<u32> = (0..MAX_QUADS as u32).flat_map(|q| [0, 1, 2, 0, 2, 3].map(|i| q * 4 + i)).collect();
        let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-indices"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        // ── post chain ──
        let pp_shader = device.create_shader_module(wgpu::include_wgsl!("pp.wgsl"));
        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let pp_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                tex_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                tex_entry(3),
                tex_entry(4),
            ],
        });
        let pp_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&pp_bgl)],
            immediate_size: 0,
        });
        let pp_make = |fs: &str, fmt: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
                layout: Some(&pp_layout),
                vertex: wgpu::VertexState { module: &pp_shader, entry_point: Some("vs_full"), compilation_options: Default::default(), buffers: &[] },
                fragment: Some(wgpu::FragmentState {
                    module: &pp_shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: fmt, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pp_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let t = build_targets(&device, &pp_bgl, &pp_sampler, config.width, config.height);
        let post = Post {
            down: pp_make("fs_down", HDR),
            blur: pp_make("fs_blur", HDR),
            comp: pp_make("fs_comp", format),
            bgl: pp_bgl,
            sampler: pp_sampler,
            t,
        };

        let offscreen = capture.then(|| offscreen_target(&device, config.format, config.width, config.height));
        Gfx { window, surface, device, queue, config, floor, add, alpha, vbuf, ibuf, ubuf, bind, post, offscreen }
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.config.width = w;
            self.config.height = h;
            if self.offscreen.is_some() {
                self.offscreen = Some(offscreen_target(&self.device, self.config.format, w, h));
            } else {
                self.surface.configure(&self.device, &self.config);
            }
            self.post.t = build_targets(&self.device, &self.post.bgl, &self.post.sampler, w, h);
        }
    }

    /// Renders a frame; with `capture`, also reads it back as tightly packed 4-byte pixels in the surface's format.
    fn draw(&mut self, frame: &scene::Frame, uni: &Uniforms, capture: bool) -> Option<Vec<u8>> {
        let st = if self.offscreen.is_some() {
            None
        } else {
            match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    self.surface.configure(&self.device, &self.config);
                    return None;
                }
                _ => return None,
            }
        };
        let target = match (&self.offscreen, &st) {
            (Some(t), _) => t.clone(),
            (None, Some(s)) => s.texture.clone(),
            _ => return None,
        };
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // Upload all batches back-to-back into the one vertex buffer.
        let batches: [&Vec<Vertex>; 5] = [&frame.m, &frame.p, &frame.b, &frame.a, &frame.c];
        let vsz = std::mem::size_of::<Vertex>() as u64;
        let mut ranges = [(0u32, 0u32); 5];
        let mut off = 0u64;
        for (i, b) in batches.iter().enumerate() {
            let n = b.len().min(MAX_QUADS * 4);
            ranges[i] = ((off / vsz) as u32, (n / 4) as u32);
            if n > 0 {
                self.queue.write_buffer(&self.vbuf, off, bytemuck::cast_slice(&b[..n]));
            }
            off += n as u64 * vsz;
        }
        self.queue.write_buffer(&self.ubuf, 0, bytemuck::bytes_of(uni));

        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.post.t.scene,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.post.t.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_bind_group(0, &self.bind, &[]);
            rp.set_pipeline(&self.floor);
            rp.draw(0..3, 0..1);

            rp.set_vertex_buffer(0, self.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint32);
            // mirrors → solids (write depth) → behind-glows → front-glows/HUD → vignette
            let order = [(0usize, false), (2, true), (1, false), (3, false), (4, true)];
            for (i, is_alpha) in order {
                let (base, quads) = ranges[i];
                if quads == 0 {
                    continue;
                }
                rp.set_pipeline(if is_alpha { &self.alpha } else { &self.add });
                rp.draw_indexed(0..quads * 6, base as i32, 0..1);
            }
        }
        // bloom chain: bright-down → blur (¼ res) → down → blur (1/16 res) → composite
        {
            let t = &self.post.t;
            self.queue.write_buffer(&t.u_comp, 0, bytemuck::cast_slice(&[0.0f32, 0.0, 0.0, uni.wall[0]]));
            let fs_pass = |enc: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, pipe: &wgpu::RenderPipeline, bg: &wgpu::BindGroup| {
                let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_pipeline(pipe);
                rp.set_bind_group(0, bg, &[]);
                rp.draw(0..3, 0..1);
            };
            let [l1a, l1b, l2a, l2b] = &t.views;
            fs_pass(&mut enc, l1a, &self.post.down, &t.passes[0]);
            fs_pass(&mut enc, l1b, &self.post.blur, &t.passes[1]);
            fs_pass(&mut enc, l1a, &self.post.blur, &t.passes[2]);
            fs_pass(&mut enc, l2a, &self.post.down, &t.passes[3]);
            fs_pass(&mut enc, l2b, &self.post.blur, &t.passes[4]);
            fs_pass(&mut enc, l2a, &self.post.blur, &t.passes[5]);
            fs_pass(&mut enc, &view, &self.post.comp, &t.passes[6]);
        }
        let grab = (capture && (self.offscreen.is_some() || self.config.usage.contains(wgpu::TextureUsages::COPY_SRC))).then(|| {
            let (w, h) = (self.config.width, self.config.height);
            let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("grab"),
                size: (row * h) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            enc.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            (buf, row, w, h)
        });
        self.queue.submit(Some(enc.finish()));
        let out = grab.map(|(buf, row, w, h)| {
            buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
            let data = buf.slice(..).get_mapped_range().expect("map grab");
            let mut px = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h as usize {
                px.extend_from_slice(&data[y * row as usize..y * row as usize + w as usize * 4]);
            }
            px
        });
        if let Some(st) = st {
            self.window.pre_present_notify();
            self.queue.present(st);
        }
        out
    }
}

/// Which GPU to render on (`gpu` in the config, or GRIDFALL_GPU): the integrated, power-saving one unless "high".
/// A screensaver shouldn't wake a laptop's discrete GPU: bringing it out of deep sleep and keeping it busy is what
/// knocks some hybrid laptops' NVIDIA cards off the bus.
fn want_fast_gpu() -> bool {
    std::env::var("GRIDFALL_GPU").unwrap_or_else(|_| crate::brand::get().gpu.clone()) == "high"
}

/// A GPU other than an NVIDIA one is present (the integrated GPU of a hybrid laptop, or an AMD/Intel card).
fn has_other_gpu() -> bool {
    std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten().any(|e| {
        let vendor = std::fs::read_to_string(e.path().join("device/vendor")).unwrap_or_default();
        let v = vendor.trim();
        !v.is_empty() && v != "0x10de"
    })
}

/// Plugged in (or a machine without a battery).
fn on_mains() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return true };
    let mut battery = false;
    for e in dir.flatten() {
        let p = e.path();
        let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
        match kind.trim() {
            "Mains" if std::fs::read_to_string(p.join("online")).unwrap_or_default().trim() == "1" => return true,
            "Battery" => battery = true,
            _ => {}
        }
    }
    !battery
}

fn debug() -> bool {
    std::env::var_os("GRIDFALL_DEBUG").is_some()
}

/// GRIDFALL_DEBUG timing: frames and CPU milliseconds spent in update / scene build / draw since `since`
struct Prof {
    n: u32,
    upd: f32,
    build: f32,
    draw: f32,
    since: Instant,
}

struct App {
    prof: Prof,
    gfx: Option<Gfx>,
    scene: Scene,
    sim: sim::Sim,
    windowed: bool,
    /// test/preview only: ignore input so a capture isn't interrupted
    keep: bool,
    start: Instant,
    last: Instant,
    next_frame: Instant,
    /// minimum time between frames; zero = every display refresh (vsync)
    frame_dt: Duration,
    /// no --fps given: follow the display, halving to a steady rate if the GPU can't keep up
    adaptive: bool,
    refresh: f32,
    slow: u32,
    fps_acc: (u32, Instant),
    fps: f32,
    pointer0: Option<(f64, f64)>,
    grace_until: Instant,
    /// --dump: render on a fixed 60 fps clock and pipe every frame into ffmpeg
    dump: Option<Dump>,
}

struct Dump {
    path: String,
    secs: f32,
    frame: u32,
    /// output size in pixels; frames are only recorded while the surface has exactly this size
    size: (u32, u32),
    written: u32,
    ffmpeg: Option<std::process::Child>,
}

impl App {
    fn quit(&self, el: &ActiveEventLoop) {
        // Input lands on one monitor's instance only; take the other screensaver instances down with us (same binary,
        // whatever it is called), but leave recordings and windowed previews running.
        let me = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        let own = std::process::id();
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
            if pid == own || std::fs::read_to_string(e.path().join("comm")).unwrap_or_default().trim() != me.get(..15).unwrap_or(&me) {
                continue;
            }
            let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            let args: Vec<&[u8]> = cmd.split(|&b| b == 0).collect();
            if args.iter().any(|a| *a == b"--dump" || *a == b"--windowed") {
                continue;
            }
            let _ = std::process::Command::new("kill").arg(pid.to_string()).spawn();
        }
        el.exit();
    }

    fn render(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        let mut dt = (now - self.last).as_secs_f32();
        self.last = now;
        let mut t = (now - self.start).as_secs_f32();
        if let Some(d) = &self.dump {
            dt = 1.0 / 60.0;
            t = d.frame as f32 / 60.0;
        }
        self.sim.aspect = self.gfx.as_ref().map(|g| g.config.width as f32 / g.config.height as f32).unwrap_or(1.6);
        let t0 = Instant::now();
        self.sim.update(dt);
        self.prof.upd += t0.elapsed().as_secs_f32() * 1000.0;

        self.fps_acc.0 += 1;
        if now - self.fps_acc.1 >= Duration::from_millis(500) {
            self.fps = self.fps_acc.0 as f32 / (now - self.fps_acc.1).as_secs_f32();
            self.fps_acc = (0, now);
            // falling short of a fast display judders between full and half rate: settle on a steady half rate
            if self.adaptive && self.dump.is_none() && self.frame_dt.is_zero() && self.refresh >= 90.0 {
                self.slow = if self.fps < self.refresh * 0.9 { self.slow + 1 } else { 0 };
                if self.slow >= 4 {
                    self.frame_dt = Duration::from_secs_f32(1.0 / (self.refresh * 0.5));
                    if debug() {
                        eprintln!("gridfall: {:.0} fps on a {:.0} Hz display: pacing at {:.0}", self.fps, self.refresh, self.refresh * 0.5);
                    }
                }
            }
        }

        let Some(g) = self.gfx.as_mut() else { return };
        let (w, h) = (g.config.width as f32, g.config.height as f32);
        let fade = (t / 1.6).clamp(0.0, 1.0);
        let t1 = Instant::now();
        let (cam, pu, pc) = self.scene.build(&self.sim, t, w, h, if self.dump.is_some() { 60.0 } else { self.fps }, fade * fade * (3.0 - 2.0 * fade));
        let uni = Uniforms {
            inv_vp: cam.vp.inverse().to_cols_array_2d(),
            eye: cam.eye.extend(1.0).to_array(),
            params: [t, w / h, self.sim.scan_x, 0.0],
            pulse: pu.map(|v| v.to_array()),
            pcol: pc.map(|v| v.to_array()),
            extra: {
                let prog = self.sim.emerge_prog();
                let ghost = if self.sim.scenario == sim::Scenario::Emerge { 1.0 - ((prog - 0.42) / 0.5).clamp(0.0, 1.0) } else { 0.0 };
                let drift = if self.sim.scenario == sim::Scenario::Emerge { self.sim.scen_t * sim::EMERGE_CELL / 28.0 } else { 0.0 };
                let bare = if self.sim.scenario == sim::Scenario::Run { 1.0 } else { 0.0 }; // transit: no floor seams, just the trails
                [ghost, self.sim.tron, drift, bare]
            },
            wall_vp: {
                let (e0, t0, u0) = self.sim.emerge_start_cam();
                scene::camera(e0, t0, u0, w / h).0.vp.to_cols_array_2d()
            },
            wall: {
                let br = crate::brand::get();
                let em = self.sim.scenario == sim::Scenario::Emerge;
                let prog = self.sim.emerge_prog();
                // the real desktop holds until the world starts rising, then dissolves into the floor
                let a = if em && br.wallpaper.is_some() { 1.0 - ((prog - 0.42) / 0.5).clamp(0.0, 1.0) } else { 0.0 };
                let aspect = br.wallpaper.as_ref().map(|w| w.w as f32 / w.h as f32).unwrap_or(1.0);
                [a, aspect, if br.desktop_grid { 1.0 } else { 0.0 }, 0.0]
            },
        };
        let fmt = g.config.format;
        let (gw, gh) = (g.config.width, g.config.height);
        let t2 = Instant::now();
        let px = g.draw(&self.scene.frame, &uni, self.dump.is_some());
        let t3 = Instant::now();
        self.prof.build += (t2 - t1).as_secs_f32() * 1000.0;
        self.prof.draw += (t3 - t2).as_secs_f32() * 1000.0;
        self.prof.n += 1;
        if debug() && t3 - self.prof.since >= Duration::from_secs(2) {
            let n = self.prof.n as f32;
            let f = &self.scene.frame;
            let quads = (f.m.len() + f.p.len() + f.b.len() + f.a.len() + f.c.len()) / 4;
            eprintln!(
                "gridfall: {:5.1} fps  update {:.2} ms  build {:.2} ms  draw {:.2} ms  quads {}  shards {}  {:?} {}x{}",
                n / (t3 - self.prof.since).as_secs_f32(),
                self.prof.upd / n,
                self.prof.build / n,
                self.prof.draw / n,
                quads,
                self.sim.shards.len(),
                self.sim.scenario,
                gw,
                gh
            );
            self.prof = Prof { n: 0, upd: 0.0, build: 0.0, draw: 0.0, since: t3 };
        }
        if let Some(d) = self.dump.as_mut() {
            d.frame += 1;
            // let the window settle at its final size for a moment before the first recorded frame
            if let Some(px) = px.filter(|_| d.frame > 30) {
                if d.size == (gw, gh) {
                    d.written += 1;
                    let child = d.ffmpeg.get_or_insert_with(|| {
                        let pix = if matches!(fmt, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb) { "bgra" } else { "rgba" };
                        std::process::Command::new("ffmpeg")
                            .args(["-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", pix, "-s", &format!("{gw}x{gh}"), "-r", "60", "-i", "-"])
                            // BT.709 conversion and tags: untagged video gets decoded as BT.709 by players while ffmpeg converts with
                            // BT.601 by default, which shifts every hue
                            .args(["-vf", "scale=out_color_matrix=bt709:out_range=tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"])
                            .args(["-c:v", "libx264", "-preset", "slow", "-crf", "16", "-pix_fmt", "yuv420p", "-profile:v", "high", "-movflags", "+faststart", &d.path])
                            .stdin(std::process::Stdio::piped())
                            .spawn()
                            .expect("ffmpeg")
                    });
                    use std::io::Write;
                    let _ = child.stdin.as_mut().map(|s| s.write_all(&px));
                }
            }
            if d.written as f32 >= d.secs * 60.0 {
                if let Some(mut c) = d.ffmpeg.take() {
                    drop(c.stdin.take());
                    let _ = c.wait();
                }
                el.exit();
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        // a recording instance gets its own title so a compositor rule can route it off-screen without touching the screensaver
        let title = if self.dump.is_some() { "gridfall-rec" } else { APP_ID };
        let mut attrs = Window::default_attributes().with_title(title).with_decorations(false);
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs = attrs.with_name(APP_ID, APP_ID);
        }
        if let Some(d) = &self.dump {
            attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(d.size.0, d.size.1));
        } else if !self.windowed {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        }
        let window = Arc::new(el.create_window(attrs).expect("window"));
        self.refresh = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).map(|r| r as f32 / 1000.0).unwrap_or(60.0);
        if self.adaptive && self.dump.is_none() && !on_mains() && self.refresh >= 90.0 {
            self.frame_dt = Duration::from_secs_f32(1.0 / (self.refresh * 0.5));
        }
        if !self.windowed {
            window.set_cursor_visible(false);
        }
        self.gfx = Some(Gfx::new(window, &self.scene.atlas, self.dump.is_some()));
        if let (Some(g), Some(d)) = (self.gfx.as_mut(), &self.dump) {
            g.resize(d.size.0, d.size.1);
        }
        let now = Instant::now();
        self.start = now;
        self.last = now;
        self.grace_until = now + Duration::from_millis(600);
        self.fps_acc = (0, now);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        let live = !self.windowed && !self.keep && Instant::now() > self.grace_until;
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => {
                // a recording keeps its own frame size whatever the compositor gives the window
                let (w, h) = self.dump.as_ref().map(|d| d.size).unwrap_or((s.width, s.height));
                if let Some(g) = self.gfx.as_mut() {
                    g.resize(w, h);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if self.windowed {
                    if matches!(event.logical_key, winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape)) {
                        el.exit();
                    }
                } else if live {
                    self.quit(el);
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, .. } | WindowEvent::Touch(_) if live => self.quit(el),
            WindowEvent::CursorMoved { position, .. } if live => match self.pointer0 {
                None => self.pointer0 = Some((position.x, position.y)),
                Some((x, y)) => {
                    if (position.x - x).abs() > 18.0 || (position.y - y).abs() > 18.0 {
                        self.quit(el);
                    }
                }
            },
            WindowEvent::RedrawRequested if self.dump.is_none() => self.render(el),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.dump.is_some() {
            // recording: as fast as the GPU goes, independent of the window and the compositor
            if self.gfx.is_some() {
                self.render(el);
            }
            el.set_control_flow(ControlFlow::Poll);
            return;
        }
        if self.frame_dt.is_zero() {
            // paced by the display: presenting blocks until the next refresh
            if let Some(g) = &self.gfx {
                g.window.request_redraw();
            }
            el.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        if now >= self.next_frame {
            self.next_frame = (self.next_frame + self.frame_dt).max(now);
            if let Some(g) = &self.gfx {
                g.window.request_redraw();
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Keep a hybrid laptop's NVIDIA card asleep: the Vulkan loader then never even loads its driver (merely listing
    // the GPUs would wake it). Set before any thread starts.
    if !want_fast_gpu() && has_other_gpu() && std::env::var_os("VK_LOADER_DRIVERS_DISABLE").is_none() {
        // SAFETY: single-threaded at this point; nothing else reads the environment concurrently
        unsafe { std::env::set_var("VK_LOADER_DRIVERS_DISABLE", "*nvidia*") };
    }
    let windowed = args.iter().any(|a| a == "--windowed");
    // --fps caps the frame rate; without it Gridfall renders at the display's refresh rate
    let fps: Option<f64> = args.iter().position(|a| a == "--fps").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok());
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7);

    let el = EventLoop::new().expect("event loop");
    let now = Instant::now();
    let mut app = App {
        prof: Prof { n: 0, upd: 0.0, build: 0.0, draw: 0.0, since: now },
        gfx: None,
        scene: Scene::new(),
        sim: sim::Sim::new(seed),
        windowed,
        keep: args.iter().any(|a| a == "--keep"),
        start: now,
        last: now,
        next_frame: now,
        frame_dt: fps.map(|f| Duration::from_secs_f64(1.0 / f.clamp(10.0, 240.0))).unwrap_or(Duration::ZERO),
        adaptive: fps.is_none(),
        // on battery a screensaver shouldn't drain the laptop: start at half the display rate (see resumed)
        refresh: 60.0,
        slow: 0,
        fps_acc: (0, now),
        fps: fps.unwrap_or(60.0) as f32,
        pointer0: None,
        grace_until: now,
        dump: args.iter().position(|a| a == "--dump").and_then(|i| args.get(i + 1)).map(|p| Dump {
            path: p.clone(),
            secs: args.iter().position(|a| a == "--secs").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(60.0),
            frame: 0,
            size: args.iter().position(|a| a == "--size").and_then(|i| args.get(i + 1)).and_then(|v| v.split_once('x')).and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))).unwrap_or((1920, 1080)),
            written: 0,
            ffmpeg: None,
        }),
    };
    // --record: hold the light-cycle arena and cycle its camera angles, with a larger HUD for a downscaled video
    if args.iter().any(|a| a == "--record") {
        app.sim.hold_arena = true;
        app.scene.hud_scale = 1.4;
        app.sim.jump(sim::Scenario::Arena);
    }
    if let Some(s) = args.iter().position(|a| a == "--hud").and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f32>().ok()) {
        app.scene.hud_scale = s;
    }
    if let Some(i) = args.iter().position(|a| a == "--scenario") {
        match args.get(i + 1).map(|s| s.as_str()) {
            Some("run") | Some("transit") => app.sim.jump(sim::Scenario::Run),
            Some("twin") => app.sim.jump(sim::Scenario::Twin),
            Some("arena") => app.sim.jump(sim::Scenario::Arena),
            Some("emerge") => {}
            _ => {}
        }
    }
    el.run_app(&mut app).expect("run");
}
