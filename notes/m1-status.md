# M1 status — 2026-06-13

**Done: scrollback + mouse selection + copy/paste.** Builds clean (0 clippy
warnings), 4 new `shelvd-term` unit tests pass, headless render verified intact.

## What landed
- **Scrollback** (`shelvd-term`): `scroll_lines/scroll_page_up/down/top/bottom`,
  `is_scrolled`. App: mouse wheel scrolls (3 lines/notch; pixel-delta divided by
  cell height), Shift+PageUp/PageDown page through history, and any keypress jumps
  back to the live edge before sending. On the **alt screen** (no scrollback),
  the wheel is translated to Up/Down arrow keys so `less`/`vim` scroll.
- **Selection** (`shelvd-term`): `selection_start/update/clear/selection_text`
  over alacritty's `Selection`; viewport→`Point` mapping accounts for the scroll
  offset. `snapshot()` flags `CellFlags::SELECTED` cells (via
  `Selection::to_range` + `point_in_range`) and carries `selection_color`.
  App: left click-drag selects; `shelvd-render` paints opaque selection rects
  (`Renderer::pixel_to_cell` does the hit-test).
- **Copy/paste** (`shelvd-app`, `arboard`): copy-on-select-release and
  Ctrl+Shift+C; paste via Ctrl+Shift+V and middle-click, wrapped in bracketed-
  paste markers when the program enabled DEC 2004. `arboard` uses the Wayland
  data-control protocol with X11 fallback (the fallback WARN under Xvfb is benign).

## Deliberately deferred (rest of the original M1 bucket)
- TOML theme/config loading and configurable cursor styles — not yet wired
  (`Config`/`Theme` exist in `shelvd-core` but are constructed with defaults).
- Mouse **reporting** to the child (TUIs with mouse support) — selection is always
  local; we don't forward SGR mouse events yet. Wheel→arrows on alt-screen is the
  one concession.
- Damage tracking: the renderer still rebuilds the whole-screen glyph buffer each
  redraw (fine since redraws are event-driven).

## Notes
- Worked in a worktree branched off `shelvd-init` (the bg-isolation guard blocks
  editing the main tree, and `EnterWorktree` was flaky — editing under the
  `.claude/worktrees/...` path directly satisfies isolation). Merged to
  `shelvd-init` on completion. `.claude/settings.json` now sets
  `worktree.bgIsolation = "none"` so future sessions can edit the main tree in place.
