//! Session-scoped undo / redo for **library edits**.
//!
//! In scope: every `library.set(...)`, `library.clear(...)`, and per-clip
//! default mutation via `library.cell_mut(...)`. Out of scope: transport
//! state, layer-level state, and the grid structure (`add_layer` /
//! `remove_column` / friends — those have bigger blast radius and
//! deserve their own confirmation flow).
//!
//! ## Storage choice — slot-preserving
//!
//! We store the **full `ClipSlot`** in each cell-shape op, including
//! the GPU `Thumbnail` and the registered `egui::TextureId`. Cost:
//! undo / redo is a pointer move, never a re-decode. The alternative
//! (storing only `CellSource` + name) would force every undo to
//! re-import from disk, which feels laggy on a 50-cell grid — and
//! sublyve's whole story is responsiveness.
//!
//! The trade-off: textures live in the history until the op falls off
//! the cap or the redo-tail is truncated by new work. At those exit
//! points the `History` hands every still-owned `egui::TextureId` to
//! the caller so egui can reclaim the GPU resource. There is exactly
//! one chokepoint (`record_op`) where library mutations enter the
//! history, so the texture-routing stays local to the
//! `AppState::*_with_undo` helpers.
//!
//! ## Op shape
//!
//! Two variants:
//!
//! * `Cell { row, col, kind, before, after }` — `before` and `after`
//!   are `Option<ClipSlot>`. The non-`None` side is the slot
//!   **currently not in the library**; the other side lives in the
//!   library and gets handed back on the next apply step. `kind`
//!   (`Place` / `Replace` / `Clear`) is baked in at record time so
//!   the menu label is stable across swaps.
//!
//! * `Defaults { row, col, before, after }` — per-clip
//!   loop / speed / blend setting change. No GPU resources.
//!
//! ## Coalescing
//!
//! A blend-mode dropdown or a speed slider can fire dozens of
//! `Defaults` ops in quick succession. Consecutive ops on the
//! *same cell* within `COALESCE_WINDOW` are merged into one entry
//! (only `after` is updated; `before` stays). One undo rewinds the
//! whole drag.

use std::time::{Duration, Instant};

use crate::library::{ClipDefaults, ClipSlot, Library};

/// Maximum ops retained. Older ops are dropped (with their textures
/// freed) when the stack grows past this. 50 is enough for "I keep
/// dragging clips into the wrong cell" without bloating GPU memory.
const HISTORY_CAP: usize = 50;

/// Consecutive `Defaults` ops on the same cell within this window are
/// merged into one entry. Keeps slider drags from filling the stack.
const COALESCE_WINDOW: Duration = Duration::from_millis(500);

/// What kind of cell change this op originally represented. Kept on
/// the op so undo / redo can log a meaningful label even after the
/// `before` / `after` slots have been swapped through the library.
///
/// `Clear` is currently unreached at runtime (no UI exists to clear
/// a single library cell — the existing layer-X button clears the
/// *playing* layer, not the library slot), but the variant + its
/// `record_clear` helper are kept ready so that future UI plumbing
/// can be added without re-touching this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// Placement into an empty cell.
    Place,
    /// Replacement of an existing occupant.
    Replace,
    /// Clearing an existing occupant.
    #[allow(dead_code)]
    Clear,
}

/// One reversible library edit.
///
/// `Cell` carries a full `ClipSlot` (GPU thumbnail + texture id); the
/// size disparity with `Defaults` is intentional. Boxing the large
/// variant would add an allocation on every op without measurably
/// helping — the stack is `Vec<LibraryOp>` with `HISTORY_CAP = 50`,
/// so the worst-case wasted space is bounded and small.
#[allow(clippy::large_enum_variant)]
pub enum LibraryOp {
    /// Cell content change. The non-`None` side is the slot **not**
    /// currently in the library; `apply_step` swaps which side is
    /// populated each time it runs.
    ///
    /// At record time we know which forward direction the op
    /// represented (Place / Replace / Clear) and bake that into
    /// `kind` so the menu label is stable.
    Cell {
        row: usize,
        col: usize,
        kind: CellKind,
        before: Option<ClipSlot>,
        after: Option<ClipSlot>,
    },
    Defaults {
        row: usize,
        col: usize,
        before: ClipDefaults,
        after: ClipDefaults,
    },
}

impl LibraryOp {
    /// Short label for the info-log on undo / redo.
    pub fn label(&self) -> &'static str {
        match self {
            LibraryOp::Cell { kind: CellKind::Place, .. } => "place",
            LibraryOp::Cell { kind: CellKind::Replace, .. } => "replace",
            LibraryOp::Cell { kind: CellKind::Clear, .. } => "clear",
            LibraryOp::Defaults { .. } => "defaults",
        }
    }
}

/// Bounded, cursor-based undo / redo for library edits.
///
/// `ops[..cursor]` is the undoable past; `ops[cursor..]` is the
/// redoable future. New ops truncate the future tail.
pub struct History {
    ops: Vec<LibraryOp>,
    /// Index of the *next* op to redo. Equivalently, the number of
    /// ops that have already been applied forward and can be undone.
    cursor: usize,
    /// Wall-clock of the last `record_op` push, for `Defaults` coalescing.
    last_push_at: Option<Instant>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    pub fn new() -> Self {
        Self {
            ops: Vec::new(),
            cursor: 0,
            last_push_at: None,
        }
    }

    /// True iff there is at least one undoable op. Powers the menu
    /// entry's greyed-out state.
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    /// True iff there is at least one redoable op.
    pub fn can_redo(&self) -> bool {
        self.cursor < self.ops.len()
    }

    /// Build + record the op for a placement into `(row, col)` whose
    /// previous occupant was `displaced` (the value `library.set`
    /// returned). The kind is `Place` if the cell was empty,
    /// `Replace` otherwise. `after` is left `None` — the live slot
    /// is in the library; we'll capture it on the first undo.
    #[must_use = "free the returned textures or they leak"]
    pub fn record_place(
        &mut self,
        row: usize,
        col: usize,
        displaced: Option<ClipSlot>,
    ) -> Vec<egui::TextureId> {
        let kind = if displaced.is_some() {
            CellKind::Replace
        } else {
            CellKind::Place
        };
        self.record_op(LibraryOp::Cell {
            row,
            col,
            kind,
            before: displaced,
            after: None,
        })
    }

    /// Build + record the op for a clear of `(row, col)` whose
    /// occupant was `removed` (the value `library.clear` returned —
    /// must be `Some`, since clearing an empty cell is a no-op the
    /// caller shouldn't record).
    ///
    /// Currently unused at runtime: no UI exists to clear a single
    /// library cell. Kept ready for when one is added.
    #[must_use = "free the returned textures or they leak"]
    #[allow(dead_code)]
    pub fn record_clear(
        &mut self,
        row: usize,
        col: usize,
        removed: ClipSlot,
    ) -> Vec<egui::TextureId> {
        self.record_op(LibraryOp::Cell {
            row,
            col,
            kind: CellKind::Clear,
            before: Some(removed),
            after: None,
        })
    }

    /// Build + record the op for a per-clip defaults change.
    #[must_use = "free the returned textures or they leak"]
    pub fn record_defaults(
        &mut self,
        row: usize,
        col: usize,
        before: ClipDefaults,
        after: ClipDefaults,
    ) -> Vec<egui::TextureId> {
        self.record_op(LibraryOp::Defaults { row, col, before, after })
    }

    /// Push a new op. Truncates any redo tail (and frees the textures
    /// it owned), coalesces consecutive `Defaults` on the same cell,
    /// and enforces `HISTORY_CAP` by dropping the oldest.
    ///
    /// The returned `Vec` lists texture ids the caller must free. It
    /// can be non-empty even on the happy path (redo-tail truncation,
    /// cap overflow), so always inspect it.
    #[must_use = "free the returned textures or they leak"]
    pub fn record_op(&mut self, op: LibraryOp) -> Vec<egui::TextureId> {
        let mut to_free = Vec::new();

        // 1) Discard the redo tail. Any textures stored there are now
        //    orphaned — free them.
        if self.cursor < self.ops.len() {
            let tail = self.ops.split_off(self.cursor);
            for dropped in tail {
                collect_textures(dropped, &mut to_free);
            }
        }

        // 2) Coalesce consecutive Defaults on same cell within window.
        let now = Instant::now();
        if let (
            LibraryOp::Defaults { row, col, after, .. },
            Some(last_at),
        ) = (&op, self.last_push_at)
            && now.duration_since(last_at) <= COALESCE_WINDOW
            && let Some(LibraryOp::Defaults {
                row: prow,
                col: pcol,
                after: pafter,
                ..
            }) = self.ops.last_mut()
            && *prow == *row
            && *pcol == *col
        {
            // Same cell, recent enough → mutate the existing op's
            // `after`. `before` (the original pre-drag state) stays.
            *pafter = *after;
            self.last_push_at = Some(now);
            return to_free;
        }

        // 3) Push, advance cursor.
        self.ops.push(op);
        self.cursor = self.ops.len();
        self.last_push_at = Some(now);

        // 4) Enforce cap by dropping oldest.
        while self.ops.len() > HISTORY_CAP {
            let dropped = self.ops.remove(0);
            collect_textures(dropped, &mut to_free);
            // The dropped op was in the past (before cursor), so the
            // cursor moves with it.
            self.cursor = self.cursor.saturating_sub(1);
        }

        to_free
    }

    /// Undo one op. Returns `None` if nothing to undo. Mutates
    /// `library` in place. The op's `before` and `after` are swapped
    /// in-place so a subsequent `redo` re-applies the forward
    /// direction.
    pub fn undo(&mut self, library: &mut Library) -> Option<()> {
        if !self.can_undo() {
            return None;
        }
        self.cursor -= 1;
        apply_step(&mut self.ops[self.cursor], library, /*forward=*/ false);
        self.last_push_at = None;
        Some(())
    }

    /// Redo one op. Returns `None` if nothing to redo.
    pub fn redo(&mut self, library: &mut Library) -> Option<()> {
        if !self.can_redo() {
            return None;
        }
        apply_step(&mut self.ops[self.cursor], library, /*forward=*/ true);
        self.cursor += 1;
        self.last_push_at = None;
        Some(())
    }

    /// Drop every op and free every texture they own. Called when
    /// the workspace is wiped (project-load) so the new project
    /// doesn't inherit ghosts from the previous session's stack.
    #[must_use = "free the returned textures or they leak"]
    pub fn clear(&mut self) -> Vec<egui::TextureId> {
        let mut to_free = Vec::new();
        for dropped in self.ops.drain(..) {
            collect_textures(dropped, &mut to_free);
        }
        self.cursor = 0;
        self.last_push_at = None;
        to_free
    }

    /// Label of the op that would be undone next, for menu text.
    pub fn peek_undo(&self) -> Option<&'static str> {
        self.cursor
            .checked_sub(1)
            .and_then(|i| self.ops.get(i))
            .map(LibraryOp::label)
    }

    /// Label of the op that would be redone next, for menu text.
    pub fn peek_redo(&self) -> Option<&'static str> {
        self.ops.get(self.cursor).map(LibraryOp::label)
    }
}

/// Apply one step (`forward = true` for redo, `false` for undo) by
/// installing one side of the op into the library and swapping the
/// op's `before` / `after` slots so the next step reverses cleanly.
///
/// Cell ops: the live slot the library returns from `set` / `clear`
/// is the "other side" we just displaced; it goes into the op so
/// the GPU `Thumbnail` is preserved across the round trip.
///
/// Defaults ops: we just write the chosen side into the cell. The
/// op's `before` / `after` are `Copy`, so a swap leaves both
/// available for the inverse step.
fn apply_step(op: &mut LibraryOp, library: &mut Library, forward: bool) {
    match op {
        LibraryOp::Cell { row, col, before, after, .. } => {
            // The side we're *installing* on this step.
            let install = if forward { after.take() } else { before.take() };
            let displaced = match install {
                Some(slot) => library.set(*row, *col, slot),
                None => library.clear(*row, *col),
            };
            // The displaced slot is the "other side"; store it for
            // the inverse step.
            if forward {
                *before = displaced;
            } else {
                *after = displaced;
            }
        }
        LibraryOp::Defaults { row, col, before, after } => {
            let want = if forward { *after } else { *before };
            if let Some(slot) = library.cell_mut(*row, *col) {
                slot.defaults = want;
            }
            // `before` / `after` are Copy; no swap needed.
        }
    }
}

fn collect_textures(op: LibraryOp, into: &mut Vec<egui::TextureId>) {
    match op {
        LibraryOp::Cell { before, after, .. } => {
            for side in [before, after].into_iter().flatten() {
                if let Some(id) = side.thumbnail_id {
                    into.push(id);
                }
            }
        }
        LibraryOp::Defaults { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ClipSlot;
    use std::path::PathBuf;

    fn slot(name: &str) -> ClipSlot {
        ClipSlot::from_path(PathBuf::from(format!("/tmp/{name}.mp4")))
    }

    #[test]
    fn fresh_history_has_no_undo_or_redo() {
        let h = History::new();
        assert!(!h.can_undo());
        assert!(!h.can_redo());
    }

    /// The integration pattern is: caller mutates library, then
    /// records the op describing what just changed. This helper
    /// mirrors that flow for tests.
    fn place(h: &mut History, lib: &mut Library, row: usize, col: usize, s: ClipSlot) {
        let displaced = lib.set(row, col, s);
        let _ = h.record_place(row, col, displaced);
    }

    fn clear(h: &mut History, lib: &mut Library, row: usize, col: usize) {
        if let Some(removed) = lib.clear(row, col) {
            let _ = h.record_clear(row, col, removed);
        }
    }

    #[test]
    fn place_op_undo_redo_round_trip() {
        let mut lib = Library::new(1, 1);
        let mut h = History::new();

        place(&mut h, &mut lib, 0, 0, slot("a"));
        assert!(h.can_undo());
        assert!(!h.can_redo());
        assert_eq!(lib.cell(0, 0).unwrap().name, "a.mp4");
        assert_eq!(h.peek_undo(), Some("place"));

        h.undo(&mut lib).unwrap();
        assert!(lib.cell(0, 0).is_none());
        assert!(!h.can_undo());
        assert!(h.can_redo());

        h.redo(&mut lib).unwrap();
        assert_eq!(lib.cell(0, 0).unwrap().name, "a.mp4");
        assert!(h.can_undo());
        assert!(!h.can_redo());
    }

    #[test]
    fn replace_op_round_trip_preserves_both_sides() {
        let mut lib = Library::new(1, 1);
        let mut h = History::new();
        place(&mut h, &mut lib, 0, 0, slot("a"));
        place(&mut h, &mut lib, 0, 0, slot("b"));
        assert_eq!(h.peek_undo(), Some("replace"));

        // We've placed `a` then replaced with `b`.
        assert_eq!(lib.cell(0, 0).unwrap().name, "b.mp4");

        // First undo: back to `a`.
        h.undo(&mut lib).unwrap();
        assert_eq!(lib.cell(0, 0).unwrap().name, "a.mp4");

        // Second undo: empty.
        h.undo(&mut lib).unwrap();
        assert!(lib.cell(0, 0).is_none());

        // Redo twice → back to `b`.
        h.redo(&mut lib).unwrap();
        h.redo(&mut lib).unwrap();
        assert_eq!(lib.cell(0, 0).unwrap().name, "b.mp4");
    }

    #[test]
    fn clear_op_undo_redo_round_trip() {
        let mut lib = Library::new(1, 1);
        lib.set(0, 0, slot("a"));
        let mut h = History::new();

        clear(&mut h, &mut lib, 0, 0);
        assert!(lib.cell(0, 0).is_none());
        assert_eq!(h.peek_undo(), Some("clear"));

        h.undo(&mut lib).unwrap();
        assert_eq!(lib.cell(0, 0).unwrap().name, "a.mp4");

        h.redo(&mut lib).unwrap();
        assert!(lib.cell(0, 0).is_none());
    }

    #[test]
    fn defaults_op_undo_redo_round_trip() {
        let mut lib = Library::new(1, 1);
        lib.set(0, 0, slot("a"));
        let mut h = History::new();

        let before = lib.cell(0, 0).unwrap().defaults;
        let mut after = before;
        after.looping = !before.looping;
        lib.cell_mut(0, 0).unwrap().defaults = after;
        let _ = h.record_op(LibraryOp::Defaults { row: 0, col: 0, before, after });

        h.undo(&mut lib).unwrap();
        assert_eq!(lib.cell(0, 0).unwrap().defaults.looping, before.looping);

        h.redo(&mut lib).unwrap();
        assert_eq!(lib.cell(0, 0).unwrap().defaults.looping, after.looping);
    }

    #[test]
    fn new_op_truncates_redo_tail() {
        let mut lib = Library::new(1, 2);
        let mut h = History::new();
        place(&mut h, &mut lib, 0, 0, slot("a"));
        place(&mut h, &mut lib, 0, 1, slot("b"));

        // Step back over the second op.
        h.undo(&mut lib).unwrap();
        assert!(h.can_redo());

        // A new op truncates the redo tail.
        place(&mut h, &mut lib, 0, 1, slot("c"));
        assert!(!h.can_redo());
        assert_eq!(h.ops.len(), 2); // op1 + the new op replacing op2
    }

    #[test]
    fn history_cap_drops_oldest_and_adjusts_cursor() {
        let mut lib = Library::new(1, 1);
        let mut h = History::new();
        for i in 0..(HISTORY_CAP + 10) {
            place(&mut h, &mut lib, 0, 0, slot(&format!("v{i}")));
        }
        assert_eq!(h.ops.len(), HISTORY_CAP);
        assert_eq!(h.cursor, HISTORY_CAP);
    }

    #[test]
    fn defaults_coalesce_within_window() {
        let mut lib = Library::new(1, 1);
        lib.set(0, 0, slot("a"));
        let mut h = History::new();

        let initial = lib.cell(0, 0).unwrap().defaults;
        let mut prev = initial;
        for step in 1..=5 {
            let mut next = prev;
            next.speed = 1.0 + 0.1 * step as f64;
            lib.cell_mut(0, 0).unwrap().defaults = next;
            let _ = h.record_defaults(0, 0, prev, next);
            prev = next;
        }
        // Five contiguous Defaults on the same cell coalesce into one.
        assert_eq!(h.ops.len(), 1);

        // One undo rewinds the whole drag back to the initial state.
        h.undo(&mut lib).unwrap();
        assert_eq!(lib.cell(0, 0).unwrap().defaults.speed, initial.speed);
    }

    #[test]
    fn defaults_no_coalesce_across_cells() {
        let mut lib = Library::new(1, 2);
        lib.set(0, 0, slot("a"));
        lib.set(0, 1, slot("b"));
        let mut h = History::new();

        let d = lib.cell(0, 0).unwrap().defaults;
        let mut after = d;
        after.looping = !d.looping;

        // First on (0,0).
        lib.cell_mut(0, 0).unwrap().defaults = after;
        let _ = h.record_defaults(0, 0, d, after);
        // Same kind but different cell → separate op (cell key differs).
        lib.cell_mut(0, 1).unwrap().defaults = after;
        let _ = h.record_defaults(0, 1, d, after);
        assert_eq!(h.ops.len(), 2);
    }

    #[test]
    fn clear_empties_history() {
        let mut lib = Library::new(1, 1);
        let mut h = History::new();
        place(&mut h, &mut lib, 0, 0, slot("a"));
        // No textures registered in tests; the API still returns an
        // empty `Vec<TextureId>` — but in production it would carry
        // the freed ids.
        let freed = h.clear();
        assert!(freed.is_empty());
        assert!(!h.can_undo());
        assert!(!h.can_redo());
    }

    #[test]
    fn peek_labels_match_state() {
        let mut lib = Library::new(1, 1);
        let mut h = History::new();
        assert_eq!(h.peek_undo(), None);
        assert_eq!(h.peek_redo(), None);

        place(&mut h, &mut lib, 0, 0, slot("a"));
        assert_eq!(h.peek_undo(), Some("place"));
        assert_eq!(h.peek_redo(), None);

        h.undo(&mut lib).unwrap();
        assert_eq!(h.peek_undo(), None);
        assert_eq!(h.peek_redo(), Some("place"));
    }
}
