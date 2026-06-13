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
    CellFlags, CellMetrics, CellSnapshot, CursorShape, GridSize, GridSnapshot, Padding, PixelSize,
    Rgba, Theme,
};

use rect::{Rect, RectRenderer};

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

    rects: RectRenderer,

    default_fg: Rgba,
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
            rects,
            default_fg: theme.palette.foreground,
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
        }
    }

    /// The grid size (in cells) that fits the current surface.
    pub fn grid_size(&self) -> GridSize {
        GridSize::from_pixels(
            PixelSize::new(self.config.width, self.config.height),
            self.cell,
            self.padding_physical(),
        )
    }

    /// Current per-cell pixel metrics.
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell
    }

    /// Draw one frame from `snap`.
    pub fn render(&mut self, snap: &GridSnapshot) -> Result<(), RenderError> {
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

        let rects = self.build_rects(snap);
        self.rects.upload(&self.device, &self.queue, &rects);

        self.build_text(snap);

        self.viewport.update(
            &self.queue,
            Resolution { width: self.config.width, height: self.config.height },
        );
        let pad = self.padding_physical();
        let text_area = TextArea {
            buffer: &self.buffer,
            left: pad.x,
            top: pad.y,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: self.config.width as i32,
                bottom: self.config.height as i32,
            },
            default_color: to_gcolor(self.default_fg),
            custom_glyphs: &[],
        };
        self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            [text_area],
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
        let row = (((y - pad.y) / ch).floor()).clamp(0.0, (grid.rows - 1) as f32) as u16;
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

    fn build_rects(&self, snap: &GridSnapshot) -> Vec<Rect> {
        let pad = self.padding_physical();
        let (cw, ch) = (self.cell.width, self.cell.height);
        let mut rects = Vec::new();
        let default_bg = snap.background;

        for row in 0..snap.rows {
            for col in 0..snap.cols {
                if let Some(cell) = snap.cell(col, row) {
                    let x = pad.x + col as f32 * cw;
                    let y = pad.y + row as f32 * ch;
                    if cell.bg != default_bg {
                        rects.push(Rect { x, y, w: cw, h: ch, color: cell.bg.to_linear_f32() });
                    }
                    if cell.flags.contains(CellFlags::SELECTED) {
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

        if let Some(cur) = snap.cursor {
            let x = pad.x + cur.col as f32 * cw;
            let y = pad.y + cur.row as f32 * ch;
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
        rects
    }

    fn build_text(&mut self, snap: &GridSnapshot) {
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
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
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

fn to_gcolor(c: Rgba) -> GColor {
    GColor::rgba(c.r, c.g, c.b, c.a)
}

fn clear_color(c: Rgba) -> wgpu::Color {
    let [r, g, b, a] = c.to_linear_f64();
    wgpu::Color { r, g, b, a }
}
