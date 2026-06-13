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

/// How the cursor is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
