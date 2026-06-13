# shelvd — pure-Rust GPU terminal

> *shelved* (a decommissioned relic) + *shell*. The rustpunk workspace's terminal.

## Project Identity

`shelvd` is a GPU-accelerated, block-aware terminal emulator in the spirit of
Warp. Hard requirements: **as pure-Rust as possible** (no system webview — this
ruled out Dioxus) and **Linux/macOS/Windows as first-class citizens**.

## Architecture

Five-crate workspace; dependency edges point only where needed:

- **`shelvd-core`** — shared vocabulary: `Rgba`, `Palette` (256-color + fg/bg/cursor),
  `GridSnapshot`/`CellSnapshot`/`CursorSnapshot`, `Theme`, `Config`, geometry
  (`GridSize`/`PixelSize`/`CellMetrics`/`Padding`). Depends on nothing else.
- **`shelvd-pty`** — `Pty::spawn` runs the shell behind `portable-pty`; a reader
  thread pushes `PtyMsg::Output` to a `flume` channel and calls a `notify`
  closure to wake the loop. No windowing dep.
- **`shelvd-term`** — wraps `alacritty_terminal`: owns `Term` + `vte` parser,
  `process(&[u8])` feeds bytes, **resolves colors** (named/indexed/spec, inverse,
  bold-brighten) into a `GridSnapshot`. Side effects (PtyWrite, Title, Bell, Exit)
  surface as `TermEvent`s on a channel.
- **`shelvd-render`** — `wgpu` + `glyphon`. `rect.rs` draws solid quads (cell
  backgrounds, cursor); glyphon draws glyphs. Consumes a `GridSnapshot`; depends
  only on `shelvd-core` (knows nothing about PTYs or alacritty).
- **`shelvd-app`** — binary `shelvd`. winit `ApplicationHandler` event loop wiring
  pty ↔ term ↔ render; maps keystrokes → byte sequences.

**The load-bearing seam:** color resolution lives in `shelvd-term`, so the
renderer is a dumb painter of `Rgba` cells. Keep it that way.

## Tech Stack (locked, mutually version-compatible)

`winit` 0.30 · `wgpu` 29 · `glyphon` 0.11 (→ cosmic-text 0.18, rustybuzz/swash —
no FreeType/HarfBuzz) · `alacritty_terminal` 0.26 (→ `vte` 0.15) · `portable-pty`
0.9. glyphon 0.11 pins `wgpu ^29`; the trio shares `raw-window-handle` 0.6.

## Critical Conventions

- `thiserror` in library crates; `anyhow` only in the binary. No `unwrap()` in libs.
- Workspace-level `[workspace.dependencies]`; members use `dep.workspace = true`.
- MIT OR Apache-2.0; repo `github.com/rustpunk/shelvd`.
- Colors stored as sRGB bytes (`Rgba`); convert to **linear** for the rect shader
  (the `*_srgb` surface re-applies the transfer) and pass sRGB bytes straight to
  glyphon. `to_linear_f32`/`to_linear_f64` exist for this.
- Renderer is event-driven: `ControlFlow::Wait` + `request_redraw()` on PTY output,
  input, and resize. No busy 60fps loop.
- Prefer fixing clippy findings over `#[allow]` (rustpunk rigor policy).

## Build / Test / Lint (the gauntlet)

```bash
cargo build --workspace
cargo clippy --workspace --all-targets   # must be zero warnings
cargo test --workspace
cargo run --release                       # opens a window
```

### Headless verification (no display)

```bash
xvfb-run -a -s "-screen 0 1280x800x24" \
  env WAYLAND_DISPLAY= WGPU_BACKEND=vulkan \
      VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  timeout 6 ./target/debug/shelvd
# exit 124 (timed out) with no panic == success. Screenshot the Xvfb root with
# `import -window root shot.png` while it runs to eyeball rendering.
```

## API gotchas (these crates churn — verified against the locked versions)

- **wgpu 29** evolved a lot vs older mental models:
  - `Instance::new(InstanceDescriptor::new_without_display_handle())` (no `Default`).
  - `request_adapter`/`request_device` return `Result` (not `Option`); `request_device`
    takes one arg; `DeviceDescriptor` derives `Default` with `trace: Trace::Off`.
  - `get_current_texture()` returns a `CurrentSurfaceTexture` **enum**
    (`Success`/`Suboptimal`/`Timeout`/`Occluded`/`Outdated`/`Lost`/`Validation`) —
    there is no `SurfaceError`.
  - `PipelineLayoutDescriptor`: `bind_group_layouts: &[Option<&BindGroupLayout>]`,
    field `immediate_size: u32` (push-constant ranges are gone).
  - `RenderPipelineDescriptor` has `multiview_mask: Option<NonZeroU32>` + `cache`.
  - `RenderPassColorAttachment` has `depth_slice: Option<u32>`;
    `RenderPassDescriptor` has `multiview_mask`.
  - `Arc<Window>.clone()` → `Surface<'static>` (wgpu holds the Arc).
  - `RenderPass` resource setters clone internally — no lifetime coupling needed.
- **cosmic-text 0.18**: `Attrs::new()` then `.color()/.family()/.weight()/.style()`;
  `Color::rgba(r,g,b,a)`; `Metrics::new(font_size, line_height)`;
  `set_text(fs, text, &attrs, shaping, Option<Align>)` (5 args);
  `set_rich_text(fs, spans, &default_attrs, shaping, align)`; `LayoutGlyph.w` is the
  advance (used to measure the monospace cell).
- **alacritty 0.26**: `Config::default()` exists; parser is
  `alacritty_terminal::vte::ansi::Processor` with `advance(&mut term, &[u8])`; the
  real `TermSize` is test-gated so we define our own `Dimensions` impl; cells expose
  `c/fg/bg/flags`; `Color = Named(NamedColor)|Spec(Rgb)|Indexed(u8)`;
  `grid().display_iter()` yields `Indexed<&Cell>`, cursor at `grid().cursor.point`.

## Roadmap

M1 scrollback/selection/copy-paste/config · M2 OSC-133 command blocks · M3 palette/editor/ligatures.
