use std::path::Path;

use anyhow::Result;
use avengine_compositor::{GpuContext, Uniforms, VideoPipelines, VideoTexture};
use avengine_core::BlendMode;
use avengine_playback::{AudioConfig, Decoder, StreamInfo, Transport};
use ringbuf::traits::Observer;
use tracing::{error, warn};
use wgpu::util::DeviceExt;

use crate::audio::AudioLayerHandle;
use crate::library::ClipDefaults;

/// How aggressively the layer drains its decoder's pending audio
/// samples into the SPSC ring buffer per `tick`. We try to keep the
/// buffer at least half-full so the cpal callback never starves.
const AUDIO_PUMP_TARGET_FILL: f32 = 0.5;

/// One row of the grid: an independent decoder + transport that draws
/// into the shared `CompositionTarget` with a chosen blend mode and
/// opacity. Multiple `Layer`s play simultaneously and composite back-to-front.
pub struct Layer {
    pub blend_mode: BlendMode,
    pub opacity: f32,
    /// Layer "master" multiplier (0.0..=1.0) applied on top of both the
    /// per-layer `opacity` (visual) and `audio_gain` (audio) — the
    /// equivalent of a DJ channel fader. `1.0` is "fully present", `0.0`
    /// is "completely faded out". Drives the live quick-controls strip.
    pub master: f32,
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

    /// Producer side of the per-layer audio ring buffer. Decoded +
    /// resampled samples land here via `tick`; the cpal callback drains
    /// them. `None` for the preview deck (which doesn't route to output).
    audio: Option<AudioLayerHandle>,
    /// Engine audio config — used by `Layer::load` to open the decoder
    /// with audio support matching the cpal stream.
    audio_config: Option<AudioConfig>,
    /// Scratch buffer reused across pumps to avoid per-tick allocs.
    audio_scratch: Vec<f32>,
}

impl Layer {
    /// Build a layer with no audio routing. Used for the preview deck —
    /// its frames go to the Cue pane but never to the output mixer.
    pub fn new_silent(gpu: &GpuContext) -> Self {
        Self::build(gpu, None, None)
    }

    /// Build a layer wired into the audio engine: decoded audio is
    /// resampled to `audio_config` and pushed into `audio.producer`.
    pub fn new(gpu: &GpuContext, audio: AudioLayerHandle, audio_config: AudioConfig) -> Self {
        Self::build(gpu, Some(audio), Some(audio_config))
    }

    fn build(
        gpu: &GpuContext,
        audio: Option<AudioLayerHandle>,
        audio_config: Option<AudioConfig>,
    ) -> Self {
        let video_texture = VideoTexture::placeholder(&gpu.device);
        let uniforms_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("avengine.layer.uniforms"),
            contents: bytemuck::bytes_of(&Uniforms::new([1.0, 1.0], 1.0)),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        Self {
            blend_mode: BlendMode::Normal,
            opacity: 1.0,
            master: 1.0,
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
            audio,
            audio_config,
            audio_scratch: Vec::new(),
        }
    }

    /// Update the layer master and propagate to the audio control so
    /// the cpal callback applies the same multiplier on the audio side.
    pub fn set_master(&mut self, master: f32) {
        let m = master.clamp(0.0, 1.0);
        self.master = m;
        if let Some(a) = self.audio.as_ref() {
            a.control.set_master(m);
        }
    }

    /// Per-layer audio gain (1.0 = unity). Routed through the audio
    /// engine's atomic so the cpal callback picks it up without locks.
    pub fn audio_gain(&self) -> f32 {
        self.audio.as_ref().map_or(1.0, |a| a.control.gain())
    }

    pub fn set_audio_gain(&self, gain: f32) {
        if let Some(a) = self.audio.as_ref() {
            a.control.set_gain(gain.clamp(0.0, 4.0));
        }
    }

    /// Hand back the layer's current audio control so a stream rebuild
    /// can keep gain / mute settings stable across device switches.
    pub fn audio_control(&self) -> Option<std::sync::Arc<crate::audio::AudioLayerControl>> {
        self.audio.as_ref().map(|a| a.control.clone())
    }

    /// Swap in a new producer handle (after a stream rebuild) without
    /// touching anything else on the layer. The caller is responsible
    /// for keeping the matching consumer alive in the new cpal callback.
    pub fn replace_audio_handle(&mut self, handle: AudioLayerHandle) {
        self.audio = Some(handle);
    }

    pub fn is_empty(&self) -> bool {
        self.decoder.is_none()
    }

    pub fn is_visible(&self) -> bool {
        !self.mute && !self.is_empty() && self.opacity * self.master > 0.001
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
        // If we're wired into the audio engine, open with audio so the
        // decoder demuxes both streams in a single pass; otherwise the
        // existing video-only path keeps preview-deck loads cheap.
        let mut decoder = match self.audio_config {
            Some(cfg) => Decoder::open_av(path, cfg)?,
            None => Decoder::open(path)?,
        };
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

        // Pump any audio that was demuxed alongside the first video
        // frame into the ring buffer so playback starts in sync.
        self.flush_audio_to_ring();
        Ok(())
    }

    pub fn clear(&mut self) {
        self.decoder = None;
        self.info = None;
        self.active_col = None;
        self.transport.playing = false;
        self.catchup = 0.0;
        // Stale audio in the ring buffer would briefly play after the
        // next clip is loaded; we accept up to ~340 ms of glitching
        // here for V1 (ringbuf 0.4 producer has no clear-from-this-side
        // primitive).
    }

    /// Set the mute flag on both the video side (skips draws) and the
    /// audio side (the cpal callback skips this layer's samples).
    pub fn set_mute(&mut self, muted: bool) {
        self.mute = muted;
        if let Some(a) = self.audio.as_ref() {
            a.control.set_muted(muted);
        }
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
    /// freshest decoded frame to the layer's `VideoTexture` and drains
    /// any decoded audio into the ring buffer. Drops the catch-up to
    /// zero on loop / EOF.
    pub fn tick(&mut self, gpu: &GpuContext, dt: f64) {
        if self.decoder.is_none() {
            return;
        }
        if !self.transport.playing {
            // Even when paused on a video boundary we drain decoded
            // audio that was buffered, so the sample queue doesn't
            // grow indefinitely.
            self.flush_audio_to_ring();
            return;
        }
        let period = self.frame_period / self.transport.speed.max(1e-3);
        self.catchup += dt;
        loop {
            // Always drain audio first; the video pump below may also
            // demux fresh audio packets, which we'll drain on the next
            // iteration.
            self.flush_audio_to_ring();

            if self.catchup < period {
                break;
            }
            self.catchup -= period;
            let decoder = self.decoder.as_mut().expect("checked above");
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
        // One last drain so any audio demuxed alongside the most
        // recent video frame goes into the ring buffer this tick.
        self.flush_audio_to_ring();
    }

    /// Move whatever audio the decoder has buffered into the ring
    /// buffer. We aim to keep the ring buffer at least
    /// `AUDIO_PUMP_TARGET_FILL` of its capacity. Anything beyond what
    /// the buffer can hold stays inside the decoder's internal queue
    /// and is drained on the next tick.
    fn flush_audio_to_ring(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        if decoder.audio_config().is_none() {
            return;
        }
        let _target = (audio.producer.capacity().get() as f32 * AUDIO_PUMP_TARGET_FILL) as usize;
        let free = audio.free_samples();
        if free == 0 {
            return;
        }
        if self.audio_scratch.len() < free {
            self.audio_scratch.resize(free, 0.0);
        }
        let scratch = &mut self.audio_scratch[..free];
        let n = decoder.take_audio_into(scratch);
        if n > 0 {
            audio.push(&scratch[..n]);
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
        // Effective opacity folds the layer master in — drag the master
        // to 0 and the layer fades out regardless of its own opacity
        // setting; the audio side applies the same multiplier in the
        // cpal mix loop.
        let effective_opacity = self.opacity * self.master;
        gpu.queue.write_buffer(
            &self.uniforms_buffer,
            0,
            bytemuck::bytes_of(&Uniforms::new(scale, effective_opacity)),
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
