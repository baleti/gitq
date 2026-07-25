//! Interactive columnar completer for gitq pipelines.
//!
//! Phase 1 (this file): one fuzzy-filtered candidate column on the left and
//! a wide live preview on the right.  Typing fuzzy-filters; Up/Down (or
//! C-n/C-p) move; Tab commits the highlighted candidate and opens the next
//! position, so Tab-after-Tab builds the pipeline one token at a time; Enter
//! accepts and prints the pipeline to stdout; Esc/C-c/C-g cancel.
//!
//! It is a *front-end over gitq's own brain*, not a second one: candidates
//! and their kinds come from [`complete_candidates`]/[`annotate`] (the same
//! registry the parser, zsh and Emacs use), and the preview is gitq's own
//! `--preview` — parse, run the source and steps, render, with a parse/exec
//! error shown as text.  So it can never disagree with the language.
//!
//! ratatui + crossterm are already the scrollback browser's dependency.  We
//! render to `/dev/tty` rather than stdout (as `ratatui::init()` would), so
//! stdout stays free to carry the chosen pipeline back to the zsh widget.
//!
//! Everything above the terminal — the split of the input, fuzzy scoring,
//! and the state machine (filter / commit / accept / go-back) — is pure and
//! unit-tested, because the live TUI can't be driven from a test.

use std::fs::OpenOptions;
use std::io;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::{execute, terminal::Clear, terminal::ClearType};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;

use crate::complete::{annotate, complete_candidates};
use crate::exec::exec_pipeline;
use crate::git::{toplevel, GitqError};
use crate::parse::parse_pipeline;
use crate::render::render_frames_text;

/// One completion candidate with its registry kind and description.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cand {
    text: String,
    kind: &'static str,
    desc: &'static str,
}

/// Candidates valid at the position PIPELINE leaves off, annotated exactly as
/// `gitq --complete-annotated` would (kind from the candidate, description
/// from its `-`-stripped base so `-date` reads as `date`).
fn candidates_for(pipeline: &str) -> Vec<Cand> {
    complete_candidates(pipeline)
        .into_iter()
        .map(|c| {
            let base = match c.strip_prefix('-') {
                Some(r) if !r.is_empty() => r,
                _ => c.as_str(),
            };
            let (_, kind, _) = annotate(&c);
            let (_, _, desc) = annotate(base);
            Cand {
                text: c,
                kind,
                desc,
            }
        })
        .collect()
}

/// Split a pipeline string into (committed head, trailing partial token).
/// The head keeps its trailing space; the partial is what the user is still
/// typing and becomes the initial fuzzy query.  Quote-aware so a value with
/// spaces (`message "fix bug`) isn't split mid-string.
fn split_last_token(s: &str) -> (String, String) {
    let (mut in_s, mut in_d) = (false, false);
    let mut last_space: Option<usize> = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '\'' if !in_d => in_s = !in_s,
            '"' if !in_s => in_d = !in_d,
            c if c.is_whitespace() && !in_s && !in_d => last_space = Some(i),
            _ => {}
        }
    }
    match last_space {
        Some(i) => (s[..=i].to_string(), s[i + 1..].to_string()),
        None => (String::new(), s.to_string()),
    }
}

/// A subsequence fuzzy score, higher is better, `None` if QUERY is not a
/// subsequence of CAND.  Rewards a match at the start, contiguous runs, and
/// tighter matches (fewer leftover characters) — enough for candidate lists
/// of a few dozen; not trying to be nucleo.
fn fuzzy_score(query: &str, cand: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let c: Vec<char> = cand.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0;
    let mut last: Option<usize> = None;
    for (ci, ch) in c.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            if ci == 0 {
                score += 10;
            }
            if let Some(l) = last {
                if ci == l + 1 {
                    score += 5;
                }
            }
            score += 1;
            last = Some(ci);
            qi += 1;
        }
    }
    if qi == q.len() {
        Some(score - (c.len() as i32 - q.len() as i32))
    } else {
        None
    }
}

/// The completer's state.  Pure with respect to the terminal: every field is
/// driven by [`CompleterState::handle`], which the tests exercise directly.
struct CompleterState {
    /// Committed pipeline text, ending in a space when non-empty.
    pipeline: String,
    /// The fuzzy query for the current position (the still-typed token).
    query: String,
    /// All candidates for the current position.
    all: Vec<Cand>,
    /// Indices into `all` matching `query`, best first.
    filtered: Vec<usize>,
    /// Index into `filtered`.
    selected: usize,
    /// Set when the user accepts; the string to print to stdout.
    accepted: Option<String>,
    quit: bool,
}

impl CompleterState {
    fn new(input: &str) -> Self {
        let (head, partial) = split_last_token(input);
        let mut st = CompleterState {
            pipeline: head,
            query: partial,
            all: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            accepted: None,
            quit: false,
        };
        st.reload_candidates();
        st
    }

    fn reload_candidates(&mut self) {
        self.all = candidates_for(&self.pipeline);
        self.refilter();
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(usize, i32)> = self
            .all
            .iter()
            .enumerate()
            .filter_map(|(i, c)| fuzzy_score(&self.query, &c.text).map(|s| (i, s)))
            .collect();
        // stable sort by score desc keeps the registry order among ties
        scored.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
        self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn move_sel(&mut self, d: isize) {
        let n = self.filtered.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + d).clamp(0, n as isize - 1) as usize;
    }

    fn highlighted(&self) -> Option<&Cand> {
        self.filtered.get(self.selected).map(|&i| &self.all[i])
    }

    /// The token the current state would contribute: the highlighted
    /// candidate, or the raw query when nothing matches (a typed-in value).
    fn current_token(&self) -> String {
        self.highlighted()
            .map(|c| c.text.clone())
            .unwrap_or_else(|| self.query.clone())
    }

    /// The pipeline as it stands with the current token — what the preview
    /// runs and what Enter accepts.
    fn effective(&self) -> String {
        format!("{}{}", self.pipeline, self.current_token())
            .trim()
            .to_string()
    }

    fn commit(&mut self) {
        let tok = self.current_token();
        if tok.is_empty() {
            return;
        }
        self.pipeline = format!("{}{} ", self.pipeline, tok);
        self.query.clear();
        self.selected = 0;
        self.reload_candidates();
    }

    /// Backspace on an empty query steps back a whole committed token — the
    /// pared-down version of Phase 2's column history.
    fn pop_token(&mut self) {
        let (head, _) = split_last_token(self.pipeline.trim_end());
        self.pipeline = head;
        self.selected = 0;
        self.reload_candidates();
    }

    fn accept(&mut self) {
        self.accepted = Some(self.effective());
        self.quit = true;
    }

    fn handle(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c' | 'g') if ctrl => self.quit = true,
            KeyCode::Enter => self.accept(),
            KeyCode::Tab => self.commit(),
            KeyCode::BackTab | KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::Char('p') if ctrl => self.move_sel(-1),
            KeyCode::Char('n') if ctrl => self.move_sel(1),
            KeyCode::Backspace => {
                if self.query.is_empty() {
                    self.pop_token();
                } else {
                    self.query.pop();
                    self.refilter();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.selected = 0;
                self.refilter();
            }
            _ => {}
        }
    }
}

/// Preview text for a pipeline: gitq's own `--preview`, with a parse or exec
/// error shown verbatim (so navigating onto a not-yet-complete step explains
/// what it still wants).
fn preview_text(pipeline: &str) -> String {
    let p = pipeline.trim();
    if p.is_empty() {
        return String::new();
    }
    match parse_pipeline(p) {
        Err(e) => e.to_string(),
        Ok(parsed) => match exec_pipeline(&parsed) {
            Err(GitqError(m)) => m,
            Ok((frames, _term)) => {
                if frames.is_empty() {
                    "(no results)".to_string()
                } else {
                    render_frames_text(&frames)
                }
            }
        },
    }
}

/// Run the completer over the pipeline typed so far.  Returns the chosen
/// pipeline on accept, or `None` on cancel.  Draws on `/dev/tty`, leaving
/// stdout for the result.
pub fn run_completer(input: &str) -> io::Result<Option<String>> {
    // exec_pipeline (the preview) resolves paths relative to the repo root,
    // like `gitq --complete` and `run` do.
    if let Ok(top) = toplevel() {
        let _ = std::env::set_current_dir(&top);
    }

    let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    enable_raw_mode()?;
    let mut backend_file = tty;
    execute!(backend_file, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(backend_file))?;

    let res = event_loop(&mut term, input);

    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        Clear(ClearType::All)
    )?;

    res
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<std::fs::File>>,
    input: &str,
) -> io::Result<Option<String>> {
    let mut st = CompleterState::new(input);
    // cache the preview keyed by the pipeline it was computed for
    let mut preview = preview_text(&st.effective());
    let mut preview_key = st.effective();

    while !st.quit {
        let key = st.effective();
        if key != preview_key {
            preview = preview_text(&key);
            preview_key = key;
        }
        term.draw(|f| draw(f, &st, &preview))?;

        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            st.handle(k.code, k.modifiers);
        }
    }
    Ok(st.accepted)
}

fn draw(f: &mut ratatui::Frame, st: &CompleterState, preview: &str) {
    let dim = Style::default().fg(Color::DarkGray);
    let sel = Style::default().add_modifier(Modifier::REVERSED);

    let rows = Layout::vertical([
        Constraint::Length(1), // prompt
        Constraint::Min(1),    // body
        Constraint::Length(1), // status
    ])
    .split(f.area());

    // prompt: the pipeline being built, then the query under a caret
    let prompt = Line::from(vec![
        Span::styled("gitq❯ ", Style::default().fg(Color::Cyan)),
        Span::raw(st.pipeline.clone()),
        Span::styled(
            st.query.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("▮", Style::default().fg(Color::Cyan)),
    ]);
    f.render_widget(Paragraph::new(prompt), rows[0]);

    // body: a compact candidate column on the left, the rest is preview.  The
    // column is sized to its content (candidate + kind) but capped so the
    // preview stays wide.
    let col_w = st
        .all
        .iter()
        .map(|c| c.text.chars().count() + c.kind.len() + 4)
        .max()
        .unwrap_or(20)
        .clamp(16, 44) as u16;
    let body = Layout::horizontal([Constraint::Length(col_w), Constraint::Min(1)]).split(rows[1]);

    let items: Vec<ListItem> = st
        .filtered
        .iter()
        .map(|&i| {
            let c = &st.all[i];
            ListItem::new(Line::from(vec![
                Span::raw(c.text.clone()),
                Span::raw("  "),
                Span::styled(c.kind.to_string(), dim),
            ]))
        })
        .collect();
    let mut ls = ListState::default();
    if !st.filtered.is_empty() {
        ls.select(Some(st.selected));
    }
    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::RIGHT))
            .highlight_style(sel)
            .highlight_symbol("▌"),
        body[0],
        &mut ls,
    );

    f.render_widget(
        Paragraph::new(preview.to_string()).wrap(Wrap { trim: false }),
        body[1],
    );

    // status: count on the left, the highlighted candidate's description, and
    // the key legend.
    let n = st.filtered.len();
    let desc = st.highlighted().map(|c| c.desc).unwrap_or("");
    let status = Line::from(vec![
        Span::styled(
            format!("{}/{}", if n == 0 { 0 } else { st.selected + 1 }, n),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(desc.to_string(), dim),
    ]);
    let legend = "Tab commit  ↵ accept  ^n/^p move  ⌫ back  Esc cancel";
    let status_row =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(legend.len() as u16)])
            .split(rows[2]);
    f.render_widget(Paragraph::new(status), status_row[0]);
    f.render_widget(Paragraph::new(legend).style(dim), status_row[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_last_token_handles_trailing_space_and_quotes() {
        assert_eq!(split_last_token(""), ("".into(), "".into()));
        assert_eq!(split_last_token("commits "), ("commits ".into(), "".into()));
        assert_eq!(
            split_last_token("commits wh"),
            ("commits ".into(), "wh".into())
        );
        assert_eq!(split_last_token("co"), ("".into(), "co".into()));
        // a space inside an open quote is not a split point
        assert_eq!(
            split_last_token("where message \"fix bug"),
            ("where message ".into(), "\"fix bug".into())
        );
    }

    #[test]
    fn fuzzy_ranks_prefix_and_contiguous_above_scattered() {
        // "wh" prefers "where" (prefix) over a scattered subsequence
        let where_s = fuzzy_score("wh", "where").unwrap();
        let scattered = fuzzy_score("wh", "with-hunk").unwrap_or(i32::MIN);
        assert!(where_s > scattered);
        // a non-subsequence does not match
        assert_eq!(fuzzy_score("zz", "where"), None);
        // empty query matches everything, neutrally
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn typing_filters_and_resets_selection() {
        let mut st = CompleterState::new("commits ");
        let before = st.filtered.len();
        assert!(before > 1);
        st.handle(KeyCode::Char('w'), KeyModifiers::NONE);
        st.handle(KeyCode::Char('h'), KeyModifiers::NONE);
        // only steps fuzzy-matching "wh" survive; "where" is the top hit
        assert!(!st.filtered.is_empty());
        assert_eq!(st.highlighted().map(|c| c.text.as_str()), Some("where"));
        assert!(st.filtered.len() < before);
    }

    #[test]
    fn tab_commits_the_token_and_advances_the_position() {
        let mut st = CompleterState::new("commits ");
        for c in "where".chars() {
            st.handle(KeyCode::Char(c), KeyModifiers::NONE);
        }
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(st.pipeline, "commits where ");
        assert!(st.query.is_empty());
        // the next position offers fields, not steps
        assert!(st
            .all
            .iter()
            .any(|c| c.text == "author" && c.kind == "field"));
        assert!(!st.quit);
    }

    #[test]
    fn enter_accepts_the_highlighted_candidate() {
        let mut st = CompleterState::new("comm");
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(st.quit);
        assert_eq!(st.accepted.as_deref(), Some("commits"));
    }

    #[test]
    fn backspace_on_empty_query_steps_back_a_committed_token() {
        let mut st = CompleterState::new("commits where ");
        assert!(st.all.iter().any(|c| c.kind == "field"));
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.pipeline, "commits ");
        // back to offering steps
        assert!(st.all.iter().any(|c| c.text == "where" && c.kind == "step"));
    }

    #[test]
    fn ctrl_c_and_esc_cancel_without_accepting() {
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(st.quit);
        assert!(st.accepted.is_none());

        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert!(st.quit && st.accepted.is_none());
    }

    #[test]
    fn a_typed_value_with_no_candidate_is_accepted_verbatim() {
        // after `where message ` a free-text value has no candidate list;
        // Enter should take the typed query
        let mut st = CompleterState::new("commits where message ");
        for c in "hello".chars() {
            st.handle(KeyCode::Char(c), KeyModifiers::NONE);
        }
        if st.highlighted().is_none() {
            st.handle(KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(st.accepted.as_deref(), Some("commits where message hello"));
        }
    }
}
