use std::path::Path;

use anyhow::{Context, Result};
use avengine_core::VideoFrame;
use avengine_playback::{Decoder, StreamInfo, Transport};
use tracing::warn;

/// One playing clip: its decoder, transport state, and stream metadata.
///
/// A `Deck` represents the single active clip currently driving the output
/// surface. Switching clips drops the old decoder (closing its FFmpeg
/// context) and opens a new one — fast enough for responsive cueing
/// without keeping every library decoder warm in memory.
pub struct Deck {
    pub decoder: Decoder,
    pub transport: Transport,
    pub info: StreamInfo,
    pub frame_period: f64,
}

impl Deck {
    pub fn open(path: &Path) -> Result<Self> {
        let decoder = Decoder::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let info = decoder.info();
        let frame_period = 1.0 / info.frame_rate.max(1e-3);
        let transport = Transport::new();
        Ok(Self { decoder, transport, info, frame_period })
    }

    /// Wall-clock period between frames at the deck's current playback speed.
    pub fn period_at_speed(&self) -> f64 {
        self.frame_period / self.transport.speed.max(1e-3)
    }

    pub fn restart(&mut self) {
        if let Err(e) = self.decoder.seek(0.0) {
            warn!("seek failed: {e}");
        }
        self.transport.position = 0.0;
        self.transport.playing = true;
    }

    /// Pull the next frame, applying loop / pause-at-end behaviour. Returns:
    /// - `Ok(Some(frame))` for a new frame to upload
    /// - `Ok(None)` when paused, looped (with no frame this tick), or
    ///   reached the end and not looping
    /// - `Err(_)` for a hard decoder failure (caller should pause the deck)
    pub fn pull_next(&mut self) -> Result<Option<VideoFrame>> {
        match self.decoder.next_frame() {
            Ok(Some(frame)) => {
                self.transport.position = frame.pts;
                Ok(Some(frame))
            }
            Ok(None) => {
                if self.transport.looping {
                    self.decoder.seek(0.0).context("seek-to-loop")?;
                    self.transport.position = 0.0;
                } else {
                    self.transport.playing = false;
                }
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}
