#!/usr/bin/env bash
# Run the corpus through a gitq binary and record every result.
#
# Usage: tools/golden.sh <binary> <outdir> [fixture-dir]
#
# One file per case per mode, holding exit code, stdout and stderr.  Output
# is regenerated per comparison run rather than committed, because `where
# date within N` is relative to now — two binaries must be sampled at the
# same moment, not against a stored baseline from another day.
set -uo pipefail

bin="${1:?usage: golden.sh <binary> <outdir> [fixture]}"
out="${2:?usage: golden.sh <binary> <outdir> [fixture]}"
fixture="${3:-}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
corpus="$here/corpus.txt"

if [ -z "$fixture" ]; then
  fixture="$(mktemp -d)/fx"
  bash "$here/fixture.sh" "$fixture" >/dev/null 2>&1
fi

bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"
rm -rf "$out"; mkdir -p "$out"
out="$(cd "$out" && pwd)"

export TZ=UTC
export GITQ_NO_NATIVE="${GITQ_NO_NATIVE:-}"

cd "$fixture"

# record <file> <label> <argv...>
record() {
  local file="$1" label="$2"; shift 2
  local so se rc
  so="$("$bin" "$@" 2>/tmp/gitq-golden-err.$$)"; rc=$?
  se="$(cat /tmp/gitq-golden-err.$$)"
  {
    printf '### case: %s\n' "$label"
    printf '### argv:'; printf ' %q' "$@"; printf '\n'
    printf '### exit: %s\n' "$rc"
    printf '### stdout:\n%s\n' "$so"
    printf '### stderr:\n%s\n' "$se"
  } > "$file"
  rm -f /tmp/gitq-golden-err.$$
}

n=0
while IFS= read -r line; do
  case "$line" in ''|'#'*) continue ;; esac

  mode="${line%% *}"
  query="${line#* }"
  [ "$mode" = "$line" ] && query=""

  expected_diff=0
  case "$mode" in '~'*) expected_diff=1; mode="${mode#\~}" ;; esac

  n=$((n+1))
  id="$(printf '%03d' "$n")"
  [ "$expected_diff" = 1 ] && id="${id}-xdiff"

  case "$mode" in
    q)
      record "$out/$id.plain" "$query" "$query"
      record "$out/$id.sexp"  "$query" --sexp "$query"
      ;;
    e)
      record "$out/$id.err" "$query" "$query"
      ;;
    c)
      record "$out/$id.comp"  "$query" --complete "$query"
      record "$out/$id.compa" "$query" --complete-annotated "$query"
      ;;
    *)
      echo "golden.sh: unknown mode '$mode' on line: $line" >&2; exit 2 ;;
  esac
done < "$corpus"

echo "recorded $n cases -> $out"
