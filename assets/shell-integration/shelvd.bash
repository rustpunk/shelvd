# shelvd shell integration for bash — OSC 133 command blocks + OSC 7 cwd.
#
# Source it from ~/.bashrc (it is inert outside shelvd and non-interactive
# shells, so an unconditional source is safe):
#
#     source /path/to/shelvd/assets/shell-integration/shelvd.bash
#
# It emits, around each command:
#   OSC 133 ; A    prompt start            OSC 133 ; C    command output starts
#   OSC 133 ; B    prompt end / input      OSC 133 ; D;n  command finished, exit n
#   OSC 7  ; url   working directory
# shelvd reads these to group each command with its output into a block.

# Only wire up inside an interactive shelvd session.
case "$-" in *i*) ;; *) return 0 2>/dev/null || true ;; esac
[ "${TERM_PROGRAM:-}" = "shelvd" ] || return 0 2>/dev/null || true
# Don't install twice.
[ -n "${__shelvd_installed:-}" ] && return 0
__shelvd_installed=1

# Tracks whether a command actually ran this cycle, so the exit-code report (D)
# is skipped on an empty prompt (just pressing Enter) and the first prompt.
__shelvd_ran=""
# Guards the per-command output marker (C) to once per command line.
__shelvd_armed=""

# Preserve any PROMPT_COMMAND the user already had; we run it ourselves.
__shelvd_orig_prompt_command="${PROMPT_COMMAND:-}"

# Runs before every prompt. Because it is a function, its body is not seen by
# the DEBUG trap (bash does not trace into functions unless `set -T`), so the
# markers it prints never get mistaken for a command.
__shelvd_prompt() {
    local ret=$?
    if [ -n "$__shelvd_orig_prompt_command" ]; then
        eval "$__shelvd_orig_prompt_command"
    fi
    # D: the previous command finished with status `ret`.
    if [ -n "$__shelvd_ran" ]; then
        printf '\033]133;D;%s\033\\' "$ret"
    fi
    __shelvd_ran=""
    __shelvd_armed=""
    # OSC 7: report the working directory.
    printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-}" "$PWD"
    # A: a fresh prompt is about to be drawn.
    printf '\033]133;A\033\\'
}

# DEBUG fires before each top-level command. The first firing after a prompt is
# the user's command; mark output start (C) once.
__shelvd_debug() {
    case "$BASH_COMMAND" in
        __shelvd_prompt) return ;;  # our own prompt hook
    esac
    [ -n "$COMP_LINE" ] && return   # tab completion, not a command
    [ -n "$__shelvd_armed" ] && return
    __shelvd_armed=1
    __shelvd_ran=1
    printf '\033]133;C\033\\'
}

trap '__shelvd_debug' DEBUG
PROMPT_COMMAND=__shelvd_prompt

# B marks the end of the prompt (start of command input). Embed it at the end of
# PS1, wrapped in \[ \] so bash keeps the prompt's display width correct.
__shelvd_b="$(printf '\033]133;B\033\\')"
PS1="${PS1}\[${__shelvd_b}\]"
unset __shelvd_b
