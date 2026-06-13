//! Pixel/cell geometry and the conversion between window pixels and grid cells.

/// Size of the terminal grid in character cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

impl GridSize {
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    /// Number of cells in the grid.
    pub const fn area(self) -> usize {
        self.cols as usize * self.rows as usize
    }
}

/// A size in physical pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

impl PixelSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// The pixel footprint of a single cell, derived from the font.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CellMetrics {
    /// Advance width of one cell.
    pub width: f32,
    /// Line height (cell height).
    pub height: f32,
}

impl CellMetrics {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// Symmetric inner padding around the grid, in pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Padding {
    pub x: f32,
    pub y: f32,
}

impl Padding {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl Default for Padding {
    fn default() -> Self {
        Self { x: 8.0, y: 8.0 }
    }
}

impl GridSize {
    /// Compute how many whole cells fit in `px` after reserving `pad` on each edge.
    /// Always returns at least a 1×1 grid so downstream math never divides by zero.
    pub fn from_pixels(px: PixelSize, cell: CellMetrics, pad: Padding) -> GridSize {
        let usable_w = (px.width as f32 - 2.0 * pad.x).max(0.0);
        let usable_h = (px.height as f32 - 2.0 * pad.y).max(0.0);
        let cols = if cell.width > 0.0 { (usable_w / cell.width).floor() as u16 } else { 1 };
        let rows = if cell.height > 0.0 { (usable_h / cell.height).floor() as u16 } else { 1 };
        GridSize { cols: cols.max(1), rows: rows.max(1) }
    }
}
