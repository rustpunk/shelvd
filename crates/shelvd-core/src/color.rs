//! 8-bit-per-channel sRGB color, plus conversions for the GPU.

use serde::{Deserialize, Serialize};

/// An sRGB color with alpha. Channels are stored as the canonical sRGB bytes
/// (the values you'd write in a config file); convert to linear for shading.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Build from a `0xRRGGBB` hex literal (alpha = 255).
    pub const fn hex(rgb: u32) -> Self {
        Self::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }

    /// Parse a CSS-style hex string: `#rgb`, `#rrggbb`, or `#rrggbbaa` (the
    /// leading `#` is optional). This is the form used in config/theme files.
    pub fn from_hex_str(s: &str) -> Result<Self, ParseColorError> {
        let h = s.strip_prefix('#').unwrap_or(s);
        let bad = || ParseColorError(s.to_owned());
        if !h.is_ascii() {
            return Err(bad());
        }
        let byte = |slice: &str| u8::from_str_radix(slice, 16).map_err(|_| bad());
        // A single nibble `n` expands to the byte `n * 17` (e.g. `f` -> 0xff).
        let nib = |slice: &str| u8::from_str_radix(slice, 16).map(|n| n * 17).map_err(|_| bad());
        match h.len() {
            3 => Ok(Self::rgb(nib(&h[0..1])?, nib(&h[1..2])?, nib(&h[2..3])?)),
            6 => Ok(Self::rgb(byte(&h[0..2])?, byte(&h[2..4])?, byte(&h[4..6])?)),
            8 => Ok(Self::new(byte(&h[0..2])?, byte(&h[2..4])?, byte(&h[4..6])?, byte(&h[6..8])?)),
            _ => Err(bad()),
        }
    }

    /// sRGB channels normalized to `[0, 1]` (no gamma conversion).
    pub fn to_srgb_f32(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// Linear-light RGBA in `[0, 1]`, suitable for a shader that writes to an
    /// `*_srgb` surface (the GPU re-applies the sRGB transfer on store).
    pub fn to_linear_f32(self) -> [f32; 4] {
        [
            srgb_to_linear(self.r),
            srgb_to_linear(self.g),
            srgb_to_linear(self.b),
            self.a as f32 / 255.0,
        ]
    }

    /// Same as [`Self::to_linear_f32`] but widened for `wgpu::Color`.
    pub fn to_linear_f64(self) -> [f64; 4] {
        let [r, g, b, a] = self.to_linear_f32();
        [r as f64, g as f64, b as f64, a as f64]
    }

    /// Blend `self` over `under` using `self`'s alpha (straight alpha).
    pub fn over(self, under: Rgba) -> Rgba {
        let sa = self.a as f32 / 255.0;
        let mix = |s: u8, u: u8| ((s as f32 * sa) + (u as f32 * (1.0 - sa))).round() as u8;
        Rgba::new(mix(self.r, under.r), mix(self.g, under.g), mix(self.b, under.b), 255)
    }
}

impl std::fmt::Debug for Rgba {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
    }
}

impl std::fmt::Display for Rgba {
    /// `#rrggbb`, or `#rrggbbaa` when alpha is not fully opaque. Round-trips
    /// through [`Rgba::from_hex_str`].
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

impl std::str::FromStr for Rgba {
    type Err = ParseColorError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex_str(s)
    }
}

/// Error returned when a color string is not valid hex.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid color {0:?}: expected #rgb, #rrggbb, or #rrggbbaa hex")]
pub struct ParseColorError(String);

impl Serialize for Rgba {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_forms() {
        assert_eq!("#ff8800".parse::<Rgba>().unwrap(), Rgba::rgb(255, 136, 0));
        assert_eq!("ff8800".parse::<Rgba>().unwrap(), Rgba::rgb(255, 136, 0));
        assert_eq!("#f80".parse::<Rgba>().unwrap(), Rgba::rgb(255, 136, 0));
        assert_eq!("#11223344".parse::<Rgba>().unwrap(), Rgba::new(0x11, 0x22, 0x33, 0x44));
        assert!("#xyz".parse::<Rgba>().is_err());
        assert!("#12345".parse::<Rgba>().is_err());
        assert!("".parse::<Rgba>().is_err());
    }

    #[test]
    fn display_round_trips() {
        let c = Rgba::rgb(0x12, 0x34, 0x56);
        assert_eq!(c.to_string(), "#123456");
        assert_eq!(c.to_string().parse::<Rgba>().unwrap(), c);
        assert_eq!(Rgba::new(1, 2, 3, 4).to_string(), "#01020304");
    }
}
