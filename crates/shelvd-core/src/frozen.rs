//! An immutable, owned buffer for one *completed* command block.
//!
//! Part of the per-block multi-grid epic: today `shelvd-term` keeps every
//! command's output in one live grid; the epic freezes each finished block into
//! its own buffer so the live grid only carries the running command. This module
//! defines the frozen buffer itself — pure data, produced later by the extractor
//! (extract completed blocks on OSC-133 `;D`) and consumed by the composite
//! snapshot/scroll model and the multi-buffer renderer.

use crate::snapshot::{CellFlags, CellSnapshot};

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
    /// The block's default fill (palette fg/bg at capture time), used to pad a
    /// visual row out to `cols` — including after [`reflow`](Self::reflow), where
    /// re-wrapping the trimmed logical lines leaves short trailing rows.
    pub blank: CellSnapshot,
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

    /// Re-wrap the block to a new width, regenerating the visual [`cells`](Self::cells)
    /// (and [`cols`](Self::cols)) from the width-independent
    /// [`logical_lines`](Self::logical_lines). This is per-block reflow: each
    /// logical line soft-wraps at `new_cols`, a wide glyph (a [`CellFlags::WIDE`]
    /// cell plus its trailing [`CellFlags::WIDE_SPACER`]) never straddling a wrap;
    /// each visual row is padded out to `new_cols` with [`blank`](Self::blank), and
    /// an empty logical line still occupies one blank row. The logical lines —
    /// the source of truth — are left untouched, so reflow is idempotent and
    /// reversible across widths. A no-op when the width is unchanged.
    pub fn reflow(&mut self, new_cols: u16) {
        if new_cols == self.cols {
            return;
        }
        self.cols = new_cols;
        let w = new_cols as usize;
        if w == 0 {
            self.cells.clear();
            return;
        }

        let mut cells: Vec<CellSnapshot> = Vec::new();
        for logical in &self.logical_lines {
            let row_start = cells.len();
            let mut col = 0usize;
            let mut i = 0usize;
            while i < logical.len() {
                let cell = logical[i];
                let wide = cell.flags.contains(CellFlags::WIDE);
                let width = if wide { 2 } else { 1 };
                // Break the row before a wide glyph that would straddle the wrap
                // (but never for a glyph too wide for the whole row — a 1-column
                // buffer — which would loop forever; there it degrades to a single
                // cell below).
                if width <= w && col + width > w {
                    while col < w {
                        cells.push(self.blank);
                        col += 1;
                    }
                    col = 0;
                }
                cells.push(cell);
                col += 1;
                if wide {
                    // Carry the trailing spacer with its glyph when it fits.
                    let spacer = logical
                        .get(i + 1)
                        .filter(|c| c.flags.contains(CellFlags::WIDE_SPACER));
                    if let Some(&spacer) = spacer {
                        i += 1; // consume the spacer regardless
                        if col < w {
                            cells.push(spacer);
                            col += 1;
                        }
                    }
                }
                i += 1;
            }
            // Pad the final visual row out to the width; an empty logical line
            // (including one trimmed down to nothing) still occupies one row.
            if cells.len() == row_start {
                cells.extend(std::iter::repeat_n(self.blank, w));
            } else {
                while (cells.len() - row_start) % w != 0 {
                    cells.push(self.blank);
                }
            }
        }
        self.cells = cells;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Rgba;

    const W: Rgba = Rgba::new(255, 255, 255, 255);
    const K: Rgba = Rgba::new(0, 0, 0, 255);

    fn blank() -> CellSnapshot {
        CellSnapshot::blank(W, K)
    }

    fn cell(c: char) -> CellSnapshot {
        CellSnapshot { c, ..blank() }
    }

    fn line(s: &str) -> Vec<CellSnapshot> {
        s.chars().map(cell).collect()
    }

    /// A wide glyph followed by its trailing spacer (a CJK cell, say), as the
    /// two-cell pair a reflow must keep together.
    fn wide(c: char) -> [CellSnapshot; 2] {
        let glyph = CellSnapshot { c, flags: CellFlags::WIDE, ..blank() };
        let spacer = CellSnapshot { c: ' ', flags: CellFlags::WIDE_SPACER, ..blank() };
        [glyph, spacer]
    }

    /// Render a frozen block's visual rows as plain strings, for assertions.
    fn rows_text(b: &FrozenBlock) -> Vec<String> {
        (0..b.rows())
            .map(|r| b.row(r).unwrap().iter().map(|c| c.c).collect())
            .collect()
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
            blank: blank(),
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
            blank: blank(),
        };
        assert_eq!(block.rows(), 0);
        assert!(block.row(0).is_none());
    }

    /// Build a frozen block from logical lines, wrapped at `cols`. The visual
    /// `cells` start out matching the logical text (no soft-wrap) so the tests can
    /// then reflow to a different width and check the result against `cols`.
    fn block_from(cols: u16, logical_lines: Vec<Vec<CellSnapshot>>) -> FrozenBlock {
        let mut b = FrozenBlock {
            id: 1,
            command: String::new(),
            failed: false,
            cwd: None,
            cols: cols.wrapping_add(1), // force reflow() to actually run
            cells: Vec::new(),
            logical_lines,
            blank: blank(),
        };
        b.reflow(cols);
        b
    }

    #[test]
    fn reflow_narrow_to_wide_unwraps() {
        // "hello" captured at width 4 (wrapped to "hell"/"o   "), reflowed wider.
        let mut b = block_from(4, vec![line("hello")]);
        assert_eq!(rows_text(&b), vec!["hell", "o   "]);
        b.reflow(10);
        assert_eq!(b.cols, 10);
        assert_eq!(rows_text(&b), vec!["hello     "], "fits on one wider row");
    }

    #[test]
    fn reflow_wide_to_narrow_rewraps() {
        let mut b = block_from(10, vec![line("hello world")]);
        assert_eq!(rows_text(&b), vec!["hello worl", "d         "]);
        b.reflow(5);
        assert_eq!(b.cols, 5);
        assert_eq!(rows_text(&b), vec!["hello", " worl", "d    "]);
    }

    #[test]
    fn reflow_round_trip_preserves_logical_text() {
        let original = vec![line("the quick brown fox"), line("jumps")];
        let mut b = block_from(19, original.clone());
        let logical_before: Vec<String> =
            b.logical_lines.iter().map(|l| l.iter().map(|c| c.c).collect()).collect();
        b.reflow(7);
        b.reflow(40);
        b.reflow(19);
        // Logical lines are the untouched source of truth.
        let logical_after: Vec<String> =
            b.logical_lines.iter().map(|l| l.iter().map(|c| c.c).collect()).collect();
        assert_eq!(logical_before, logical_after);
        // And the visual text concatenates back to the same content per line.
        let joined: String =
            rows_text(&b).iter().map(|r| r.trim_end()).collect::<Vec<_>>().join("|");
        assert_eq!(joined, "the quick brown fox|jumps");
    }

    #[test]
    fn reflow_keeps_wide_glyph_pairs_intact_across_a_wrap() {
        // "ab" then a wide glyph: at width 3 the glyph cannot share row 0 (a b _)
        // without splitting its spacer, so it wraps whole to row 1.
        let mut logical = line("ab");
        logical.extend(wide('世'));
        let b = block_from(3, vec![logical]);
        let rows = rows_text(&b);
        assert_eq!(rows.len(), 2, "wide glyph pushed to a new row: {rows:?}");
        assert_eq!(rows[0], "ab ", "row 0 padded rather than splitting the glyph");
        // The glyph and its spacer stay adjacent on row 1.
        let r1 = b.row(1).unwrap();
        assert_eq!(r1[0].c, '世');
        assert!(r1[0].flags.contains(CellFlags::WIDE));
        assert!(r1[1].flags.contains(CellFlags::WIDE_SPACER));
    }

    #[test]
    fn reflow_empty_logical_line_keeps_a_blank_row() {
        let b = block_from(6, vec![line("top"), Vec::new(), line("end")]);
        assert_eq!(rows_text(&b), vec!["top   ", "      ", "end   "]);
    }
}
