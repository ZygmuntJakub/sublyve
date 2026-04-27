use std::path::PathBuf;

use avengine_compositor::Thumbnail;

/// One clip slot in the grid: the file on disk, a display name, and an
/// optional GPU thumbnail.
///
/// The thumbnail is owned by the slot. The corresponding `egui::TextureId`
/// (registered with `egui_wgpu::Renderer`) is tracked separately in the
/// `AppState` because egui registration is the app's responsibility — the
/// compositor crate stays egui-free.
pub struct ClipSlot {
    pub path: PathBuf,
    pub name: String,
    pub thumbnail: Option<Thumbnail>,
    pub thumbnail_id: Option<egui::TextureId>,
}

impl ClipSlot {
    pub fn from_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string());
        Self { path, name, thumbnail: None, thumbnail_id: None }
    }
}

/// 2D clip grid: `layers × columns`. Cells are addressed `(row, col)`
/// where `row = layer index`, `col = column index`. Storage is row-major
/// `Vec<Option<ClipSlot>>`; index helpers convert.
pub struct Library {
    layers: usize,
    columns: usize,
    cells: Vec<Option<ClipSlot>>,
}

impl Library {
    pub fn new(layers: usize, columns: usize) -> Self {
        let layers = layers.max(1);
        let columns = columns.max(1);
        let mut cells = Vec::with_capacity(layers * columns);
        cells.resize_with(layers * columns, || None);
        Self { layers, columns, cells }
    }

    pub fn layers(&self) -> usize {
        self.layers
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn idx(&self, row: usize, col: usize) -> Option<usize> {
        if row < self.layers && col < self.columns {
            Some(row * self.columns + col)
        } else {
            None
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&ClipSlot> {
        self.idx(row, col).and_then(|i| self.cells[i].as_ref())
    }

    /// Place a clip at `(row, col)`. Returns the previous occupant, if any
    /// (so the caller can free its egui texture id before dropping it).
    pub fn set(&mut self, row: usize, col: usize, clip: ClipSlot) -> Option<ClipSlot> {
        let i = self.idx(row, col)?;
        self.cells[i].replace(clip)
    }

    /// Clear `(row, col)` and return the removed clip (if any). Used by
    /// the test suite today; the UI route for "stop a clip" goes through
    /// `Layer::clear` (which empties the layer) rather than removing the
    /// library entry.
    #[cfg(test)]
    pub fn clear(&mut self, row: usize, col: usize) -> Option<ClipSlot> {
        let i = self.idx(row, col)?;
        self.cells[i].take()
    }

    /// Find the first empty cell in row-major order.
    pub fn first_empty(&self) -> Option<(usize, usize)> {
        for row in 0..self.layers {
            for col in 0..self.columns {
                if self.cells[row * self.columns + col].is_none() {
                    return Some((row, col));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(name: &str) -> ClipSlot {
        ClipSlot::from_path(PathBuf::from(format!("/tmp/{name}.mp4")))
    }

    #[test]
    fn idx_round_trips_within_bounds() {
        let lib = Library::new(3, 4);
        assert_eq!(lib.idx(0, 0), Some(0));
        assert_eq!(lib.idx(0, 3), Some(3));
        assert_eq!(lib.idx(1, 0), Some(4));
        assert_eq!(lib.idx(2, 3), Some(11));
    }

    #[test]
    fn idx_rejects_out_of_bounds() {
        let lib = Library::new(2, 2);
        assert_eq!(lib.idx(2, 0), None);
        assert_eq!(lib.idx(0, 2), None);
        assert_eq!(lib.idx(99, 99), None);
    }

    #[test]
    fn set_returns_previous_occupant() {
        let mut lib = Library::new(2, 2);
        assert!(lib.set(0, 0, slot("a")).is_none());
        let evicted = lib.set(0, 0, slot("b")).expect("should evict");
        assert_eq!(evicted.name, "a.mp4");
        assert_eq!(lib.cell(0, 0).expect("now b").name, "b.mp4");
    }

    #[test]
    fn clear_removes_and_returns() {
        let mut lib = Library::new(1, 1);
        lib.set(0, 0, slot("x"));
        let removed = lib.clear(0, 0).expect("had a clip");
        assert_eq!(removed.name, "x.mp4");
        assert!(lib.cell(0, 0).is_none());
        assert!(lib.clear(0, 0).is_none(), "clearing empty is a no-op");
    }

    #[test]
    fn first_empty_walks_row_major() {
        let mut lib = Library::new(2, 3);
        assert_eq!(lib.first_empty(), Some((0, 0)));
        lib.set(0, 0, slot("a"));
        assert_eq!(lib.first_empty(), Some((0, 1)));
        lib.set(0, 1, slot("b"));
        lib.set(0, 2, slot("c"));
        assert_eq!(lib.first_empty(), Some((1, 0)));
    }

    #[test]
    fn first_empty_returns_none_when_full() {
        let mut lib = Library::new(1, 2);
        lib.set(0, 0, slot("a"));
        lib.set(0, 1, slot("b"));
        assert_eq!(lib.first_empty(), None);
    }

    #[test]
    fn dimensions_are_at_least_one() {
        let lib = Library::new(0, 0);
        assert_eq!(lib.layers(), 1);
        assert_eq!(lib.columns(), 1);
    }
}
