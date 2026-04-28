//! CPU-side video + audio playback: FFmpeg-backed decoding and a
//! transport clock.

pub mod decoder;
pub mod transport;

pub use decoder::{AudioConfig, Decoder, StreamInfo};
pub use transport::Transport;
