//! Foundational types shared across the avengine crates.
//!
//! `core` deliberately has no dependency on FFmpeg or wgpu — it is the
//! vocabulary that the playback (CPU-side decode) and compositor (GPU-side
//! upload/render) crates communicate in.

pub mod blend;
pub mod error;
pub mod frame;

pub use blend::BlendMode;
pub use error::AvError;
pub use frame::VideoFrame;
