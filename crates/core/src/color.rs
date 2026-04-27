#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const WHITE: Self = Self(255, 255, 255, 255);
    pub const BLACK: Self = Self(0, 0, 0, 255);
    pub const TRANSPARENT: Self = Self(0, 0, 0, 0);
}

impl Default for Rgba {
    fn default() -> Self {
        Self::WHITE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    Add,
    Multiply,
    Screen,
    Subtract,
    Overlay,
}
