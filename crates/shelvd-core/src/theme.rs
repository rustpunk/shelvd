//! Visual configuration: palette, font, spacing, cursor.

use crate::geometry::Padding;
use crate::palette::Palette;
use crate::snapshot::CursorShape;

/// Everything that controls how the terminal looks.
#[derive(Clone, Debug)]
pub struct Theme {
    pub palette: Palette,
    /// Preferred font family. `None` asks the renderer for a system monospace.
    pub font_family: Option<String>,
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Line-height multiplier applied to the font size.
    pub line_height: f32,
    /// Inner padding around the grid.
    pub padding: Padding,
    /// Default cursor shape.
    pub cursor_shape: CursorShape,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            palette: Palette::default(),
            font_family: None,
            font_size: 15.0,
            line_height: 1.25,
            padding: Padding::default(),
            cursor_shape: CursorShape::Block,
        }
    }
}
