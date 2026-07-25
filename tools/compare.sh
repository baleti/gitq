#!/usr/bin/env bash
# Compare two golden.sh output directories.
#
# Usage: tools/compare.sh <reference-dir> <candidate-dir>
#
# Cases marked ~ in the corpus (filenames carrying -xdiff) are reported
# separately: those are the deliberate gap fixes, where a diff is the point.
set -uo pipefail

ref="${1:?usage: compare.sh <ref-dir> <cand-dir>}"
cand="${2:?usage: compare.sh <ref-dir> <cand-dir>}"

pass=0; fail=0; xdiff_same=0; xdiff_diff=0
failed=()

for f in "$ref"/*; do
  base="$(basename "$f")"
  other="$cand/$base"
  if [ ! -e "$other" ]; then
    echo "MISSING in candidate: $base"; fail=$((fail+1)); failed+=("$base"); continue
  fi
  if diff -q "$f" "$other" >/dev/null 2>&1; then
    case "$base" in *-xdiff*) xdiff_same=$((xdiff_same+1)) ;; *) pass=$((pass+1)) ;; esac
  else
    case "$base" in
      *-xdiff*) xdiff_diff=$((xdiff_diff+1)) ;;
      *) fail=$((fail+1)); failed+=("$base") ;;
    esac
  fi
done

echo "match:        $pass"
echo "MISMATCH:     $fail"
echo "gap-fix diff: $xdiff_diff (expected to differ)"
echo "gap-fix same: $xdiff_same (marked ~ but identical - gap not yet fixed)"

if [ "$fail" -gt 0 ]; then
  echo
  echo "first mismatches:"
  for b in "${failed[@]:0:10}"; do
    echo "--- $b"
    diff "$ref/$b" "$cand/$b" | head -20
  done
  exit 1
fi
