//! An immutable, owned buffer for one *completed* command block.
//!
//! Part of the per-block multi-grid epic: today `shelvd-term` keeps every
//! command's output in one live grid; the epic freezes each finished block into
//! its own buffer so the live grid only carries the running command. This module
//! defines the frozen buffer itself — pure data, produced later by the extractor
//! (extract completed blocks on OSC-133 `;D`) and consumed by the composite
//! snapshot/scroll model and the multi-buffer renderer.

use crate::snapshot::CellSnapshot;

/// A frozen, fully color-resolved buffer for one completed command block.
///
/// It holds two views of the block's output, both as resolved [`CellSnapshot`]s
/// (named/indexed colors already resolved against the palette, so the renderer
/// stays a dumb painter):
///
/// - **visual rows** — the block exactly as it was rendered at the width it was
///   captured (`cols`), stored row-major in [`cells`](Self::cells). Render-ready
///   as-is, with no reflow.
/// - **logical lines** — each line's full run of cells *before* it was
///   soft-wrapped to `cols`, in [`logical_lines`](Self::logical_lines). This is
///   the source of truth for reflowing the block to a new width later (per-block
///   reflow); a hard newline ends a logical line, a soft wrap does not.
///
/// Plus the decoration a block carries: its [`id`](Self::id),
/// [`command`](Self::command) text, whether it [`failed`](Self::failed), and the
/// [`cwd`](Self::cwd) it ran in.
///
/// This is pure data: constructing a `FrozenBlock` has no side effects and draws
/// nothing on its own.
#[derive(Clone, Debug)]
pub struct FrozenBlock {
    /// Stable block id, mirroring [`crate::snapshot::RowDecor::block_id`]
    /// (never 0; 0 means "no block").
    pub id: u32,
    /// The command text, as captured between OSC-133 `;B` and `;C`.
    pub command: String,
    /// The command finished with a non-zero exit code.
    pub failed: bool,
    /// Working directory the command ran in (OSC 7), best-effort.
    pub cwd: Option<String>,
    /// Width, in columns, the block was captured at — the row stride of
    /// [`cells`](Self::cells).
    pub cols: u16,
    /// Visual rows as rendered: row-major resolved cells, with
    /// `cells.len() == cols * rows`. Render-ready at `cols`.
    pub cells: Vec<CellSnapshot>,
    /// Logical (unwrapped) lines: each the full run of cells on a line before it
    /// was soft-wrapped at `cols`. Source for reflow to a new width.
    pub logical_lines: Vec<Vec<CellSnapshot>>,
}

impl FrozenBlock {
    /// Number of visual rows — `cells.len() / cols`, or 0 when `cols` is 0.
    #[inline]
    pub fn rows(&self) -> usize {
        if self.cols == 0 {
            0
        } else {
            self.cells.len() / self.cols as usize
        }
    }

    /// The visual cells of row `r` (length `cols`), or `None` if `r` is out of
    /// range or the block has no width.
    #[inline]
    pub fn row(&self, r: usize) -> Option<&[CellSnapshot]> {
        let cols = self.cols as usize;
        if cols == 0 {
            return None;
        }
        let start = r.checked_mul(cols)?;
        self.cells.get(start..start.checked_add(cols)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Rgba;

    fn cell(c: char) -> CellSnapshot {
        let w = Rgba::new(255, 255, 255, 255);
        let k = Rgba::new(0, 0, 0, 255);
        CellSnapshot { c, ..CellSnapshot::blank(w, k) }
    }

    fn line(s: &str) -> Vec<CellSnapshot> {
        s.chars().map(cell).collect()
    }

    #[test]
    fn construction_exposes_metadata_rows_and_logical_lines() {
        // A 4-wide block whose one logical line "hello" soft-wrapped to two
        // visual rows ("hell" / "o   ").
        let cols = 4u16;
        let mut cells = line("hell");
        cells.extend(line("o   "));
        let block = FrozenBlock {
            id: 7,
            command: "echo hello".to_owned(),
            failed: true,
            cwd: Some("/tmp".to_owned()),
            cols,
            cells,
            logical_lines: vec![line("hello")],
        };

        // Metadata is carried verbatim.
        assert_eq!(block.id, 7);
        assert_eq!(block.command, "echo hello");
        assert!(block.failed);
        assert_eq!(block.cwd.as_deref(), Some("/tmp"));

        // Visual rows are derived from the flat cell buffer and the width.
        assert_eq!(block.rows(), 2);
        let row0: String = block.row(0).unwrap().iter().map(|c| c.c).collect();
        let row1: String = block.row(1).unwrap().iter().map(|c| c.c).collect();
        assert_eq!(row0, "hell");
        assert_eq!(row1, "o   ");
        assert!(block.row(2).is_none(), "out-of-range rows return None");

        // The logical line keeps the unwrapped text for later reflow.
        let logical: String = block.logical_lines[0].iter().map(|c| c.c).collect();
        assert_eq!(logical, "hello");
    }

    #[test]
    fn zero_width_block_has_no_rows() {
        let block = FrozenBlock {
            id: 1,
            command: String::new(),
            failed: false,
            cwd: None,
            cols: 0,
            cells: Vec::new(),
            logical_lines: Vec::new(),
        };
        assert_eq!(block.rows(), 0);
        assert!(block.row(0).is_none());
    }
}
