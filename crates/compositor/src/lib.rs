//! GPU compositing: shared GPU context, video pipeline, per-window surfaces.
//!
//! The crate splits the rendering stack along the lines a multi-window VJ
//! app needs:
//!
//! - [`GpuContext`] — instance/adapter/device/queue, created once and
//!   shared across windows.
//! - [`VideoPipeline`] — the fullscreen-quad render pipeline; one per
//!   surface texture format.
//! - [`VideoTexture`] — the active decoded frame on the GPU. Uploaded once
//!   per frame; sampled by every window.
//! - [`WindowSurface`] — per-window swapchain + letterbox uniform + bind
//!   group. Rebuilds its bind group automatically when the underlying
//!   `VideoTexture` is reallocated.

pub mod gpu;
pub mod pipeline;
pub mod quad;
pub mod surface;
pub mod video_texture;

pub use gpu::GpuContext;
pub use pipeline::VideoPipeline;
pub use surface::{AcquiredFrame, WindowSurface};
pub use video_texture::VideoTexture;
