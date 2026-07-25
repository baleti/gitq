//! Capture a tmux pane's scrollback as raw text.
//!
//! The whole scrollback subsystem is gated on tmux: outside it there is no
//! portable way for a program to read back previously-printed terminal
//! output.  So this module shells out to `tmux capture-pane` and fails loud
//! (the house [`GitqError`]) when `$TMUX` or the `tmux` binary is missing,
//! rather than degrading to some emulator-specific hack.
//!
//! Bytes in, lenient-UTF-8 decoded once — same discipline as [`crate::git`],
//! so a latin-1 byte in captured command output can't throw.

use std::process::Command;

use crate::git::{gitq_error, GitqError, R};

/// Which tmux pane to read.  `CurrentPane` resolves to whatever pane tmux
/// considers current for the invoking client — what you want when a zsh
/// widget inside a pane calls gitq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    CurrentPane,
    /// tmux target-pane syntax, e.g. `session:0.1`
    NamedPane(String),
}

/// Capture full scrollback + visible screen with SGR escape sequences
/// intact, so [`crate::scrollback::ansi`] can recover colour/attributes.
/// Errors loudly when tmux isn't available.
///
/// Flags: `-e` keeps SGR escapes; `-p` prints to stdout; `-S -` starts at
/// the beginning of history (full scrollback); `-J` joins wrapped lines so
/// an 80-column-wrapped command line stays one logical line — without it,
/// boundary detection would split on the pane's soft wraps.
///
/// Note that tmux consumes OSC sequences (including OSC-133
/// shell-integration markers) rather than reproducing them in the grid
/// capture; only SGR and anchored OSC-8 hyperlinks round-trip.  See
/// `doc/scrollback.org`.
pub fn capture_scrollback(target: &CaptureTarget) -> R<String> {
    if std::env::var_os("TMUX").is_none() {
        return gitq_error(
            "gitq: scrollback needs tmux ($TMUX not set) — run inside a tmux session",
        );
    }

    let mut args: Vec<&str> = vec!["capture-pane", "-e", "-p", "-S", "-", "-J"];
    if let CaptureTarget::NamedPane(t) = target {
        args.push("-t");
        args.push(t);
    }

    let out = Command::new("tmux")
        .args(&args)
        .output()
        .map_err(|_| GitqError("gitq: scrollback needs the tmux binary on $PATH".into()))?;

    if !out.status.success() {
        return gitq_error(
            "gitq: scrollback capture failed (tmux capture-pane returned an error)",
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
