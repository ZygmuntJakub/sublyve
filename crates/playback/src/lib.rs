//! CPU-side video + audio playback: FFmpeg-backed decoding and a
//! transport clock.

pub mod cameras;
pub mod decoder;
pub mod hotplug;
pub mod transport;

pub use avengine_core::AvError;
pub use cameras::CameraDevice;
pub use decoder::{AudioConfig, Decoder, StreamInfo};
pub use hotplug::Watcher as CameraHotplugWatcher;
pub use transport::Transport;
