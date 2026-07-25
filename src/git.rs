//! Git execution layer and data fetchers.
//!
//! All git output is read as bytes and decoded as lenient UTF-8 once: real
//! histories carry latin-1 metadata (git.git does), and strict decoding
//! throws on the first such byte.
//!
//! # On zero-copy
//!
//! The Haskell build made every field a zero-copy `Text` slice of one
//! decoded buffer.  Here each field is its own `Arc<str>`, which costs one
//! allocation per field.  That is a deliberate first cut: correctness and
//! the corpus come first, the gix backend will change the shape of this
//! code anyway, and the profile — not a guess — should decide whether a
//! slice type earns its complexity.  See the benchmark commit.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::ast::EntryFilter;
use crate::frame::{Frame, FrameType, Value};

/// A user-facing gitq error (parse error, missing repo, guarded terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitqError(pub String);

impl std::fmt::Display for GitqError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitqError {}

pub type R<T> = Result<T, GitqError>;

pub fn gitq_error<T>(msg: impl Into<String>) -> R<T> {
    Err(GitqError(msg.into()))
}

/// Decode git's bytes leniently and split into non-empty lines.
fn lines_of(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Run git; return output as a list of non-empty lines.  Stderr is
/// discarded, not mixed into the captured output — otherwise a git error
/// message (e.g. an invalid revision) gets split into lines and silently
/// returned as if it were real data.
pub fn run_git(args: &[&str]) -> Vec<String> {
    run_git_stdin(args, "")
}

/// Like [`run_git`], feeding the given input to git's stdin — used with
/// `--stdin` to pass arbitrarily many revisions in a single process,
/// bypassing the argv length limit.
pub fn run_git_stdin(args: &[&str], input: &str) -> Vec<String> {
    let Ok(mut child) = Command::new("git")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };

    // Write stdin from a separate thread: a large --stdin batch would
    // otherwise deadlock against a filling stdout pipe.
    if let Some(mut stdin) = child.stdin.take() {
        let data = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(data.as_bytes());
        });
    }

    match child.wait_with_output() {
        Ok(out) => lines_of(&out.stdout),
        Err(_) => Vec::new(),
    }
}

/// Like [`run_git`], but a git failure is surfaced instead of swallowed:
/// `Err` carries git's own stderr.  For steps where an invalid argument
/// (e.g. a bad revspec) must be a loud error, not a silently-empty result.
pub fn run_git_loud(args: &[&str]) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;

    if out.status.success() {
        Ok(lines_of(&out.stdout))
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Run git; return the first line of output, or None.
pub fn run_git_string(args: &[&str]) -> Option<String> {
    run_git(args).into_iter().next()
}

/// Run git with the terminal's stdio inherited (lets `git commit` open the
/// user's editor).  Errors loudly on a non-zero exit.
pub fn run_git_inherit(args: &[&str]) -> R<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .map_err(|e| GitqError(format!("gitq: could not run git: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        gitq_error(format!(
            "git {} exited with status {code}",
            args.iter().take(2).cloned().collect::<Vec<_>>().join(" ")
        ))
    }
}

/// The git toplevel, or a loud error when not in a repository.
pub fn toplevel() -> R<String> {
    match run_git_string(&["rev-parse", "--show-toplevel"]) {
        Some(t) => Ok(t),
        None => gitq_error("gitq: not in a git repository"),
    }
}

/// NUL-delimited log format using git's `%x00` escape (safe as a CLI arg).
pub const LOG_FORMAT: &str = "%H%x00%ae%x00%an%x00%ai%x00%P%x00%T%x00%s";

fn s(v: &str) -> Value {
    Value::Str(Arc::from(v))
}

/// Parse a NUL-delimited commit log line into a commit frame, or None.
pub fn parse_commit_line(line: &str) -> Option<Frame> {
    let parts: Vec<&str> = line.split('\0').collect();
    if parts.len() < 7 {
        return None;
    }
    let (sha, email, author, date, parents, tree) =
        (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]);
    if sha.is_empty() {
        return None;
    }
    // A NUL inside the subject would have split further; rejoin.
    let message = parts[6..].join("");

    let mut attrs = BTreeMap::new();
    attrs.insert("sha".to_string(), s(sha));
    attrs.insert("email".to_string(), s(email));
    attrs.insert("author".to_string(), s(author));
    attrs.insert("date".to_string(), s(date));
    attrs.insert("tree".to_string(), s(tree));
    attrs.insert("message".to_string(), s(&message));

    Some(Frame {
        ty: FrameType::Commit,
        parents: parents
            .split_whitespace()
            .map(|p| Arc::from(p) as Arc<str>)
            .collect(),
        attrs,
    })
}

/// Commits reachable from HEAD (or within a range) as commit frames.
pub fn fetch_commits(range: Option<&str>) -> Vec<Frame> {
    let fmt = format!("--format={LOG_FORMAT}");
    let args: Vec<&str> = match range {
        Some(r) => vec!["log", &fmt, r],
        None => vec!["log", &fmt],
    };
    run_git(&args)
        .iter()
        .filter_map(|l| parse_commit_line(l))
        .collect()
}

/// Many commits by full SHA in a single git process, as a SHA-keyed map.
/// One `git log --no-walk --stdin` replaces one process per commit — the
/// difference between milliseconds and minutes on ancestor-closure walks.
///
/// Callers must pass full SHAs (the `%H`/`%P` values git itself printed);
/// unresolvable input would fail the whole batch, unlike [`fetch_commit`]
/// which probes one tolerant `rev-parse`.
pub fn fetch_commit_map(shas: &[Arc<str>]) -> BTreeMap<String, Frame> {
    if shas.is_empty() {
        return BTreeMap::new();
    }
    let mut seen = HashSet::new();
    let mut input = String::new();
    for sha in shas {
        if seen.insert(sha.as_ref()) {
            input.push_str(sha);
            input.push('\n');
        }
    }
    let fmt = format!("--format={LOG_FORMAT}");
    let lines = run_git_stdin(&["log", "--no-walk=unsorted", &fmt, "--stdin"], &input);

    lines
        .iter()
        .filter_map(|l| parse_commit_line(l))
        .filter_map(|f| {
            let sha = f.field("sha")?.as_str()?.to_string();
            Some((sha, f))
        })
        .collect()
}

/// A single commit by SHA or ref, or None.
pub fn fetch_commit(sha_or_ref: &str) -> Option<Frame> {
    let sha = run_git_string(&["rev-parse", "--verify", sha_or_ref])?;
    let fmt = format!("--format={LOG_FORMAT}");
    run_git(&["log", "--no-walk", &fmt, &sha])
        .iter()
        .filter_map(|l| parse_commit_line(l))
        .next()
}

fn parse_ref_line(reftype: Option<&str>, line: &str) -> Option<Frame> {
    let (sha, rest) = line.split_once(' ')?;
    if sha.len() < 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) || rest.is_empty() {
        return None;
    }
    let mut attrs: Vec<(String, Value)> =
        vec![("sha".to_string(), s(sha)), ("name".to_string(), s(rest))];
    if let Some(rt) = reftype {
        attrs.push(("reftype".to_string(), s(rt)));
    }
    Some(Frame::new(FrameType::Ref, attrs))
}

fn fetch_for_each_ref(reftype: Option<&str>, patterns: &[&str]) -> Vec<Frame> {
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname:short)"];
    args.extend_from_slice(patterns);
    run_git(&args)
        .iter()
        .filter_map(|l| parse_ref_line(reftype, l))
        .collect()
}

pub fn fetch_branches() -> Vec<Frame> {
    fetch_for_each_ref(Some("branch"), &["refs/heads/"])
}

pub fn fetch_tags() -> Vec<Frame> {
    fetch_for_each_ref(Some("tag"), &["refs/tags/"])
}

pub fn fetch_refs() -> Vec<Frame> {
    fetch_for_each_ref(None, &[])
}

/// Working-tree status for one worktree, as (modified, staged, untracked).
///
/// Gap fix: 0.7.0 declared these three fields on the worktree shape and
/// never populated them, so they type-checked and read as false forever.
/// `git status --porcelain` gives two status columns per entry — index
/// state then worktree state — plus `??` for untracked.
fn worktree_status(path: &str) -> (bool, bool, bool) {
    let lines = run_git(&["-C", path, "status", "--porcelain"]);
    let mut modified = false;
    let mut staged = false;
    let mut untracked = false;

    for l in &lines {
        let mut cs = l.chars();
        let (Some(index), Some(tree)) = (cs.next(), cs.next()) else {
            continue;
        };
        if index == '?' && tree == '?' {
            untracked = true;
            continue;
        }
        if index != ' ' && index != '!' {
            staged = true;
        }
        if tree != ' ' && tree != '!' {
            modified = true;
        }
    }
    (modified, staged, untracked)
}

/// All worktrees, from `git worktree list --porcelain`.
pub fn fetch_worktrees() -> Vec<Frame> {
    let lines = run_git(&["worktree", "list", "--porcelain"]);
    let mut out: Vec<Vec<(String, Value)>> = Vec::new();

    for l in &lines {
        if let Some(p) = l.strip_prefix("worktree ") {
            out.push(vec![("path".to_string(), s(p))]);
        } else if let Some(cur) = out.last_mut() {
            if let Some(sha) = l.strip_prefix("HEAD ") {
                cur.push(("sha".to_string(), s(sha)));
            } else if let Some(b) = l.strip_prefix("branch ") {
                let short = b.strip_prefix("refs/heads/").unwrap_or(b);
                cur.push(("branch".to_string(), s(short)));
            } else if *l == "detached" {
                cur.push(("detached".to_string(), Value::Bool(true)));
            }
        }
    }

    out.into_iter()
        .map(|mut attrs| {
            let path = attrs
                .iter()
                .find(|(k, _)| k == "path")
                .and_then(|(_, v)| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let (modified, staged, untracked) = worktree_status(&path);
            attrs.push(("modified".to_string(), Value::Bool(modified)));
            attrs.push(("staged".to_string(), Value::Bool(staged)));
            attrs.push(("untracked".to_string(), Value::Bool(untracked)));
            Frame::new(FrameType::Worktree, attrs)
        })
        .collect()
}

/// Blob/tree entries under a tree SHA, optionally filtered by entry type
/// and path glob.
pub fn fetch_blobs_at(
    tree_sha: &str,
    path_filter: Option<&str>,
    type_filter: Option<EntryFilter>,
) -> Vec<Frame> {
    let kind_name = |t: EntryFilter| match t {
        EntryFilter::Blob => "blob",
        EntryFilter::Tree => "tree",
    };

    run_git(&["ls-tree", "-r", tree_sha])
        .iter()
        .filter_map(|line| {
            // format: "<mode> <type> <sha>\t<path>"
            let (meta, path) = line.split_once('\t')?;
            let w: Vec<&str> = meta.split_whitespace().collect();
            let [mode, ftype, sha] = w[..] else {
                return None;
            };
            if ftype != "blob" && ftype != "tree" {
                return None;
            }
            if type_filter.is_some_and(|tf| kind_name(tf) != ftype) {
                return None;
            }
            if path_filter.is_some_and(|pf| !path_matches(pf, path)) {
                return None;
            }
            let ty = if ftype == "blob" {
                FrameType::Blob
            } else {
                FrameType::Tree
            };
            Some(Frame::new(
                ty,
                vec![
                    ("sha".to_string(), s(sha)),
                    ("path".to_string(), s(path)),
                    ("mode".to_string(), s(mode)),
                ],
            ))
        })
        .collect()
}

/// Glob match (shell wildcards `*`, `?`, `[...]` — `*` crosses `/`, same as
/// Emacs's wildcard-to-regexp), with a literal-substring fallback.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    glob_match(pattern, path) || path.contains(pattern)
}

fn glob_match(p: &str, s: &str) -> bool {
    let pc: Vec<char> = p.chars().collect();
    let sc: Vec<char> = s.chars().collect();
    glob_at(&pc, &sc)
}

fn glob_at(p: &[char], s: &[char]) -> bool {
    let Some((&head, ptail)) = p.split_first() else {
        return s.is_empty();
    };
    match head {
        // `*` crosses `/`, matching every suffix
        '*' => (0..=s.len()).any(|i| glob_at(ptail, &s[i..])),
        '?' => !s.is_empty() && glob_at(ptail, &s[1..]),
        '[' => {
            let Some(close) = ptail.iter().position(|&c| c == ']') else {
                return false;
            };
            if close == 0 {
                return false;
            }
            let class = &ptail[..close];
            let after = &ptail[close + 1..];
            !s.is_empty() && class_match(class, s[0]) && glob_at(after, &s[1..])
        }
        c => !s.is_empty() && c == s[0] && glob_at(ptail, &s[1..]),
    }
}

fn class_match(class: &[char], c: char) -> bool {
    if let Some((&'!', rest)) = class.split_first() {
        return !class_match(rest, c);
    }
    let mut i = 0;
    while i < class.len() {
        if i + 2 < class.len() && class[i + 1] == '-' {
            if class[i] <= c && c <= class[i + 2] {
                return true;
            }
            i += 3;
        } else {
            if class[i] == c {
                return true;
            }
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_lines_parse_into_commit_frames() {
        let line =
            "abc123\0a@e.com\0alice\u{0}2024-01-01 10:00:00 +0000\0p1 p2\0tree1\0subject here";
        let f = parse_commit_line(line).expect("should parse");
        assert_eq!(f.ty, FrameType::Commit);
        assert_eq!(f.field("sha").unwrap().as_str(), Some("abc123"));
        assert_eq!(f.field("author").unwrap().as_str(), Some("alice"));
        assert_eq!(f.field("email").unwrap().as_str(), Some("a@e.com"));
        assert_eq!(f.field("message").unwrap().as_str(), Some("subject here"));
        assert_eq!(f.parents.len(), 2);
        // parents-count is computed, not stored
        assert_eq!(f.field("parents-count"), Some(Value::Num(2)));
    }

    #[test]
    fn a_root_commit_has_no_parents() {
        let line = "abc\0e\0a\0d\0\0t\0msg";
        let f = parse_commit_line(line).unwrap();
        assert_eq!(f.parents.len(), 0);
        assert_eq!(f.field("parents-count"), Some(Value::Num(0)));
    }

    #[test]
    fn malformed_commit_lines_are_dropped_not_guessed() {
        assert!(parse_commit_line("").is_none());
        assert!(parse_commit_line("only\0three\0fields").is_none());
        // empty sha
        assert!(parse_commit_line("\0e\0a\0d\0p\0t\0m").is_none());
    }

    #[test]
    fn ref_lines_need_a_full_hex_sha() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert!(parse_ref_line(Some("branch"), &format!("{sha} main")).is_some());
        // too short
        assert!(parse_ref_line(None, "abc main").is_none());
        // not hex
        assert!(parse_ref_line(None, &format!("{} main", "z".repeat(40))).is_none());
        // no name
        assert!(parse_ref_line(None, sha).is_none());
    }

    #[test]
    fn glob_matching() {
        assert!(path_matches("*.txt", "a.txt"));
        assert!(path_matches("*.txt", "dir/a.txt"), "* crosses /");
        assert!(!path_matches("*.md", "a.txt"));
        assert!(path_matches("a?.txt", "ab.txt"));
        assert!(!path_matches("a?.txt", "abc.txt"));
        assert!(path_matches("[ab].txt", "a.txt"));
        assert!(path_matches("[a-c].txt", "b.txt"));
        assert!(!path_matches("[a-c].txt", "d.txt"));
        assert!(path_matches("[!a].txt", "b.txt"));
    }

    #[test]
    fn glob_falls_back_to_substring() {
        // the elisp original's behaviour: a pattern with no wildcards that
        // is not a full match still matches as a substring
        assert!(path_matches("txt", "a.txt"));
        assert!(path_matches("dir/", "dir/a.txt"));
    }
}
