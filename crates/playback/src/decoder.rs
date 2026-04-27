use std::path::Path;

use avengine_core::{AvError, VideoFrame};
use ffmpeg_next as ffmpeg;

pub struct Decoder {
    input: ffmpeg::format::context::Input,
    video_stream_index: usize,
    decoder: ffmpeg::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    duration_secs: f64,
}

impl Decoder {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, AvError> {
        ffmpeg::init().map_err(|e| AvError::Ffmpeg(e.to_string()))?;

        let input = ffmpeg::format::input(&path)
            .map_err(|e| AvError::Ffmpeg(e.to_string()))?;

        let video_stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| AvError::Decode("no video stream found".into()))?;
        let stream_index = video_stream.index();

        let duration_secs = {
            let tb = video_stream.time_base();
            let dur = video_stream.duration();
            dur as f64 * tb.numerator() as f64 / tb.denominator() as f64
        };

        let context = ffmpeg::codec::context::Context::from_parameters(
            video_stream.parameters(),
        )
        .map_err(|e| AvError::Ffmpeg(e.to_string()))?;

        let decoder = context
            .decoder()
            .video()
            .map_err(|e| AvError::Ffmpeg(e.to_string()))?;

        let scaler = ffmpeg::software::scaling::context::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            ffmpeg::format::Pixel::RGBA,
            decoder.width(),
            decoder.height(),
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| AvError::Ffmpeg(e.to_string()))?;

        Ok(Self {
            input,
            video_stream_index: stream_index,
            decoder,
            scaler,
            duration_secs,
        })
    }

    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, AvError> {
        let stream_index = self.video_stream_index;

        while let Some((stream, packet)) = self.input.packets().next() {
            if stream.index() != stream_index {
                continue;
            }

            self.decoder
                .send_packet(&packet)
                .map_err(|e| AvError::Ffmpeg(e.to_string()))?;

            let mut decoded = ffmpeg::frame::Video::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    let mut rgba = ffmpeg::frame::Video::empty();
                    self.scaler
                        .run(&decoded, &mut rgba)
                        .map_err(|e| AvError::Ffmpeg(e.to_string()))?;

                    let width = rgba.width();
                    let height = rgba.height();
                    let src_stride = rgba.stride(0);
                    let src_data = rgba.data(0);

                    let mut data = Vec::with_capacity((width * height * 4) as usize);
                    for row in 0..height as usize {
                        let offset = row * src_stride;
                        data.extend_from_slice(
                            &src_data[offset..offset + (width as usize * 4)],
                        );
                    }

                    return Ok(Some(VideoFrame::new(width, height, data)));
                }
                Err(err) => {
                    if err == (ffmpeg::Error::Other {
                        errno: ffmpeg::util::error::EAGAIN,
                    }) {
                        continue;
                    }
                    return Err(AvError::Ffmpeg(err.to_string()));
                }
            }
        }

        Ok(None)
    }

    pub fn duration(&self) -> f64 {
        self.duration_secs
    }

    pub fn seek(&mut self, timestamp_secs: f64) -> Result<(), AvError> {
        let stream = self
            .input
            .stream(self.video_stream_index)
            .ok_or_else(|| AvError::InvalidState("video stream missing".into()))?;
        let tb = stream.time_base();
        let pts = (timestamp_secs * tb.denominator() as f64 / tb.numerator() as f64) as i64;

        self.input
            .seek(pts, pts..)
            .map_err(|e| AvError::Ffmpeg(e.to_string()))?;

        self.decoder.flush();

        Ok(())
    }
}
