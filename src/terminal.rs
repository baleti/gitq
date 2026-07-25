//! Terminal application for the CLI.  A terminal consumes the pipeline's
//! final frames and performs an effect: display, clipboard, or repository
//! mutation.  A terminal that cannot do what it says errors before doing
//! something else (the fail-loud principle) — a few Emacs-only behaviours
//! of the original are adapted for a real terminal and documented in
//! README.
//!
//! Guards are named preconditions a terminal stacks ahead of its effect.
//! Returning `Result` and using `?` already gives the early-exit the
//! Haskell got from IO + GitqError; what matters is that the vocabulary is
//! shared, so a new terminal composes requirements instead of growing
//! bespoke nested conditionals.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::ast::Terminal;
use crate::frame::Frame;
use crate::git::*;
use crate::render::{put_utf8, render_frames_text};

/// The first frame's commit SHA, or a loud error naming the terminal.
fn first_sha(what: &str, frames: &[Frame]) -> R<String> {
    match frames.first().and_then(Frame::commit_sha) {
        Some(sha) => Ok(sha.to_string()),
        None => gitq_error(format!("gitq {what}: no commit in result")),
    }
}

/// Refuse to act over uncommitted work: a conflicted rewrite on top of a
/// dirty tree would be doing more than the query says.
fn require_clean_tree(what: &str) -> R<()> {
    if run_git(&["status", "--porcelain"]).is_empty() {
        Ok(())
    } else {
        gitq_error(format!(
            "gitq {what}: working tree is not clean; commit or stash first"
        ))
    }
}

/// Refuse to rewrite history the current branch doesn't contain.
fn require_ancestor_of_head(what: &str, sha: &str) -> R<()> {
    let ok = Command::new("git")
        .args(["merge-base", "--is-ancestor", sha, "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        gitq_error(format!(
            "gitq {what}: {} is not an ancestor of HEAD",
            sha.chars().take(8).collect::<String>()
        ))
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Resolve a rev to its full SHA, falling back to the input.
fn full_sha(sha: &str) -> String {
    run_git_string(&["rev-parse", sha]).unwrap_or_else(|| sha.to_string())
}

pub fn apply_terminal(frames: &[Frame], term: &Terminal, pipeline_str: &str) -> R<()> {
    match term {
        Terminal::Show => {
            if frames.is_empty() {
                println!("gitq: (no results) — {pipeline_str}");
            } else {
                put_utf8(&render_frames_text(frames));
            }
        }

        Terminal::Copy => {
            let sha = first_sha("copy", frames)?;
            if copy_to_clipboard(&sha) {
                println!("gitq: copied {}", short(&sha));
            } else {
                return gitq_error(
                    "gitq copy: no clipboard tool found (wl-copy, xclip, xsel, pbcopy)",
                );
            }
        }

        Terminal::Insert => println!("{}", first_sha("insert", frames)?),

        Terminal::Count => println!("{}", frames.len()),

        Terminal::BranchOff(name, wt) => {
            let sha = first_sha("branch-off", frames)?;
            let Some(name) = name else {
                return gitq_error(
                    "gitq branch-off: a branch name is required (/branch-off \"NAME\")",
                );
            };
            match wt {
                Some(path) => run_git(&["worktree", "add", "-b", name, path, &sha]),
                None => run_git(&["checkout", "-b", name, &sha]),
            };
            println!("gitq: created branch '{name}'");
        }

        Terminal::Amend(no_edit, msg) => {
            // git commit --amend only ever rewrites HEAD.  If the pipeline
            // selected some other commit, silently amending HEAD instead
            // would be doing something different from what the query says.
            if let (Some(sel), Some(head)) = (
                frames.first().and_then(Frame::commit_sha),
                run_git_string(&["rev-parse", "HEAD"]),
            ) {
                if let Some(resolved) = run_git_string(&["rev-parse", &sel]) {
                    if resolved != head {
                        return gitq_error(format!(
                            "gitq amend: selected commit {} is not HEAD (amend only rewrites HEAD; use /reword for older commits)",
                            short(&sel)
                        ));
                    }
                }
            }
            match (no_edit, msg) {
                (true, _) => run_git_inherit(&["commit", "--amend", "--no-edit"])?,
                (_, Some(m)) => run_git_inherit(&["commit", "--amend", "-m", m])?,
                (false, None) => run_git_inherit(&["commit", "--amend"])?,
            }
        }

        Terminal::Reword(msg) => {
            let sha = first_sha("reword", frames)?;
            let head = run_git_string(&["rev-parse", "HEAD"]);
            let full = run_git_string(&["rev-parse", &sha]);
            if full == head {
                match msg {
                    Some(m) => run_git_inherit(&["commit", "--amend", "-m", m])?,
                    None => run_git_inherit(&["commit", "--amend"])?,
                }
            } else {
                return gitq_error(format!(
                    "gitq reword: rewording a non-HEAD commit is not implemented in the CLI yet (selected {})",
                    short(&sha)
                ));
            }
        }

        Terminal::Squash(msg) => {
            // inherited stub: reports what it would do
            let suffix = msg
                .as_ref()
                .map(|m| format!(" -> \"{m}\""))
                .unwrap_or_default();
            println!(
                "gitq squash: {} commits{suffix} — not implemented yet",
                frames.len()
            );
        }

        Terminal::Remove => {
            let sha = first_sha("remove", frames)?;
            let full = full_sha(&sha);
            require_clean_tree("remove")?;
            require_ancestor_of_head("remove", &full)?;
            run_git_inherit(&["rebase", "--onto", &format!("{full}^"), &full])?;
            println!("gitq: removed commit {}", short(&full));
        }

        Terminal::Commit(msg) => match msg {
            Some(m) => run_git_inherit(&["commit", "-m", m])?,
            None => run_git_inherit(&["commit"])?,
        },

        Terminal::Stage => {
            run_git(&["add", "--update"]);
            println!("gitq: staged modified files");
        }

        Terminal::Mark(label) => {
            let sha = first_sha("mark", frames)?;
            let Some(label) = label else {
                return gitq_error("gitq mark: a label is required (/mark LABEL)");
            };
            run_git(&["notes", "add", "-m", label, &sha]);
            println!("gitq: marked {} with '{label}'", short(&sha));
        }

        Terminal::Worktree(path) => {
            let sha = first_sha("worktree", frames)?;
            let full = full_sha(&sha);
            let path = match path {
                Some(p) => p.clone(),
                None => {
                    // default path follows the worktree convention:
                    // <repo-root>/.worktree/<full-40-char-hash>
                    format!("{}/.worktree/{full}", toplevel()?)
                }
            };
            run_git(&["worktree", "add", "--detach", &path, &full]);
            println!("gitq: added worktree at {path}");
        }
    }
    Ok(())
}

/// Try the common clipboard tools in order; true if one accepted the text.
pub fn copy_to_clipboard(text: &str) -> bool {
    let tools: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for (cmd, args) in tools {
        let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                continue;
            }
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}
