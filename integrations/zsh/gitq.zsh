# gitq — complete zsh integration in one file.
#
# Everything gitq needs from zsh lives here: the classic completion function,
# the columnar TUI on TAB, and the scrollback widgets.  There is nothing else
# to install and no other plugin to load.
#
#   source /path/to/gitq.zsh
#
# *Order does not matter.*  It may be sourced before or after `compinit`,
# before or after fzf-tab or any other TAB-binding plugin, early or late in
# ~/.zshrc.  All the order-sensitive work — registering the completion,
# capturing the previous TAB binding, binding keys — is deferred to the first
# prompt, by which point the rest of your config has finished loading.  That
# deferral is the whole reason this can be a single file you drop anywhere.
#
# Do NOT put this on $fpath.  fpath means autoload: compinit scans it for
# `#compdef` files and never sources anything, so a file placed there would
# be installed and silently inert.  Sourcing is the only supported setup.
#
# What it sets up:
#
#   TAB   on a `gitq …` line   → gitq's columnar completer TUI, with a live
#                                preview and an M-x command palette.  Runs in
#                                a tmux popup when available, so it gets the
#                                whole terminal rather than the pane.
#         on anything else     → whatever TAB did before (fzf-tab, plain
#                                completion, …), captured at first prompt.
#   M-b                        → browse this pane's scrollback (needs tmux)
#   M-e                        → send scrollback to Emacs (needs emacsclient)
#   menu completion            → `gitq --complete-annotated`, grouped by kind
#
# Dependencies: the `gitq` binary. That is all — no fzf, no fzf-tab, no
# plugin manager. tmux is required only by the two scrollback widgets, which
# say so rather than failing silently.
#
# Opt-outs, set before sourcing:
#   GITQ_NO_TAB=1         leave TAB alone entirely
#   GITQ_NO_SCROLLBACK=1  do not bind M-b / M-e
#   GITQ_POPUP_WIDTH/HEIGHT  popup geometry (default 100%/100%)

# Guard against double-sourcing: re-running would capture our own widget as
# the TAB fall-through and recurse.
(( ${+_gitq_zsh_loaded} )) && return 0
typeset -g _gitq_zsh_loaded=1

# --- classic completion ------------------------------------------------------
#
# gitq takes its whole pipeline as a single argument, so completion happens
# *within* that argument: split the typed prefix, move everything before the
# last token into IPREFIX, and let the binary generate candidates.  The
# grammar therefore lives in gitq, shared with the Emacs client — this file
# never carries its own copy of the step/field lists.

# Sets four globals, so it is testable without a completion context:
#   _gitq_head     already-complete pipeline text, with a trailing space
#   _gitq_partial  the token being completed
#   _gitq_inword   the part of the current word before the token (an opening
#                  quote, plus any earlier tokens sharing the word)
#   _gitq_full     the whole pipeline typed so far
_gitq_compute_prefix() {
  local -a prior
  prior=("${(@)words[2,CURRENT-1]}")

  # Words completed in earlier argv positions (the unquoted form). Strip a
  # wrapping quote from each so gitq's tokenizer sees the pipeline itself.
  local w head=""
  for w in "${prior[@]}"; do
    w="${w#[\"\']}"
    w="${w%[\"\']}"
    [[ -n "$w" ]] && head+="$w "
  done

  # Now split the current word. In the quoted form this holds the whole
  # pipeline typed so far; in the unquoted form it is just one token.
  local cur="$PREFIX" qlead=""
  case "$cur" in
    \'*) qlead="'"; cur="${cur#\'}" ;;
    \"*) qlead='"'; cur="${cur#\"}" ;;
  esac

  local -a typed
  typed=("${(z)cur}")               # (z) splits respecting shell quoting

  local partial="" inword=""
  if (( ${#typed[@]} > 0 )); then
    # A trailing space means the last token is finished and we are starting
    # a new, empty one.
    if [[ "$cur" == *[[:space:]] ]]; then
      inword="$cur"
    else
      partial="${typed[-1]}"
      if (( ${#typed[@]} > 1 )); then
        typed[-1]=()
        inword="${(j: :)typed} "
      fi
    fi
  fi

  _gitq_head="$head"
  _gitq_partial="$partial"
  _gitq_inword="$qlead$inword"
  _gitq_full="$head$inword$partial"
}

_gitq() {
  # Only the pipeline is completed; there are no other arguments.
  (( CURRENT < 2 )) && return 1

  local _gitq_head _gitq_partial _gitq_inword _gitq_full
  _gitq_compute_prefix

  # IPREFIX is the non-completing prefix; PREFIX is the token being replaced.
  IPREFIX+="$_gitq_inword"
  PREFIX="$_gitq_partial"
  SUFFIX=""

  # Ask gitq for candidates given everything typed so far.
  local -a annotated
  annotated=(${(f)"$(gitq --complete-annotated "$_gitq_full" 2>/dev/null)"})
  (( ${#annotated[@]} == 0 )) && return 1

  # Group by kind for a nicer menu. The kind comes from the binary
  # ("candidate<TAB>kind<TAB>description"), the same registry the parser and
  # the Emacs client use, so these lists cannot drift out of date.
  local -a src_cands step_cands term_cands morph_cands field_cands op_cands other_cands
  local line c kind
  for line in $annotated; do
    c="${line%%$'\t'*}"
    kind="${${line#*$'\t'}%%$'\t'*}"
    case "$kind" in
      source)   src_cands+=("$c")   ;;
      step)     step_cands+=("$c")  ;;
      morphism) morph_cands+=("$c") ;;
      field)    field_cands+=("$c") ;;
      operator) op_cands+=("$c")    ;;
      terminal) term_cands+=("$c")  ;;
      # dynamic values (author names, shas, refs) have no registry kind
      *)        other_cands+=("$c") ;;
    esac
  done

  local expl
  [[ ${#src_cands[@]}   -gt 0 ]] && { _description gitq-src   expl 'source';    compadd "$expl[@]" -- $src_cands   }
  [[ ${#step_cands[@]}  -gt 0 ]] && { _description gitq-step  expl 'step';      compadd "$expl[@]" -- $step_cands  }
  [[ ${#morph_cands[@]} -gt 0 ]] && { _description gitq-morph expl 'morphism';  compadd "$expl[@]" -- $morph_cands }
  [[ ${#field_cands[@]} -gt 0 ]] && { _description gitq-field expl 'field';     compadd "$expl[@]" -- $field_cands }
  [[ ${#op_cands[@]}    -gt 0 ]] && { _description gitq-op    expl 'operator';  compadd "$expl[@]" -- $op_cands    }
  [[ ${#term_cands[@]}  -gt 0 ]] && { _description gitq-term  expl '/terminal'; compadd "$expl[@]" -- $term_cands  }
  [[ ${#other_cands[@]} -gt 0 ]] && { _description gitq-other expl 'value';     compadd "$expl[@]" -- $other_cands }
}

# --- TAB: gitq's columnar completer TUI --------------------------------------
#
# Like fzf's widgets, the TUI draws on /dev/tty and prints only the chosen
# pipeline to stdout, which we capture and put back on the line.

gitq-complete-tui-widget() {
  emulate -L zsh
  setopt localoptions extendedglob

  # Only take over when the command being edited is gitq; otherwise run
  # whatever TAB was before. An explicit array is required:
  # ${${(z)LBUFFER}[1]} subscripts a *scalar* (the first character) when the
  # line is a single word.
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

  # Run in a tmux popup when we can.  A popup is sized against the *client*,
  # not the pane, so the completer gets the whole terminal however small the
  # pane it was invoked from — and being an overlay it writes nothing into any
  # pane's grid or scrollback (measured; an inline UI cannot promise that,
  # since rows that scroll off during drawing are committed to history before
  # anything can erase them).
  #
  # This mirrors what fzf's own `--tmux` does for ^R.  stdout has to come back
  # through a file: the popup runs its command in a separate pane, so the
  # widget cannot read its stdout directly.  `-E` closes the popup when gitq
  # exits and propagates its exit status.
  local result ret tmp err
  local gitq_bin=${commands[gitq]}
  if [[ -n $TMUX ]] && (( $+commands[tmux] )); then
    if ! tmp=$(mktemp "${TMPDIR:-/tmp}/gitq-tui.XXXXXX"); then
      zle -M "gitq: could not create a temp file"
      return 1
    fi
    # `zle -I` first: zle owns the display until told otherwise, and a
    # full-screen program drawing underneath it can be painted over.
    zle -I
    # tmux's own errors (no such command on <3.2, a popup that will not fit)
    # go to stderr and would otherwise vanish, leaving TAB looking dead.
    # gitq's own stderr has to go to a file as well.  It would otherwise be
    # written to the popup's terminal, which tmux destroys the instant gitq
    # exits — so a gitq that fails on startup looks like a popup that flashes
    # and vanishes, with nothing anywhere to say why.
    local errfile=${tmp}.err
    # `-B` drops the popup border: at 100%/100% the border otherwise costs
    # two rows and two columns, so the completer would be smaller than the
    # terminal it is meant to fill.  Measured: 133x55 bordered vs 135x57
    # borderless on a 135x57 client.
    # An ABSOLUTE path, not `gitq`: display-popup runs its command with the
    # tmux *server's* environment, not this shell's, and a non-interactive
    # `zsh -c` never sources ~/.zshrc — so a gitq installed to ~/.local/bin
    # (added to PATH there) is simply not found, and the popup dies with 127
    # before drawing anything.
    err=$(tmux display-popup -B -E -w "${GITQ_POPUP_WIDTH:-100%}" -h "${GITQ_POPUP_HEIGHT:-100%}" \
      "${(q)gitq_bin} --complete-tui ${(q)pipeline} > ${(q)tmp} 2> ${(q)errfile}" 2>&1 >/dev/null)
    ret=$?
    result=$(<$tmp)
    local gitq_err=; [[ -r $errfile ]] && gitq_err=$(<$errfile)
    rm -f $tmp $errfile
    if [[ -n $err ]]; then
      zle -M "gitq: tmux popup failed: $err"
      # a popup that could not open is not a cancelled completion; fall back
      # rather than leaving the user with nothing
      result=$(gitq --complete-tui "$pipeline")
      ret=$?
    elif [[ -z $result && -n $gitq_err ]]; then
      # exited without choosing anything *and* said something: that is a
      # failure, not a cancellation
      zle -M "gitq: ${gitq_err%%$'\n'*}"
    fi
  else
    result=$(gitq --complete-tui "$pipeline")
    ret=$?
  fi

  # exit 0 with output = accepted; anything else = cancelled (or an M-x
  # command ran), so leave the line as the user left it.
  if (( ret == 0 )) && [[ -n $result ]]; then
    local q=${result//\'/\'\\\'\'}   # single-quote, escaping embedded quotes
    LBUFFER="gitq '$q'"
    RBUFFER=""
  fi
  zle reset-prompt
}
zle -N gitq-complete-tui-widget

# --- scrollback widgets ------------------------------------------------------
#
# gitq marks its *own* output invisibly as it prints, so those boundaries are
# exact with no shell integration at all (`gitq --scrollback --gitq-only`).
# Splitting the scrollback of other commands still infers prompts, which is
# best-effort and tunable:
#
#   export GITQ_SCROLLBACK_PROMPT_REGEX='^my-prompt-pattern '
#
# (a POSIX ERE, matched against each ANSI-stripped line).

gitq-scrollback-browse-widget() {
  if [[ -z "$TMUX" ]]; then
    zle -M "gitq: scrollback browsing needs tmux"
    return 1
  fi
  # Overlay the browser with tmux's own popup (tmux >= 3.2) so it behaves
  # like a real UI layer instead of trashing the current line's redraw.
  zle -I
  # absolute path for the same reason as the completer: a popup does not get
  # this shell's PATH
  tmux display-popup -E -w 90% -h 90% "${(q)commands[gitq]} --scrollback-browse"
  zle reset-prompt
}
zle -N gitq-scrollback-browse-widget

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
  # sexp can be large and emacsclient's argv escaping gets fragile past a
  # few KB. Emacs deletes the file after reading it.
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

# --- deferred setup ----------------------------------------------------------
#
# Run once, at the first prompt. This is what makes sourcing order irrelevant:
# by now compinit has defined `compdef`, and every plugin that wanted TAB has
# already taken it, so the binding we capture as the fall-through is the real
# previous one rather than whatever happened to be set mid-startup.

_gitq_setup() {
  emulate -L zsh

  # Register the completion. `compdef` exists only after compinit; if the
  # user never runs compinit there is simply no menu completion, and the TUI
  # (which needs none of this) still works.
  if (( $+functions[compdef] )); then
    compdef _gitq gitq
  fi

  if (( ! ${+GITQ_NO_TAB} )); then
    # Capture whatever TAB is bound to now, so non-gitq lines keep it.
    # Guard against capturing ourselves on a re-run.
    local cur=${${(z)"$(bindkey '^I')"}[2]}
    if [[ -n $cur && $cur != gitq-complete-tui-widget ]]; then
      typeset -g GITQ_FALLBACK_TAB=$cur
    fi
    : ${GITQ_FALLBACK_TAB:=expand-or-complete}
    bindkey '^I' gitq-complete-tui-widget
  fi

  if (( ! ${+GITQ_NO_SCROLLBACK} )); then
    bindkey '\eb' gitq-scrollback-browse-widget   # M-b, rebind freely
    bindkey '\ee' gitq-scrollback-to-emacs-widget # M-e, rebind freely
  fi

  # one-shot
  add-zsh-hook -d precmd _gitq_setup
  unfunction _gitq_setup
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _gitq_setup
