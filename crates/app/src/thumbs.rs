use std::path::Path;

use anyhow::{Context, Result};
use avengine_core::VideoFrame;
use avengine_playback::Decoder;

/// Default thumbnail size (matches Resolume's small clip-cell preview).
pub const DEFAULT_W: u32 = 320;
pub const DEFAULT_H: u32 = 180;

/// Decode the first video frame of `path`, downscaled to `(width, height)`
/// by FFmpeg's software scaler. Returns the packed RGBA frame ready for
/// upload to a small wgpu texture (see `compositor::Thumbnail::from_frame`).
///
/// Synchronous and one-shot — opens a Decoder, pulls one frame, drops it.
/// Cost is dominated by container demux + first I-frame decode (~30–80 ms
/// for 1080p H.264 on Apple silicon).
pub fn extract_thumbnail(path: &Path, width: u32, height: u32) -> Result<VideoFrame> {
    let mut decoder = Decoder::open_scaled(path, Some((width, height)))
        .with_context(|| format!("opening {} for thumbnail", path.display()))?;
    let frame = decoder
        .next_frame()
        .with_context(|| format!("decoding first frame of {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("{} has no decodable frames", path.display()))?;
    Ok(frame)
}
