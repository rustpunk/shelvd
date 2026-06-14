//! `shelvd` — the terminal application.
//!
//! Owns the winit event loop and wires the three subsystems together:
//! the [`Pty`] feeds bytes to the [`Terminal`], which produces a snapshot the
//! [`Renderer`] draws; keystrokes are translated to byte sequences and written
//! back to the PTY. The PTY reader thread wakes the loop through an
//! [`EventLoopProxy`].

mod overlay;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use shelvd_core::{Config, OverlayColors, ResizeEdge, Rgba, TitlebarHit};
use shelvd_pty::{Pty, PtyMsg, PtyOptions, PtySize};
use shelvd_render::Renderer;
use shelvd_term::{BandState, SemanticKind, TermEvent, Terminal};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{ResizeDirection, Window, WindowId};

use overlay::{key_to_action, Action, BandInput, OverlayState};

/// Events delivered to the loop from other threads.
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    /// The PTY reader thread has new output (or the child exited).
    PtyReadable,
}

/// How long each cursor blink phase (visible, then hidden) lasts.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Duration of the smooth "fill transition": when a burst of output makes the
/// bottom-anchor reserve shrink by several rows at once, the grid glides up to
/// its new position over this window instead of jumping.
const FILL_ANIM: Duration = Duration::from_millis(120);

/// Frame cadence while the fill transition is animating (~60fps).
const FILL_ANIM_TICK: Duration = Duration::from_millis(16);

/// Smallest anchor-shift decrease (in rows) that triggers the glide. Single-line
/// scrolls (Δ == 1) stay instant so ordinary typing feels snappy.
const FILL_ANIM_MIN_ROWS: u16 = 2;

/// After a resize / scale change, suppress the fill glide for this long. The shell
/// answers the resize's SIGWINCH by redrawing its prompt; that echo must not be
/// mistaken for output filling the screen and kick off a spurious downward glide.
const GLIDE_RESIZE_COOLDOWN: Duration = Duration::from_millis(150);

/// Two titlebar presses within this window count as a double-click (maximize).
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Eased remaining grid offset for the fill transition: starts at `from_px`
/// (t = 0, content held where it was) and eases out to 0 (t >= 1, content
/// settled at the anchored position). Cubic ease-out keeps it snappy.
fn fill_anim_offset(from_px: f32, t: f32) -> f32 {
    if t >= 1.0 {
        return 0.0;
    }
    let t = t.max(0.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    from_px * (1.0 - eased)
}

/// Progress of the fill transition in [0, 1] given when it started.
fn elapsed_t(started: Instant, now: Instant) -> f32 {
    (now.duration_since(started).as_secs_f32() / FILL_ANIM.as_secs_f32()).clamp(0.0, 1.0)
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
    /// The open command palette / history overlay, if any. While present, the
    /// keyboard drives the overlay instead of the PTY.
    overlay: Option<OverlayState>,
    /// The bottom band's input line — what the user is typing while a command
    /// runs. Sent to the running command on Enter, or queued on Ctrl+Shift+Enter.
    input: BandInput,
    /// Commands queued ahead, flushed one at a time to the PTY on each new shell
    /// prompt (OSC 133;A).
    queue: VecDeque<String>,
    /// Overlay colors resolved from the theme at startup.
    overlay_colors: OverlayColors,
    /// Current downward pixel offset of the grid layer for the fill transition
    /// (0 == idle). Eases from `anim_from_px` to 0 over [`FILL_ANIM`].
    anim_offset_px: f32,
    /// The offset the current glide started from.
    anim_from_px: f32,
    /// When the current glide started.
    anim_started: Instant,
    /// Anchor shift (top-reserved blank rows) observed after the last PTY chunk,
    /// used to detect the burst-shrink that triggers a glide.
    prev_anchor_shift: u16,
    /// When the window geometry (size / scale) last changed. The fill glide is
    /// held off for [`GLIDE_RESIZE_COOLDOWN`] afterward so the shell's SIGWINCH
    /// prompt-redraw can't be mistaken for output and trigger a spurious glide.
    last_geometry_change: Instant,
    /// Timestamp of the last titlebar press, for double-click-to-maximize.
    last_titlebar_press: Option<Instant>,
    /// Which window button the pointer is hovering (for the highlight).
    hovered_button: Option<TitlebarHit>,
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
            .with_inner_size(LogicalSize::new(960.0, 600.0))
            .with_min_inner_size(LogicalSize::new(360.0, 240.0))
            // winit's client-side titlebar (sctk-adwaita) mis-accounts its own
            // height on GNOME / Pop!_OS: every interactive move round-trips a
            // configure that subtracts the ~36px titlebar again, so the window
            // sheds a row on each drag. Until that's fixed upstream (winit is
            // version-locked) or shelvd draws its own titlebar, run undecorated
            // and move via the compositor gesture (Super+drag on Pop!_OS).
            .with_decorations(false);
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
        let terminal_anchor = terminal.anchor_shift();

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
            overlay: None,
            input: BandInput::default(),
            queue: VecDeque::new(),
            overlay_colors: overlay_colors(&self.config),
            anim_offset_px: 0.0,
            anim_from_px: 0.0,
            anim_started: Instant::now(),
            prev_anchor_shift: terminal_anchor,
            last_geometry_change: Instant::now(),
            last_titlebar_press: None,
            hovered_button: None,
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
                // The OS taskbar/overview gets the program's title; the drawn
                // titlebar keeps shelvd's own name rather than echoing the shell.
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
                // A fresh prompt (133;A) is the cue to advance the type-ahead queue.
                TermEvent::SemanticPrompt { kind, .. } => {
                    if kind == SemanticKind::PromptStart {
                        on_prompt_start(state);
                    }
                    dirty = true;
                }
                TermEvent::Bell | TermEvent::ClipboardStore(_) | TermEvent::Wakeup
                | TermEvent::MouseCursorDirty | TermEvent::WorkingDirectory(_) => {}
            }
        }

        if dirty {
            // Fill transition: if a burst of output shrank the bottom-anchor
            // reserve by several rows at once, glide the grid up instead of
            // letting it jump. Only at the live edge of the main screen, and not
            // within the cooldown right after a resize (there the shrink would be
            // the shell's SIGWINCH prompt-redraw, not genuine output filling in).
            let shift = state.terminal.anchor_shift();
            let now = Instant::now();
            let cooled =
                now.duration_since(state.last_geometry_change) >= GLIDE_RESIZE_COOLDOWN;
            if cooled
                && shift < state.prev_anchor_shift
                && !state.terminal.alt_screen()
                && !state.terminal.is_scrolled()
            {
                let delta = state.prev_anchor_shift - shift;
                if delta >= FILL_ANIM_MIN_ROWS {
                    let cell_h = state.renderer.cell_metrics().height;
                    // Accumulate onto any offset still in flight so back-to-back
                    // bursts compound smoothly rather than snapping.
                    let remaining = fill_anim_offset(
                        state.anim_from_px,
                        elapsed_t(state.anim_started, now),
                    );
                    let max_px = state.window.inner_size().height as f32;
                    let from = (delta as f32 * cell_h + remaining).min(max_px);
                    state.anim_from_px = from;
                    state.anim_offset_px = from;
                    state.anim_started = now;
                }
            }
            state.prev_anchor_shift = shift;

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
                // Wayland delivers a redundant same-size configure when the window
                // is *moved* (drag-and-drop). Re-running the resize would fire a
                // needless pty SIGWINCH whose prompt-redraw echo can spuriously
                // start the fill glide — the grid slides down ("the bottom grows")
                // then eases back. Ignore configures that change nothing.
                if (size.width, size.height) == state.renderer.surface_size()
                    && (scale - state.renderer.scale()).abs() <= f32::EPSILON
                {
                    return;
                }
                state.renderer.resize(size.width, size.height, scale);
                let grid = state.renderer.grid_size();
                state.terminal.resize(grid.cols, grid.rows);
                let _ = state.pty.resize(PtySize {
                    rows: grid.rows,
                    cols: grid.cols,
                    pixel_width: size.width as u16,
                    pixel_height: size.height as u16,
                });
                // A resize re-lays-out the grid; cancel any glide and re-baseline
                // the anchor so the new layout never triggers a spurious one.
                cancel_fill_anim(state);
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
                cancel_fill_anim(state);
                state.window.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                state.mouse_pos = (position.x as f32, position.y as f32);
                update_titlebar_hover(state);
                if state.overlay.is_some() {
                    return; // overlay swallows pointer motion
                }
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
                // A click anywhere dismisses an open overlay.
                if state.overlay.is_some() {
                    if elem_state == ElementState::Pressed {
                        state.overlay = None;
                        state.window.request_redraw();
                    }
                    return;
                }
                // Window chrome wins over the child and local selection: a press
                // on a resize edge or the titlebar drives the compositor (winit
                // decorations are off, so shelvd owns the move/resize).
                if button == MouseButton::Left && elem_state == ElementState::Pressed {
                    let (mx, my) = state.mouse_pos;
                    if let Some(edge) = state.renderer.resize_edge(mx, my) {
                        let _ = state.window.drag_resize_window(to_resize_dir(edge));
                        return;
                    }
                    if let Some(hit) = state.renderer.titlebar_hit(mx, my) {
                        match hit {
                            TitlebarHit::Drag => handle_titlebar_press(state),
                            TitlebarHit::Minimize => state.window.set_minimized(true),
                            TitlebarHit::Maximize => {
                                let maximized = state.window.is_maximized();
                                state.window.set_maximized(!maximized);
                            }
                            TitlebarHit::Close => event_loop.exit(),
                        }
                        return;
                    }
                }
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
                if state.overlay.is_some() {
                    // Scroll the overlay list instead of the terminal, honoring
                    // the delta magnitude the same way the terminal-scroll path
                    // below does (so a fast flick moves several rows, not one).
                    // Positive `notches` means scrolling up, which moves the
                    // highlight up — hence the negated step.
                    let notches = match delta {
                        MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i32,
                        MouseScrollDelta::PixelDelta(p) => {
                            let ch = state.renderer.cell_metrics().height.max(1.0);
                            (p.y as f32 / ch).round() as i32
                        }
                    };
                    if notches != 0 {
                        if let Some(ov) = state.overlay.as_mut() {
                            ov.move_selection(-notches);
                            state.window.request_redraw();
                        }
                    }
                    return;
                }
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
                // While an overlay is open the keyboard drives it, not the PTY.
                if state.overlay.is_some() {
                    handle_overlay_key(state, event_loop, &event);
                    return;
                }
                let mods = state.modifiers;
                // A bound key chord — command palette, history, block jumps,
                // clipboard, paging — runs its action; everything else is input.
                // The keymap and the palette share one command table (see
                // `overlay`). Ctrl+C is unbound here, so it still sends SIGINT.
                if let Some(action) = key_to_action(&event, mods) {
                    run_action(state, event_loop, action);
                    state.window.request_redraw();
                    return;
                }
                // While a command runs, the bottom band is the live input field:
                // typing edits it (Enter sends it to the running command, while
                // Ctrl+Shift+Enter — handled above — queues it) instead of leaking
                // raw into the output. Control/Alt combos still pass straight
                // through, so Ctrl+C and friends can signal the command. The alt
                // screen is exempt — full-screen apps own all input there.
                if state.terminal.command_running()
                    && !state.terminal.alt_screen()
                    && !mods.control_key()
                    && !mods.alt_key()
                {
                    handle_band_key(state, &event);
                    return;
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
                // With an overlay open, focus is on it — hide the grid cursor.
                if state.overlay.is_some() {
                    snapshot.cursor = None;
                }
                let capacity = state.renderer.overlay_capacity();
                let overlay = state
                    .overlay
                    .as_ref()
                    .map(|ov| ov.to_overlay(state.overlay_colors, capacity));
                if let Err(e) =
                    state.renderer.render(&snapshot, overlay.as_ref(), state.anim_offset_px)
                {
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
        let now = Instant::now();
        // The next instant the loop must wake on its own, if any. The fill
        // transition and the cursor blink each contribute one; we take the
        // soonest, and otherwise idle on `Wait`. This keeps the loop event-driven
        // — it only ticks while something is actively animating.
        let mut wake: Option<Instant> = None;

        // --- fill transition ---------------------------------------------------
        if state.anim_offset_px != 0.0 {
            let t = elapsed_t(state.anim_started, now);
            state.anim_offset_px = fill_anim_offset(state.anim_from_px, t);
            state.window.request_redraw();
            if t < 1.0 {
                wake = Some(now + FILL_ANIM_TICK);
            }
        }

        // --- cursor blink ------------------------------------------------------
        if state.focused && state.terminal.cursor_blinking() {
            if now.duration_since(state.last_blink) >= CURSOR_BLINK_INTERVAL {
                state.blink_on = !state.blink_on;
                state.last_blink = now;
                state.window.request_redraw();
            }
            let next_blink = state.last_blink + CURSOR_BLINK_INTERVAL;
            wake = Some(wake.map_or(next_blink, |w| w.min(next_blink)));
        } else if !state.blink_on {
            // Not blinking: make sure the cursor is solid again.
            state.blink_on = true;
            state.window.request_redraw();
        }

        match wake {
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Stop any in-flight fill transition and re-baseline the anchor to the current
/// layout, so the next layout change is measured from solid ground (used after a
/// resize / scale change, which re-lay out the whole grid).
fn cancel_fill_anim(state: &mut State) {
    state.anim_offset_px = 0.0;
    state.anim_from_px = 0.0;
    state.prev_anchor_shift = state.terminal.anchor_shift();
    state.last_geometry_change = Instant::now();
}

/// A left press on the titlebar: a quick second press toggles maximize,
/// otherwise it begins a compositor-driven window move.
fn handle_titlebar_press(state: &mut State) {
    let now = Instant::now();
    let double = state
        .last_titlebar_press
        .is_some_and(|t| now.duration_since(t) <= DOUBLE_CLICK);
    if double {
        state.last_titlebar_press = None;
        let maximized = state.window.is_maximized();
        state.window.set_maximized(!maximized);
    } else {
        state.last_titlebar_press = Some(now);
        let _ = state.window.drag_window();
    }
}

/// Map a core resize edge to winit's compositor resize direction.
fn to_resize_dir(edge: ResizeEdge) -> ResizeDirection {
    match edge {
        ResizeEdge::North => ResizeDirection::North,
        ResizeEdge::South => ResizeDirection::South,
        ResizeEdge::East => ResizeDirection::East,
        ResizeEdge::West => ResizeDirection::West,
        ResizeEdge::NorthEast => ResizeDirection::NorthEast,
        ResizeEdge::NorthWest => ResizeDirection::NorthWest,
        ResizeEdge::SouthEast => ResizeDirection::SouthEast,
        ResizeEdge::SouthWest => ResizeDirection::SouthWest,
    }
}

/// Refresh the hovered window button from the pointer position, redrawing only
/// on change so the hover highlight follows the cursor without churn.
fn update_titlebar_hover(state: &mut State) {
    let hit = state
        .renderer
        .titlebar_hit(state.mouse_pos.0, state.mouse_pos.1)
        .filter(|h| !matches!(h, TitlebarHit::Drag));
    if hit != state.hovered_button {
        state.hovered_button = hit;
        state.renderer.set_hovered_button(hit);
        state.window.request_redraw();
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

/// Copy the whole block at the top of the viewport to the system clipboard.
fn copy_block(state: &mut State) {
    let Some(text) = state.terminal.current_block_text() else {
        return;
    };
    if let Some(clipboard) = state.clipboard.as_mut() {
        if let Err(e) = clipboard.set_text(text) {
            log::debug!("block copy failed: {e}");
        }
    }
}

/// Route a key press to the open overlay: query edits, navigation, accept/cancel.
fn handle_overlay_key(state: &mut State, event_loop: &ActiveEventLoop, event: &KeyEvent) {
    // Escape and Enter act on the overlay slot itself (clear it / take it, then
    // run the chosen action), so they're handled before the shared binding.
    match &event.logical_key {
        Key::Named(NamedKey::Escape) => {
            state.overlay = None;
            state.window.request_redraw();
            return;
        }
        Key::Named(NamedKey::Enter) => {
            let action = state.overlay.take().and_then(|o| o.selected_action());
            if let Some(action) = action {
                run_action(state, event_loop, action);
            }
            state.window.request_redraw();
            return;
        }
        _ => {}
    }

    // Everything else edits the live overlay; bind it mutably just once.
    let Some(o) = state.overlay.as_mut() else {
        return;
    };
    match &event.logical_key {
        Key::Named(NamedKey::ArrowUp) => o.move_selection(-1),
        Key::Named(NamedKey::ArrowDown) => o.move_selection(1),
        Key::Named(NamedKey::Backspace) => o.backspace(),
        Key::Named(NamedKey::Space) => o.input_char(' '),
        Key::Character(s) => {
            for c in s.chars() {
                o.input_char(c);
            }
        }
        _ => {}
    }
    state.window.request_redraw();
}

/// Execute an overlay [`Action`] against the live subsystems.
fn run_action(state: &mut State, event_loop: &ActiveEventLoop, action: Action) {
    match action {
        Action::OpenPalette => {
            state.selecting = false; // drop any in-progress drag
            state.overlay = Some(OverlayState::palette());
        }
        Action::ScrollToTop => state.terminal.scroll_to_top(),
        Action::ScrollToBottom => state.terminal.scroll_to_bottom(),
        Action::CopySelection => copy_selection(state),
        Action::CopyBlock => copy_block(state),
        Action::Paste => paste_clipboard(state),
        Action::PrevBlock => {
            state.terminal.scroll_to_prev_block();
        }
        Action::NextBlock => {
            state.terminal.scroll_to_next_block();
        }
        Action::PageUp => state.terminal.scroll_page_up(),
        Action::PageDown => state.terminal.scroll_page_down(),
        Action::SearchHistory => open_history(state),
        Action::Quit => event_loop.exit(),
        Action::InsertCommand(cmd) => {
            // Jump to the live edge first (like normal key input) so the shell's
            // echo of the inserted command is on-screen, not below the viewport.
            state.terminal.scroll_to_bottom();
            if let Err(e) = state.pty.write(cmd.as_bytes()) {
                log::debug!("history insert failed: {e}");
            }
        }
        Action::QueueInput => {
            // Add the band's current input to the type-ahead queue, to run on the
            // next prompt. Meaningful while a command is running (that is when the
            // band is the input field); a no-op when the line is empty.
            let line = state.input.take();
            if !line.trim().is_empty() {
                state.queue.push_back(line);
            }
            sync_band(state);
        }
    }
}

/// Mirror the app's band input + queue into the terminal so it renders the input
/// line and the queued commands. Called after any change to either, before the
/// next redraw, so layout and rendering stay derived from one source.
fn sync_band(state: &mut State) {
    let band = BandState {
        input: state.input.text().to_owned(),
        queued: state.queue.iter().cloned().collect(),
        // Mask the input when the running command has turned echo off (a password
        // prompt); the real bytes are still what we send on Enter.
        masked: !state.pty.echo_enabled(),
    };
    state.terminal.set_band(band);
}

/// Route a key press to the band's input line while a command runs: edit the
/// text, or — on Enter — send it to the running command's stdin and clear it.
/// (Ctrl+Shift+Enter, handled earlier as [`Action::QueueInput`], queues instead.)
fn handle_band_key(state: &mut State, event: &KeyEvent) {
    match &event.logical_key {
        Key::Named(NamedKey::Enter) => {
            // Send the typed line to the running command's stdin, then clear it.
            let mut bytes = state.input.take().into_bytes();
            bytes.push(b'\r');
            if let Err(e) = state.pty.write(&bytes) {
                log::debug!("band send failed: {e}");
            }
        }
        Key::Named(NamedKey::Backspace) => state.input.backspace(),
        Key::Named(NamedKey::Space) => state.input.input_char(' '),
        Key::Character(s) => {
            for ch in s.chars() {
                state.input.input_char(ch);
            }
        }
        _ => {}
    }
    // Keep the band at the live edge so the input line stays on screen.
    state.terminal.scroll_to_bottom();
    sync_band(state);
    state.window.request_redraw();
}

/// Handle a fresh shell prompt (OSC 133;A): advance the type-ahead queue by one.
/// The queue runs on every prompt until drained. When nothing is queued but the
/// band still holds a half-typed line, that line is handed to the now-idle prompt
/// (no Enter, so it lands as editable input) rather than dropped.
fn on_prompt_start(state: &mut State) {
    if let Some(cmd) = state.queue.pop_front() {
        let mut bytes = cmd.into_bytes();
        bytes.push(b'\r'); // type the command and run it
        if let Err(e) = state.pty.write(&bytes) {
            log::debug!("queue flush failed: {e}");
        }
        sync_band(state);
        return;
    }
    if !state.input.is_empty() {
        let pending = state.input.take();
        if let Err(e) = state.pty.write(pending.as_bytes()) {
            log::debug!("band handoff failed: {e}");
        }
        sync_band(state);
    }
}

/// Open the history overlay from this session's block command strings.
fn open_history(state: &mut State) {
    let commands = history_commands(&state.terminal);
    state.selecting = false; // drop any in-progress drag
    state.overlay = Some(OverlayState::history(commands));
}

/// Recent command strings, most recent first, deduplicated.
fn history_commands(terminal: &Terminal) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for block in terminal.blocks().iter().rev() {
        if !block.command.is_empty() && seen.insert(block.command.as_str()) {
            out.push(block.command.clone());
        }
    }
    out
}

/// Resolve the overlay's colors from the active theme.
fn overlay_colors(config: &Config) -> OverlayColors {
    let pal = &config.theme.palette;
    let (bg, fg) = (pal.background, pal.foreground);
    OverlayColors {
        panel_bg: mix(fg, bg, 22),       // a subtle lift above the terminal background
        fg,
        dim: mix(fg, bg, 120),           // muted text for details / placeholder
        sel_bg: mix(pal.cursor, bg, 64), // warm highlight on the selected row
        accent: pal.cursor,
    }
}

/// `top` composited over `bottom` at `alpha`/255 — a quick opaque blend.
fn mix(top: Rgba, bottom: Rgba, alpha: u8) -> Rgba {
    Rgba::new(top.r, top.g, top.b, alpha).over(bottom)
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
    use super::{fill_anim_offset, mouse_report, MouseAction};
    use winit::keyboard::ModifiersState;

    #[test]
    fn fill_anim_offset_eases_from_full_to_zero() {
        let from = 48.0;
        // t = 0 holds the content where it was; t >= 1 settles at the anchor.
        assert_eq!(fill_anim_offset(from, 0.0), from);
        assert_eq!(fill_anim_offset(from, 1.0), 0.0);
        assert_eq!(fill_anim_offset(from, 1.5), 0.0, "past the end stays settled");
        // Negative time is clamped to the start.
        assert_eq!(fill_anim_offset(from, -0.5), from);

        // Strictly decreasing across the animation, and always within (0, from).
        let mut prev = from;
        for i in 1..10 {
            let t = i as f32 / 10.0;
            let cur = fill_anim_offset(from, t);
            assert!(cur < prev, "offset decreases monotonically: {cur} !< {prev}");
            assert!(cur >= 0.0 && cur <= from, "offset stays in range: {cur}");
            prev = cur;
        }
    }


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
