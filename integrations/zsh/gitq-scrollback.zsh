# gitq scrollback — zsh ZLE widgets.
#
# Two keystroke-bound widgets over the scrollback subsystem (see
# doc/scrollback.org).  Both require tmux (there is no portable way to read
# a pane's scrollback outside it) and fail loud via zle's echo-area, never
# silently:
#
#   \eb  ("browse")  → gitq-scrollback-browse-widget    (Meta-b)
#   \ee  ("emacs")   → gitq-scrollback-to-emacs-widget   (Meta-e)
#
# Both keys are plain `bindkey' bindings you can rebind freely.
#
# Install: `make install-zsh-scrollback` symlinks this next to _gitq and
# prints the one `source' line to add to ~/.zshrc.  Source it from shell
# startup (order doesn't matter — these are widgets, not completion).
#
# On boundary detection, and why there are no OSC-133 shell-integration
# hooks here:
#
#   The usual "exact" shell-integration approach emits invisible OSC-133
#   markers from precmd/preexec so a capture can find precise command and
#   output boundaries.  That does NOT work through tmux: tmux consumes
#   OSC-133 for its own bookkeeping and `capture-pane -e' reproduces only
#   SGR attributes and anchored OSC-8 hyperlinks from the grid — the 133
#   markers never come back (measured on tmux 3.5a; see doc/scrollback.org).
#   Emitting them anyway would imply an exactness gitq can't deliver here,
#   so this file ships no such hooks.  gitq splits scrollback by detecting
#   shell prompts instead — best-effort, and tunable for your prompt with:
#
#     export GITQ_SCROLLBACK_PROMPT_REGEX='^my-prompt-pattern '
#
#   (a POSIX ERE, matched against each ANSI-stripped line).

# --- \eb : browse scrollback in an interactive TUI --------------------------

gitq-scrollback-browse-widget() {
  if [[ -z "$TMUX" ]]; then
    zle -M "gitq: scrollback browsing needs tmux"
    return 1
  fi
  # Overlay the browser on top of the current pane with tmux's own popup
  # (tmux >= 3.2), so it behaves like a real UI layer instead of trashing
  # the current line's redraw.  -E closes the popup when gitq exits.
  zle -I
  tmux display-popup -E -w 90% -h 90% "gitq --scrollback-browse"
  zle reset-prompt
}
zle -N gitq-scrollback-browse-widget
bindkey '\eb' gitq-scrollback-browse-widget   # Meta-b — rebind freely

# --- \ee : send scrollback to Emacs -----------------------------------------

gitq-scrollback-to-emacs-widget() {
  if [[ -z "$TMUX" ]]; then
    zle -M "gitq: scrollback capture needs tmux"
    return 1
  fi
  if ! command -v emacsclient >/dev/null 2>&1; then
    zle -M "gitq: emacsclient not found"
    return 1
  fi
  local tmpfile
  tmpfile=$(mktemp "${TMPDIR:-/tmp}/gitq-scrollback.XXXXXX.el") || {
    zle -M "gitq: could not create temp file"
    return 1
  }
  # A temp file, not `emacsclient -e' with the payload inline: scrollback
  # sexp can be large and emacsclient's argv escaping/length gets fragile
  # past a few KB.  Emacs deletes the file after reading it
  # (gitq-scrollback-open-from-file).
  if ! gitq --scrollback --sexp > "$tmpfile" 2>/dev/null; then
    zle -M "gitq: scrollback capture failed"
    rm -f "$tmpfile"
    return 1
  fi
  if ! emacsclient -e "(gitq-scrollback-open-from-file \"$tmpfile\")" >/dev/null 2>&1; then
    zle -M "gitq: emacsclient call failed (is the Emacs daemon running?)"
    rm -f "$tmpfile"
    return 1
  fi
  zle -M "gitq: sent scrollback to Emacs"
}
zle -N gitq-scrollback-to-emacs-widget
bindkey '\ee' gitq-scrollback-to-emacs-widget   # Meta-e — rebind freely
