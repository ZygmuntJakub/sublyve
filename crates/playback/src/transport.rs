#[derive(Debug, Clone)]
pub struct Transport {
    pub playing: bool,
    pub position: f64,
    pub loop_enabled: bool,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            playing: false,
            position: 0.0,
            loop_enabled: true,
        }
    }

    pub fn toggle_play(&mut self) {
        self.playing = !self.playing;
    }

    pub fn advance(&mut self, dt: f64, speed: f64) {
        if self.playing {
            self.position += dt * speed;
        }
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self::new()
    }
}
