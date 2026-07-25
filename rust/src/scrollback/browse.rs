//! The interactive scrollback browser: a ratatui TUI whose movement is
//! *entry-based*, not line-based — `j`/`k` jump whole command+output
//! entries, which is the entire point of the feature (vim keys operating on
//! entry boundaries instead of the terminal's glyph grid).
//!
//! ratatui + crossterm are the one heavy-dependency exception in an
//! otherwise stdlib-light codebase; hand-rolling raw-mode terminal handling
//! is the actual reliability risk they exist to remove.
//!
//! One structural difference from the brick original: brick had a
//! `viewport` widget and a `visible` marker that scrolled the selected
//! widget into view for us.  ratatui has no equivalent, so scrolling is
//! explicit — we record the line offset each entry starts at while building
//! the frame, then clamp the scroll offset so the selection stays on screen.

use std::collections::BTreeSet;
use std::io::Write;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style as RStyle};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use super::ansi::{Style, StyledSpan};
use super::entry::Entry;
use super::render::render_entries_sexp;
use crate::terminal::copy_to_clipboard;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Searching,
}

struct BrowserState {
    entries: Vec<Entry>,
    /// Index into the *visible* entries.
    selected: usize,
    /// `Entry::index` values whose output is collapsed.
    folded: BTreeSet<usize>,
    /// Active filter.
    search: Option<String>,
    mode: Mode,
    /// In-progress search text.
    input: String,
    /// First `g` of a `gg` pressed.
    pending_g: bool,
    status: String,
    scroll: u16,
    quit: bool,
}

impl BrowserState {
    fn visible(&self) -> Vec<&Entry> {
        match &self.search {
            None => self.entries.iter().collect(),
            Some(q) => self
                .entries
                .iter()
                .filter(|e| entry_matches(q, e))
                .collect(),
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.visible().get(self.selected).copied()
    }

    fn move_sel(&mut self, d: isize) {
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let i = (self.selected as isize + d).clamp(0, n as isize - 1);
        self.selected = i as usize;
    }

    fn set_fold(&mut self, fold: bool) {
        if let Some(ix) = self.selected_entry().map(|e| e.index) {
            if fold {
                self.folded.insert(ix);
            } else {
                self.folded.remove(&ix);
            }
        }
    }
}

fn line_text(spans: &[StyledSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn entry_matches(q: &str, e: &Entry) -> bool {
    let ql = q.to_lowercase();
    let in_cmd = e
        .command
        .as_ref()
        .is_some_and(|c| c.to_lowercase().contains(&ql));
    let in_out = e
        .output
        .iter()
        .any(|l| line_text(l).to_lowercase().contains(&ql));
    in_cmd || in_out
}

/// Launch the browser over a list of entries.  Returns when the user quits;
/// a no-op on an empty list (nothing to browse).
pub fn run_browser(entries: Vec<Entry>) -> std::io::Result<()> {
    if entries.is_empty() {
        println!("gitq: (no scrollback entries)");
        return Ok(());
    }

    let mut st = BrowserState {
        entries,
        selected: 0,
        folded: BTreeSet::new(),
        search: None,
        mode: Mode::Normal,
        input: String::new(),
        pending_g: false,
        status: String::new(),
        scroll: 0,
        quit: false,
    };

    let mut term = ratatui::init();
    let res = event_loop(&mut term, &mut st);
    ratatui::restore();
    res
}

fn event_loop(term: &mut DefaultTerminal, st: &mut BrowserState) -> std::io::Result<()> {
    while !st.quit {
        term.draw(|f| draw(f, st))?;
        if let Event::Key(key) = event::read()? {
            // crossterm reports press AND release on some platforms; acting
            // on both would double every keystroke
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match st.mode {
                Mode::Searching => handle_search_key(st, key.code),
                Mode::Normal => handle_normal_key(st, key.code),
            }
        }
    }
    Ok(())
}

fn handle_normal_key(st: &mut BrowserState, code: KeyCode) {
    let pending_g = st.pending_g;
    // any key other than a second 'g' clears a pending 'gg'
    st.pending_g = false;
    st.status.clear();

    match code {
        KeyCode::Char('g') => {
            if pending_g {
                st.selected = 0;
            } else {
                st.pending_g = true;
            }
        }
        KeyCode::Char('G') => st.selected = st.visible().len().saturating_sub(1),
        KeyCode::Char('j') | KeyCode::Down => st.move_sel(1),
        KeyCode::Char('k') | KeyCode::Up => st.move_sel(-1),
        KeyCode::Char('h') => st.set_fold(true),
        KeyCode::Char('l') => st.set_fold(false),
        KeyCode::Char('y') => yank_command(st),
        KeyCode::Char('e') => send_to_emacs(st),
        KeyCode::Char('/') => {
            st.mode = Mode::Searching;
            st.input.clear();
        }
        KeyCode::Char('q') => st.quit = true,
        KeyCode::Esc => {
            st.search = None;
            st.selected = 0;
        }
        _ => {}
    }
}

fn handle_search_key(st: &mut BrowserState, code: KeyCode) {
    match code {
        KeyCode::Enter => {
            st.mode = Mode::Normal;
            st.search = if st.input.is_empty() {
                None
            } else {
                Some(st.input.clone())
            };
            st.selected = 0;
        }
        KeyCode::Esc => {
            st.mode = Mode::Normal;
            st.input.clear();
        }
        KeyCode::Char(c) => st.input.push(c),
        KeyCode::Backspace => {
            st.input.pop();
        }
        _ => {}
    }
}

// --- actions -------------------------------------------------------------

fn yank_command(st: &mut BrowserState) {
    match st.selected_entry().and_then(|e| e.command.clone()) {
        None => st.status = "nothing to yank".into(),
        Some(cmd) => {
            st.status = if copy_to_clipboard(&cmd) {
                "yanked command".into()
            } else {
                "no clipboard tool".into()
            }
        }
    }
}

/// Send the selected entry to Emacs via emacsclient, using the same sexp the
/// CLI's `--scrollback --sexp` produces.  A temp file sidesteps emacsclient's
/// argv escaping/length limits; Emacs deletes it after reading (see
/// `gitq-scrollback-open-from-file`).
fn send_to_emacs(st: &mut BrowserState) {
    let Some(e) = st.selected_entry().cloned() else {
        st.status = "no entry selected".into();
        return;
    };
    st.status = match open_in_emacs(&[e]) {
        Ok(true) => "sent to Emacs".into(),
        Ok(false) => "emacsclient failed".into(),
        Err(m) => m,
    };
}

fn open_in_emacs(entries: &[Entry]) -> Result<bool, String> {
    let path = std::env::temp_dir().join(format!(
        "gitq-scrollback-{}.el",
        std::process::id()
    ));
    let mut f = std::fs::File::create(&path).map_err(|e| format!("temp file failed: {e}"))?;
    f.write_all(render_entries_sexp(entries).as_bytes())
        .map_err(|e| format!("temp file failed: {e}"))?;
    drop(f);

    // Emacs deletes the temp file after reading it, so we don't remove it.
    let form = format!(
        "(gitq-scrollback-open-from-file \"{}\")",
        path.display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
    );
    match std::process::Command::new("emacsclient")
        .args(["-e", &form])
        .output()
    {
        Err(_) => Err("emacsclient not found".into()),
        Ok(o) => Ok(o.status.success()),
    }
}

// --- drawing -------------------------------------------------------------

/// Map a parsed [`Style`] to a ratatui style: SGR palette indices to
/// colours (ISO 0–15, indexed for 16–255), and bold/underline/reverse
/// through.
fn style_to_ratatui(s: &Style) -> RStyle {
    let mut out = RStyle::default();
    if let Some(c) = s.fg {
        out = out.fg(palette_color(c));
    }
    if let Some(c) = s.bg {
        out = out.bg(palette_color(c));
    }
    if s.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if s.underline {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    if s.reverse {
        out = out.add_modifier(Modifier::REVERSED);
    }
    out
}

fn palette_color(c: u8) -> Color {
    match c {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        n => Color::Indexed(n),
    }
}

fn draw(f: &mut Frame, st: &mut BrowserState) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(f.area());
    let body = chunks[0];

    let vis = st.visible();
    if vis.is_empty() {
        f.render_widget(
            Paragraph::new("(no matching entries)").block(Block::default()),
            body,
        );
        render_status(f, chunks[1], st, 0);
        return;
    }

    let sel_attr = RStyle::default().add_modifier(Modifier::REVERSED);
    let idx_attr = RStyle::default().fg(Color::DarkGray);

    let mut lines: Vec<Line> = Vec::new();
    // line offset where each entry starts, so scrolling can target one
    let mut starts: Vec<usize> = Vec::new();
    let mut sel_span: (usize, usize) = (0, 0);

    for (i, e) in vis.iter().enumerate() {
        let selected = i == st.selected;
        starts.push(lines.len());
        let start = lines.len();

        let folded = st.folded.contains(&e.index);
        let mut header: Vec<Span> = vec![
            Span::styled(format!("{:<4}", format!("[{}]", e.index)), idx_attr),
            Span::raw(" "),
            Span::raw(if folded { "▸" } else { "▾" }),
            Span::raw(" "),
            Span::raw(e.command.clone().unwrap_or_else(|| "(no command)".into())),
        ];
        if let Some(c) = e.exit_code {
            let a = if c == 0 {
                RStyle::default().fg(Color::Green)
            } else {
                RStyle::default().fg(Color::Red)
            };
            header.push(Span::raw("  "));
            header.push(Span::styled(format!("exit {c}"), a));
        }
        let mut hline = Line::from(header);
        if selected {
            hline = hline.style(sel_attr);
        }
        lines.push(hline);

        if !folded {
            for out_line in &e.output {
                // keep blank output lines visible
                if out_line.is_empty() {
                    lines.push(Line::from(" "));
                    continue;
                }
                let spans: Vec<Span> = out_line
                    .iter()
                    .map(|s| Span::styled(s.text.clone(), style_to_ratatui(&s.style)))
                    .collect();
                lines.push(Line::from(spans));
            }
        }
        if selected {
            sel_span = (start, lines.len());
        }
    }

    // Scroll so the selected entry is on screen: brick did this for us via
    // `visible`; here we clamp explicitly.
    let h = body.height as usize;
    let (top, bottom) = sel_span;
    let mut scroll = st.scroll as usize;
    if top < scroll {
        scroll = top;
    } else if bottom > scroll + h {
        scroll = bottom.saturating_sub(h);
    }
    scroll = scroll.min(lines.len().saturating_sub(1));
    st.scroll = scroll as u16;

    let total = lines.len();
    f.render_widget(Paragraph::new(lines).scroll((st.scroll, 0)), body);
    render_status(f, chunks[1], st, total);
}

fn render_status(f: &mut Frame, area: ratatui::layout::Rect, st: &BrowserState, _total: usize) {
    let line = match st.mode {
        Mode::Searching => format!("/{}", st.input),
        Mode::Normal => {
            let n = st.visible().len();
            let mut counter = format!("{}/{}", (st.selected + 1).min(n.max(1)), n);
            if let Some(q) = &st.search {
                counter.push_str(&format!("  filter:{q}"));
            }
            let mut parts = vec![counter];
            if !st.status.is_empty() {
                parts.push(st.status.clone());
            }
            parts.push(
                "j/k move  h/l fold  gg/G ends  / search  y yank  e emacs  q quit".to_string(),
            );
            parts.join("   ")
        }
    };
    f.render_widget(
        Paragraph::new(line)
            .style(RStyle::default().add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::entry::parse_entries;

    fn state(raw: &str) -> BrowserState {
        BrowserState {
            entries: parse_entries(raw),
            selected: 0,
            folded: BTreeSet::new(),
            search: None,
            mode: Mode::Normal,
            input: String::new(),
            pending_g: false,
            status: String::new(),
            scroll: 0,
            quit: false,
        }
    }

    const SAMPLE: &str = "$ ls\na.txt\n$ grep needle\nfound needle\n$ pwd\n/tmp\n";

    #[test]
    fn movement_is_entry_based_and_clamped() {
        let mut st = state(SAMPLE);
        assert_eq!(st.visible().len(), 3);
        handle_normal_key(&mut st, KeyCode::Char('j'));
        assert_eq!(st.selected, 1);
        // k past the start clamps rather than wrapping
        handle_normal_key(&mut st, KeyCode::Char('k'));
        handle_normal_key(&mut st, KeyCode::Char('k'));
        assert_eq!(st.selected, 0);
        // G / gg
        handle_normal_key(&mut st, KeyCode::Char('G'));
        assert_eq!(st.selected, 2);
        handle_normal_key(&mut st, KeyCode::Char('g'));
        assert!(st.pending_g);
        handle_normal_key(&mut st, KeyCode::Char('g'));
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn a_lone_g_does_not_jump_and_is_cleared_by_the_next_key() {
        let mut st = state(SAMPLE);
        st.selected = 2;
        handle_normal_key(&mut st, KeyCode::Char('g'));
        handle_normal_key(&mut st, KeyCode::Char('j'));
        assert!(!st.pending_g);
        assert_eq!(st.selected, 2, "g then j must not jump to the top");
    }

    #[test]
    fn folding_tracks_the_entry_not_the_visible_position() {
        let mut st = state(SAMPLE);
        handle_normal_key(&mut st, KeyCode::Char('j'));
        handle_normal_key(&mut st, KeyCode::Char('h'));
        assert!(st.folded.contains(&1));
        handle_normal_key(&mut st, KeyCode::Char('l'));
        assert!(!st.folded.contains(&1));
    }

    #[test]
    fn search_filters_on_command_and_output_case_insensitively() {
        let mut st = state(SAMPLE);
        handle_normal_key(&mut st, KeyCode::Char('/'));
        assert_eq!(st.mode, Mode::Searching);
        for c in "NEEDLE".chars() {
            handle_search_key(&mut st, KeyCode::Char(c));
        }
        handle_search_key(&mut st, KeyCode::Enter);
        assert_eq!(st.mode, Mode::Normal);
        assert_eq!(st.visible().len(), 1);
        assert_eq!(st.visible()[0].command.as_deref(), Some("grep needle"));
    }

    #[test]
    fn escape_clears_the_filter_and_resets_the_selection() {
        let mut st = state(SAMPLE);
        st.search = Some("needle".into());
        st.selected = 0;
        handle_normal_key(&mut st, KeyCode::Esc);
        assert!(st.search.is_none());
        assert_eq!(st.visible().len(), 3);
    }

    #[test]
    fn escape_while_searching_abandons_the_input_without_filtering() {
        let mut st = state(SAMPLE);
        handle_normal_key(&mut st, KeyCode::Char('/'));
        handle_search_key(&mut st, KeyCode::Char('x'));
        handle_search_key(&mut st, KeyCode::Esc);
        assert_eq!(st.mode, Mode::Normal);
        assert!(st.search.is_none());
        assert_eq!(st.visible().len(), 3);
    }

    #[test]
    fn backspace_edits_the_search_input() {
        let mut st = state(SAMPLE);
        handle_normal_key(&mut st, KeyCode::Char('/'));
        handle_search_key(&mut st, KeyCode::Char('a'));
        handle_search_key(&mut st, KeyCode::Char('b'));
        handle_search_key(&mut st, KeyCode::Backspace);
        assert_eq!(st.input, "a");
    }

    #[test]
    fn an_empty_search_clears_the_filter_rather_than_matching_nothing() {
        let mut st = state(SAMPLE);
        st.search = Some("needle".into());
        handle_normal_key(&mut st, KeyCode::Char('/'));
        handle_search_key(&mut st, KeyCode::Enter);
        assert!(st.search.is_none());
    }

    #[test]
    fn movement_stays_in_range_when_a_filter_shrinks_the_list() {
        let mut st = state(SAMPLE);
        st.selected = 2;
        st.search = Some("needle".into());
        // selection is now past the end of the filtered list
        st.move_sel(1);
        assert_eq!(st.selected, 0);
        assert!(st.selected_entry().is_some());
    }

    #[test]
    fn q_sets_quit() {
        let mut st = state(SAMPLE);
        handle_normal_key(&mut st, KeyCode::Char('q'));
        assert!(st.quit);
    }
}
