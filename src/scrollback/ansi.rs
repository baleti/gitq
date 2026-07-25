//! A small SGR (Select Graphic Rendition) scanner over captured pane text.
//!
//! `tmux capture-pane -e` has already done the hard part — it resolved
//! cursor motion, overwrites and scrolling into a flat grid, then re-emitted
//! the visible attributes as plain SGR.  What is left in the text is SGR
//! colour/attribute codes (and the occasional stray non-SGR CSI or OSC that
//! survived), not a live terminal.  So this is a hand-rolled scanner,
//! deliberately *not* a VT100 emulator: it turns SGR runs into styled spans
//! and silently drops everything else.
//!
//! tmux emits SGR split and restated per line (an emitted `ESC[1;31m` comes
//! back as `ESC[1m ESC[31m`, and the active attribute is repeated at the
//! start of each captured line), so callers thread the returned [`Style`]
//! into the next line — see [`parse_ansi_line`].

/// The graphic attributes a span carries.  Colours are SGR palette indices
/// (0–15 for the 8+bright set, 0–255 for 256-colour); `None` means the
/// terminal default.  Truecolour (`38;2;r;g;b`) is out of scope and clears
/// the colour to default rather than approximating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    pub fn is_default(&self) -> bool {
        *self == Style::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub style: Style,
    pub text: String,
}

/// The visible text of a line, with all escape sequences stripped — used to
/// test a captured line against the prompt regex and to recover the typed
/// command from a prompt line.
pub fn visible_text(line: &str) -> String {
    parse_ansi_line(Style::default(), line)
        .1
        .into_iter()
        .map(|s| s.text)
        .collect()
}

/// Parse one line of possibly-SGR-laden text into styled spans, carrying the
/// style forward from the end of the previous line (SGR state persists
/// across newlines in a real terminal).  The returned [`Style`] is that
/// trailing state; pass it to the next line's call to thread a whole buffer.
///
/// Adjacent runs with the same style are coalesced, so a dropped CSI/OSC in
/// the middle of otherwise-uniform text does not split a span.
pub fn parse_ansi_line(sty0: Style, line: &str) -> (Style, Vec<StyledSpan>) {
    let cs: Vec<char> = line.chars().collect();
    let mut sty = sty0;
    let mut pend = String::new();
    let mut spans: Vec<StyledSpan> = Vec::new();
    let mut i = 0;

    // A style change flushes the open span under the OLD style before the
    // new run starts.
    fn flush(spans: &mut Vec<StyledSpan>, style: Style, pend: &mut String) {
        if !pend.is_empty() {
            spans.push(StyledSpan {
                style,
                text: std::mem::take(pend),
            });
        }
    }

    while i < cs.len() {
        if cs[i] != '\u{1b}' {
            pend.push(cs[i]);
            i += 1;
            continue;
        }
        match scan_escape(&cs, i + 1) {
            Esc::Sgr(params, next) => {
                let new = apply_sgr(sty, &parse_params(&params));
                if new != sty {
                    flush(&mut spans, sty, &mut pend);
                    sty = new;
                }
                i = next;
            }
            // merge across a dropped sequence
            Esc::Dropped(next) => i = next,
            // lone ESC, drop it
            Esc::NoSeq => i += 1,
        }
    }
    flush(&mut spans, sty, &mut pend);
    (sty, spans)
}

enum Esc {
    /// SGR parameter text, and the index after the final `m`.
    Sgr(String, usize),
    /// A non-SGR CSI / OSC sequence; carries the index to keep scanning at.
    Dropped(usize),
    /// The ESC did not begin a recognised sequence.
    NoSeq,
}

/// Classify the text immediately after an `ESC`.  A CSI (`ESC [`) ending in
/// `m` is SGR; any other CSI final byte, or an OSC (`ESC ]` … ST/BEL), is
/// dropped.
fn scan_escape(cs: &[char], at: usize) -> Esc {
    let is_param = |c: char| ('\u{30}'..='\u{3f}').contains(&c); // 0-9 : ; < = > ?
    let is_inter = |c: char| ('\u{20}'..='\u{2f}').contains(&c); // space ! " # ... /
    let is_final = |c: char| ('\u{40}'..='\u{7e}').contains(&c); // @ A-Z [ ... ~

    match cs.get(at) {
        Some('[') => {
            let mut j = at + 1;
            let ps = j;
            while j < cs.len() && is_param(cs[j]) {
                j += 1;
            }
            let params: String = cs[ps..j].iter().collect();
            while j < cs.len() && is_inter(cs[j]) {
                j += 1;
            }
            match cs.get(j) {
                Some('m') => Esc::Sgr(params, j + 1),
                Some(&c) if is_final(c) => Esc::Dropped(j + 1),
                // malformed; resync past it
                Some(_) => Esc::Dropped(j),
                // truncated CSI at end of text
                None => Esc::Dropped(cs.len()),
            }
        }
        Some(']') => Esc::Dropped(drop_osc(cs, at + 1)),
        _ => Esc::NoSeq,
    }
}

/// Skip an OSC body up to its terminator (ST = `ESC \`, or a bare BEL),
/// returning the index after it.  An unterminated OSC swallows the rest.
fn drop_osc(cs: &[char], mut i: usize) -> usize {
    while i < cs.len() {
        match cs[i] {
            '\u{7}' => return i + 1,
            '\u{1b}' if cs.get(i + 1) == Some(&'\\') => return i + 2,
            _ => i += 1,
        }
    }
    cs.len()
}

/// Split an SGR parameter string on `;` into codes; an empty field (or a
/// wholly empty parameter string, i.e. `ESC[m`) is 0 (reset).
fn parse_params(ps: &str) -> Vec<i32> {
    if ps.is_empty() {
        return vec![0];
    }
    ps.split(';')
        .map(|f| {
            if f.is_empty() || !f.chars().all(|c| c.is_ascii_digit()) {
                // e.g. a private `:`-subparam; treat as 0
                0
            } else {
                f.parse().unwrap_or(0)
            }
        })
        .collect()
}

/// Fold a list of SGR codes into a [`Style`].  38/48 consume their `5;N`
/// (256-colour) or `2;r;g;b` (truecolour) operands from the tail.
fn apply_sgr(mut s: Style, codes: &[i32]) -> Style {
    let mut i = 0;
    while i < codes.len() {
        let c = codes[i];
        i += 1;
        match c {
            0 => s = Style::default(),
            1 => s.bold = true,
            4 => s.underline = true,
            7 => s.reverse = true,
            22 => s.bold = false,
            24 => s.underline = false,
            27 => s.reverse = false,
            39 => s.fg = None,
            49 => s.bg = None,
            38 | 48 => {
                let target_fg = c == 38;
                match codes.get(i) {
                    Some(5) => {
                        if let Some(&n) = codes.get(i + 1) {
                            let v = Some(n.clamp(0, 255) as u8);
                            if target_fg {
                                s.fg = v
                            } else {
                                s.bg = v
                            }
                            i += 2;
                        }
                    }
                    Some(2) if codes.len() >= i + 4 => {
                        // truecolour is out of scope: clear rather than approximate
                        if target_fg {
                            s.fg = None
                        } else {
                            s.bg = None
                        }
                        i += 4;
                    }
                    _ => {}
                }
            }
            30..=37 => s.fg = Some((c - 30) as u8),
            90..=97 => s.fg = Some((c - 90 + 8) as u8),
            40..=47 => s.bg = Some((c - 40) as u8),
            100..=107 => s.bg = Some((c - 100 + 8) as u8),
            _ => {}
        }
    }
    s
}

/// The SGR sequence that sets exactly this style starting from default.
fn style_sgr(s: &Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if s.bold {
        codes.push("1".into());
    }
    if s.underline {
        codes.push("4".into());
    }
    if s.reverse {
        codes.push("7".into());
    }
    // normal 0-7 use <n0>+index, bright 8-15 use <n1>+(index-8),
    // 256 use <ext>;5;index
    let color = |n0: &str, n1: &str, ext: &str, c: Option<u8>| -> Option<String> {
        let c = c?;
        Some(if c < 8 {
            format!("{n0}{c}")
        } else if c < 16 {
            format!("{n1}{}", c - 8)
        } else {
            format!("{ext};5;{c}")
        })
    };
    codes.extend(color("3", "9", "38", s.fg));
    codes.extend(color("4", "10", "48", s.bg));
    format!("\u{1b}[{}m", codes.join(";"))
}

/// Re-render styled spans back to text with real SGR escapes, so
/// `--scrollback` piped to `less -R` shows colour, and the `--sexp`
/// `:output` string carries ANSI for Emacs's `ansi-color` to apply.  A
/// default-styled span is emitted bare; a styled one is wrapped in its SGR
/// and a trailing reset, so spans stay self-contained.
pub fn spans_to_ansi(spans: &[StyledSpan]) -> String {
    spans
        .iter()
        .map(|s| {
            if s.style.is_default() {
                s.text.clone()
            } else {
                format!("{}{}\u{1b}[0m", style_sgr(&s.style), s.text)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_one_default_span() {
        let (sty, spans) = parse_ansi_line(Style::default(), "hello");
        assert!(sty.is_default());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello");
    }

    #[test]
    fn sgr_splits_spans_and_sets_attributes() {
        let (_, spans) = parse_ansi_line(Style::default(), "a\u{1b}[1;31mb\u{1b}[0mc");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "a");
        assert_eq!(spans[1].text, "b");
        assert!(spans[1].style.bold);
        assert_eq!(spans[1].style.fg, Some(1));
        assert_eq!(spans[2].text, "c");
        assert!(spans[2].style.is_default());
    }

    #[test]
    fn style_threads_across_lines() {
        // tmux restates attributes per line, but a real terminal persists
        // them; the caller threads the returned style into the next line
        let (sty, _) = parse_ansi_line(Style::default(), "\u{1b}[32mgreen");
        assert_eq!(sty.fg, Some(2));
        let (_, spans) = parse_ansi_line(sty, "still green");
        assert_eq!(spans[0].style.fg, Some(2));
    }

    #[test]
    fn bright_and_256_colours() {
        let (_, s) = parse_ansi_line(Style::default(), "\u{1b}[91mx");
        assert_eq!(s[0].style.fg, Some(9));
        let (_, s) = parse_ansi_line(Style::default(), "\u{1b}[38;5;200mx");
        assert_eq!(s[0].style.fg, Some(200));
    }

    #[test]
    fn truecolour_clears_rather_than_approximating() {
        let (_, s) = parse_ansi_line(Style::default(), "\u{1b}[31m\u{1b}[38;2;10;20;30mx");
        assert_eq!(s[0].style.fg, None);
    }

    #[test]
    fn non_sgr_sequences_are_dropped_without_splitting_a_span() {
        // a cursor-position CSI in the middle of uniform text
        let (_, spans) = parse_ansi_line(Style::default(), "ab\u{1b}[2Kcd");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "abcd");
    }

    #[test]
    fn osc_sequences_are_skipped_to_their_terminator() {
        // OSC-8 hyperlink, BEL-terminated
        let (_, spans) = parse_ansi_line(Style::default(), "a\u{1b}]8;;http://x\u{7}b");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "ab");
        // ST-terminated
        let (_, spans) = parse_ansi_line(Style::default(), "a\u{1b}]0;title\u{1b}\\b");
        assert_eq!(spans[0].text, "ab");
    }

    #[test]
    fn visible_text_strips_everything() {
        assert_eq!(
            visible_text("\u{1b}[1;32muser@host\u{1b}[0m$ ls"),
            "user@host$ ls"
        );
    }

    #[test]
    fn bare_esc_bracket_m_resets() {
        let (sty, _) = parse_ansi_line(
            Style {
                bold: true,
                ..Default::default()
            },
            "\u{1b}[mx",
        );
        assert!(sty.is_default());
    }

    #[test]
    fn spans_round_trip_through_ansi() {
        let input = "a\u{1b}[1;31mb\u{1b}[0mc";
        let (_, spans) = parse_ansi_line(Style::default(), input);
        let out = spans_to_ansi(&spans);
        // re-parsing the rendered form must give the same visible text and
        // the same styles
        let (_, again) = parse_ansi_line(Style::default(), &out);
        assert_eq!(
            again.iter().map(|s| s.text.clone()).collect::<String>(),
            "abc"
        );
        assert_eq!(again[1].style.fg, Some(1));
        assert!(again[1].style.bold);
    }

    #[test]
    fn truncated_csi_at_end_of_line_does_not_hang() {
        let (_, spans) = parse_ansi_line(Style::default(), "abc\u{1b}[1");
        assert_eq!(spans[0].text, "abc");
    }
}
