//! Top-level runtime configuration. (M0: in-memory defaults; TOML loading lands
//! in a later milestone.)

use crate::theme::Theme;

/// Runtime configuration for a shelvd session.
#[derive(Clone, Debug)]
pub struct Config {
    pub theme: Theme,
    /// Shell to launch. `None` falls back to `$SHELL` / a platform default.
    pub shell: Option<String>,
    /// Number of scrollback lines to retain.
    pub scrollback: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            shell: None,
            scrollback: 10_000,
        }
    }
}
