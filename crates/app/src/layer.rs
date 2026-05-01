use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use avengine_compositor::{GpuContext, Uniforms, VideoPipelines, VideoTexture};
use avengine_core::BlendMode;
use avengine_playback::{AudioConfig, Decoder, StreamInfo, Transport};
use wgpu::util::DeviceExt;

use crate::audio::{AudioLayerHandle, AudioLayerProducer};
use crate::decode_worker::{self, DecodedItem, DecoderCmd, FRAME_CHANNEL_CAPACITY};
use crate::library::ClipDefaults;

/// One row of the grid: an independent decoder + transport that draws
/// into the shared `CompositionTarget` with a chosen blend mode and
/// opacity. Multiple `Layer`s play simultaneously and composite back-to-front.
///
/// Decoding runs on a dedicated worker thread (one per loaded layer);
/// the worker pushes decoded frames into a small bounded channel that
/// `tick` drains and uploads to the GPU. See `decode_worker.rs`.
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

    /// True when the active source is a live capture device (camera).
    /// Drives UI gates: hides scrub bar / loop / speed editors.
    pub is_live: bool,

    pub transport: Transport,
    pub info: Option<StreamInfo>,

    frame_period: f64,
    /// Per-layer wall-clock catch-up accumulator. Bounded by the global
    /// `MAX_CATCHUP_SECS` clamp applied in `tick`.
    catchup: f64,

    pub video_texture: VideoTexture,
    /// Generation of `video_texture` the bind group was built for.
    bound_video_gen: u64,
    bind_group: Option<wgpu::BindGroup>,

    uniforms_buffer: wgpu::Buffer,

    /// Producer-side audio plumbing. The `producer` field is `None`
    /// while a worker owns it (between `load` and `clear`); we get it
    /// back via the worker's `JoinHandle` on shutdown.
    audio: Option<AudioLayerHandle>,
    /// Engine audio config — used by `Layer::load` to open the decoder
    /// with audio support matching the cpal stream.
    audio_config: Option<AudioConfig>,

    // ---- Threaded decode plumbing (None when layer is empty) ----
    /// Frames produced by the worker; popped by `tick`.
    frame_rx: Option<Receiver<DecodedItem>>,
    /// Commands to the worker (Seek / SwapAudioProducer / Stop).
    cmd_tx: Option<Sender<DecoderCmd>>,
    /// Worker handle. Joined in `clear` to reclaim the audio producer.
    worker: Option<JoinHandle<Option<AudioLayerProducer>>>,
    /// Generation tag — incremented on every Layer-side seek; the worker
    /// echoes it on each frame, and `tick` drops items whose epoch is
    /// older than this (those are pre-seek leftovers in the channel).
    seek_epoch: u64,
    /// Looping flag shared with the worker — read on EOF to decide
    /// loop vs. stop. Cheaper than a command for a per-toggle flag.
    looping_atomic: Arc<AtomicBool>,
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

    /// Build a layer that already knows its audio config but doesn't
    /// have a producer handle yet. Used when adding a layer at
    /// runtime — the caller pushes this layer onto `composition.layers`
    /// and immediately calls `AudioEngine::switch_device(...)`, which
    /// allocates a fresh ring buffer and calls `replace_audio_handle`.
    pub fn new_pending_audio(gpu: &GpuContext, audio_config: AudioConfig) -> Self {
        Self::build(gpu, None, Some(audio_config))
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
            is_live: false,
            transport: Transport::new(),
            info: None,
            frame_period: 1.0 / 30.0,
            catchup: 0.0,
            video_texture,
            bound_video_gen: u64::MAX,
            bind_group: None,
            uniforms_buffer,
            audio,
            audio_config,
            frame_rx: None,
            cmd_tx: None,
            worker: None,
            seek_epoch: 0,
            looping_atomic: Arc::new(AtomicBool::new(false)),
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
    pub fn audio_control(&self) -> Option<Arc<crate::audio::AudioLayerControl>> {
        self.audio.as_ref().map(|a| a.control.clone())
    }

    /// Swap in a new producer handle (after a stream rebuild) without
    /// touching anything else on the layer. If a worker is running, the
    /// new producer is forwarded to it via a command; otherwise it gets
    /// parked on the layer's `AudioLayerHandle` for the next clip load.
    pub fn replace_audio_handle(&mut self, mut handle: AudioLayerHandle) {
        if self.worker.is_some()
            && let Some(prod) = handle.take_producer()
            && let Some(tx) = self.cmd_tx.as_ref()
        {
            // Worker now owns this new producer; the AudioLayerHandle
            // we keep on the layer holds only `control` while the
            // worker is alive.
            let _ = tx.send(DecoderCmd::SwapAudioProducer(Some(prod)));
        }
        self.audio = Some(handle);
    }

    /// Update both `transport.looping` (UI-visible) and the atomic the
    /// decode worker reads on EOF. Use this rather than mutating
    /// `transport.looping` directly so the worker sees the change.
    pub fn set_looping(&mut self, looping: bool) {
        self.transport.looping = looping;
        self.looping_atomic.store(looping, Ordering::Relaxed);
    }

    pub fn is_empty(&self) -> bool {
        self.worker.is_none()
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
    /// layer's `VideoTexture` so the next composition render isn't black;
    /// the worker thread then spawns to handle frames 2..N.
    pub fn load(
        &mut self,
        gpu: &GpuContext,
        path: &Path,
        col: usize,
        defaults: ClipDefaults,
    ) -> Result<()> {
        // Tear down any previously-running worker for this layer first
        // (reclaims the audio producer onto self.audio).
        self.stop_worker();

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
        self.set_looping(defaults.looping);
        self.transport.speed = defaults.speed;
        self.blend_mode = defaults.blend;
        self.catchup = 0.0;
        self.active_col = Some(col);
        self.is_live = false;
        self.seek_epoch = 0;

        // Decode + upload first frame inline so the cell isn't black for
        // the first few ticks. Worker takes over from frame 2.
        if let Some(frame) = decoder.next_frame()? {
            self.transport.position = frame.pts;
            self.video_texture.upload(&gpu.device, &gpu.queue, &frame);
        }
        self.info = Some(info);

        // Hand the audio producer (if any) to the worker.
        let audio_producer = self.audio.as_mut().and_then(|a| a.take_producer());

        let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedItem>(FRAME_CHANNEL_CAPACITY);
        let (cmd_tx, cmd_rx) = mpsc::channel::<DecoderCmd>();
        let worker = decode_worker::spawn(
            decoder,
            frame_tx,
            cmd_rx,
            self.looping_atomic.clone(),
            audio_producer,
        );

        self.frame_rx = Some(frame_rx);
        self.cmd_tx = Some(cmd_tx);
        self.worker = Some(worker);
        Ok(())
    }

    /// Load a *camera* (live capture device) into this layer. Same
    /// flow as `load`, but goes through `Decoder::open_camera` and
    /// hard-disables looping / non-1.0 speed (live streams aren't
    /// seekable, and `defaults.looping` / `defaults.speed` are
    /// silently ignored). `is_live` is set so the UI can hide its
    /// scrub bar / loop / speed editors for this layer.
    ///
    /// `has_audio` comes from camera enumeration: when false, we don't
    /// even ask the decoder to open audio, which avoids the spurious
    /// "no audio stream" warning on video-only cams. When true and
    /// the decoder still can't bring up audio (e.g. macOS Microphone
    /// permission denied), the warning fires — that's a real signal.
    pub fn load_camera(
        &mut self,
        gpu: &GpuContext,
        format_name: &str,
        device: &str,
        has_audio: bool,
        col: usize,
        defaults: ClipDefaults,
    ) -> Result<()> {
        self.stop_worker();

        let audio_cfg = if has_audio { self.audio_config } else { None };
        let mut decoder = Decoder::open_camera(format_name, device, audio_cfg)?;
        let info = decoder.info();
        self.frame_period = 1.0 / info.frame_rate.max(1e-3);
        self.transport = Transport::new();
        self.transport.playing = true;
        self.set_looping(false);
        self.transport.speed = 1.0;
        self.blend_mode = defaults.blend;
        self.catchup = 0.0;
        self.active_col = Some(col);
        self.is_live = true;
        self.seek_epoch = 0;

        if let Some(frame) = decoder.next_frame()? {
            self.transport.position = frame.pts;
            self.video_texture.upload(&gpu.device, &gpu.queue, &frame);
        }
        self.info = Some(info);

        let audio_producer = self.audio.as_mut().and_then(|a| a.take_producer());
        let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedItem>(FRAME_CHANNEL_CAPACITY);
        let (cmd_tx, cmd_rx) = mpsc::channel::<DecoderCmd>();
        let worker = decode_worker::spawn(
            decoder,
            frame_tx,
            cmd_rx,
            self.looping_atomic.clone(),
            audio_producer,
        );

        self.frame_rx = Some(frame_rx);
        self.cmd_tx = Some(cmd_tx);
        self.worker = Some(worker);
        Ok(())
    }

    /// Stop the worker thread (if any) and reclaim the audio producer.
    fn stop_worker(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(DecoderCmd::Stop);
        }
        // Drop the receiver — if the worker is blocked on `send`, this
        // unblocks it with a disconnect error and it exits cleanly.
        self.frame_rx = None;
        if let Some(handle) = self.worker.take()
            && let Ok(returned) = handle.join()
            && let Some(prod) = returned
            && let Some(audio) = self.audio.as_mut()
        {
            audio.set_producer(prod);
        }
    }

    pub fn clear(&mut self) {
        self.stop_worker();
        self.info = None;
        self.active_col = None;
        self.is_live = false;
        self.transport.playing = false;
        self.catchup = 0.0;
        self.seek_epoch = 0;
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
        if self.worker.is_none() {
            return;
        }
        self.seek_internal(0.0);
        self.transport.playing = true;
    }

    /// Seek the layer to `secs` (in source time) without changing the
    /// playing/looping state. Used by the inspector's scrub bar and by
    /// `restart`.
    ///
    /// Bumps the seek epoch and dispatches a `Seek` command to the
    /// worker; pre-seek frames already in the channel are filtered out
    /// of `tick` via the epoch tag. For *paused* layers we additionally
    /// block briefly for the worker's first post-seek frame and upload
    /// it inline — without this, scrubbing a paused layer wouldn't
    /// update the visible texture (since `tick` short-circuits when
    /// `transport.playing` is false). 50 ms is comfortably above a
    /// worker round-trip but unnoticeable as a UI hitch.
    pub fn seek(&mut self, gpu: &GpuContext, secs: f64) {
        if self.is_live {
            // Live capture devices aren't seekable; ignore. UI gates
            // this too (no scrub bar shown), but keyboard shortcut R
            // still calls restart() → seek(0); we silently no-op.
            return;
        }
        self.seek_internal(secs.max(0.0));
        if !self.transport.playing {
            self.upload_post_seek_inline(gpu);
        }
    }

    fn upload_post_seek_inline(&mut self, gpu: &GpuContext) {
        let Some(rx) = self.frame_rx.as_ref() else {
            return;
        };
        // Loop a few times so a stray pre-seek frame doesn't make us
        // upload the wrong content.
        for _ in 0..FRAME_CHANNEL_CAPACITY + 1 {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(item) => {
                    if item.epoch < self.seek_epoch {
                        continue;
                    }
                    if let Some(frame) = item.frame {
                        self.transport.position = frame.pts;
                        self.video_texture.upload(&gpu.device, &gpu.queue, &frame);
                    }
                    return;
                }
                Err(_) => return,
            }
        }
    }

    fn seek_internal(&mut self, secs: f64) {
        let Some(tx) = self.cmd_tx.as_ref() else {
            return;
        };
        self.seek_epoch = self.seek_epoch.wrapping_add(1);
        if tx
            .send(DecoderCmd::Seek(secs, self.seek_epoch))
            .is_err()
        {
            return;
        }
        self.transport.position = secs;
        self.catchup = 0.0;
        // Drain any in-flight pre-seek frames so they don't hang around
        // in the channel into the next tick. Worker may push more
        // pre-seek frames before observing the Seek command — those get
        // filtered by the epoch tag on the next `tick`.
        if let Some(rx) = self.frame_rx.as_ref() {
            while rx.try_recv().is_ok() {}
        }
    }

    /// Pump the decoder for `dt` seconds of wall-clock time. Pops as
    /// many frames as needed from the worker's channel, uploads the
    /// freshest one to the layer's `VideoTexture`, and drops the
    /// catch-up to zero on loop / EOF.
    ///
    /// Audio drain is owned by the worker now (it has the producer);
    /// `tick` no longer touches the audio path.
    pub fn tick(&mut self, gpu: &GpuContext, dt: f64) {
        if self.worker.is_none() {
            return;
        }
        if !self.transport.playing {
            // Backpressure handles the pause: with main not draining,
            // the bounded channel fills, the worker blocks on `send`,
            // and no more decode work runs.
            return;
        }
        let period = self.frame_period / self.transport.speed.max(1e-3);
        self.catchup += dt;

        let Some(rx) = self.frame_rx.as_ref() else {
            return;
        };

        loop {
            if self.catchup < period {
                break;
            }
            match rx.try_recv() {
                Ok(item) => {
                    if item.epoch < self.seek_epoch {
                        // Pre-seek leftover; skip without consuming budget.
                        continue;
                    }
                    match item.frame {
                        Some(frame) => {
                            self.transport.position = frame.pts;
                            self.video_texture.upload(&gpu.device, &gpu.queue, &frame);
                            self.catchup -= period;
                        }
                        None => {
                            // EOF marker. Same shape as the old `Ok(None)`.
                            if self.transport.looping {
                                // Looping is handled by the worker (it
                                // reads the atomic on EOF and seeks(0)
                                // itself). If we still got an EOF marker
                                // here, looping must have flipped off
                                // between worker's check and main's read.
                                self.transport.position = 0.0;
                                // Wake the worker by issuing a Seek(0).
                                self.seek_internal(0.0);
                            } else {
                                self.transport.playing = false;
                            }
                            self.catchup = 0.0;
                            break;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Worker hasn't produced the next frame yet; we'll
                    // try again next tick. Don't burn the catchup —
                    // dropping a frame here would only make the layer
                    // visibly stutter the next time decode catches up.
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker exited unexpectedly; stop accepting input.
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

impl Drop for Layer {
    fn drop(&mut self) {
        // Make sure the worker exits even if the Layer was just dropped
        // (e.g. composition shrink). Best-effort: the worker may have
        // already exited on its own.
        self.stop_worker();
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
