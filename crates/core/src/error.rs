use std::io;

#[derive(Debug, thiserror::Error)]
pub enum AvError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),

    #[error("gpu error: {0}")]
    Gpu(String),

    #[error("invalid state: {0}")]
    InvalidState(String),
}

impl AvError {
    /// Wraps any displayable error into `AvError::Ffmpeg`. Useful for
    /// `.map_err(AvError::ffmpeg)` against `ffmpeg_next::Error` without
    /// having to depend on FFmpeg from this crate.
    pub fn ffmpeg<E: std::fmt::Display>(err: E) -> Self {
        Self::Ffmpeg(err.to_string())
    }

    pub fn gpu<E: std::fmt::Display>(err: E) -> Self {
        Self::Gpu(err.to_string())
    }

    pub fn decode<S: Into<String>>(msg: S) -> Self {
        Self::Decode(msg.into())
    }

    pub fn invalid_state<S: Into<String>>(msg: S) -> Self {
        Self::InvalidState(msg.into())
    }
}
