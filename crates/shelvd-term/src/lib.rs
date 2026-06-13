//! `shelvd-term` — terminal emulation state.
//!
//! Wraps [`alacritty_terminal`]: owns the [`Term`] grid and the `vte` parser,
//! feeds it raw PTY bytes, and produces a fully color-resolved
//! [`GridSnapshot`] for the renderer. Terminal-generated side effects (writes
//! back to the PTY, title changes, the bell) surface as [`TermEvent`]s on a
//! channel for the event-loop owner to act on.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor};
use alacritty_terminal::Term;

use shelvd_core::{
    CellFlags, CellSnapshot, CursorShape, CursorSnapshot, GridSnapshot, Palette, Rgba,
};

/// A side effect produced by the terminal while parsing.
#[derive(Debug, Clone)]
pub enum TermEvent {
    /// The terminal wants to send these bytes back to the child (device
    /// attribute replies, cursor position reports, etc.). Must be honored or
    /// some programs hang.
    PtyWrite(Vec<u8>),
    /// Window title change (OSC 0/2).
    Title(String),
    /// Reset the window title to the default.
    ResetTitle,
    /// The program asked to put text on the system clipboard (OSC 52).
    ClipboardStore(String),
    /// Ring the bell.
    Bell,
    /// The grid changed and should be redrawn.
    Wakeup,
    /// The mouse cursor shape may need updating.
    MouseCursorDirty,
    /// The cursor's blink configuration changed (DECSCUSR); the event loop
    /// should refresh its blink scheduling.
    CursorBlink,
    /// The child process exited.
    Exit,
}

/// Grid dimensions handed to alacritty. `total_lines == screen_lines`; the grid
/// adds scrollback itself based on [`Config::scrolling_history`].
#[derive(Clone, Copy, Debug)]
struct TermDimensions {
    cols: u16,
    rows: u16,
}

impl TermDimensions {
    fn new(cols: u16, rows: u16) -> Self {
        Self { cols: cols.max(1), rows: rows.max(1) }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Forwards alacritty events onto a channel, mapping them to [`TermEvent`].
#[derive(Clone)]
struct EventProxy(flume::Sender<TermEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::PtyWrite(s) => Some(TermEvent::PtyWrite(s.into_bytes())),
            Event::Title(t) => Some(TermEvent::Title(t)),
            Event::ResetTitle => Some(TermEvent::ResetTitle),
            Event::ClipboardStore(_, s) => Some(TermEvent::ClipboardStore(s)),
            Event::Bell => Some(TermEvent::Bell),
            Event::Wakeup => Some(TermEvent::Wakeup),
            Event::MouseCursorDirty => Some(TermEvent::MouseCursorDirty),
            Event::CursorBlinkingChange => Some(TermEvent::CursorBlink),
            Event::Exit => Some(TermEvent::Exit),
            // Callback-carrying queries (color/text-area/clipboard-load) are
            // ignored for now; they land in a later milestone.
            _ => None,
        };
        if let Some(ev) = mapped {
            let _ = self.0.send(ev);
        }
    }
}

/// The terminal model: parser + grid + color resolution.
pub struct Terminal {
    term: Term<EventProxy>,
    parser: Processor,
    events: flume::Receiver<TermEvent>,
    palette: Palette,
    cursor_shape: CursorShape,
    dims: TermDimensions,
}

impl Terminal {
    /// Create a terminal with the given grid size, scrollback depth, palette,
    /// and default cursor shape.
    pub fn new(
        cols: u16,
        rows: u16,
        scrollback: usize,
        palette: Palette,
        cursor_shape: CursorShape,
    ) -> Self {
        let (tx, rx) = flume::unbounded();
        let config = Config { scrolling_history: scrollback, ..Config::default() };
        let dims = TermDimensions::new(cols, rows);
        let term = Term::new(config, &dims, EventProxy(tx));
        Self {
            term,
            parser: Processor::new(),
            events: rx,
            palette,
            cursor_shape,
            dims,
        }
    }

    /// Feed bytes read from the PTY into the parser.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the grid to `cols` × `rows` cells.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.dims = TermDimensions::new(cols, rows);
        self.term.resize(self.dims);
    }

    /// Channel of terminal-generated side effects.
    pub fn events(&self) -> &flume::Receiver<TermEvent> {
        &self.events
    }

    /// Current grid size in cells.
    pub fn grid_size(&self) -> (u16, u16) {
        (self.dims.cols, self.dims.rows)
    }

    /// Replace the active palette (e.g. on theme change).
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    // --- scrollback ----------------------------------------------------------

    /// Scroll the viewport by `delta` lines (positive scrolls up, into history).
    pub fn scroll_lines(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Scroll up one screen.
    pub fn scroll_page_up(&mut self) {
        self.term.scroll_display(Scroll::PageUp);
    }

    /// Scroll down one screen.
    pub fn scroll_page_down(&mut self) {
        self.term.scroll_display(Scroll::PageDown);
    }

    /// Jump to the oldest line in history.
    pub fn scroll_to_top(&mut self) {
        self.term.scroll_display(Scroll::Top);
    }

    /// Jump back to the live edge (display offset 0).
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Whether the viewport is scrolled away from the live edge.
    pub fn is_scrolled(&self) -> bool {
        self.term.grid().display_offset() != 0
    }

    // --- terminal modes ------------------------------------------------------

    /// Whether the program enabled bracketed paste (DEC 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Whether the program is reading mouse events.
    pub fn mouse_mode(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether mouse reports should use SGR encoding (DEC 1006) rather than the
    /// legacy X10 byte encoding.
    pub fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    /// Whether the program asked for *all* pointer motion to be reported
    /// (DEC 1003), regardless of whether a button is held.
    pub fn mouse_report_all_motion(&self) -> bool {
        self.term.mode().contains(TermMode::MOUSE_MOTION)
    }

    /// Whether the program asked for button-held drag motion to be reported
    /// (DEC 1002).
    pub fn mouse_report_drag(&self) -> bool {
        self.term.mode().contains(TermMode::MOUSE_DRAG)
    }

    /// Whether the alternate screen is active (e.g. a full-screen TUI).
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether the program has requested a blinking cursor (DECSCUSR).
    pub fn cursor_blinking(&self) -> bool {
        self.term.cursor_style().blinking
    }

    // --- selection -----------------------------------------------------------

    /// Begin a simple (linear) selection at a viewport cell.
    pub fn selection_start(&mut self, col: u16, row: u16, right_half: bool) {
        let point = self.viewport_to_point(col, row);
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, side(right_half)));
    }

    /// Extend the in-progress selection to a viewport cell.
    pub fn selection_update(&mut self, col: u16, row: u16, right_half: bool) {
        let point = self.viewport_to_point(col, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side(right_half));
        }
    }

    /// Clear any active selection.
    pub fn selection_clear(&mut self) {
        self.term.selection = None;
    }

    /// The selected text, if the selection is non-empty.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// Map a viewport cell (accounting for scrollback offset) to a grid point.
    fn viewport_to_point(&self, col: u16, row: u16) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let max_col = self.dims.cols.saturating_sub(1) as usize;
        Point::new(Line(row as i32 - offset), Column((col as usize).min(max_col)))
    }

    /// Build a render-ready snapshot of the visible grid.
    pub fn snapshot(&self) -> GridSnapshot {
        let cols = self.dims.cols;
        let rows = self.dims.rows;
        let mut snap =
            GridSnapshot::filled(cols, rows, self.palette.foreground, self.palette.background);
        snap.selection_color = self.palette.selection;

        let selection = self.term.selection.as_ref().and_then(|s| s.to_range(&self.term));
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;

        for indexed in grid.display_iter() {
            let row_i = indexed.point.line.0 + offset;
            if row_i < 0 {
                continue;
            }
            let (row, col) = (row_i as usize, indexed.point.column.0);
            if row >= rows as usize || col >= cols as usize {
                continue;
            }
            let cell = indexed.cell;
            let dst = row * cols as usize + col;
            let selected = selection
                .as_ref()
                .is_some_and(|range| point_in_range(indexed.point, range));

            // Trailing/leading spacers of a wide glyph paint background only.
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                let (_, bg) = self.cell_colors(cell);
                let mut flags = CellFlags::WIDE_SPACER;
                if selected {
                    flags |= CellFlags::SELECTED;
                }
                snap.cells[dst] = CellSnapshot { c: ' ', fg: bg, bg, flags };
                continue;
            }

            let (fg, bg) = self.cell_colors(cell);
            let mut flags = CellFlags::empty();
            if cell.flags.contains(Flags::BOLD) {
                flags |= CellFlags::BOLD;
            }
            if cell.flags.contains(Flags::ITALIC) {
                flags |= CellFlags::ITALIC;
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                flags |= CellFlags::UNDERLINE;
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                flags |= CellFlags::STRIKEOUT;
            }
            if cell.flags.contains(Flags::WIDE_CHAR) {
                flags |= CellFlags::WIDE;
            }
            if selected {
                flags |= CellFlags::SELECTED;
            }

            let c = if cell.c == '\0' { ' ' } else { cell.c };
            snap.cells[dst] = CellSnapshot { c, fg, bg, flags };
        }

        snap.cursor = self.cursor_snapshot(offset, cols, rows);
        snap
    }

    fn cursor_snapshot(&self, offset: i32, cols: u16, rows: u16) -> Option<CursorSnapshot> {
        if self.cursor_shape == CursorShape::Hidden
            || !self.term.mode().contains(TermMode::SHOW_CURSOR)
        {
            return None;
        }
        let Point { line, column } = self.term.grid().cursor.point;
        let row_i = line.0 + offset;
        if row_i < 0 || row_i as usize >= rows as usize || column.0 >= cols as usize {
            return None;
        }
        Some(CursorSnapshot {
            col: column.0 as u16,
            row: row_i as u16,
            shape: self.cursor_shape,
            color: self.palette.cursor,
            text_color: self.palette.cursor_text,
        })
    }

    fn cell_colors(&self, cell: &Cell) -> (Rgba, Rgba) {
        let mut fg = self.resolve(cell.fg, cell.flags, true);
        let mut bg = self.resolve(cell.bg, cell.flags, false);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        (fg, bg)
    }

    fn resolve(&self, color: AnsiColor, flags: Flags, is_fg: bool) -> Rgba {
        match color {
            AnsiColor::Spec(rgb) => Rgba::rgb(rgb.r, rgb.g, rgb.b),
            AnsiColor::Indexed(i) => self.palette.indexed(i),
            AnsiColor::Named(named) => self.resolve_named(named, flags, is_fg),
        }
    }

    fn resolve_named(&self, named: NamedColor, flags: Flags, is_fg: bool) -> Rgba {
        use NamedColor::*;
        match named {
            Foreground | BrightForeground | DimForeground => self.palette.foreground,
            Background => self.palette.background,
            Cursor => self.palette.cursor,
            other => {
                let mut idx = other as usize;
                if idx >= 16 {
                    // Dim* and any other special slots: fall back to default fg.
                    return self.palette.foreground;
                }
                // Bold brightens the 8 base foreground colors (xterm behavior).
                if is_fg && idx < 8 && flags.contains(Flags::BOLD) {
                    idx += 8;
                }
                self.palette.indexed(idx as u8)
            }
        }
    }
}

/// Which half of a cell a pixel fell on, as an alacritty selection side.
fn side(right_half: bool) -> Side {
    if right_half {
        Side::Right
    } else {
        Side::Left
    }
}

/// Whether a grid point lies within a selection range (linear or block).
fn point_in_range(p: Point, range: &SelectionRange) -> bool {
    if range.is_block {
        p.line >= range.start.line
            && p.line <= range.end.line
            && p.column >= range.start.column
            && p.column <= range.end.column
    } else {
        let after_start = p.line > range.start.line
            || (p.line == range.start.line && p.column >= range.start.column);
        let before_end = p.line < range.end.line
            || (p.line == range.end.line && p.column <= range.end.column);
        after_start && before_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shelvd_core::Palette;

    fn terminal(cols: u16, rows: u16) -> Terminal {
        Terminal::new(cols, rows, 1000, Palette::default(), CursorShape::Block)
    }

    #[test]
    fn scrolls_into_history_and_back() {
        let mut t = terminal(10, 3);
        for i in 0..20 {
            t.process(format!("line{i}\r\n").as_bytes());
        }
        assert!(!t.is_scrolled(), "starts at the live edge");
        t.scroll_lines(5);
        assert!(t.is_scrolled(), "wheel-up moves into history");
        t.scroll_to_bottom();
        assert!(!t.is_scrolled(), "scroll-to-bottom returns to the live edge");
    }

    #[test]
    fn selection_yields_text() {
        let mut t = terminal(20, 3);
        t.process(b"hello world");
        t.selection_start(0, 0, false);
        t.selection_update(4, 0, true); // through the right half of column 4
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn snapshot_flags_selected_cells() {
        let mut t = terminal(20, 3);
        t.process(b"hello");
        t.selection_start(0, 0, false);
        t.selection_update(4, 0, true);
        let snap = t.snapshot();
        assert!(snap.cell(0, 0).unwrap().flags.contains(CellFlags::SELECTED));
        assert!(snap.cell(4, 0).unwrap().flags.contains(CellFlags::SELECTED));
        assert!(!snap.cell(6, 0).unwrap().flags.contains(CellFlags::SELECTED));
        assert_eq!(snap.selection_color, Palette::default().selection);
    }

    #[test]
    fn clearing_selection_drops_text() {
        let mut t = terminal(20, 3);
        t.process(b"hello");
        t.selection_start(0, 0, false);
        t.selection_update(4, 0, true);
        assert!(t.selection_text().is_some());
        t.selection_clear();
        assert!(t.selection_text().is_none());
    }

    #[test]
    fn cursor_blink_follows_decscusr() {
        let mut t = terminal(10, 3);
        assert!(!t.cursor_blinking(), "default cursor is steady");
        t.process(b"\x1b[1 q"); // DECSCUSR: blinking block
        assert!(t.cursor_blinking(), "DECSCUSR 1 enables blinking");
        t.process(b"\x1b[2 q"); // DECSCUSR: steady block
        assert!(!t.cursor_blinking(), "DECSCUSR 2 is steady");
    }
}
