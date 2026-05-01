use std::path::PathBuf;

use avengine_compositor::Thumbnail;
use avengine_core::BlendMode;

/// Per-clip default settings, applied every time the clip is triggered
/// onto a layer (`Layer::load` writes them into the layer's transport +
/// blend mode). Live edits in the right-hand layer inspector still take
/// precedence until the next trigger; the defaults are what the clip
/// "wants" by default.
///
/// For *camera* cells `looping` and `speed` are silently ignored at
/// trigger time (live streams aren't seekable) — only `blend` is honoured.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClipDefaults {
    pub looping: bool,
    pub speed: f64,
    pub blend: BlendMode,
}

impl Default for ClipDefaults {
    fn default() -> Self {
        Self { looping: true, speed: 1.0, blend: BlendMode::Normal }
    }
}

/// What a library cell points at.
///
/// `File` is the original behaviour: a path to a video file on disk.
/// `Camera` is a live capture device — `format_name` is FFmpeg's input
/// format (`"avfoundation"` / `"v4l2"` / `"dshow"`), `device` is the
/// device-specific URL we hand to `avformat_open_input`,
/// `display_name` is the human-readable label we show in the UI and
/// match against on project reload, and `has_audio` records whether
/// enumeration paired the camera with a microphone — the trigger
/// path uses this to skip the audio-open attempt entirely on
/// video-only cameras (which would otherwise log a benign "no audio
/// stream" warning every time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellSource {
    File { path: PathBuf },
    Camera {
        format_name: String,
        device: String,
        display_name: String,
        has_audio: bool,
    },
}

impl CellSource {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Camera { .. })
    }
}

/// One clip slot in the grid: the source (file path or camera device),
/// a display name, an optional GPU thumbnail, and per-clip default
/// settings.
///
/// The thumbnail is owned by the slot. The corresponding `egui::TextureId`
/// (registered with `egui_wgpu::Renderer`) is tracked separately in the
/// `AppState` because egui registration is the app's responsibility — the
/// compositor crate stays egui-free. Camera cells don't decode a
/// thumbnail (would require opening the device); they render a glyph +
/// `name` instead.
pub struct ClipSlot {
    pub source: CellSource,
    pub name: String,
    pub thumbnail: Option<Thumbnail>,
    pub thumbnail_id: Option<egui::TextureId>,
    pub defaults: ClipDefaults,
}

impl ClipSlot {
    pub fn from_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string());
        Self {
            source: CellSource::File { path },
            name,
            thumbnail: None,
            thumbnail_id: None,
            defaults: ClipDefaults::default(),
        }
    }

    pub fn from_camera(
        format_name: String,
        device: String,
        display_name: String,
        has_audio: bool,
    ) -> Self {
        Self {
            source: CellSource::Camera {
                format_name,
                device,
                display_name: display_name.clone(),
                has_audio,
            },
            name: display_name,
            thumbnail: None,
            thumbnail_id: None,
            defaults: ClipDefaults::default(),
        }
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

    /// Mutable accessor — used by the bottom panel to write per-clip
    /// `ClipDefaults` (loop / speed / blend defaults).
    pub fn cell_mut(&mut self, row: usize, col: usize) -> Option<&mut ClipSlot> {
        let i = self.idx(row, col)?;
        self.cells[i].as_mut()
    }

    /// Place a clip at `(row, col)`. Returns the previous occupant, if any
    /// (so the caller can free its egui texture id before dropping it).
    pub fn set(&mut self, row: usize, col: usize, clip: ClipSlot) -> Option<ClipSlot> {
        let i = self.idx(row, col)?;
        self.cells[i].replace(clip)
    }

    /// Clear `(row, col)` and return the removed clip (if any). The UI
    /// route for "stop a clip" goes through `Layer::clear` (which
    /// empties the layer); this is for project-load and any future
    /// "remove clip" UI action.
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

    /// Append a new empty layer row (one empty cell per existing
    /// column). Returns `false` if at the hard limit.
    pub fn add_layer(&mut self, max_layers: usize) -> bool {
        if self.layers >= max_layers {
            return false;
        }
        for _ in 0..self.columns {
            self.cells.push(None);
        }
        self.layers += 1;
        true
    }

    /// Drop the highest-indexed layer (= the row visually at the top
    /// of the grid). Returns the cells from that row so the caller
    /// can free their egui texture ids; returns an empty vec when at
    /// the minimum (one row is always required).
    pub fn remove_layer(&mut self) -> Vec<Option<ClipSlot>> {
        if self.layers <= 1 {
            return Vec::new();
        }
        let last_start = (self.layers - 1) * self.columns;
        let dropped: Vec<Option<ClipSlot>> = self.cells.drain(last_start..).collect();
        self.layers -= 1;
        dropped
    }

    /// Append a new empty column on the right. Returns `false` if at
    /// the hard limit.
    pub fn add_column(&mut self, max_columns: usize) -> bool {
        if self.columns >= max_columns {
            return false;
        }
        let layers = self.layers;
        let old_cols = self.columns;
        let new_cols = old_cols + 1;
        let mut new_cells = Vec::with_capacity(layers * new_cols);
        for row in 0..layers {
            for col in 0..old_cols {
                new_cells.push(self.cells[row * old_cols + col].take());
            }
            new_cells.push(None);
        }
        self.cells = new_cells;
        self.columns = new_cols;
        true
    }

    /// Drop the rightmost column. Returns the dropped cells so the
    /// caller can free their egui texture ids and detect which
    /// layers had their active clip removed. Returns an empty vec
    /// when at the minimum (one column is always required).
    pub fn remove_column(&mut self) -> Vec<Option<ClipSlot>> {
        if self.columns <= 1 {
            return Vec::new();
        }
        let layers = self.layers;
        let old_cols = self.columns;
        let new_cols = old_cols - 1;
        let mut dropped = Vec::with_capacity(layers);
        let mut new_cells = Vec::with_capacity(layers * new_cols);
        for row in 0..layers {
            for col in 0..new_cols {
                new_cells.push(self.cells[row * old_cols + col].take());
            }
            dropped.push(self.cells[row * old_cols + new_cols].take());
        }
        self.cells = new_cells;
        self.columns = new_cols;
        dropped
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

    #[test]
    fn clip_defaults_are_loop_speed1_normal() {
        let d = ClipDefaults::default();
        assert!(d.looping);
        assert_eq!(d.speed, 1.0);
        assert_eq!(d.blend, BlendMode::Normal);
    }

    #[test]
    fn cell_mut_round_trips_a_default_change() {
        let mut lib = Library::new(1, 1);
        lib.set(0, 0, slot("x"));
        lib.cell_mut(0, 0).expect("present").defaults.looping = false;
        assert!(!lib.cell(0, 0).unwrap().defaults.looping);
    }

    #[test]
    fn add_layer_appends_empty_row() {
        let mut lib = Library::new(2, 3);
        lib.set(1, 2, slot("top-right"));
        assert!(lib.add_layer(8));
        assert_eq!(lib.layers(), 3);
        assert_eq!(lib.columns(), 3);
        // Existing data preserved.
        assert_eq!(lib.cell(1, 2).expect("kept").name, "top-right.mp4");
        // New row is fully empty.
        assert!(lib.cell(2, 0).is_none());
        assert!(lib.cell(2, 1).is_none());
        assert!(lib.cell(2, 2).is_none());
    }

    #[test]
    fn add_layer_respects_max() {
        let mut lib = Library::new(4, 2);
        assert!(!lib.add_layer(4));
        assert_eq!(lib.layers(), 4);
    }

    #[test]
    fn remove_layer_drops_last_row_and_returns_clips() {
        let mut lib = Library::new(3, 2);
        lib.set(2, 0, slot("a"));
        lib.set(2, 1, slot("b"));
        let dropped = lib.remove_layer();
        assert_eq!(lib.layers(), 2);
        assert_eq!(dropped.len(), 2);
        assert_eq!(dropped[0].as_ref().unwrap().name, "a.mp4");
        assert_eq!(dropped[1].as_ref().unwrap().name, "b.mp4");
    }

    #[test]
    fn remove_layer_keeps_minimum_of_one() {
        let mut lib = Library::new(1, 4);
        lib.set(0, 0, slot("only"));
        let dropped = lib.remove_layer();
        assert!(dropped.is_empty());
        assert_eq!(lib.layers(), 1);
        assert_eq!(lib.cell(0, 0).expect("preserved").name, "only.mp4");
    }

    #[test]
    fn add_column_inserts_at_end_of_each_row() {
        let mut lib = Library::new(2, 2);
        lib.set(0, 0, slot("a"));
        lib.set(0, 1, slot("b"));
        lib.set(1, 1, slot("c"));
        assert!(lib.add_column(8));
        assert_eq!(lib.columns(), 3);
        // Existing cells stay where they were.
        assert_eq!(lib.cell(0, 0).expect("a").name, "a.mp4");
        assert_eq!(lib.cell(0, 1).expect("b").name, "b.mp4");
        assert_eq!(lib.cell(1, 1).expect("c").name, "c.mp4");
        // New column is empty.
        assert!(lib.cell(0, 2).is_none());
        assert!(lib.cell(1, 2).is_none());
    }

    #[test]
    fn remove_column_drops_rightmost_and_returns_clips() {
        let mut lib = Library::new(2, 3);
        lib.set(0, 2, slot("right-top"));
        lib.set(1, 0, slot("left-bot"));
        let dropped = lib.remove_column();
        assert_eq!(lib.columns(), 2);
        assert_eq!(dropped.len(), 2);
        assert_eq!(dropped[0].as_ref().unwrap().name, "right-top.mp4");
        assert!(dropped[1].is_none()); // bottom-right was empty
        // Surviving cells.
        assert_eq!(lib.cell(1, 0).expect("kept").name, "left-bot.mp4");
    }

    #[test]
    fn remove_column_keeps_minimum_of_one() {
        let mut lib = Library::new(2, 1);
        lib.set(0, 0, slot("only"));
        let dropped = lib.remove_column();
        assert!(dropped.is_empty());
        assert_eq!(lib.columns(), 1);
        assert_eq!(lib.cell(0, 0).expect("preserved").name, "only.mp4");
    }
}
