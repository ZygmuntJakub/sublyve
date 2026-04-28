use std::path::Path;

use anyhow::Result;
use avengine_compositor::{GpuContext, Uniforms, VideoPipelines, VideoTexture};
use avengine_core::BlendMode;
use avengine_playback::{Decoder, StreamInfo, Transport};
use tracing::{error, warn};
use wgpu::util::DeviceExt;

use crate::library::ClipDefaults;

/// One row of the grid: an independent decoder + transport that draws
/// into the shared `CompositionTarget` with a chosen blend mode and
/// opacity. Multiple `Layer`s play simultaneously and composite back-to-front.
pub struct Layer {
    pub blend_mode: BlendMode,
    pub opacity: f32,
    pub mute: bool,

    /// Column of the currently-loaded clip (or `None` if the layer is empty).
    pub active_col: Option<usize>,

    pub transport: Transport,
    pub info: Option<StreamInfo>,

    decoder: Option<Decoder>,
    frame_period: f64,
    /// Per-layer wall-clock catch-up accumulator. Bounded by the global
    /// `MAX_CATCHUP_SECS` clamp applied in `tick`.
    catchup: f64,

    pub video_texture: VideoTexture,
    /// Generation of `video_texture` the bind group was built for.
    bound_video_gen: u64,
    bind_group: Option<wgpu::BindGroup>,

    uniforms_buffer: wgpu::Buffer,
}

impl Layer {
    pub fn new(gpu: &GpuContext) -> Self {
        let video_texture = VideoTexture::placeholder(&gpu.device);
        let uniforms_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("avengine.layer.uniforms"),
            contents: bytemuck::bytes_of(&Uniforms::new([1.0, 1.0], 1.0)),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        Self {
            blend_mode: BlendMode::Normal,
            opacity: 1.0,
            mute: false,
            active_col: None,
            transport: Transport::new(),
            info: None,
            decoder: None,
            frame_period: 1.0 / 30.0,
            catchup: 0.0,
            video_texture,
            bound_video_gen: u64::MAX,
            bind_group: None,
            uniforms_buffer,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.decoder.is_none()
    }

    pub fn is_visible(&self) -> bool {
        !self.mute && !self.is_empty() && self.opacity > 0.001
    }

    /// Load a clip into this layer. Replaces any existing decoder.
    ///
    /// `defaults` are written into the transport (looping, speed) and the
    /// layer's `blend_mode` on entry, so triggering a clip always yields
    /// its declared default behaviour. The user can still override these
    /// from the right-hand layer inspector mid-playback; the next trigger
    /// will reset them again.
    ///
    /// The first frame is decoded synchronously and uploaded to the
    /// layer's `VideoTexture` so the next composition render isn't black.
    pub fn load(
        &mut self,
        gpu: &GpuContext,
        path: &Path,
        col: usize,
        defaults: ClipDefaults,
    ) -> Result<()> {
        let mut decoder = Decoder::open(path)?;
        let info = decoder.info();
        self.frame_period = 1.0 / info.frame_rate.max(1e-3);
        self.transport = Transport::new();
        self.transport.playing = true;
        self.transport.looping = defaults.looping;
        self.transport.speed = defaults.speed;
        self.blend_mode = defaults.blend;
        self.catchup = 0.0;
        self.active_col = Some(col);

        if let Some(frame) = decoder.next_frame()? {
            self.transport.position = frame.pts;
            self.video_texture.upload(&gpu.device, &gpu.queue, &frame);
        }

        self.info = Some(info);
        self.decoder = Some(decoder);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.decoder = None;
        self.info = None;
        self.active_col = None;
        self.transport.playing = false;
        self.catchup = 0.0;
    }

    pub fn restart(&mut self) {
        if let Some(d) = self.decoder.as_mut() {
            if let Err(e) = d.seek(0.0) {
                warn!("layer restart seek failed: {e}");
            }
            self.transport.position = 0.0;
            self.transport.playing = true;
            self.catchup = 0.0;
        }
    }

    /// Pump the decoder for `dt` seconds of wall-clock time. Uploads the
    /// freshest decoded frame to the layer's `VideoTexture`. Drops the
    /// catch-up to zero on loop / EOF.
    pub fn tick(&mut self, gpu: &GpuContext, dt: f64) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        if !self.transport.playing {
            return;
        }
        let period = self.frame_period / self.transport.speed.max(1e-3);
        self.catchup += dt;
        while self.catchup >= period {
            self.catchup -= period;
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    self.transport.position = frame.pts;
                    self.video_texture.upload(&gpu.device, &gpu.queue, &frame);
                }
                Ok(None) => {
                    if self.transport.looping {
                        if let Err(e) = decoder.seek(0.0) {
                            warn!("layer loop-seek failed: {e}");
                            self.transport.playing = false;
                        }
                        self.transport.position = 0.0;
                    } else {
                        self.transport.playing = false;
                    }
                    self.catchup = 0.0;
                    break;
                }
                Err(e) => {
                    error!("layer decode error: {e}");
                    self.transport.playing = false;
                    self.catchup = 0.0;
                    break;
                }
            }
        }
    }

    /// Refresh the per-layer uniforms (opacity + composition-fit scale)
    /// and rebuild the bind group when the video texture has been
    /// reallocated. `composition_size` is the size of the render target
    /// we're about to draw into, used to letterbox the video.
    pub fn prepare_draw(
        &mut self,
        gpu: &GpuContext,
        pipelines: &VideoPipelines,
        composition_size: (u32, u32),
    ) {
        let scale = letterbox_scale(
            self.video_texture.size().0,
            self.video_texture.size().1,
            composition_size.0,
            composition_size.1,
        );
        gpu.queue.write_buffer(
            &self.uniforms_buffer,
            0,
            bytemuck::bytes_of(&Uniforms::new(scale, self.opacity)),
        );

        if self.bound_video_gen != self.video_texture.generation() || self.bind_group.is_none() {
            self.bind_group = Some(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("avengine.layer.bind_group"),
                layout: &pipelines.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(self.video_texture.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&pipelines.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.uniforms_buffer.as_entire_binding(),
                    },
                ],
            }));
            self.bound_video_gen = self.video_texture.generation();
        }
    }

    pub fn draw(&self, rpass: &mut wgpu::RenderPass<'_>, pipelines: &VideoPipelines) {
        if !self.is_visible() {
            return;
        }
        let Some(bg) = self.bind_group.as_ref() else {
            return;
        };
        rpass.set_pipeline(pipelines.pipeline_for(self.blend_mode));
        rpass.set_bind_group(0, bg, &[]);
        rpass.set_vertex_buffer(0, pipelines.vertex_buffer.slice(..));
        rpass.draw(0..6, 0..1);
    }
}

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
