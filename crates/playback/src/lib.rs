//! CPU-side video playback: FFmpeg-backed decoding and a transport clock.

pub mod decoder;
pub mod transport;

pub use decoder::{Decoder, StreamInfo};
pub use transport::Transport;
