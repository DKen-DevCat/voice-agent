# voice-agent shell integration. ZDOTDIR points here so zsh reads these
# files; each one sources the user's own file first, then restores our ZDOTDIR.
__va_zdotdir="$ZDOTDIR"
ZDOTDIR="${VA_USER_ZDOTDIR:-$HOME}"
[[ -f "$ZDOTDIR/.zshenv" ]] && builtin source "$ZDOTDIR/.zshenv"
VA_USER_ZDOTDIR="$ZDOTDIR"
ZDOTDIR="$__va_zdotdir"
