//! gitq — standalone CLI for the GitQ pipeline language.

use std::process::exit;

use gitq::complete::{annotate, complete_candidates};
use gitq::complete_tui::{run_completer, Outcome, PaletteCommand};
use gitq::exec::exec_pipeline;
use gitq::git::{toplevel, GitqError};
use gitq::parse::parse_pipeline;
use gitq::render::{put_utf8, render_frames_sexp, render_frames_text};
use gitq::scrollback::browse::run_browser;
use gitq::scrollback::capture::{capture_scrollback, CaptureTarget};
use gitq::scrollback::entry::{parse_entries_with, parse_gitq_regions, DEFAULT_PROMPT_REGEX};
use gitq::scrollback::mark;
use gitq::scrollback::render::{render_entries_sexp, render_entries_text};
use gitq::terminal::apply_terminal;

const USAGE: &str = "\
Usage: gitq [--sexp] [--preview] <pipeline>
       gitq --complete <prefix>
       gitq --scrollback [--sexp] [--tmux-target TARGET]
       gitq --scrollback-browse [--tmux-target TARGET]

Examples:
  gitq 'commits [0:10]'
  gitq 'commits where author alice /show'
  gitq 'commits in main..HEAD /count'
  gitq 'HEAD via parent* where message \"fix\"'
  gitq 'commits pickaxe \"needle\" via diff.lines where content \"needle\"'

Flags:
  --sexp          print frames as Emacs Lisp plists (for the Emacs integration)
  --preview       parse and run the source and steps, but never apply a terminal
  --complete      print completion candidates for the given pipeline prefix
  --scrollback        capture the current tmux pane's scrollback and print entries
  --scrollback-browse browse captured scrollback in an interactive TUI
  --gitq-only         with --scrollback[-browse], show only gitq's own output,
                      from the invisible markers gitq emitted (exact; no prompts)
  --complete-tui  interactive columnar completer (prints the chosen pipeline)
  --tmux-target       with --scrollback[-browse], capture a specific tmux pane

Environment:
  GITQ_NO_MARK    set to stop gitq marking its own output in tmux panes
  GITQ_SCROLLBACK_PROMPT_REGEX  override the prompt heuristic (POSIX ERE)";

fn usage() {
    eprintln!("{USAGE}");
}

/// Remove a boolean flag, reporting whether it was present.
fn take_flag(args: &mut Vec<String>, f: &str) -> bool {
    let had = args.iter().any(|a| a == f);
    args.retain(|a| a != f);
    had
}

/// Remove a flag and the argument that follows it as its value.
fn take_val_flag(args: &mut Vec<String>, f: &str) -> Option<String> {
    let i = args.iter().position(|a| a == f)?;
    args.remove(i);
    if i < args.len() {
        Some(args.remove(i))
    } else {
        None
    }
}

/// Die quietly when stdout closes early, the way every other Unix tool
/// does.  Rust masks SIGPIPE at startup and turns the resulting EPIPE into
/// a panic, so `gitq ... | head` printed a panic message and a backtrace
/// hint instead of simply stopping.
#[cfg(unix)]
fn restore_sigpipe() {
    // SAFETY: setting a signal disposition to the default is async-signal
    // safe and is done once, before any threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn main() {
    restore_sigpipe();

    let mut args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--").collect();

    if args.is_empty() {
        usage();
        exit(1);
    }
    if args[0] == "-h" || args[0] == "--help" {
        usage();
        exit(0);
    }

    if args[0] == "--complete" || args[0] == "--complete-annotated" {
        let annotated = args[0] == "--complete-annotated";
        complete(annotated, &args[1..]);
        exit(0);
    }

    if args.iter().any(|a| a == "--complete-tui") {
        args.retain(|a| a != "--complete-tui");
        match run_completer(&args.join(" ")) {
            Ok(Outcome::Accepted(pipeline)) => println!("{pipeline}"),
            // A palette command runs only now, with the completer's terminal
            // already torn down — the browser drives its own.  No pipeline was
            // chosen, so exit non-zero and leave the command line alone.
            Ok(Outcome::Command(PaletteCommand::ScrollbackBrowse)) => {
                if let Err(GitqError(msg)) = scrollback_browse(false, None) {
                    eprintln!("{msg}");
                }
                exit(1);
            }
            // cancelled: print nothing, exit non-zero so the widget leaves
            // the command line untouched
            Ok(Outcome::Cancelled) => exit(1),
            Err(e) => {
                eprintln!("gitq: {e}");
                exit(1);
            }
        }
        exit(0);
    }

    if args.iter().any(|a| a == "--scrollback-browse") {
        args.retain(|a| a != "--scrollback-browse");
        let gitq_only = take_flag(&mut args, "--gitq-only");
        let target = take_val_flag(&mut args, "--tmux-target");
        if let Err(GitqError(msg)) = scrollback_browse(gitq_only, target) {
            eprintln!("{msg}");
            exit(1);
        }
        exit(0);
    }

    if args.iter().any(|a| a == "--scrollback") {
        args.retain(|a| a != "--scrollback");
        let sexp = take_flag(&mut args, "--sexp");
        let gitq_only = take_flag(&mut args, "--gitq-only");
        let target = take_val_flag(&mut args, "--tmux-target");
        if let Err(GitqError(msg)) = scrollback(sexp, gitq_only, target) {
            eprintln!("{msg}");
            exit(1);
        }
        exit(0);
    }

    let sexp = take_flag(&mut args, "--sexp");
    let preview = take_flag(&mut args, "--preview");
    let _ = take_val_flag(&mut args, "--tmux-target");
    let pipeline = args.join(" ");

    if let Err(GitqError(msg)) = run(sexp, preview, &pipeline) {
        eprintln!("{msg}");
        exit(1);
    }
}

/// Completion must never error mid-keystroke; just print what we can.
/// Annotated mode prints "candidate\tkind\tdescription" so callers need
/// neither their own description registry nor their own grammar
/// classification.
fn complete(annotated: bool, rest: &[String]) {
    let Ok(top) = toplevel() else { return };
    if std::env::set_current_dir(&top).is_err() {
        return;
    }
    for c in complete_candidates(&rest.join(" ")) {
        if annotated {
            // a leading `-` (sort negation) describes as its base field
            let base = match c.strip_prefix('-') {
                Some(r) if !r.is_empty() => r,
                _ => &c,
            };
            let (_, kind, _) = annotate(&c);
            let (_, _, desc) = annotate(base);
            println!("{c}\t{kind}\t{desc}");
        } else {
            println!("{c}");
        }
    }
}

fn run(sexp: bool, preview: bool, pipeline: &str) -> Result<(), GitqError> {
    let parsed = match parse_pipeline(pipeline) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    };

    let top = toplevel()?;
    std::env::set_current_dir(&top)
        .map_err(|e| GitqError(format!("gitq: cannot enter {top}: {e}")))?;

    let (frames, term) = exec_pipeline(&parsed)?;

    let display = || {
        if sexp {
            // never marked: Emacs parses this, and an escape it did not ask
            // for is a bug, not a feature
            put_utf8(&render_frames_sexp(&frames));
        } else if frames.is_empty() {
            put_marked(pipeline, &format!("gitq: (no results) — {pipeline}\n"));
        } else {
            put_marked(pipeline, &render_frames_text(&frames));
        }
    };

    match (preview, &term) {
        (true, _) => display(),
        // structured consumers never trigger effects
        (false, Some(_)) if sexp => display(),
        (false, Some(t)) => apply_terminal(&frames, t, pipeline)?,
        (false, None) => display(),
    }
    Ok(())
}

/// Print gitq's own results, invisibly delimited so a later
/// `gitq --scrollback` can recover exactly where they began and ended.
///
/// The markers ride on characters gitq was printing anyway, so this costs no
/// visible columns, and they are emitted only when stdout is a tmux pane —
/// a pipe or a redirect gets clean bytes.  See [`mark`].
fn put_marked(pipeline: &str, text: &str) {
    if mark::marking_enabled() {
        // reached only on the success path; failures print to stderr
        put_utf8(&mark::wrap(&mark::new_id(), Some(pipeline), Some(0), text));
    } else {
        put_utf8(text);
    }
}

fn capture_target(t: Option<String>) -> CaptureTarget {
    match t {
        Some(t) => CaptureTarget::NamedPane(t),
        None => CaptureTarget::CurrentPane,
    }
}

fn prompt_regex() -> String {
    std::env::var("GITQ_SCROLLBACK_PROMPT_REGEX")
        .unwrap_or_else(|_| DEFAULT_PROMPT_REGEX.to_string())
}

/// Capture the tmux pane's scrollback, split it into entries, and print
/// them — human-readable, or as Emacs Lisp plists with `--sexp`.  Unlike a
/// pipeline this needs no git repository; it reads the terminal, not git.
fn scrollback(sexp: bool, gitq_only: bool, target: Option<String>) -> Result<(), GitqError> {
    let raw = capture_scrollback(&capture_target(target))?;
    // The dump is not itself marked: it would nest a fresh region around
    // content that is mostly a replay of older ones.
    let entries = if gitq_only {
        parse_gitq_regions(&raw)
    } else {
        parse_entries_with(&prompt_regex(), &raw)
    };
    put_utf8(&if sexp {
        render_entries_sexp(&entries)
    } else {
        render_entries_text(&entries)
    });
    Ok(())
}

/// Capture and launch the interactive entry browser in this terminal.  This
/// is what the zsh `\eb` widget shells out to (wrapped in `tmux
/// display-popup`).
fn scrollback_browse(gitq_only: bool, target: Option<String>) -> Result<(), GitqError> {
    let raw = capture_scrollback(&capture_target(target))?;
    let entries = if gitq_only {
        parse_gitq_regions(&raw)
    } else {
        parse_entries_with(&prompt_regex(), &raw)
    };
    run_browser(entries).map_err(|e| GitqError(format!("gitq: scrollback browser failed: {e}")))
}
