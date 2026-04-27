/// A decoded RGBA8 video frame in CPU memory, ready for GPU upload.
///
/// The buffer is tightly packed (`row_bytes() = 4 * width`), so it can be
/// passed directly to `wgpu::Queue::write_texture` without per-row copies.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// Presentation timestamp in seconds, relative to the start of the stream.
    pub pts: f64,
    pub data: Vec<u8>,
}

impl VideoFrame {
    pub fn new(width: u32, height: u32, pts: f64, data: Vec<u8>) -> Self {
        debug_assert_eq!(
            data.len(),
            (width as usize) * (height as usize) * 4,
            "VideoFrame data length must equal width*height*4 (RGBA8)",
        );
        Self { width, height, pts, data }
    }

    pub fn row_bytes(&self) -> u32 {
        4 * self.width
    }

    pub fn aspect_ratio(&self) -> f32 {
        if self.height == 0 {
            1.0
        } else {
            self.width as f32 / self.height as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_bytes_matches_width() {
        let frame = VideoFrame::new(4, 2, 0.0, vec![0; 4 * 2 * 4]);
        assert_eq!(frame.row_bytes(), 16);
    }

    #[test]
    fn aspect_ratio_handles_zero_height() {
        let frame = VideoFrame::new(0, 0, 0.0, vec![]);
        assert_eq!(frame.aspect_ratio(), 1.0);
    }

    #[test]
    fn aspect_ratio_16x9() {
        let frame = VideoFrame::new(1920, 1080, 0.0, vec![0; 1920 * 1080 * 4]);
        assert!((frame.aspect_ratio() - 16.0 / 9.0).abs() < 1e-6);
    }
}
