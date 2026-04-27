use avengine_core::VideoFrame;

/// The currently-displayed video frame on the GPU.
///
/// Owned by the application and shared (by reference) across both windows.
/// Each `WindowSurface` watches `generation()` to know when to rebuild its
/// bind group, since reallocation invalidates any view bound previously.
pub struct VideoTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
    generation: u64,
}

impl VideoTexture {
    /// Allocate a 1x1 placeholder so windows can build a bind group before
    /// the first real frame has been decoded.
    pub fn placeholder(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("avengine.video.placeholder"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view, size: (1, 1), generation: 0 }
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Upload an RGBA frame. Reallocates the texture (and bumps the
    /// `generation` counter) only when the frame dimensions change.
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &VideoFrame) {
        let new_size = (frame.width, frame.height);
        if self.size != new_size {
            let extent = wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            };
            self.texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("avengine.video.texture"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // sRGB-aware sampling so values land in linear space before
                // shading and re-encode on write into the sRGB surface;
                // matches egui's color pipeline.
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.view = self.texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.size = new_size;
            self.generation = self.generation.wrapping_add(1);
        }

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
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
}
