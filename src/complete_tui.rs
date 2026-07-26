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

/// The pipeline being edited, plus the candidates for wherever the cursor is.
///
/// The line is an ordinary editable buffer with a cursor, not a committed
/// prefix plus a trailing token.  The earlier split made Backspace delete a
/// whole committed token, which is no way for a line to behave: editing keys
/// should mean what they mean in a shell, and completion should read the
/// token the cursor is actually in.
struct Column {
    line: String,
    /// Byte offset into `line`; always kept on a char boundary.
    cursor: usize,
    /// Candidates for the position the cursor is in, and the prefix they were
    /// computed for — recomputing on every keystroke would re-run the parser
    /// for a cursor move that changed nothing.
    all: Vec<Cand>,
    cand_key: String,
    filtered: Vec<usize>,
    selected: usize,
}

impl Column {
    fn new(line: String, cursor: usize) -> Self {
        let mut c = Column {
            line,
            cursor,
            all: Vec::new(),
            cand_key: String::new(),
            filtered: Vec::new(),
            selected: 0,
        };
        // not `sync()`: an empty `cand_key` equals the prefix of an empty
        // line, so it would decide nothing had changed and never load
        c.cand_key = c.prefix();
        c.all = candidates_for(&c.cand_key);
        c.refilter();
        c
    }

    /// Everything before the token the cursor is in — what candidates are
    /// computed from.
    fn prefix(&self) -> String {
        split_last_token(&self.line[..self.cursor]).0
    }

    /// The part of the current token before the cursor — the fuzzy query.
    fn query(&self) -> String {
        split_last_token(&self.line[..self.cursor]).1
    }

    /// Reload candidates if the position changed, then refilter.
    fn sync(&mut self) {
        let prefix = self.prefix();
        if prefix != self.cand_key {
            self.all = candidates_for(&prefix);
            self.cand_key = prefix;
            self.selected = 0;
        }
        self.refilter();
    }

    fn refilter(&mut self) {
        let q = self.query();
        let mut scored: Vec<(usize, i32)> = self
            .all
            .iter()
            .enumerate()
            .filter_map(|(i, c)| fuzzy_score(&q, &c.text).map(|s| (i, s)))
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

    /// Replace the whole line, putting the cursor at the end.
    fn set_line(&mut self, line: String) {
        self.line = line;
        self.cursor = self.line.len();
        self.sync();
    }

    /// The pipeline the *preview* should show: the line with the highlighted
    /// candidate standing in at the cursor.
    ///
    /// It takes the candidate even when nothing has been typed, which is what
    /// makes the preview useful the moment the completer opens — `commits` is
    /// highlighted, so its commits are what you see, and moving down shows
    /// what each source would give.  A preview that waited for you to type
    /// first would leave the opening screen blank, teaching nothing.
    ///
    /// This is deliberately *not* what Enter accepts.  Enter takes the line as
    /// typed (see `accept`), because a highlight is where a menu opened, not
    /// a choice the user made; the preview showing more than Enter commits is
    /// the point of a preview.
    fn effective(&self) -> String {
        match self.highlighted() {
            // deliberately drops everything after the cursor: the preview
            // answers "what does the pipeline mean *here*", so putting the
            // cursor back inside `commits` shows commits, not the result of
            // steps the cursor has not reached yet
            Some(c) => format!("{}{}", self.prefix(), c.text).trim().to_string(),
            None => self.line[..self.cursor].trim().to_string(),
        }
    }

    /// The whole line with the token under the cursor completed — what Enter
    /// accepts.  Unlike [`effective`](Self::effective) this keeps the tail,
    /// since accepting must not silently drop the rest of the pipeline.
    fn completed_line(&self) -> String {
        let Some(c) = self.highlighted() else {
            return self.line.trim().to_string();
        };
        let tail = &self.line[self.token_end()..];
        let sep = if tail.is_empty() || tail.starts_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        format!("{}{}{sep}{tail}", self.prefix(), c.text)
            .trim()
            .to_string()
    }

    // --- editing ---------------------------------------------------------

    fn insert(&mut self, c: char) {
        self.line.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.sync();
    }

    /// Previous char boundary, or the cursor when already at the start.
    fn prev_boundary(&self) -> usize {
        self.line[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(self.cursor)
    }

    fn next_boundary(&self) -> usize {
        self.line[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor)
    }

    /// Start of the word before the cursor, skipping trailing spaces first —
    /// readline's `backward-word`.
    fn word_start(&self) -> usize {
        let s = &self.line[..self.cursor];
        let trimmed = s.trim_end();
        match trimmed.rfind(char::is_whitespace) {
            Some(i) => i + 1,
            None => 0,
        }
    }

    /// End of the token the cursor is inside — where a completion's
    /// replacement stops.  Not the cursor: completing `comm|its` must replace
    /// the whole word, or the unconsumed tail is left behind
    /// (`commits` + `its ...`).
    fn token_end(&self) -> usize {
        match self.line[self.cursor..].find(char::is_whitespace) {
            Some(i) => self.cursor + i,
            None => self.line.len(),
        }
    }

    fn word_end(&self) -> usize {
        let rest = &self.line[self.cursor..];
        let skipped = rest.len() - rest.trim_start().len();
        match rest.trim_start().find(char::is_whitespace) {
            Some(i) => self.cursor + skipped + i,
            None => self.line.len(),
        }
    }

    fn delete_back(&mut self) {
        let b = self.prev_boundary();
        if b != self.cursor {
            self.line.replace_range(b..self.cursor, "");
            self.cursor = b;
            self.sync();
        }
    }

    fn delete_forward(&mut self) {
        let b = self.next_boundary();
        if b != self.cursor {
            self.line.replace_range(self.cursor..b, "");
            self.sync();
        }
    }

    fn kill_word_back(&mut self) {
        let b = self.word_start();
        if b != self.cursor {
            self.line.replace_range(b..self.cursor, "");
            self.cursor = b;
            self.sync();
        }
    }

    fn kill_line(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.sync();
    }

    fn move_cursor(&mut self, to: usize) {
        self.cursor = to;
        self.sync();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// A `^w` was pressed and the next key is a window command.
    pending_window: bool,
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
        let mut st = CompleterState {
            column: Column::new(input.to_string(), input.len()),
            focus: Focus::Columns,
            frames: Vec::new(),
            message: None,
            frames_key: String::new(),
            frames_from: String::new(),
            preview_sel: 0,
            pending_window: false,
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
                let base = self.active().line.trim().to_string();
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
    /// Tab: replace the token under the cursor with the highlighted
    /// candidate and leave a space, so Tab-after-Tab builds the pipeline.
    fn commit(&mut self) {
        let col = self.active();
        let Some(cand) = col.highlighted().map(|c| c.text.clone()) else {
            return;
        };
        let head = col.prefix();
        // from the end of the token, not the cursor: Tab replaces the whole
        // word being completed
        let tail = col.line[col.token_end()..].to_string();
        // a space after the candidate, unless the tail already supplies one
        let sep = if tail.starts_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        let cursor = head.len() + cand.len() + sep.len();
        let line = format!("{head}{cand}{sep}{tail}");
        let col = self.active_mut();
        col.line = line;
        col.cursor = cursor.min(col.line.len());
        col.sync();
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
                // toggle, matching what space does to a single row: a range
                // already marked in full comes off, anything else goes on.
                // Adding unconditionally meant a range could only be undone
                // row by row, which is not what a toggle key should do.
                if (lo..=hi).all(|i| self.marked.contains(&i)) {
                    for i in lo..=hi {
                        self.marked.remove(&i);
                    }
                } else {
                    self.marked.extend(lo..=hi);
                }
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
            self.active_mut().set_line(format!("{n} "));
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
        self.accepted = Some(if col.query().is_empty() {
            col.line.trim().to_string()
        } else {
            col.completed_line()
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

    /// The key after a `^w`: vim's window commands over the two panes this UI
    /// has.  `w` goes to the *other* one, which with two panes is both
    /// "next" and "last".
    fn handle_window_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('h') => self.focus = Focus::Columns,
            KeyCode::Char('l') => self.enter_preview(),
            KeyCode::Char('w') => match self.focus {
                Focus::Columns => self.enter_preview(),
                _ => self.focus = Focus::Columns,
            },
            // an unmapped window key does nothing, as in vim, rather than
            // falling through to be typed into the query
            _ => {}
        }
    }

    fn handle(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);

        // `^w h` / `^w l` / `^w w`, handled before the per-focus tables so the
        // prefix works identically from either pane.  Not while the palette is
        // open: it owns the keyboard, and `w` is a character there.
        if !matches!(self.focus, Focus::Palette) {
            if std::mem::take(&mut self.pending_window) {
                self.handle_window_key(code);
                return;
            }
            if ctrl && code == KeyCode::Char('w') {
                self.pending_window = true;
                return;
            }
        }

        match self.focus {
            Focus::Columns => match code {
                // M-x, as in Emacs
                KeyCode::Char('x') if alt => self.open_palette(),
                // ^L to the preview, ^H back from it — the vim direction
                // keys without the ^w prefix, which still works too.  Both
                // are consumed here, so neither reaches the shell that
                // spawned the completer.
                KeyCode::Char('l') if ctrl => self.enter_preview(),
                KeyCode::Esc => self.quit = true,
                KeyCode::Char('c' | 'g') if ctrl => self.quit = true,
                KeyCode::Enter => self.accept(),
                KeyCode::Tab => self.commit(),
                KeyCode::Up => self.active_mut().move_sel(-1),
                KeyCode::Down => self.active_mut().move_sel(1),
                KeyCode::Char('p' | 'k') if ctrl => self.active_mut().move_sel(-1),
                KeyCode::Char('n' | 'j') if ctrl => self.active_mut().move_sel(1),
                // --- line editing, readline/emacs spellings ---------------
                //
                // ^W and ^K are deliberately absent: ^w is the window prefix
                // and ^k moves the selection.  M-<backspace> takes ^W's job
                // (kill word); ^U takes ^K's, clearing the whole line rather
                // than to the end — there is no third free chord for the
                // narrower version, and a pipeline is one line.
                KeyCode::Backspace if alt => self.active_mut().kill_word_back(),
                KeyCode::Backspace => self.active_mut().delete_back(),
                KeyCode::Delete => self.active_mut().delete_forward(),
                KeyCode::Char('d') if ctrl => self.active_mut().delete_forward(),
                KeyCode::Char('u') if ctrl => self.active_mut().kill_line(),
                KeyCode::Char('a') if ctrl => self.active_mut().move_cursor(0),
                KeyCode::Home => self.active_mut().move_cursor(0),
                KeyCode::Char('e') if ctrl => {
                    let end = self.active().line.len();
                    self.active_mut().move_cursor(end);
                }
                KeyCode::End => {
                    let end = self.active().line.len();
                    self.active_mut().move_cursor(end);
                }
                KeyCode::Left if ctrl => {
                    let to = self.active().word_start();
                    self.active_mut().move_cursor(to);
                }
                KeyCode::Right if ctrl => {
                    let to = self.active().word_end();
                    self.active_mut().move_cursor(to);
                }
                KeyCode::Char('b') if alt => {
                    let to = self.active().word_start();
                    self.active_mut().move_cursor(to);
                }
                KeyCode::Char('f') if alt => {
                    let to = self.active().word_end();
                    self.active_mut().move_cursor(to);
                }
                KeyCode::Left => {
                    let to = self.active().prev_boundary();
                    self.active_mut().move_cursor(to);
                }
                KeyCode::Char('b') if ctrl => {
                    let to = self.active().prev_boundary();
                    self.active_mut().move_cursor(to);
                }
                KeyCode::Right => {
                    let to = self.active().next_boundary();
                    self.active_mut().move_cursor(to);
                }
                KeyCode::Char('f') if ctrl => {
                    let to = self.active().next_boundary();
                    self.active_mut().move_cursor(to);
                }
                // `!alt` so an unbound M-<key> is ignored rather than typed
                KeyCode::Char(c) if !ctrl && !alt => self.active_mut().insert(c),
                _ => {}
            },
            Focus::Preview => match code {
                KeyCode::Char('h') if ctrl => self.focus = Focus::Columns,
                KeyCode::Char('v') => self.toggle_visual(),
                // space, not `m`: nothing else in this pane wants it, and
                // "tick this one" is what a spacebar means in every list UI
                KeyCode::Char(' ') => self.mark_selection(),
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
    let col0 = st.active();
    let rev = Style::default().add_modifier(Modifier::REVERSED);
    let (cursor_glyph, after_cursor) = match col0.line[col0.cursor..].chars().next() {
        Some(c) => (c.to_string(), &col0.line[col0.cursor + c.len_utf8()..]),
        // at end of line: a reversed space stands in, taking one cell that
        // was already blank
        None => (" ".to_string(), ""),
    };
    let prompt = Line::from(vec![
        Span::styled("gitq❯ ", cyan),
        Span::raw(col0.line[..col0.cursor].to_string()),
        // reverse video *on* the character, the way a shell draws a cursor.
        // Inserting a block glyph instead shifted every following character
        // one cell right as the cursor moved.
        Span::styled(cursor_glyph.clone(), rev),
        Span::raw(after_cursor.to_string()),
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
            .map(|&j| col.all[j].text.chars().count() + 3)
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
        .map(|&j| ListItem::new(Line::from(Span::raw(col.all[j].text.clone()))))
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

    // preview cell: an interactive frame list when focused, else text.
    // Titled with the pipeline it is actually previewing — which is the line
    // only up to the cursor, so moving back into an earlier token has a
    // visible explanation rather than looking like stale results.
    let pv = cells[1];
    let shown_pipeline = st.effective();
    let pv_block = Block::default()
        .borders(Borders::TOP)
        .title(if shown_pipeline.is_empty() {
            " (nothing to preview) ".to_string()
        } else {
            format!(" {shown_pipeline} ")
        });
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
                // A frame is not always one line: a hunk renders as a header
                // plus its body, and stuffing that into a single Span dropped
                // the newlines and flattened the hunk into an unreadable
                // smear.  One Line per row keeps it as it reads unfocused,
                // and a multi-row ListItem makes the selection move across
                // whole frames rather than rows.
                let text = render_frame_line(fr);
                let mut rows: Vec<Line> = text
                    .lines()
                    .enumerate()
                    .map(|(n, raw)| {
                        // the gutter marks the frame, so only its first row
                        let g = if n == 0 { glyph } else { " " };
                        let line = Line::from(vec![
                            Span::styled(g, style),
                            Span::raw(" "),
                            Span::raw(raw.to_string()),
                        ]);
                        if in_visual {
                            line.style(Style::default().bg(Color::Rgb(40, 50, 60)))
                        } else {
                            line
                        }
                    })
                    .collect();
                // a frame that renders to nothing still needs a row to sit on
                if rows.is_empty() {
                    rows.push(Line::from(" "));
                }
                ListItem::new(rows)
            })
            .collect();
        let mut ls = ListState::default();
        ls.select(Some(st.preview_sel));
        f.render_stateful_widget(
            List::new(items)
                .block(pv_block)
                .highlight_style(sel)
                .highlight_symbol("▌"),
            pv,
            &mut ls,
        );
    } else if !st.frames.is_empty() {
        f.render_widget(
            Paragraph::new(render_frames_text(&st.frames))
                .block(pv_block)
                .wrap(Wrap { trim: false }),
            pv,
        );
    } else {
        f.render_widget(
            Paragraph::new(st.message.clone().unwrap_or_default())
                .block(pv_block)
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
                    "^j/^k extend  space mark range  v cancel  ↵ act on selection"
                } else {
                    "^j/^k move  v visual  space mark  ↵ pivot  ^H back"
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
        assert_eq!(st.active().line, "commits where ");
        // and the new position offers fields, not steps
        assert!(st.active().all.iter().any(|c| c.kind == "field"));
    }

    #[test]
    fn backspace_deletes_one_character_not_a_token() {
        // the whole point: editing keys mean what they mean in a shell
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(st.active().line, "commits in ");
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().line, "commits in");
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().line, "commits i");
    }

    #[test]
    fn alt_backspace_kills_a_word() {
        let mut st = CompleterState::new("commits where author ");
        st.handle(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(st.active().line, "commits where ");
        st.handle(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(st.active().line, "commits ");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_move_to_the_ends() {
        let mut st = CompleterState::new("commits where");
        st.handle(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(st.active().cursor, 0);
        st.handle(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(st.active().cursor, "commits where".len());
    }

    #[test]
    fn editing_in_the_middle_completes_the_token_the_cursor_is_in() {
        let mut st = CompleterState::new("commits where author");
        // put the cursor just after `where`
        st.handle(KeyCode::Char('a'), KeyModifiers::CONTROL);
        for _ in 0..13 {
            st.handle(KeyCode::Right, KeyModifiers::NONE);
        }
        assert_eq!(st.active().query(), "where");
        // typing here edits in place rather than appending
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().line, "commits wher author");
    }

    #[test]
    fn ctrl_u_clears_the_line_and_ctrl_d_deletes_forward() {
        let mut st = CompleterState::new("commits where");
        st.handle(KeyCode::Char('a'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(st.active().line, "ommits where");
        st.handle(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(st.active().line, "");
        assert_eq!(st.active().cursor, 0);
    }

    #[test]
    fn a_drill_replaces_the_line_and_leaves_it_editable() {
        // a drill replaces the pipeline, so the old prefix is not a prefix of
        // the new one — stepping back has to be a history pop, not string
        // surgery on the current text
        let mut st = CompleterState::new("commits ");
        st.frames = vec![commit_frame("cafef00d")];
        st.handle(KeyCode::Char('w'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char('l'), KeyModifiers::NONE);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.active().line, "cafef00d ");

        // and the drilled line is ordinary editable text from here
        st.handle(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(st.active().line, "");
    }

    #[test]
    fn backspace_at_the_start_of_the_line_does_nothing() {
        let mut st = CompleterState::new("");
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        st.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(st.active().line, "");
        assert_eq!(st.active().cursor, 0);
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
        assert_eq!(st.active().query(), "");
    }

    #[test]
    fn movement_in_the_drilled_preview_works_with_and_without_ctrl() {
        // regression: the bare j/k arms shadowed the Ctrl ones entirely, so
        // ^j/^k did nothing here
        let mut st = CompleterState::new("commits ");
        st.frames = vec![commit_frame("aaaa"), commit_frame("bbbb")];
        st.handle(KeyCode::Char('w'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char('l'), KeyModifiers::NONE);
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

    fn ctrl_w(st: &mut CompleterState, k: char) {
        st.handle(KeyCode::Char('w'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char(k), KeyModifiers::NONE);
    }

    #[test]
    fn ctrl_w_h_and_l_move_focus_like_vim_windows() {
        let mut st = CompleterState::new("commits ");
        st.frames = vec![commit_frame("aaaa")];
        ctrl_w(&mut st, 'l');
        assert_eq!(st.focus, Focus::Preview);
        ctrl_w(&mut st, 'h');
        assert_eq!(st.focus, Focus::Columns);
    }

    #[test]
    fn ctrl_w_w_goes_to_the_other_pane_from_either_side() {
        let mut st = CompleterState::new("commits ");
        st.frames = vec![commit_frame("aaaa")];
        ctrl_w(&mut st, 'w');
        assert_eq!(st.focus, Focus::Preview);
        ctrl_w(&mut st, 'w');
        assert_eq!(st.focus, Focus::Columns);
    }

    #[test]
    fn the_key_after_ctrl_w_is_never_typed_into_the_query() {
        // an unmapped window key does nothing, as in vim
        let mut st = CompleterState::new("commits ");
        ctrl_w(&mut st, 'z');
        assert_eq!(st.active().query(), "");
        assert_eq!(st.focus, Focus::Columns);
        ctrl_w(&mut st, 'h');
        assert_eq!(st.active().query(), "", "a window key was typed");
    }

    #[test]
    fn ctrl_l_and_ctrl_h_switch_focus_without_the_prefix() {
        // measured: ^H arrives as Char('h')+CONTROL, distinct from Backspace,
        // so it is safe to bind in a pane that also edits text
        let mut st = CompleterState::new("commits ");
        st.frames = vec![commit_frame("aaaa")];
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Preview);
        st.handle(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Columns);
    }

    #[test]
    fn ctrl_l_is_swallowed_rather_than_reaching_the_shell() {
        // the completer owns the terminal in raw mode, so a ^L bound to
        // clear-history in the spawning shell must never see it
        let mut st = CompleterState::new("commits ");
        st.frames.clear(); // nothing to preview: ^L still must not fall through
        st.handle(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(st.focus, Focus::Columns);
        assert_eq!(st.active().line, "commits ", "^L was typed into the line");
        assert!(!st.quit);
    }

    #[test]
    fn ctrl_w_l_into_an_empty_preview_is_a_no_op() {
        let mut st = CompleterState::new("commits ");
        st.frames.clear();
        ctrl_w(&mut st, 'l');
        assert_eq!(st.focus, Focus::Columns);
    }

    #[test]
    fn ctrl_l_drills_the_preview_and_pivots_onto_the_selected_frame() {
        let mut st = CompleterState::new("commits ");
        // stand in a known preview rather than depend on the repo's frames
        st.frames = vec![commit_frame("cafef00d")];
        st.message = None;
        st.handle(KeyCode::Char('w'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(st.focus, Focus::Preview);
        st.handle(KeyCode::Enter, KeyModifiers::NONE); // pivot on the frame
        assert_eq!(st.focus, Focus::Columns);
        assert_eq!(st.active().line, "cafef00d ");
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
    fn tab_mid_token_replaces_the_whole_word() {
        // regression: completing `comm|its` left the tail behind, giving
        // `commits its`
        let mut st = CompleterState::new("commits in main");
        st.active_mut().move_cursor(4);
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(st.active().line, "commits in main");
    }

    #[test]
    fn the_preview_stops_at_the_cursor() {
        // cursor back inside `commits`: the preview answers what the pipeline
        // means *there*, so the later steps are not applied
        let mut st = CompleterState::new("commits in main");
        st.active_mut().move_cursor(4); // inside `commits`
        assert_eq!(st.active().query(), "comm");
        assert_eq!(st.effective(), "commits");
    }

    #[test]
    fn accepting_keeps_the_tail_the_preview_dropped() {
        // the preview may stop at the cursor; Enter must not silently discard
        // the rest of the line
        let mut st = CompleterState::new("commits in main");
        st.active_mut().move_cursor(4);
        assert_eq!(st.effective(), "commits");
        assert_eq!(st.active().completed_line(), "commits in main");
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.accepted.as_deref(), Some("commits in main"));
    }

    #[test]
    fn the_preview_is_populated_the_moment_the_completer_opens() {
        // nothing typed, `commits` merely highlighted — the preview must
        // already show what it would produce, or the opening screen teaches
        // nothing about what the tool does
        let st = CompleterState::new("");
        assert_eq!(
            st.active().highlighted().map(|c| c.text.as_str()),
            Some("commits")
        );
        assert_eq!(st.effective(), "commits");
        assert!(!st.frames.is_empty(), "preview blank on open");
    }

    #[test]
    fn the_preview_follows_the_highlight_but_enter_does_not() {
        // the two rules differ on purpose: the preview shows what the
        // highlight *would* give, Enter takes only what is on the line
        let mut st = CompleterState::new("");
        st.handle(KeyCode::Char('n'), KeyModifiers::CONTROL); // next source
        let hl = st.active().highlighted().unwrap().text.clone();
        assert_eq!(
            st.effective(),
            hl,
            "preview stopped following the highlight"
        );
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.accepted.as_deref(), Some(""), "Enter took the highlight");
    }

    #[test]
    fn substituting_into_the_middle_of_a_line_keeps_a_separator() {
        let mut st = CompleterState::new("commits  where");
        // cursor between the two spaces: empty query, non-empty tail
        st.active_mut().move_cursor(9);
        assert_eq!(st.active().query(), "");
        assert!(
            !st.effective().contains("where") || st.effective().contains(" where"),
            "candidate ran into the following token: {}",
            st.effective()
        );
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
        st.handle(KeyCode::Char('w'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(st.focus, Focus::Columns);
    }

    // --- visual selection in the preview ----------------------------------

    fn previewing(n: usize) -> CompleterState {
        let mut st = CompleterState::new("commits ");
        st.frames = (0..n).map(|i| commit_frame(&format!("sha{i}"))).collect();
        st.handle(KeyCode::Char('w'), KeyModifiers::CONTROL);
        st.handle(KeyCode::Char('l'), KeyModifiers::NONE);
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
    fn space_banks_the_range_and_leaves_visual_so_another_can_be_made() {
        let mut st = previewing(10);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 1);
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(st.visual_from.is_none(), "still in visual mode after m");
        assert_eq!(st.marked.len(), 2);

        // a second, disjoint run
        down(&mut st, 4);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 1);
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(st.selection_step().as_deref(), Some("[0..2,5..7]"));
    }

    #[test]
    fn contiguous_marks_collapse_into_one_range_and_singles_stay_bare() {
        let mut st = previewing(10);
        for r in [0usize, 1, 2, 5, 8] {
            st.preview_sel = r;
            st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        }
        assert_eq!(st.selection_step().as_deref(), Some("[0..3,5,8]"));
    }

    #[test]
    fn space_over_an_already_marked_range_unmarks_it() {
        let mut st = previewing(8);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(st.selection_step().as_deref(), Some("[0..3]"));

        // select the same range again and toggle it off
        st.preview_sel = 0;
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(st.marked.is_empty(), "range stayed marked: {:?}", st.marked);
    }

    #[test]
    fn space_over_a_partly_marked_range_marks_all_of_it() {
        // only a fully marked range comes off; a partial one fills in, so a
        // sloppy overlap adds rather than punching holes
        let mut st = previewing(8);
        st.preview_sel = 1;
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE); // mark row 1 only
        st.preview_sel = 0;
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(st.selection_step().as_deref(), Some("[0..3]"));
    }

    #[test]
    fn m_no_longer_marks() {
        let mut st = previewing(4);
        st.handle(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(st.marked.is_empty(), "m still marks");
    }

    #[test]
    fn space_on_an_already_marked_row_unmarks_it() {
        let mut st = previewing(4);
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(st.marked.len(), 1);
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
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
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE); // a mark
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
        assert_eq!(st.active().line, "commits [0..3] ");
        // the shape is unchanged, so the same candidates still apply
        assert!(st.active().all.iter().any(|c| c.kind == "step"));
        // and the selection does not linger
        assert!(st.marked.is_empty() && st.visual_from.is_none());
    }

    #[test]
    fn a_selection_leaves_an_editable_line() {
        let mut st = previewing(8);
        st.handle(KeyCode::Char('v'), KeyModifiers::NONE);
        down(&mut st, 2);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.active().line, "commits [0..3] ");
        // the emitted step is text like any other — kill the word to drop it
        st.handle(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(st.active().line, "commits ");
    }

    #[test]
    fn pivoting_with_no_selection_still_re_roots_on_the_object() {
        // the single-object drill must not regress into a slice
        let mut st = previewing(3);
        st.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(st.active().line, "sha0 ");
    }

    #[test]
    fn the_emitted_step_is_a_pipeline_the_language_actually_parses() {
        let mut st = previewing(10);
        for r in [1usize, 2, 6] {
            st.preview_sel = r;
            st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
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
        st.handle(KeyCode::Char(' '), KeyModifiers::NONE);
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
        let before = st.active().query();
        let (c, m) = alt('x');
        st.handle(c, m);
        assert_eq!(st.focus, Focus::Palette);
        assert_eq!(st.active().query(), before, "M-x leaked a character");
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
            st.active().query(),
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
        assert_eq!(st.active().query(), "");
        assert_eq!(st.focus, Focus::Columns);
    }

    #[test]
    fn the_palette_does_not_disturb_the_column_behind_it() {
        let mut st = CompleterState::new("commits ");
        st.handle(KeyCode::Tab, KeyModifiers::NONE);
        let prefix = st.active().line.clone();
        let (c, m) = alt('x');
        st.handle(c, m);
        st.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(st.active().line, prefix);
    }
}
