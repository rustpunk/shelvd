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

use std::time::Instant;

use shelvd_core::{
    CellFlags, CellSnapshot, CursorShape, CursorSnapshot, GridSnapshot, Palette, Rgba, RowDecor,
    StickyHeader,
};

mod block;
mod osc133;

pub use block::{Block, BlockState};
pub use osc133::SemanticKind;
use osc133::{Marker, Scanner};

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
    /// A shell-integration semantic-prompt marker (OSC 133), anchored to the
    /// absolute grid line where the cursor sat when it arrived.
    SemanticPrompt { kind: SemanticKind, line: i64 },
    /// The shell reported its working directory (OSC 7).
    WorkingDirectory(String),
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
    /// Sender shared with [`EventProxy`], so OSC-133 markers detected by the tee
    /// reach the same channel as alacritty's own side effects.
    tx: flume::Sender<TermEvent>,
    /// Tees the byte stream for shell-integration markers `alacritty` drops.
    scanner: Scanner,
    /// Absolute grid-line index of active line 0, advanced as content scrolls
    /// into history. Anchors semantic-prompt markers to a stable line number.
    abs_base: i64,
    /// Last observed scrollback depth, to derive the scroll delta each chunk.
    prev_history: usize,
    /// Last observed alt-screen state, to resync across buffer swaps.
    prev_alt: bool,
    /// Command blocks, oldest first, delimited by semantic-prompt markers.
    blocks: Vec<Block>,
    /// Next block id to hand out (ids are never 0).
    next_block_id: u32,
    /// Working directory reported (OSC 7) since the last block opened.
    pending_cwd: Option<String>,
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
        let term = Term::new(config, &dims, EventProxy(tx.clone()));
        Self {
            term,
            parser: Processor::new(),
            events: rx,
            tx,
            scanner: Scanner::new(),
            abs_base: 0,
            prev_history: 0,
            prev_alt: false,
            blocks: Vec::new(),
            next_block_id: 1,
            pending_cwd: None,
            palette,
            cursor_shape,
            dims,
        }
    }

    /// Feed bytes read from the PTY into the parser.
    ///
    /// The bytes are teed for OSC-133 shell-integration markers *before* they
    /// reach alacritty (which silently drops them). To anchor each marker to the
    /// grid line where it occurred, the stream is fed to alacritty in segments
    /// split at every marker terminator, reading the cursor in between.
    pub fn process(&mut self, bytes: &[u8]) {
        let hits = self.scanner.scan(bytes);
        if hits.is_empty() {
            self.advance_segment(bytes);
            return;
        }
        let mut start = 0;
        for (end, marker) in hits {
            self.advance_segment(&bytes[start..end]);
            start = end;
            self.on_marker(marker);
        }
        self.advance_segment(&bytes[start..]);
    }

    /// Feed one segment to alacritty and refresh the absolute-line origin.
    fn advance_segment(&mut self, seg: &[u8]) {
        if seg.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, seg);
        self.sync_abs_base();
    }

    /// Act on a recognized shell-integration marker: update the block model and
    /// emit the marker on the event channel.
    fn on_marker(&mut self, marker: Marker) {
        match marker {
            Marker::Semantic(kind) => {
                let line = self.absolute_cursor_line();
                let col = self.term.grid().cursor.point.column.0;
                self.apply_semantic(&kind, line, col);
                let _ = self.tx.send(TermEvent::SemanticPrompt { kind, line });
            }
            Marker::Cwd(path) => {
                // Attach to the current block if it lacks one; stash for the next.
                if let Some(b) = self.blocks.last_mut() {
                    if b.cwd.is_none() {
                        b.cwd = Some(path.clone());
                    }
                }
                self.pending_cwd = Some(path.clone());
                let _ = self.tx.send(TermEvent::WorkingDirectory(path));
            }
        }
    }

    /// Fold one semantic-prompt marker into the block model.
    fn apply_semantic(&mut self, kind: &SemanticKind, line: i64, col: usize) {
        match kind {
            SemanticKind::PromptStart => {
                self.prune_blocks();
                let id = self.next_block_id;
                self.next_block_id += 1;
                let cwd = self.pending_cwd.take();
                self.blocks.push(Block::new(id, line, cwd));
            }
            SemanticKind::PromptEnd => {
                if let Some(b) = self.blocks.last_mut() {
                    b.command_line = line;
                    b.command_col = col;
                }
            }
            SemanticKind::OutputStart => {
                // Capture the typed command (B..C) exactly once.
                let pending = self
                    .blocks
                    .last()
                    .filter(|b| b.output_line.is_none())
                    .map(|b| (b.command_line, b.command_col));
                if let Some((cmd_line, cmd_col)) = pending {
                    let command = self.capture_command(cmd_line, cmd_col, line);
                    let started = Instant::now();
                    if let Some(b) = self.blocks.last_mut() {
                        b.output_line = Some(line);
                        b.command = command;
                        b.started_at = Some(started);
                    }
                }
            }
            SemanticKind::CommandFinished(exit) => {
                let out_line = self.blocks.last().and_then(|b| b.output_line);
                let excerpt = out_line.map(|out| self.capture_excerpt(out, line));
                if let Some(b) = self.blocks.last_mut() {
                    b.end_line = Some(line);
                    b.exit_code = *exit;
                    b.state = match exit {
                        Some(0) | None => BlockState::Success,
                        Some(_) => BlockState::Failed,
                    };
                    if let Some(e) = excerpt {
                        b.output_excerpt = e;
                    }
                }
            }
        }
    }

    /// All command blocks, oldest first.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Scroll so the previous block's prompt sits at the top of the viewport.
    /// Returns whether there was a previous block to jump to.
    pub fn scroll_to_prev_block(&mut self) -> bool {
        let top = self.viewport_top_line();
        let target = self.blocks.iter().rev().find(|b| b.prompt_line < top).map(|b| b.prompt_line);
        match target {
            Some(line) => {
                self.scroll_top_to(line);
                true
            }
            None => false,
        }
    }

    /// Scroll so the next block's prompt sits at the top of the viewport.
    pub fn scroll_to_next_block(&mut self) -> bool {
        let top = self.viewport_top_line();
        let target = self.blocks.iter().find(|b| b.prompt_line > top).map(|b| b.prompt_line);
        match target {
            Some(line) => {
                self.scroll_top_to(line);
                true
            }
            None => false,
        }
    }

    /// The full text (prompt through output) of the block currently at the top
    /// of the viewport, for copy-block.
    pub fn current_block_text(&self) -> Option<String> {
        let top = self.viewport_top_line();
        let cursor_abs = self.absolute_cursor_line();
        let idx = self.block_row(top, cursor_abs)?;
        let start_abs = self.blocks[idx].prompt_line;
        let end_abs = if idx + 1 < self.blocks.len() {
            self.blocks[idx + 1].prompt_line
        } else {
            self.blocks[idx].end_line.map_or(cursor_abs + 1, |e| e + 1)
        };
        let start = Point::new(Line(self.abs_to_grid_line(start_abs)), Column(0));
        let last_col = self.dims.cols.saturating_sub(1) as usize;
        let end = Point::new(Line(self.abs_to_grid_line(end_abs - 1)), Column(last_col));
        let text = self.term.bounds_to_string(start, end).trim_end().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Absolute line currently shown at the top of the viewport.
    fn viewport_top_line(&self) -> i64 {
        self.abs_base - self.term.grid().display_offset() as i64
    }

    /// Scroll the display so absolute line `abs` becomes the top visible row.
    fn scroll_top_to(&mut self, abs: i64) {
        let d_req = (self.abs_base - abs).max(0);
        let d0 = self.term.grid().display_offset() as i64;
        let delta = (d_req - d0) as i32;
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    /// Clamp an absolute line to a valid grid `Line`, so text extraction never
    /// indexes outside the retained buffer.
    fn abs_to_grid_line(&self, abs: i64) -> i32 {
        let g = abs - self.abs_base;
        let top = -(self.term.grid().history_size() as i64);
        let bottom = self.dims.rows.saturating_sub(1) as i64;
        g.clamp(top, bottom) as i32
    }

    /// Read the typed command text spanning `B`..`C`.
    fn capture_command(&self, cmd_line: i64, cmd_col: usize, out_line: i64) -> String {
        let last = out_line - 1; // output begins on out_line; command ends before it
        if last < cmd_line {
            return String::new();
        }
        let start = Point::new(Line(self.abs_to_grid_line(cmd_line)), Column(cmd_col));
        let end_col = self.dims.cols.saturating_sub(1) as usize;
        let end = Point::new(Line(self.abs_to_grid_line(last)), Column(end_col));
        if end.line < start.line {
            return String::new();
        }
        self.term.bounds_to_string(start, end).trim().to_string()
    }

    /// Read a trailing slice of a block's output, for later suggested actions.
    fn capture_excerpt(&self, out_line: i64, end_line: i64) -> String {
        const EXCERPT_LINES: i64 = 5;
        if end_line <= out_line {
            return String::new();
        }
        let last = end_line - 1;
        let first = (end_line - EXCERPT_LINES).max(out_line);
        let start = Point::new(Line(self.abs_to_grid_line(first)), Column(0));
        let end_col = self.dims.cols.saturating_sub(1) as usize;
        let end = Point::new(Line(self.abs_to_grid_line(last)), Column(end_col));
        self.term.bounds_to_string(start, end).trim_end().to_string()
    }

    /// Drop blocks whose whole range has scrolled out of retained history, plus
    /// a hard cap so a long-lived session can't grow the list without bound.
    fn prune_blocks(&mut self) {
        const MAX_BLOCKS: usize = 2000;
        let oldest = self.abs_base - self.term.grid().history_size() as i64;
        let mut keep_from = 0;
        for i in 0..self.blocks.len() {
            let end = self.blocks.get(i + 1).map_or(i64::MAX, |b| b.prompt_line);
            if end <= oldest {
                keep_from = i + 1;
            } else {
                break;
            }
        }
        if keep_from > 0 {
            self.blocks.drain(0..keep_from);
        }
        if self.blocks.len() > MAX_BLOCKS {
            let excess = self.blocks.len() - MAX_BLOCKS;
            self.blocks.drain(0..excess);
        }
    }

    /// The index of the block covering absolute line `abs`, if any. Blocks are
    /// contiguous: a block runs until the next block's prompt; the open block
    /// runs to its recorded end or the cursor.
    fn block_row(&self, abs: i64, cursor_abs: i64) -> Option<usize> {
        let count = self.blocks.partition_point(|b| b.prompt_line <= abs);
        if count == 0 {
            return None;
        }
        let idx = count - 1;
        let end = if idx + 1 < self.blocks.len() {
            self.blocks[idx + 1].prompt_line
        } else {
            self.blocks[idx].end_line.map_or(cursor_abs + 1, |e| e + 1)
        };
        if abs < end {
            Some(idx)
        } else {
            None
        }
    }

    /// Fill in per-row block decoration, the sticky header, and resolve the
    /// block colors from the palette (color resolution stays in this crate).
    fn decorate(&self, snap: &mut GridSnapshot, offset: i32, shift: i32) {
        snap.block_stripe = self.palette.indexed(9); // bright red
        let red = self.palette.indexed(1);
        snap.block_tint = Rgba::new(red.r, red.g, red.b, 30); // subtle wash
        snap.block_separator = self.palette.indexed(8); // ash

        if self.blocks.is_empty() {
            return;
        }
        let cursor_abs = self.absolute_cursor_line();
        for r in 0..snap.rows as i64 {
            // The bottom-anchor padding rows above the content map to no grid line.
            if r < shift as i64 {
                continue;
            }
            let abs = self.abs_base + (r - shift as i64) - offset as i64;
            if let Some(idx) = self.block_row(abs, cursor_abs) {
                let b = &self.blocks[idx];
                snap.rows_decor[r as usize] = RowDecor {
                    block_id: b.id,
                    failed: b.state == BlockState::Failed,
                    block_top: abs == b.prompt_line,
                };
            }
        }

        // Sticky header: the top visible row's block, if its prompt scrolled off.
        // Only when content fills the grid (no bottom-anchor padding above it).
        if shift == 0 {
            let top_abs = self.abs_base - offset as i64;
            if let Some(idx) = self.block_row(top_abs, cursor_abs) {
                let b = &self.blocks[idx];
                if b.prompt_line < top_abs && !b.command.is_empty() {
                    snap.sticky = Some(StickyHeader {
                        command: b.command.clone(),
                        failed: b.state == BlockState::Failed,
                    });
                }
            }
        }
    }

    /// Advance [`Self::abs_base`] by however many lines just scrolled into
    /// history. Exact until the scrollback buffer saturates; frozen while the
    /// alt screen is active (its content never enters the main history).
    fn sync_abs_base(&mut self) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let hist = self.term.grid().history_size();
        if alt != self.prev_alt {
            // The active grid swapped buffers; the main-screen origin is
            // preserved across the swap, so only rebase the history baseline.
            self.prev_alt = alt;
            self.prev_history = hist;
            return;
        }
        if !alt {
            if hist >= self.prev_history {
                self.abs_base += (hist - self.prev_history) as i64;
            } else {
                // History shrank (a clear): every existing anchor is meaningless.
                self.abs_base = 0;
                self.blocks.clear();
            }
        }
        self.prev_history = hist;
    }

    /// Absolute grid line of the cursor right now.
    fn absolute_cursor_line(&self) -> i64 {
        self.abs_base + self.term.grid().cursor.point.line.0 as i64
    }

    /// Resize the grid to `cols` × `rows` cells.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.dims = TermDimensions::new(cols, rows);
        self.term.resize(self.dims);
        // Reflow renumbers lines; rebase the scroll baseline to the new history
        // depth so the next chunk's delta is measured from solid ground. Block
        // anchors don't survive reflow (we keep a single grid, not Warp's
        // per-block grids), so drop them and let new commands re-establish them.
        self.prev_history = self.term.grid().history_size();
        self.prev_alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.blocks.clear();
        self.pending_cwd = None;
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
        let shift = self.display_shift();
        let max_col = self.dims.cols.saturating_sub(1) as usize;
        Point::new(Line(row as i32 - shift - offset), Column((col as usize).min(max_col)))
    }

    /// Blank rows to reserve above the grid so the live prompt rests on the
    /// bottom of the window (Warp-style) instead of climbing down from the top.
    /// Active only at the live edge of the main screen, and only until output
    /// grows tall enough to fill the grid; scrolled history and full-screen apps
    /// (the alternate screen) are laid out top-to-bottom as usual, so it returns
    /// 0 there and the snapshot matches a conventional terminal.
    fn display_shift(&self) -> i32 {
        let grid = self.term.grid();
        if grid.display_offset() != 0 || self.term.mode().contains(TermMode::ALT_SCREEN) {
            return 0;
        }
        let rows = self.dims.rows as i32;
        if rows <= 1 {
            return 0;
        }
        (rows - 1 - self.content_bottom()).max(0)
    }

    /// The lowest occupied visible row at the live edge: the cursor's row or the
    /// last row carrying a printed glyph, whichever is lower (0-based screen row).
    /// Only meaningful at display offset 0 — where a screen row equals its grid
    /// line — which is the only state [`Self::display_shift`] calls it in.
    fn content_bottom(&self) -> i32 {
        let grid = self.term.grid();
        let mut bottom = grid.cursor.point.line.0.max(0);
        for indexed in grid.display_iter() {
            let row = indexed.point.line.0;
            if row > bottom && indexed.cell.c != ' ' && indexed.cell.c != '\0' {
                bottom = row;
            }
        }
        bottom
    }

    /// Build a render-ready snapshot of the visible grid.
    pub fn snapshot(&self) -> GridSnapshot {
        let cols = self.dims.cols;
        let rows = self.dims.rows;
        let mut snap =
            GridSnapshot::filled(cols, rows, self.palette.foreground, self.palette.background);
        snap.selection_color = self.palette.selection;

        let shift = self.display_shift();
        let selection = self.term.selection.as_ref().and_then(|s| s.to_range(&self.term));
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;

        for indexed in grid.display_iter() {
            let row_i = indexed.point.line.0 + offset + shift;
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

        snap.cursor = self.cursor_snapshot(offset, shift, cols, rows);
        self.decorate(&mut snap, offset, shift);
        snap
    }

    fn cursor_snapshot(&self, offset: i32, shift: i32, cols: u16, rows: u16) -> Option<CursorSnapshot> {
        if self.cursor_shape == CursorShape::Hidden
            || !self.term.mode().contains(TermMode::SHOW_CURSOR)
        {
            return None;
        }
        let Point { line, column } = self.term.grid().cursor.point;
        let row_i = line.0 + offset + shift;
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
        // The line is bottom-anchored; select on the row the cursor landed on.
        let row = t.snapshot().cursor.expect("cursor visible").row;
        t.selection_start(0, row, false);
        t.selection_update(4, row, true); // through the right half of column 4
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn snapshot_flags_selected_cells() {
        let mut t = terminal(20, 3);
        t.process(b"hello");
        let row = t.snapshot().cursor.expect("cursor visible").row;
        t.selection_start(0, row, false);
        t.selection_update(4, row, true);
        let snap = t.snapshot();
        assert!(snap.cell(0, row).unwrap().flags.contains(CellFlags::SELECTED));
        assert!(snap.cell(4, row).unwrap().flags.contains(CellFlags::SELECTED));
        assert!(!snap.cell(6, row).unwrap().flags.contains(CellFlags::SELECTED));
        assert_eq!(snap.selection_color, Palette::default().selection);
    }

    #[test]
    fn clearing_selection_drops_text() {
        let mut t = terminal(20, 3);
        t.process(b"hello");
        let row = t.snapshot().cursor.expect("cursor visible").row;
        t.selection_start(0, row, false);
        t.selection_update(4, row, true);
        assert!(t.selection_text().is_some());
        t.selection_clear();
        assert!(t.selection_text().is_none());
    }

    #[test]
    fn live_prompt_anchors_to_the_bottom() {
        let mut t = terminal(20, 6);
        t.process(b"$ ");
        let snap = t.snapshot();
        let cur = snap.cursor.expect("cursor visible");
        assert_eq!(cur.row, 5, "the live prompt rests on the bottom row");
        assert_eq!(snap.cell(0, 5).unwrap().c, '$');
        assert!(snap.cell(0, 0).unwrap().is_blank(), "rows above are blank padding");
    }

    #[test]
    fn clear_keeps_the_prompt_at_the_bottom() {
        let mut t = terminal(20, 6);
        t.process(b"one\r\ntwo\r\n$ ");
        // What `clear` emits: home the cursor and erase the screen + scrollback.
        t.process(b"\x1b[H\x1b[2J\x1b[3J$ ");
        let snap = t.snapshot();
        assert_eq!(
            snap.cursor.expect("cursor visible").row,
            5,
            "clear leaves the prompt anchored to the bottom"
        );
        assert_eq!(snap.cell(0, 5).unwrap().c, '$');
    }

    #[test]
    fn full_screen_output_is_not_shifted() {
        // Once content fills the grid there is no padding: the first line sits on
        // the top row and the last on the bottom, exactly like a normal terminal.
        let mut t = terminal(20, 3);
        t.process(b"a\r\nb\r\nc");
        let snap = t.snapshot();
        assert_eq!(snap.cell(0, 0).unwrap().c, 'a');
        assert_eq!(snap.cell(0, 2).unwrap().c, 'c');
        assert_eq!(snap.cursor.expect("cursor visible").row, 2);
    }

    #[test]
    fn alt_screen_fills_from_the_top() {
        let mut t = terminal(20, 4);
        t.process(b"\x1b[?1049h"); // enter the alternate screen (full-screen apps)
        t.process(b"x");
        let snap = t.snapshot();
        assert_eq!(snap.cell(0, 0).unwrap().c, 'x', "alt screen is not bottom-anchored");
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

    /// Collect the absolute line of the first semantic-prompt event, if any.
    fn first_prompt_line(t: &Terminal) -> Option<i64> {
        t.events().try_iter().find_map(|e| match e {
            TermEvent::SemanticPrompt { line, .. } => Some(line),
            _ => None,
        })
    }

    #[test]
    fn semantic_prompt_anchors_to_cursor_line() {
        let mut t = terminal(20, 5);
        t.process(b"a\r\nb\r\nc\r\n\x1b]133;A\x07");
        // Three newlines on a 5-row grid leave the cursor on line 3, unscrolled.
        assert_eq!(first_prompt_line(&t), Some(3));
    }

    #[test]
    fn semantic_prompt_line_tracks_scrollback() {
        let mut t = terminal(20, 3);
        for i in 0..6 {
            t.process(format!("L{i}\r\n").as_bytes());
        }
        t.process(b"\x1b]133;A\x07");
        // Six lines on a 3-row grid scroll four into history (abs_base 4); the
        // cursor sits on the bottom visible row (2) -> absolute line 6.
        assert_eq!(first_prompt_line(&t), Some(6));
    }

    #[test]
    fn working_directory_event_surfaces() {
        let mut t = terminal(20, 3);
        t.process(b"\x1b]7;file://host/tmp/x\x07");
        let cwd = t.events().try_iter().find_map(|e| match e {
            TermEvent::WorkingDirectory(p) => Some(p),
            _ => None,
        });
        assert_eq!(cwd.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn marker_split_across_process_calls() {
        let mut t = terminal(20, 3);
        t.process(b"x\x1b]133;");
        t.process(b"B\x07y");
        let kinds: Vec<_> = t
            .events()
            .try_iter()
            .filter_map(|e| match e {
                TermEvent::SemanticPrompt { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec![SemanticKind::PromptEnd]);
    }

    #[test]
    fn builds_block_from_markers() {
        let mut t = terminal(40, 10);
        t.process(
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07file1\r\nfile2\r\n\x1b]133;D;0\x07",
        );
        let blocks = t.blocks();
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.command, "ls");
        assert_eq!(b.exit_code, Some(0));
        assert_eq!(b.state, BlockState::Success);
        assert!(b.output_excerpt.contains("file2"));
    }

    #[test]
    fn failed_block_decorates_rows() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07boom\r\n\x1b]133;C\x07err\r\n\x1b]133;D;1\x07");
        let b = &t.blocks()[0];
        assert_eq!(b.state, BlockState::Failed);
        assert_eq!(b.exit_code, Some(1));

        let snap = t.snapshot();
        // The block is anchored to the bottom, so its prompt row is the first
        // non-padding row; the blank rows above it carry no decoration.
        let top = snap
            .rows_decor
            .iter()
            .position(|d| d.block_top)
            .expect("a block-top row");
        let id = snap.rows_decor[top].block_id;
        assert_ne!(id, 0);
        assert!(snap.rows_decor[top].failed);
        assert!(snap.rows_decor[top].block_top, "prompt row is the block top");
        assert!(snap.rows_decor[top + 1].failed);
        assert!(!snap.rows_decor[top + 1].block_top);
        assert_eq!(snap.rows_decor[0].block_id, 0);
    }

    #[test]
    fn second_prompt_closes_the_first_block() {
        let mut t = terminal(40, 12);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07a\r\n\x1b]133;C\x07\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07b\r\n\x1b]133;C\x07");
        let blocks = t.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].command, "a");
        assert_eq!(blocks[1].command, "b");
        assert!(blocks[0].id != blocks[1].id);
    }

    #[test]
    fn current_block_text_reads_the_top_block() {
        let mut t = terminal(40, 10);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\r\n\x1b]133;C\x07hi\r\n\x1b]133;D;0\x07");
        let text = t.current_block_text().expect("a block is at the top");
        assert!(text.contains("echo hi"), "includes the command: {text:?}");
        assert!(text.contains("hi"), "includes the output: {text:?}");
    }

    #[test]
    fn block_navigation_scrolls_between_prompts() {
        let mut t = terminal(40, 4); // small grid so blocks scroll into history
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07one\r\n\x1b]133;C\x07a\r\nb\r\nc\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07two\r\n\x1b]133;C\x07d\r\ne\r\nf\r\n\x1b]133;D;0\x07");
        assert!(!t.is_scrolled(), "starts at the live edge");
        // Walk backward to the earliest block (each jump lands on a prompt above
        // the current top), then confirm forward navigation returns toward it.
        let mut jumps = 0;
        while t.scroll_to_prev_block() {
            jumps += 1;
        }
        assert!(jumps >= 1, "navigated to at least one earlier block");
        assert!(t.is_scrolled(), "now parked in history");
        assert!(t.scroll_to_next_block(), "can move forward to a later block");
    }

    #[test]
    fn sticky_header_appears_when_prompt_scrolls_off() {
        let mut t = terminal(20, 3);
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07");
        for i in 0..10 {
            input.extend_from_slice(format!("out{i}\r\n").as_bytes());
        }
        t.process(&input);
        let snap = t.snapshot();
        let sticky = snap.sticky.expect("prompt scrolled off the top -> sticky header");
        assert_eq!(sticky.command, "make");
    }
}
