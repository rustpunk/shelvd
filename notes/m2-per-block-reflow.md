# M2 #17 — per-block reflow on resize

Issue #17 (epic #12, item 5). Goal: on resize, re-wrap each frozen block's
**logical** lines to the new width independently, so block scrollback survives
reflow instead of being discarded.

## The problem (why resize currently clears everything)

`Terminal::resize` today does `self.blocks.clear(); self.frozen.clear()`
(`lib.rs`). It throws away all OSC-133 block history on every resize.

It does that because the composite scroll model (#15) addresses frozen blocks
through the **live grid's absolute-line space**: `composite_row_into(abs)` finds
the block at `abs` via `block_row` (which uses `Block::prompt_line`), and for a
finished block maps `abs` into the frozen buffer bottom-aligned to
`Block::end_line`. alacritty's reflow renumbers the whole grid + history, so those
abs anchors go stale — and there's no reliable way to recover where the old OSC
marks landed in the reflowed grid. Clearing was the safe-but-lossy escape.

A naive "reflow frozen + restack below abs_base" reintroduces a **double-render**:
finished-block tails that are still on screen (in the live grid's active region)
would render once from the live grid and once from the restacked frozen buffer.

## Decision (bounded, correct for the common case)

1. **`FrozenBlock::reflow(new_cols)`** in `shelvd-core` — re-wrap `logical_lines`
   (width-independent source of truth) into fresh `cells` at `new_cols`. Wide
   glyphs (a `WIDE` cell + its trailing `WIDE_SPACER`) never straddle a wrap. Pad
   each visual row to width with a stored default `blank` cell; an empty logical
   line still occupies one blank row. `logical_lines` are untouched.

2. **Re-anchor on resize instead of clearing.** After alacritty reflows the live
   grid, reflow each frozen buffer, then rebuild the index-aligned `Block` anchors
   so the frozen stack is contiguous and ends just above the **open block's
   prompt line**:
   - Resting prompt (not `command_running`): the prompt sits on the cursor's grid
     line, so pin `open.prompt_line = abs_base + cursor.line`. The frozen stack
     ends at `open.prompt_line - 1`, covering the on-screen finished tails — which
     therefore render *from frozen* (shadowing the live grid), so no double-render.
   - Running command / no open block: pin to `abs_base` (stack ends at
     `abs_base - 1`, in history). On-screen finished tails during a running
     command are uncommon (the command's output usually fills the screen), so the
     residual double-render risk is the documented edge case.
   - Guard: if the `blocks`/`frozen` index-alignment invariant is violated, fall
     back to the old clear-both behavior.

3. **`composite_oldest_abs`** = the first block's `prompt_line` when blocks exist
   (frozen is authoritative), else `grid_oldest`. Prevents scrolling into the raw
   alacritty history that sits below the re-anchored frozen stack (it holds the
   same blocks at the wrong positions). Minor cost: pre-first-block content
   (shell banner) is no longer reachable in scrollback once blocks exist.

4. **`prune_blocks`** exempts frozen-backed blocks from the history-floor prune
   (still bounded by `MAX_BLOCKS`), so reflowed scrollback genuinely outlives the
   live grid's own scrollback — finally making the `composite_oldest_abs` comment
   ("frozen buffers can outlive the grid's own scrollback") true. The history-
   shrink/clear (RIS) path is unchanged and still wipes everything in lockstep.

## Known limitation (documented, matches existing single-grid tradeoffs)

Resizing **while a command is running** and then scrolling back can't precisely
place the running command's already-scrolled output (its prompt position in the
reflowed grid is unrecoverable). Full fidelity needs the per-block-grid renderer
(epic #12 item 4) where the live grid holds only the active block. Out of scope
for #17.

## Tests

- `frozen.rs`: reflow narrow→wide, wide→narrow, round-trip preserves logical
  text; wide-glyph pair stays intact across a wrap; empty logical line → blank row.
- `lib.rs`: replace `resize_drops_frozen_blocks_in_lockstep` with
  `resize_reflows_frozen_blocks`; add a scroll-back-after-resize round-trip;
  `clearing_history_drops_frozen_blocks_in_lockstep` stays green.
