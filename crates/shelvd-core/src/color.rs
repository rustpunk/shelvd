//! 8-bit-per-channel sRGB color, plus conversions for the GPU.

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

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}
