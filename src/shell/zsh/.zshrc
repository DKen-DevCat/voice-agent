ZDOTDIR="$VA_USER_ZDOTDIR"
# /etc/zshrc ran while ZDOTDIR pointed here; keep history in the user's usual file
[[ "$HISTFILE" == "$__va_zdotdir"/* ]] && HISTFILE="$ZDOTDIR/.zsh_history"
[[ -f "$ZDOTDIR/.zshrc" ]] && builtin source "$ZDOTDIR/.zshrc"
# ZDOTDIR stays on the user's dir from here, so zsh reads their .zlogin itself.
unset __va_zdotdir

# Nobody watches this shell: nothing may stop to ask or page
setopt RM_STAR_SILENT NO_CORRECT NO_CORRECT_ALL
export PAGER=cat GIT_PAGER=cat GIT_EDITOR=true EDITOR=true VISUAL=true

# OSC 633 markers are taken out of the stream by voice-agent. They tell it
# when a command starts (C), when it ends and with which status (D), the
# working directory (W), and when the prompt is back (A).
__va_precmd() {
    local ec=$?
    builtin printf '\e]633;D;%s\a' "$ec"
    builtin printf '\e]633;W;%s\a' "$PWD"
    builtin printf '\e]633;A\a'
}
__va_preexec() {
    builtin printf '\e]633;C\a'
}

# voice-agent runs exactly what it wrote to $VA_CMD_FILE. Builtin widgets
# (.accept-line) keep the user's own bindings (abbreviation expanders etc.)
# from rewriting the command.
__va_clear_widget() {
    BUFFER=""
    CURSOR=0
}
__va_run_widget() {
    BUFFER="$(<"$VA_CMD_FILE")"
    CURSOR=${#BUFFER}
    zle .accept-line
}
zle -N __va_clear_widget
zle -N __va_run_widget

for __va_km in emacs viins vicmd; do
    bindkey -M $__va_km '\e[5151~' __va_run_widget
    bindkey -M $__va_km '\e[5152~' __va_clear_widget
done
unset __va_km

# Our precmd must run first to see the command's real exit status
precmd_functions=(__va_precmd ${precmd_functions:#__va_precmd})
preexec_functions+=(__va_preexec)

# Marks the continuation prompt so voice-agent can tell an incomplete command
# (unterminated quote, open heredoc) from one that is still running
PS2=$'%{\e]633;F\a%}'"$PS2"
