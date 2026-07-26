#!/usr/bin/env bash
# Tests for the bash integration's widget behaviour.
#
#   bash tools/test-bash-widget.bash
#
# A bash completion function is an ordinary shell function driven by
# COMP_LINE / COMP_POINT / COMP_WORDS, so most of this needs no terminal: the
# tests set those, call the function, and read COMPREPLY back.  That is the
# half where the bugs have actually been — pipeline extraction, which command
# TAB belongs to, and whether gitq is found at all.
#
# `gitq` is stubbed on PATH.  The binary has its own suite; what is under test
# here is the shell code around it, and a stub also makes the tests run
# without a repository or a tty.

set -uo pipefail

pass=0 fail=0
ok()   { pass=$((pass+1)); printf 'ok    %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf 'FAIL  %s\n      %s\n' "$1" "$2"; }
is()   { [[ "$2" == "$3" ]] && ok "$1" || bad "$1" "expected [$3], got [$2]"; }

here=$(cd "$(dirname "$0")" && pwd)
integration="$here/../integrations/bash/gitq.bash"

# --- a stub gitq, so no binary, repo or tty is needed ----------------------
stub=$(mktemp -d)
trap 'rm -rf "$stub"' EXIT
cat > "$stub/gitq" <<'STUB'
#!/usr/bin/env bash
# record how we were *invoked*, path included, then answer plausibly
printf '%s %s\n' "$0" "$*" >> "${GITQ_STUB_LOG:-/dev/null}"
case "$1" in
  --complete-tui) printf 'commits where author alice\n' ;;
  --complete)     printf 'commits\nbranches\ntags\n' ;;
esac
STUB
chmod +x "$stub/gitq"
PATH="$stub:$PATH"
export GITQ_STUB_LOG="$stub/log"

# shellcheck source=/dev/null
source "$integration"

# --- pipeline extraction ---------------------------------------------------
# The line as typed -> what gitq's tokenizer should be handed.

check_pipeline() {
  local line=$1 want=$2
  COMP_LINE="$line" COMP_POINT=${#line}
  is "pipeline of [$line]" "$(_gitq_pipeline_of_line)" "$want"
}

check_pipeline "gitq "                    ""
check_pipeline "gitq commits wh"          "commits wh"
check_pipeline "gitq 'commits where a"    "commits where a"
check_pipeline "gitq \"commits\""         "commits\""

# COMP_POINT is honoured: completion happens at the cursor, not at end of line
COMP_LINE="gitq commits where author" COMP_POINT=13
is "pipeline stops at the cursor" "$(_gitq_pipeline_of_line)" "commits "

# --- which command TAB belongs to ------------------------------------------
# The reason this is `complete -F` and not `bind -x`: it must not touch any
# other command's completion.

is "gitq has a completion function" \
   "$(complete -p gitq 2>/dev/null | grep -c '_gitq_complete')" "1"
is "ls is left alone" \
   "$(complete -p ls 2>/dev/null | grep -c '_gitq_complete')" "0"

# --- the TUI path ----------------------------------------------------------
# Outside tmux the function runs gitq directly and offers its stdout as the
# single completion.

run_complete() {
  local line=$1
  COMP_LINE="$line" COMP_POINT=${#line}
  COMP_WORDS=("gitq" "") COMP_CWORD=1
  COMPREPLY=()
  ( unset TMUX; _gitq_complete )
  # the subshell above cannot export COMPREPLY back, so redo it in-process
  unset TMUX
  COMPREPLY=()
  _gitq_complete
}

run_complete "gitq "
is "the TUI's output becomes the completion" \
   "${COMPREPLY[0]}" "'commits where author alice'"

# The binary is called by ABSOLUTE path: a tmux popup runs with the server's
# environment, not this shell's, so a bare `gitq` is not found there.
if grep -q '^/.*gitq --complete-tui' "$GITQ_STUB_LOG" 2>/dev/null; then
  ok "gitq is invoked by absolute path"
else
  bad "gitq is invoked by absolute path" "log: $(head -1 "$GITQ_STUB_LOG" 2>/dev/null)"
fi

# --- cancelling ------------------------------------------------------------
cat > "$stub/gitq" <<'STUB'
#!/usr/bin/env bash
exit 1
STUB
chmod +x "$stub/gitq"
run_complete "gitq commits"
is "a cancelled TUI leaves the line alone" "${#COMPREPLY[@]}" "0"

# --- the opt-out -----------------------------------------------------------
cat > "$stub/gitq" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  --complete) printf 'commits\nbranches\ntags\n' ;;
  --complete-tui) printf 'SHOULD NOT RUN\n' ;;
esac
STUB
chmod +x "$stub/gitq"

GITQ_NO_TAB=1
COMP_LINE="gitq co" COMP_POINT=7
COMP_WORDS=("gitq" "co") COMP_CWORD=1
COMPREPLY=()
_gitq_complete
is "GITQ_NO_TAB falls back to candidates" "${COMPREPLY[0]:-}" "commits"
is "and does not run the TUI" \
   "$(printf '%s\n' "${COMPREPLY[@]}" | grep -c 'SHOULD NOT RUN')" "0"
unset GITQ_NO_TAB

# --- a missing binary ------------------------------------------------------
rm -f "$stub/gitq"
COMP_LINE="gitq " COMP_POINT=5
COMP_WORDS=("gitq" "") COMP_CWORD=1
COMPREPLY=()
_gitq_complete 2>/dev/null
is "no gitq on PATH is quiet, not an error" "${#COMPREPLY[@]}" "0"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ $fail -eq 0 ]]
