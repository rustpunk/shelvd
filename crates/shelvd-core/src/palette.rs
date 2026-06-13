//! The 256-color terminal palette plus the special foreground/background/cursor
//! slots. Indices follow the xterm convention:
//!
//! | Indices  | Meaning           |
//! | -------- | ----------------- |
//! | 0..16    | named ANSI colors |
//! | 16..232  | 6×6×6 color cube  |
//! | 232..256 | grayscale ramp    |

use crate::color::Rgba;

/// A resolved color table used to turn indexed/named terminal colors into RGB.
#[derive(Clone, Debug)]
pub struct Palette {
    /// The 256 indexable colors.
    pub colors: [Rgba; 256],
    /// Default foreground.
    pub foreground: Rgba,
    /// Default background.
    pub background: Rgba,
    /// Cursor color.
    pub cursor: Rgba,
    /// Color of text under a block cursor.
    pub cursor_text: Rgba,
    /// Selection background.
    pub selection: Rgba,
}

impl Palette {
    /// Look up one of the 256 indexed colors.
    #[inline]
    pub fn indexed(&self, idx: u8) -> Rgba {
        self.colors[idx as usize]
    }

    /// The shelvd default: a warm, low-glow dark theme — old phosphor on
    /// shelved hardware that someone powered back up.
    pub fn shelvd_dark() -> Self {
        // The 16 base colors: a muted, slightly amber-tinted set fitting the
        // decommissioned-relic aesthetic (warm foreground, desaturated accents).
        const BASE16: [u32; 16] = [
            0x14110f, // 0 black (near-background, warm)
            0xc25f5f, // 1 red (oxidized)
            0x7f9a6a, // 2 green (verdigris)
            0xc9a45b, // 3 yellow (brass)
            0x6f8aa6, // 4 blue (cold steel)
            0x9a7aa6, // 5 magenta (tarnish)
            0x6fa6a0, // 6 cyan (patina)
            0xcfc4b4, // 7 white (bone)
            0x4a4137, // 8 bright black (ash)
            0xe07a6f, // 9 bright red
            0x9ec27e, // 10 bright green
            0xe6c178, // 11 bright yellow
            0x8fb0d0, // 12 bright blue
            0xc0a0cc, // 13 bright magenta
            0x8fd0c8, // 14 bright cyan
            0xf0e6d6, // 15 bright white
        ];

        let mut colors = [Rgba::rgb(0, 0, 0); 256];
        for (i, &hex) in BASE16.iter().enumerate() {
            colors[i] = Rgba::hex(hex);
        }
        // 16..232: 6×6×6 color cube.
        let levels = [0u8, 95, 135, 175, 215, 255];
        let mut idx = 16;
        for r in 0..6 {
            for g in 0..6 {
                for b in 0..6 {
                    colors[idx] = Rgba::rgb(levels[r], levels[g], levels[b]);
                    idx += 1;
                }
            }
        }
        // 232..256: 24-step grayscale ramp.
        for i in 0..24 {
            let v = 8 + i as u8 * 10;
            colors[232 + i] = Rgba::rgb(v, v, v);
        }

        Self {
            colors,
            foreground: Rgba::hex(0xcfc4b4),
            background: Rgba::hex(0x14110f),
            cursor: Rgba::hex(0xe6a86c),
            cursor_text: Rgba::hex(0x14110f),
            selection: Rgba::hex(0x3a3228),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::shelvd_dark()
    }
}
