//! Rendering [`Entry`] values: plain text (colour re-rendered as real ANSI,
//! so `gitq --scrollback | less -R` works) and Emacs Lisp plists (`--sexp`).
//!
//! The plist shape mirrors [`crate::render`]'s frame plists so the Emacs side
//! reads them the same way, but is written for [`Entry`] directly — a
//! scrollback entry is not a git frame, so it is not routed through the frame
//! renderer.  `:output` stays one ANSI-laden string per entry; Emacs owns the
//! ANSI→face mapping (`ansi-color.el`), so it is not duplicated here.

use super::ansi::spans_to_ansi;
use super::entry::Entry;

fn output_text(e: &Entry) -> String {
    e.output
        .iter()
        .map(|l| {
            let mut s = spans_to_ansi(l);
            s.push('\n');
            s
        })
        .collect()
}

/// One human-readable block per entry: a header naming its index and exit
/// code (when known), then the command, then the output with colour restored.
pub fn render_entries_text(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| {
            let exit = match e.exit_code {
                Some(c) => format!(" (exit {c})"),
                None => String::new(),
            };
            format!(
                "=== entry {}{exit} ===\n$ {}\n{}",
                e.index,
                e.command.as_deref().unwrap_or("(no command)"),
                output_text(e)
            )
        })
        .collect()
}

/// An Emacs-Lisp string literal, escaping `"`, backslash, and the escape
/// byte so the reader accepts the ANSI-laden output payload.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{1b}' => out.push_str("\\e"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One Emacs Lisp plist per entry, one per line, e.g.
/// `(:index 0 :command "git status" :exit-code nil :output "…")`.
pub fn render_entries_sexp(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| {
            format!(
                "(:index {} :command {} :exit-code {} :output {})\n",
                e.index,
                e.command.as_deref().map(quote).unwrap_or("nil".into()),
                e.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or("nil".to_string()),
                quote(&output_text(e))
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::entry::parse_entries;
    use super::*;

    #[test]
    fn text_rendering_has_a_header_per_entry() {
        let es = parse_entries("$ ls\na.txt\n$ pwd\n/tmp\n");
        let out = render_entries_text(&es);
        assert!(out.contains("=== entry 0 ==="));
        assert!(out.contains("$ ls"));
        assert!(out.contains("=== entry 1 ==="));
    }

    #[test]
    fn a_commandless_entry_says_so_rather_than_rendering_empty() {
        let es = parse_entries("orphan output\n");
        assert!(render_entries_text(&es).contains("(no command)"));
    }

    #[test]
    fn exit_codes_appear_in_the_header_only_when_known() {
        let es = parse_entries("$ ls\nout\n");
        assert!(!render_entries_text(&es).contains("exit"));
        let raw = "\u{1b}]133;A\u{7}\u{1b}]133;B\u{7}false\u{1b}]133;C\u{7}\u{1b}]133;D;1\u{7}";
        assert!(render_entries_text(&parse_entries(raw)).contains("(exit 1)"));
    }

    #[test]
    fn sexp_escapes_the_escape_byte_so_emacs_can_read_it() {
        let es = parse_entries("$ ls\n\u{1b}[31mred\u{1b}[0m\n");
        let out = render_entries_sexp(&es);
        // a raw ESC would break `read`; it must be the two-char \e
        assert!(!out.contains('\u{1b}'), "raw ESC leaked into the plist");
        assert!(out.contains("\\e["), "{out}");
        // newlines inside the payload are escaped too, so one entry is one line
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn sexp_renders_nil_for_absent_command_and_exit_code() {
        let es = parse_entries("orphan\n");
        let out = render_entries_sexp(&es);
        assert!(out.contains(":command nil"));
        assert!(out.contains(":exit-code nil"));
    }
}
