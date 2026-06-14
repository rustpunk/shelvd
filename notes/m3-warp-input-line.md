# M3 — persistent anchored input line (#7), Full Warp model

Started 2026-06-14. Goal chosen by the project owner: the **Full Warp model** —
a persistent input line pinned at the bottom that output never moves, with the
command-output region above it scrolling top-to-bottom. Kills the prompt-climb
([[prompt-climb-single-grid]]).

## Empirically observed current behavior (driven headlessly, xdotool)

- **At rest:** shell PS1 is bottom-anchored via `display_shift()` — prompt rests at
  the window bottom. (#7 acceptance 3 already met.)
- **Command running, screen not yet full (fill phase):** the command block +
  output is *bottom-anchored* and **climbs upward** one row per output line until
  the screen fills. This is the un-Warp phase #7 targets.
- **Screen full:** scrolls cleanly — running command becomes a bold sticky header,
  output scrolls, the shelvd input band stays pinned at the bottom.
- The **input band** (shelvd-owned, type-ahead ghost + queue, from #19's first
  slice) is already pinned at the bottom *while a command runs*.

Root cause: on a single grid the idle prompt rests at the *bottom*, so when a
command runs there is no room *below* it — output must push it up. You cannot get
{bottom-anchored idle prompt + no jump + no climb} together on one grid. The clean
fix needs the input line to be its own region.

## Echo-suppression probe (decides the implementation strategy)

Tested (python pty, `bash --noprofile --norc -i`): disabling termios `ECHO` does
**not** cleanly stop bash from echoing the command line — readline manages its own
echo and the line still leaks into the output. So "suppress the shell's echo"
cannot lean on termios; it would require fully bypassing readline (a custom read
loop), which forfeits history / completion / line-editing unless reimplemented.

## Two implementation strategies

### Approach O — shelvd owns the editor, suppresses echo (literal #19)
shelvd captures keystrokes locally, renders the input itself, and prevents the
shell from echoing. Per the probe this needs a readline bypass (e.g. the shell
runs a `read`-driven loop, or we drive a line editor entirely shelvd-side).
- Pro: total control; the natural home for ghost-text / shelvd history / multiline.
- Con: high effort + risk; must reimplement history, completion, signals/job
  control interactions. This *is* the #19 deep epic.

### Approach P — passthrough + relocate the active input line (recommended)
Keep the shell's readline (free history/completion/editing). Suppress only the
*visible* prompt prefix (empty PS1, keep the OSC-133 markers). shelvd identifies
the **active input region** — the live-edge line(s) from prompt-end (OSC 133;B) to
the cursor while no command is running — and **relocates** it into the pinned
bottom band, **excluding** those rows from the output region. The output region is
then plain top-to-bottom (no bottom-anchor), so the climb disappears; at rest the
output region is short/empty at the top and the band sits at the bottom (matches
today's idle look).
- Pro: lower risk; reuses readline; same visible UX; no echo fight.
- Con: must track the active input region precisely (multi-line continuation,
  completion menus readline paints below the line, redraw on resize).
- Evolves toward O later: ghost-text/history become an enhancement layer over the
  relocated band without reworking the anchor model.

**Recommendation: Approach P** as the foundation; revisit O's richer editor under
#19 once the pinned-band layout is solid.

## CORRECTION (2026-06-14, after first review)

The first cut **top-anchored** the output region (clamped `display_shift <= 0`),
which removed the climb but left a **gap**: short at-rest output floated at the top
with empty space down to the pinned prompt. The owner wants it to "work like a
traditional terminal — fill bottom→up then scroll as it grows." The fix:
**keep the bottom-anchor** for the output (`display_shift_with` uses `.max(-band)`)
and rely on the **input-line relocation alone** to satisfy #7's "prompt holds
still." The relocated input sits in the pinned band; the output below hugs the
band, fills upward, and scrolls — exactly traditional behavior, no gap, and the
*input* never moves (the committed command line scrolls like any output, which the
owner confirmed is what they want). The fill-glide stays live (bottom-anchor is
back), so follow-up #27 is moot.

## Increment plan (Approach P) — each a gated PR

1. **Top-anchor the output region + always-present empty band.** Remove the
   at-rest bottom-anchor so output is conventional top-to-bottom; render a pinned
   (empty) input band at the bottom at rest too. Empty PS1 prefix (keep markers).
   At this point the shell's echoed input still shows on the grid's last line —
   acceptable intermediate, verified by screenshot.
2. **Relocate the active input line into the band.** Identify the active input
   region at the live edge (post-B, pre-C, command not running) and exclude it
   from the output snapshot; render its text + cursor in the band. Now the input
   visibly lives only in the pinned band; output scrolls above it; no climb.
3. **Polish:** multi-line continuation, completion-menu handling, resize redraw,
   cursor shape/blink in the band, selection interaction.

## Key code seams

- `crates/shelvd-term/src/lib.rs`: `display_shift()/display_shift_with()` (l.884),
  `content_bottom()` (l.941), `input_band_rows()` (l.916), `fill_input_band()`
  (l.1105), `snapshot()` live-edge path (l.965), `viewport_to_point()` (l.855),
  `command_running()` (l.784), `absolute_cursor_line()` (l.689).
- `crates/shelvd-app/src/main.rs`: KeyboardInput routing (l.574), `handle_band_key`
  (l.1007), `sync_band` (l.993), prompt/queue advance (l.1032).
- `assets/shell-integration/shelvd.bash`: PS1 + OSC-133 markers (l.64-77).
- `BandState` (term l.72), `BandInput` (app `overlay.rs`).

## Verification

Headless Xvfb + `lvp_icd` + xdotool drives commands and captures frames (see the
job tmp pattern). Gotchas learned this session: kill the binary with `pkill -x
shelvd` (NOT `pkill -f target/debug/shelvd` — matches the script's own cmdline and
self-terminates, exit 144); find the window with `xdotool search --class shelvd`
(not `--name`).
