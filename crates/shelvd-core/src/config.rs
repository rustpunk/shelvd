//! Top-level runtime configuration and TOML loading.
//!
//! The runtime [`Config`] is the in-memory defaults optionally overlaid with a
//! user TOML file (see [`Config::load_default`]). The on-disk format is a thin
//! spec layer in which every field is optional and *merges onto* the defaults,
//! so a partial file overrides only the keys it names.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::color::Rgba;
use crate::error::{Error, Result};
use crate::palette::Palette;
use crate::snapshot::CursorShape;
use crate::theme::Theme;

/// Runtime configuration for a shelvd session.
#[derive(Clone, Debug)]
pub struct Config {
    pub theme: Theme,
    /// Shell to launch. `None` falls back to `$SHELL` / a platform default.
    pub shell: Option<String>,
    /// Number of scrollback lines to retain.
    pub scrollback: usize,
    /// Whether a program may set the system clipboard via OSC 52. On by default
    /// (matching alacritty's `OnlyCopy` and mainstream terminals); set `false` to
    /// deny program-driven clipboard writes. The read direction is never honored.
    pub osc52_clipboard_write: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            shell: None,
            scrollback: 10_000,
            osc52_clipboard_write: true,
        }
    }
}

impl Config {
    /// Default config-file path: `<config_dir>/shelvd/config.toml`
    /// (e.g. `~/.config/shelvd/config.toml` on Linux).
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("shelvd").join("config.toml"))
    }

    /// Parse the config file at `path`, merging its values onto the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))?;
        file.resolve()
    }

    /// Load from `$SHELVD_CONFIG` if set, otherwise the default path. A missing
    /// file yields the built-in defaults; a malformed file logs a warning and
    /// also falls back to the defaults, so a typo never stops the terminal from
    /// starting.
    pub fn load_default() -> Self {
        let path = match std::env::var_os("SHELVD_CONFIG") {
            Some(p) => Some(PathBuf::from(p)),
            None => Self::config_path(),
        };
        let Some(path) = path else {
            return Self::default();
        };
        if !path.exists() {
            return Self::default();
        }
        match Self::load(&path) {
            Ok(config) => config,
            Err(e) => {
                log::warn!("{e}; using built-in defaults");
                Self::default()
            }
        }
    }
}

// --- on-disk spec: every field optional, merged onto the defaults ------------

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    shell: Option<String>,
    scrollback: Option<usize>,
    osc52_clipboard_write: Option<bool>,
    theme: ThemeFile,
}

impl ConfigFile {
    fn resolve(self) -> Result<Config> {
        let mut config = Config::default();
        if self.shell.is_some() {
            config.shell = self.shell;
        }
        if let Some(scrollback) = self.scrollback {
            config.scrollback = scrollback;
        }
        if let Some(osc52_clipboard_write) = self.osc52_clipboard_write {
            config.osc52_clipboard_write = osc52_clipboard_write;
        }
        self.theme.resolve_into(&mut config.theme)?;
        Ok(config)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemeFile {
    font_family: Option<String>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    cursor_shape: Option<CursorShape>,
    padding: Option<PaddingFile>,
    palette: PaletteFile,
}

impl ThemeFile {
    fn resolve_into(self, theme: &mut Theme) -> Result<()> {
        if self.font_family.is_some() {
            theme.font_family = self.font_family;
        }
        if let Some(font_size) = self.font_size {
            theme.font_size = font_size;
        }
        if let Some(line_height) = self.line_height {
            theme.line_height = line_height;
        }
        if let Some(cursor_shape) = self.cursor_shape {
            theme.cursor_shape = cursor_shape;
        }
        if let Some(padding) = self.padding {
            if let Some(x) = padding.x {
                theme.padding.x = x;
            }
            if let Some(y) = padding.y {
                theme.padding.y = y;
            }
        }
        self.palette.resolve_into(&mut theme.palette)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PaddingFile {
    x: Option<f32>,
    y: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PaletteFile {
    foreground: Option<Rgba>,
    background: Option<Rgba>,
    cursor: Option<Rgba>,
    cursor_text: Option<Rgba>,
    selection: Option<Rgba>,
    /// The 16 base ANSI colors (indices 0..16). Must be exactly 16 when present;
    /// the 6×6×6 cube and grayscale ramp are regenerated from them.
    ansi: Option<Vec<Rgba>>,
}

impl PaletteFile {
    fn resolve_into(self, palette: &mut Palette) -> Result<()> {
        if let Some(c) = self.foreground {
            palette.foreground = c;
        }
        if let Some(c) = self.background {
            palette.background = c;
        }
        if let Some(c) = self.cursor {
            palette.cursor = c;
        }
        if let Some(c) = self.cursor_text {
            palette.cursor_text = c;
        }
        if let Some(c) = self.selection {
            palette.selection = c;
        }
        if let Some(ansi) = self.ansi {
            let base: [Rgba; 16] = ansi.as_slice().try_into().map_err(|_| {
                Error::Config(format!(
                    "theme.palette.ansi must list exactly 16 colors, found {}",
                    ansi.len()
                ))
            })?;
            // Rebuild from the (already-applied) special slots so both win.
            *palette = Palette::from_base16(
                base,
                palette.foreground,
                palette.background,
                palette.cursor,
                palette.cursor_text,
                palette.selection,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(toml_src: &str) -> Result<Config> {
        toml::from_str::<ConfigFile>(toml_src)
            .map_err(|e| Error::Config(e.to_string()))?
            .resolve()
    }

    #[test]
    fn empty_is_default() {
        let config = resolve("").unwrap();
        assert_eq!(config.scrollback, Config::default().scrollback);
        assert!(config.shell.is_none());
    }

    #[test]
    fn partial_overrides_only_named() {
        let config = resolve(
            r##"
            scrollback = 500
            [theme.palette]
            background = "#102030"
            "##,
        )
        .unwrap();
        assert_eq!(config.scrollback, 500);
        assert_eq!(config.theme.palette.background, Rgba::rgb(0x10, 0x20, 0x30));
        // Anything not named keeps the default.
        assert_eq!(config.theme.palette.foreground, Palette::default().foreground);
        assert_eq!(config.theme.font_size, Theme::default().font_size);
    }

    #[test]
    fn cursor_shape_parses() {
        let config = resolve("[theme]\ncursor_shape = \"beam\"\n").unwrap();
        assert_eq!(config.theme.cursor_shape, CursorShape::Beam);
    }

    #[test]
    fn ansi_must_be_sixteen() {
        assert!(resolve("[theme.palette]\nansi = [\"#000000\", \"#ffffff\"]\n").is_err());
    }

    #[test]
    fn ansi_sixteen_sets_base_and_regenerates_cube() {
        let mut src = String::from("[theme.palette]\nansi = [");
        for i in 0..16u8 {
            src.push_str(&format!("\"#0000{i:02x}\","));
        }
        src.push_str("]\n");
        let palette = resolve(&src).unwrap().theme.palette;
        assert_eq!(palette.indexed(1), Rgba::rgb(0, 0, 1));
        // Index 16 is the xterm cube origin, regenerated regardless of base16.
        assert_eq!(palette.indexed(16), Rgba::rgb(0, 0, 0));
    }

    #[test]
    fn unknown_field_rejected() {
        assert!(toml::from_str::<ConfigFile>("nonsense = 1\n").is_err());
    }

    #[test]
    fn osc52_clipboard_write_defaults_on() {
        assert!(resolve("").unwrap().osc52_clipboard_write);
    }

    #[test]
    fn osc52_clipboard_write_can_be_disabled() {
        let config = resolve("osc52_clipboard_write = false\n").unwrap();
        assert!(!config.osc52_clipboard_write);
        // Anything not named keeps its default.
        assert_eq!(config.scrollback, Config::default().scrollback);
    }
}
