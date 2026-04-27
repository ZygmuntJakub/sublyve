use avengine_core::VideoFrame;

/// A small RGBA texture used for grid-cell previews and the cue pane.
///
/// The compositor crate stays egui-free, so this type holds only the wgpu
/// resources. The app crate is responsible for registering the `view()`
/// with `egui_wgpu::Renderer::register_native_texture` and keeping the
/// returned `egui::TextureId` next to it.
pub struct Thumbnail {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
}

impl Thumbnail {
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    /// Allocate and upload a frame as a thumbnail. The frame is expected
    /// to already be at the desired thumbnail dimensions (see
    /// `app/thumbs.rs::extract_thumbnail` which uses FFmpeg's scaler to
    /// downscale on import).
    pub fn from_frame(device: &wgpu::Device, queue: &wgpu::Queue, frame: &VideoFrame) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("avengine.thumbnail"),
            size: wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
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

        Self { texture, view, size: (frame.width, frame.height) }
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn aspect_ratio(&self) -> f32 {
        if self.size.1 == 0 {
            1.0
        } else {
            self.size.0 as f32 / self.size.1 as f32
        }
    }
}

// Keep the texture alive even though we don't read it back; the bind group
// that registers `view()` with egui needs the underlying texture to outlive
// the registration.
impl Drop for Thumbnail {
    fn drop(&mut self) {
        // No-op; here only to make the lifetime intent explicit.
        let _ = &self.texture;
    }
}
