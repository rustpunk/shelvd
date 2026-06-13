//! `shelvd` — the terminal application.
//!
//! Owns the winit event loop and wires the three subsystems together:
//! the [`Pty`] feeds bytes to the [`Terminal`], which produces a snapshot the
//! [`Renderer`] draws; keystrokes are translated to byte sequences and written
//! back to the PTY. The PTY reader thread wakes the loop through an
//! [`EventLoopProxy`].

use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// How long each cursor blink phase (visible, then hidden) lasts.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

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
    /// Base button code (0/1/2) held while the program is reading mouse events,
    /// used to report drag motion and to anchor the matching release report.
    mouse_held: Option<u8>,
    /// Last grid cell a motion report was emitted for, so per-pixel motion
    /// collapses to one report per cell crossed.
    last_report_cell: (u16, u16),
    /// Whether the window currently has focus (the cursor only blinks focused).
    focused: bool,
    /// Current blink phase — `true` shows the cursor, `false` hides it.
    blink_on: bool,
    /// When the blink phase last toggled.
    last_blink: Instant,
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
            mouse_held: None,
            last_report_cell: (u16::MAX, u16::MAX),
            focused: true,
            blink_on: true,
            last_blink: Instant::now(),
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
                TermEvent::CursorBlink => {
                    // Blink config changed: show the cursor and restart the phase.
                    state.blink_on = true;
                    state.last_blink = Instant::now();
                    dirty = true;
                }
                // A command-block boundary moved; redraw so block visuals follow.
                TermEvent::SemanticPrompt { .. } => dirty = true,
                TermEvent::Bell | TermEvent::ClipboardStore(_) | TermEvent::Wakeup
                | TermEvent::MouseCursorDirty | TermEvent::WorkingDirectory(_) => {}
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
                // Forward motion to the child when it is reading the mouse (and
                // Shift, the local-selection override, is not held).
                if state.terminal.mouse_mode() && !state.modifiers.shift_key() {
                    let held = state.mouse_held;
                    let report = state.terminal.mouse_report_all_motion()
                        || (held.is_some() && state.terminal.mouse_report_drag());
                    if report {
                        let (col, row, _) =
                            state.renderer.pixel_to_cell(state.mouse_pos.0, state.mouse_pos.1);
                        if (col, row) != state.last_report_cell {
                            state.last_report_cell = (col, row);
                            report_mouse(state, MouseAction::Motion(held.unwrap_or(3)), col, row);
                        }
                    }
                    return;
                }
                if state.selecting {
                    let (col, row, right) =
                        state.renderer.pixel_to_cell(state.mouse_pos.0, state.mouse_pos.1);
                    state.terminal.selection_update(col, row, right);
                    state.window.request_redraw();
                }
            }

            WindowEvent::MouseInput { state: elem_state, button, .. } => {
                // Forward clicks to the child when it is reading the mouse; Shift
                // forces local selection/paste instead (the standard override).
                if state.terminal.mouse_mode() && !state.modifiers.shift_key() {
                    if let Some(base) = mouse_button_code(button) {
                        let (col, row, _) =
                            state.renderer.pixel_to_cell(state.mouse_pos.0, state.mouse_pos.1);
                        let action = match elem_state {
                            ElementState::Pressed => {
                                state.mouse_held = Some(base);
                                MouseAction::Press(base)
                            }
                            ElementState::Released => {
                                state.mouse_held = None;
                                MouseAction::Release(base)
                            }
                        };
                        report_mouse(state, action, col, row);
                    }
                    return;
                }
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
                // While the child reads the mouse, report wheel notches to it
                // (one report per notch). Shift falls through to local scrolling.
                if state.terminal.mouse_mode() && !state.modifiers.shift_key() {
                    let notches = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                        MouseScrollDelta::PixelDelta(p) => {
                            let ch = state.renderer.cell_metrics().height.max(1.0);
                            (p.y as f32 / ch).round() as i32
                        }
                    };
                    if notches != 0 {
                        let (col, row, _) =
                            state.renderer.pixel_to_cell(state.mouse_pos.0, state.mouse_pos.1);
                        let up = notches > 0;
                        for _ in 0..notches.unsigned_abs() {
                            report_mouse(state, MouseAction::Wheel(up), col, row);
                        }
                    }
                    return;
                }
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

            WindowEvent::Focused(focused) => {
                state.focused = focused;
                // Reset to a solid, visible cursor whenever focus changes.
                state.blink_on = true;
                state.last_blink = Instant::now();
                state.window.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                let mut snapshot = state.terminal.snapshot();
                // Honor the blink phase: while focused and the program asked for
                // a blinking cursor, drop the cursor on the "off" phase.
                if state.focused && state.terminal.cursor_blinking() && !state.blink_on {
                    snapshot.cursor = None;
                }
                if let Err(e) = state.renderer.render(&snapshot) {
                    log::error!("render error: {e}");
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.focused && state.terminal.cursor_blinking() {
            let now = Instant::now();
            if now.duration_since(state.last_blink) >= CURSOR_BLINK_INTERVAL {
                state.blink_on = !state.blink_on;
                state.last_blink = now;
                state.window.request_redraw();
            }
            // Wake again at the next toggle; this is the only thing that turns
            // the otherwise event-driven loop into a ~2 Hz tick, and only while
            // a blinking cursor is focused.
            event_loop
                .set_control_flow(ControlFlow::WaitUntil(state.last_blink + CURSOR_BLINK_INTERVAL));
        } else {
            // Not blinking: make sure the cursor is solid, then idle the loop.
            if !state.blink_on {
                state.blink_on = true;
                state.window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Wait);
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

/// A pointer action to report to a program that is reading the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseAction {
    /// Button pressed; carries the base button code (0 left, 1 middle, 2 right).
    Press(u8),
    /// Button released; carries the base button code.
    Release(u8),
    /// Pointer moved; carries the held button code, or 3 when no button is down.
    Motion(u8),
    /// Wheel turned; `true` is a scroll-up notch.
    Wheel(bool),
}

/// Map a winit mouse button to its base report code, or `None` for buttons the
/// X11 mouse protocol cannot encode.
fn mouse_button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}

/// Encode a mouse action as the escape sequence a program expects: SGR
/// (DEC 1006) when `sgr` is set, otherwise the legacy X10 byte encoding.
/// `col`/`row` are 0-based grid cells; the wire protocol is 1-based.
fn mouse_report(sgr: bool, action: MouseAction, col: u16, row: u16, mods: ModifiersState) -> Vec<u8> {
    let mod_bits = (if mods.shift_key() { 4 } else { 0 })
        + (if mods.alt_key() { 8 } else { 0 })
        + (if mods.control_key() { 16 } else { 0 });
    let (base, motion, released) = match action {
        MouseAction::Press(b) => (b as u32, false, false),
        MouseAction::Release(b) => (b as u32, false, true),
        MouseAction::Motion(b) => (b as u32, true, false),
        MouseAction::Wheel(up) => (if up { 64 } else { 65 }, false, false),
    };
    let col1 = col as u32 + 1;
    let row1 = row as u32 + 1;

    if sgr {
        let cb = base + mod_bits + if motion { 32 } else { 0 };
        let last = if released { 'm' } else { 'M' };
        format!("\x1b[<{cb};{col1};{row1}{last}").into_bytes()
    } else {
        // Legacy encoding cannot say which button was released, so it reports 3.
        let base = if released { 3 } else { base };
        let cb = base + mod_bits + if motion { 32 } else { 0 };
        // Each field is offset by 32; values past 223 are unencodable, so clamp.
        let enc = |v: u32| (v + 32).min(255) as u8;
        vec![0x1b, b'[', b'M', enc(cb), enc(col1), enc(row1)]
    }
}

/// Encode `action` for the active mouse mode and write it to the child.
fn report_mouse(state: &mut State, action: MouseAction, col: u16, row: u16) {
    let bytes = mouse_report(state.terminal.sgr_mouse(), action, col, row, state.modifiers);
    if let Err(e) = state.pty.write(&bytes) {
        log::debug!("mouse report write failed: {e}");
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

#[cfg(test)]
mod tests {
    use super::{mouse_report, MouseAction};
    use winit::keyboard::ModifiersState;

    fn none() -> ModifiersState {
        ModifiersState::empty()
    }

    #[test]
    fn sgr_press_and_release_keep_the_button() {
        assert_eq!(mouse_report(true, MouseAction::Press(0), 0, 0, none()), b"\x1b[<0;1;1M".to_vec());
        // Release uses a lowercase final byte but reports the same button.
        assert_eq!(mouse_report(true, MouseAction::Release(0), 0, 0, none()), b"\x1b[<0;1;1m".to_vec());
    }

    #[test]
    fn sgr_wheel_and_motion_set_their_bits() {
        // Wheel-up is button 64; coordinates are 1-based.
        assert_eq!(mouse_report(true, MouseAction::Wheel(true), 5, 2, none()), b"\x1b[<64;6;3M".to_vec());
        // Motion adds the 32 motion bit to the held button (0 -> 32).
        assert_eq!(mouse_report(true, MouseAction::Motion(0), 3, 1, none()), b"\x1b[<32;4;2M".to_vec());
    }

    #[test]
    fn legacy_offsets_every_field_by_32() {
        // Press left at the origin: button 0 -> 32 (space), col/row 1 -> 33 ('!').
        assert_eq!(mouse_report(false, MouseAction::Press(0), 0, 0, none()), vec![0x1b, b'[', b'M', 32, 33, 33]);
        // Legacy release cannot name the button, so it reports 3 (32 + 3 = 35).
        assert_eq!(mouse_report(false, MouseAction::Release(0), 0, 0, none()), vec![0x1b, b'[', b'M', 35, 33, 33]);
    }

    #[test]
    fn ctrl_modifier_sets_the_control_bit() {
        // Ctrl adds 16, so a left press becomes button 16 in SGR.
        assert_eq!(
            mouse_report(true, MouseAction::Press(0), 0, 0, ModifiersState::CONTROL),
            b"\x1b[<16;1;1M".to_vec()
        );
    }
}
