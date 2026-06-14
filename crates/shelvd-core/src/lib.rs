//! `shelvd-core` — shared vocabulary for the shelvd terminal.
//!
//! Every other crate speaks these types: [`Rgba`] color, the [`Palette`],
//! geometry ([`GridSize`], [`PixelSize`], [`CellMetrics`]), the render-ready
//! [`GridSnapshot`], and the [`Theme`]/[`Config`] that tie them together.

pub mod color;
pub mod config;
pub mod error;
pub mod frozen;
pub mod geometry;
pub mod overlay;
pub mod palette;
pub mod snapshot;
pub mod theme;

pub use color::Rgba;
pub use config::Config;
pub use error::{Error, Result};
pub use frozen::FrozenBlock;
pub use geometry::{CellMetrics, GridSize, Padding, PixelSize, ResizeEdge, TitlebarHit};
pub use overlay::{Overlay, OverlayColors, OverlayItem};
pub use palette::Palette;
pub use snapshot::{
    CellFlags, CellSnapshot, CursorShape, CursorSnapshot, GridSnapshot, RowDecor, StickyHeader,
};
pub use theme::Theme;
