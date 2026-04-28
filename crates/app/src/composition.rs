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
}

impl Composition {
    /// Build a composition. One `AudioLayerHandle` per layer is consumed
    /// — the producer ends end up inside the `Layer`s, the matching
    /// consumer ends are already wired into the audio engine's cpal
    /// stream by the caller.
    pub fn new(
        gpu: &GpuContext,
        audio_handles: Vec<AudioLayerHandle>,
        audio_config: AudioConfig,
        width: u32,
        height: u32,
    ) -> Self {
        let layers = audio_handles
            .into_iter()
            .map(|h| Layer::new(gpu, h, audio_config))
            .collect();
        let target = CompositionTarget::new(&gpu.device, width, height);
        Self { layers, target }
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
        for layer in &self.layers {
            layer.draw(&mut rpass, pipelines);
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
