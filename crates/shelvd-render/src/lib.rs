//! `shelvd-render` — GPU renderer.
//!
//! Owns the [`wgpu`] surface/device and a [`glyphon`] text stack, and draws a
//! [`GridSnapshot`] each frame: solid cell backgrounds and the cursor via the
//! [`rect`] layer, then glyphs via glyphon. It depends only on `shelvd-core`,
//! so it knows nothing about PTYs or `alacritty_terminal`.

mod rect;

use std::sync::Arc;

use glyphon::{
    Attrs, Buffer, Cache, Color as GColor, Family, FontSystem, Metrics, PrepareError,
    RenderError as GlyphRenderError, Resolution, Shaping, Style, SwashCache, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport, Weight,
};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use shelvd_core::{
    CellFlags, CellMetrics, CellSnapshot, CursorShape, GridSize, GridSnapshot, Overlay, Padding,
    PixelSize, ResizeEdge, Rgba, RowDecor, Theme, TitlebarHit,
};

use rect::{Rect, RectRenderer};

/// Height of shelvd's custom titlebar strip, in logical pixels.
const TITLEBAR_LOGICAL_H: f32 = 30.0;

/// Hit margin around the window edges for interactive resize, in logical pixels.
const RESIZE_MARGIN_LOGICAL: f32 = 8.0;

/// Errors from creating or driving the renderer.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("failed to create surface: {0}")]
    CreateSurface(#[from] wgpu::CreateSurfaceError),
    #[error("no compatible GPU adapter: {0}")]
    Adapter(#[from] wgpu::RequestAdapterError),
    #[error("failed to create device: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    #[error("frame acquisition failed: {0}")]
    Frame(&'static str),
    #[error("text prepare error: {0}")]
    Prepare(#[from] PrepareError),
    #[error("glyph render error: {0}")]
    GlyphRender(#[from] GlyphRenderError),
}

/// The GPU renderer.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    buffer: Buffer,
    /// One-line buffer for the sticky command header.
    sticky_buffer: Buffer,
    /// Multi-line buffer for the command-palette / history overlay.
    overlay_buffer: Buffer,
    /// One-line buffer for the custom titlebar's window title.
    titlebar_buffer: Buffer,
    /// One-glyph buffer for the close button's `×`.
    close_buffer: Buffer,

    rects: RectRenderer,

    default_fg: Rgba,
    /// Titlebar background and (slightly dimmed) title-text colors.
    titlebar_bg: Rgba,
    titlebar_fg: Rgba,
    /// Which window button is hovered (highlighted), if any.
    hovered_button: Option<TitlebarHit>,
    /// Hover-highlight colors for ordinary buttons and the close button.
    titlebar_btn_hover: Rgba,
    titlebar_close_hover: Rgba,
    padding_logical: Padding,
    font_size_logical: f32,
    line_height_factor: f32,
    font_family: Option<String>,
    scale: f32,
    cell: CellMetrics,
}

impl Renderer {
    /// Create a renderer for `window` at the given physical size and DPI scale.
    pub fn new<W>(
        window: Arc<W>,
        width: u32,
        height: u32,
        scale: f32,
        theme: &Theme,
    ) -> Result<Self, RenderError>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window)?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("shelvd device"),
            ..Default::default()
        }))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let swash_cache = SwashCache::new();

        let font_size_logical = theme.font_size;
        let line_height_factor = theme.line_height;
        let font_family = theme.font_family.clone();
        let metrics = metrics_for(font_size_logical, line_height_factor, scale);
        let cell = measure_cell(&mut font_system, metrics, font_family.as_deref());

        let mut buffer = Buffer::new(&mut font_system, metrics);
        buffer.set_size(&mut font_system, None, None);

        let mut sticky_buffer = Buffer::new(&mut font_system, metrics);
        sticky_buffer.set_size(&mut font_system, None, None);

        let mut overlay_buffer = Buffer::new(&mut font_system, metrics);
        overlay_buffer.set_size(&mut font_system, None, None);

        let mut titlebar_buffer = Buffer::new(&mut font_system, metrics);
        titlebar_buffer.set_size(&mut font_system, None, None);
        titlebar_buffer.set_text(
            &mut font_system,
            "shelvd",
            &base_attrs(font_family.as_deref()),
            Shaping::Advanced,
            None,
        );
        titlebar_buffer.shape_until_scroll(&mut font_system, false);

        let mut close_buffer = Buffer::new(&mut font_system, metrics);
        close_buffer.set_size(&mut font_system, None, None);
        close_buffer.set_text(
            &mut font_system,
            "×",
            &base_attrs(font_family.as_deref()),
            Shaping::Advanced,
            None,
        );
        close_buffer.shape_until_scroll(&mut font_system, false);

        let titlebar_bg = lift(theme.palette.foreground, theme.palette.background, 16);
        let titlebar_fg = lift(theme.palette.foreground, theme.palette.background, 205);
        let titlebar_btn_hover = lift(theme.palette.foreground, theme.palette.background, 36);
        let titlebar_close_hover = theme.palette.indexed(1);

        let rects = RectRenderer::new(&device, format);
        rects.set_resolution(&queue, config.width as f32, config.height as f32);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            buffer,
            sticky_buffer,
            overlay_buffer,
            titlebar_buffer,
            close_buffer,
            rects,
            default_fg: theme.palette.foreground,
            titlebar_bg,
            titlebar_fg,
            hovered_button: None,
            titlebar_btn_hover,
            titlebar_close_hover,
            padding_logical: theme.padding,
            font_size_logical,
            line_height_factor,
            font_family,
            scale,
            cell,
        })
    }

    /// Reconfigure the surface for a new physical size / DPI scale.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        if width == 0 || height == 0 {
            return;
        }
        let scale_changed = (scale - self.scale).abs() > f32::EPSILON;
        self.scale = scale;
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.rects.set_resolution(&self.queue, width as f32, height as f32);
        if scale_changed {
            let metrics = self.metrics();
            self.cell = measure_cell(&mut self.font_system, metrics, self.font_family.as_deref());
            self.buffer.set_metrics(&mut self.font_system, metrics);
            self.sticky_buffer.set_metrics(&mut self.font_system, metrics);
            self.overlay_buffer.set_metrics(&mut self.font_system, metrics);
            self.titlebar_buffer.set_metrics(&mut self.font_system, metrics);
            self.close_buffer.set_metrics(&mut self.font_system, metrics);
        }
    }

    /// The grid size (in cells) that fits the current surface.
    pub fn grid_size(&self) -> GridSize {
        // The titlebar eats into the usable height; the grid fills what is left
        // below it, minus the usual symmetric padding.
        let usable_h = (self.config.height as f32 - self.titlebar_height()).max(0.0) as u32;
        GridSize::from_pixels(
            PixelSize::new(self.config.width, usable_h),
            self.cell,
            self.padding_physical(),
        )
    }

    /// Current per-cell pixel metrics.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell
    }

    /// How many overlay item rows fit below the query line this frame. The app
    /// uses this to window its filtered list before building the [`Overlay`], so
    /// only the visible rows are materialized.
    pub fn overlay_capacity(&self) -> usize {
        (self.grid_size().rows.saturating_sub(2) as usize).min(12)
    }

    /// Current surface size in physical pixels.
    pub fn surface_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// The DPI scale factor the renderer is currently configured for.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Physical height of the custom titlebar strip.
    pub fn titlebar_height(&self) -> f32 {
        TITLEBAR_LOGICAL_H * self.scale
    }

    /// Vertical origin (physical px) of the grid's top row: below the titlebar
    /// strip and the top padding.
    fn grid_origin_y(&self) -> f32 {
        self.titlebar_height() + self.padding_physical().y
    }

    /// What the pointer is over within the titlebar: a window button, the
    /// draggable region, or `None` below the titlebar strip. Buttons are square,
    /// right-aligned: minimize, maximize, close (close at the far right).
    pub fn titlebar_hit(&self, x: f32, y: f32) -> Option<TitlebarHit> {
        if y >= self.titlebar_height() {
            return None;
        }
        let bw = self.titlebar_height();
        let w = self.config.width as f32;
        Some(if x >= w - bw {
            TitlebarHit::Close
        } else if x >= w - 2.0 * bw {
            TitlebarHit::Maximize
        } else if x >= w - 3.0 * bw {
            TitlebarHit::Minimize
        } else {
            TitlebarHit::Drag
        })
    }

    /// Set which window button is highlighted (hover feedback).
    pub fn set_hovered_button(&mut self, hit: Option<TitlebarHit>) {
        self.hovered_button = hit;
    }

    /// Window buttons at the right of the titlebar: a hover highlight plus a
    /// geometric icon (the close `×` glyph is drawn as text in `render`).
    fn push_titlebar_buttons(&self, rects: &mut Vec<Rect>, width: f32, tb: f32) {
        let bw = tb; // square buttons
        let min_x = width - 3.0 * bw;
        let max_x = width - 2.0 * bw;
        let close_x = width - bw;

        if let Some(h) = self.hovered_button {
            let cell = match h {
                TitlebarHit::Minimize => Some((min_x, self.titlebar_btn_hover)),
                TitlebarHit::Maximize => Some((max_x, self.titlebar_btn_hover)),
                TitlebarHit::Close => Some((close_x, self.titlebar_close_hover)),
                TitlebarHit::Drag => None,
            };
            if let Some((hx, color)) = cell {
                rects.push(Rect { x: hx, y: 0.0, w: bw, h: tb, color: color.to_linear_f32() });
            }
        }

        let icon = self.titlebar_fg.to_linear_f32();
        let thick = (1.5 * self.scale).max(1.0);
        let cy = tb * 0.5;
        // Minimize: a centered horizontal bar.
        let iw = bw * 0.32;
        rects.push(Rect { x: min_x + (bw - iw) * 0.5, y: cy - thick * 0.5, w: iw, h: thick, color: icon });
        // Maximize: a small square outline (four thin sides).
        let s = bw * 0.30;
        let sx = max_x + (bw - s) * 0.5;
        let sy = cy - s * 0.5;
        rects.push(Rect { x: sx, y: sy, w: s, h: thick, color: icon });
        rects.push(Rect { x: sx, y: sy + s - thick, w: s, h: thick, color: icon });
        rects.push(Rect { x: sx, y: sy, w: thick, h: s, color: icon });
        rects.push(Rect { x: sx + s - thick, y: sy, w: thick, h: s, color: icon });
        // Close `×` is drawn as a glyph in `render`.
    }

    /// Which window edge/corner the pointer is over for interactive resize, or
    /// `None` in the interior. A small margin hugs each edge; corners win.
    pub fn resize_edge(&self, x: f32, y: f32) -> Option<ResizeEdge> {
        edge_at(
            x,
            y,
            self.config.width as f32,
            self.config.height as f32,
            RESIZE_MARGIN_LOGICAL * self.scale,
        )
    }

    /// Draw one frame: the grid from `snap`, plus `overlay` (command palette /
    /// history search) layered on top when present.
    ///
    /// `grid_offset_px` shifts the grid layer down by that many physical pixels
    /// (the app's fill-transition glide). It moves grid content only — the
    /// sticky header and any overlay stay pinned to the top.
    pub fn render(
        &mut self,
        snap: &GridSnapshot,
        overlay: Option<&Overlay>,
        grid_offset_px: f32,
    ) -> Result<(), RenderError> {
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(t) | Cst::Suboptimal(t) => t,
            // Transient: skip this frame, the next redraw will retry.
            Cst::Timeout | Cst::Occluded => return Ok(()),
            // Surface needs reconfiguring; do it and retry once.
            Cst::Outdated | Cst::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Cst::Success(t) | Cst::Suboptimal(t) => t,
                    _ => return Ok(()),
                }
            }
            Cst::Validation => return Err(RenderError::Frame("surface validation error")),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let layout = overlay.map(|ov| self.overlay_layout(ov));
        // The overlay panel overdraws the top rows; blank those grid glyphs (or
        // just the single sticky-header row when there is no overlay).
        let blank_rows =
            layout.map_or(u16::from(snap.sticky.is_some()), |l| l.panel_rows);

        let mut rects = self.build_rects(snap, overlay.is_some(), grid_offset_px);
        if let (Some(ov), Some(l)) = (overlay, layout) {
            self.append_overlay_rects(&mut rects, ov, l);
        }
        self.rects.upload(&self.device, &self.queue, &rects);

        self.build_text(snap, blank_rows);

        // Sticky header — suppressed while an overlay is open.
        let sticky_color = if overlay.is_none() {
            snap.sticky
                .as_ref()
                .map(|s| if s.failed { snap.block_stripe } else { self.default_fg })
        } else {
            None
        };
        if let (Some(sticky), Some(color)) = (&snap.sticky, sticky_color) {
            let attrs = cell_attrs(self.font_family.as_deref(), color, true, false);
            self.sticky_buffer.set_text(
                &mut self.font_system,
                &sticky.command,
                &attrs,
                Shaping::Advanced,
                None,
            );
            self.sticky_buffer.shape_until_scroll(&mut self.font_system, false);
        }

        // Overlay text.
        if let Some(ov) = overlay {
            self.set_overlay_text(ov);
        }

        self.viewport.update(
            &self.queue,
            Resolution { width: self.config.width, height: self.config.height },
        );
        let pad = self.padding_physical();
        let tb = self.titlebar_height();
        let grid_pad = Padding::new(pad.x, self.grid_origin_y() + grid_offset_px);
        // Sticky header and overlay text sit just below the titlebar strip.
        let below_bar = Padding::new(pad.x, tb + pad.y);
        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let mut areas = vec![text_area(&self.buffer, to_gcolor(self.default_fg), grid_pad, bounds)];
        // Window title, vertically centered in the titlebar strip.
        let title_pad = Padding::new(pad.x, ((tb - self.cell.height) * 0.5).max(0.0));
        areas.push(text_area(&self.titlebar_buffer, to_gcolor(self.titlebar_fg), title_pad, bounds));
        // Close-button glyph (×), centered in the rightmost titlebar button.
        let close_x = self.config.width as f32 - tb;
        let close_pad = Padding::new(
            close_x + (tb - self.cell.width) * 0.5,
            ((tb - self.cell.height) * 0.5).max(0.0),
        );
        areas.push(text_area(&self.close_buffer, to_gcolor(self.titlebar_fg), close_pad, bounds));
        if let Some(color) = sticky_color {
            areas.push(text_area(&self.sticky_buffer, to_gcolor(color), below_bar, bounds));
        }
        if let Some(ov) = overlay {
            areas.push(text_area(&self.overlay_buffer, to_gcolor(ov.colors.fg), below_bar, bounds));
        }
        self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        )?;

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("shelvd encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shelvd pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(snap.background)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.rects.draw(&mut pass);
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)?;
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        self.atlas.trim();
        Ok(())
    }

    /// Map a physical pixel position to a grid cell plus which half was hit
    /// (`true` = right half), clamped to the grid.
    pub fn pixel_to_cell(&self, x: f32, y: f32) -> (u16, u16, bool) {
        let pad = self.padding_physical();
        let cw = self.cell.width.max(1.0);
        let ch = self.cell.height.max(1.0);
        let grid = self.grid_size();
        let col = (((x - pad.x) / cw).floor()).clamp(0.0, (grid.cols - 1) as f32) as u16;
        let row = (((y - self.grid_origin_y()) / ch).floor()).clamp(0.0, (grid.rows - 1) as f32) as u16;
        let right_half = (x - (pad.x + col as f32 * cw)) > cw * 0.5;
        (col, row, right_half)
    }

    fn metrics(&self) -> Metrics {
        metrics_for(self.font_size_logical, self.line_height_factor, self.scale)
    }

    fn padding_physical(&self) -> Padding {
        Padding::new(
            self.padding_logical.x * self.scale,
            self.padding_logical.y * self.scale,
        )
    }

    fn build_rects(&self, snap: &GridSnapshot, overlay_open: bool, grid_offset_px: f32) -> Vec<Rect> {
        let pad = self.padding_physical();
        let (cw, ch) = (self.cell.width, self.cell.height);
        let width = self.config.width as f32;
        let mut rects = Vec::new();
        let default_bg = snap.background;
        // Grid content starts below the titlebar and rides the fill-transition
        // offset; the titlebar and sticky band stay pinned while the grid glides.
        let grid_pad_y = self.grid_origin_y() + grid_offset_px;

        // Cell backgrounds.
        for row in 0..snap.rows {
            for col in 0..snap.cols {
                if let Some(cell) = snap.cell(col, row) {
                    if cell.bg != default_bg {
                        let x = pad.x + col as f32 * cw;
                        let y = grid_pad_y + row as f32 * ch;
                        rects.push(Rect { x, y, w: cw, h: ch, color: cell.bg.to_linear_f32() });
                    }
                }
            }
        }

        // Failed-block background wash (translucent, over the cell backgrounds).
        // A blank top-padding row carries no block (`block_id == 0`) and is never
        // `failed`, so this never washes into the empty space above the prompt.
        let tint = snap.block_tint.to_linear_f32();
        for (row, decor) in snap.rows_decor.iter().enumerate() {
            if decor.failed && decor.block_id != 0 {
                let y = grid_pad_y + row as f32 * ch;
                rects.push(Rect { x: 0.0, y, w: width, h: ch, color: tint });
            }
        }

        // Selection, painted over the wash.
        for row in 0..snap.rows {
            for col in 0..snap.cols {
                if let Some(cell) = snap.cell(col, row) {
                    if cell.flags.contains(CellFlags::SELECTED) {
                        let x = pad.x + col as f32 * cw;
                        let y = grid_pad_y + row as f32 * ch;
                        rects.push(Rect {
                            x,
                            y,
                            w: cw,
                            h: ch,
                            color: snap.selection_color.to_linear_f32(),
                        });
                    }
                }
            }
        }

        // Exit-code stripe in the left gutter + a hairline rule between blocks.
        let stripe = snap.block_stripe.to_linear_f32();
        let sep = snap.block_separator.to_linear_f32();
        let stripe_x = (2.0 * self.scale).min(pad.x);
        let stripe_w = (3.0 * self.scale).max(2.0);
        let sep_h = self.scale.max(1.0);
        // Inset the divider from the window edges so it reads as a subtle rule
        // between blocks rather than a full-bleed bar.
        let sep_inset = pad.x.min(width * 0.5).floor();
        let sep_w = (width - 2.0 * sep_inset).max(0.0);
        for (row, decor) in snap.rows_decor.iter().enumerate() {
            let y = grid_pad_y + row as f32 * ch;
            // A failed row always belongs to a real block, but guard on
            // `block_id` so the intent — never stripe blank padding — is explicit.
            if decor.failed && decor.block_id != 0 {
                rects.push(Rect { x: stripe_x, y, w: stripe_w, h: ch, color: stripe });
            }
            if separator_above(&snap.rows_decor, row) {
                rects.push(Rect { x: sep_inset, y, w: sep_w, h: sep_h, color: sep });
            }
        }

        // Sticky command header: an opaque band over row 0 with a bottom rule.
        // An open overlay covers the same rows, so skip it then.
        if let Some(sticky) = snap.sticky.as_ref().filter(|_| !overlay_open) {
            let tb = self.titlebar_height();
            let band_h = pad.y + ch;
            rects.push(Rect { x: 0.0, y: tb, w: width, h: band_h, color: default_bg.to_linear_f32() });
            rects.push(Rect { x: 0.0, y: tb + band_h - sep_h, w: width, h: sep_h, color: sep });
            if sticky.failed {
                rects.push(Rect { x: stripe_x, y: tb, w: stripe_w, h: band_h, color: stripe });
            }
        }

        if let Some(cur) = snap.cursor {
            let x = pad.x + cur.col as f32 * cw;
            let y = grid_pad_y + cur.row as f32 * ch;
            let color = cur.color.to_linear_f32();
            match cur.shape {
                CursorShape::Block => rects.push(Rect { x, y, w: cw, h: ch, color }),
                CursorShape::Beam => {
                    rects.push(Rect { x, y, w: (cw * 0.12).max(1.0), h: ch, color })
                }
                CursorShape::Underline => {
                    let th = (ch * 0.12).max(1.0);
                    rects.push(Rect { x, y: y + ch - th, w: cw, h: th, color });
                }
                CursorShape::Hidden => {}
            }
        }

        // Custom titlebar strip drawn on top (winit decorations are disabled, so
        // this is shelvd's own window chrome); the grid is inset below it.
        let tb = self.titlebar_height();
        rects.push(Rect { x: 0.0, y: 0.0, w: width, h: tb, color: self.titlebar_bg.to_linear_f32() });
        rects.push(Rect { x: 0.0, y: tb - sep_h, w: width, h: sep_h, color: sep });
        self.push_titlebar_buttons(&mut rects, width, tb);

        rects
    }

    fn build_text(&mut self, snap: &GridSnapshot, blank_rows: u16) {
        struct Run {
            start: usize,
            end: usize,
            fg: Rgba,
            bold: bool,
            italic: bool,
        }

        let cursor_block = snap.cursor.filter(|c| c.shape == CursorShape::Block);
        let mut text = String::with_capacity((snap.cols as usize + 1) * snap.rows as usize);
        let mut runs: Vec<Run> = Vec::new();
        let fallback = CellSnapshot::blank(self.default_fg, snap.background);

        for row in 0..snap.rows {
            for col in 0..snap.cols {
                let cell = snap.cell(col, row).copied().unwrap_or(fallback);
                let mut fg = cell.fg;
                if let Some(cur) = cursor_block {
                    if cur.col == col && cur.row == row {
                        fg = cur.text_color;
                    }
                }
                let bold = cell.flags.contains(CellFlags::BOLD);
                let italic = cell.flags.contains(CellFlags::ITALIC);
                // Rows under the overlay panel / sticky header are blanked so
                // their grid glyphs don't draw under the panel text.
                let ch = if row < blank_rows || cell.c == '\0' {
                    ' '
                } else {
                    cell.c
                };
                let start = text.len();
                text.push(ch);
                let end = text.len();
                match runs.last_mut() {
                    Some(r)
                        if r.fg == fg && r.bold == bold && r.italic == italic && r.end == start =>
                    {
                        r.end = end;
                    }
                    _ => runs.push(Run { start, end, fg, bold, italic }),
                }
            }
            let start = text.len();
            text.push('\n');
            runs.push(Run {
                start,
                end: text.len(),
                fg: self.default_fg,
                bold: false,
                italic: false,
            });
        }

        let family = self.font_family.as_deref();
        let spans = runs
            .iter()
            .map(|r| (&text[r.start..r.end], cell_attrs(family, r.fg, r.bold, r.italic)));
        let default_attrs = cell_attrs(family, self.default_fg, false, false);
        self.buffer.set_rich_text(
            &mut self.font_system,
            spans,
            &default_attrs,
            Shaping::Advanced,
            None,
        );
        self.buffer.shape_until_scroll(&mut self.font_system, false);
    }

    /// Compute the overlay panel's row count. The app has already windowed the
    /// items to what fits ([`overlay_capacity`](Self::overlay_capacity)), so the
    /// panel is simply the query row plus the visible items.
    fn overlay_layout(&self, ov: &Overlay) -> OverlayLayout {
        OverlayLayout { panel_rows: 1 + ov.items.len() as u16 }
    }

    /// Append the overlay's solid quads: panel, selection highlight, query
    /// cursor, bottom rule.
    fn append_overlay_rects(&self, rects: &mut Vec<Rect>, ov: &Overlay, layout: OverlayLayout) {
        let pad = self.padding_physical();
        let (cw, ch) = (self.cell.width, self.cell.height);
        let width = self.config.width as f32;
        let c = &ov.colors;

        let tb = self.titlebar_height();
        let panel_h = pad.y + layout.panel_rows as f32 * ch;
        rects.push(Rect { x: 0.0, y: tb, w: width, h: panel_h, color: c.panel_bg.to_linear_f32() });

        if let Some(sv) = ov.selected_visible {
            let row = 1 + sv as u16;
            let y = tb + pad.y + row as f32 * ch;
            rects.push(Rect { x: 0.0, y, w: width, h: ch, color: c.sel_bg.to_linear_f32() });
        }

        // A thin accent bar where the next typed character will land.
        let cursor_col = ov.prompt.chars().count() + 1 + ov.query.chars().count();
        let cx = pad.x + cursor_col as f32 * cw;
        rects.push(Rect { x: cx, y: tb + pad.y, w: (2.0 * self.scale).max(1.0), h: ch, color: c.accent.to_linear_f32() });

        let rule_h = self.scale.max(1.0);
        rects.push(Rect { x: 0.0, y: tb + panel_h - rule_h, w: width, h: rule_h, color: c.accent.to_linear_f32() });
    }

    /// Lay out the overlay's text (query line + the pre-windowed items) into the
    /// overlay buffer as colored spans.
    fn set_overlay_text(&mut self, ov: &Overlay) {
        let c = ov.colors;
        let mut text = String::new();
        let mut spans: Vec<(usize, usize, Rgba, bool)> = Vec::new();

        push_span(&mut text, &mut spans, &ov.prompt, c.accent, true);
        push_span(&mut text, &mut spans, " ", c.fg, false);
        if ov.query.is_empty() {
            push_span(&mut text, &mut spans, "type to search…", c.dim, false);
        } else {
            push_span(&mut text, &mut spans, &ov.query, c.fg, false);
        }

        for (idx, item) in ov.items.iter().enumerate() {
            push_span(&mut text, &mut spans, "\n", c.fg, false);
            let selected = Some(idx) == ov.selected_visible;
            push_span(&mut text, &mut spans, "  ", c.fg, false);
            push_span(&mut text, &mut spans, &item.label, c.fg, selected);
            if let Some(detail) = &item.detail {
                push_span(&mut text, &mut spans, "  ", c.dim, false);
                push_span(&mut text, &mut spans, detail, c.dim, false);
            }
        }

        let family = self.font_family.as_deref();
        let default_attrs = cell_attrs(family, c.fg, false, false);
        let spans_iter = spans
            .iter()
            .map(|(s, e, col, bold)| (&text[*s..*e], cell_attrs(family, *col, *bold, false)));
        self.overlay_buffer.set_rich_text(
            &mut self.font_system,
            spans_iter,
            &default_attrs,
            Shaping::Advanced,
            None,
        );
        self.overlay_buffer.shape_until_scroll(&mut self.font_system, false);
    }
}

/// Geometry of the overlay panel for one frame.
#[derive(Clone, Copy)]
struct OverlayLayout {
    /// Grid rows the panel occupies (1 query row + the pre-windowed items).
    panel_rows: u16,
}

/// Append a styled run to `text`/`spans`, skipping empty segments.
fn push_span(text: &mut String, spans: &mut Vec<(usize, usize, Rgba, bool)>, s: &str, color: Rgba, bold: bool) {
    if s.is_empty() {
        return;
    }
    let start = text.len();
    text.push_str(s);
    spans.push((start, text.len(), color, bold));
}

fn metrics_for(font_size_logical: f32, line_height_factor: f32, scale: f32) -> Metrics {
    let font_size = (font_size_logical * scale).max(1.0);
    let line_height = (font_size_logical * line_height_factor * scale).max(1.0);
    Metrics::new(font_size, line_height)
}

fn base_attrs(family: Option<&str>) -> Attrs<'_> {
    match family {
        Some(name) => Attrs::new().family(Family::Name(name)),
        None => Attrs::new().family(Family::Monospace),
    }
}

fn cell_attrs(family: Option<&str>, fg: Rgba, bold: bool, italic: bool) -> Attrs<'_> {
    let mut attrs = base_attrs(family).color(to_gcolor(fg));
    if bold {
        attrs = attrs.weight(Weight::BOLD);
    }
    if italic {
        attrs = attrs.style(Style::Italic);
    }
    attrs
}

fn measure_cell(font_system: &mut FontSystem, metrics: Metrics, family: Option<&str>) -> CellMetrics {
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(font_system, None, None);
    let attrs = base_attrs(family);
    buffer.set_text(font_system, "MMMMMMMMMM", &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);

    let (mut total, mut count) = (0.0f32, 0u32);
    for run in buffer.layout_runs() {
        for glyph in run.glyphs.iter() {
            total += glyph.w;
            count += 1;
        }
    }
    let width = if count > 0 {
        total / count as f32
    } else {
        metrics.font_size * 0.6
    };
    CellMetrics::new(width.max(1.0), metrics.line_height)
}

/// One full-window text area at the grid's text origin — the layout shared by
/// the grid, sticky-header, and overlay text layers.
fn text_area(buffer: &Buffer, color: GColor, pad: Padding, bounds: TextBounds) -> TextArea<'_> {
    TextArea {
        buffer,
        left: pad.x,
        top: pad.y,
        scale: 1.0,
        bounds,
        default_color: color,
        custom_glyphs: &[],
    }
}

/// `top` at `alpha`/255 composited over `bottom` — a quick opaque blend, used for
/// the titlebar tints.
fn lift(top: Rgba, bottom: Rgba, alpha: u8) -> Rgba {
    Rgba::new(top.r, top.g, top.b, alpha).over(bottom)
}

/// Which window edge/corner a pixel falls in, given a hit margin `m`. Corners
/// take precedence over edges; the interior is `None`.
fn edge_at(x: f32, y: f32, w: f32, h: f32, m: f32) -> Option<ResizeEdge> {
    let (left, right) = (x <= m, x >= w - m);
    let (top, bottom) = (y <= m, y >= h - m);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::NorthWest,
        (true, _, _, true) => ResizeEdge::NorthEast,
        (_, true, true, _) => ResizeEdge::SouthWest,
        (_, true, _, true) => ResizeEdge::SouthEast,
        (true, _, _, _) => ResizeEdge::North,
        (_, true, _, _) => ResizeEdge::South,
        (_, _, true, _) => ResizeEdge::West,
        (_, _, _, true) => ResizeEdge::East,
        _ => return None,
    })
}

fn to_gcolor(c: Rgba) -> GColor {
    GColor::rgba(c.r, c.g, c.b, c.a)
}

fn clear_color(c: Rgba) -> wgpu::Color {
    let [r, g, b, a] = c.to_linear_f64();
    wgpu::Color { r, g, b, a }
}

/// Whether to draw a block-separator rule above `row`.
///
/// Drawn at the top of every command block (a `block_top` row), so each block —
/// including the live prompt's, which sits below the bottom-anchor padding — has
/// a consistent top delimiter from the moment its prompt appears, rather than the
/// rule popping in only once a second block exists. Skipped on row 0, where there
/// is nothing above to divide it from.
fn separator_above(rows_decor: &[RowDecor], row: usize) -> bool {
    row > 0 && rows_decor[row].block_top
}

#[cfg(test)]
mod tests {
    use super::{edge_at, separator_above, ResizeEdge, RowDecor};

    fn block_top(block_id: u32) -> RowDecor {
        RowDecor { block_id, failed: false, block_top: true }
    }

    fn body(block_id: u32) -> RowDecor {
        RowDecor { block_id, failed: false, block_top: false }
    }

    #[test]
    fn separator_tops_every_block_below_row_zero() {
        // Layout: two blank top-padding rows, then block 1, then block 2.
        let rows = [
            RowDecor::default(), // 0: blank padding
            RowDecor::default(), // 1: blank padding
            block_top(1),        // 2: first visible block — sits below padding
            body(1),             // 3
            block_top(2),        // 4: next block — sits below a real block
            body(2),             // 5
        ];

        // Row 0 can never have a separator above it.
        assert!(!separator_above(&rows, 0));
        // The first/current block is delimited even though only blank padding
        // sits above it, so a fresh prompt carries its top rule from the start.
        assert!(separator_above(&rows, 2));
        // A non-`block_top` body row: no rule.
        assert!(!separator_above(&rows, 3));
        // A later block's top is delimited from the block above it.
        assert!(separator_above(&rows, 4));
        assert!(!separator_above(&rows, 5));
    }

    #[test]
    fn resize_edges_hit_corners_then_sides_then_interior() {
        // 100x80 window, 6px hit margin.
        let e = |x, y| edge_at(x, y, 100.0, 80.0, 6.0);
        assert_eq!(e(1.0, 1.0), Some(ResizeEdge::NorthWest));
        assert_eq!(e(99.0, 1.0), Some(ResizeEdge::NorthEast));
        assert_eq!(e(1.0, 79.0), Some(ResizeEdge::SouthWest));
        assert_eq!(e(99.0, 79.0), Some(ResizeEdge::SouthEast));
        assert_eq!(e(50.0, 1.0), Some(ResizeEdge::North));
        assert_eq!(e(50.0, 79.0), Some(ResizeEdge::South));
        assert_eq!(e(1.0, 40.0), Some(ResizeEdge::West));
        assert_eq!(e(99.0, 40.0), Some(ResizeEdge::East));
        assert_eq!(e(50.0, 40.0), None);
    }
}
