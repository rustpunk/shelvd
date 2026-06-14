//! The command-block model: one [`Block`] per shell command, delimited by the
//! OSC-133 semantic-prompt markers the tee recovers.
//!
//! A block accretes through a command's lifecycle: `A` opens it (prompt start),
//! `B` marks where the typed command begins, `C` marks where its output begins
//! (and is when the command text is captured), and `D` closes it with the exit
//! code. Line fields are **absolute** grid lines (see `abs_base` in `lib.rs`),
//! so they stay put as content scrolls into history.

use std::time::Instant;

/// Where a block is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockState {
    /// Command submitted; still running (or finished without an exit code).
    Running,
    /// Finished with exit code 0.
    Success,
    /// Finished with a non-zero exit code.
    Failed,
}

/// One command and its output, grouped as a navigable, decoratable unit.
#[derive(Clone, Debug)]
pub struct Block {
    /// Stable identifier (never 0; 0 means "no block" in row metadata).
    pub id: u32,
    /// Absolute line of the prompt start (OSC 133;A).
    pub prompt_line: i64,
    /// Absolute line where command input begins (OSC 133;B).
    pub command_line: i64,
    /// Column on `command_line` where command input begins.
    pub command_col: usize,
    /// Absolute line where output begins (OSC 133;C), once known.
    pub output_line: Option<i64>,
    /// Absolute line the command finished on (OSC 133;D), once known.
    pub end_line: Option<i64>,
    /// The command text, captured from `B`..`C`.
    pub command: String,
    /// The full prompt line as rendered — the prompt prefix plus the typed
    /// command (the `command_line` from column 0). Drives the running-command
    /// band so it mirrors the executed line as it appears in the block above.
    pub prompt_command: String,
    /// The command's exit code, if reported.
    pub exit_code: Option<i32>,
    /// Lifecycle state, derived from the exit code.
    pub state: BlockState,
    /// Working directory the command ran in (OSC 7), best-effort.
    pub cwd: Option<String>,
    /// When the command was submitted (at `C`).
    pub started_at: Option<Instant>,
    /// A trailing slice of the command's output, for later suggested actions.
    pub output_excerpt: String,
}

impl Block {
    /// Open a fresh block at a prompt-start marker.
    pub fn new(id: u32, prompt_line: i64, cwd: Option<String>) -> Self {
        Self {
            id,
            prompt_line,
            command_line: prompt_line,
            command_col: 0,
            output_line: None,
            end_line: None,
            command: String::new(),
            prompt_command: String::new(),
            exit_code: None,
            state: BlockState::Running,
            cwd,
            started_at: None,
            output_excerpt: String::new(),
        }
    }
}
