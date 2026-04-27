use std::path::Path;

use avengine_core::{AvError, VideoFrame};
use ffmpeg_next as ffmpeg;
use tracing::debug;

/// Static metadata about the video stream that the decoder is bound to.
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    /// Total stream duration in seconds. Zero if unknown.
    pub duration: f64,
    /// Average frame rate in Hz. Falls back to 30.0 if the container
    /// reports neither `avg_frame_rate` nor `r_frame_rate`.
    pub frame_rate: f64,
}

/// A pull-based video decoder over a single video stream.
///
/// `next_frame` follows the standard FFmpeg send-packet / receive-frame
/// loop and correctly drains the decoder at end-of-stream, so frames
/// buffered for B-frame reordering are not silently dropped.
pub struct Decoder {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    info: StreamInfo,
    /// True once `send_eof` has been called. Cleared by `seek`.
    drained: bool,
}

impl Decoder {
    /// Open a decoder that emits frames at native source resolution.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, AvError> {
        Self::open_scaled(path, None)
    }

    /// Open a decoder that resamples each output frame to `target_size` via
    /// FFmpeg's software scaler. Used by the thumbnail extractor to get a
    /// 320×180 (or whatever) frame in one pass without a separate CPU
    /// resize step.
    pub fn open_scaled<P: AsRef<Path>>(
        path: P,
        target_size: Option<(u32, u32)>,
    ) -> Result<Self, AvError> {
        ffmpeg::init().map_err(AvError::ffmpeg)?;

        let input = ffmpeg::format::input(&path).map_err(AvError::ffmpeg)?;

        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| AvError::decode("no video stream found"))?;
        let stream_index = stream.index();
        let time_base = stream.time_base();

        let codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(AvError::ffmpeg)?;
        let decoder = codec_ctx.decoder().video().map_err(AvError::ffmpeg)?;

        let (target_w, target_h) = target_size
            .map(|(w, h)| (w.max(1), h.max(1)))
            .unwrap_or((decoder.width(), decoder.height()));

        let scaler = ffmpeg::software::scaling::context::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            ffmpeg::format::Pixel::RGBA,
            target_w,
            target_h,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .map_err(AvError::ffmpeg)?;

        let frame_rate = pick_frame_rate(&stream);
        let duration = if stream.duration() == ffmpeg::ffi::AV_NOPTS_VALUE {
            0.0
        } else {
            stream.duration() as f64 * rational_as_f64(time_base)
        };

        let info = StreamInfo {
            width: decoder.width(),
            height: decoder.height(),
            duration,
            frame_rate,
        };

        debug!(?info, "opened video stream");

        Ok(Self {
            input,
            decoder,
            scaler,
            stream_index,
            time_base,
            info,
            drained: false,
        })
    }

    pub fn info(&self) -> StreamInfo {
        self.info
    }

    /// Pulls the next decoded frame, or `Ok(None)` if the stream is exhausted.
    ///
    /// Implements the canonical FFmpeg pattern:
    ///   1. Try `receive_frame`. If a frame is ready, return it.
    ///   2. On `EAGAIN`, push more packets and retry.
    ///   3. When `input.packets()` is exhausted, send EOF and continue
    ///      receiving until the decoder returns `EOF`.
    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, AvError> {
        loop {
            let mut decoded = ffmpeg::frame::Video::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => return self.convert(decoded).map(Some),
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    // Need more input — fall through to packet feed.
                }
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(e) => return Err(AvError::ffmpeg(e)),
            }

            if self.drained {
                // EOF already signalled; the only valid responses above are
                // a frame or `Eof`. Anything else means the decoder is stuck.
                return Ok(None);
            }

            match self.input.packets().next() {
                Some((stream, packet)) => {
                    if stream.index() == self.stream_index {
                        self.decoder.send_packet(&packet).map_err(AvError::ffmpeg)?;
                    }
                }
                None => {
                    self.decoder.send_eof().map_err(AvError::ffmpeg)?;
                    self.drained = true;
                }
            }
        }
    }

    /// Seek to (approximately) `timestamp_secs` and flush the decoder.
    /// The next decoded frame will have a PTS at or after the requested time.
    pub fn seek(&mut self, timestamp_secs: f64) -> Result<(), AvError> {
        let ts = (timestamp_secs / rational_as_f64(self.time_base)) as i64;
        self.input.seek(ts, ..ts).map_err(AvError::ffmpeg)?;
        self.decoder.flush();
        self.drained = false;
        Ok(())
    }

    fn convert(&mut self, decoded: ffmpeg::frame::Video) -> Result<VideoFrame, AvError> {
        let mut rgba = ffmpeg::frame::Video::empty();
        self.scaler.run(&decoded, &mut rgba).map_err(AvError::ffmpeg)?;

        let width = rgba.width();
        let height = rgba.height();
        let row_len = (width as usize) * 4;
        let stride = rgba.stride(0);
        let src = rgba.data(0);

        // Repack to a tightly-packed buffer so the GPU upload doesn't need
        // a per-row stride.
        let mut data = Vec::with_capacity(row_len * height as usize);
        if stride == row_len {
            data.extend_from_slice(&src[..row_len * height as usize]);
        } else {
            for row in 0..height as usize {
                let offset = row * stride;
                data.extend_from_slice(&src[offset..offset + row_len]);
            }
        }

        let pts = decoded
            .pts()
            .map_or(0.0, |p| p as f64 * rational_as_f64(self.time_base));

        Ok(VideoFrame::new(width, height, pts, data))
    }
}

fn pick_frame_rate(stream: &ffmpeg::format::stream::Stream) -> f64 {
    let candidates = [stream.avg_frame_rate(), stream.rate()];
    for r in candidates {
        let f = rational_as_f64(r);
        if f.is_finite() && f > 0.0 {
            return f;
        }
    }
    30.0
}

fn rational_as_f64(r: ffmpeg::Rational) -> f64 {
    let den = r.denominator();
    if den == 0 {
        0.0
    } else {
        r.numerator() as f64 / den as f64
    }
}
