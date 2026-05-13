use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use avengine_compositor::{CompositionTarget, GpuContext, VideoPipelines};
use avengine_playback::AudioConfig;

use crate::audio::AudioLayerHandle;
use crate::layer::Layer;

/// Owns the offscreen render target and the stack of layers that draw
/// into it. `tick` advances every layer by one wall-clock interval; `render`
/// composites them all (in low-to-high index order) into the target.
pub struct Composition {
    pub layers: Vec<Layer>,
    pub target: CompositionTarget,
    /// Shared aggregate "is at least one layer soloed" flag. Owned by
    /// `AudioEngine` (so it survives audio-device switches); cloned in
    /// here so the render path reads from the same atomic the cpal
    /// callback reads. Updated via `recompute_solo`.
    pub any_solo_active: Arc<AtomicBool>,
}

impl Composition {
    /// Build a composition. One `AudioLayerHandle` per layer is consumed
    /// — the producer ends end up inside the `Layer`s, the matching
    /// consumer ends are already wired into the audio engine's cpal
    /// stream by the caller. `any_solo_active` is sourced from
    /// `AudioEngine::any_solo_active()` and is the same Arc the audio
    /// thread reads in its mix loop.
    pub fn new(
        gpu: &GpuContext,
        audio_handles: Vec<AudioLayerHandle>,
        audio_config: AudioConfig,
        width: u32,
        height: u32,
        any_solo_active: Arc<AtomicBool>,
    ) -> Self {
        let layers = audio_handles
            .into_iter()
            .map(|h| Layer::new(gpu, h, audio_config))
            .collect();
        let target = CompositionTarget::new(&gpu.device, width, height);
        Self {
            layers,
            target,
            any_solo_active,
        }
    }

    /// Recompute the shared `any_solo_active` atomic from the current
    /// per-layer `solo` flags. Must be called after any mutation that
    /// adds/removes layers or flips a `solo` flag.
    ///
    /// **Empty layers don't count.** A soloed-but-empty layer would
    /// otherwise silence + hide every other layer in the composition
    /// (the soloed slot has nothing to show), which is a footgun. Solo
    /// on an empty layer is inert until a clip is triggered onto it.
    pub fn recompute_solo(&self) {
        let any = self.layers.iter().any(|l| l.solo && !l.is_empty());
        self.any_solo_active.store(any, Ordering::Relaxed);
    }

    /// Flip a layer's solo flag and refresh the aggregate. The action
    /// handler in `main.rs` calls this rather than poking the layer
    /// directly so the aggregate never drifts.
    pub fn set_layer_solo(&mut self, idx: usize, solo: bool) {
        if let Some(l) = self.layers.get_mut(idx) {
            l.set_solo(solo);
        }
        self.recompute_solo();
    }

    pub fn tick(&mut self, gpu: &GpuContext, dt: f64) {
        for layer in &mut self.layers {
            layer.tick(gpu, dt);
        }
    }

    pub fn render(
        &mut self,
        gpu: &GpuContext,
        pipelines: &VideoPipelines,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let composition_size = self.target.size;
        for layer in &mut self.layers {
            layer.prepare_draw(gpu, pipelines, composition_size);
        }

        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("avengine.composition.pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.target.view,
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
        let any_solo = self.any_solo_active.load(Ordering::Relaxed);
        for layer in &self.layers {
            layer.draw(&mut rpass, pipelines, any_solo);
        }
    }

    pub fn any_playing(&self) -> bool {
        self.layers.iter().any(|l| l.transport.playing && !l.is_empty())
    }

    pub fn set_all_playing(&mut self, playing: bool) {
        for layer in &mut self.layers {
            if !layer.is_empty() {
                layer.transport.playing = playing;
            }
        }
    }

    pub fn restart_all(&mut self) {
        for layer in &mut self.layers {
            layer.restart();
        }
    }
}
