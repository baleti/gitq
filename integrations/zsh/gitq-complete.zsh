# gitq's own TAB completer: a columnar fuzzy completer with a live preview
# (`gitq --complete-tui`), used in place of the normal completion system when
# the command line is a gitq invocation.  Every other command keeps whatever
# TAB already did — so this replaces fzf-tab *for gitq only*.
#
# Sourced, not autoloaded (it binds a key and defines a ZLE widget).  Source
# it AFTER fzf-tab so the fall-through captures fzf-tab's TAB binding:
#   source ~/.local/share/zsh/completions/gitq-complete.zsh
#
# Like fzf's own widgets, the TUI draws on /dev/tty and prints only the
# chosen pipeline to stdout, which we capture and put back on the line.

gitq-complete-tui-widget() {
  emulate -L zsh
  setopt localoptions extendedglob

  # Only take over when the command being edited is gitq; otherwise run
  # whatever TAB was before (fzf-tab, or plain completion).  An explicit
  # array is required: ${${(z)LBUFFER}[1]} subscripts a *scalar* (the first
  # character) when the line is a single word.
  local -a _words
  _words=(${(z)LBUFFER})
  if [[ ${_words[1]} != gitq ]] || ! (( $+commands[gitq] )); then
    zle ${GITQ_FALLBACK_TAB:-expand-or-complete}
    return
  fi

  # The pipeline is everything after the `gitq` word, minus a wrapping quote.
  local pipeline=${LBUFFER#*gitq}
  pipeline=${pipeline##[[:space:]]#}
  local lead=${pipeline[1]}
  if [[ $lead == "'" || $lead == '"' ]]; then
    pipeline=${pipeline#$lead}
    pipeline=${pipeline%$lead}
  fi

  local result
  result=$(gitq --complete-tui "$pipeline")
  local ret=$?

  # exit 0 with output = accepted; anything else = cancelled, leave the line.
  if (( ret == 0 )) && [[ -n $result ]]; then
    local q=${result//\'/\'\\\'\'}   # single-quote, escaping embedded quotes
    LBUFFER="gitq '$q'"
    RBUFFER=""
  fi
  zle reset-prompt
}
zle -N gitq-complete-tui-widget

# Remember the current TAB binding once, to fall through for non-gitq lines.
# Guard against re-sourcing rebinding to ourselves.
() {
  local cur=${${(z)"$(bindkey '^I')"}[2]}
  [[ -n $cur && $cur != gitq-complete-tui-widget ]] && typeset -g GITQ_FALLBACK_TAB=$cur
}
: ${GITQ_FALLBACK_TAB:=expand-or-complete}

bindkey '^I' gitq-complete-tui-widget
