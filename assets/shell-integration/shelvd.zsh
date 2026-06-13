# shelvd shell integration for zsh — OSC 133 command blocks + OSC 7 cwd.
#
# Source it from ~/.zshrc (inert outside shelvd / non-interactive shells):
#
#     source /path/to/shelvd/assets/shell-integration/shelvd.zsh
#
# zsh has native precmd/preexec hooks, so the mapping is exact:
#   precmd  -> D (exit code) + OSC 7 (cwd) + A (prompt start)
#   preexec -> C (command output starts)
#   PS1     -> B (prompt end) appended, zero-width via %{ %}

[[ -o interactive ]] || return 0
[[ "${TERM_PROGRAM:-}" == "shelvd" ]] || return 0
[[ -n "${__shelvd_installed:-}" ]] && return 0
typeset -g __shelvd_installed=1
typeset -g __shelvd_ran=""

autoload -Uz add-zsh-hook

__shelvd_precmd() {
    local ret=$?
    # D: the previous command finished (skipped on the first prompt / empty line).
    [[ -n "$__shelvd_ran" ]] && printf '\e]133;D;%s\e\\' "$ret"
    __shelvd_ran=""
    # OSC 7: working directory.
    printf '\e]7;file://%s%s\e\\' "${HOST}" "$PWD"
    # A: a fresh prompt is about to be drawn.
    printf '\e]133;A\e\\'
}

__shelvd_preexec() {
    # C: a command was submitted; its output begins now.
    __shelvd_ran=1
    printf '\e]133;C\e\\'
}

add-zsh-hook precmd __shelvd_precmd
add-zsh-hook preexec __shelvd_preexec

# B marks the end of the prompt. %{ %} tells zsh the bytes are zero-width.
PS1="${PS1}%{$(printf '\e]133;B\e\\')%}"
