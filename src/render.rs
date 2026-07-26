//! Frame rendering: plain text for the terminal, s-expressions for the
//! Emacs integration (`--sexp`).
//!
//! Output is written as UTF-8 bytes, bypassing the locale — lenient-UTF-8
//! in, UTF-8 out, symmetrically.

use std::io::Write;

use crate::frame::{Frame, FrameType, Value};

/// Left-justify to `n` characters (never truncates, like `T.justifyLeft`).
fn pad(n: usize, t: &str) -> String {
    let len = t.chars().count();
    if len >= n {
        t.to_string()
    } else {
        format!("{t}{}", " ".repeat(n - len))
    }
}

fn take_chars(n: usize, t: &str) -> String {
    t.chars().take(n).collect()
}

/// A field as display text, `?` when absent.
fn str_of(f: &Frame, k: &str) -> String {
    match f.field(k) {
        Some(Value::Str(s)) => s.to_string(),
        Some(Value::Num(n)) => n.to_string(),
        Some(Value::Bool(b)) => if b { "true" } else { "false" }.to_string(),
        None => "?".to_string(),
    }
}

/// A numeric field as display text, `0` when absent or non-numeric.
fn num_of(f: &Frame, k: &str) -> String {
    match f.field(k) {
        Some(Value::Num(n)) => n.to_string(),
        _ => "0".to_string(),
    }
}

/// A string field, empty when absent.
fn or_empty(f: &Frame, k: &str) -> String {
    match f.field(k) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    }
}

/// One plain-text line per frame, matching the original CLI's format.
pub fn render_frame_line(f: &Frame) -> String {
    match f.ty {
        FrameType::Commit => format!(
            "{}  {}  {}  {}",
            take_chars(8, &str_of(f, "sha")),
            pad(20, &take_chars(20, &str_of(f, "author"))),
            take_chars(10, &str_of(f, "date")),
            or_empty(f, "message")
        ),
        FrameType::Ref => format!(
            "{}  {}",
            pad(40, &str_of(f, "name")),
            take_chars(8, &str_of(f, "sha"))
        ),
        FrameType::Blob => str_of(f, "path"),
        FrameType::Tree => match f.field("path") {
            // subtree entry
            Some(Value::Str(p)) => p.to_string(),
            _ => format!("(tree {})", str_of(f, "sha")),
        },
        FrameType::Worktree => format!(
            "{}  {}",
            pad(40, &str_of(f, "path")),
            match f.field("branch") {
                Some(Value::Str(b)) => b.to_string(),
                _ => "(detached)".to_string(),
            }
        ),
        FrameType::Line => format!(
            "{}:{}: {}",
            str_of(f, "path"),
            num_of(f, "line-number"),
            or_empty(f, "content")
        ),
        FrameType::Hunk => {
            let mut out = String::new();
            if let Some(Value::Str(c)) = f.field("commit-sha") {
                out.push_str(&take_chars(8, &c));
                out.push_str("  ");
            }
            out.push_str(&format!(
                "{}:{}-{}",
                str_of(f, "path"),
                num_of(f, "start-line"),
                num_of(f, "end-line")
            ));
            if let (Some(Value::Str(a)), Some(Value::Str(d))) = (f.field("author"), f.field("date"))
            {
                // 16 chars: `2026-07-21 14:05`.  Hunks from one day are
                // routine, so the day alone cannot tell them apart or order
                // them; seconds would add a column and settle nothing.
                out.push_str(&format!("  {}  {}", a, take_chars(16, &d)));
            }
            if let Some(Value::Str(c)) = f.field("content") {
                if !c.is_empty() {
                    out.push('\n');
                    out.push_str(c.trim_end());
                }
            }
            out
        }
        FrameType::DiffLine => {
            let mut out = String::new();
            if let Some(Value::Str(c)) = f.field("commit-sha") {
                out.push_str(&take_chars(8, &c));
                out.push_str("  ");
            }
            out.push_str(&format!(
                "{}:{}: {}{}",
                str_of(f, "path"),
                num_of(f, "line-number"),
                str_of(f, "sign"),
                or_empty(f, "content")
            ));
            out
        }
        FrameType::Diff => str_of(f, "path"),
        // projected or unknown — key:value pairs
        FrameType::Projection => f
            .attrs
            .iter()
            .map(|(k, v)| format!("{k}:{}", show_val(v)))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn show_val(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Num(n) => n.to_string(),
        Value::Bool(b) => if *b { "t" } else { "nil" }.to_string(),
    }
}

pub fn render_frames_text(frames: &[Frame]) -> String {
    frames
        .iter()
        .map(|f| {
            let mut l = render_frame_line(f);
            l.push('\n');
            l
        })
        .collect()
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn sexp_val(v: &Value) -> String {
    match v {
        Value::Str(s) => quote(s),
        Value::Num(n) => n.to_string(),
        Value::Bool(b) => if *b { "t" } else { "nil" }.to_string(),
    }
}

/// A frame as an Emacs Lisp plist, one per line — the Emacs integration
/// reads these to rebuild frames with text properties.
pub fn render_frame_sexp(f: &Frame) -> String {
    let mut out = format!("(:type {}", f.ty.as_str());
    if !f.parents.is_empty() {
        out.push_str(" :parents (");
        out.push_str(
            &f.parents
                .iter()
                .map(|p| quote(p))
                .collect::<Vec<_>>()
                .join(" "),
        );
        out.push(')');
    }
    for (k, v) in &f.attrs {
        out.push_str(&format!(" :{k} {}", sexp_val(v)));
    }
    out.push(')');
    out
}

pub fn render_frames_sexp(frames: &[Frame]) -> String {
    frames
        .iter()
        .map(|f| {
            let mut l = render_frame_sexp(f);
            l.push('\n');
            l
        })
        .collect()
}

/// Write to stdout as UTF-8 bytes, bypassing the locale.
pub fn put_utf8(t: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(t.as_bytes());
    let _ = lock.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn v(s: &str) -> Value {
        Value::Str(Arc::from(s))
    }

    #[test]
    fn commit_lines_are_column_aligned() {
        let f = Frame::new(
            FrameType::Commit,
            vec![
                ("sha", v("0123456789abcdef")),
                ("author", v("alice")),
                ("date", v("2024-01-01 10:00:00 +0000")),
                ("message", v("initial commit")),
            ],
        );
        let l = render_frame_line(&f);
        // 8-char sha, 20-wide author column, 10-char date
        assert!(l.starts_with("01234567  alice"));
        assert!(l.contains("  2024-01-01  initial commit"));
    }

    #[test]
    fn missing_fields_render_as_question_marks_not_blanks() {
        let f = Frame::new(FrameType::Commit, Vec::<(String, Value)>::new());
        // a missing sha is visible, not silently empty
        assert!(render_frame_line(&f).starts_with('?'));
    }

    #[test]
    fn sexp_escapes_quotes_and_backslashes() {
        let f = Frame::new(FrameType::Commit, vec![("message", v(r#"say "hi" \ ok"#))]);
        let s = render_frame_sexp(&f);
        assert!(s.contains(r#"\"hi\""#), "{s}");
        assert!(s.contains(r"\\"), "{s}");
    }

    #[test]
    fn sexp_renders_parents_and_booleans() {
        let mut f = Frame::new(FrameType::Worktree, vec![("detached", Value::Bool(true))]);
        f.parents = vec![Arc::from("p1"), Arc::from("p2")];
        let s = render_frame_sexp(&f);
        assert!(s.contains(r#":parents ("p1" "p2")"#), "{s}");
        assert!(s.contains(":detached t"), "{s}");
    }

    #[test]
    fn sexp_omits_the_parents_key_when_there_are_none() {
        let f = Frame::new(FrameType::Commit, vec![("sha", v("abc"))]);
        assert!(!render_frame_sexp(&f).contains(":parents"));
    }

    #[test]
    fn projections_render_as_key_value_pairs() {
        let f = Frame::new(
            FrameType::Projection,
            vec![("sha", v("abc")), ("modified", Value::Bool(false))],
        );
        let l = render_frame_line(&f);
        // BTreeMap order: modified before sha
        assert_eq!(l, "modified:nil sha:abc");
    }
}
