use std::collections::VecDeque;
use std::path::Path;

use avengine_core::{AvError, VideoFrame};
use ffmpeg_next as ffmpeg;
use tracing::{debug, warn};

/// Static metadata about the source streams the decoder is bound to.
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    /// Total stream duration in seconds. Zero if unknown.
    pub duration: f64,
    /// Average frame rate in Hz. Falls back to 30.0 if the container
    /// reports neither `avg_frame_rate` nor `r_frame_rate`.
    pub frame_rate: f64,
    /// True if the source has an audio stream the decoder is producing
    /// resampled samples for.
    pub has_audio: bool,
}

/// Engine-wide audio target the decoder resamples to.
///
/// The compositor's `AudioEngine` picks one rate / channel layout for the
/// whole session; every layer's decoder resamples to those values so the
/// mixer can sum buffers without worrying about heterogeneous formats.
#[derive(Debug, Clone, Copy)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
}

/// A pull-based video + audio decoder over a single input file.
///
/// `next_video_frame` follows the canonical FFmpeg send-packet /
/// receive-frame loop and routes audio packets to the audio decoder + a
/// `swresample` resampler in the same pass, so demuxing happens once.
/// Decoded audio sits in `pending_audio` for the caller to drain (the
/// app side feeds it into the per-layer SPSC ring buffer that the cpal
/// callback consumes).
pub struct Decoder {
    input: ffmpeg::format::context::Input,

    video_stream: usize,
    video_decoder: ffmpeg::decoder::Video,
    video_scaler: ffmpeg::software::scaling::Context,
    video_time_base: ffmpeg::Rational,
    /// True once `send_eof` has been called on the *video* decoder.
    /// Cleared by `seek`.
    video_drained: bool,

    audio: Option<AudioPipeline>,

    info: StreamInfo,
}

struct AudioPipeline {
    stream: usize,
    decoder: ffmpeg::decoder::Audio,
    resampler: ffmpeg::software::resampling::Context,
    config: AudioConfig,
    /// Resampled, interleaved samples awaiting consumption. One value
    /// per (channel × sample). Bounded only by how aggressively the
    /// caller drains; in steady state it stays small.
    pending: VecDeque<f32>,
    drained: bool,
}

impl Decoder {
    /// Open a video-only decoder at native source resolution.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, AvError> {
        Self::open_with_options(path, None, None)
    }

    /// Open a decoder that resamples each output frame to `target_size` via
    /// FFmpeg's software scaler. Used by the thumbnail extractor to get a
    /// 320×180 frame in one pass without a separate CPU resize step.
    pub fn open_scaled<P: AsRef<Path>>(
        path: P,
        target_size: Option<(u32, u32)>,
    ) -> Result<Self, AvError> {
        Self::open_with_options(path, target_size, None)
    }

    /// Open a decoder that produces both video and audio. Audio is
    /// resampled to `audio` (interleaved f32). If the source has no
    /// audio stream the audio side is silently disabled — the caller
    /// can check `info().has_audio` to know.
    pub fn open_av<P: AsRef<Path>>(path: P, audio: AudioConfig) -> Result<Self, AvError> {
        Self::open_with_options(path, None, Some(audio))
    }

    fn open_with_options<P: AsRef<Path>>(
        path: P,
        target_size: Option<(u32, u32)>,
        audio: Option<AudioConfig>,
    ) -> Result<Self, AvError> {
        ffmpeg::init().map_err(AvError::ffmpeg)?;

        let input = ffmpeg::format::input(&path).map_err(AvError::ffmpeg)?;

        // Video.
        let v_stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| AvError::decode("no video stream found"))?;
        let video_stream = v_stream.index();
        let video_time_base = v_stream.time_base();

        let v_codec_ctx = ffmpeg::codec::context::Context::from_parameters(v_stream.parameters())
            .map_err(AvError::ffmpeg)?;
        let video_decoder = v_codec_ctx.decoder().video().map_err(AvError::ffmpeg)?;

        let (target_w, target_h) = target_size
            .map(|(w, h)| (w.max(1), h.max(1)))
            .unwrap_or((video_decoder.width(), video_decoder.height()));

        let video_scaler = ffmpeg::software::scaling::context::Context::get(
            video_decoder.format(),
            video_decoder.width(),
            video_decoder.height(),
            ffmpeg::format::Pixel::RGBA,
            target_w,
            target_h,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .map_err(AvError::ffmpeg)?;

        let frame_rate = pick_frame_rate(&v_stream);
        let duration = if v_stream.duration() == ffmpeg::ffi::AV_NOPTS_VALUE {
            0.0
        } else {
            v_stream.duration() as f64 * rational_as_f64(video_time_base)
        };

        // Audio (best-effort — missing audio stream is not an error).
        let audio_pipe = audio.and_then(|cfg| match try_open_audio(&input, cfg) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!("audio stream not available: {e}");
                None
            }
        });

        let info = StreamInfo {
            width: video_decoder.width(),
            height: video_decoder.height(),
            duration,
            frame_rate,
            has_audio: audio_pipe.is_some(),
        };
        debug!(?info, "opened decoder");

        Ok(Self {
            input,
            video_stream,
            video_decoder,
            video_scaler,
            video_time_base,
            video_drained: false,
            audio: audio_pipe,
            info,
        })
    }

    pub fn info(&self) -> StreamInfo {
        self.info
    }

    /// Pulls the next decoded video frame, or `Ok(None)` if the stream
    /// is exhausted. Audio packets demuxed along the way are decoded
    /// and pushed into the internal `pending_audio` buffer; drain it
    /// with `take_audio_into`.
    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, AvError> {
        loop {
            let mut decoded = ffmpeg::frame::Video::empty();
            match self.video_decoder.receive_frame(&mut decoded) {
                Ok(()) => return self.convert_video(decoded).map(Some),
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    // Need more input — fall through to packet feed.
                }
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(e) => return Err(AvError::ffmpeg(e)),
            }

            if self.video_drained {
                return Ok(None);
            }

            match self.input.packets().next() {
                Some((stream, packet)) => {
                    let idx = stream.index();
                    if idx == self.video_stream {
                        self.video_decoder.send_packet(&packet).map_err(AvError::ffmpeg)?;
                    } else if let Some(audio) = self.audio.as_mut()
                        && idx == audio.stream
                    {
                        audio.send_packet_and_drain(&packet)?;
                    }
                }
                None => {
                    self.video_decoder.send_eof().map_err(AvError::ffmpeg)?;
                    self.video_drained = true;
                    if let Some(audio) = self.audio.as_mut()
                        && !audio.drained
                    {
                        audio.decoder.send_eof().map_err(AvError::ffmpeg)?;
                        audio.drained = true;
                        audio.drain_decoded()?;
                    }
                }
            }
        }
    }

    /// Drain up to `dst.len()` pending audio samples (interleaved
    /// channels) into `dst`. Returns the number of samples written.
    /// Returns 0 if there's no audio stream or if the buffer is empty.
    pub fn take_audio_into(&mut self, dst: &mut [f32]) -> usize {
        let Some(audio) = self.audio.as_mut() else {
            return 0;
        };
        let mut written = 0;
        while written < dst.len() {
            let Some(s) = audio.pending.pop_front() else {
                break;
            };
            dst[written] = s;
            written += 1;
        }
        written
    }

    /// Number of pending audio samples (interleaved channels) currently
    /// buffered.
    pub fn pending_audio_samples(&self) -> usize {
        self.audio.as_ref().map_or(0, |a| a.pending.len())
    }

    /// Audio config the decoder was opened with, if any.
    pub fn audio_config(&self) -> Option<AudioConfig> {
        self.audio.as_ref().map(|a| a.config)
    }

    /// Seek to (approximately) `timestamp_secs` and flush both decoders.
    /// The next decoded video frame will have a PTS at or after the
    /// requested time; pending audio is dropped.
    ///
    /// `Input::seek` (which wraps `avformat_seek_file` with
    /// `stream_index = -1`) expects the target timestamp in
    /// `AV_TIME_BASE` units — microseconds, *not* the per-stream
    /// `time_base`. The previous version of this method used the
    /// stream time_base, which sent FFmpeg the wrong target on every
    /// call; only restarts (`seek(0.0)`) happened to behave correctly
    /// because zero is zero in any unit.
    pub fn seek(&mut self, timestamp_secs: f64) -> Result<(), AvError> {
        let ts = (timestamp_secs.max(0.0) * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
        self.input.seek(ts, ..ts).map_err(AvError::ffmpeg)?;
        self.video_decoder.flush();
        self.video_drained = false;
        if let Some(audio) = self.audio.as_mut() {
            audio.decoder.flush();
            audio.pending.clear();
            audio.drained = false;
        }
        Ok(())
    }

    fn convert_video(&mut self, decoded: ffmpeg::frame::Video) -> Result<VideoFrame, AvError> {
        let mut rgba = ffmpeg::frame::Video::empty();
        self.video_scaler.run(&decoded, &mut rgba).map_err(AvError::ffmpeg)?;

        let width = rgba.width();
        let height = rgba.height();
        let row_len = (width as usize) * 4;
        let stride = rgba.stride(0);
        let src = rgba.data(0);

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
            .map_or(0.0, |p| p as f64 * rational_as_f64(self.video_time_base));

        Ok(VideoFrame::new(width, height, pts, data))
    }
}

impl AudioPipeline {
    fn send_packet_and_drain(&mut self, packet: &ffmpeg::Packet) -> Result<(), AvError> {
        if self.drained {
            return Ok(());
        }
        self.decoder.send_packet(packet).map_err(AvError::ffmpeg)?;
        self.drain_decoded()
    }

    fn drain_decoded(&mut self) -> Result<(), AvError> {
        loop {
            let mut decoded = ffmpeg::frame::Audio::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => self.append_resampled(&decoded)?,
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    return Ok(());
                }
                Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(e) => return Err(AvError::ffmpeg(e)),
            }
        }
    }

    fn append_resampled(&mut self, decoded: &ffmpeg::frame::Audio) -> Result<(), AvError> {
        let mut resampled = ffmpeg::frame::Audio::empty();
        self.resampler.run(decoded, &mut resampled).map_err(AvError::ffmpeg)?;

        let n_samples = resampled.samples();
        let n_channels = self.config.channels as usize;
        if n_samples == 0 || n_channels == 0 {
            return Ok(());
        }

        // We resample to a packed (interleaved) f32 layout, so plane 0
        // holds `samples * channels` floats laid out as
        // `[L0, R0, L1, R1, …]`.
        let bytes = resampled.data(0);
        let needed_bytes = n_samples * n_channels * std::mem::size_of::<f32>();
        let slice = &bytes[..needed_bytes.min(bytes.len())];
        // FFmpeg always allocates audio planes with at least 64-byte
        // alignment via av_malloc, so the &[u8] slice is f32-aligned.
        // `try_cast_slice` will return Err only on length-mismatch
        // (slice not a multiple of 4); in practice that never happens
        // for resampled f32 packed audio.
        let floats: &[f32] = bytemuck::try_cast_slice(slice).map_err(|e| {
            AvError::decode(format!("resampled audio buffer not f32-aligned: {e}"))
        })?;
        self.pending.extend(floats.iter().copied());
        Ok(())
    }
}

fn try_open_audio(
    input: &ffmpeg::format::context::Input,
    cfg: AudioConfig,
) -> Result<AudioPipeline, AvError> {
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .ok_or_else(|| AvError::decode("no audio stream"))?;
    let stream_index = stream.index();

    let codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(AvError::ffmpeg)?;
    let decoder = codec_ctx.decoder().audio().map_err(AvError::ffmpeg)?;

    let dst_layout = match cfg.channels {
        1 => ffmpeg::ChannelLayout::MONO,
        2 => ffmpeg::ChannelLayout::STEREO,
        _ => return Err(AvError::decode("unsupported audio channel count")),
    };

    let resampler = ffmpeg::software::resampling::Context::get(
        decoder.format(),
        decoder.channel_layout(),
        decoder.rate(),
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
        dst_layout,
        cfg.sample_rate,
    )
    .map_err(AvError::ffmpeg)?;

    Ok(AudioPipeline {
        stream: stream_index,
        decoder,
        resampler,
        config: cfg,
        pending: VecDeque::with_capacity((cfg.sample_rate as usize) * 2 * cfg.channels as usize),
        drained: false,
    })
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

