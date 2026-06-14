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

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// What the editable compose band should display this frame: the command being
/// typed ahead and how many commands are already queued behind it. The event
/// loop pushes this with [`Terminal::set_compose`] while compose-next mode is
/// engaged; `None` is the resting state, where the band (if any) mirrors the
/// running command instead. The caret column is derived here from `text` (the
/// line is end-anchored), keeping all band layout inside this crate.
#[derive(Clone, Debug, Default)]
pub struct ComposeBand {
    /// The command composed so far.
    pub text: String,
    /// Commands queued ahead of this one, surfaced as an "N queued" hint.
    pub queued: usize,
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
    /// Whether any PTY output has arrived yet. Before the shell prints its first
    /// byte the grid is empty; suppressing the cursor until then avoids a lone
    /// cursor flashing at the bottom of an otherwise blank, bottom-anchored screen.
    seen_output: bool,
    /// Compose-next mode payload, pushed by the app. While set, the input band is
    /// forced (even at a prompt) and rendered as the editable compose line rather
    /// than the locked running-command mirror. `None` is the resting state.
    compose: Option<ComposeBand>,
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
            seen_output: false,
            compose: None,
        }
    }

    /// Feed bytes read from the PTY into the parser.
    ///
    /// The bytes are teed for OSC-133 shell-integration markers *before* they
    /// reach alacritty (which silently drops them). To anchor each marker to the
    /// grid line where it occurred, the stream is fed to alacritty in segments
    /// split at every marker terminator, reading the cursor in between.
    pub fn process(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.seen_output = true;
        }
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
                    // The prompt prefix (columns before the command), kept with its
                    // real colors so the band shows it in its normal style.
                    let prompt_prefix = self.capture_row_cells(cmd_line, cmd_col);
                    let started = Instant::now();
                    if let Some(b) = self.blocks.last_mut() {
                        b.output_line = Some(line);
                        b.command = command;
                        b.prompt_prefix = prompt_prefix;
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

    /// Resolve one alacritty cell into a render-ready [`CellSnapshot`] (colors via
    /// the full path so inverse/hidden/bold-brighten are respected, plus the
    /// attribute flags). Selection is layered on by the caller — it is a viewport
    /// concern, not a property of the cell. Shared by the grid snapshot and the
    /// prompt-prefix capture so both resolve cells identically.
    fn cell_to_snapshot(&self, cell: &Cell) -> CellSnapshot {
        // Trailing/leading spacers of a wide glyph paint background only.
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            let (_, bg) = self.cell_colors(cell);
            return CellSnapshot { c: ' ', fg: bg, bg, flags: CellFlags::WIDE_SPACER };
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
        let c = if cell.c == '\0' { ' ' } else { cell.c };
        CellSnapshot { c, fg, bg, flags }
    }

    /// Capture columns `[0, end_col)` of one absolute grid line as render-ready
    /// cells — the prompt prefix (everything before the typed command), kept with
    /// its real colors so the input band can show it in its normal style rather
    /// than a flat dim. Read at OutputStart, when the line is still on the grid.
    fn capture_row_cells(&self, line: i64, end_col: usize) -> Vec<CellSnapshot> {
        let g = Line(self.abs_to_grid_line(line));
        let grid = self.term.grid();
        let end = end_col.min(self.dims.cols as usize);
        (0..end).map(|c| self.cell_to_snapshot(&grid[g][Column(c)])).collect()
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
        // Only when content fills the grid — either no bottom-anchor padding above
        // the top row (shift == 0) or the input band pushed the top row off the
        // screen (shift < 0). The top row maps to `abs_base - shift - offset`.
        if shift <= 0 {
            let top_abs = self.abs_base - shift as i64 - offset as i64;
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

    /// Blank rows currently reserved above the grid by the bottom-anchor layout.
    /// The app watches this for the smooth fill transition: when a burst of output
    /// makes it shrink, content jumps up by that many cell-heights in one frame.
    pub fn anchor_shift(&self) -> u16 {
        self.display_shift().max(0) as u16
    }

    // --- compose-next mode ---------------------------------------------------

    /// Engage / update / exit compose-next mode. While `Some`, the terminal
    /// reserves a bottom band even at a prompt and renders the editable buffer
    /// (with a caret and an "N queued" hint) in place of the locked
    /// running-command mirror; `None` returns the band to its resting state. The
    /// app owns the authoritative edit state and pushes a fresh view here on
    /// every change, so layout ([`Self::display_shift`]) and rendering
    /// ([`Self::fill_input_band`]) stay derived from one source.
    pub fn set_compose(&mut self, compose: Option<ComposeBand>) {
        self.compose = compose;
    }

    /// Whether a command is running at the live edge: shell integration reported
    /// its output start (OSC 133;C) but not yet its finish (133;D). Compose-next
    /// mode only engages while this holds — there must be a running command to
    /// type ahead of — so the app gates the keybinding on it.
    pub fn command_running(&self) -> bool {
        matches!(self.blocks.last(), Some(b) if b.output_line.is_some() && b.end_line.is_none())
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

    /// Rows of breathing room left below the bottom-anchored prompt so it does not
    /// sit flush against the window's bottom edge. Reserved when the grid has ample
    /// room; it shrinks (and then vanishes) as output fills the screen, since the
    /// shift is clamped at 0 and content always wins over the gutter.
    const BOTTOM_GUTTER: i32 = 1;

    /// Blank rows to reserve above the grid so the live prompt rests near the
    /// bottom of the window (Warp-style) instead of climbing down from the top.
    /// Active only at the live edge of the main screen, and only until output
    /// grows tall enough to fill the grid; scrolled history and full-screen apps
    /// (the alternate screen) are laid out top-to-bottom as usual, so it returns
    /// 0 there and the snapshot matches a conventional terminal. A small
    /// [`Self::BOTTOM_GUTTER`] keeps the prompt off the very last row when room
    /// permits, collapsing gracefully as the screen fills.
    fn display_shift(&self) -> i32 {
        self.display_shift_with(self.input_band_rows())
    }

    /// [`Self::display_shift`] given a precomputed band reserve, so a caller that
    /// also fills the band ([`Self::snapshot`]) derives both from one value.
    fn display_shift_with(&self, band: i32) -> i32 {
        if self.is_scrolled() || self.alt_screen() {
            return 0;
        }
        let rows = self.dims.rows as i32;
        if rows <= 1 {
            return 0;
        }
        // Reserve a strip at the bottom: a breathing gutter at a prompt, or the
        // persistent input band while a command runs. The gutter yields to content
        // (clamp at 0); the band holds even once output fills the screen (clamp at
        // `-band`, scrolling the oldest visible row off the top — it self-heals
        // into scrollback as more output arrives).
        let reserve = Self::BOTTOM_GUTTER.max(band);
        (rows - 1 - reserve - self.content_bottom()).max(-band)
    }

    /// Rows to reserve at the bottom for the pinned input band. One while a shell
    /// command is running at the live edge of the main screen — so its output
    /// scrolls above a fixed strip — or while compose-next mode is engaged (so the
    /// editable line persists across the brief prompt between queued commands), and
    /// zero everywhere else: at a resting prompt the prompt *is* the bottom line
    /// (today's anchor, [`Self::BOTTOM_GUTTER`]), and the band is ruled out without
    /// shell integration, on the alt screen, and in scrolled-back history.
    fn input_band_rows(&self) -> i32 {
        // Need at least one row above the band; on a degenerate 1-row grid
        // `display_shift_with` also bails, so keep the two in agreement.
        if self.dims.rows <= 1 || self.is_scrolled() || self.alt_screen() {
            return 0;
        }
        if self.compose.is_some() || self.command_running() {
            1
        } else {
            0
        }
    }

    /// The lowest occupied visible row at the live edge: the cursor's row or the
    /// last row carrying visible content, whichever is lower (0-based screen row).
    /// A row counts as occupied if it holds a printed glyph *or* any cell whose
    /// effective background differs from the palette default — a highlighted blank
    /// line (all spaces, colored background) is still content and must not be
    /// clipped off the bottom by the anchor. Only meaningful at display offset 0 —
    /// where a screen row equals its grid line — which is the only state
    /// [`Self::display_shift`] calls it in.
    fn content_bottom(&self) -> i32 {
        let grid = self.term.grid();
        let mut bottom = grid.cursor.point.line.0.max(0);
        for indexed in grid.display_iter() {
            let row = indexed.point.line.0;
            if row <= bottom {
                continue;
            }
            let cell = indexed.cell;
            let has_glyph = cell.c != ' ' && cell.c != '\0';
            // Resolve through the full color path so inverse/hidden are respected.
            let colored_bg = self.cell_colors(cell).1 != self.palette.background;
            if has_glyph || colored_bg {
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

        let band = self.input_band_rows();
        let shift = self.display_shift_with(band);
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

            let mut snapshot = self.cell_to_snapshot(cell);
            if selected {
                snapshot.flags |= CellFlags::SELECTED;
            }
            snap.cells[dst] = snapshot;
        }

        snap.cursor = self.cursor_snapshot(offset, shift, cols, rows);
        self.decorate(&mut snap, offset, shift);
        self.fill_input_band(&mut snap, band);
        snap
    }

    /// Paint the input band and flag it on the snapshot. The band exists while a
    /// command is running (see [`Self::input_band_rows`]) or while compose-next
    /// mode is engaged; at a resting prompt the bottom row is the live prompt
    /// itself, so this is a no-op and the snapshot keeps its plain bottom-anchor.
    ///
    /// The band always reads as an anchored shell prompt: it leads with the
    /// captured prompt prefix (e.g. `user@host:~$ `) in its real colors, then —
    /// at rest — the running command dimmed so only the command is distinct, or —
    /// while composing — the editable buffer (see [`Self::fill_compose_band`]).
    fn fill_input_band(&self, snap: &mut GridSnapshot, band: i32) {
        if band <= 0 {
            return;
        }
        let cols = snap.cols as usize;
        let band_top = snap.rows.saturating_sub(band as u16) as usize;
        // De-emphasized so the running command reads as locked chrome, not live
        // output.
        let dim = self.palette.indexed(8); // ash / bright-black
        // Reset the band rows to blanks and drop any block decoration that mapped
        // onto them, so the strip is its own region rather than the running
        // block's trailing output.
        let blank = CellSnapshot::blank(dim, self.palette.background);
        for row in band_top..snap.rows as usize {
            for col in 0..cols {
                snap.cells[row * cols + col] = blank;
            }
            snap.rows_decor[row] = RowDecor::default();
        }
        snap.input_band_rows = band as u16;

        // Lead with the prompt prefix in its captured colors so the band reads as
        // a real prompt line, shared by the resting mirror and the compose line.
        let prefix_w = {
            let prefix = match self.blocks.last() {
                Some(b) => b.prompt_prefix.as_slice(),
                None => &[],
            };
            let row = &mut snap.cells[band_top * cols..(band_top + 1) * cols];
            let mut col = 0;
            for cell in prefix {
                if col >= cols {
                    break;
                }
                row[col] = *cell;
                col += 1;
            }
            col
        };

        if self.compose.is_some() {
            self.fill_compose_band(snap, band_top, prefix_w);
            return;
        }
        // Resting band: the running command after the prompt, dimmed so only the
        // command is the distinct, locked part. Fall back to a generic marker if
        // the command wasn't captured.
        let command = match self.blocks.last() {
            Some(b) if !b.command.is_empty() => b.command.as_str(),
            _ => "running…",
        };
        let row = &mut snap.cells[band_top * cols..(band_top + 1) * cols];
        write_row(&mut row[prefix_w.min(cols)..], command, blank);
    }

    /// Render the editable compose-next line after the prompt prefix: the typed
    /// command in the normal foreground (so it reads as live input, not the dim
    /// locked command), a beam caret at its end, and a right-aligned dim hint —
    /// the queue depth once commands wait, otherwise the affordance that Enter
    /// queues the line. The text is end-anchored: if it outgrows the band, only
    /// its tail (where the caret sits) shows. `prefix_w` is where the prompt ends.
    fn fill_compose_band(&self, snap: &mut GridSnapshot, band_top: usize, prefix_w: usize) {
        let Some(compose) = self.compose.as_ref() else {
            return;
        };
        let cols = snap.cols as usize;
        let bg = self.palette.background;
        let fg_blank = CellSnapshot::blank(self.palette.foreground, bg);
        let dim_blank = CellSnapshot::blank(self.palette.indexed(8), bg);

        let hint = if compose.queued > 0 {
            format!("{} queued", compose.queued)
        } else {
            "Enter to queue".to_owned()
        };
        let hint_w = UnicodeWidthStr::width(hint.as_str());
        // Reserve the hint's columns plus a gap at the right (dropped if it would
        // crowd the line), and a trailing column for the caret, so the editable
        // text collides with neither the prompt, the hint, nor the caret.
        let right = if hint_w + 1 < cols { hint_w + 1 } else { 0 };
        let text_cols = cols.saturating_sub(prefix_w).saturating_sub(right).saturating_sub(1);

        let shown = visible_tail(&compose.text, text_cols);
        let row = &mut snap.cells[band_top * cols..(band_top + 1) * cols];
        write_row(&mut row[prefix_w.min(cols)..], shown, fg_blank);
        if right > 0 {
            write_row(&mut row[cols - hint_w..], &hint, dim_blank);
        }

        // A beam caret reads as a text-insertion point, distinct from the block
        // cursor the shell draws; place it just past the prompt and the shown
        // text. This runs after `cursor_snapshot` set the grid cursor, so it takes
        // precedence.
        let caret = (prefix_w + UnicodeWidthStr::width(shown)).min(cols.saturating_sub(1)) as u16;
        snap.cursor = Some(CursorSnapshot {
            col: caret,
            row: band_top as u16,
            shape: CursorShape::Beam,
            color: self.palette.cursor,
            text_color: self.palette.cursor_text,
        });
    }

    fn cursor_snapshot(&self, offset: i32, shift: i32, cols: u16, rows: u16) -> Option<CursorSnapshot> {
        if !self.seen_output
            || self.cursor_shape == CursorShape::Hidden
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

/// Write `text` into a single display row (`cells`, length == the column count),
/// honoring double-width glyphs: a wide char takes a [`CellFlags::WIDE`] cell plus
/// a trailing [`CellFlags::WIDE_SPACER`], so it never overdraws its neighbor.
/// `blank` supplies the row's colors; writing stops when the row is full.
fn write_row(cells: &mut [CellSnapshot], text: &str, blank: CellSnapshot) {
    let cols = cells.len();
    let mut col = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w == 0 {
            continue; // control / zero-width: nothing to place
        }
        if col + w > cols {
            break;
        }
        cells[col] = CellSnapshot { c: ch, ..blank };
        if w == 2 {
            cells[col].flags = CellFlags::WIDE;
            cells[col + 1] = CellSnapshot { c: ' ', flags: CellFlags::WIDE_SPACER, ..blank };
        }
        col += w;
    }
}

/// The trailing slice of `text` whose display width fits in `max` columns, so an
/// end-anchored caret stays visible once the composed line outgrows the band. A
/// wide glyph that would straddle the boundary is dropped whole (its two columns
/// don't fit), never split.
fn visible_tail(text: &str, max: usize) -> &str {
    let mut width = 0;
    let mut start = text.len();
    for (i, ch) in text.char_indices().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max {
            break;
        }
        width += w;
        start = i;
    }
    &text[start..]
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
        // One blank gutter row is left below the prompt, so on a 6-row grid the
        // prompt rests on row 4 (the last row, 5, is breathing room).
        assert_eq!(cur.row, 4, "the live prompt rests just above the bottom gutter");
        assert_eq!(snap.cell(0, 4).unwrap().c, '$');
        assert!(snap.cell(0, 5).unwrap().is_blank(), "the bottom row is gutter");
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
            4,
            "clear leaves the prompt anchored near the bottom, above the gutter"
        );
        assert_eq!(snap.cell(0, 4).unwrap().c, '$');
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
    fn fresh_terminal_hides_the_cursor_until_first_output() {
        let mut t = terminal(20, 6);
        // Before any PTY byte arrives the bottom-anchored screen is blank; a lone
        // cursor must not flash there.
        assert!(t.snapshot().cursor.is_none(), "no cursor before first output");
        t.process(b"x");
        assert!(t.snapshot().cursor.is_some(), "cursor appears once output arrives");
    }

    #[test]
    fn colored_blank_row_participates_in_the_anchor() {
        // A row that is all spaces but carries a non-default background is visible
        // content: the anchor must reserve a line for it rather than clip it.
        let mut t = terminal(20, 6);
        // Row 0: a printed glyph. Row 1: five spaces painted blue (index 4), then
        // park the cursor back up on row 0 so the colored row, not the cursor, is
        // the lowest occupied line.
        t.process(b"top\r\n\x1b[44m     \x1b[0m\x1b[A\r");
        let snap = t.snapshot();
        // content_bottom == 1 -> shift = (6 - 1 - 1 - 1).max(0) = 3, so grid row 1
        // lands on display row 4, just above the one-row bottom gutter.
        let blue = Palette::default().indexed(4);
        assert_eq!(snap.cell(0, 4).unwrap().bg, blue, "the colored blank row is kept");
        assert!(snap.cell(0, 5).unwrap().is_blank(), "the bottom row stays a gutter");
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

    /// Read a display row's glyphs into a string, for asserting on band text.
    fn row_text(snap: &GridSnapshot, row: u16) -> String {
        (0..snap.cols).filter_map(|c| snap.cell(c, row).map(|x| x.c)).collect()
    }

    #[test]
    fn running_command_reserves_a_bottom_band() {
        let mut t = terminal(20, 6);
        // A block in the running state: prompt, command, output started but not
        // finished (no 133;D).
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07ping x\r\n\x1b]133;C\x07reply\r\n");
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 1, "a running command reserves the band");
        let last = snap.rows - 1;
        assert_eq!(snap.cell(0, last).unwrap().c, '$', "band mirrors the prompt line");
        assert!(row_text(&snap, last).contains("ping x"), "band shows the executed command");
    }

    #[test]
    fn running_band_persists_when_output_fills_the_screen() {
        let mut t = terminal(20, 4);
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07loop\r\n\x1b]133;C\x07");
        for i in 0..12 {
            input.extend_from_slice(format!("out{i}\r\n").as_bytes());
        }
        t.process(&input);
        let snap = t.snapshot();
        // Output overflows the 4-row grid, yet the bottom strip is still the band,
        // not the newest output line: the persistent band did not collapse (the
        // single-grid cost is the oldest visible row scrolling off the top).
        assert_eq!(snap.input_band_rows, 1);
        let last = snap.rows - 1;
        assert_eq!(snap.cell(0, last).unwrap().c, '$');
        assert!(row_text(&snap, last).contains("loop"), "band still names the command");
        assert!(!row_text(&snap, last).contains("out"), "output never overwrites the band");
    }

    #[test]
    fn input_band_suppressed_on_a_one_row_grid() {
        // Degenerate 1-row grid: `display_shift` bails, so `input_band_rows` must
        // agree (return 0) or `fill_input_band` would overwrite the only row.
        let mut t = terminal(20, 1);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07go\r\n\x1b]133;C\x07out\r\n");
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 0, "no band when there is no row above it");
    }

    #[test]
    fn band_handles_wide_glyphs_in_the_command() {
        let mut t = terminal(20, 6);
        // A CJK char in the command is double-width: the band must flag it WIDE
        // with a trailing spacer rather than overdrawing its neighbor.
        t.process("\x1b]133;A\x07$ \x1b]133;B\x07編\r\n\x1b]133;C\x07x\r\n".as_bytes());
        let snap = t.snapshot();
        let last = snap.rows - 1;
        // Prompt line is "$ 編": '$'(0) ' '(1) '編'(2, wide) spacer(3).
        assert_eq!(snap.cell(2, last).unwrap().c, '編');
        assert!(snap.cell(2, last).unwrap().flags.contains(CellFlags::WIDE));
        assert!(snap.cell(3, last).unwrap().flags.contains(CellFlags::WIDE_SPACER));
    }

    #[test]
    fn prompt_has_no_input_band() {
        let mut t = terminal(20, 6);
        // Command finished and a fresh prompt drawn: no band — the prompt is the
        // bottom line itself (today's plain bottom-anchor, preserved).
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07x\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 0, "a prompt keeps the plain bottom-anchor");
        let cur = snap.cursor.expect("cursor visible");
        assert_eq!(snap.cell(0, cur.row).unwrap().c, '$');
    }

    #[test]
    fn command_running_tracks_output_to_finish() {
        let mut t = terminal(20, 6);
        assert!(!t.command_running(), "idle before any command");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07go\r\n\x1b]133;C\x07");
        assert!(t.command_running(), "output started, not yet finished");
        t.process(b"out\r\n\x1b]133;D;0\x07");
        assert!(!t.command_running(), "command finished");
    }

    #[test]
    fn compose_band_keeps_the_prompt_prefix_before_the_editable_text() {
        let mut t = terminal(40, 6);
        // A running command so the prompt prefix ("$ ") is captured.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        t.set_compose(Some(ComposeBand { text: "echo hi".into(), queued: 0 }));
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 1, "compose holds the band");
        let last = snap.rows - 1;
        let line = row_text(&snap, last);
        // Reads as an anchored prompt: prefix kept, editable command after it.
        assert!(line.starts_with("$ "), "the prompt prefix leads the band: {line:?}");
        assert!(line.contains("echo hi"), "the editable command follows it: {line:?}");
        let cur = snap.cursor.expect("a caret in the band");
        assert_eq!(cur.row, last, "the caret sits on the band row");
        // Caret after the prompt ("$ " = 2 cols) plus the typed text ("echo hi" = 7).
        assert_eq!(cur.col, 9, "the caret rests past the prompt and the typed text");
        assert_eq!(cur.shape, CursorShape::Beam, "a beam reads as a text-insertion point");
    }

    #[test]
    fn resting_band_keeps_the_prompt_normal_and_dims_the_command() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        let snap = t.snapshot();
        let last = snap.rows - 1;
        let pal = Palette::default();
        // The prompt prefix stays in its normal foreground; only the running
        // command is dimmed (ash) so it reads as the distinct, locked part.
        assert_eq!(snap.cell(0, last).unwrap().c, '$');
        assert_eq!(snap.cell(0, last).unwrap().fg, pal.foreground, "the prompt keeps its normal style");
        assert_eq!(snap.cell(2, last).unwrap().c, 's', "the command begins after the prompt");
        assert_eq!(snap.cell(2, last).unwrap().fg, pal.indexed(8), "the command is dimmed/distinct");
    }

    #[test]
    fn compose_band_overrides_the_locked_command_and_counts_the_queue() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        // While running and not composing, the band mirrors the executed command.
        assert!(row_text(&t.snapshot(), 5).contains("sleep 9"), "resting band mirrors the command");
        t.set_compose(Some(ComposeBand { text: "next".into(), queued: 2 }));
        let line = row_text(&t.snapshot(), 5);
        assert!(line.starts_with("$ "), "the prompt prefix is still there: {line:?}");
        assert!(line.contains("next"), "editable buffer replaces the command: {line:?}");
        assert!(!line.contains("sleep"), "the locked command is gone while composing: {line:?}");
        assert!(line.contains("2 queued"), "the queue depth is surfaced: {line:?}");
    }

    #[test]
    fn compose_band_hints_that_enter_queues() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        t.set_compose(Some(ComposeBand { text: "echo hi".into(), queued: 0 }));
        // With nothing queued yet, the hint teaches the queue affordance.
        assert!(row_text(&t.snapshot(), 5).contains("Enter to queue"), "the queue hint shows");
    }

    #[test]
    fn compose_band_yields_to_scrollback() {
        let mut t = terminal(20, 3);
        for i in 0..10 {
            t.process(format!("l{i}\r\n").as_bytes());
        }
        t.set_compose(Some(ComposeBand { text: "x".into(), queued: 0 }));
        t.scroll_lines(3);
        // Scrolled history lays out top-to-bottom like a normal terminal; the band
        // (and its forced reserve) yields, matching `viewport_to_point`.
        assert_eq!(t.snapshot().input_band_rows, 0, "no band in scrolled history");
    }

    #[test]
    fn compose_band_shows_the_caret_tail_when_it_overflows() {
        // A composed line wider than the grid keeps its end (where the caret is)
        // visible, dropping the head — the caret never leaves the band.
        let mut t = terminal(8, 4);
        t.set_compose(Some(ComposeBand { text: "abcdefghij".into(), queued: 0 }));
        let snap = t.snapshot();
        let last = snap.rows - 1;
        let line = row_text(&snap, last);
        assert!(line.trim_end().ends_with('j'), "the tail of the line is shown: {line:?}");
        let cur = snap.cursor.expect("a caret in the band");
        assert!(cur.col < snap.cols, "the caret stays within the band: {}", cur.col);
    }
}
