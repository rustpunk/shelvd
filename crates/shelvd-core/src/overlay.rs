//! A render-ready description of a modal overlay (command palette / history
//! search). This is *application* UI, not terminal state — `shelvd-app` owns the
//! interaction and builds one of these each frame; `shelvd-render` paints it on
//! top of the grid. It lives in `shelvd-core` only because it is shared render
//! vocabulary, like [`GridSnapshot`](crate::GridSnapshot).

use crate::color::Rgba;

/// Colors for the overlay, resolved from the theme by the app.
#[derive(Clone, Copy, Debug)]
pub struct OverlayColors {
    /// Opaque panel background.
    pub panel_bg: Rgba,
    /// Primary text.
    pub fg: Rgba,
    /// Secondary/disabled text (item details, placeholder).
    pub dim: Rgba,
    /// Background of the selected row.
    pub sel_bg: Rgba,
    /// Accent for the prompt sigil, query cursor, and bottom rule.
    pub accent: Rgba,
}

/// One selectable row in the overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayItem {
    /// Primary label (the action name, or a history command).
    pub label: String,
    /// Optional secondary text shown dimmed after the label.
    pub detail: Option<String>,
}

/// A modal overlay to draw over the grid: a query line plus a filtered list.
#[derive(Clone, Debug)]
pub struct Overlay {
    /// Sigil shown before the query (e.g. `>` or `history`).
    pub prompt: String,
    /// The text typed so far.
    pub query: String,
    /// Filtered items, best match first.
    pub items: Vec<OverlayItem>,
    /// Index into `items` of the highlighted row.
    pub selected: usize,
    /// Resolved colors.
    pub colors: OverlayColors,
}
