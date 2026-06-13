# shelvd shell integration for fish — OSC 133 command blocks + OSC 7 cwd.
#
# Source it from ~/.config/fish/config.fish (inert outside shelvd /
# non-interactive shells):
#
#     source /path/to/shelvd/assets/shell-integration/shelvd.fish
#
# fish has fish_preexec / fish_postexec events for C / D, and the prompt is
# wrapped to emit A (start) and B (end) around the user's prompt text.

status is-interactive; or return 0
test "$TERM_PROGRAM" = "shelvd"; or return 0
set -q __shelvd_installed; and return 0
set -g __shelvd_installed 1

function __shelvd_preexec --on-event fish_preexec
    # C: a command was submitted; its output begins now.
    printf '\e]133;C\e\\'
end

function __shelvd_postexec --on-event fish_postexec
    # D: the command finished; report its exit status (captured first).
    set -l ret $status
    printf '\e]133;D;%s\e\\' $ret
end

# Wrap the user's fish_prompt so A + OSC 7 print before it and B after it.
if not functions -q __shelvd_user_prompt
    functions -c fish_prompt __shelvd_user_prompt
end

function fish_prompt
    # A: a fresh prompt is about to be drawn.
    printf '\e]133;A\e\\'
    # OSC 7: working directory.
    printf '\e]7;file://%s%s\e\\' (hostname) "$PWD"
    __shelvd_user_prompt
    # B: end of the prompt; command input begins.
    printf '\e]133;B\e\\'
end
