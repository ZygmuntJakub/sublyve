//! GPU compositing: shared GPU context, multi-blend pipelines, per-window
//! surfaces, the offscreen composition target, and per-clip thumbnails.
//!
//! Layout:
//! - [`GpuContext`] — instance/adapter/device/queue, shared across windows.
//! - [`VideoPipelines`] — one render pipeline per [`avengine_core::BlendMode`],
//!   plus the shared sampler / vertex buffer / bind-group layout. Both the
//!   per-layer draws and the surface-blit draw go through these.
//! - [`VideoTexture`] — a single decoded frame on the GPU; one per layer.
//! - [`CompositionTarget`] — the offscreen texture every layer composites
//!   into; both windows sample it.
//! - [`Thumbnail`] — small per-clip preview texture (registered by the app
//!   crate with `egui_wgpu::Renderer::register_native_texture`).
//! - [`WindowSurface`] — per-window swapchain that blits the
//!   `CompositionTarget` to the surface, letterboxed.

pub mod composition;
pub mod gpu;
pub mod pipeline;
pub mod quad;
pub mod surface;
pub mod thumbnail;
pub mod video_texture;

pub use composition::CompositionTarget;
pub use gpu::GpuContext;
pub use pipeline::{Uniforms, VideoPipelines};
pub use surface::{AcquiredFrame, WindowSurface};
pub use thumbnail::Thumbnail;
pub use video_texture::VideoTexture;
