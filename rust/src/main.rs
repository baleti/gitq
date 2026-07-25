//! gitq — standalone CLI for the GitQ pipeline language.

use std::process::exit;

use gitq::complete::{annotate, complete_candidates};
use gitq::exec::exec_pipeline;
use gitq::git::{toplevel, GitqError};
use gitq::parse::parse_pipeline;
use gitq::render::{put_utf8, render_frames_sexp, render_frames_text};
use gitq::terminal::apply_terminal;

const USAGE: &str = "\
Usage: gitq [--sexp] [--preview] <pipeline>
       gitq --complete <prefix>
       gitq --scrollback [--sexp] [--tmux-target TARGET]
       gitq --scrollback-browse [--tmux-target TARGET]

Examples:
  gitq 'commits take 10'
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
  --tmux-target       with --scrollback[-browse], capture a specific tmux pane";

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

fn main() {
    let mut args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--")
        .collect();

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

    if args.iter().any(|a| a == "--scrollback" || a == "--scrollback-browse") {
        // The scrollback subsystem is not ported yet.  Failing loudly beats
        // accepting the flag and doing nothing, which is the exact silence
        // this port exists to remove.
        eprintln!("gitq: --scrollback is not yet available in this build");
        exit(1);
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
            put_utf8(&render_frames_sexp(&frames));
        } else if frames.is_empty() {
            println!("gitq: (no results) — {pipeline}");
        } else {
            put_utf8(&render_frames_text(&frames));
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
