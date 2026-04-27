/// A vertex of the fullscreen quad: NDC position + UV coords.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];

    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Two triangles forming a fullscreen quad in NDC, with UVs flipped on Y
/// to match wgpu's top-left texture origin against FFmpeg's row-major
/// frame layout.
pub const QUAD_VERTICES: [Vertex; 6] = [
    Vertex { position: [-1.0, -1.0], uv: [0.0, 1.0] },
    Vertex { position: [ 1.0, -1.0], uv: [1.0, 1.0] },
    Vertex { position: [ 1.0,  1.0], uv: [1.0, 0.0] },
    Vertex { position: [-1.0, -1.0], uv: [0.0, 1.0] },
    Vertex { position: [ 1.0,  1.0], uv: [1.0, 0.0] },
    Vertex { position: [-1.0,  1.0], uv: [0.0, 0.0] },
];
