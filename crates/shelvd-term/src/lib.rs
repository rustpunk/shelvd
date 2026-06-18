//! `shelvd-term` — terminal emulation state.
//!
//! Wraps [`alacritty_terminal`]: owns the [`Term`] grid and the `vte` parser,
//! feeds it raw PTY bytes, and produces a fully color-resolved
//! [`GridSnapshot`] for the renderer. Terminal-generated side effects (writes
//! back to the PTY, title changes, the bell) surface as [`TermEvent`]s on a
//! channel for the event-loop owner to act on.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor};
use alacritty_terminal::Term;

use std::time::Instant;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use shelvd_core::{
    CellFlags, CellSnapshot, CursorShape, CursorSnapshot, FrozenBlock, GridSnapshot, Palette, Rgba,
    RowDecor, StickyHeader,
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

/// What the bottom input band should display this frame, pushed by the app via
/// [`Terminal::set_band`]: the text typed into the always-editable input line and
/// the commands queued to run on upcoming prompts. While a command runs the band
/// is the live input field — when `input` is empty it shows the running command
/// grayed as a placeholder (see [`Terminal::command_running`]); as soon as the
/// user types, their text replaces it. All band layout stays inside this crate.
#[derive(Clone, Debug, Default)]
pub struct BandState {
    /// Text typed into the band's input line — the next command being composed.
    /// Empty means show the running command as a grayed placeholder instead.
    pub input: String,
    /// Commands queued to run on upcoming prompts, oldest (next to run) first;
    /// shown stacked above the input line, pushing the console up.
    pub queued: Vec<String>,
    /// Mask the typed input (render one bullet per character) because the running
    /// command has turned terminal echo off — e.g. a `sudo` password prompt. The
    /// real text is still sent on Enter; only the on-screen rendering is hidden.
    pub masked: bool,
    /// The un-typed completion to show dimmed after the caret (ghost text): the
    /// rest of a prior command whose prefix matches `input`. None = nothing to
    /// suggest. The app computes it; the band only paints it.
    pub suggestion: Option<String>,
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
    /// Frozen buffers of completed blocks, oldest first, one per finished block.
    /// Captured on OSC-133 `;D` and kept in lockstep with `blocks` (a finished
    /// block lives in both until pruned), index-aligned by id. The first slice of
    /// the multi-grid model: finished output owned here, leaving the live grid the
    /// active region.
    frozen: Vec<FrozenBlock>,
    /// Composite scroll position. `None` at the live edge (the bottom-anchored
    /// live grid). `Some(abs)` is the absolute line shown at the top of the
    /// viewport while scrolled back through history; the snapshot then composites
    /// frozen block buffers (above) with the live grid's active region (below),
    /// indexed in absolute-line space — independent of alacritty's own scrollback
    /// offset, which we leave at 0.
    scroll_top_abs: Option<i64>,
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
    /// Input-band state pushed by the app: the editable compose line and the
    /// type-ahead queue. While either is active the band is forced (even at a
    /// prompt) and shows the type-ahead UI; otherwise it falls back to the locked
    /// running-command mirror.
    band: BandState,
    /// Active text selection, in composite (absolute-line) coordinates so it spans
    /// frozen block buffers and the live grid alike. `None` when nothing is
    /// selected. Replaces alacritty's single-grid `Selection`.
    sel: Option<CompositeSelection>,
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
            frozen: Vec::new(),
            scroll_top_abs: None,
            next_block_id: 1,
            pending_cwd: None,
            palette,
            cursor_shape,
            dims,
            seen_output: false,
            band: BandState::default(),
            sel: None,
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
                // Freeze the now-completed block into an immutable buffer for the
                // multi-grid model. Its source rows stay on the live grid for now
                // (the composite that draws frozen buffers instead lands later),
                // so nothing is double-drawn.
                let frozen = self.blocks.last().and_then(|b| self.freeze_block(b));
                if let Some(frozen) = frozen {
                    self.frozen.push(frozen);
                }
            }
        }
    }

    /// All command blocks, oldest first.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Frozen buffers of completed blocks, oldest first — one per finished block,
    /// captured on OSC-133 `;D`. The live grid still renders their source rows for
    /// now; the composite snapshot that draws these instead lands with the rest of
    /// the multi-grid model.
    pub fn frozen_blocks(&self) -> &[FrozenBlock] {
        &self.frozen
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
        // Read through the composite, so a block that has scrolled out of the live
        // grid still copies from its frozen buffer (not clamped grid lines).
        let last_col = self.dims.cols.saturating_sub(1) as usize;
        let text = self.composite_text((start_abs, 0), (end_abs - 1, last_col));
        let text = text.trim_end().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Absolute line currently shown at the top of the viewport: the composite
    /// scroll position when scrolled, else the live edge (`abs_base`, the active
    /// grid's line 0).
    fn viewport_top_line(&self) -> i64 {
        self.scroll_top_abs.unwrap_or(self.abs_base)
    }

    /// Oldest absolute line the composite can render. With command blocks the
    /// first block's prompt is the floor: the frozen buffers are authoritative for
    /// finished output (and outlive the grid's own scrollback, so the first block
    /// may sit below the grid floor), and after a reflow the raw grid history below
    /// the re-anchored stack holds the same blocks at stale positions — so it must
    /// not be reachable. Without blocks the live grid's retained-history floor
    /// applies. Always `<= abs_base`, so it is a safe clamp lower bound.
    fn composite_oldest_abs(&self) -> i64 {
        match self.blocks.first() {
            // Clamp to `abs_base`: a block whose prompt sits inside the active
            // region (everything fits on one screen) leaves nothing to scroll up to.
            Some(b) => b.prompt_line.min(self.abs_base),
            None => self.abs_base - self.term.grid().history_size() as i64,
        }
    }

    /// Park the composite so absolute line `abs` is the top visible row, clamped to
    /// the oldest renderable line; snapping back to the live edge once `abs`
    /// reaches it.
    fn scroll_top_to(&mut self, abs: i64) {
        let edge = self.abs_base; // top line shown at the live edge
        let clamped = abs.clamp(self.composite_oldest_abs(), edge);
        self.scroll_top_abs = if clamped >= edge { None } else { Some(clamped) };
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

    /// Freeze a finished block into an immutable [`FrozenBlock`]: its rows
    /// (prompt→end) captured as resolved cells at the current width, plus the
    /// unwrapped logical lines for later reflow. Returns `None` until the block
    /// has an end line. Only retained rows are read — any head that already
    /// scrolled out of history is omitted, since those cells are gone.
    fn freeze_block(&self, b: &Block) -> Option<FrozenBlock> {
        let end = b.end_line?;
        let cols = self.dims.cols as usize;
        let oldest = self.abs_base - self.term.grid().history_size() as i64;
        let start = b.prompt_line.max(oldest);

        let grid = self.term.grid();
        let mut cells: Vec<CellSnapshot> = Vec::new();
        let mut logical_lines: Vec<Vec<CellSnapshot>> = Vec::new();
        let mut logical: Vec<CellSnapshot> = Vec::new();

        let mut abs = start;
        while abs <= end {
            let g = Line(self.abs_to_grid_line(abs));
            // Soft-wrap is flagged on a row's last cell: a wrapped row continues
            // the same logical line; an unwrapped row ends it.
            let wrapped = cols > 0 && grid[g][Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            for c in 0..cols {
                let snap = self.cell_to_snapshot(&grid[g][Column(c)]);
                cells.push(snap);
                logical.push(snap);
            }
            if !wrapped {
                trim_trailing_blanks(&mut logical);
                logical_lines.push(std::mem::take(&mut logical));
            }
            abs += 1;
        }
        // A trailing soft-wrapped run with no closing unwrapped row (defensive;
        // a finished block normally ends on an unwrapped line).
        if !logical.is_empty() {
            trim_trailing_blanks(&mut logical);
            logical_lines.push(logical);
        }

        Some(FrozenBlock {
            id: b.id,
            command: b.command.clone(),
            failed: b.state == BlockState::Failed,
            cwd: b.cwd.clone(),
            cols: self.dims.cols,
            cells,
            logical_lines,
            blank: CellSnapshot::blank(self.palette.foreground, self.palette.background),
        })
    }

    /// Drop blocks whose whole range has scrolled out of retained history *and*
    /// have no frozen buffer to render them from, plus a hard cap so a long-lived
    /// session can't grow the list without bound. Frozen blocks are authoritative
    /// for finished output and outlive the grid's own scrollback, so they survive
    /// the history floor and are bounded only by `MAX_BLOCKS`.
    fn prune_blocks(&mut self) {
        const MAX_BLOCKS: usize = 2000;
        let oldest = self.abs_base - self.term.grid().history_size() as i64;
        let mut keep_from = 0;
        for i in 0..self.blocks.len() {
            let end = self.blocks.get(i + 1).map_or(i64::MAX, |b| b.prompt_line);
            // A frozen-backed block (index-aligned by id) can still render once its
            // rows leave the grid, so the history floor doesn't evict it.
            let frozen_backed = self.frozen.get(i).is_some_and(|f| f.id == self.blocks[i].id);
            if end <= oldest && !frozen_backed {
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
        // Keep the frozen list in lockstep: drop any frozen buffer whose source
        // block was just pruned (ids are monotonic, both lists oldest-first).
        match self.blocks.first().map(|b| b.id) {
            Some(id) => self.frozen.retain(|f| f.id >= id),
            None => self.frozen.clear(),
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
                self.frozen.clear();
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
        // depth so the next chunk's delta is measured from solid ground.
        self.prev_history = self.term.grid().history_size();
        self.prev_alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.pending_cwd = None;
        // Re-wrap each frozen block's logical lines to the new width and re-anchor
        // the block stack against the reflowed grid, so command-block scrollback
        // survives the resize (per-block reflow) instead of being discarded.
        self.reanchor_blocks_for_reflow(cols);
    }

    /// Reflow the frozen buffers to `cols` and rebuild the block anchors against
    /// the reflowed live grid. alacritty's reflow renumbers the grid (and so
    /// invalidates the absolute anchors the composite addresses blocks by), and
    /// there's no way to recover where the old OSC marks landed — so re-derive the
    /// anchors: stack the (reflowed) frozen blocks contiguously, ending just above
    /// the open block's prompt, so finished output renders from the frozen buffers
    /// and the live grid only backs the active region. Falls back to clearing the
    /// whole block model if the `blocks`/`frozen` index-alignment invariant is
    /// somehow broken (the safe pre-reflow behavior).
    fn reanchor_blocks_for_reflow(&mut self, cols: u16) {
        if self.blocks.is_empty() {
            // Nothing anchored (e.g. no shell integration); the live grid alone
            // backs scrollback and alacritty has already reflowed it.
            self.frozen.clear();
            return;
        }
        // The open (unfinished) block, if any, is always the last one; every
        // finished block ahead of it has an index-aligned frozen buffer.
        let has_open = matches!(self.blocks.last(), Some(b) if b.end_line.is_none());
        let finished = self.blocks.len() - usize::from(has_open);
        if finished != self.frozen.len()
            || self.blocks.iter().zip(&self.frozen).any(|(b, f)| b.id != f.id)
        {
            self.blocks.clear();
            self.frozen.clear();
            return;
        }

        for fb in &mut self.frozen {
            fb.reflow(cols);
        }

        // Where the open block's content begins in the reflowed active region.
        // At a resting prompt the prompt sits on the cursor's line, so the frozen
        // stack ends just above it (and covers the on-screen finished tails, which
        // therefore render from frozen — no double-draw). While a command runs (or
        // in the brief gap with no open block) the prompt position is unrecoverable,
        // so anchor at the active-region top.
        let base = if has_open && !self.command_running() && !self.alt_screen() {
            self.abs_base + i64::from(self.term.grid().cursor.point.line.0.max(0))
        } else {
            self.abs_base
        };

        // Stack the finished blocks upward from `base - 1`, newest first.
        let mut end = base - 1;
        for i in (0..self.frozen.len()).rev() {
            let height = self.frozen[i].rows().max(1) as i64;
            let start = end - (height - 1);
            let out_off = self.blocks[i]
                .output_line
                .map(|o| (o - self.blocks[i].prompt_line).clamp(0, height - 1));
            self.blocks[i].prompt_line = start;
            self.blocks[i].output_line = out_off.map(|off| start + off);
            self.blocks[i].end_line = Some(end);
            end = start - 1;
        }

        // Pin the open block (if any) to the active-region boundary it now owns.
        if has_open {
            if let Some(b) = self.blocks.last_mut() {
                let out_off = b.output_line.map(|o| (o - b.prompt_line).max(0));
                b.prompt_line = base;
                b.output_line = out_off.map(|off| base + off);
            }
        }
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
    /// Moves the composite top line; clamped to the oldest renderable line and
    /// snapping back to the live edge at the bottom.
    pub fn scroll_lines(&mut self, delta: i32) {
        let cur = self.viewport_top_line();
        self.scroll_top_to(cur - delta as i64);
    }

    /// Scroll up one screen.
    pub fn scroll_page_up(&mut self) {
        self.scroll_lines(self.dims.rows.saturating_sub(1) as i32);
    }

    /// Scroll down one screen.
    pub fn scroll_page_down(&mut self) {
        self.scroll_lines(-(self.dims.rows.saturating_sub(1) as i32));
    }

    /// Jump to the oldest line in history.
    pub fn scroll_to_top(&mut self) {
        self.scroll_top_to(self.composite_oldest_abs());
    }

    /// Jump back to the live edge.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_top_abs = None;
    }

    /// Whether the viewport is scrolled away from the live edge.
    pub fn is_scrolled(&self) -> bool {
        self.scroll_top_abs.is_some()
    }

    /// Blank rows currently reserved above the output region by the bottom-anchor
    /// layout. The app watches this for the smooth fill transition: when a burst of
    /// output makes it shrink, the output glides up by that many cell-heights in one
    /// frame. (The pinned input band is unaffected; only the output region moves.)
    pub fn anchor_shift(&self) -> u16 {
        self.display_shift().max(0) as u16
    }

    // --- compose-next mode ---------------------------------------------------

    /// Push the bottom input band's state (the editable compose line and the
    /// type-ahead queue). While either is non-empty the terminal reserves a band
    /// even at a prompt and renders the type-ahead UI in place of the locked
    /// running-command mirror. The app owns the authoritative edit/queue state and
    /// pushes a fresh view here on every change, so band height
    /// ([`Self::display_shift`]) and rendering ([`Self::fill_input_band`]) stay
    /// derived from one source.
    pub fn set_band(&mut self, band: BandState) {
        self.band = band;
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

    /// Begin a linear selection at a viewport cell.
    pub fn selection_start(&mut self, col: u16, row: u16, right_half: bool) {
        let p = self.viewport_to_abs(col, row, right_half);
        self.sel = Some(CompositeSelection { anchor: p, head: p });
    }

    /// Extend the in-progress selection to a viewport cell.
    pub fn selection_update(&mut self, col: u16, row: u16, right_half: bool) {
        let p = self.viewport_to_abs(col, row, right_half);
        if let Some(sel) = self.sel.as_mut() {
            sel.head = p;
        }
    }

    /// Clear any active selection.
    pub fn selection_clear(&mut self) {
        self.sel = None;
    }

    /// The selected text, if the selection is non-empty. Walks the selected
    /// absolute lines through [`Self::composite_row_into`], so it reads frozen
    /// block buffers and the live grid identically — a selection can span the
    /// frozen/live boundary. Each visual line is right-trimmed and joined by `\n`.
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.sel?;
        let cols = self.dims.cols as usize;
        let (start, end) = sel.bounds(cols)?;
        let text = self.composite_text(start, end);
        (!text.trim().is_empty()).then_some(text)
    }

    /// Concatenate the composite cells in the inclusive `(abs, col)` range
    /// `start..=end` (reading order) as text: each absolute line right-trimmed,
    /// wide-glyph spacers dropped, lines joined by `\n`. Shared by selection copy
    /// and whole-block copy.
    fn composite_text(&self, start: (i64, usize), end: (i64, usize)) -> String {
        let cols = self.dims.cols as usize;
        let cursor_abs = self.absolute_cursor_line();
        let mut buf =
            vec![CellSnapshot::blank(self.palette.foreground, self.palette.background); cols];
        let mut lines: Vec<String> = Vec::new();
        for abs in start.0..=end.0 {
            self.composite_row_into(abs, cursor_abs, &mut buf);
            let lo = if abs == start.0 { start.1 } else { 0 };
            let hi = if abs == end.0 { end.1 } else { cols.saturating_sub(1) };
            let line: String = buf
                .get(lo..=hi)
                .unwrap_or(&[])
                .iter()
                .filter(|c| !c.flags.contains(CellFlags::WIDE_SPACER))
                .map(|c| c.c)
                .collect();
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// Map a viewport cell to a composite selection endpoint (absolute line +
    /// column + which cell-half the pointer fell on). The line math mirrors the
    /// snapshot's row→line mapping (scrolled: straight onto the composite line;
    /// live edge: undo the bottom-anchor shift) but yields an absolute line, so the
    /// selection lives in the same coordinate space as the composite.
    fn viewport_to_abs(&self, col: u16, row: u16, right_half: bool) -> SelPoint {
        let max_col = self.dims.cols.saturating_sub(1) as usize;
        let abs = match self.scroll_top_abs {
            Some(top) => top + row as i64,
            None => self.abs_base + row as i64 - self.display_shift() as i64,
        };
        SelPoint { abs, col: (col as usize).min(max_col), side: side(right_half) }
    }

    /// Whether the composite cell at `(abs, col)` lies within the active
    /// selection. Linear selection is a reading-order range, so the
    /// lexicographic `(abs, col)` comparison is exactly the membership test.
    fn is_selected(&self, abs: i64, col: usize) -> bool {
        let Some(sel) = self.sel else { return false };
        let Some((start, end)) = sel.bounds(self.dims.cols as usize) else {
            return false;
        };
        (abs, col) >= start && (abs, col) <= end
    }

    /// Rows of breathing room left below the bottom-anchored prompt so it does not
    /// sit flush against the window's bottom edge. Reserved when the grid has ample
    /// room; it shrinks (and then vanishes) as output fills the screen, since the
    /// shift is clamped at 0 and content always wins over the gutter.
    const BOTTOM_GUTTER: i32 = 1;

    /// Most queued commands shown as their own rows in the band before the rest
    /// collapse into a "+N more" line, so a long queue can't swallow the screen.
    const MAX_QUEUE_ROWS: i32 = 6;

    /// Blank rows to reserve above the output region so it rests just above the
    /// pinned input band (Warp-style). The live input line is relocated into the
    /// band ([`Self::fill_input_band`]) and holds still there, so the output below
    /// is laid out **like a conventional terminal**: it hugs the band, fills
    /// upward, and scrolls the oldest visible row off the top once it grows past
    /// the grid. The reserve shrinks (then the offset goes negative) as output
    /// fills, clamped at `-band`. Scrolled history and the alternate screen are laid
    /// out plainly, so it returns 0 there.
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
        // Bottom-anchor the output region just above the band; it fills upward and
        // then scrolls (the `-band` floor is hit once alacritty itself scrolls,
        // content_bottom saturating at `rows - 1`). At a resting prompt the whole
        // (soft-wrapped) input line is relocated into the band
        // ([`Self::fill_resting_input`]), so those rows do not count toward the
        // output's lowest line — subtract their height, or the output would sit
        // needlessly high.
        let reserve = Self::BOTTOM_GUTTER.max(band);
        let relocated = if self.command_running() { 0 } else { self.resting_input_height() };
        let out_bottom = self.content_bottom() - relocated;
        (rows - 1 - reserve - out_bottom).max(-band)
    }

    /// Rows to reserve at the bottom for the pinned input band: the input line plus
    /// one row per queued command (stacked above it). The input line is **always**
    /// present at the live edge now — the relocated shell prompt at a resting prompt,
    /// the type-ahead field while a command runs — so output above it always scrolls
    /// top-to-bottom. Queued commands add rows whenever the queue is non-empty,
    /// pushing the console up. The band is ruled out on the alt screen and in
    /// scrollback (laid out plainly there), and on a degenerate 1-row grid. Capped to
    /// leave at least one content row, and [`Self::MAX_QUEUE_ROWS`] queued rows
    /// before the remainder collapses into a "+N more" line.
    fn input_band_rows(&self) -> i32 {
        // Need at least one row above the band; on a degenerate 1-row grid
        // `display_shift_with` also bails, so keep the two in agreement.
        if self.dims.rows <= 1 || self.is_scrolled() || self.alt_screen() {
            return 0;
        }
        // The input line — the running type-ahead field (one row), or the relocated
        // resting prompt, which is as tall as its (soft-wrapped) logical input.
        let max_total = (self.dims.rows as i32 - 1).max(1);
        let input = if self.command_running() {
            self.band_input_height()
        } else {
            self.resting_input_height()
        };
        let input = input.clamp(1, max_total);
        let queued = self.band.queued.len() as i32;
        // Keep at least one content row; show up to MAX_QUEUE_ROWS queued rows.
        let queue_rows = queued.min(Self::MAX_QUEUE_ROWS).min(max_total - input).max(0);
        input + queue_rows
    }

    /// Grid-row span `[top, bottom]` of the resting prompt's logical input — the
    /// soft-wrapped command line the cursor sits on. `bottom` follows soft-wrap
    /// (`WRAPLINE`) down from the cursor; `top` is the open block's command line
    /// (where input begins) when shell integration provides it, else the soft-wrap
    /// predecessors of the cursor. Bounded so the band always leaves one content
    /// row. A single row when the input does not wrap — so single-line behavior is
    /// unchanged.
    fn resting_input_span(&self) -> (i32, i32) {
        let grid = self.term.grid();
        let cols = self.dims.cols as usize;
        let cursor = grid.cursor.point.line.0;
        let bot_bound = self.dims.rows as i32 - 1;
        let last = Column(cols.saturating_sub(1));

        let mut bottom = cursor;
        while cols > 0
            && bottom < bot_bound
            && grid[Line(bottom)][last].flags.contains(Flags::WRAPLINE)
        {
            bottom += 1;
        }

        let mut top = match self.blocks.last().filter(|b| b.end_line.is_none()) {
            Some(b) => self.abs_to_grid_line(b.command_line).min(cursor),
            None => {
                let mut t = cursor;
                while cols > 0 && t > 0 && grid[Line(t - 1)][last].flags.contains(Flags::WRAPLINE) {
                    t -= 1;
                }
                t
            }
        };
        // Leave at least one content row above the band.
        let max_h = bot_bound.max(1);
        top = top.max(bottom - (max_h - 1)).min(bottom);
        (top, bottom)
    }

    /// Number of rows the resting prompt's relocated input occupies (its
    /// [`Self::resting_input_span`] height), at least 1.
    fn resting_input_height(&self) -> i32 {
        let (top, bottom) = self.resting_input_span();
        (bottom - top + 1).max(1)
    }

    /// Display width of the captured prompt prefix that leads the running
    /// type-ahead field, clamped to the grid (each prefix cell is one column).
    fn band_prefix_width(&self) -> usize {
        let cols = self.dims.cols as usize;
        self.blocks.last().map(|b| b.prompt_prefix.len()).unwrap_or(0).min(cols)
    }

    /// The typed input as it is rendered into the band: the text itself, or one
    /// bullet per character when masked (no-echo). Borrows when unmasked.
    fn band_input_source(&self) -> std::borrow::Cow<'_, str> {
        if self.band.masked {
            std::borrow::Cow::Owned("\u{2022}".repeat(self.band.input.chars().count()))
        } else {
            std::borrow::Cow::Borrowed(self.band.input.as_str())
        }
    }

    /// Visual rows the running type-ahead field needs once soft-wrapped at the
    /// grid width: the prompt prefix plus the typed input. One row when it fits a
    /// single line (so single-line behavior is unchanged) or nothing is typed (the
    /// running command shows as a one-row placeholder). The running analog of
    /// [`Self::resting_input_height`].
    fn band_input_height(&self) -> i32 {
        let cols = self.dims.cols as usize;
        if cols == 0 || self.band.input.is_empty() {
            return 1;
        }
        let source = self.band_input_source();
        let (chunks, caret_wrapped) = wrap_chunks(self.band_prefix_width(), &source, cols);
        (chunks.len() + caret_wrapped as usize) as i32
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
    ///
    /// At the live edge the live input line is relocated into the pinned bottom band
    /// ([`Self::fill_input_band`]) and the output region above it is bottom-anchored
    /// (hugs the band, fills upward, then scrolls) — so the input holds still while
    /// output behaves like a conventional terminal; scrolled back, it paints the
    /// composite — frozen block buffers above the live grid's active region — via
    /// [`Self::composite_snapshot`].
    pub fn snapshot(&self) -> GridSnapshot {
        if self.scroll_top_abs.is_some() {
            return self.composite_snapshot();
        }
        let cols = self.dims.cols;
        let rows = self.dims.rows;
        let mut snap =
            GridSnapshot::filled(cols, rows, self.palette.foreground, self.palette.background);
        snap.selection_color = self.palette.selection;

        let band = self.input_band_rows();
        let shift = self.display_shift_with(band);
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;
        // At a resting prompt the live input line — its whole soft-wrapped span —
        // is relocated into the band ([`Self::fill_input_band`]); skip those rows
        // here so they aren't also drawn in the output region (which would double
        // them and leave the prompt in the scrolling area). While a command runs
        // there is no resting prompt to move.
        let relocate =
            (band > 0 && !self.command_running()).then(|| self.resting_input_span());

        for indexed in grid.display_iter() {
            if let Some((top, bottom)) = relocate {
                if (top..=bottom).contains(&indexed.point.line.0) {
                    continue;
                }
            }
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
            let abs = self.abs_base + indexed.point.line.0 as i64;
            let selected = self.is_selected(abs, col);

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

    /// Snapshot while scrolled back through history: a top-aligned window over the
    /// composite, starting at [`Self::scroll_top_abs`]. Each row is sourced from a
    /// finished block's frozen buffer when the absolute line falls inside one, and
    /// from the live grid otherwise (the active region, or raw history when there
    /// is no shell integration). No bottom-anchor, input band, or live cursor here
    /// — those belong to the live edge. Selection is flagged per cell in composite
    /// (absolute-line) space, so it spans the frozen/live boundary.
    fn composite_snapshot(&self) -> GridSnapshot {
        let cols = self.dims.cols;
        let rows = self.dims.rows;
        let mut snap =
            GridSnapshot::filled(cols, rows, self.palette.foreground, self.palette.background);
        snap.selection_color = self.palette.selection;
        snap.block_stripe = self.palette.indexed(9); // bright red
        let red = self.palette.indexed(1);
        snap.block_tint = Rgba::new(red.r, red.g, red.b, 30); // subtle wash
        snap.block_separator = self.palette.indexed(8); // ash

        let ncols = cols as usize;
        let top_abs = self.viewport_top_line();
        let cursor_abs = self.absolute_cursor_line();
        for r in 0..rows as usize {
            let abs = top_abs + r as i64;
            let dst = r * ncols;
            self.composite_row_into(abs, cursor_abs, &mut snap.cells[dst..dst + ncols]);
            for c in 0..ncols {
                if self.is_selected(abs, c) {
                    snap.cells[dst + c].flags |= CellFlags::SELECTED;
                }
            }
            if let Some(idx) = self.block_row(abs, cursor_abs) {
                let b = &self.blocks[idx];
                snap.rows_decor[r] = RowDecor {
                    block_id: b.id,
                    failed: b.state == BlockState::Failed,
                    block_top: abs == b.prompt_line,
                };
            }
        }

        // Sticky header for the top row's block, if its prompt is above the view.
        if let Some(idx) = self.block_row(top_abs, cursor_abs) {
            let b = &self.blocks[idx];
            if b.prompt_line < top_abs && !b.command.is_empty() {
                snap.sticky = Some(StickyHeader {
                    command: b.command.clone(),
                    failed: b.state == BlockState::Failed,
                });
            }
        }
        snap
    }

    /// Fill one composite row (`out.len() == cols`) for absolute line `abs`: from a
    /// finished block's frozen buffer when `abs` lands inside one, else from the
    /// live grid. Out-of-range lines stay blank.
    fn composite_row_into(&self, abs: i64, cursor_abs: i64, out: &mut [CellSnapshot]) {
        out.fill(CellSnapshot::blank(self.palette.foreground, self.palette.background));

        // Finished block → its frozen buffer (index-aligned with `blocks` by id).
        if let Some(idx) = self.block_row(abs, cursor_abs) {
            if idx < self.frozen.len() && self.frozen[idx].id == self.blocks[idx].id {
                let fb = &self.frozen[idx];
                // The buffer's last row is the block's end line; map back from there
                // so a clipped head (rows lost to history) lines up correctly.
                if let Some(end) = self.blocks[idx].end_line {
                    let k = abs - end + fb.rows() as i64 - 1;
                    if let Some(src) = usize::try_from(k).ok().and_then(|k| fb.row(k)) {
                        let n = src.len().min(out.len());
                        out[..n].copy_from_slice(&src[..n]);
                    }
                }
                return;
            }
        }

        // Otherwise the live grid: the active region, or un-integrated history.
        let g = abs - self.abs_base;
        let top = -(self.term.grid().history_size() as i64);
        let bottom = self.dims.rows.saturating_sub(1) as i64;
        if g < top || g > bottom {
            return; // outside the retained grid → blank
        }
        let grid = self.term.grid();
        let gl = Line(g as i32);
        for (c, cell) in out.iter_mut().enumerate().take(self.dims.cols as usize) {
            *cell = self.cell_to_snapshot(&grid[gl][Column(c)]);
        }
    }

    /// Paint the pinned input band and flag it on the snapshot. At the live edge
    /// the band always carries an **input line** on its bottom row, with one
    /// **queued row** per typed-ahead command stacked above it (oldest at the top),
    /// so queuing grows the band and pushes the console up.
    ///
    /// The input line's source depends on state: while a command runs it is the
    /// type-ahead field (the captured prompt prefix, then the typed text or the
    /// running command grayed as a placeholder, plus a beam caret); at a resting
    /// prompt it is the **relocated live shell prompt** — the grid's cursor line
    /// (prompt prefix + typed input) lifted out of the output region so the output
    /// can scroll top-to-bottom while the prompt holds at the bottom.
    fn fill_input_band(&self, snap: &mut GridSnapshot, band: i32) {
        if band <= 0 {
            return;
        }
        let cols = snap.cols as usize;
        let rows = snap.rows as usize;
        let band_top = rows.saturating_sub(band as usize);
        let dim = self.palette.indexed(8); // ash / bright-black
        // Reset the band rows to blanks and drop any block decoration that mapped
        // onto them, so the strip is its own region rather than the running
        // block's trailing output.
        let blank = CellSnapshot::blank(dim, self.palette.background);
        for row in band_top..rows {
            for col in 0..cols {
                snap.cells[row * cols + col] = blank;
            }
            snap.rows_decor[row] = RowDecor::default();
        }
        snap.input_band_rows = band as u16;

        // The input occupies the bottom rows of the band, as tall as its
        // soft-wrapped height (the running type-ahead field or the relocated
        // resting prompt); queued commands stack in whatever rows remain above it.
        let input_height = if self.command_running() {
            (self.band_input_height() as usize).clamp(1, band as usize)
        } else {
            (self.resting_input_height() as usize).clamp(1, band as usize)
        };
        let input_top = rows - input_height;
        let queue_rows = (band as usize).saturating_sub(input_height);
        self.fill_queued_rows(snap, band_top, queue_rows);

        if self.command_running() {
            // Running: the type-ahead field, led by the captured prompt prefix and
            // soft-wrapped down the input rows.
            self.fill_band_input(snap, input_top, input_height);
        } else {
            // Resting prompt: relocate the (soft-wrapped) input line into the band.
            self.fill_resting_input(snap, input_top, input_height);
        }
    }

    /// Relocate the live shell prompt's input into the band at a resting prompt:
    /// copy its grid rows (the soft-wrapped [`Self::resting_input_span`] — prompt
    /// prefix + typed input) verbatim into the band's bottom `height` rows and
    /// place the cursor on the matching row, so the prompt holds at the bottom
    /// while output above scrolls top-to-bottom. The same rows are skipped in the
    /// output loop ([`Self::snapshot`]) so they are not drawn twice.
    fn fill_resting_input(&self, snap: &mut GridSnapshot, input_top: usize, height: usize) {
        let cols = snap.cols as usize;
        let grid = self.term.grid();
        let (top, bottom) = self.resting_input_span();
        let h = ((bottom - top + 1).max(1) as usize).min(height);
        for i in 0..h {
            let src = Line(top + i as i32);
            let brow = input_top + i;
            let dst = &mut snap.cells[brow * cols..(brow + 1) * cols];
            for (c, cell) in dst.iter_mut().enumerate() {
                *cell = self.cell_to_snapshot(&grid[src][Column(c)]);
            }
        }
        // The cursor sits on the band row matching its grid row within the span.
        let offset = (grid.cursor.point.line.0 - top).clamp(0, h as i32 - 1) as usize;
        snap.cursor = self.band_cursor(grid.cursor.point.column.0, input_top + offset);
    }

    /// A beam caret on the band's input area at `col`/`row`: a text-insertion
    /// point, distinct from the block cursor the shell draws. Set after the
    /// output-region cursor so it takes precedence — the input lives in the band.
    fn band_beam(&self, col: usize, row: usize) -> CursorSnapshot {
        let cols = self.dims.cols as usize;
        CursorSnapshot {
            col: col.min(cols.saturating_sub(1)) as u16,
            row: row as u16,
            shape: CursorShape::Beam,
            color: self.palette.cursor,
            text_color: self.palette.cursor_text,
        }
    }

    /// A cursor on the band's input row at `col`, honoring the shell's cursor shape
    /// and the same visibility gates as [`Self::cursor_snapshot`]. `None` when the
    /// cursor is hidden — overriding any output-region cursor set earlier.
    fn band_cursor(&self, col: usize, row: usize) -> Option<CursorSnapshot> {
        if !self.seen_output
            || self.cursor_shape == CursorShape::Hidden
            || !self.term.mode().contains(TermMode::SHOW_CURSOR)
        {
            return None;
        }
        let cols = self.dims.cols as usize;
        Some(CursorSnapshot {
            col: col.min(cols.saturating_sub(1)) as u16,
            row: row as u16,
            shape: self.cursor_shape,
            color: self.palette.cursor,
            text_color: self.palette.cursor_text,
        })
    }

    /// Render the queued commands as dim, italic rows stacked above the input
    /// line, oldest (next to run) first. When more are queued than `count` rows,
    /// the bottom row collapses the remainder into a "+N more queued" line.
    fn fill_queued_rows(&self, snap: &mut GridSnapshot, top: usize, count: usize) {
        if count == 0 {
            return;
        }
        let cols = snap.cols as usize;
        let total = self.band.queued.len();
        // Dim + italic reads as pending, distinct from the bright input line.
        let mut style = CellSnapshot::blank(self.palette.indexed(8), self.palette.background);
        style.flags = CellFlags::ITALIC;
        for i in 0..count {
            let text = if i + 1 == count && total > count {
                format!("+ {} more queued", total - (count - 1))
            } else {
                format!("\u{2022} {}", self.band.queued[i]) // • bullet
            };
            let row = &mut snap.cells[(top + i) * cols..(top + i + 1) * cols];
            write_row(row, &text, style);
        }
    }

    /// Render the always-editable type-ahead field into the band's input rows
    /// [`input_top`, `input_top` + `height`): the captured prompt prefix, then the
    /// typed text in the normal foreground (one bullet per char when masked),
    /// soft-wrapped at the grid width, with a beam caret after the last char.
    /// When nothing is typed, the running command shows grayed as a placeholder on
    /// the single input row. When the wrapped field is taller than `height` (the
    /// band clamps it to leave a content row), the bottom rows are shown so the
    /// caret stays visible.
    fn fill_band_input(&self, snap: &mut GridSnapshot, input_top: usize, height: usize) {
        let cols = snap.cols as usize;
        if cols == 0 || height == 0 {
            return;
        }
        let bg = self.palette.background;
        let prefix = self.blocks.last().map(|b| b.prompt_prefix.as_slice()).unwrap_or(&[]);
        let prefix_w = prefix.len().min(cols);
        let row_range = |brow: usize| brow * cols..(brow + 1) * cols;

        if self.band.input.is_empty() {
            // Grayed placeholder: the running command, from its start, until typed
            // over. One row; the caret rests at the start of the input area.
            let placeholder = match self.blocks.last() {
                Some(b) if !b.command.is_empty() => b.command.as_str(),
                _ => "running…",
            };
            let dim_blank = CellSnapshot::blank(self.palette.indexed(8), bg);
            let row = &mut snap.cells[row_range(input_top)];
            for (cell, src) in row.iter_mut().zip(prefix) {
                *cell = *src;
            }
            write_row(&mut row[prefix_w.min(cols)..], placeholder, dim_blank);
            snap.cursor = Some(self.band_beam(prefix_w, input_top));
            return;
        }

        // Soft-wrap the typed text (bullets when masked) into visual rows; show the
        // bottom `height` of them so the caret row stays visible when clamped.
        let fg_blank = CellSnapshot::blank(self.palette.foreground, bg);
        let source = self.band_input_source();
        let (chunks, caret_wrapped) = wrap_chunks(prefix_w, &source, cols);
        let total_rows = chunks.len() + caret_wrapped as usize;
        let first_visible = total_rows.saturating_sub(height);

        for (vrow, chunk) in chunks.iter().enumerate() {
            if vrow < first_visible {
                continue;
            }
            let brow = input_top + (vrow - first_visible);
            let row = &mut snap.cells[row_range(brow)];
            if vrow == 0 {
                // The prompt prefix leads the first visual row.
                for (cell, src) in row.iter_mut().zip(prefix) {
                    *cell = *src;
                }
                write_row(&mut row[prefix_w.min(cols)..], chunk, fg_blank);
            } else {
                write_row(row, chunk, fg_blank);
            }
        }

        // The caret sits after the last glyph, or at column 0 of the spilled row
        // when the last row filled exactly.
        let (caret_vrow, caret_vcol) = if caret_wrapped {
            (chunks.len(), 0)
        } else {
            let base = if chunks.len() <= 1 { prefix_w } else { 0 };
            (chunks.len() - 1, base + UnicodeWidthStr::width(*chunks.last().unwrap_or(&"")))
        };

        // Ghost text: the un-typed completion, dimmed (the "ash" the placeholder
        // and queued rows use), from the caret to the end of its row — clipped, so
        // the suggestion never grows the band.
        if let (Some(suffix), true) = (self.band.suggestion.as_deref(), caret_vrow >= first_visible) {
            let brow = input_top + (caret_vrow - first_visible);
            let ghost = CellSnapshot::blank(self.palette.indexed(8), bg);
            let row = &mut snap.cells[row_range(brow)];
            write_row(&mut row[caret_vcol.min(cols)..], suffix, ghost);
        }

        let caret_row = input_top + caret_vrow.saturating_sub(first_visible);
        snap.cursor = Some(self.band_beam(caret_vcol, caret_row));
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

/// Split `text` at the longest leading slice whose display width fits in `max`
/// columns (wide glyphs kept whole, never split): the head to place on the current
/// row and the rest to carry to the next. The head-anchored unit of a soft wrap.
fn head_fit(text: &str, max: usize) -> (&str, &str) {
    let mut width = 0;
    for (i, ch) in text.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max {
            return (&text[..i], &text[i..]);
        }
        width += w;
    }
    (text, "")
}

/// Soft-wrap `text` across the band's input rows: the first row begins at column
/// `prefix_w` (just past the prompt prefix), the rest span the full `cols`. Returns
/// the per-row slices and whether the caret spills onto a fresh trailing row (the
/// last row filled exactly, so the insertion point wraps). Used for both the
/// reserved height and the rendering, so the two never disagree.
fn wrap_chunks(prefix_w: usize, text: &str, cols: usize) -> (Vec<&str>, bool) {
    let first_avail = cols.saturating_sub(prefix_w);
    let (head, mut rest) = head_fit(text, first_avail);
    let mut chunks = vec![head];
    while !rest.is_empty() {
        let (h, r) = head_fit(rest, cols);
        if h.is_empty() {
            break; // a glyph wider than the whole row — cannot be placed
        }
        chunks.push(h);
        rest = r;
    }
    let last = *chunks.last().unwrap_or(&"");
    let last_avail = if chunks.len() <= 1 { first_avail } else { cols };
    let caret_wrapped = !last.is_empty() && UnicodeWidthStr::width(last) == last_avail;
    (chunks, caret_wrapped)
}

/// Drop trailing blank cells from a frozen logical line — soft-wrap padding is
/// not part of the unwrapped content the line stands for.
fn trim_trailing_blanks(line: &mut Vec<CellSnapshot>) {
    while line.last().is_some_and(CellSnapshot::is_blank) {
        line.pop();
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

/// One endpoint of a composite selection: an absolute line, a column, and which
/// half of that cell the pointer fell on (so the boundary cell is included or not).
#[derive(Clone, Copy)]
struct SelPoint {
    abs: i64,
    col: usize,
    side: Side,
}

/// A linear text selection over the composite (absolute-line) coordinate space,
/// so it spans frozen block buffers and the live grid alike — unlike alacritty's
/// `Selection`, which only sees the single live grid.
#[derive(Clone, Copy)]
struct CompositeSelection {
    /// Where the drag started (its `side` is fixed for the drag).
    anchor: SelPoint,
    /// The current drag end (updated as the pointer moves).
    head: SelPoint,
}

impl CompositeSelection {
    /// The selection's inclusive `(abs, col)` start and end in reading order, with
    /// the cell-half folded in: a `Right`-half start excludes its cell (begins at
    /// the next column, wrapping a line), a `Left`-half end excludes its cell.
    /// Returns `None` when the result is empty (e.g. a click with no drag).
    fn bounds(&self, cols: usize) -> Option<((i64, usize), (i64, usize))> {
        let (s, e) = if (self.anchor.abs, self.anchor.col) <= (self.head.abs, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        let start = match s.side {
            Side::Right if s.col + 1 >= cols => (s.abs + 1, 0),
            Side::Right => (s.abs, s.col + 1),
            Side::Left => (s.abs, s.col),
        };
        let end = match e.side {
            Side::Left if e.col == 0 => (e.abs - 1, cols.saturating_sub(1)),
            Side::Left => (e.abs, e.col - 1),
            Side::Right => (e.abs, e.col),
        };
        (start <= end).then_some((start, end))
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
    fn scrolling_spans_frozen_blocks_and_live_grid() {
        let mut t = terminal(20, 4);
        // Two finished blocks (frozen) with distinctive output, then a running one
        // that keeps a live/active region at the bottom.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07first\r\n\x1b]133;C\x07AAA\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07second\r\n\x1b]133;C\x07BBB\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07third\r\n\x1b]133;C\x07CCC\r\n");
        assert_eq!(t.frozen_blocks().len(), 2, "two finished blocks frozen");

        // At the live edge the oldest output has scrolled above the 4-row viewport.
        let edge: String = t.snapshot().cells.iter().map(|c| c.c).collect();
        assert!(!edge.contains("AAA"), "first block is above the live edge: {edge:?}");

        // Scrolled to the top, the composite sources the first block from its
        // frozen buffer — spanning frozen blocks and (further down) the live grid.
        t.scroll_to_top();
        assert!(t.is_scrolled(), "parked in history");
        let hist: String = t.snapshot().cells.iter().map(|c| c.c).collect();
        assert!(hist.contains("AAA"), "scrolled view shows the first frozen block: {hist:?}");

        // Back at the live edge the running command's output is visible again.
        t.scroll_to_bottom();
        assert!(!t.is_scrolled());
        let back: String = t.snapshot().cells.iter().map(|c| c.c).collect();
        assert!(back.contains("CCC"), "live edge shows the running output: {back:?}");
    }

    #[test]
    fn composite_scroll_clamps_at_both_ends() {
        let mut t = terminal(10, 3);
        for i in 0..20 {
            t.process(format!("l{i}\r\n").as_bytes());
        }
        assert!(!t.is_scrolled());
        t.scroll_lines(-5); // already at the edge — cannot scroll further down
        assert!(!t.is_scrolled(), "scrolling down at the live edge is a no-op");
        t.scroll_to_top();
        assert!(t.is_scrolled(), "scrolled to the oldest line");
        let oldest_top = t.viewport_top_line();
        t.scroll_lines(100); // cannot move past the oldest line
        assert_eq!(t.viewport_top_line(), oldest_top, "clamped at the oldest line");
        t.scroll_to_bottom();
        assert!(!t.is_scrolled(), "returned to the live edge");
    }

    #[test]
    fn selection_yields_text() {
        let mut t = terminal(20, 3);
        // The live input line is relocated into the band, so select committed
        // output: "hello world" is bottom-anchored on row 1 (just above the band),
        // the prompt rests in the band on row 2.
        t.process(b"hello world\r\n$ ");
        t.selection_start(0, 1, false);
        t.selection_update(4, 1, true); // through the right half of column 4
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn snapshot_flags_selected_cells() {
        let mut t = terminal(20, 3);
        t.process(b"hello\r\n$ ");
        t.selection_start(0, 1, false);
        t.selection_update(4, 1, true);
        let snap = t.snapshot();
        assert!(snap.cell(0, 1).unwrap().flags.contains(CellFlags::SELECTED));
        assert!(snap.cell(4, 1).unwrap().flags.contains(CellFlags::SELECTED));
        assert!(!snap.cell(6, 1).unwrap().flags.contains(CellFlags::SELECTED));
        assert_eq!(snap.selection_color, Palette::default().selection);
    }

    #[test]
    fn selection_spans_the_frozen_live_boundary() {
        let mut t = terminal(20, 8);
        // A finished (frozen) block above a still-running (live) one.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07first\r\n\x1b]133;C\x07AAA\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07second\r\n\x1b]133;C\x07CCC\r\n");
        assert_eq!(t.frozen_blocks().len(), 1, "first frozen, second still running");

        // Locate the rendered rows by content (robust to bottom-anchor geometry).
        let snap = t.snapshot();
        let find = |needle: &str| (0..snap.rows).find(|&r| row_text(&snap, r).contains(needle));
        let a_row = find("AAA").expect("frozen output visible");
        let c_row = find("CCC").expect("live output visible");
        assert!(a_row < c_row, "frozen block sits above the live one");

        // Drag from the frozen output down into the live output.
        t.selection_start(0, a_row, false);
        t.selection_update(2, c_row, true);
        let text = t.selection_text().expect("a cross-boundary selection has text");
        assert!(text.contains("AAA"), "includes frozen-block output (read from the buffer): {text:?}");
        assert!(text.contains("CCC"), "includes live-grid output: {text:?}");
        assert!(
            text.find("AAA") < text.find("CCC"),
            "frozen text precedes live text in reading order: {text:?}"
        );
    }

    #[test]
    fn composite_snapshot_flags_selection_when_scrolled() {
        let mut t = terminal(20, 4);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07first\r\n\x1b]133;C\x07AAA\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07second\r\n\x1b]133;C\x07BBB\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07third\r\n\x1b]133;C\x07CCC\r\n");
        t.scroll_to_top();
        assert!(t.is_scrolled(), "parked in history");

        // Select the oldest frozen block's output row, found by content.
        let snap0 = t.snapshot();
        let a_row =
            (0..snap0.rows).find(|&r| row_text(&snap0, r).contains("AAA")).expect("AAA at top");
        t.selection_start(0, a_row, false);
        t.selection_update(2, a_row, true);

        // The composite (scrolled) snapshot now flags the selected frozen cells.
        let snap = t.snapshot();
        assert!(
            snap.cell(0, a_row).unwrap().flags.contains(CellFlags::SELECTED),
            "frozen cell flagged selected in the composite snapshot"
        );
        assert!(!snap.cell(5, a_row).unwrap().flags.contains(CellFlags::SELECTED), "past the selection");
        assert!(t.selection_text().is_some_and(|s| s.contains("AAA")), "frozen selection yields text");
    }

    #[test]
    fn clearing_selection_drops_text() {
        let mut t = terminal(20, 3);
        t.process(b"hello\r\n$ ");
        t.selection_start(0, 1, false);
        t.selection_update(4, 1, true);
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
        // The live prompt is relocated into the pinned band on the bottom row; the
        // output region above it is empty.
        assert_eq!(snap.input_band_rows, 1, "the prompt is the band's input line");
        assert_eq!(cur.row, 5, "the relocated prompt rests on the bottom row");
        assert_eq!(snap.cell(0, 5).unwrap().c, '$');
        assert!(snap.cell(0, 0).unwrap().is_blank(), "the output region above is empty");
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
            "clear leaves the prompt relocated into the band on the bottom row"
        );
        assert_eq!(snap.cell(0, 5).unwrap().c, '$');
    }

    #[test]
    fn output_hugs_the_band_with_no_gap() {
        // The output region is bottom-anchored just above the pinned input band:
        // recent output hugs the band rather than floating at the top with a gap.
        let mut t = terminal(20, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\r\n\x1b]133;C\x07hi\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        let snap = t.snapshot();
        // The resting prompt is pinned on the bottom row (the band)...
        assert_eq!(snap.cursor.expect("cursor visible").row, 5);
        assert_eq!(snap.cell(0, 5).unwrap().c, '$');
        // ...and the newest output ("hi") hugs the row just above it — no gap.
        assert_eq!(snap.cell(0, 4).unwrap().c, 'h', "output hugs the band, not the top");
    }

    #[test]
    fn input_band_holds_still_as_output_grows() {
        // The defining #7 property: the input line holds still while output fills
        // and scrolls beneath it like a conventional terminal.
        let mut t = terminal(20, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07");
        let band_top = |t: &Terminal| {
            let s = t.snapshot();
            s.rows - s.input_band_rows
        };
        let pinned = band_top(&t);
        for line in [b"o1\r\n".as_slice(), b"o2\r\n", b"o3\r\n", b"o4\r\n"] {
            t.process(line);
            assert_eq!(band_top(&t), pinned, "the input band stays pinned while output grows");
        }
    }

    #[test]
    fn full_screen_output_is_not_shifted() {
        // Output fills top-to-bottom like a normal terminal: the first line sits on
        // the top row. The cursor line ('c') is the resting input, relocated into
        // the band on the bottom row.
        let mut t = terminal(20, 3);
        t.process(b"a\r\nb\r\nc");
        let snap = t.snapshot();
        assert_eq!(snap.cell(0, 0).unwrap().c, 'a');
        assert_eq!(snap.cell(0, 1).unwrap().c, 'b');
        assert_eq!(snap.cell(0, 2).unwrap().c, 'c', "the input line 'c' is in the band");
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
        // A blank row with a non-default background is visible content: content_bottom
        // counts it so a full screen keeps it just above the band rather than
        // clipping it off the bottom.
        let mut t = terminal(20, 4);
        // A running command whose newest output line is five blue (index 4) spaces,
        // on a grid that is full once the band is reserved.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07run\r\n\x1b]133;C\x07a\r\nb\r\n\x1b[44m     \x1b[0m");
        let snap = t.snapshot();
        assert!(t.command_running());
        let blue = Palette::default().indexed(4);
        let last_out = snap.rows - 2; // the row just above the band
        assert_eq!(snap.cell(0, last_out).unwrap().bg, blue, "the colored blank row is kept above the band");
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
        // Output is bottom-anchored, so the block's prompt row is the first
        // non-padding row; the blank rows above it carry no decoration, and the
        // finished command's cursor line is relocated into the band (no decoration).
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
        assert_eq!(snap.rows_decor[0].block_id, 0, "rows above the block are padding");
        let band = snap.rows as usize - 1;
        assert_eq!(snap.rows_decor[band].block_id, 0, "the band row carries no block decoration");
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
    fn resting_prompt_relocates_into_the_band() {
        let mut t = terminal(20, 6);
        // Command finished and a fresh prompt drawn: the prompt is relocated into
        // the pinned band on the bottom row, with its output above it.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07x\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 1, "the resting prompt is the band's input line");
        let cur = snap.cursor.expect("cursor visible");
        assert_eq!(cur.row, 5, "the prompt rests on the bottom row");
        assert_eq!(snap.cell(0, 5).unwrap().c, '$');
        // The finished command's prompt line ("$ x") stays in the output region,
        // bottom-anchored just above the band on row 4.
        assert_eq!(snap.cell(0, 4).unwrap().c, '$');
        assert_eq!(snap.cell(2, 4).unwrap().c, 'x');
    }

    #[test]
    fn wrapped_resting_input_relocates_whole_into_the_band() {
        let mut t = terminal(10, 6);
        // A typed command longer than the width soft-wraps across two rows at a
        // resting prompt (OSC-133 prompt + command, no OutputStart — still editing).
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07abcdefghijklmno");
        assert!(!t.command_running(), "still editing at the prompt");

        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 2, "band is as tall as the wrapped input");
        let rows = snap.rows;
        let band0 = row_text(&snap, rows - 2);
        let band1 = row_text(&snap, rows - 1);
        assert!(band0.contains("abcdefgh"), "first input row in the band: {band0:?}");
        assert!(band1.contains("ijklmno"), "continuation row in the band: {band1:?}");
        // The wrapped input is *relocated*, not duplicated into the scrolling output.
        for r in 0..rows - 2 {
            let line = row_text(&snap, r);
            assert!(!line.contains("abcdefgh"), "row {r} must not duplicate the input: {line:?}");
        }
        // The caret rides the continuation row where the typing ended.
        let cur = snap.cursor.expect("cursor visible at the prompt");
        assert_eq!(cur.row, rows - 1, "cursor on the last input row");
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
    fn command_finished_freezes_the_block() {
        let mut t = terminal(20, 6);
        assert!(t.frozen_blocks().is_empty(), "nothing frozen before a command finishes");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07echo\r\n\x1b]133;C\x07hi there\r\n\x1b]133;D;0\x07");
        let frozen = t.frozen_blocks();
        assert_eq!(frozen.len(), 1, "the finished block is frozen exactly once");
        let fb = &frozen[0];
        assert_eq!(fb.command, "echo", "command text carried over");
        assert!(!fb.failed, "exit 0 is success");
        assert_eq!(fb.cols, 20, "captured at the grid width");
        assert!(fb.rows() >= 2, "prompt and output rows captured");
        let text: String = fb.cells.iter().map(|c| c.c).collect();
        assert!(text.contains("hi there"), "frozen cells include the output: {text:?}");
        let has_output_line = fb
            .logical_lines
            .iter()
            .any(|l| l.iter().map(|c| c.c).collect::<String>().contains("hi there"));
        assert!(has_output_line, "a logical line holds the unwrapped output");
    }

    #[test]
    fn frozen_blocks_list_in_order_with_metadata() {
        let mut t = terminal(20, 8);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07one\r\n\x1b]133;C\x07a\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07two\r\n\x1b]133;C\x07b\r\n\x1b]133;D;1\x07");
        let f = t.frozen_blocks();
        assert_eq!(f.len(), 2, "one frozen buffer per finished block");
        assert_eq!(f[0].command, "one");
        assert!(!f[0].failed, "exit 0 is success");
        assert_eq!(f[1].command, "two");
        assert!(f[1].failed, "exit 1 is failed");
        assert!(f[0].id < f[1].id, "oldest first, by ascending block id");
    }

    #[test]
    fn resize_reflows_frozen_blocks() {
        let mut t = terminal(20, 6);
        // One finished block whose 12-char output fits on a single 20-wide row.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07echo\r\n\x1b]133;C\x07hello world!\r\n\x1b]133;D;0\x07");
        assert_eq!(t.frozen_blocks().len(), 1);
        assert_eq!(t.frozen_blocks()[0].cols, 20);

        // Narrow to 8 columns: the block survives, re-wrapped to the new width.
        t.resize(8, 6);
        let f = t.frozen_blocks();
        assert_eq!(f.len(), 1, "frozen block survives the resize");
        assert_eq!(f[0].cols, 8, "re-wrapped to the new width");
        // The unwrapped text is the untouched source of truth across the reflow.
        let logical: String = f[0]
            .logical_lines
            .iter()
            .map(|l| l.iter().map(|c| c.c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(logical.contains("hello world!"), "output text preserved: {logical:?}");
        // The 12-char line now spans more than one 8-wide visual row.
        assert!(f[0].rows() >= 2, "the long line re-wrapped onto multiple rows");
    }

    #[test]
    fn frozen_history_survives_resize_at_a_resting_prompt() {
        let mut t = terminal(20, 4);
        // Two finished blocks with distinctive output, then a freshly drawn prompt
        // we sit at (resting — no running command).
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07first\r\n\x1b]133;C\x07AAA\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07second\r\n\x1b]133;C\x07BBB\r\n\x1b]133;D;0\x07");
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(!t.command_running(), "resting at a prompt");
        assert_eq!(t.frozen_blocks().len(), 2);

        // Resize narrower: the block history survives and re-wraps to the new width.
        t.resize(10, 4);
        assert_eq!(t.frozen_blocks().len(), 2, "history survives the resize");
        assert!(t.frozen_blocks().iter().all(|f| f.cols == 10), "each block re-wrapped");

        // Scrolled to the top, the oldest block's output still renders — sourced
        // from its reflowed frozen buffer, not the (renumbered) live grid.
        t.scroll_to_top();
        assert!(t.is_scrolled(), "parked in history after the resize");
        let hist: String = t.snapshot().cells.iter().map(|c| c.c).collect();
        assert!(hist.contains("AAA"), "oldest block survives resize + reflow: {hist:?}");
    }

    #[test]
    fn resize_with_no_blocks_clears_cleanly() {
        // No shell integration: the live grid alone backs scrollback, and resize
        // must not leave a stale frozen list behind.
        let mut t = terminal(20, 4);
        t.process(b"plain output\r\nno markers here\r\n");
        assert!(t.blocks().is_empty());
        t.resize(10, 6);
        assert!(t.blocks().is_empty());
        assert!(t.frozen_blocks().is_empty());
    }

    #[test]
    fn clearing_history_drops_frozen_blocks_in_lockstep() {
        let mut t = terminal(20, 4);
        // Build up some history so a reset actually shrinks it.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07a\r\n\x1b]133;C\x07l1\r\nl2\r\nl3\r\nl4\r\nl5\r\n\x1b]133;D;0\x07");
        assert_eq!(t.frozen_blocks().len(), 1);
        assert!(!t.blocks().is_empty());
        // A full reset (RIS) clears the screen and scrollback: history shrinks, so
        // the whole block model is wiped — the frozen list must go with it.
        t.process(b"\x1bc");
        assert!(t.blocks().is_empty(), "blocks cleared when history shrinks");
        assert!(t.frozen_blocks().is_empty(), "frozen cleared in lockstep");
    }

    /// Band state with the given input text and queued commands (echo on, no
    /// ghost-text suggestion).
    fn band(input: &str, queued: &[&str]) -> BandState {
        BandState {
            input: input.to_owned(),
            queued: queued.iter().map(|s| (*s).to_owned()).collect(),
            masked: false,
            suggestion: None,
        }
    }

    #[test]
    fn band_input_keeps_the_prompt_prefix_before_the_typed_text() {
        let mut t = terminal(40, 6);
        // A running command so the prompt prefix ("$ ") is captured.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        t.set_band(band("echo hi", &[]));
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 1, "just the input line when nothing is queued");
        let last = snap.rows - 1;
        let line = row_text(&snap, last);
        // Reads as an anchored prompt: prefix kept, typed text after it.
        assert!(line.starts_with("$ "), "the prompt prefix leads the band: {line:?}");
        assert!(line.contains("echo hi"), "the typed text follows it: {line:?}");
        let cur = snap.cursor.expect("a caret in the band");
        assert_eq!(cur.row, last, "the caret sits on the input row");
        // Caret after the prompt ("$ " = 2 cols) plus the typed text ("echo hi" = 7).
        assert_eq!(cur.col, 9, "the caret rests past the prompt and the typed text");
        assert_eq!(cur.shape, CursorShape::Beam, "a beam reads as a text-insertion point");
    }

    #[test]
    fn masked_band_hides_typed_input_behind_bullets() {
        let mut t = terminal(40, 6);
        // A running command (e.g. `sudo`) that turned echo off to read a secret.
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sudo true\r\n\x1b]133;C\x07");
        t.set_band(BandState {
            input: "hunter2".to_owned(),
            queued: Vec::new(),
            masked: true,
            suggestion: None,
        });
        let snap = t.snapshot();
        let last = snap.rows - 1;
        let line = row_text(&snap, last);
        assert!(line.starts_with("$ "), "the prompt prefix is still shown: {line:?}");
        assert!(!line.contains("hunter2"), "the secret never reaches the screen: {line:?}");
        // One bullet per typed character, and nothing but bullets after the prefix.
        let bullets = line.chars().filter(|&c| c == '\u{2022}').count();
        assert_eq!(bullets, "hunter2".chars().count(), "one bullet per typed char");
        // The beam caret still trails the masked text (prompt "$ " = 2 cols + 7).
        let cur = snap.cursor.expect("a caret in the band");
        assert_eq!(cur.row, last);
        assert_eq!(cur.col, 9, "the caret rests past the prompt and the masked text");
    }

    #[test]
    fn ghost_text_renders_the_suggestion_dimmed_after_the_input() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        // Typed "ec" with the rest of "echo hi" offered as ghost text.
        t.set_band(BandState {
            input: "ec".to_owned(),
            queued: Vec::new(),
            masked: false,
            suggestion: Some("ho hi".to_owned()),
        });
        let snap = t.snapshot();
        let last = snap.rows - 1;
        // Typed prefix and dimmed suffix read as one command on the input row.
        assert!(row_text(&snap, last).contains("echo hi"), "prefix + ghost read as one line");
        let pal = Palette::default();
        // "$ " (2 cols) + "ec" (2 cols): typed text in normal fg, ghost starts at col 4.
        assert_eq!(snap.cell(2, last).unwrap().c, 'e');
        assert_eq!(snap.cell(2, last).unwrap().fg, pal.foreground, "typed text in normal fg");
        assert_eq!(snap.cell(4, last).unwrap().c, 'h', "the ghost suffix follows the typed text");
        assert_eq!(snap.cell(4, last).unwrap().fg, pal.indexed(8), "the ghost suffix is dimmed");
    }

    #[test]
    fn no_suggestion_leaves_the_band_clear_after_the_caret() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        t.set_band(band("ec", &[])); // the helper supplies suggestion: None
        let snap = t.snapshot();
        let last = snap.rows - 1;
        // Caret at "$ " (2) + "ec" (2) = col 4; with nothing to suggest it stays blank.
        assert!(snap.cell(4, last).unwrap().is_blank(), "no ghost text painted past the caret");
    }

    #[test]
    fn empty_band_shows_the_running_command_grayed_with_the_caret_at_the_start() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        // Nothing typed yet: the band shows the running command as a placeholder.
        let snap = t.snapshot();
        let last = snap.rows - 1;
        let pal = Palette::default();
        assert_eq!(snap.cell(0, last).unwrap().c, '$');
        assert_eq!(snap.cell(0, last).unwrap().fg, pal.foreground, "prompt keeps its normal style");
        assert_eq!(snap.cell(2, last).unwrap().c, 's', "the running command is the placeholder");
        assert_eq!(snap.cell(2, last).unwrap().fg, pal.indexed(8), "the placeholder is grayed");
        // The band is the live input: a caret sits at the start of the field.
        let cur = snap.cursor.expect("a caret in the band");
        assert_eq!(cur.row, last);
        assert_eq!(cur.col, 2, "the caret rests at the start of the input area, after the prompt");
        assert_eq!(cur.shape, CursorShape::Beam);
    }

    #[test]
    fn typing_replaces_the_grayed_placeholder() {
        let mut t = terminal(40, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        t.set_band(band("next", &[]));
        let line = row_text(&t.snapshot(), 5);
        assert!(line.starts_with("$ "), "the prompt prefix is still there: {line:?}");
        assert!(line.contains("next"), "the typed text replaces the placeholder: {line:?}");
        assert!(!line.contains("sleep"), "the running-command placeholder is gone: {line:?}");
    }

    #[test]
    fn queued_commands_stack_above_the_input_line_and_grow_the_band() {
        let mut t = terminal(40, 8);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        t.set_band(band("", &["one", "two", "three"]));
        let snap = t.snapshot();
        // One input line + one row per queued command: the band grew, pushing the
        // console up.
        assert_eq!(snap.input_band_rows, 4, "input line plus three queued rows");
        let rows = snap.rows;
        // Queued rows sit above the input line, oldest (next to run) at the top.
        assert!(row_text(&snap, rows - 4).contains("one"), "next-to-run queued at the top");
        assert!(row_text(&snap, rows - 3).contains("two"));
        assert!(row_text(&snap, rows - 2).contains("three"), "newest just above the input line");
        assert!(row_text(&snap, rows - 1).starts_with("$ "), "the input line is at the bottom");
    }

    #[test]
    fn a_long_queue_collapses_into_a_more_row() {
        let mut t = terminal(40, 14);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07work\r\n");
        let many: Vec<&str> = "abcdefghij".split("").filter(|s| !s.is_empty()).collect(); // 10 items
        t.set_band(band("", &many));
        let snap = t.snapshot();
        // Capped at MAX_QUEUE_ROWS queued rows (6) plus the input line.
        assert_eq!(snap.input_band_rows, 7, "queue rows are capped");
        let collapsed = (0..snap.rows).any(|r| row_text(&snap, r).contains("more queued"));
        assert!(collapsed, "the overflow collapses into a \"+N more queued\" row");
    }

    #[test]
    fn band_yields_to_scrollback() {
        let mut t = terminal(20, 3);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07go\r\n\x1b]133;C\x07");
        for i in 0..10 {
            t.process(format!("l{i}\r\n").as_bytes());
        }
        t.set_band(band("x", &[]));
        t.scroll_lines(3);
        // Scrolled history lays out top-to-bottom like a normal terminal; the band
        // (and its forced reserve) yields, matching the composite snapshot path.
        assert_eq!(t.snapshot().input_band_rows, 0, "no band in scrolled history");
    }

    #[test]
    fn wrapped_running_type_ahead_spans_band_rows() {
        // Type-ahead wider than the grid soft-wraps onto extra band rows instead of
        // truncating — the whole line stays visible, the caret on the last row. The
        // running analog of `wrapped_resting_input_relocates_whole_into_the_band`.
        let mut t = terminal(8, 6);
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07go\r\n\x1b]133;C\x07");
        t.set_band(band("abcdefghijkl", &[]));
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 2, "the band grew to fit the wrapped input");
        let rows = snap.rows;
        // "$ " (2 cols) + the first 6 chars on the first row; the rest continue below.
        let r0 = row_text(&snap, rows - 2);
        let r1 = row_text(&snap, rows - 1);
        assert!(r0.starts_with("$ "), "the prompt prefix leads the first row: {r0:?}");
        assert!(r0.contains("abcdef"), "first wrapped row: {r0:?}");
        assert!(r1.contains("ghijkl"), "continuation row: {r1:?}");
        let cur = snap.cursor.expect("a caret in the band");
        assert_eq!(cur.row, rows - 1, "the caret sits on the continuation row");
        assert!(cur.col < snap.cols, "the caret stays within the band");
    }

    #[test]
    fn wrapped_type_ahead_clamps_to_the_band_keeping_the_caret_visible() {
        // On a short grid the band can't grow without starving the output, so the
        // wrapped field is clamped and the bottom (caret) row is shown.
        let mut t = terminal(8, 2); // one content row above the band
        t.process(b"\x1b]133;A\x07$ \x1b]133;B\x07go\r\n\x1b]133;C\x07");
        t.set_band(band("abcdefghijkl", &[]));
        let snap = t.snapshot();
        assert_eq!(snap.input_band_rows, 1, "clamped to one band row, leaving a content row");
        let last = snap.rows - 1;
        let line = row_text(&snap, last);
        assert!(line.contains("ghijkl"), "the tail (caret end) stays visible: {line:?}");
        let cur = snap.cursor.expect("a caret in the band");
        assert_eq!(cur.row, last);
        assert!(cur.col < snap.cols, "the caret stays within the band");
    }
}
