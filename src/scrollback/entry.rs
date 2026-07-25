//! Splitting a captured scrollback buffer into *entries* — one shell
//! command plus its output.
//!
//! The boundary problem has two strategies, and which one runs is decided
//! per buffer, never mixed:
//!
//! * **Heuristic prompt detection** (the real primary path under tmux): a
//!   line matching a prompt regex starts a new entry; the visible text after
//!   the prompt is the command, the lines until the next prompt are the
//!   output.  Best-effort by nature — a command line that itself looks like
//!   a prompt fools it — and configurable via
//!   `GITQ_SCROLLBACK_PROMPT_REGEX`, which the CLI passes in.
//!
//! * **OSC-133 markers** (exact, but *not* recoverable from a tmux capture —
//!   tmux consumes OSC-133).  Kept for buffers from any future
//!   marker-preserving source; it only engages when `ESC ] 133 ; A` actually
//!   appears, so under tmux it never does.
//!
//! An [`Entry`] is deliberately not a [`crate::frame::Frame`] — a scrollback
//! entry is not a git object — so it gets its own record and renderers.

use regex::Regex;

use super::ansi::{parse_ansi_line, visible_text, Style, StyledSpan};
use super::mark::{marks_in, MarkKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrySource {
    Markers,
    Heuristic,
    /// Boundaries came from gitq's own invisible OSC-8 markers — exact, not
    /// inferred.  See [`crate::scrollback::mark`].
    GitqMark,
}

/// One shell command and its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 0-based, oldest first.
    pub index: usize,
    /// `None` when no command line was recoverable.
    pub command: Option<String>,
    /// Styled output lines, in order.
    pub output: Vec<Vec<StyledSpan>>,
    /// From OSC-133;D; always `None` on the heuristic path.
    pub exit_code: Option<i32>,
    pub source: EntrySource,
}

/// Default prompt matcher, anchored at the start of the (ANSI-stripped)
/// line.  Catches the common interactive shapes — a no-space run ending in a
/// `$` or `%` sigil (`demo$ `, `user@host:~/proj$ `, bare `$ `), or a
/// non-empty run ending in `#` or `>` (`root# `, `host> `).  `#`/`>` require
/// a non-empty prefix so leading markdown headings and quoted lines in
/// captured output are less likely to be mistaken for prompts.  Deliberately
/// best-effort — override with `GITQ_SCROLLBACK_PROMPT_REGEX`.
pub const DEFAULT_PROMPT_REGEX: &str = r"^([^[:space:]]*[$%] |[^[:space:]]+[#>] )";

/// Split a buffer with the default prompt regex.
pub fn parse_entries(raw: &str) -> Vec<Entry> {
    parse_entries_with(DEFAULT_PROMPT_REGEX, raw)
}

/// Split a buffer, choosing the strategy by content: markers if any
/// `ESC ] 133 ; A` is present anywhere, otherwise the heuristic with the
/// given prompt regex.  A buffer with no boundary at all yields a single
/// entry covering everything, never an empty list or a crash.
pub fn parse_entries_with(prompt_rx: &str, raw: &str) -> Vec<Entry> {
    let mut entries = if raw.contains("\u{1b}]133;A") {
        parse_markers(raw)
    } else {
        // An unusable prompt regex falls back to the default rather than
        // producing no entries at all.
        let rx = Regex::new(prompt_rx)
            .or_else(|_| Regex::new(DEFAULT_PROMPT_REGEX))
            .expect("default prompt regex must compile");
        parse_heuristic(&rx, raw)
    };
    for (i, e) in entries.iter_mut().enumerate() {
        e.index = i;
    }
    entries
}

// --- heuristic path ------------------------------------------------------

/// If a line looks like a prompt, the command typed after it (the visible
/// remainder), else `None`.  Runs against the ANSI-stripped line so a
/// coloured prompt still matches.
fn prompt_command(rx: &Regex, line: &str) -> Option<String> {
    let vis = visible_text(line);
    let m = rx.find(&vis)?;
    // the regex is anchored, but be explicit: a match must start at 0
    if m.start() != 0 {
        return None;
    }
    Some(vis[m.end()..].trim().to_string())
}

fn parse_heuristic(rx: &Regex, raw: &str) -> Vec<Entry> {
    let mut lines: Vec<&str> = raw.lines().collect();
    // tmux pads the capture with the pane's blank rows below the cursor;
    // drop trailing whitespace-only lines so the bare trailing prompt
    // collapses to an empty, dropped group instead of a "(no command)" entry
    while lines
        .last()
        .is_some_and(|l| visible_text(l).trim().is_empty())
    {
        lines.pop();
    }

    let mut groups: Vec<(Option<String>, Vec<&str>)> = Vec::new();
    let mut cur_cmd: Option<String> = None;
    let mut acc: Vec<&str> = Vec::new();

    // Yield the in-progress group only if it carries something.  A plain fn
    // rather than a closure: the borrow checker cannot tie the `&str`
    // lifetime in `groups` to the one in `acc` through a closure's inferred
    // signature.
    fn emit<'a>(
        groups: &mut Vec<(Option<String>, Vec<&'a str>)>,
        cmd: &mut Option<String>,
        acc: &mut Vec<&'a str>,
    ) {
        if cmd.is_some() || !acc.is_empty() {
            groups.push((cmd.take(), std::mem::take(acc)));
        }
    }

    for l in lines {
        match prompt_command(rx, l) {
            Some(command) => {
                emit(&mut groups, &mut cur_cmd, &mut acc);
                // an empty command (bare prompt, e.g. the trailing live
                // prompt every capture ends on) becomes None, so a
                // content-free group is dropped
                cur_cmd = if command.is_empty() {
                    None
                } else {
                    Some(command)
                };
            }
            None => acc.push(l),
        }
    }
    emit(&mut groups, &mut cur_cmd, &mut acc);

    groups
        .into_iter()
        .map(|(command, out_lines)| {
            // Where gitq marked its own output, those boundaries are exact
            // and win over the lines the prompt heuristic happened to group.
            match marked_region(&out_lines) {
                Some((region, exit)) => Entry {
                    index: 0,
                    command,
                    output: style_lines(region.iter().map(|s| s.to_string()).collect()),
                    exit_code: exit,
                    source: EntrySource::GitqMark,
                },
                None => Entry {
                    index: 0,
                    command,
                    output: style_lines(out_lines.iter().map(|s| s.to_string()).collect()),
                    exit_code: None,
                    source: EntrySource::Heuristic,
                },
            }
        })
        .collect()
}

// --- gitq's own markers --------------------------------------------------

/// The slice of LINES that gitq marked as its own output, plus the exit code
/// its end marker carried.
///
/// An end marker that never arrived (the region is still being written, or
/// its tail has already been evicted from the history limit) is not a failure:
/// the region simply runs to the end of what we have, with an unknown exit
/// code.  Only the *first* region in a group is taken — a group holding two
/// gitq invocations means the prompt heuristic under-split, and guessing
/// which one the user meant would be worse than reporting the first.
fn marked_region<'a>(lines: &[&'a str]) -> Option<(Vec<&'a str>, Option<i32>)> {
    let mut begin: Option<(usize, String)> = None;

    for (i, l) in lines.iter().enumerate() {
        for m in marks_in(l) {
            match (&begin, m.kind) {
                (None, MarkKind::Begin) => begin = Some((i, m.id)),
                (Some((b, id)), MarkKind::End) if *id == m.id => {
                    return Some((lines[*b..=i].to_vec(), m.exit));
                }
                _ => {}
            }
        }
    }

    begin.map(|(b, _)| (lines[b..].to_vec(), None))
}

/// Wrapper mirroring [`marked_region`] but keeping the begin marker's
/// recorded pipeline, which the exact path reports as the entry's command.
fn marked_region_cmd(lines: &[&str]) -> Option<String> {
    lines.iter().flat_map(|l| marks_in(l)).find_map(|m| m.cmd)
}

/// Every gitq-marked region in a captured buffer, ignoring prompts entirely.
///
/// This is the exact path: it needs no prompt regex, no shell integration and
/// no configuration, because gitq stated these boundaries itself when it
/// printed.  Backs `--scrollback --gitq-only`.
pub fn parse_gitq_regions(raw: &str) -> Vec<Entry> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut entries: Vec<Entry> = Vec::new();
    let mut open: Option<(usize, String)> = None;

    for (i, l) in lines.iter().enumerate() {
        for m in marks_in(l) {
            match (&open, m.kind) {
                (None, MarkKind::Begin) => open = Some((i, m.id)),
                (Some((b, id)), MarkKind::End) if *id == m.id => {
                    entries.push(region_entry(&lines[*b..=i], m.exit));
                    open = None;
                }
                _ => {}
            }
        }
    }
    // a region whose end marker is missing still carries usable output
    if let Some((b, _)) = open {
        entries.push(region_entry(&lines[b..], None));
    }

    for (i, e) in entries.iter_mut().enumerate() {
        e.index = i;
    }
    entries
}

fn region_entry(lines: &[&str], exit: Option<i32>) -> Entry {
    Entry {
        index: 0,
        // the pipeline gitq recorded when it printed, not one read back off a
        // prompt line
        command: marked_region_cmd(lines),
        output: style_lines(lines.iter().map(|s| s.to_string()).collect()),
        exit_code: exit,
        source: EntrySource::GitqMark,
    }
}

// --- marker path (OSC-133) -----------------------------------------------

const INTRO_MARKER: &str = "\u{1b}]133;";

enum Mark {
    PromptStart,
    CmdStart,
    OutputStart,
    CmdEnd(Option<i32>),
}

enum Tok {
    Text(String),
    Mark(Mark),
}

/// Break an OSC body at its terminator (ST `ESC \` or BEL), returning the
/// payload before it and the text after.
fn break_osc_term(t: &str) -> (String, &str) {
    let cs: Vec<char> = t.chars().collect();
    let mut acc = String::new();
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '\u{7}' => {
                let consumed: usize = cs[..=i].iter().map(|c| c.len_utf8()).sum();
                return (acc, &t[consumed..]);
            }
            '\u{1b}' if cs.get(i + 1) == Some(&'\\') => {
                let consumed: usize = cs[..i + 2].iter().map(|c| c.len_utf8()).sum();
                return (acc, &t[consumed..]);
            }
            c => {
                acc.push(c);
                i += 1;
            }
        }
    }
    (acc, "")
}

fn parse_mark(payload: &str) -> Option<Mark> {
    let mut cs = payload.chars();
    match cs.next()? {
        'A' => Some(Mark::PromptStart),
        'B' => Some(Mark::CmdStart),
        'C' => Some(Mark::OutputStart),
        'D' => {
            let rest: String = cs.collect();
            let code = rest.strip_prefix(';').and_then(|c| {
                if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_digit()) {
                    c.parse().ok()
                } else {
                    None
                }
            });
            Some(Mark::CmdEnd(code))
        }
        _ => None,
    }
}

/// Tokenise a buffer into interleaved plain text and 133 markers, splitting
/// on `ESC ] 133 ;` up to each OSC terminator.
fn tokenize_markers(mut t: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    loop {
        match t.find(INTRO_MARKER) {
            None => {
                if !t.is_empty() {
                    out.push(Tok::Text(t.to_string()));
                }
                return out;
            }
            Some(i) => {
                if i > 0 {
                    out.push(Tok::Text(t[..i].to_string()));
                }
                let body = &t[i + INTRO_MARKER.len()..];
                let (payload, after) = break_osc_term(body);
                if let Some(m) = parse_mark(&payload) {
                    out.push(Tok::Mark(m));
                }
                t = after;
            }
        }
    }
}

/// Walk the 133 token stream, building entries.  Text between `;B` and `;C`
/// is the command; between `;C` and `;D` is output; `;D;code` closes the
/// entry with its exit code.  Prompt text (`;A`..`;B`) is discarded.
fn parse_markers(raw: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut cmd: Option<String> = None;
    let mut out: Vec<String> = Vec::new();

    fn make(cmd: Option<String>, code: Option<i32>, out: &[String]) -> Entry {
        let joined = out.concat();
        Entry {
            index: 0,
            command: cmd,
            output: style_lines(joined.lines().map(str::to_string).collect()),
            exit_code: code,
            source: EntrySource::Markers,
        }
    }

    for tok in tokenize_markers(raw) {
        match tok {
            Tok::Text(txt) => out.push(txt),
            Tok::Mark(Mark::PromptStart) => {
                if cmd.is_some() || !out.is_empty() {
                    entries.push(make(cmd.take(), None, &out));
                    out.clear();
                }
            }
            // discard prompt text, await command
            Tok::Mark(Mark::CmdStart) => {
                cmd = Some(String::new());
                out.clear();
            }
            // accumulated text was the command
            Tok::Mark(Mark::OutputStart) => {
                if cmd.is_some() {
                    cmd = Some(out.concat().trim().to_string());
                }
                out.clear();
            }
            Tok::Mark(Mark::CmdEnd(code)) => {
                entries.push(make(cmd.take(), code, &out));
                out.clear();
            }
        }
    }
    if cmd.is_some() || !out.is_empty() {
        entries.push(make(cmd, None, &out));
    }
    entries
}

// --- shared --------------------------------------------------------------

/// Parse raw output lines into styled spans, threading SGR state across the
/// lines of a single entry (reset to default at each entry boundary).
fn style_lines(lines: Vec<String>) -> Vec<Vec<StyledSpan>> {
    let mut sty = Style::default();
    lines
        .into_iter()
        .map(|l| {
            let (s, spans) = parse_ansi_line(sty, &l);
            sty = s;
            spans
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(e: &Entry) -> String {
        e.output
            .iter()
            .map(|l| {
                let mut s: String = l.iter().map(|sp| sp.text.clone()).collect();
                s.push('\n');
                s
            })
            .collect()
    }

    #[test]
    fn heuristic_splits_on_prompt_lines() {
        let raw = "user@host$ ls\na.txt\nb.txt\nuser@host$ echo hi\nhi\nuser@host$ ";
        let es = parse_entries(raw);
        assert_eq!(es.len(), 2);
        assert_eq!(es[0].command.as_deref(), Some("ls"));
        assert_eq!(text_of(&es[0]), "a.txt\nb.txt\n");
        assert_eq!(es[1].command.as_deref(), Some("echo hi"));
        assert_eq!(es[1].index, 1);
        assert_eq!(es[0].source, EntrySource::Heuristic);
    }

    #[test]
    fn the_trailing_live_prompt_does_not_become_an_empty_entry() {
        // every capture ends on the prompt the user is sitting at
        let es = parse_entries("$ ls\na.txt\n$ ");
        assert_eq!(es.len(), 1);
    }

    #[test]
    fn tmux_blank_padding_below_the_cursor_is_trimmed() {
        let es = parse_entries("$ ls\na.txt\n$ \n\n   \n\n");
        assert_eq!(es.len(), 1);
        assert_eq!(text_of(&es[0]), "a.txt\n");
    }

    #[test]
    fn output_before_any_prompt_becomes_a_commandless_entry() {
        let es = parse_entries("leftover output\n$ ls\na.txt\n");
        assert_eq!(es.len(), 2);
        assert_eq!(es[0].command, None);
        assert_eq!(text_of(&es[0]), "leftover output\n");
    }

    #[test]
    fn a_buffer_with_no_prompt_at_all_is_one_entry() {
        let es = parse_entries("just\nsome\noutput\n");
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].command, None);
    }

    #[test]
    fn an_empty_buffer_yields_no_entries_rather_than_crashing() {
        assert!(parse_entries("").is_empty());
        assert!(parse_entries("\n\n  \n").is_empty());
    }

    #[test]
    fn coloured_prompts_still_match() {
        let raw = "\u{1b}[1;32muser@host\u{1b}[0m$ ls\na.txt\n";
        let es = parse_entries(raw);
        assert_eq!(es[0].command.as_deref(), Some("ls"));
    }

    #[test]
    fn prompt_shapes_the_default_regex_accepts() {
        for p in [
            "$ ",
            "demo$ ",
            "user@host:~/proj$ ",
            "% ",
            "root# ",
            "host> ",
        ] {
            let es = parse_entries(&format!("{p}cmd\nout\n"));
            assert_eq!(es[0].command.as_deref(), Some("cmd"), "prompt {p:?}");
        }
    }

    #[test]
    fn a_custom_prompt_regex_is_honoured() {
        let es = parse_entries_with(r"^PROMPT> ", "PROMPT> ls\na.txt\n");
        assert_eq!(es[0].command.as_deref(), Some("ls"));
    }

    #[test]
    fn an_invalid_custom_regex_falls_back_instead_of_yielding_nothing() {
        let es = parse_entries_with(r"^([unclosed", "$ ls\na.txt\n");
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].command.as_deref(), Some("ls"));
    }

    #[test]
    fn osc133_markers_win_when_present_and_carry_exit_codes() {
        let raw = "\u{1b}]133;A\u{7}user@host$ \u{1b}]133;B\u{7}git status\
                   \u{1b}]133;C\u{7}nothing to commit\n\u{1b}]133;D;0\u{7}";
        let es = parse_entries(raw);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].source, EntrySource::Markers);
        assert_eq!(es[0].command.as_deref(), Some("git status"));
        assert_eq!(es[0].exit_code, Some(0));
        assert_eq!(text_of(&es[0]), "nothing to commit\n");
    }

    #[test]
    fn marker_exit_codes_survive_nonzero() {
        let raw = "\u{1b}]133;A\u{7}\u{1b}]133;B\u{7}false\u{1b}]133;C\u{7}\u{1b}]133;D;1\u{7}";
        let es = parse_entries(raw);
        assert_eq!(es[0].exit_code, Some(1));
    }

    // --- gitq's own markers ----------------------------------------------

    use super::super::mark::wrap;

    #[test]
    fn gitq_marks_give_an_entry_exact_bounds_and_a_real_exit_code() {
        // the heuristic would hand the whole group to the entry; the markers
        // narrow it to precisely what gitq printed
        let raw = format!(
            "$ gitq 'commits take 1'\n{}stray line after\n",
            wrap("i1", None, Some(0), "abc123 first commit\n")
        );
        let es = parse_entries(&raw);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].source, EntrySource::GitqMark);
        assert_eq!(es[0].exit_code, Some(0));
        assert_eq!(text_of(&es[0]), "abc123 first commit\n");
    }

    #[test]
    fn an_unmarked_entry_stays_heuristic() {
        let es = parse_entries("$ ls\na.txt\n");
        assert_eq!(es[0].source, EntrySource::Heuristic);
        assert_eq!(es[0].exit_code, None);
    }

    #[test]
    fn gitq_only_finds_regions_without_any_prompt() {
        // no prompt anywhere: the exact path does not need one
        let raw = format!(
            "noise\n{}\nmore noise\n{}",
            wrap("a", None, Some(0), "first result\n"),
            wrap("b", None, Some(2), "second result\nline two\n")
        );
        let es = parse_gitq_regions(&raw);
        assert_eq!(es.len(), 2);
        assert_eq!(text_of(&es[0]), "first result\n");
        assert_eq!(es[0].exit_code, Some(0));
        assert_eq!(text_of(&es[1]), "second result\nline two\n");
        assert_eq!(es[1].exit_code, Some(2));
        assert_eq!(es[1].index, 1);
    }

    #[test]
    fn a_region_whose_end_marker_was_evicted_still_yields_output() {
        // history-limit eviction truncates the tail, losing the end marker
        let full = wrap("z", None, Some(0), "kept line\ncut line\n");
        let truncated: String = full.lines().next().unwrap().to_string();
        let es = parse_gitq_regions(&truncated);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].exit_code, None, "an absent end marker is not exit 0");
        assert_eq!(text_of(&es[0]), "kept line\n");
    }

    #[test]
    fn gitq_only_on_a_buffer_with_no_markers_is_empty_not_everything() {
        assert!(parse_gitq_regions("$ ls\na.txt\nb.txt\n").is_empty());
    }

    #[test]
    fn mismatched_marker_ids_do_not_close_a_region() {
        // an interleaved end from a different invocation must not truncate
        let raw = "\u{1b}]8;;gitq:b;aa\u{1b}\\X\nmiddle\n\u{1b}]8;;gitq:e;bb\u{1b}\\Y\ntail\n";
        let es = parse_gitq_regions(raw);
        assert_eq!(es.len(), 1);
        // ran to the end rather than closing on the foreign id
        assert!(text_of(&es[0]).contains("tail"));
    }

    #[test]
    fn markers_never_leak_into_the_visible_output() {
        let raw = wrap("i", None, Some(0), "On branch main\n");
        let es = parse_gitq_regions(&raw);
        let t = text_of(&es[0]);
        assert_eq!(t, "On branch main\n");
        assert!(!t.contains('\u{1b}'), "escape leaked into entry text");
    }

    #[test]
    fn st_terminated_markers_parse_too() {
        let raw = "\u{1b}]133;A\u{1b}\\\u{1b}]133;B\u{1b}\\ls\u{1b}]133;C\u{1b}\\out\n\u{1b}]133;D;0\u{1b}\\";
        let es = parse_entries(raw);
        assert_eq!(es[0].command.as_deref(), Some("ls"));
        assert_eq!(es[0].exit_code, Some(0));
    }
}
