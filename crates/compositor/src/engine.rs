use std::sync::Arc;

use avengine_core::{AvError, VideoFrame};
use tracing::{debug, warn};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::quad::{QUAD_VERTICES, Vertex};

/// What `acquire` returns: a surface frame ready to render into, or a hint
/// that the caller should skip this frame (e.g. swapchain just resized).
pub enum AcquiredFrame {
    Ready(wgpu::SurfaceTexture),
    Skip,
}

/// Owns the wgpu surface and the fullscreen-quad pipeline that draws the
/// active video frame. Higher-level UI (egui) is composited by the caller
/// inside the same render pass returned by `acquire`.
pub struct Engine {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,

    uniforms_buffer: wgpu::Buffer,
    video_texture: Option<wgpu::Texture>,
    video_bind_group: Option<wgpu::BindGroup>,
    video_size: Option<(u32, u32)>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    scale: [f32; 2],
    _pad: [f32; 2],
}

impl Engine {
    pub async fn new(window: Window) -> Result<Self, AvError> {
        let window = Arc::new(window);
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let surface = instance
            .create_surface(window.clone())
            .map_err(AvError::gpu)?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| AvError::gpu("no suitable GPU adapter"))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("avengine.device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .map_err(AvError::gpu)?;

        let caps = surface.get_capabilities(&adapter);
        let format = pick_surface_format(&caps);
        let alpha_mode = caps.alpha_modes[0];

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        debug!(?format, ?alpha_mode, w = config.width, h = config.height, "surface configured");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("avengine.quad.shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/quad.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("avengine.quad.bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<Uniforms>() as u64
                        ),
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("avengine.quad.layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("avengine.quad.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Vertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            multisample: wgpu::MultisampleState::default(),
            depth_stencil: None,
            multiview: None,
            cache: None,
        });

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("avengine.quad.vbo"),
            contents: bytemuck::cast_slice(&QUAD_VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("avengine.video.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let uniforms_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("avengine.quad.uniforms"),
            contents: bytemuck::bytes_of(&Uniforms { scale: [1.0, 1.0], _pad: [0.0; 2] }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            vertex_buffer,
            sampler,
            bind_group_layout,
            uniforms_buffer,
            video_texture: None,
            video_bind_group: None,
            video_size: None,
        })
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    pub fn surface_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.recompute_scale();
    }

    /// Upload an RGBA frame to the GPU. Reuses the texture across calls when
    /// the dimensions are unchanged so the steady-state cost is one
    /// `write_texture` per frame.
    pub fn upload_frame(&mut self, frame: &VideoFrame) {
        let new_size = (frame.width, frame.height);
        let needs_alloc = self.video_size != Some(new_size);

        if needs_alloc {
            let extent = wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            };

            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("avengine.video.texture"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // Sample as sRGB so that the GPU performs sRGB→linear decode
                // before fragment shading and re-encodes on write to the
                // sRGB-capable surface. This keeps perceptual colors stable
                // and avoids the double-gamma artefacts that show up when
                // sampling Rgba8Unorm into an sRGB target.
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("avengine.video.bind_group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.uniforms_buffer.as_entire_binding(),
                    },
                ],
            });

            self.video_texture = Some(texture);
            self.video_bind_group = Some(bind_group);
            self.video_size = Some(new_size);
            self.recompute_scale();
        }

        let texture = self
            .video_texture
            .as_ref()
            .expect("video_texture must exist after allocation above");

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(frame.row_bytes()),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Acquire the next swapchain texture. Returns `Skip` for transient
    /// errors (lost/outdated swapchain) so the caller can drop this frame
    /// rather than panicking.
    pub fn acquire(&mut self) -> Result<AcquiredFrame, AvError> {
        match self.surface.get_current_texture() {
            Ok(frame) => Ok(AcquiredFrame::Ready(frame)),
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                warn!("surface lost/outdated; reconfiguring");
                self.surface.configure(&self.device, &self.config);
                Ok(AcquiredFrame::Skip)
            }
            Err(wgpu::SurfaceError::Timeout) => {
                warn!("surface timeout; skipping frame");
                Ok(AcquiredFrame::Skip)
            }
            Err(e @ wgpu::SurfaceError::OutOfMemory) => Err(AvError::gpu(e)),
        }
    }

    /// Record the video draw commands into an in-flight render pass. No-op
    /// if no frame has been uploaded yet. The pass lifetime is unconstrained
    /// because wgpu 23's `RenderPass` is internally reference-counted; we
    /// don't tie pipeline/bind-group lifetimes to `&self`.
    pub fn draw_video(&self, rpass: &mut wgpu::RenderPass<'_>) {
        let Some(bind_group) = self.video_bind_group.as_ref() else {
            return;
        };
        rpass.set_pipeline(&self.pipeline);
        rpass.set_bind_group(0, bind_group, &[]);
        rpass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        rpass.draw(0..6, 0..1);
    }

    fn recompute_scale(&mut self) {
        let Some((vw, vh)) = self.video_size else {
            return;
        };
        let scale = letterbox_scale(vw, vh, self.config.width, self.config.height);
        self.queue.write_buffer(
            &self.uniforms_buffer,
            0,
            bytemuck::bytes_of(&Uniforms { scale, _pad: [0.0; 2] }),
        );
    }
}

/// Compute per-axis NDC scale so a video of `(vw, vh)` fits inside a
/// surface of `(sw, sh)` while preserving aspect ratio (letterbox/pillarbox).
fn letterbox_scale(vw: u32, vh: u32, sw: u32, sh: u32) -> [f32; 2] {
    if vw == 0 || vh == 0 || sw == 0 || sh == 0 {
        return [1.0, 1.0];
    }
    let video = vw as f32 / vh as f32;
    let surface = sw as f32 / sh as f32;
    if video > surface {
        [1.0, surface / video]
    } else {
        [video / surface, 1.0]
    }
}

fn pick_surface_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    // Prefer an sRGB format so that egui's sRGB-aware shader and our
    // sRGB video texture line up without per-platform color shifts.
    caps.formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .unwrap_or(caps.formats[0])
}

#[cfg(test)]
mod tests {
    use super::letterbox_scale;

    #[test]
    fn fills_when_aspects_match() {
        assert_eq!(letterbox_scale(1920, 1080, 3840, 2160), [1.0, 1.0]);
    }

    #[test]
    fn pillarboxes_wide_surface_around_narrow_video() {
        // 1:1 video on 2:1 surface → x shrinks to 0.5, y full.
        let s = letterbox_scale(1000, 1000, 2000, 1000);
        assert!((s[0] - 0.5).abs() < 1e-6);
        assert!((s[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn letterboxes_tall_surface_with_wide_video() {
        // 2:1 video on 1:1 surface → x full, y shrinks to 0.5.
        let s = letterbox_scale(2000, 1000, 1000, 1000);
        assert!((s[0] - 1.0).abs() < 1e-6);
        assert!((s[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn zero_dim_is_safe() {
        assert_eq!(letterbox_scale(0, 0, 0, 0), [1.0, 1.0]);
        assert_eq!(letterbox_scale(1920, 1080, 0, 1080), [1.0, 1.0]);
    }
}
