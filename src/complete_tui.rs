//! Interactive columnar completer for gitq pipelines.
//!
//! A row of *miller columns* on the left and a wide live preview on the
//! right.  Each column is the fuzzy-filtered completion set for one position
//! in the pipeline; Tab commits the highlighted candidate and opens the next
//! column, so committed columns stay visible to the left as the pipeline
//! grows.  `Ctrl-L` steps into the preview: the result frames become a
//! selectable list, and choosing one *pivots* — a fresh column opens,
//! anchored on the selected object (a commit by its SHA, a hunk re-derived
//! through `diff.hunks`, …), exactly as the Emacs `gitq-results-refine`
//! walk does.  That is the embark-style move: act on a concrete object in
//! the results and get the completions valid *for it*.
//!
//! It is a front-end over gitq's own brain, never a second one: candidates
//! and kinds come from [`complete_candidates`]/[`annotate`], the preview and
//! the frames drilled into come from the same parse+exec+render `--preview`
//! uses (so it never applies a terminal — highlighting `/remove` shows what
//! it would touch and mutates nothing), and the anchor grammar is the same
//! the Emacs client builds.
//!
//! Rendering is on `/dev/tty` (stdout carries the chosen pipeline back to
//! the zsh widget).  Everything above the terminal — the input split, fuzzy
//! scoring, the anchor grammar, and the column/preview state machine — is
//! pure and unit-tested, because the live TUI can't be driven from a test.

use std::fs::OpenOptions;
use std::io;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;

use crate::complete::{annotate, complete_candidates};
use crate::exec::exec_pipeline;
use crate::frame::{Frame, FrameType, Value};
use crate::git::{toplevel, GitqError};
use crate::parse::parse_pipeline;
use crate::render::{render_frame_line, render_frames_text};

// --- candidates ----------------------------------------------------------

/// One completion candidate with its registry kind and description.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cand {
    text: String,
    kind: &'static str,
    desc: &'static str,
}

/// Candidates valid at the position PIPELINE leaves off, annotated exactly as
/// `gitq --complete-annotated` would.
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
/// Quote-aware so a value with spaces (`message "fix bug`) isn't split.
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

/// Subsequence fuzzy score, higher is better; `None` if not a subsequence.
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

// --- anchoring a drilled frame (the Rust twin of gitq--frame-anchor) ------

fn quote_value(v: &Value) -> String {
    match v {
        Value::Num(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => format!("{:?}", s.as_ref()),
    }
}

/// A `where` clause pinning FRAME by the given (field, label) equalities,
/// using only the fields the frame actually carries — so it always
/// type-checks against the frame's own shape.
fn identity_clause(frame: &Frame, specs: &[(&str, &str)]) -> Option<String> {
    let conds: Vec<String> = specs
        .iter()
        .filter_map(|(fname, label)| {
            frame
                .field(fname)
                .map(|v| format!("{} == {}", label, quote_value(&v)))
        })
        .collect();
    (!conds.is_empty()).then(|| format!("where {}", conds.join(", ")))
}

/// BASE narrowed to FRAME, or `None` if there is no BASE to narrow.
fn pinned(base: &str, frame: &Frame, specs: &[(&str, &str)]) -> Option<String> {
    let base = base.trim();
    if base.is_empty() {
        return None;
    }
    match identity_clause(frame, specs) {
        Some(c) => Some(format!("{base} {c}")),
        None => Some(base.to_string()),
    }
}

/// FRAME re-derived from its own commit through MORPHISM, pinned by SPECS —
/// falling back to pinning inside BASE when it has no owning commit.
fn derived_anchor(
    frame: &Frame,
    morphism: &str,
    specs: &[(&str, &str)],
    base: &str,
) -> Option<String> {
    match frame.commit_sha() {
        Some(sha) => {
            let clause = identity_clause(frame, specs)
                .map(|c| format!(" {c}"))
                .unwrap_or_default();
            Some(format!("{sha} via {morphism}{clause}"))
        }
        None => pinned(base, frame, specs),
    }
}

/// A pipeline whose result is just FRAME, to continue the query from.  The
/// anchor preserves the frame's shape, not only its identity — a commit is
/// its own SHA (a source in its own right), a hunk is re-derived through
/// `diff.hunks` so `via commit`/`via history` stay available, and anything
/// with no standalone source form is pinned inside BASE.
fn frame_anchor(frame: &Frame, base: &str) -> Option<String> {
    let str_field = |name: &str| {
        frame
            .field(name)
            .and_then(|v| v.as_str().map(str::to_string))
    };
    match frame.ty {
        FrameType::Commit => str_field("sha"),
        FrameType::Ref => str_field("name").or_else(|| str_field("sha")),
        FrameType::Hunk => derived_anchor(
            frame,
            "diff.hunks",
            &[("path", "path"), ("start-line", "start-line")],
            base,
        ),
        FrameType::DiffLine => derived_anchor(
            frame,
            "diff.lines",
            &[
                ("path", "path"),
                ("line-number", "line-number"),
                ("sign", "sign"),
            ],
            base,
        ),
        FrameType::Blob => pinned(base, frame, &[("sha", "sha"), ("path", "path")]),
        FrameType::Line => pinned(
            base,
            frame,
            &[
                ("commit-sha", "commit-sha"),
                ("path", "path"),
                ("line-number", "line-number"),
            ],
        ),
        FrameType::Worktree => pinned(base, frame, &[("path", "path")]),
        _ => pinned(base, frame, &[]),
    }
}

// --- preview -------------------------------------------------------------

/// Frames for a pipeline via gitq's own `--preview` (parse, run source and
/// steps, never apply a terminal).  `Err` carries the parse/exec message or
/// the empty-result note, to show in place of frames.
fn preview_result(pipeline: &str) -> Result<Vec<Frame>, String> {
    let p = pipeline.trim();
    if p.is_empty() {
        return Ok(Vec::new());
    }
    match parse_pipeline(p) {
        Err(e) => Err(e.to_string()),
        Ok(parsed) => match exec_pipeline(&parsed) {
            Err(GitqError(m)) => Err(m),
            Ok((frames, _term)) => {
                if frames.is_empty() {
                    Err("(no results)".to_string())
                } else {
                    Ok(frames)
                }
            }
        },
    }
}

// --- state ---------------------------------------------------------------

/// One miller column: the completion set for a single pipeline position.
struct Column {
    /// Committed pipeline up to (not including) this column; trailing space
    /// when non-empty.
    prefix: String,
    query: String,
    all: Vec<Cand>,
    filtered: Vec<usize>,
    selected: usize,
}

impl Column {
    fn new(prefix: String, query: String) -> Self {
        let mut c = Column {
            prefix,
            query,
            all: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
        };
        c.all = candidates_for(&c.prefix);
        c.refilter();
        c
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(usize, i32)> = self
            .all
            .iter()
            .enumerate()
            .filter_map(|(i, c)| fuzzy_score(&self.query, &c.text).map(|s| (i, s)))
            .collect();
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

    /// The token this column contributes: the highlighted candidate, or the
    /// raw query when nothing matches (a typed-in value).
    fn current_token(&self) -> String {
        self.highlighted()
            .map(|c| c.text.clone())
            .unwrap_or_else(|| self.query.clone())
    }

    /// The pipeline through this column, trimmed.
    fn effective(&self) -> String {
        format!("{}{}", self.prefix, self.current_token())
            .trim()
            .to_string()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Focus {
    Columns,
    Preview,
}

struct CompleterState {
    /// At least one; the last is the active (deepest) column.
    columns: Vec<Column>,
    focus: Focus,
    /// Frames of the active column's effective pipeline, and the pipeline
    /// they were computed for.
    frames: Vec<Frame>,
    message: Option<String>,
    frames_key: String,
    /// Selection within the preview when `Focus::Preview`.
    preview_sel: usize,
    accepted: Option<String>,
    quit: bool,
}

impl CompleterState {
    fn new(input: &str) -> Self {
        let (head, partial) = split_last_token(input);
        let mut st = CompleterState {
            columns: vec![Column::new(head, partial)],
            focus: Focus::Columns,
            frames: Vec::new(),
            message: None,
            frames_key: String::new(),
            preview_sel: 0,
            accepted: None,
            quit: false,
        };
        st.refresh_preview();
        st
    }

    fn active(&self) -> &Column {
        self.columns.last().expect("at least one column")
    }

    fn active_mut(&mut self) -> &mut Column {
        self.columns.last_mut().expect("at least one column")
    }

    fn effective(&self) -> String {
        self.active().effective()
    }

    /// Recompute the preview frames when the active pipeline has changed.
    /// In `Focus::Preview` the pipeline is fixed, so this is a no-op there.
    fn refresh_preview(&mut self) {
        let key = self.effective();
        if key == self.frames_key {
            return;
        }
        match preview_result(&key) {
            Ok(frames) => {
                self.frames = frames;
                self.message = None;
            }
            Err(msg) => {
                self.frames = Vec::new();
                self.message = Some(msg);
            }
        }
        self.frames_key = key;
        self.preview_sel = 0;
    }

    /// Tab: commit the active token, opening the next column.
    fn commit(&mut self) {
        let tok = self.active().current_token();
        if tok.is_empty() {
            return;
        }
        let next_prefix = format!("{} ", self.active().effective());
        self.columns.push(Column::new(next_prefix, String::new()));
    }

    /// Backspace on an empty query: drop the active column, back one level.
    fn pop_column(&mut self) {
        if self.columns.len() > 1 {
            self.columns.pop();
        }
    }

    /// Ctrl-L: step into the preview to drill its frames (only if there are
    /// any).
    fn enter_preview(&mut self) {
        if !self.frames.is_empty() {
            self.focus = Focus::Preview;
            self.preview_sel = self.preview_sel.min(self.frames.len() - 1);
        }
    }

    /// Pivot on the selected frame: open a fresh column anchored on it.
    fn pivot(&mut self) {
        let base = self.effective();
        if let Some(frame) = self.frames.get(self.preview_sel) {
            if let Some(anchor) = frame_anchor(frame, &base) {
                self.columns
                    .push(Column::new(format!("{anchor} "), String::new()));
            }
        }
        self.focus = Focus::Columns;
    }

    fn accept(&mut self) {
        self.accepted = Some(self.effective());
        self.quit = true;
    }

    fn handle(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match self.focus {
            Focus::Columns => match code {
                KeyCode::Esc => self.quit = true,
                KeyCode::Char('c' | 'g') if ctrl => self.quit = true,
                KeyCode::Enter => self.accept(),
                KeyCode::Tab => self.commit(),
                KeyCode::Char('l') if ctrl => self.enter_preview(),
                KeyCode::BackTab | KeyCode::Up => self.active_mut().move_sel(-1),
                KeyCode::Down => self.active_mut().move_sel(1),
                KeyCode::Char('p') if ctrl => self.active_mut().move_sel(-1),
                KeyCode::Char('n') if ctrl => self.active_mut().move_sel(1),
                KeyCode::Backspace => {
                    if self.active().query.is_empty() {
                        self.pop_column();
                    } else {
                        self.active_mut().query.pop();
                        self.active_mut().refilter();
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    let col = self.active_mut();
                    col.query.push(c);
                    col.selected = 0;
                    col.refilter();
                }
                _ => {}
            },
            Focus::Preview => match code {
                KeyCode::Esc => self.focus = Focus::Columns,
                KeyCode::Enter | KeyCode::Tab => self.pivot(),
                KeyCode::Char('l') if ctrl => self.pivot(),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.preview_sel = self.preview_sel.saturating_sub(1)
                }
                KeyCode::Char('p') if ctrl => self.preview_sel = self.preview_sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.preview_sel =
                        (self.preview_sel + 1).min(self.frames.len().saturating_sub(1))
                }
                KeyCode::Char('n') if ctrl => {
                    self.preview_sel =
                        (self.preview_sel + 1).min(self.frames.len().saturating_sub(1))
                }
                _ => {}
            },
        }
    }
}

// --- terminal driver -----------------------------------------------------

/// Run the completer over the pipeline typed so far.  Returns the chosen
/// pipeline on accept, or `None` on cancel.  Draws on `/dev/tty`, leaving
/// stdout for the result.
pub fn run_completer(input: &str) -> io::Result<Option<String>> {
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
    while !st.quit {
        if st.focus == Focus::Columns {
            st.refresh_preview();
        }
        term.draw(|f| draw(f, &st))?;
        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            st.handle(k.code, k.modifiers);
        }
    }
    Ok(st.accepted)
}

// --- drawing -------------------------------------------------------------

fn draw(f: &mut ratatui::Frame, st: &CompleterState) {
    let dim = Style::default().fg(Color::DarkGray);
    let sel = Style::default().add_modifier(Modifier::REVERSED);
    let sel_dim = Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let cyan = Style::default().fg(Color::Cyan);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(f.area());

    // prompt: the pipeline built so far, then the active query under a caret
    let prompt = Line::from(vec![
        Span::styled("gitq❯ ", cyan),
        Span::raw(st.active().prefix.clone()),
        Span::styled(
            st.active().query.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("▮", cyan),
    ]);
    f.render_widget(Paragraph::new(prompt), rows[0]);

    // body: the rightmost columns that fit, then the preview
    let col_w = 24u16;
    let preview_min = 30u16;
    let max_cols = ((rows[1].width.saturating_sub(preview_min)) / col_w).max(1) as usize;
    let start = st.columns.len().saturating_sub(max_cols);
    let shown = &st.columns[start..];

    let mut constraints: Vec<Constraint> =
        shown.iter().map(|_| Constraint::Length(col_w)).collect();
    constraints.push(Constraint::Min(10));
    let cells = Layout::horizontal(constraints).split(rows[1]);

    for (i, col) in shown.iter().enumerate() {
        let is_active = start + i == st.columns.len() - 1;
        let hl = if is_active && st.focus == Focus::Columns {
            sel
        } else {
            sel_dim
        };
        let items: Vec<ListItem> = col
            .filtered
            .iter()
            .map(|&j| {
                let c = &col.all[j];
                ListItem::new(Line::from(vec![
                    Span::raw(c.text.clone()),
                    Span::raw(" "),
                    Span::styled(c.kind.to_string(), dim),
                ]))
            })
            .collect();
        let mut ls = ListState::default();
        if !col.filtered.is_empty() {
            ls.select(Some(col.selected));
        }
        f.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::RIGHT))
                .highlight_style(hl)
                .highlight_symbol("▌"),
            cells[i],
            &mut ls,
        );
    }

    // preview cell: an interactive frame list when focused, else text
    let pv = cells[shown.len()];
    if st.focus == Focus::Preview {
        let items: Vec<ListItem> = st
            .frames
            .iter()
            .map(|fr| ListItem::new(Line::from(render_frame_line(fr))))
            .collect();
        let mut ls = ListState::default();
        ls.select(Some(st.preview_sel));
        f.render_stateful_widget(
            List::new(items).highlight_style(sel).highlight_symbol("▌"),
            pv,
            &mut ls,
        );
    } else if !st.frames.is_empty() {
        f.render_widget(
            Paragraph::new(render_frames_text(&st.frames)).wrap(Wrap { trim: false }),
            pv,
        );
    } else {
        f.render_widget(
            Paragraph::new(st.message.clone().unwrap_or_default())
                .style(dim)
                .wrap(Wrap { trim: false }),
            pv,
        );
    }

    // status
    let (info, legend) = match st.focus {
        Focus::Columns => {
            let n = st.active().filtered.len();
            let desc = st.active().highlighted().map(|c| c.desc).unwrap_or("");
            (
                format!(
                    "{}/{}  {}",
                    if n == 0 { 0 } else { st.active().selected + 1 },
                    n,
                    desc
                ),
                "Tab dive  ^L into preview  ↵ accept  ⌫ back  Esc quit",
            )
        }
        Focus::Preview => (
            format!("preview {}/{}", st.preview_sel + 1, st.frames.len()),
            "j/k move  ^L/↵ pivot on selection  Esc back",
        ),
    };
    let status = Layout::horizontal([Constraint::Min(1), Constraint::Length(legend.len() as u16)])
        .split(rows[2]);
    f.render_widget(
        Paragraph::new(info).style(Style::default().add_modifier(Modifier::BOLD)),
        status[0],
    );
    f.render_widget(Paragraph::new(legend).style(dim), status[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_frame(sha: &str) -> Frame {
        Frame::new(FrameType::Commit, [("sha", Value::from(sha))])
    }

    #[test]
    fn split_last_token_handles_trailing_space_and_quotes() {
        assert_eq!(
            split_last_token("commits wh"),
            ("commits ".into(), "wh".into())
        );
        assert_eq!(split_last_token("commits "), ("commits ".into(), "".into()));
        assert_eq!(
            split_last_token("where message \"fix bug"),
            ("where message ".into(), "\"fix bug".into())
        );
    }

    #[test]
    fn fuzzy_prefers_prefix() {
        assert!(fuzzy_score("wh", "where").unwrap() > fuzzy_score("wh", "with-h").unwrap());
        assert_eq!(fuzzy_score("zz", "where"), None);
    }

    #[test]
    fn anchor_of_a_commit_is_its_bare_sha() {
        let f = commit_frame("abc123");
        assert_eq!(
            frame_anchor(&f, "commits where author a"),
            Some("abc123".into())
        );
    }

    #[test]
    fn anchor_of_a_hunk_re_derives_through_diff_hunks() {
        let f = Frame::new(
            FrameType::Hunk,
            [
                ("commit-sha", Value::from("abc123")),
                ("path", Value::from("src/main.rs")),
                ("start-line", Value::Num(42)),
            ],
        );
        assert_eq!(
            frame_anchor(&f, "commits"),
            Some("abc123 via diff.hunks where path == \"src/main.rs\", start-line == 42".into())
        );
    }

    #[test]
    fn anchor_of_a_blob_pins_inside_the_base() {
        let f = Frame::new(
            FrameType::Blob,
            [("sha", Value::from("deadbeef")), ("path", Value::from("x"))],
        );
        assert_eq!(
            frame_anchor(&f, "HEAD via tree.blobs"),
            Some("HEAD via tree.blobs where sha == \"deadbeef\", path == \"x\"".into())
        );
        // no base to pin into => nothing
        assert_eq!(frame_anchor(&f, ""), None);
    }

    #[test]
    fn tab_commits_and_opens_a_new_column_keeping_the_old() {
        let mut st = CompleterState::new("commits ");
        for c in "where".chars() {
            st.handle(KeyCode::Char(c), KeyModifiers::NONE);
        }
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(st.columns.len(), 2);
        assert_eq!(st.active().prefix, "commits where ");
        // the previous column is still there
        assert_eq!(st.columns[0].prefix, "commits ");
    }

    #[test]
    fn backspace_on_empty_query_pops_back_a_column() {
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Tab, KeyModifiers::NONE); // commit the highlighted step
        assert_eq!(st.columns.len(), 2);
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.columns.len(), 1);
    }

    #[test]
    fn ctrl_l_drills_the_preview_and_pivots_onto_the_selected_frame() {
        let mut st = CompleterState::new("commits ");
        // stand in a known preview rather than depend on the repo's frames
        st.frames = vec![commit_frame("cafef00d")];
        st.message = None;
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Preview);
        st.handle(KeyCode::Enter, KeyModifiers::NONE); // pivot on the frame
        assert_eq!(st.focus, Focus::Columns);
        assert_eq!(st.active().prefix, "cafef00d ");
    }

    #[test]
    fn enter_accepts_the_active_effective_pipeline() {
        let mut st = CompleterState::new("comm");
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.accepted.as_deref(), Some("commits"));
    }

    #[test]
    fn drilling_an_empty_preview_is_a_no_op() {
        let mut st = CompleterState::new("commits ");
        st.frames.clear();
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Columns);
    }
}
