//! Invisible self-marking of gitq's *own* output, so a later scrollback
//! capture can recover exactly where it started and ended.
//!
//! This is the one part of the scrollback subsystem that is **exact** rather
//! than best-effort, and it earns that by not guessing: gitq owns its own
//! stdout, so instead of inferring boundaries from the shape of the terminal
//! it simply states them.  No shell hooks, no `PROMPT` modification, no
//! prompt regex — a pane is self-describing once gitq has written to it.
//!
//! # The channel
//!
//! The marker is an OSC-8 hyperlink whose URI is a `gitq:` payload, anchored
//! to a character gitq was going to print anyway — the first and last
//! non-newline characters of its output.  That matters for three measured
//! reasons (tmux 3.5a, `capture-pane -e -p -S - -J`):
//!
//! * OSC-8 is the only OSC that survives a tmux capture at all.  OSC 133,
//!   1337, 52 and private numbers are consumed and never reproduced.
//! * tmux keeps a hyperlink only if it is anchored to at least one real
//!   cell; a *zero-width* hyperlink is dropped.  An earlier revision of
//!   `doc/scrollback.org` tested only the zero-width form and wrongly
//!   concluded that no invisible channel exists.
//! * Anchoring to gitq's own characters, rather than to an inserted space,
//!   costs **zero visible columns** — a plain (non-`-e`) capture of marked
//!   output is byte-identical to unmarked output.
//!
//! Markers survive being scrolled out of the visible grid into history, which
//! is the whole point: `gitq --scrollback` reads them back long afterwards.
//!
//! OSC-8 is deliberately used instead of OSC-133 even though tmux *does*
//! index OSC-133 (queryable via copy-mode `next-prompt`).  OSC-133 means
//! "shell prompt/command boundary"; emitting it for gitq's output would
//! corrupt tmux's prompt index and any real shell integration the user runs.
//! A hyperlink is a content attribute and pollutes nothing.
//!
//! # Why it is off unless stdout is a terminal
//!
//! `gitq … | jq`, `--sexp` consumed by Emacs, and `$(gitq …)` in a script
//! must receive clean bytes.  Marking is therefore gated on stdout being a
//! tty *and* `$TMUX` being set, so the escape only ever reaches a pane whose
//! scrollback could later be captured.

use std::fmt::Write as _;

/// URI scheme prefix identifying a marker as gitq's, so unrelated OSC-8
/// hyperlinks in captured output are ignored.
pub const SCHEME: &str = "gitq:";

/// Set to any value to suppress marking entirely.
pub const DISABLE_VAR: &str = "GITQ_NO_MARK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    Begin,
    End,
}

/// One recovered marker.  `id` pairs a `Begin` with its `End`, so interleaved
/// or nested output (a gitq invocation inside another's `$( )`) still matches
/// up, and a region whose `End` scrolled out of history is detectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub kind: MarkKind,
    pub id: String,
    /// The pipeline that produced the region, carried on the `Begin` marker.
    /// Exact, because gitq states its own argv rather than re-reading it off
    /// a prompt line that may have scrolled, wrapped or been edited.
    pub cmd: Option<String>,
    /// Carried on the `End` marker only.
    pub exit: Option<i32>,
}

/// Whether this process should mark its output: inside tmux, writing to a
/// terminal, and not switched off.  Checked once per run by the caller.
pub fn marking_enabled() -> bool {
    std::env::var_os("TMUX").is_some() && std::env::var_os(DISABLE_VAR).is_none() && stdout_is_tty()
}

#[cfg(unix)]
fn stdout_is_tty() -> bool {
    // SAFETY: isatty on a constant fd has no preconditions and no side effects.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

#[cfg(not(unix))]
fn stdout_is_tty() -> bool {
    false
}

/// An identifier unique among the marked regions a pane can plausibly hold.
/// Process id plus a nanosecond timestamp: no uuid dependency, and unique
/// even when the same pid is reused, since the clock has moved.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}

/// One OSC-8 hyperlink wrapping exactly one character.
fn link(payload: &str, ch: char) -> String {
    format!("\u{1b}]8;;{payload}\u{1b}\\{ch}\u{1b}]8;;\u{1b}\\")
}

/// Percent-encode everything that is not an unreserved URI character, so a
/// pipeline containing `;`, a space, a quote or a non-ASCII byte cannot break
/// the field split or the OSC terminator scan.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // a truncated or non-hex escape is kept literally rather than dropped
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn begin_payload(id: &str, cmd: Option<&str>) -> String {
    let mut s = format!("{SCHEME}b;{id}");
    if let Some(c) = cmd {
        let _ = write!(s, ";cmd={}", enc(c));
    }
    s
}

fn end_payload(id: &str, exit: Option<i32>) -> String {
    let mut s = format!("{SCHEME}e;{id}");
    if let Some(c) = exit {
        // `write!` to a String cannot fail
        let _ = write!(s, ";exit={c}");
    }
    s
}

/// Wrap TEXT so its first and last printed characters carry the begin and end
/// markers.  Newlines are skipped as anchors because a newline is not a cell —
/// tmux has nothing to attach the hyperlink to.
///
/// Degenerate inputs are handled rather than marked wrongly: text with no
/// printable character at all is returned untouched (there is no region to
/// delimit), and text with exactly one gets its end marker on an appended
/// space, the only case where marking costs a column.
pub fn wrap(id: &str, cmd: Option<&str>, exit: Option<i32>, text: &str) -> String {
    let anchors: Vec<(usize, char)> = text
        .char_indices()
        .filter(|(_, c)| *c != '\n' && *c != '\r')
        .collect();

    let begin = begin_payload(id, cmd);
    let end = end_payload(id, exit);

    match anchors.len() {
        0 => text.to_string(),
        1 => {
            let (i, c) = anchors[0];
            let mut out = String::with_capacity(text.len() + begin.len() + end.len() + 32);
            out.push_str(&text[..i]);
            out.push_str(&link(&begin, c));
            out.push_str(&link(&end, ' '));
            out.push_str(&text[i + c.len_utf8()..]);
            out
        }
        _ => {
            let (bi, bc) = anchors[0];
            let (ei, ec) = anchors[anchors.len() - 1];
            let mut out = String::with_capacity(text.len() + begin.len() + end.len() + 32);
            out.push_str(&text[..bi]);
            out.push_str(&link(&begin, bc));
            out.push_str(&text[bi + bc.len_utf8()..ei]);
            out.push_str(&link(&end, ec));
            out.push_str(&text[ei + ec.len_utf8()..]);
            out
        }
    }
}

/// Recover gitq markers from one captured line, in order.
///
/// tmux re-emits a cell's hyperlink attribute when it restates a run, so the
/// same payload can appear several times in a row for a single marked
/// character; consecutive duplicates are collapsed.  Non-gitq hyperlinks
/// (`ESC]8;;https://…`) and every other OSC are ignored.
pub fn marks_in(line: &str) -> Vec<Mark> {
    const OSC8: &str = "\u{1b}]8;;";
    let mut out: Vec<Mark> = Vec::new();
    let mut rest = line;

    while let Some(i) = rest.find(OSC8) {
        let body = &rest[i + OSC8.len()..];
        let (payload, after) = split_at_terminator(body);
        if let Some(m) = parse_payload(&payload) {
            // tmux restates the attribute; the same mark twice is one mark
            if out.last() != Some(&m) {
                out.push(m);
            }
        }
        rest = after;
    }
    out
}

/// Split an OSC body at its terminator (ST `ESC \` or BEL), returning the
/// payload and the remainder after it.  An unterminated body consumes the
/// rest of the line rather than looping.
fn split_at_terminator(t: &str) -> (String, &str) {
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x07 {
            return (t[..i].to_string(), &t[i + 1..]);
        }
        if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
            return (t[..i].to_string(), &t[i + 2..]);
        }
        i += 1;
    }
    (t.to_string(), "")
}

fn parse_payload(p: &str) -> Option<Mark> {
    let rest = p.strip_prefix(SCHEME)?;
    let mut parts = rest.split(';');
    let kind = match parts.next()? {
        "b" => MarkKind::Begin,
        "e" => MarkKind::End,
        _ => return None,
    };
    let id = parts.next()?;
    if id.is_empty() {
        return None;
    }
    let mut exit = None;
    let mut cmd = None;
    for f in parts {
        if let Some(v) = f.strip_prefix("exit=") {
            exit = v.parse().ok();
        } else if let Some(v) = f.strip_prefix("cmd=") {
            cmd = Some(dec(v));
        }
    }
    Some(Mark {
        kind,
        id: id.to_string(),
        cmd,
        exit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::ansi::visible_text;

    #[test]
    fn marking_costs_no_visible_columns() {
        let text = "On branch main\nnothing to commit\n";
        let marked = wrap("7f3a", None, Some(0), text);
        assert_ne!(marked, text, "nothing was marked");
        // the whole point: what the user sees is unchanged
        assert_eq!(visible_text(&marked), visible_text(text));
    }

    #[test]
    fn the_markers_anchor_to_the_first_and_last_printable_characters() {
        let marked = wrap("id1", None, None, "ab\ncd\n");
        // begin rides the 'a', end rides the 'd'
        assert!(marked.starts_with("\u{1b}]8;;gitq:b;id1\u{1b}\\a"));
        assert!(marked.contains("\u{1b}]8;;gitq:e;id1\u{1b}\\d"));
        // and the trailing newline is still outside the marked run
        assert!(marked.ends_with('\n'));
    }

    #[test]
    fn a_round_trip_recovers_both_marks_and_the_exit_code() {
        let marked = wrap("abc", None, Some(3), "hello\nworld\n");
        let all: Vec<Mark> = marked.lines().flat_map(marks_in).collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, MarkKind::Begin);
        assert_eq!(all[0].id, "abc");
        assert_eq!(all[1].kind, MarkKind::End);
        assert_eq!(all[1].id, "abc");
        assert_eq!(all[1].exit, Some(3));
    }

    #[test]
    fn the_pipeline_round_trips_through_the_begin_marker() {
        // spaces, quotes and `;` would all break a naive field split
        let pipeline = r#"commits where message "fix; bug" /show"#;
        let marked = wrap("i", Some(pipeline), Some(0), "out\nmore\n");
        let all: Vec<Mark> = marked.lines().flat_map(marks_in).collect();
        assert_eq!(all[0].cmd.as_deref(), Some(pipeline));
        // it rides the begin marker only
        assert_eq!(all[1].cmd, None);
        assert_eq!(visible_text(&marked), "out\nmore\n");
    }

    #[test]
    fn a_non_ascii_pipeline_round_trips() {
        let pipeline = "commits where message \"→ café\"";
        let marked = wrap("i", Some(pipeline), None, "x\ny\n");
        let all: Vec<Mark> = marked.lines().flat_map(marks_in).collect();
        assert_eq!(all[0].cmd.as_deref(), Some(pipeline));
    }

    #[test]
    fn an_end_marker_without_an_exit_code_parses_as_unknown() {
        let m = marks_in("x\u{1b}]8;;gitq:e;q1\u{1b}\\y");
        assert_eq!(m[0].exit, None);
        assert_eq!(m[0].kind, MarkKind::End);
    }

    #[test]
    fn tmux_restating_the_hyperlink_does_not_double_the_mark() {
        // measured: tmux repeats the attribute for a restated run
        let line = "last lin\u{1b}]8;;gitq:e;7f3a;exit=0\u{1b}\\e\
                    \u{1b}]8;;gitq:e;7f3a;exit=0\u{1b}\\\u{1b}]8;;\u{1b}\\";
        assert_eq!(marks_in(line).len(), 1);
    }

    #[test]
    fn unrelated_hyperlinks_and_oscs_are_ignored() {
        let line = "a\u{1b}]8;;https://example.com\u{1b}\\b\u{1b}]0;title\u{7}c";
        assert!(marks_in(line).is_empty());
    }

    #[test]
    fn bel_terminated_markers_parse_too() {
        let m = marks_in("a\u{1b}]8;;gitq:b;zz\u{7}b");
        assert_eq!(m[0].kind, MarkKind::Begin);
        assert_eq!(m[0].id, "zz");
    }

    #[test]
    fn output_with_nothing_printable_is_left_alone() {
        // no cell to anchor to, so marking it would be a lie
        assert_eq!(wrap("i", None, None, ""), "");
        assert_eq!(wrap("i", None, None, "\n\n"), "\n\n");
    }

    #[test]
    fn a_single_character_output_still_gets_both_markers() {
        let marked = wrap("i", None, Some(1), "x\n");
        let all: Vec<Mark> = marked.lines().flat_map(marks_in).collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].exit, Some(1));
    }

    #[test]
    fn multibyte_output_anchors_on_char_boundaries() {
        // slicing by byte index must not split a UTF-8 sequence
        let marked = wrap("i", None, None, "→ ok ←\n");
        assert_eq!(visible_text(&marked), "→ ok ←\n");
        assert_eq!(marks_in(marked.lines().next().unwrap()).len(), 2);
    }

    #[test]
    fn an_unterminated_marker_consumes_the_rest_without_looping() {
        // a capture truncated mid-marker must terminate, not spin
        let m = marks_in("a\u{1b}]8;;gitq:b;truncated");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].id, "truncated");
    }
}
