# shelvd shell integration

These scripts make your shell emit [OSC 133 semantic-prompt][osc133] markers and
[OSC 7][osc7] working-directory reports. shelvd reads them to group each command
with its output into a **block** — the unit it can navigate, copy, and decorate
with an exit-code stripe.

Without integration shelvd is still a normal terminal; blocks just don't appear.

## Install

Source the script for your shell from its startup file. Each script is inert
outside an interactive shelvd session (it checks `$TERM_PROGRAM`), so an
unconditional source is safe.

| Shell | Startup file | Line to add |
| ----- | ------------ | ----------- |
| bash  | `~/.bashrc`  | `source /path/to/shelvd/assets/shell-integration/shelvd.bash` |
| zsh   | `~/.zshrc`   | `source /path/to/shelvd/assets/shell-integration/shelvd.zsh`  |
| fish  | `~/.config/fish/config.fish` | `source /path/to/shelvd/assets/shell-integration/shelvd.fish` |

`TERM_PROGRAM=shelvd` is exported into every child shell automatically, so the
guard works with no extra setup.

## What it emits

Around each command line:

| Marker        | Meaning                              | When |
| ------------- | ------------------------------------ | ---- |
| `OSC 133 ; A` | prompt start                         | before the prompt is drawn |
| `OSC 133 ; B` | prompt end / command input begins    | end of `PS1` |
| `OSC 133 ; C` | command submitted, output begins     | preexec |
| `OSC 133 ; D;n` | command finished with exit code `n`| precmd / postexec |
| `OSC 7 ; file://host/path` | working directory       | each prompt |

All markers are terminated with ST (`ESC \`) so they never ring the bell.

## Notes

- **bash** approximates `preexec` with a `DEBUG` trap. The prompt hook runs as a
  function, so its own output is not mistaken for a command. A pre-existing
  `PROMPT_COMMAND` is preserved and run first. (Trap tracing into functions,
  `set -T`, is not supported and may produce a spurious output marker.)
- **zsh** and **fish** use native hooks, so the mapping is exact.

[osc133]: https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md
[osc7]: https://gitlab.freedesktop.org/terminal-wg/specifications/-/issues/20
