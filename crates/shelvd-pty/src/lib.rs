//! `shelvd-pty` — pseudo-terminal lifecycle.
//!
//! Spawns a shell behind a PTY ([`portable_pty`], so unix PTY and Windows
//! ConPTY are handled uniformly), pumps its output to a [`flume`] channel from a
//! background reader thread, and accepts input writes and resizes.
//!
//! The reader thread calls a caller-supplied `notify` closure whenever new
//! output (or the child's exit) lands, so an event-loop owner can wake itself
//! without polling. `shelvd-pty` stays free of any windowing dependency.
//!
//! When it recognizes the shell, it also **auto-injects** shelvd's
//! shell-integration script (the embedded `assets/shell-integration/` files), so
//! OSC-133 command blocks work with no dotfile edit: shelvd launches the shell
//! with a generated init that sources the user's normal config and then the
//! integration. Injection is best-effort — if anything fails, a plain shell is
//! launched instead.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty};

pub use portable_pty::PtySize;

/// Shell-integration scripts, embedded so the installed binary is self-contained.
const BASH_INTEGRATION: &str = include_str!("../../../assets/shell-integration/shelvd.bash");
const ZSH_INTEGRATION: &str = include_str!("../../../assets/shell-integration/shelvd.zsh");
const FISH_INTEGRATION: &str = include_str!("../../../assets/shell-integration/shelvd.fish");

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
    /// Program to run. `None` uses `$SHELL` / the platform default shell.
    pub shell: Option<String>,
    /// Extra arguments passed to the shell. When non-empty, integration
    /// injection is skipped (the caller is driving the command line itself).
    pub args: Vec<String>,
    /// Working directory for the child.
    pub cwd: Option<PathBuf>,
    /// Additional environment variables (applied after defaults, so they win).
    pub env: Vec<(String, String)>,
    /// Initial PTY size.
    pub size: PtySize,
    /// Auto-inject shelvd's shell integration (OSC-133 command blocks) when the
    /// shell is recognized (bash/zsh/fish). On by default — no dotfile edit
    /// needed; the generated init sources the user's own config first.
    pub shell_integration: bool,
}

impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            shell: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            size: PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            shell_integration: true,
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
    /// Temp dir holding the generated shell-integration init files, removed on
    /// drop.
    integration_dir: Option<PathBuf>,
}

impl Pty {
    /// Spawn the child behind a fresh PTY. `notify` is invoked from the reader
    /// thread whenever a [`PtyMsg`] becomes available on the receiver.
    pub fn spawn(opts: PtyOptions, notify: impl Fn() + Send + 'static) -> Result<Self, PtyError> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(opts.size)?;

        // Resolve the shell program (so we can both launch and classify it).
        let shell = opts
            .shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .filter(|s| !s.trim().is_empty());
        let mut cmd = match &shell {
            Some(shell) => CommandBuilder::new(shell),
            None => CommandBuilder::new_default_prog(),
        };
        for arg in &opts.args {
            cmd.arg(arg);
        }
        if let Some(cwd) = &opts.cwd {
            cmd.cwd(cwd.as_os_str());
        }
        // Sensible defaults first; the integration guard reads TERM_PROGRAM.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "shelvd");

        // Auto-inject shell integration unless the caller supplied its own args.
        let integration_dir = if opts.shell_integration && opts.args.is_empty() {
            let kind = shell.as_deref().map(ShellKind::detect).unwrap_or(ShellKind::Other);
            inject_integration(&mut cmd, kind).unwrap_or_else(|e| {
                log::debug!("shell integration injection skipped: {e}");
                None
            })
        } else {
            None
        };

        // User-provided env overrides everything above.
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

        Ok(Self { master: pair.master, writer, rx, child, reader: Some(reader), integration_dir })
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

    /// Whether the child's terminal currently echoes typed input.
    ///
    /// Returns `false` when a program has switched the tty to no-echo to read a
    /// secret — e.g. `sudo` or `ssh` prompting for a password — so a caller that
    /// echoes input itself (the input band) can mask it. The real bytes are still
    /// sent on write; only the on-screen rendering should hide them.
    ///
    /// Defaults to `true` (assume echo is on) when the termios query fails, and
    /// is a constant `true` off-unix where termios is unavailable — we never hide
    /// input we are not sure is secret.
    #[cfg(unix)]
    pub fn echo_enabled(&self) -> bool {
        use nix::sys::termios::LocalFlags;
        self.master
            .get_termios()
            .map(|t| t.local_flags.contains(LocalFlags::ECHO))
            .unwrap_or(true)
    }

    /// See the unix variant; off-unix termios is unavailable, so input is always
    /// shown as typed.
    #[cfg(not(unix))]
    pub fn echo_enabled(&self) -> bool {
        true
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // The reader thread ends on its own once the PTY closes; don't join to
        // avoid blocking shutdown if it is mid-read.
        self.reader.take();
        if let Some(dir) = self.integration_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The shells whose integration shelvd can auto-inject.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Other,
}

impl ShellKind {
    /// Classify a shell by its program path's file name.
    fn detect(path: &str) -> Self {
        let name = Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim_start_matches('-'); // login shells arrive as e.g. "-bash"
        match name {
            "bash" => Self::Bash,
            "zsh" => Self::Zsh,
            "fish" => Self::Fish,
            _ => Self::Other,
        }
    }
}

/// Write the generated init file(s) and point `cmd` at them so the shell loads
/// the user's own config and then shelvd's integration. Returns the temp dir to
/// clean up, or `None` for shells we don't recognize.
fn inject_integration(cmd: &mut CommandBuilder, kind: ShellKind) -> std::io::Result<Option<PathBuf>> {
    if kind == ShellKind::Other {
        return Ok(None);
    }
    // One temp dir per process holds the generated files.
    let dir = std::env::temp_dir().join(format!("shelvd-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    match kind {
        ShellKind::Bash => {
            // bash --rcfile reads this instead of ~/.bashrc, so re-source the
            // usual files first, then the integration.
            let rc = dir.join("init.bash");
            let body = format!(
                "# Generated by shelvd. Loads your bash config, then shelvd integration.\n\
                 if [ -f /etc/bash.bashrc ]; then source /etc/bash.bashrc; fi\n\
                 if [ -f \"$HOME/.bashrc\" ]; then source \"$HOME/.bashrc\"; fi\n\
                 {BASH_INTEGRATION}"
            );
            std::fs::write(&rc, body)?;
            cmd.arg("--rcfile");
            cmd.arg(&rc);
            cmd.arg("-i");
        }
        ShellKind::Fish => {
            // fish -C runs after the user's config.fish.
            let script = dir.join("shelvd.fish");
            std::fs::write(&script, FISH_INTEGRATION)?;
            cmd.arg("-C");
            cmd.arg(format!("source {}", script.display()));
        }
        ShellKind::Zsh => {
            // zsh reads $ZDOTDIR/.zshenv then .zshrc; point ZDOTDIR at our temp
            // dir, re-source the user's files from their real dir, then the
            // integration, and restore ZDOTDIR so later config behaves.
            let user_zdotdir = std::env::var("ZDOTDIR")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default();
            std::fs::write(
                dir.join(".zshenv"),
                "[ -f \"$SHELVD_USER_ZDOTDIR/.zshenv\" ] && source \"$SHELVD_USER_ZDOTDIR/.zshenv\"\n",
            )?;
            let zshrc = format!(
                "ZDOTDIR=\"$SHELVD_USER_ZDOTDIR\"\n\
                 [ -f \"$SHELVD_USER_ZDOTDIR/.zshrc\" ] && source \"$SHELVD_USER_ZDOTDIR/.zshrc\"\n\
                 {ZSH_INTEGRATION}"
            );
            std::fs::write(dir.join(".zshrc"), zshrc)?;
            cmd.env("SHELVD_USER_ZDOTDIR", user_zdotdir);
            cmd.env("ZDOTDIR", &dir);
        }
        ShellKind::Other => return Ok(None),
    }
    Ok(Some(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_shell_kind_from_path() {
        assert_eq!(ShellKind::detect("/bin/bash"), ShellKind::Bash);
        assert_eq!(ShellKind::detect("/usr/bin/zsh"), ShellKind::Zsh);
        assert_eq!(ShellKind::detect("/usr/local/bin/fish"), ShellKind::Fish);
        assert_eq!(ShellKind::detect("-bash"), ShellKind::Bash); // login-shell argv0
        assert_eq!(ShellKind::detect("/bin/sh"), ShellKind::Other);
        assert_eq!(ShellKind::detect(""), ShellKind::Other);
    }

    #[test]
    fn integration_writes_bash_rcfile() {
        let mut cmd = CommandBuilder::new("/bin/bash");
        let dir = inject_integration(&mut cmd, ShellKind::Bash).unwrap().unwrap();
        let rc = dir.join("init.bash");
        let body = std::fs::read_to_string(&rc).unwrap();
        assert!(body.contains("source \"$HOME/.bashrc\""));
        assert!(body.contains("__shelvd_prompt"), "embeds the integration script");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_shell_is_not_injected() {
        let mut cmd = CommandBuilder::new("/bin/sh");
        assert!(inject_integration(&mut cmd, ShellKind::Other).unwrap().is_none());
    }
}
