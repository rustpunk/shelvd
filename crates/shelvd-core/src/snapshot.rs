//! A fully color-resolved, render-ready view of the terminal grid.
//!
//! `shelvd-term` produces a [`GridSnapshot`] each frame (resolving named/indexed
//! colors against the palette and applying inverse/dim), so `shelvd-render`
//! never needs to know about `alacritty_terminal` or the palette.

use crate::color::Rgba;

bitflags::bitflags! {
    /// Per-cell rendering attributes.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub struct CellFlags: u16 {
        const BOLD       = 1 << 0;
        const ITALIC     = 1 << 1;
        const UNDERLINE  = 1 << 2;
        const STRIKEOUT  = 1 << 3;
        /// Leading cell of a double-width (e.g. CJK) glyph.
        const WIDE       = 1 << 4;
        /// Trailing placeholder cell that follows a [`CellFlags::WIDE`] cell.
        const WIDE_SPACER = 1 << 5;
        /// Cell covered by the active selection.
        const SELECTED   = 1 << 6;
    }
}

/// One rendered cell: a character plus its resolved colors and attributes.
#[derive(Clone, Copy, Debug)]
pub struct CellSnapshot {
    pub c: char,
    pub fg: Rgba,
    pub bg: Rgba,
    pub flags: CellFlags,
}

impl CellSnapshot {
    pub fn blank(fg: Rgba, bg: Rgba) -> Self {
        Self { c: ' ', fg, bg, flags: CellFlags::empty() }
    }

    /// Whether this cell would draw nothing but its background.
    pub fn is_blank(&self) -> bool {
        self.c == ' ' || self.c == '\0'
    }
}

/// How the cursor is drawn. Deserializes from `"block"`/`"beam"`/`"underline"`/
/// `"hidden"` in config files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
    Hidden,
}

/// The cursor's position and appearance for this frame.
#[derive(Clone, Copy, Debug)]
pub struct CursorSnapshot {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgba,
    /// Color to paint the glyph sitting under a block cursor.
    pub text_color: Rgba,
}

/// Per-visible-row command-block decoration, parallel to the rows of a
/// [`GridSnapshot`]. `shelvd-term` fills this in; the renderer just paints it,
/// so all block-color resolution still lives behind the load-bearing seam.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowDecor {
    /// The command block this row belongs to (0 = none).
    pub block_id: u32,
    /// The row's block finished with a non-zero exit code.
    pub failed: bool,
    /// This is the first row of its block — draw a separator above it.
    pub block_top: bool,
}

/// A command header pinned to the top of the viewport when its block's prompt
/// has scrolled out of view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickyHeader {
    /// The block's command text.
    pub command: String,
    /// The block finished with a non-zero exit code.
    pub failed: bool,
}

/// A complete, render-ready snapshot of the visible grid.
#[derive(Clone, Debug)]
pub struct GridSnapshot {
    pub cols: u16,
    pub rows: u16,
    /// Row-major cells, length == `cols * rows`.
    pub cells: Vec<CellSnapshot>,
    pub cursor: Option<CursorSnapshot>,
    /// Default background, used to clear the surface.
    pub background: Rgba,
    /// Background painted over cells flagged [`CellFlags::SELECTED`].
    pub selection_color: Rgba,
    /// Per-row block decoration, length == `rows`.
    pub rows_decor: Vec<RowDecor>,
    /// Sticky command header for the block at the top of the viewport, if its
    /// prompt has scrolled off.
    pub sticky: Option<StickyHeader>,
    /// Left-edge stripe color for a failed block.
    pub block_stripe: Rgba,
    /// Subtle background wash (with alpha) over a failed block's rows.
    pub block_tint: Rgba,
    /// Hairline color drawn between adjacent blocks.
    pub block_separator: Rgba,
}

impl GridSnapshot {
    /// A grid filled with blank cells.
    pub fn filled(cols: u16, rows: u16, fg: Rgba, bg: Rgba) -> Self {
        let blank = CellSnapshot::blank(fg, bg);
        Self {
            cols,
            rows,
            cells: vec![blank; cols as usize * rows as usize],
            cursor: None,
            background: bg,
            selection_color: bg,
            rows_decor: vec![RowDecor::default(); rows as usize],
            sticky: None,
            block_stripe: bg,
            block_tint: bg,
            block_separator: bg,
        }
    }

    #[inline]
    pub fn index(&self, col: u16, row: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    #[inline]
    pub fn cell(&self, col: u16, row: u16) -> Option<&CellSnapshot> {
        if col < self.cols && row < self.rows {
            self.cells.get(self.index(col, row))
        } else {
            None
        }
    }
}
