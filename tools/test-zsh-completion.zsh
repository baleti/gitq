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
eval "$(sed -n '/^_gitq_compute_prefix() {/,/^}/p' "$here/../integrations/zsh/_gitq")"

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

exit $failed
