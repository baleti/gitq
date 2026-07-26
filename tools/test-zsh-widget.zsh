#!/usr/bin/env zsh
# Tests for the zsh integration's widget behaviour.
#
#   zsh tools/test-zsh-widget.zsh
#
# Two halves, because zsh splits them.
#
# The pure half — pipeline extraction — is an ordinary function and runs
# here directly.
#
# The setup half is not: ZLE needs a terminal, and the whole design of this
# integration is that `bindkey`, `compdef` and the TAB fall-through are
# deferred to the *first prompt*, which never arrives in `zsh -c`.  Those
# tests therefore run a real interactive zsh inside a throwaway tmux server
# and read the bindings back.  They skip, loudly, when tmux is absent.
#
# That deferral is exactly where the bugs have been — a file installed on
# fpath and never sourced, a TAB binding that swallowed every other command,
# a popup that could not find gitq — so it is the half most worth covering.

emulate -L zsh
setopt no_unset

typeset -g pass=0 fail=0 skip=0
ok()   { (( pass++ )); print -r -- "ok    $1" }
bad()  { (( fail++ )); print -r -- "FAIL  $1"; print -r -- "      $2" }
skip() { (( skip++ )); print -r -- "skip  $1 ($2)" }
is()   { [[ "$2" == "$3" ]] && ok "$1" || bad "$1" "expected [$3], got [$2]" }

here=${0:A:h}
integration=$here/../integrations/zsh/gitq.zsh

# --- pure: the pipeline inside a command line ------------------------------

# Pull the function out rather than sourcing the file: sourcing binds keys
# and registers hooks, which a test has no business doing to its own shell.
eval "$(sed -n '/^_gitq_pipeline_of_line() {/,/^}/p' $integration)"

check() { is "pipeline of [$1]" "$(_gitq_pipeline_of_line "$1")" "$2" }

check "gitq "                     ""
check "gitq commits wh"           "commits wh"
check "gitq 'commits where a"     "commits where a"
check "gitq 'commits where a'"    "commits where a"
check 'gitq "commits"'            "commits"
# a pipeline containing the word gitq must not be re-split on it
check "gitq 'commits grep gitq"   "commits grep gitq"

# --- interactive: what the first prompt sets up ----------------------------

if ! command -v tmux >/dev/null 2>&1; then
  skip "interactive setup" "tmux not installed"
else
  sock=gitqtest$$
  rc=$(mktemp -d)
  trap "tmux -L $sock kill-server 2>/dev/null; rm -rf $rc" EXIT

  # A shell that has *something else* on TAB first, so the fall-through has
  # a real binding to capture rather than the default.
  cat > $rc/.zshrc <<RC
PS1='> '
autoload -Uz compinit && compinit -u
other-tab-widget() { zle expand-or-complete }
zle -N other-tab-widget
bindkey '^I' other-tab-widget
source $integration
RC

  probe() {
    # run one command in a real interactive zsh and return its output
    local out
    tmux -L $sock kill-server 2>/dev/null
    tmux -L $sock new-session -d -x 80 -y 24 \
      "env ZDOTDIR=$rc HOME=$rc zsh -i" 2>/dev/null
    sleep 1.5   # let the first prompt fire the deferred setup
    tmux -L $sock send-keys "$1" Enter 2>/dev/null
    sleep 1
    out=$(tmux -L $sock capture-pane -p 2>/dev/null)
    tmux -L $sock kill-server 2>/dev/null
    print -r -- "$out"
  }

  out=$(probe 'bindkey "^I" | tail -1')
  if [[ "$out" == *gitq-complete-tui-widget* ]]; then
    ok "TAB is bound at the first prompt"
  else
    bad "TAB is bound at the first prompt" "got: ${out##*$'\n'}"
  fi

  # The fall-through: TAB on a non-gitq line must reach whatever TAB was.
  out=$(probe 'print "FB=[$GITQ_FALLBACK_TAB]"')
  if [[ "$out" == *"FB=[other-tab-widget]"* ]]; then
    ok "the previous TAB binding is captured for other commands"
  else
    bad "the previous TAB binding is captured for other commands" \
        "expected other-tab-widget, got: $(print -r -- $out | grep -o 'FB=\[[^]]*\]' | tail -1)"
  fi

  # Re-sourcing is a no-op.  Without the double-source guard the setup would
  # run again and capture *our own* widget as the fall-through, so TAB would
  # call itself.  (Note this is what the guard buys: the `$cur !=
  # gitq-complete-tui-widget` check inside the setup is belt-and-braces and
  # cannot fire while the guard holds — removing it does not fail this test.)
  out=$(probe "source $integration; print \"FB=[\$GITQ_FALLBACK_TAB]\"")
  if [[ "$out" == *"FB=[other-tab-widget]"* ]]; then
    ok "re-sourcing leaves the fall-through intact"
  else
    bad "re-sourcing leaves the fall-through intact" \
        "got: $(print -r -- $out | grep -o 'FB=\[[^]]*\]' | tail -1)"
  fi

  # The setup is one-shot: it removes its own hook and itself, so a later
  # prompt cannot re-run it and re-capture the binding.
  # NB: the capture includes the command as typed, so the marker must not
  # appear there — print a substituted value, never a literal answer.
  out=$(probe 'n=$+functions[_gitq_setup]; print "SETUPFN=$n"')
  [[ "$out" == *"SETUPFN=0"* ]] \
    && ok "the deferred setup runs once and removes itself" \
    || bad "the deferred setup runs once and removes itself" \
        "got: $(print -r -- $out | grep -o 'SETUPFN=[0-9]' | tail -1)"

  # The completion function is registered with compdef, not left on fpath.
  out=$(probe 'n=$+functions[_gitq]; print "COMPFN=$n"')
  [[ "$out" == *"COMPFN=1"* ]] \
    && ok "the completion function is defined" \
    || bad "the completion function is defined" \
        "got: $(print -r -- $out | grep -o 'COMPFN=[0-9]' | tail -1)"

  # The scrollback widgets are bound too.
  out=$(probe 'bindkey "\eb" | tail -1')
  [[ "$out" == *gitq-scrollback-browse-widget* ]] \
    && ok "M-b is bound to the scrollback browser" \
    || bad "M-b is bound to the scrollback browser" "got: ${out##*$'\n'}"

  # Opt-outs are honoured.
  cat > $rc/.zshrc <<RC
PS1='> '
autoload -Uz compinit && compinit -u
other-tab-widget() { zle expand-or-complete }
zle -N other-tab-widget
bindkey '^I' other-tab-widget
GITQ_NO_TAB=1
GITQ_NO_SCROLLBACK=1
source $integration
RC
  out=$(probe 'bindkey "^I" | tail -1')
  [[ "$out" == *other-tab-widget* ]] \
    && ok "GITQ_NO_TAB leaves TAB alone" \
    || bad "GITQ_NO_TAB leaves TAB alone" "got: ${out##*$'\n'}"

  out=$(probe 'bindkey "\eb" | tail -1')
  [[ "$out" != *gitq-scrollback* ]] \
    && ok "GITQ_NO_SCROLLBACK leaves M-b alone" \
    || bad "GITQ_NO_SCROLLBACK leaves M-b alone" "M-b was bound anyway"
fi

print -r -- ""
print -r -- "$pass passed, $fail failed, $skip skipped"
(( fail == 0 ))
