use std::fmt;

#[derive(Debug)]
pub enum AvError {
    Io(std::io::Error),
    Decode(String),
    Ffmpeg(String),
    Wgpu(String),
    InvalidState(String),
}

impl fmt::Display for AvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AvError::Io(e) => write!(f, "I/O error: {e}"),
            AvError::Decode(s) => write!(f, "decode error: {s}"),
            AvError::Ffmpeg(s) => write!(f, "ffmpeg error: {s}"),
            AvError::Wgpu(s) => write!(f, "wgpu error: {s}"),
            AvError::InvalidState(s) => write!(f, "invalid state: {s}"),
        }
    }
}

impl std::error::Error for AvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AvError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AvError {
    fn from(e: std::io::Error) -> Self {
        AvError::Io(e)
    }
}
