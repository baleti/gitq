//! Interactive columnar completer for gitq pipelines.
//!
//! One fuzzy-filtered candidate column on the left and a wide live preview
//! on the right.  The column always shows the completion set for the current
//! position; Tab commits the highlighted candidate and moves the column on,
//! Backspace steps it back.  The pipeline built so far is spelled out in the
//! line above, which is why a second column earned nothing: it could only
//! restate what that line already says, at the cost of the width the preview
//! needs.
//!
//! `Ctrl-L` steps into the preview.  The result frames become a selectable
//! list, and choosing one *pivots* — the column re-roots on that object (a
//! commit by its SHA, a hunk re-derived through `diff.hunks`, …), exactly as
//! the Emacs `gitq-results-refine` walk does.  That is the embark-style move:
//! act on a concrete object in the results and get the completions valid *for
//! it*.  `v`/`m` there select several rows instead, which narrows the query
//! with a positional `[...]` step rather than re-rooting it.
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

use std::collections::BTreeSet;
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

/// The candidate column: the completion set for the current pipeline
/// position, plus where it has been.
struct Column {
    /// Committed pipeline up to (not including) this column; trailing space
    /// when non-empty.
    prefix: String,
    /// Prefixes this column has been at, oldest first.  Backspace on an empty
    /// query pops one.  A stack rather than string-surgery on `prefix`,
    /// because a move is not always an extension: drilling onto an object
    /// *replaces* the pipeline, and the old prefix is not a prefix of the new
    /// one to trim back to.
    history: Vec<String>,
    query: String,
    all: Vec<Cand>,
    filtered: Vec<usize>,
    selected: usize,
}

impl Column {
    fn new(prefix: String, query: String) -> Self {
        let mut c = Column {
            history: Vec::new(),
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

    /// Move this column to a new prefix, remembering where it was.
    fn move_to(&mut self, prefix: String) {
        self.history.push(std::mem::take(&mut self.prefix));
        self.set_prefix(prefix);
    }

    /// Step back to the previous prefix, if there is one.
    fn back(&mut self) -> bool {
        match self.history.pop() {
            Some(p) => {
                self.set_prefix(p);
                true
            }
            None => false,
        }
    }

    fn set_prefix(&mut self, prefix: String) {
        self.prefix = prefix;
        self.query.clear();
        self.selected = 0;
        self.all = candidates_for(&self.prefix);
        self.refilter();
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

/// A non-pipeline action offered by the `M-x` palette.
///
/// Deliberately *not* a home for pipeline steps: those are the columns' job,
/// where they get the live preview and the Tab-to-commit flow.  The palette
/// is for things that act on the session rather than on the pipeline being
/// built, which have nowhere else to live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteCommand {
    ScrollbackBrowse,
}

/// The palette's contents.  One row per command: what to type, and what it
/// does.  Grows by adding a line here and a match arm in the caller.
const PALETTE: &[(&str, &str, PaletteCommand)] = &[(
    "scrollback-browse",
    "browse this tmux pane's captured scrollback",
    PaletteCommand::ScrollbackBrowse,
)];

/// What the completer finished with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A pipeline to print to stdout.
    Accepted(String),
    /// A palette command for the caller to run once the TUI is torn down —
    /// the browser drives its own terminal, so it cannot start underneath
    /// this one.
    Command(PaletteCommand),
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
enum Focus {
    Columns,
    Preview,
    /// The `M-x` palette, overlaid on the columns.
    Palette,
}

struct CompleterState {
    /// The single candidate column.  Its own `history` carries the moves
    /// that have been made through it.
    column: Column,
    focus: Focus,
    /// Frames of the active column's effective pipeline, and the pipeline
    /// they were computed for.
    frames: Vec<Frame>,
    message: Option<String>,
    frames_key: String,
    /// The pipeline the displayed frames were actually produced by.  Not the
    /// same as `frames_key` once the preview falls back past an unrunnable
    /// candidate — and it is this, not the key, that a pivot must build on,
    /// or the rows would be re-selected out of a different result set.
    frames_from: String,
    /// Selection within the preview when `Focus::Preview`.
    preview_sel: usize,
    /// Filter text and selection for the `M-x` palette.
    palette_query: String,
    palette_sel: usize,
    /// Anchor of the visual-mode range; `Some` while `v` is active.  The
    /// live range is anchor..=preview_sel, in either direction.
    visual_from: Option<usize>,
    /// Rows put aside with `m`.  They survive leaving visual mode, so
    /// several disjoint runs can be gathered before acting on them.
    marked: BTreeSet<usize>,
    accepted: Option<String>,
    /// Set when a palette command is chosen; run after the TUI exits.
    command: Option<PaletteCommand>,
    quit: bool,
}

impl CompleterState {
    fn new(input: &str) -> Self {
        let (head, partial) = split_last_token(input);
        let mut st = CompleterState {
            column: Column::new(head, partial),
            focus: Focus::Columns,
            frames: Vec::new(),
            message: None,
            frames_key: String::new(),
            frames_from: String::new(),
            preview_sel: 0,
            palette_query: String::new(),
            palette_sel: 0,
            visual_from: None,
            marked: BTreeSet::new(),
            accepted: None,
            command: None,
            quit: false,
        };
        st.refresh_preview();
        st
    }

    fn active(&self) -> &Column {
        &self.column
    }

    fn active_mut(&mut self) -> &mut Column {
        &mut self.column
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
                self.frames_from = key.clone();
            }
            Err(msg) => {
                // The highlighted candidate does not form a runnable pipeline
                // -- `commits ` with `in` highlighted is mid-step, not wrong.
                // Blanking the preview there loses sight of the very data
                // being narrowed, so fall back to what the *committed* prefix
                // produces and keep showing it.
                //
                // The fallback is only taken when the prefix itself runs, so a
                // genuinely instructive error still surfaces: `commits where `
                // with `sha` highlighted fails, and so does `commits where`,
                // leaving the original message on screen.
                let base = self.active().prefix.trim().to_string();
                match preview_result(&base) {
                    Ok(frames) if !base.is_empty() => {
                        self.frames = frames;
                        self.message = None;
                        self.frames_from = base;
                    }
                    _ => {
                        self.frames = Vec::new();
                        self.message = Some(msg);
                    }
                }
            }
        }
        self.frames_key = key;
        self.preview_sel = 0;
        // row numbers refer to the old result set; keeping them would act on
        // whatever now happens to sit at those positions
        self.clear_selection();
    }

    /// Tab: commit the highlighted candidate and advance *this* column.
    ///
    /// It replaces the column in place rather than stacking a new one to the
    /// right.  The committed token is already shown in the pipeline line
    /// above, so a second column would display the same information twice and
    /// spend the width that makes the preview readable.  Extra columns are
    /// reserved for [`pivot`](Self::pivot), where the new column has a
    /// genuinely different root and the pair is worth seeing side by side.
    fn commit(&mut self) {
        let tok = self.active().current_token();
        if tok.is_empty() {
            return;
        }
        let next_prefix = format!("{} ", self.active().effective());
        self.active_mut().move_to(next_prefix);
    }

    /// Backspace on an empty query: undo the last move — a committed token,
    /// a preview selection, or a drill onto an object, all the same way.
    fn pop_column(&mut self) {
        self.active_mut().back();
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
    /// The live visual range, if visual mode is active.
    fn visual_range(&self) -> Option<(usize, usize)> {
        self.visual_from
            .map(|a| (a.min(self.preview_sel), a.max(self.preview_sel)))
    }

    /// Every row currently selected: what `m` has put aside, plus the live
    /// visual range, falling back to the row under the cursor so a plain
    /// pivot still works with no selection at all.
    fn selected_rows(&self) -> Vec<usize> {
        let mut rows = self.marked.clone();
        if let Some((lo, hi)) = self.visual_range() {
            rows.extend(lo..=hi);
        }
        if rows.is_empty() {
            rows.insert(self.preview_sel);
        }
        rows.into_iter()
            .filter(|&i| i < self.frames.len())
            .collect()
    }

    /// `v`: start a visual range here, or cancel the one in progress.
    fn toggle_visual(&mut self) {
        self.visual_from = match self.visual_from {
            Some(_) => None,
            None => Some(self.preview_sel),
        };
    }

    /// `m`: put the current range aside and leave visual mode, so another
    /// run can be gathered.  With no range, marks (or unmarks) the row under
    /// the cursor, which is how single rows accumulate.
    fn mark_selection(&mut self) {
        match self.visual_range() {
            Some((lo, hi)) => {
                self.marked.extend(lo..=hi);
                self.visual_from = None;
            }
            None => {
                if !self.marked.remove(&self.preview_sel) {
                    self.marked.insert(self.preview_sel);
                }
            }
        }
    }

    /// Contiguous runs of the selected rows, as a gitq selection step —
    /// `[1:3,5:7]`.  Emitting the query rather than carrying hidden state is
    /// the point: what you selected stays readable, editable and re-runnable.
    fn selection_step(&self) -> Option<String> {
        let rows = self.selected_rows();
        if rows.is_empty() {
            return None;
        }
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for r in rows {
            match runs.last_mut() {
                Some((_, end)) if *end + 1 == r => *end = r,
                _ => runs.push((r, r)),
            }
        }
        let parts: Vec<String> = runs
            .iter()
            .map(|(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}..{}", b + 1) // half-open, as the language reads it
                }
            })
            .collect();
        Some(format!("[{}]", parts.join(",")))
    }

    fn clear_selection(&mut self) {
        self.visual_from = None;
        self.marked.clear();
    }

    /// Pivot on the selection, or on the row under the cursor.
    ///
    /// The two cases differ on purpose.  A bare cursor re-roots on the object
    /// itself (a commit becomes its own source), which is the drill that
    /// makes single-object exploration work.  An *explicit* selection instead
    /// appends a positional step to the pipeline you are already in, because
    /// several rows have no single identity to re-root on — and because
    /// `commits ... [1:3,5:7]` is a query you can read and edit afterwards.
    fn pivot(&mut self) {
        // what produced the rows on screen, which is not always `effective()`
        let base = self.frames_from.clone();
        let explicit = self.visual_from.is_some() || !self.marked.is_empty();
        let next = if explicit && !base.trim().is_empty() {
            self.selection_step()
                .map(|sel| format!("{} {sel}", base.trim()))
        } else {
            self.frames
                .get(self.preview_sel)
                .and_then(|f| frame_anchor(f, &base))
        };
        if let Some(n) = next {
            // Both a selection and a single-object drill move the column
            // that produced them.  Neither warrants a new one: a selection
            // does not change the frame shape at all, and a drill replaces
            // the query rather than standing beside it — the column it would
            // sit next to is the one it was derived from, already visible in
            // the pipeline line above.  Backspace undoes either.
            self.active_mut().move_to(format!("{n} "));
        }
        self.clear_selection();
        self.focus = Focus::Columns;
    }

    /// Enter: accept the pipeline as it stands.
    ///
    /// With nothing typed there is no token being completed, so the
    /// highlighted candidate is only the first row of a menu — appending it
    /// would add a step the user never asked for.  Tab is how a candidate is
    /// chosen; Enter takes what is on the line.
    ///
    /// It also restores what-you-see-is-what-you-get: with an empty query the
    /// preview falls back past the unrunnable candidate and shows the
    /// committed pipeline, so accepting the candidate handed back something
    /// the preview had never displayed.
    ///
    /// With a query typed, the candidate *is* what is being completed, and
    /// Enter still takes it — `comm` + Enter is `commits`.
    fn accept(&mut self) {
        let col = self.active();
        self.accepted = Some(if col.query.is_empty() {
            col.prefix.trim().to_string()
        } else {
            col.effective()
        });
        self.quit = true;
    }

    /// Palette rows matching the current filter, best first.
    fn palette_rows(&self) -> Vec<&'static (&'static str, &'static str, PaletteCommand)> {
        let mut scored: Vec<(&'static (&str, &str, PaletteCommand), i32)> = PALETTE
            .iter()
            .filter_map(|row| fuzzy_score(&self.palette_query, row.0).map(|s| (row, s)))
            .collect();
        scored.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
        scored.into_iter().map(|(r, _)| r).collect()
    }

    fn open_palette(&mut self) {
        self.focus = Focus::Palette;
        self.palette_query.clear();
        self.palette_sel = 0;
    }

    fn handle(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        match self.focus {
            Focus::Columns => match code {
                // M-x, as in Emacs
                KeyCode::Char('x') if alt => self.open_palette(),
                KeyCode::Esc => self.quit = true,
                KeyCode::Char('c' | 'g') if ctrl => self.quit = true,
                KeyCode::Enter => self.accept(),
                KeyCode::Tab => self.commit(),
                KeyCode::Char('l') if ctrl => self.enter_preview(),
                KeyCode::BackTab | KeyCode::Up => self.active_mut().move_sel(-1),
                KeyCode::Down => self.active_mut().move_sel(1),
                KeyCode::Char('p' | 'k') if ctrl => self.active_mut().move_sel(-1),
                KeyCode::Char('n' | 'j') if ctrl => self.active_mut().move_sel(1),
                KeyCode::Backspace => {
                    if self.active().query.is_empty() {
                        self.pop_column();
                    } else {
                        self.active_mut().query.pop();
                        self.active_mut().refilter();
                    }
                }
                // `!alt` so an unbound M-<key> is ignored rather than typed
                KeyCode::Char(c) if !ctrl && !alt => {
                    let col = self.active_mut();
                    col.query.push(c);
                    col.selected = 0;
                    col.refilter();
                }
                _ => {}
            },
            Focus::Preview => match code {
                KeyCode::Char('v') => self.toggle_visual(),
                KeyCode::Char('m') => self.mark_selection(),
                // Esc peels one layer at a time: the range, then the marks,
                // then the preview itself — so it never discards more than
                // the user was looking at.
                KeyCode::Esc => {
                    if self.visual_from.is_some() {
                        self.visual_from = None;
                    } else if !self.marked.is_empty() {
                        self.marked.clear();
                    } else {
                        self.focus = Focus::Columns;
                    }
                }
                KeyCode::Enter | KeyCode::Tab => self.pivot(),
                KeyCode::Char('l') if ctrl => self.pivot(),
                // Nothing is typed here, so movement takes the vi and the
                // readline pair with or without Ctrl — one arm each, rather
                // than a Ctrl-guarded arm the bare one would shadow.
                KeyCode::Up | KeyCode::Char('k' | 'p') => {
                    self.preview_sel = self.preview_sel.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j' | 'n') => {
                    self.preview_sel =
                        (self.preview_sel + 1).min(self.frames.len().saturating_sub(1))
                }
                _ => {}
            },
            // The palette owns the keyboard while open, so a stray character
            // cannot leak into the column behind it.
            Focus::Palette => {
                let n = self.palette_rows().len();
                match code {
                    KeyCode::Esc => self.focus = Focus::Columns,
                    KeyCode::Char('c' | 'g') if ctrl => self.focus = Focus::Columns,
                    KeyCode::Enter => {
                        if let Some(row) = self.palette_rows().get(self.palette_sel) {
                            self.command = Some(row.2);
                            self.quit = true;
                        }
                    }
                    KeyCode::Up | KeyCode::BackTab => {
                        self.palette_sel = self.palette_sel.saturating_sub(1)
                    }
                    KeyCode::Char('p' | 'k') if ctrl => {
                        self.palette_sel = self.palette_sel.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        self.palette_sel = (self.palette_sel + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::Char('n' | 'j') if ctrl => {
                        self.palette_sel = (self.palette_sel + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::Backspace => {
                        self.palette_query.pop();
                        self.palette_sel = 0;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        self.palette_query.push(c);
                        self.palette_sel = 0;
                    }
                    _ => {}
                }
            }
        }
    }
}

// --- terminal driver -----------------------------------------------------

/// Run the completer over the pipeline typed so far.  Returns the chosen
/// pipeline on accept, a palette command to run after teardown, or
/// `Cancelled`.  Draws on `/dev/tty`, leaving
/// stdout for the result.
pub fn run_completer(input: &str) -> io::Result<Outcome> {
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
) -> io::Result<Outcome> {
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
    Ok(match (st.command, st.accepted) {
        (Some(c), _) => Outcome::Command(c),
        (None, Some(p)) => Outcome::Accepted(p),
        (None, None) => Outcome::Cancelled,
    })
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

    // body: the rightmost columns that fit, then the preview.
    //
    // Each column is sized to its own longest entry rather than to a shared
    // constant, because the constant clipped: morphism names like
    // `tree.entries[Blob]` are half again the width of a step keyword, and a
    // candidate you cannot read is worse than a narrower preview.
    let preview_min = 30u16;
    let avail = rows[1].width;
    // one cell for the highlight symbol, one between name and kind, one of
    // right padding so text never abuts the border
    let natural = |col: &Column| -> u16 {
        col.filtered
            .iter()
            .map(|&j| {
                let c = &col.all[j];
                c.text.chars().count() + c.kind.chars().count() + 3
            })
            .max()
            .unwrap_or(12)
            .min(avail.saturating_sub(preview_min).max(12) as usize) as u16
    };

    let col = st.active();
    let cells =
        Layout::horizontal([Constraint::Length(natural(col)), Constraint::Min(10)]).split(rows[1]);

    let hl = if st.focus == Focus::Columns {
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
        cells[0],
        &mut ls,
    );

    // preview cell: an interactive frame list when focused, else text
    let pv = cells[1];
    if st.focus == Focus::Preview {
        let vis = st.visual_range();
        let items: Vec<ListItem> = st
            .frames
            .iter()
            .enumerate()
            .map(|(i, fr)| {
                // a gutter, so a selected row reads as selected even when the
                // cursor has moved off it
                let in_visual = vis.is_some_and(|(lo, hi)| i >= lo && i <= hi);
                let (glyph, style) = if st.marked.contains(&i) {
                    ("*", Style::default().fg(Color::Yellow))
                } else if in_visual {
                    ("│", Style::default().fg(Color::Cyan))
                } else {
                    (" ", dim)
                };
                let line = Line::from(vec![
                    Span::styled(glyph, style),
                    Span::raw(" "),
                    Span::raw(render_frame_line(fr)),
                ]);
                ListItem::new(if in_visual {
                    line.style(Style::default().bg(Color::Rgb(40, 50, 60)))
                } else {
                    line
                })
            })
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

    // The palette overlays the body rather than replacing the layout, so the
    // pipeline you were building stays visible behind it.
    if st.focus == Focus::Palette {
        draw_palette(f, rows[1], st);
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
                "Tab next  ^j/^k move  ^L preview  M-x cmds  ↵ accept",
            )
        }
        Focus::Preview => {
            let n = st.selected_rows().len();
            let sel = match (st.visual_from.is_some(), st.marked.len()) {
                (false, 0) => String::new(),
                _ => format!(
                    "  {} selected {}",
                    n,
                    st.selection_step().unwrap_or_default()
                ),
            };
            (
                format!("preview {}/{}{sel}", st.preview_sel + 1, st.frames.len()),
                if st.visual_from.is_some() {
                    "^j/^k extend  m mark range  v cancel  ↵ act on selection"
                } else {
                    "^j/^k move  v visual  m mark  ↵ pivot  Esc back"
                },
            )
        }
        Focus::Palette => {
            let n = st.palette_rows().len();
            (
                format!("M-x {}/{}", if n == 0 { 0 } else { st.palette_sel + 1 }, n),
                "↵ run  ^j/^k move  Esc close palette",
            )
        }
    };
    let status = Layout::horizontal([Constraint::Min(1), Constraint::Length(legend.len() as u16)])
        .split(rows[2]);
    f.render_widget(
        Paragraph::new(info).style(Style::default().add_modifier(Modifier::BOLD)),
        status[0],
    );
    f.render_widget(Paragraph::new(legend).style(dim), status[1]);
}

/// The `M-x` palette: a centred, fuzzy-filtered list of session commands.
fn draw_palette(f: &mut ratatui::Frame, area: ratatui::layout::Rect, st: &CompleterState) {
    let dim = Style::default().fg(Color::DarkGray);
    let sel = Style::default().add_modifier(Modifier::REVERSED);
    let rows = st.palette_rows();

    // sized to content, but never wider or taller than the space we have
    let w = rows
        .iter()
        .map(|r| r.0.len() + r.1.len() + 6)
        .max()
        .unwrap_or(40)
        .clamp(30, area.width.saturating_sub(4).max(30) as usize) as u16;
    let h = (rows.len() as u16 + 3).min(area.height.max(3));
    let rect = ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    };

    // clear what is underneath so the columns do not show through
    f.render_widget(ratatui::widgets::Clear, rect);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            ListItem::new(Line::from(vec![
                Span::raw(r.0.to_string()),
                Span::raw("  "),
                Span::styled(r.1.to_string(), dim),
            ]))
        })
        .collect();

    let mut ls = ListState::default();
    if !rows.is_empty() {
        ls.select(Some(st.palette_sel.min(rows.len() - 1)));
    }
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" M-x {}▮ ", st.palette_query)),
            )
            .highlight_style(sel)
            .highlight_symbol("▌"),
        rect,
        &mut ls,
    );
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
    fn tab_advances_the_active_column_in_place() {
        let mut st = CompleterState::new("commits ");
        for c in "where".chars() {
            st.handle(KeyCode::Char(c), KeyModifiers::NONE);
        }
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        // the committed token is already visible in the pipeline line, so it
        // does not also get a column of its own
        assert_eq!(st.active().prefix, "commits where ");
        // and the new position offers fields, not steps
        assert!(st.active().all.iter().any(|c| c.kind == "field"));
    }

    #[test]
    fn backspace_on_empty_query_steps_back_a_token() {
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Tab, KeyModifiers::NONE); // commit the highlighted step
        let advanced = st.active().prefix.clone();
        assert_ne!(advanced, "commits ");
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "commits ");
    }

    #[test]
    fn backspace_unwinds_a_drill_one_move_at_a_time() {
        // a drill replaces the pipeline, so the old prefix is not a prefix of
        // the new one — stepping back has to be a history pop, not string
        // surgery on the current text
        let mut st = CompleterState::new("commits ");
        st.frames = vec![commit_frame("cafef00d")];
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "cafef00d ");

        st.handle(KeyCode::Tab, KeyModifiers::NONE); // a token on top of it
        assert!(st.active().prefix.starts_with("cafef00d "));
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "cafef00d ");
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "commits ", "the drill was not undone");
    }

    #[test]
    fn backspace_at_the_start_does_nothing_rather_than_underflowing() {
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "commits ");
    }

    #[test]
    fn ctrl_j_and_ctrl_k_move_the_selection() {
        let mut st = CompleterState::new("commits ");
        assert!(st.active().filtered.len() > 2);
        st.handle(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(st.active().selected, 1);
        st.handle(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(st.active().selected, 2);
        st.handle(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(st.active().selected, 1);
        // and they must not be typed into the query
        assert_eq!(st.active().query, "");
    }

    #[test]
    fn movement_in_the_drilled_preview_works_with_and_without_ctrl() {
        // regression: the bare j/k arms shadowed the Ctrl ones entirely, so
        // ^j/^k did nothing here
        let mut st = CompleterState::new("commits ");
        st.frames = vec![commit_frame("aaaa"), commit_frame("bbbb")];
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Preview);
        st.handle(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(st.preview_sel, 1, "^j did not move in the preview");
        st.handle(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(st.preview_sel, 0, "^k did not move in the preview");
        st.handle(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(st.preview_sel, 1, "bare j stopped working");
    }

    #[test]
    fn ctrl_j_and_ctrl_k_move_in_the_palette_too() {
        let mut st = CompleterState::new("commits ");
        let (c, m) = alt('x');
        st.handle(c, m);
        st.handle(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(st.palette_sel, st.palette_rows().len() - 1);
        st.handle(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(st.palette_sel, 0);
        assert_eq!(st.palette_query, "", "^j/^k leaked into the filter");
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
    fn enter_completes_the_token_being_typed() {
        let mut st = CompleterState::new("comm");
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.accepted.as_deref(), Some("commits"));
    }

    #[test]
    fn enter_with_nothing_typed_does_not_append_the_highlighted_candidate() {
        // `commits ` with `in` merely highlighted must accept `commits`
        let mut st = CompleterState::new("commits ");
        assert!(
            st.active().highlighted().is_some(),
            "no candidate highlighted, test proves nothing"
        );
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.accepted.as_deref(), Some("commits"));
    }

    #[test]
    fn tab_is_still_how_a_candidate_is_taken() {
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.accepted.as_deref(), Some("commits in"));
    }

    #[test]
    fn the_preview_falls_back_when_the_highlighted_candidate_is_mid_step() {
        // `commits ` highlights a step like `in`; `commits in` is not runnable,
        // but the pane should still show what `commits` produces rather than
        // blanking the very data being narrowed
        let st = CompleterState::new("commits ");
        assert!(st.active().highlighted().is_some());
        assert!(
            !st.frames.is_empty(),
            "preview blanked instead of falling back to the committed pipeline"
        );
        assert!(st.message.is_none());
    }

    #[test]
    fn an_instructive_error_still_shows_when_the_prefix_itself_cannot_run() {
        // the fallback must not swallow real guidance: `commits where sha`
        // fails, and so does `commits where`, so the message stays
        let st = CompleterState::new("commits where ");
        assert!(st.frames.is_empty());
        let msg = st.message.expect("the error was swallowed by the fallback");
        assert!(msg.contains("where"), "{msg}");
    }

    #[test]
    fn drilling_an_empty_preview_is_a_no_op() {
        let mut st = CompleterState::new("commits ");
        st.frames.clear();
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Columns);
    }

    // --- visual selection in the preview ----------------------------------

    fn previewing(n: usize) -> CompleterState {
        let mut st = CompleterState::new("commits ");
        st.frames = (0..n).map(|i| commit_frame(&format!("sha{i}"))).collect();
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Preview);
        st
    }

    fn down(st: &mut CompleterState, n: usize) {
        for _ in 0..n {
            st.handle(KeyCode::Char('j'), KeyModifiers::CONTROL);
        }
    }

    #[test]
    fn v_starts_a_range_that_movement_extends() {
        let mut st = previewing(6);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        assert_eq!(st.visual_range(), Some((0, 2)));
        assert_eq!(st.selected_rows(), vec![0, 1, 2]);
        assert_eq!(st.selection_step().as_deref(), Some("[0..3]"));
    }

    #[test]
    fn a_range_extends_upwards_too() {
        let mut st = previewing(6);
        down(&mut st, 4);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        st.handle(KeyCode::Char('k'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(st.visual_range(), Some((2, 4)));
        assert_eq!(st.selection_step().as_deref(), Some("[2..5]"));
    }

    #[test]
    fn m_banks_the_range_and_leaves_visual_so_another_can_be_made() {
        let mut st = previewing(10);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 1);
        st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(st.visual_from.is_none(), "still in visual mode after m");
        assert_eq!(st.marked.len(), 2);

        // a second, disjoint run
        down(&mut st, 4);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 1);
        st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(st.selection_step().as_deref(), Some("[0..2,5..7]"));
    }

    #[test]
    fn contiguous_marks_collapse_into_one_range_and_singles_stay_bare() {
        let mut st = previewing(10);
        for r in [0usize, 1, 2, 5, 8] {
            st.preview_sel = r;
            st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        }
        assert_eq!(st.selection_step().as_deref(), Some("[0..3,5,8]"));
    }

    #[test]
    fn m_on_an_already_marked_row_unmarks_it() {
        let mut st = previewing(4);
        st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(st.marked.len(), 1);
        st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(st.marked.is_empty());
    }

    #[test]
    fn v_again_cancels_the_range_without_marking_it() {
        let mut st = previewing(5);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        assert!(st.visual_from.is_none());
        assert!(st.marked.is_empty());
    }

    #[test]
    fn escape_peels_the_range_then_the_marks_then_the_preview() {
        let mut st = previewing(6);
        st.handle(KeyCode::Char('m'), KeyModifiers::NONE); // a mark
        down(&mut st, 2);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE); // and a range
        st.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert!(st.visual_from.is_none());
        assert_eq!(st.marked.len(), 1, "Esc took the marks too");
        assert_eq!(st.focus, Focus::Preview);
        st.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert!(st.marked.is_empty());
        assert_eq!(st.focus, Focus::Preview, "Esc left the preview too early");
        st.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(st.focus, Focus::Columns);
    }

    #[test]
    fn a_selection_narrows_the_column_it_came_from_without_opening_one() {
        let mut st = previewing(8);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.focus, Focus::Columns);
        assert_eq!(st.active().prefix, "commits [0..3] ");
        // the shape is unchanged, so the same candidates still apply
        assert!(st.active().all.iter().any(|c| c.kind == "step"));
        // and the selection does not linger
        assert!(st.marked.is_empty() && st.visual_from.is_none());
    }

    #[test]
    fn backspace_undoes_a_selection_that_narrowed_in_place() {
        let mut st = previewing(8);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "commits [0..3] ");
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "commits ");
    }

    #[test]
    fn pivoting_with_no_selection_still_re_roots_on_the_object() {
        // the single-object drill must not regress into a slice
        let mut st = previewing(3);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, "sha0 ");
    }

    #[test]
    fn the_emitted_step_is_a_pipeline_the_language_actually_parses() {
        let mut st = previewing(10);
        for r in [1usize, 2, 6] {
            st.preview_sel = r;
            st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        }
        let step = st.selection_step().unwrap();
        assert_eq!(step, "[1..3,6]");
        let p = crate::parse::parse_pipeline(&format!("commits {step}"))
            .expect("the TUI emitted something the parser rejects");
        // and it picks exactly the rows that were marked
        let sels = match &p.steps[0] {
            crate::ast::Step::Slice(s) => s.clone(),
            other => panic!("expected a slice step, got {other:?}"),
        };
        assert_eq!(crate::slice::positions(&sels, 10).unwrap(), vec![1, 2, 6]);
    }

    #[test]
    fn a_stale_selection_is_dropped_when_the_result_set_changes() {
        // row numbers refer to the old frames; acting on them afterwards
        // would hit whatever now sits at those positions
        let mut st = previewing(5);
        st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(!st.marked.is_empty());
        st.focus = Focus::Columns;
        st.frames_key = "stale".into();
        st.refresh_preview();
        assert!(st.marked.is_empty(), "stale rows survived a preview change");
    }

    // --- M-x palette ------------------------------------------------------

    fn alt(c: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn m_x_opens_the_palette_without_typing_into_the_column() {
        let mut st = CompleterState::new("commits ");
        let before = st.active().query.clone();
        let (c, m) = alt('x');
        st.handle(c, m);
        assert_eq!(st.focus, Focus::Palette);
        assert_eq!(st.active().query, before, "M-x leaked a character");
        assert!(!st.palette_rows().is_empty());
    }

    #[test]
    fn the_palette_offers_no_pipeline_steps() {
        // steps belong in the columns, where they get the preview
        for row in PALETTE {
            assert!(
                complete_candidates("").iter().all(|c| c != row.0),
                "palette duplicates the completion candidate {}",
                row.0
            );
        }
    }

    #[test]
    fn typing_in_the_palette_filters_it_and_does_not_reach_the_column() {
        let mut st = CompleterState::new("commits ");
        let (c, m) = alt('x');
        st.handle(c, m);
        for ch in "scroll".chars() {
            st.handle(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(st.palette_query, "scroll");
        assert_eq!(
            st.active().query,
            "",
            "palette input leaked into the column"
        );
        assert_eq!(st.palette_rows().len(), 1);
    }

    #[test]
    fn enter_in_the_palette_selects_a_command_rather_than_a_pipeline() {
        let mut st = CompleterState::new("commits ");
        let (c, m) = alt('x');
        st.handle(c, m);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(st.quit);
        assert_eq!(st.command, Some(PaletteCommand::ScrollbackBrowse));
        assert!(
            st.accepted.is_none(),
            "a command must not accept a pipeline"
        );
    }

    #[test]
    fn escape_closes_the_palette_and_keeps_the_completer_alive() {
        let mut st = CompleterState::new("commits ");
        let (c, m) = alt('x');
        st.handle(c, m);
        st.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(st.focus, Focus::Columns);
        assert!(!st.quit, "Esc closed the completer, not just the palette");
        assert!(st.command.is_none());
    }

    #[test]
    fn palette_movement_clamps_at_both_ends() {
        let mut st = CompleterState::new("");
        let (c, m) = alt('x');
        st.handle(c, m);
        let n = st.palette_rows().len();
        for _ in 0..5 {
            st.handle(KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(st.palette_sel, n - 1);
        for _ in 0..5 {
            st.handle(KeyCode::Up, KeyModifiers::NONE);
        }
        assert_eq!(st.palette_sel, 0);
    }

    #[test]
    fn a_palette_query_matching_nothing_cannot_run_a_command() {
        let mut st = CompleterState::new("");
        let (c, m) = alt('x');
        st.handle(c, m);
        for ch in "zzzz".chars() {
            st.handle(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert!(st.palette_rows().is_empty());
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(st.command.is_none());
        assert!(!st.quit);
    }

    #[test]
    fn an_unbound_meta_key_is_ignored_rather_than_typed() {
        let mut st = CompleterState::new("commits ");
        let (c, m) = alt('j');
        st.handle(c, m);
        assert_eq!(st.active().query, "");
        assert_eq!(st.focus, Focus::Columns);
    }

    #[test]
    fn the_palette_does_not_disturb_the_column_behind_it() {
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        let prefix = st.active().prefix.clone();
        let (c, m) = alt('x');
        st.handle(c, m);
        st.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(st.active().prefix, prefix);
    }
}
