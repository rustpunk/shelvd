# M2 status — 2026-06-13

**Done: mouse reporting (M1 remainder) + OSC-133 command blocks (M2).** Builds
clean (0 clippy warnings), 37 unit tests pass, block visuals screenshot-verified
under Xvfb. Landed on `main` as five commits (mouse reporting, OSC tee, shell
scripts, block model, visuals+nav).

## What landed

### Part A — mouse reporting to the child
- When a program enables mouse tracking (`TermMode::MOUSE_MODE`), the app forwards
  press/release/motion/wheel to the PTY instead of doing local selection; **Shift
  forces local selection** (the standard override). Encoder builds the bytes
  itself: **SGR** (`CSI < b ; col ; row M/m`) when `SGR_MOUSE`, else **legacy X10**
  (`CSI M` with each field +32). Motion reported only when asked (1003 always, or
  1002 with a button held), coalesced per cell; wheel = one report per notch.
  New `Terminal` accessors: `sgr_mouse/mouse_report_all_motion/mouse_report_drag`.

### M2 — command blocks
- **OSC-133 tee** (`shelvd-term/osc133.rs`): a stateful `Scanner` frames OSC
  sequences *before* alacritty (which drops 133), recognizing `133;A/B/C/D[;exit]`
  and `7;file://…` cwd, with a partial-sequence accumulator **across PTY reads**
  (the main risk; unit-tested byte-by-byte, mid-payload, and across the ESC/`\` of
  ST). `process()` feeds alacritty in segments split at each marker terminator,
  reading the cursor between to anchor to an **absolute** grid line.
- **abs_base**: absolute origin of active line 0, advanced by history growth.
  Exact until the scrollback buffer saturates; frozen across alt-screen swaps;
  blocks cleared on reflow/clear.  *Known limit:* after a session emits more than
  `scrollback` lines, anchors for off-screen history may drift (the single-grid
  tradeoff; Warp's per-block grids are deferred).
- **Block model** (`shelvd-term/block.rs`): `Block { lines, command, exit_code,
  state, cwd, started_at, output_excerpt }`, driven by the markers; command text
  captured B→C via `bounds_to_string`; pruned out of history + hard cap.
- **Snapshot metadata** (`shelvd-core`): per-row `RowDecor` (block id / failed /
  block-top), a `StickyHeader`, and resolved block colors — filled by
  `shelvd-term` so the renderer stays a dumb painter.
- **Visuals** (`shelvd-render`): red left **exit-code stripe** + subtle bg **tint**
  on failed blocks, **separators** above each block, and a **sticky command
  header** (second glyphon buffer) when a block's prompt scrolls off.
- **Navigation** (`shelvd-app`): Ctrl+Shift+Up/Down jump between block prompts,
  Ctrl+Shift+X copies the whole top block.
- **Shell integration** (`assets/shell-integration/`): zsh/bash/fish scripts emit
  the markers at precmd/preexec; bash verified end-to-end.

## Deferred / next (M3)
- Inter-block **whitespace** (needs row re-layout; we draw separator lines only).
- Per-block grids to survive reflow and remove the saturation drift.
- M3: command palette, history/Ctrl-R, static suggested-actions (rides on the
  block tuple `command/exit_code/output_excerpt` captured here), ghost-text.

## Env note
- Worked in worktree `.claude/worktrees/m2` on branch `m2-command-blocks` (the
  cwd-scoped bg-isolation guard rejected in-place edits; the workspace-root
  `bgIsolation = "none"` isn't read from the `shelvd/` cwd). Fast-forwarded `main`
  per piece. The worktree `target/` was repeatedly wiped between shell calls —
  rebuild before any headless run/screenshot.
