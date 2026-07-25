#!/usr/bin/env zsh
# Test _gitq's prefix computation without a running completion system.
#
# gitq accepts its pipeline quoted (`gitq 'commits wh'`) or unquoted
# (`gitq commits wh`), and the completer has to reconstruct the same
# pipeline text from either.  It used to only handle the quoted form: it
# bailed out unless the cursor was on argument 2, so `gitq commits <TAB>`
# silently did nothing.
#
# Usage: tools/test-zsh-completion.zsh   (exits non-zero on failure)
set -u
here="${0:A:h}"
eval "$(sed -n '/^_gitq_compute_prefix() {/,/^}/p' "$here/../integrations/zsh/gitq.zsh")"

failed=0

check() {
  local desc="$1"; shift
  local want_full="$1"; shift
  local want_partial="$1"; shift
  words=("$@")
  CURRENT=${#words[@]}
  PREFIX="${words[-1]}"
  local _gitq_head _gitq_partial _gitq_inword _gitq_full
  _gitq_compute_prefix
  if [[ "$_gitq_full" == "$want_full" && "$_gitq_partial" == "$want_partial" ]]; then
    printf 'ok    %-34s full=%-26s partial=%s\n' "$desc" "[$_gitq_full]" "[$_gitq_partial]"
  else
    printf 'FAIL  %-34s full=[%s] want [%s]  partial=[%s] want [%s]\n' \
      "$desc" "$_gitq_full" "$want_full" "$_gitq_partial" "$want_partial"
    failed=1
  fi
}

# unquoted form (the broken case)
check 'gitq <tab>'              ''               ''       gitq ''
check 'gitq com<tab>'           'com'            'com'    gitq 'com'
check 'gitq commits <tab>'      'commits '       ''       gitq 'commits' ''
check 'gitq commits wh<tab>'    'commits wh'     'wh'     gitq 'commits' 'wh'
check 'gitq commits where a<tab>' 'commits where a' 'a'   gitq 'commits' 'where' 'a'
# quoted form (must keep working)
check "gitq 'commits <tab>"     'commits '       ''       gitq "'commits "
check "gitq 'commits wh<tab>"   'commits wh'     'wh'     gitq "'commits wh"
check "gitq 'commits where a<tab>" 'commits where a' 'a'  gitq "'commits where a"


# --- full-flow tests -------------------------------------------------------
#
# The tests above exercise _gitq_compute_prefix in isolation.  That is not
# enough: 5fcc62c left a stale `local full_prefix="${before}${partial}"`
# after the correct assignment, so full_prefix was reset to "" on every call
# and completion offered top-level sources regardless of context — while the
# isolated tests still passed.  These run the real _gitq and assert on the
# prefix it actually hands to the binary.

# Stub the pieces _gitq talks to, so it can run outside a completion system.
# _gitq calls the binary inside $(...), i.e. a subshell, so the stub cannot
# report back through a variable — it writes to a file instead.
typeset -g _stub_file="${TMPDIR:-/tmp}/gitq-completion-test.$$"
gitq() {
  # record the prefix _gitq asked about; emit one plausible candidate
  print -r -- "$2" > "$_stub_file"
  print -r -- $'cand\tfield\tdescription'
}
compadd()      { : }
_description() { : }
zstyle()       { : }

eval "$(sed -n '/^_gitq() {/,/^}/p' "$here/../integrations/zsh/gitq.zsh")"

flow() {
  local desc="$1" want="$2"; shift 2
  # _gitq runs in a normal interactive shell, with no `set -u`.  Match that
  # here, so a reference to an unset variable degrades to "" exactly as it
  # does in real use — the 5fcc62c clobber was SILENT, and a test that turns
  # it into an abort would be testing the harness, not the completer.
  setopt localoptions nounset_off 2>/dev/null || set +u
  words=("$@")
  CURRENT=${#words[@]}
  PREFIX="${words[-1]}"
  IPREFIX="" SUFFIX=""
  print -r -- "<never called>" > "$_stub_file"
  _gitq >/dev/null 2>&1
  local _stub_prefix_seen="$(<$_stub_file)"
  if [[ "$_stub_prefix_seen" == "$want" ]]; then
    printf 'ok    %-34s asked for [%s]\n' "$desc" "$_stub_prefix_seen"
  else
    printf 'FAIL  %-34s asked for [%s]  want [%s]\n' "$desc" "$_stub_prefix_seen" "$want"
    failed=1
  fi
}

flow 'flow: gitq <tab>'               ''                  gitq ''
flow 'flow: gitq commits <tab>'       'commits '          gitq 'commits' ''
flow 'flow: gitq commits where <tab>' 'commits where '    gitq 'commits' 'where' ''
flow 'flow: gitq commits wh<tab>'     'commits wh'        gitq 'commits' 'wh'
flow "flow: gitq 'commits where <tab>" 'commits where '   gitq "'commits where "

rm -f "$_stub_file"
exit $failed
