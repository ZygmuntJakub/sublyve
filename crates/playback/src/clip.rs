use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Clip {
    pub path: PathBuf,
    pub start: f64,
    pub end: f64,
    pub speed: f64,
}

impl Clip {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            start: 0.0,
            end: f64::MAX,
            speed: 1.0,
        }
    }

    pub fn in_point(&self) -> f64 {
        self.start
    }

    pub fn out_point(&self, duration: f64) -> f64 {
        self.end.min(duration)
    }
}
