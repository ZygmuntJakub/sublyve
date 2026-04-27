use avengine_core::BlendMode;
use wgpu::util::DeviceExt;

use crate::quad::{QUAD_VERTICES, Vertex};

/// Per-draw uniforms: NDC scale (letterbox) + per-layer opacity.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniforms {
    pub scale: [f32; 2],
    pub opacity: f32,
    pub _pad: f32,
}

impl Uniforms {
    pub const SIZE: u64 = std::mem::size_of::<Self>() as u64;

    pub fn new(scale: [f32; 2], opacity: f32) -> Self {
        Self { scale, opacity, _pad: 0.0 }
    }
}

/// One render pipeline per `BlendMode`. The blend state is baked into the
/// pipeline so switching modes is a `set_pipeline` call, not a runtime
/// uniform tweak. The vertex buffer / sampler / bind group layout are
/// shared across all variants.
pub struct VideoPipelines {
    pub vertex_buffer: wgpu::Buffer,
    pub sampler: wgpu::Sampler,
    pub bind_group_layout: wgpu::BindGroupLayout,

    normal: wgpu::RenderPipeline,
    add: wgpu::RenderPipeline,
    multiply: wgpu::RenderPipeline,
    screen: wgpu::RenderPipeline,
}

impl VideoPipelines {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
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
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(Uniforms::SIZE),
                    },
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("avengine.quad.layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let make = |label: &str, blend: wgpu::BlendState| -> wgpu::RenderPipeline {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
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
                        blend: Some(blend),
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
            })
        };

        let normal = make("avengine.quad.pipeline.normal", blend_normal());
        let add = make("avengine.quad.pipeline.add", blend_add());
        let multiply = make("avengine.quad.pipeline.multiply", blend_multiply());
        let screen = make("avengine.quad.pipeline.screen", blend_screen());

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

        Self {
            vertex_buffer,
            sampler,
            bind_group_layout,
            normal,
            add,
            multiply,
            screen,
        }
    }

    pub fn pipeline_for(&self, mode: BlendMode) -> &wgpu::RenderPipeline {
        match mode {
            BlendMode::Normal => &self.normal,
            BlendMode::Add => &self.add,
            BlendMode::Multiply => &self.multiply,
            BlendMode::Screen => &self.screen,
        }
    }
}

// All formulas below assume the fragment shader emits PREMULTIPLIED alpha
// (`vec4(rgb * a, a)`), so `src.rgb` already incorporates the per-layer
// opacity. That keeps the blend states themselves opacity-agnostic.

// Standard "over": out = src + dst * (1 - src.a).
fn blend_normal() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    }
}

// Linear add: out = src + dst. Opacity 0 → src is zero → leaves dst alone.
fn blend_add() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    }
}

// Multiply: out = src * dst + dst * (1 - src.a). Opacity 0 → src=0, src.a=0
// → out = dst. Opacity 1 (opaque source) → out = src * dst.
fn blend_multiply() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Dst,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    }
}

// Screen: out = src + dst * (1 - src). With premultiplied src,
// at opacity 0 src=0 → out=dst; at opacity 1, opaque src →
// out = src + dst - src * dst.
fn blend_screen() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrc,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap sanity-check that each `BlendMode` variant maps to a distinct
    /// pipeline pointer. We can't compare blend states (`BlendState: !Eq`),
    /// but pointer-distinctness rules out the obvious bug of returning the
    /// same pipeline for every mode.
    ///
    /// This test allocates a wgpu device on the fallback adapter, which is
    /// available everywhere wgpu's `vulkan_portability` / `metal` backends
    /// load. CI without a GPU can skip this test (it'll panic on adapter
    /// selection); for now we run it locally.
    #[test]
    #[ignore = "requires a GPU adapter"]
    fn pipeline_for_each_mode_is_distinct() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("adapter");
        let (device, _queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor::default(),
            None,
        ))
        .expect("device");

        let pipelines = VideoPipelines::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let n = pipelines.pipeline_for(BlendMode::Normal) as *const _;
        let a = pipelines.pipeline_for(BlendMode::Add) as *const _;
        let m = pipelines.pipeline_for(BlendMode::Multiply) as *const _;
        let s = pipelines.pipeline_for(BlendMode::Screen) as *const _;
        assert_ne!(n, a);
        assert_ne!(n, m);
        assert_ne!(n, s);
        assert_ne!(a, m);
        assert_ne!(a, s);
        assert_ne!(m, s);
    }
}
