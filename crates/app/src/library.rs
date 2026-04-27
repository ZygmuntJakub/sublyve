use std::path::PathBuf;

/// One entry in the clip library: a path on disk and its display name.
///
/// Decoder state is *not* held here — only the active clip has an open
/// decoder (see `Deck`). This keeps the library cheap to build, lets the
/// user load hundreds of files without exhausting FFmpeg context limits,
/// and matches the way Resolume treats inactive deck slots.
#[derive(Debug, Clone)]
pub struct ClipSlot {
    pub path: PathBuf,
    pub name: String,
}

impl ClipSlot {
    pub fn from_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string());
        Self { path, name }
    }
}

#[derive(Debug, Default)]
pub struct Library {
    pub clips: Vec<ClipSlot>,
    pub active: Option<usize>,
}

impl Library {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add<P: Into<PathBuf>>(&mut self, path: P) -> usize {
        let idx = self.clips.len();
        self.clips.push(ClipSlot::from_path(path.into()));
        idx
    }

    pub fn is_active(&self, index: usize) -> bool {
        self.active == Some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_falls_back_to_full_path_for_path_without_filename() {
        let slot = ClipSlot::from_path(PathBuf::from("/"));
        assert_eq!(slot.name, "/");
    }

    #[test]
    fn name_uses_basename() {
        let slot = ClipSlot::from_path(PathBuf::from("/tmp/foo bar.mp4"));
        assert_eq!(slot.name, "foo bar.mp4");
    }

    #[test]
    fn add_returns_index_and_appends() {
        let mut lib = Library::new();
        assert_eq!(lib.add("/a.mp4"), 0);
        assert_eq!(lib.add("/b.mp4"), 1);
        assert_eq!(lib.clips.len(), 2);
        assert!(lib.active.is_none());
    }
}
