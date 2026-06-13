# M0 status — 2026-06-13

**Done: a working basic terminal.** Window opens, spawns `$SHELL`, parses VT/ANSI,
renders the grid (truecolor + warm-dark theme + amber block cursor), keyboard
input forwarded to the PTY, resize wired through render → term → pty. Builds with
zero clippy warnings. Verified headless under Xvfb + lavapipe (see CLAUDE.md
recipe) — screenshot in `docs/screenshot.png` shows the live bash prompt.

## What's wired end-to-end
- `shelvd-app` winit loop: `resumed` creates window + `Renderer` + `Terminal` +
  `Pty`; PTY reader thread wakes the loop via `EventLoopProxy<UserEvent::PtyReadable>`.
- `user_event` drains PTY bytes → `terminal.process` → drains `TermEvent`s
  (PtyWrite back to pty, Title → window title, Exit → quit) → `request_redraw`.
- `RedrawRequested` → `terminal.snapshot()` → `renderer.render(&snap)`.
- Key mapping in `key_to_bytes`/`control_byte` (named keys, Ctrl-combos, Alt-ESC).

## Known shortcuts to revisit in M1+
- Text rebuilds the whole-screen glyphon `Buffer` every redraw (fine because
  redraws are event-driven, but add damage tracking when blocks land).
- Scrollback view: `display_offset` is read but there's no wheel/key scrolling yet.
- Mouse: no selection / click reporting yet.
- Font: uses system `Family::Monospace`; bundle a font for reproducibility (M1).
- Callback-carrying alacritty events (ColorRequest, TextAreaSizeRequest,
  ClipboardLoad) are dropped in `EventProxy` — wire them when needed.
- DIM and Dim* named colors fall back to default fg (M1: real dimming).

## Gotchas already paid for
See CLAUDE.md "API gotchas" — wgpu 29 `CurrentSurfaceTexture` enum,
`immediate_size`, `multiview_mask`, `depth_slice`; cosmic-text 0.18 `set_text`
5-arg; alacritty `TermSize` is test-gated (we roll our own `Dimensions`).

## Location note
Canonical tree is `~/code/rustpunk/shelvd` (sibling of ferrule/hasp/etc.). This
session was started from a git worktree for edit isolation but all files live at
the real path. (Earlier working name was `glimrot`, renamed to `shelvd`.)
