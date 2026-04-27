#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl VideoFrame {
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    pub fn byte_size(&self) -> usize {
        self.data.len()
    }

    pub fn row_bytes(&self) -> u32 {
        4 * self.width
    }
}

#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    pub data: Vec<f32>,
}
