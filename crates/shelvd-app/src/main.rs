//! `shelvd` — the terminal application.
//!
//! Owns the winit event loop and wires the three subsystems together:
//! the [`Pty`] feeds bytes to the [`Terminal`], which produces a snapshot the
//! [`Renderer`] draws; keystrokes are translated to byte sequences and written
//! back to the PTY. The PTY reader thread wakes the loop through an
//! [`EventLoopProxy`].

use std::sync::Arc;

use arboard::Clipboard;
use shelvd_core::Config;
use shelvd_pty::{Pty, PtyMsg, PtyOptions, PtySize};
use shelvd_render::Renderer;
use shelvd_term::{TermEvent, Terminal};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// Events delivered to the loop from other threads.
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    /// The PTY reader thread has new output (or the child exited).
    PtyReadable,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let mut app = App::new(proxy, Config::load_default());
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    config: Config,
    state: Option<State>,
}

struct State {
    window: Arc<Window>,
    renderer: Renderer,
    terminal: Terminal,
    pty: Pty,
    modifiers: ModifiersState,
    /// System clipboard handle (`None` if it failed to initialize).
    clipboard: Option<Clipboard>,
    /// Last known pointer position in physical pixels.
    mouse_pos: (f32, f32),
    /// Whether the left button is down and a selection is being dragged.
    selecting: bool,
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>, config: Config) -> Self {
        Self { proxy, config, state: None }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return; // Already initialized (resumed can fire more than once).
        }

        let attributes = Window::default_attributes()
            .with_title("shelvd")
            .with_inner_size(LogicalSize::new(960.0, 600.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                log::error!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        let renderer = match Renderer::new(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            scale,
            &self.config.theme,
        ) {
            Ok(renderer) => renderer,
            Err(e) => {
                log::error!("failed to initialize renderer: {e}");
                event_loop.exit();
                return;
            }
        };

        let grid = renderer.grid_size();
        let terminal = Terminal::new(
            grid.cols,
            grid.rows,
            self.config.scrollback,
            self.config.theme.palette.clone(),
            self.config.theme.cursor_shape,
        );

        let proxy = self.proxy.clone();
        let pty_opts = PtyOptions {
            shell: self.config.shell.clone(),
            size: PtySize {
                rows: grid.rows,
                cols: grid.cols,
                pixel_width: size.width as u16,
                pixel_height: size.height as u16,
            },
            ..Default::default()
        };
        let pty = match Pty::spawn(pty_opts, move || {
            let _ = proxy.send_event(UserEvent::PtyReadable);
        }) {
            Ok(pty) => pty,
            Err(e) => {
                log::error!("failed to spawn shell: {e}");
                event_loop.exit();
                return;
            }
        };

        window.request_redraw();
        self.state = Some(State {
            window,
            renderer,
            terminal,
            pty,
            modifiers: ModifiersState::empty(),
            clipboard: Clipboard::new().ok(),
            mouse_pos: (0.0, 0.0),
            selecting: false,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::PtyReadable = event;
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let mut dirty = false;
        while let Ok(msg) = state.pty.receiver().try_recv() {
            match msg {
                PtyMsg::Output(bytes) => {
                    state.terminal.process(&bytes);
                    dirty = true;
                }
                PtyMsg::Exit => {
                    event_loop.exit();
                    return;
                }
            }
        }

        // Honor terminal-generated side effects (replies, title, exit).
        while let Ok(ev) = state.terminal.events().try_recv() {
            match ev {
                TermEvent::PtyWrite(bytes) => {
                    if let Err(e) = state.pty.write(&bytes) {
                        log::debug!("pty write failed: {e}");
                    }
                }
                TermEvent::Title(title) => state.window.set_title(&title),
                TermEvent::ResetTitle => state.window.set_title("shelvd"),
                TermEvent::Exit => {
                    event_loop.exit();
                    return;
                }
                TermEvent::Bell | TermEvent::ClipboardStore(_) | TermEvent::Wakeup
                | TermEvent::MouseCursorDirty => {}
            }
        }

        if dirty {
            state.window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::ModifiersChanged(modifiers) => {
                state.modifiers = modifiers.state();
            }

            WindowEvent::Resized(size) => {
                let scale = state.window.scale_factor() as f32;
                state.renderer.resize(size.width, size.height, scale);
                let grid = state.renderer.grid_size();
                state.terminal.resize(grid.cols, grid.rows);
                let _ = state.pty.resize(PtySize {
                    rows: grid.rows,
                    cols: grid.cols,
                    pixel_width: size.width as u16,
                    pixel_height: size.height as u16,
                });
                state.window.request_redraw();
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = state.window.inner_size();
                state.renderer.resize(size.width, size.height, scale_factor as f32);
                let grid = state.renderer.grid_size();
                state.terminal.resize(grid.cols, grid.rows);
                let _ = state.pty.resize(PtySize {
                    rows: grid.rows,
                    cols: grid.cols,
                    pixel_width: size.width as u16,
                    pixel_height: size.height as u16,
                });
                state.window.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_pos = (position.x as f32, position.y as f32);
                if state.selecting {
                    let (col, row, right) =
                        state.renderer.pixel_to_cell(state.mouse_pos.0, state.mouse_pos.1);
                    state.terminal.selection_update(col, row, right);
                    state.window.request_redraw();
                }
            }

            WindowEvent::MouseInput { state: elem_state, button, .. } => {
                match (button, elem_state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        let (col, row, right) =
                            state.renderer.pixel_to_cell(state.mouse_pos.0, state.mouse_pos.1);
                        state.terminal.selection_start(col, row, right);
                        state.selecting = true;
                        state.window.request_redraw();
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        state.selecting = false;
                        copy_selection(state); // copy-on-select
                    }
                    (MouseButton::Middle, ElementState::Pressed) => paste_clipboard(state),
                    _ => {}
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i32,
                    MouseScrollDelta::PixelDelta(p) => {
                        let ch = state.renderer.cell_metrics().height.max(1.0);
                        (p.y as f32 / ch).round() as i32
                    }
                };
                if lines != 0 {
                    if state.terminal.alt_screen() && !state.terminal.mouse_mode() {
                        // No scrollback on the alt screen: drive the app with arrows.
                        let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
                        for _ in 0..lines.unsigned_abs() {
                            let _ = state.pty.write(seq);
                        }
                    } else {
                        state.terminal.scroll_lines(lines);
                        state.window.request_redraw();
                    }
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let mods = state.modifiers;
                // Ctrl+Shift+C / Ctrl+Shift+V: clipboard. (Ctrl+C alone still sends SIGINT.)
                if mods.control_key() && mods.shift_key() {
                    if let Key::Character(s) = &event.logical_key {
                        if s.eq_ignore_ascii_case("c") {
                            copy_selection(state);
                            return;
                        }
                        if s.eq_ignore_ascii_case("v") {
                            paste_clipboard(state);
                            return;
                        }
                    }
                }
                // Shift+PageUp / Shift+PageDown: scroll the viewport through history.
                if mods.shift_key() {
                    if let Key::Named(named) = &event.logical_key {
                        match named {
                            NamedKey::PageUp => {
                                state.terminal.scroll_page_up();
                                state.window.request_redraw();
                                return;
                            }
                            NamedKey::PageDown => {
                                state.terminal.scroll_page_down();
                                state.window.request_redraw();
                                return;
                            }
                            _ => {}
                        }
                    }
                }
                // Normal input: jump back to the live edge, then send the bytes.
                if let Some(bytes) = key_to_bytes(&event, mods) {
                    state.terminal.scroll_to_bottom();
                    state.window.request_redraw();
                    if let Err(e) = state.pty.write(&bytes) {
                        log::debug!("pty write failed: {e}");
                    }
                }
            }

            WindowEvent::RedrawRequested => {
                let snapshot = state.terminal.snapshot();
                if let Err(e) = state.renderer.render(&snapshot) {
                    log::error!("render error: {e}");
                }
            }

            _ => {}
        }
    }
}

/// Translate a key press into the byte sequence a terminal expects.
fn key_to_bytes(event: &KeyEvent, mods: ModifiersState) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();

    let with_alt = |bytes: Vec<u8>| -> Option<Vec<u8>> {
        if alt && !bytes.is_empty() {
            let mut prefixed = Vec::with_capacity(bytes.len() + 1);
            prefixed.push(0x1b);
            prefixed.extend_from_slice(&bytes);
            Some(prefixed)
        } else {
            Some(bytes)
        }
    };

    match &event.logical_key {
        Key::Named(named) => {
            let bytes: Vec<u8> = match named {
                NamedKey::Enter => vec![b'\r'],
                NamedKey::Backspace => vec![0x7f],
                NamedKey::Tab if shift => b"\x1b[Z".to_vec(),
                NamedKey::Tab => vec![b'\t'],
                NamedKey::Escape => vec![0x1b],
                NamedKey::Space => vec![b' '],
                NamedKey::ArrowUp => b"\x1b[A".to_vec(),
                NamedKey::ArrowDown => b"\x1b[B".to_vec(),
                NamedKey::ArrowRight => b"\x1b[C".to_vec(),
                NamedKey::ArrowLeft => b"\x1b[D".to_vec(),
                NamedKey::Home => b"\x1b[H".to_vec(),
                NamedKey::End => b"\x1b[F".to_vec(),
                NamedKey::PageUp => b"\x1b[5~".to_vec(),
                NamedKey::PageDown => b"\x1b[6~".to_vec(),
                NamedKey::Delete => b"\x1b[3~".to_vec(),
                NamedKey::Insert => b"\x1b[2~".to_vec(),
                _ => return None,
            };
            with_alt(bytes)
        }
        Key::Character(s) => {
            if ctrl {
                let c = s.chars().next()?;
                return control_byte(c).map(|b| vec![b]);
            }
            let text = event
                .text
                .as_ref()
                .map(|t| t.as_bytes().to_vec())
                .unwrap_or_else(|| s.as_bytes().to_vec());
            with_alt(text)
        }
        _ => event.text.as_ref().map(|t| t.as_bytes().to_vec()),
    }
}

/// Map a character under Ctrl to its control byte (e.g. `c` → 0x03).
fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '?' => Some(0x1f),
        _ => None,
    }
}

/// Copy the current selection to the system clipboard, if it is non-empty.
fn copy_selection(state: &mut State) {
    let Some(text) = state.terminal.selection_text() else {
        return;
    };
    if let Some(clipboard) = state.clipboard.as_mut() {
        if let Err(e) = clipboard.set_text(text) {
            log::debug!("clipboard copy failed: {e}");
        }
    }
}

/// Paste clipboard text into the PTY.
fn paste_clipboard(state: &mut State) {
    let text = state
        .clipboard
        .as_mut()
        .and_then(|clipboard| clipboard.get_text().ok());
    if let Some(text) = text {
        paste_to_pty(state, &text);
    }
}

/// Write pasted text to the PTY, wrapping it in bracketed-paste markers when the
/// program has enabled that mode.
fn paste_to_pty(state: &mut State, text: &str) {
    let payload = if state.terminal.bracketed_paste() {
        let mut buf = Vec::with_capacity(text.len() + 12);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~");
        buf
    } else {
        text.as_bytes().to_vec()
    };
    if let Err(e) = state.pty.write(&payload) {
        log::debug!("paste write failed: {e}");
    }
}
