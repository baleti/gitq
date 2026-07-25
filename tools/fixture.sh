#!/usr/bin/env bash
# Build the deterministic golden-corpus fixture repository.
#
# Every commit pins author AND committer identity and date, so the SHAs are
# byte-reproducible on any machine — which is what lets the golden outputs
# contain real SHAs instead of being scrubbed.
#
# Usage: tools/fixture.sh <dir>
set -euo pipefail

dir="${1:?usage: fixture.sh <dir>}"
rm -rf "$dir"
mkdir -p "$dir"
cd "$dir"

export TZ=UTC
export GIT_CONFIG_NOSYSTEM=1

git init -q -b main
git config user.name alice
git config user.email alice@example.com
git config commit.gpgsign false
git config tag.gpgsign false
git config gc.auto 0

# commit as NAME EMAIL DATE MESSAGE
commit() {
  local name="$1" email="$2" date="$3" msg="$4"
  GIT_AUTHOR_NAME="$name" GIT_AUTHOR_EMAIL="$email" GIT_AUTHOR_DATE="$date" \
  GIT_COMMITTER_NAME="$name" GIT_COMMITTER_EMAIL="$email" GIT_COMMITTER_DATE="$date" \
    git commit -q -m "$msg"
}

# --- c1: root commit, by alice -------------------------------------------
cat > a.txt <<'EOF'
hello
needle-alpha
context line one
context line two
EOF
cat > README.md <<'EOF'
# fixture
A deterministic repository for the gitq golden corpus.
EOF
git add a.txt README.md
commit alice alice@example.com "2024-01-01T10:00:00+0000" "initial commit"
git tag v1

# --- c2: by bob ----------------------------------------------------------
echo "world" > b.txt
git add b.txt
commit bob bob@example.com "2024-02-01T10:00:00+0000" "add b"

# --- c3: by alice, multi-hunk edit ---------------------------------------
cat > a.txt <<'EOF'
hello
needle-alpha
needle-beta
context line one
context line two
tail added far below to force a second hunk
EOF
git add a.txt
commit alice alice@example.com "2024-03-01T10:00:00+0000" "fix a needle-beta"

# --- c4: feature branch off c2, by carol ---------------------------------
git checkout -q -b feature HEAD~1
echo "feature work" > feature.txt
git add feature.txt
commit carol carol@example.com "2024-03-15T12:00:00+0000" "add feature file"

# --- c5: merge feature into main -> a commit with parents-count 2 --------
git checkout -q main
GIT_AUTHOR_NAME=alice GIT_AUTHOR_EMAIL=alice@example.com GIT_AUTHOR_DATE="2024-04-01T10:00:00+0000" \
GIT_COMMITTER_NAME=alice GIT_COMMITTER_EMAIL=alice@example.com GIT_COMMITTER_DATE="2024-04-01T10:00:00+0000" \
  git merge -q --no-ff -m "merge feature" feature

# --- c6: rename b.txt -> c.txt -------------------------------------------
git mv b.txt c.txt
commit bob bob@example.com "2024-05-01T10:00:00+0000" "rename b to c"

# --- c7: delete README.md ------------------------------------------------
git rm -q README.md
commit alice alice@example.com "2024-06-01T10:00:00+0000" "drop the readme"

# --- c8: latin-1 metadata and content ------------------------------------
# git.git has latin-1 in its history; lenient UTF-8 decoding of git output
# was a real bug (commit 1abd03d).  Keep a regression case in the fixture.
printf 'caf\xe9 latin1 line\n' > latin1.txt
git add latin1.txt
GIT_AUTHOR_NAME="$(printf 'Ren\xe9')" GIT_AUTHOR_EMAIL=rene@example.com \
GIT_AUTHOR_DATE="2024-07-01T10:00:00+0000" \
GIT_COMMITTER_NAME="$(printf 'Ren\xe9')" GIT_COMMITTER_EMAIL=rene@example.com \
GIT_COMMITTER_DATE="2024-07-01T10:00:00+0000" \
  git commit -q -m "$(printf 'add caf\xe9 file')"

git tag -a v2 -m "annotated tag two" 2>/dev/null

# --- dirty worktree state ------------------------------------------------
# The `worktrees` source declares modified/staged/untracked flags that the
# Haskell build never populates; the port fixes that, so the fixture has to
# actually exhibit all three states.
echo "staged change" >> a.txt
git add a.txt
echo "unstaged change" >> c.txt
echo "untracked" > untracked.txt

git commit-graph write --reachable -q 2>/dev/null || true
