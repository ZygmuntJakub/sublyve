use std::sync::Arc;

use avengine_core::AvError;
use tracing::{debug, warn};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::gpu::GpuContext;
use crate::pipeline::{Uniforms, VideoPipeline};
use crate::video_texture::VideoTexture;

pub enum AcquiredFrame {
    Ready(wgpu::SurfaceTexture),
    Skip,
}

/// A drawable window: owns the wgpu `Surface`, its config, and the
/// per-window letterbox uniform. Bind groups are rebuilt lazily whenever
/// the shared `VideoTexture` is reallocated (size change).
pub struct WindowSurface {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,

    uniforms_buffer: wgpu::Buffer,
    bind_group: Option<wgpu::BindGroup>,
    /// `VideoTexture::generation()` value that `bind_group` was built for.
    /// `u64::MAX` means "never built". Mismatch triggers a rebuild.
    bound_generation: u64,
}

impl WindowSurface {
    /// Wrap an already-created `wgpu::Surface`. Surface creation is the
    /// caller's responsibility because the *first* surface has to exist
    /// before `GpuContext::with_instance` can pick a compatible adapter;
    /// that same flow then constructs every subsequent surface from the
    /// shared `gpu.instance`.
    pub fn new(
        gpu: &GpuContext,
        window: Arc<Window>,
        surface: wgpu::Surface<'static>,
    ) -> Result<Self, AvError> {
        let size = window.inner_size();

        let caps = surface.get_capabilities(&gpu.adapter);
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
        surface.configure(&gpu.device, &config);
        debug!(?format, w = config.width, h = config.height, "surface configured");

        let uniforms_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("avengine.surface.uniforms"),
            contents: bytemuck::bytes_of(&Uniforms { scale: [1.0, 1.0], _pad: [0.0; 2] }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        Ok(Self {
            window,
            surface,
            config,
            uniforms_buffer,
            bind_group: None,
            bound_generation: u64::MAX,
        })
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, gpu: &GpuContext, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&gpu.device, &self.config);
    }

    pub fn acquire(&mut self, gpu: &GpuContext) -> Result<AcquiredFrame, AvError> {
        match self.surface.get_current_texture() {
            Ok(t) => Ok(AcquiredFrame::Ready(t)),
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                warn!("surface lost/outdated; reconfiguring");
                self.surface.configure(&gpu.device, &self.config);
                Ok(AcquiredFrame::Skip)
            }
            Err(wgpu::SurfaceError::Timeout) => {
                warn!("surface timeout; skipping frame");
                Ok(AcquiredFrame::Skip)
            }
            Err(e @ wgpu::SurfaceError::OutOfMemory) => Err(AvError::gpu(e)),
        }
    }

    /// Refresh the per-window letterbox uniform and rebuild the bind group
    /// if the shared video texture has been reallocated. Call once per
    /// frame before `draw_video`.
    pub fn prepare_video(
        &mut self,
        gpu: &GpuContext,
        pipeline: &VideoPipeline,
        video: &VideoTexture,
    ) {
        let scale = letterbox_scale(
            video.size().0,
            video.size().1,
            self.config.width,
            self.config.height,
        );
        gpu.queue.write_buffer(
            &self.uniforms_buffer,
            0,
            bytemuck::bytes_of(&Uniforms { scale, _pad: [0.0; 2] }),
        );

        if self.bound_generation != video.generation() || self.bind_group.is_none() {
            self.bind_group = Some(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("avengine.surface.bind_group"),
                layout: &pipeline.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(video.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.uniforms_buffer.as_entire_binding(),
                    },
                ],
            }));
            self.bound_generation = video.generation();
        }
    }

    pub fn draw_video(&self, rpass: &mut wgpu::RenderPass<'_>, pipeline: &VideoPipeline) {
        let Some(bg) = self.bind_group.as_ref() else {
            return;
        };
        rpass.set_pipeline(&pipeline.pipeline);
        rpass.set_bind_group(0, bg, &[]);
        rpass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..));
        rpass.draw(0..6, 0..1);
    }
}

fn pick_surface_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    caps.formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .unwrap_or(caps.formats[0])
}

/// Per-axis NDC scale so a `(vw, vh)` video fits inside an `(sw, sh)`
/// surface while preserving aspect ratio (letterbox/pillarbox).
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

#[cfg(test)]
mod tests {
    use super::letterbox_scale;

    #[test]
    fn fills_when_aspects_match() {
        assert_eq!(letterbox_scale(1920, 1080, 3840, 2160), [1.0, 1.0]);
    }

    #[test]
    fn pillarboxes_wide_surface_around_narrow_video() {
        let s = letterbox_scale(1000, 1000, 2000, 1000);
        assert!((s[0] - 0.5).abs() < 1e-6);
        assert!((s[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn letterboxes_tall_surface_with_wide_video() {
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
