ZDOTDIR="$VA_USER_ZDOTDIR"
[[ -f "$ZDOTDIR/.zprofile" ]] && builtin source "$ZDOTDIR/.zprofile"
ZDOTDIR="$__va_zdotdir"
