//! Draws I420 frames into a window with wgpu: three R8 textures, YUV→RGB
//! (BT.709 limited range) in the fragment shader, letterboxed to keep the
//! aspect ratio.

use std::sync::Arc;

use anyhow::{Context, bail};
use reemoth_video::I420Frame;
use winit::window::Window;

const SHADER: &str = r#"
struct Params {
    // Fraction of the window the picture covers on each axis.
    scale: vec2<f32>,
    // 1.0 when the surface is sRGB and expects linear output.
    srgb: f32,
    _pad: f32,
};
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // Two triangles covering the letterboxed rectangle.
    var corners = array<vec2<f32>, 6>(
        vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(-1.0, 1.0),
        vec2(-1.0, 1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
    );
    let c = corners[i];
    var out: VsOut;
    out.pos = vec4(c * params.scale, 0.0, 1.0);
    out.uv = vec2((c.x + 1.0) * 0.5, (1.0 - c.y) * 0.5);
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(tex_y, samp, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (textureSample(tex_u, samp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (textureSample(tex_v, samp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    var rgb = clamp(vec3(
        y + 1.5748 * v,
        y - 0.1873 * u - 0.4681 * v,
        y + 1.8556 * u,
    ), vec3(0.0), vec3(1.0));
    if (params.srgb > 0.5) {
        rgb = pow(rgb, vec3(2.2));
    }
    return vec4(rgb, 1.0);
}
"#;

struct Planes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    planes: Option<Planes>,
    srgb: bool,
    pub adapter_name: String,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(window.clone())
            .context("creating window surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .context("no GPU adapter can draw to this window (Vulkan or OpenGL drivers needed)")?;
        let adapter_name = format!(
            "{} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("reemoth"),
            required_features: wgpu::Features::empty(),
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .context("opening GPU device")?;

        let caps = surface.get_capabilities(&adapter);
        if caps.formats.is_empty() {
            bail!("the window surface supports no formats");
        }
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Mailbox/Immediate show a frame as soon as it is drawn; Fifo waits for vblank.
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
                tex_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        // Linear filtering only matters when the window is not 1:1 with the stream.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler,
            params,
            planes: None,
            srgb: format.is_srgb(),
            adapter_name,
        })
    }

    /// Where the picture is drawn, in window pixels: (left, top, width, height).
    pub fn picture_rect(&self) -> Option<(f64, f64, f64, f64)> {
        let p = self.planes.as_ref()?;
        let (cw, ch) = (f64::from(self.config.width), f64::from(self.config.height));
        let (sx, sy) = fit(p.width, p.height, self.config.width, self.config.height);
        let (w, h) = (f64::from(sx) * cw, f64::from(sy) * ch);
        Some(((cw - w) / 2.0, (ch - h) / 2.0, w, h))
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Uploads `frame` (if any) and draws the latest picture.
    pub fn draw(&mut self, frame: Option<&I420Frame>) -> anyhow::Result<()> {
        if let Some(f) = frame {
            self.upload(f);
        }
        let output = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(e) => bail!("acquiring the next frame to draw: {e}"),
        };
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if let Some(planes) = &self.planes {
                let (sx, sy) = fit(
                    planes.width,
                    planes.height,
                    self.config.width,
                    self.config.height,
                );
                let mut params = [0u8; 16];
                params[0..4].copy_from_slice(&sx.to_le_bytes());
                params[4..8].copy_from_slice(&sy.to_le_bytes());
                params[8..12]
                    .copy_from_slice(&(if self.srgb { 1.0f32 } else { 0.0 }).to_le_bytes());
                self.queue.write_buffer(&self.params, 0, &params);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &planes.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        output.present();
        Ok(())
    }

    fn upload(&mut self, f: &I420Frame) {
        let (w, h) = (f.width as u32, f.height as u32);
        if self
            .planes
            .as_ref()
            .is_none_or(|p| (p.width, p.height) != (w, h))
        {
            self.planes = Some(self.create_planes(w, h));
        }
        let planes = self.planes.as_ref().expect("just created");
        for (tex, data, pw, ph) in [
            (&planes.y, &f.y, w, h),
            (&planes.u, &f.u, w / 2, h / 2),
            (&planes.v, &f.v, w / 2, h / 2),
        ] {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pw),
                    rows_per_image: Some(ph),
                },
                wgpu::Extent3d {
                    width: pw,
                    height: ph,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    fn create_planes(&self, w: u32, h: u32) -> Planes {
        let make = |label, width, height| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y = make("y", w, h);
        let u = make("u", w / 2, h / 2);
        let v = make("v", w / 2, h / 2);
        let views = [&y, &u, &v].map(|t| t.create_view(&Default::default()));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Planes {
            width: w,
            height: h,
            y,
            u,
            v,
            bind_group,
        }
    }
}

/// Clip-space scale that fits a `pw`×`ph` picture into a `ww`×`wh` window.
fn fit(pw: u32, ph: u32, ww: u32, wh: u32) -> (f32, f32) {
    let pic = pw as f32 / ph as f32;
    let win = ww as f32 / wh as f32;
    if win > pic {
        (pic / win, 1.0)
    } else {
        (1.0, win / pic)
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn letterboxes() {
        assert_eq!(fit(1920, 1080, 1920, 1080), (1.0, 1.0));
        let (x, y) = fit(1920, 1080, 1000, 1000);
        assert_eq!(x, 1.0);
        assert!((y - 0.5625).abs() < 1e-4);
        let (x, y) = fit(1920, 1080, 2560, 1080);
        assert!((x - 0.75).abs() < 1e-4 && y == 1.0);
    }
}
