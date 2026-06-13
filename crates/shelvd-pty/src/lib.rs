//! `shelvd-pty` — pseudo-terminal lifecycle.
//!
//! Spawns a shell behind a PTY ([`portable_pty`], so unix PTY and Windows
//! ConPTY are handled uniformly), pumps its output to a [`flume`] channel from a
//! background reader thread, and accepts input writes and resizes.
//!
//! The reader thread calls a caller-supplied `notify` closure whenever new
//! output (or the child's exit) lands, so an event-loop owner can wake itself
//! without polling. `shelvd-pty` stays free of any windowing dependency.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread::JoinHandle;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty};

pub use portable_pty::PtySize;

/// Errors from spawning or driving the PTY.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("pty backend error: {0}")]
    Backend(#[from] anyhow::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// How to launch the child process.
#[derive(Clone, Debug)]
pub struct PtyOptions {
    /// Program to run. `None` uses the platform default shell
    /// (`$SHELL` on unix, `%ComSpec%` on Windows).
    pub shell: Option<String>,
    /// Extra arguments passed to the shell.
    pub args: Vec<String>,
    /// Working directory for the child.
    pub cwd: Option<PathBuf>,
    /// Additional environment variables (applied after defaults, so they win).
    pub env: Vec<(String, String)>,
    /// Initial PTY size.
    pub size: PtySize,
}

impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            shell: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            size: PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        }
    }
}

/// A message from the PTY reader thread.
#[derive(Debug)]
pub enum PtyMsg {
    /// A chunk of raw bytes read from the child.
    Output(Vec<u8>),
    /// The child closed the PTY (EOF) or the reader stopped.
    Exit,
}

/// A live pseudo-terminal with its child process.
///
/// Dropping the `Pty` kills the child.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: flume::Receiver<PtyMsg>,
    child: Box<dyn Child + Send + Sync>,
    reader: Option<JoinHandle<()>>,
}

impl Pty {
    /// Spawn the child behind a fresh PTY. `notify` is invoked from the reader
    /// thread whenever a [`PtyMsg`] becomes available on the receiver.
    pub fn spawn(
        opts: PtyOptions,
        notify: impl Fn() + Send + 'static,
    ) -> Result<Self, PtyError> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(opts.size)?;

        let mut cmd = match &opts.shell {
            Some(shell) => CommandBuilder::new(shell),
            None => CommandBuilder::new_default_prog(),
        };
        for arg in &opts.args {
            cmd.arg(arg);
        }
        if let Some(cwd) = &opts.cwd {
            cmd.cwd(cwd.as_os_str());
        }
        // Sensible defaults first; user-provided env overrides them.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "shelvd");
        for (key, value) in &opts.env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd)?;
        // The slave handle is not needed after spawning; dropping it lets the
        // master see EOF once the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = flume::unbounded();
        let reader = std::thread::Builder::new()
            .name("shelvd-pty-reader".to_owned())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(PtyMsg::Output(buf[..n].to_vec())).is_err() {
                                return; // receiver dropped; stop quietly
                            }
                            notify();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            log::debug!("pty reader stopped: {e}");
                            break;
                        }
                    }
                }
                let _ = tx.send(PtyMsg::Exit);
                notify();
            })?;

        Ok(Self { master: pair.master, writer, rx, child, reader: Some(reader) })
    }

    /// Non-blocking receiver of reader-thread messages.
    pub fn receiver(&self) -> &flume::Receiver<PtyMsg> {
        &self.rx
    }

    /// Write bytes to the child's input.
    pub fn write(&mut self, data: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY to a new cell/pixel size.
    pub fn resize(&mut self, size: PtySize) -> Result<(), PtyError> {
        self.master.resize(size)?;
        Ok(())
    }

    /// Whether the child has already exited.
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // The reader thread ends on its own once the PTY closes; don't join to
        // avoid blocking shutdown if it is mid-read.
        self.reader.take();
    }
}
