use tracing::debug;

/// Offscreen render target every layer composites into.
///
/// Both windows then sample this texture (the output window blits it to
/// its surface; the control window also exposes it to egui as a live
/// preview thumbnail). Decoupling composition resolution from window
/// resolution is the standard VJ workflow — Resolume calls this the
/// "composition size" — and avoids re-encoding/re-uploading the layer
/// stack when the user resizes the control window.
pub struct CompositionTarget {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub size: (u32, u32),
    /// Bumped on every reallocation. Consumers that cache bind groups or
    /// egui texture registrations watch this to know when to refresh.
    pub generation: u64,
}

impl CompositionTarget {
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let (texture, view) = create(device, width.max(1), height.max(1));
        debug!(w = width, h = height, "composition target created");
        Self {
            texture,
            view,
            size: (width.max(1), height.max(1)),
            generation: 0,
        }
    }

    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let new_size = (width.max(1), height.max(1));
        if new_size == self.size {
            return;
        }
        let (texture, view) = create(device, new_size.0, new_size.1);
        self.texture = texture;
        self.view = view;
        self.size = new_size;
        self.generation = self.generation.wrapping_add(1);
        debug!(w = new_size.0, h = new_size.1, "composition target resized");
    }
}

fn create(device: &wgpu::Device, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("avengine.composition.target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: CompositionTarget::FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}
