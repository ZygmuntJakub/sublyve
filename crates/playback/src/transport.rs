/// Playhead state for a single video stream.
///
/// `position` is in seconds from the start of the stream and is meant to be
/// updated from the PTS of the most recently decoded frame, not from
/// wall-clock time. Wall-clock advancement is the responsibility of the
/// host (it determines when to pull the next frame).
#[derive(Debug, Clone)]
pub struct Transport {
    pub playing: bool,
    pub looping: bool,
    pub position: f64,
    pub speed: f64,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            playing: false,
            looping: true,
            position: 0.0,
            speed: 1.0,
        }
    }

    pub fn toggle_play(&mut self) {
        self.playing = !self.playing;
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_transport_is_paused_at_origin() {
        let t = Transport::new();
        assert!(!t.playing);
        assert!(t.looping);
        assert_eq!(t.position, 0.0);
        assert_eq!(t.speed, 1.0);
    }

    #[test]
    fn toggle_play_flips() {
        let mut t = Transport::new();
        t.toggle_play();
        assert!(t.playing);
        t.toggle_play();
        assert!(!t.playing);
    }
}
