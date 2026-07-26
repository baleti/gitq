//! Pipeline execution: sources, morphisms (`via`), and relational steps.
//!
//! Each morphism maps one frame to a *list* of frames; execution lifts that
//! map pointwise over the incoming list and appends the results — the
//! composition that needs no plumbing, spelled `flat_map` here and Kleisli
//! composition over the list monad in the Haskell original.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use regex::Regex;

use crate::ast::*;
use crate::frame::{Frame, FrameType, Value};
use crate::git::*;
use crate::registry::{field_type, FieldType};

fn s(v: &str) -> Value {
    Value::Str(Arc::from(v))
}

fn str_field(f: &Frame, k: &str) -> Option<Arc<str>> {
    match f.field(k) {
        Some(Value::Str(v)) => Some(v),
        _ => None,
    }
}

/// Execute a parsed pipeline's source and steps, returning the final frames
/// and the (unapplied) terminal.  The terminal is identified but never run
/// here — callers decide whether to apply it for real or ignore it for a
/// read-only preview.
pub fn exec_pipeline(p: &Pipeline) -> R<(Vec<Frame>, Option<Terminal>)> {
    let mut frames = exec_source(&p.source)?;
    for step in &p.steps {
        frames = exec_step(frames, step)?;
    }
    Ok((frames, p.terminal.clone()))
}

pub fn exec_source(src: &Source) -> R<Vec<Frame>> {
    Ok(match src {
        Source::Commits(range) => fetch_commits(range.as_deref())?,
        Source::Ref(r) => fetch_commit(r).into_iter().collect(),
        Source::Branches => fetch_branches(),
        Source::Tags => fetch_tags(),
        Source::Refs => fetch_refs(),
        Source::Worktrees => fetch_worktrees(),
        // every commit's hunks: the same frames `commits via diff.hunks`
        // gives, so the two agree by construction
        Source::Hunks => fetch_commits(None)?.iter().flat_map(hunks_of).collect(),
        Source::Blobs => match run_git_string(&["rev-parse", "HEAD^{tree}"]) {
            Some(tree) => fetch_blobs_at(&tree, None, None),
            None => Vec::new(),
        },
    })
}

pub fn exec_step(frames: Vec<Frame>, step: &Step) -> R<Vec<Frame>> {
    Ok(match step {
        Step::Via(m) => exec_via(frames, m)?,

        Step::Where(conds) => {
            // Compile each condition once (regex compiled, period parsed) —
            // never per frame.
            let preds: Vec<CompiledCond> = conds.iter().map(CompiledCond::new).collect();
            let now = Utc::now();
            frames
                .into_iter()
                .filter(|f| preds.iter().all(|p| p.eval(f, now)))
                .collect()
        }

        Step::Grep(pat, re) => exec_grep(&frames, pat, *re),
        Step::Pickaxe(pat, re) => exec_pickaxe(frames, pat, *re),

        Step::Path(pat) => frames
            .into_iter()
            .filter(|f| match f.field("path") {
                Some(Value::Str(p)) => path_matches(pat, &p),
                _ => false,
            })
            .collect(),

        Step::Pick(fields) => frames.iter().map(|f| project(fields, f)).collect(),

        Step::Slice(sels) => {
            let idx = crate::slice::positions(sels, frames.len()).map_err(GitqError)?;
            idx.into_iter().map(|i| frames[i].clone()).collect()
        }
        Step::Sort(field, desc) => exec_sort(frames, field, *desc),

        Step::InRange(revspec) => {
            // git parses the revspec (so A..B, A...B, --not, multiple revs
            // and :/msg all work); we only intersect the incoming stream by
            // commit SHA — the commit-sha fallback in Frame::commit_sha
            // makes this uniform across commit, hunk, line and diff-line
            // frames.  A revspec git rejects is a loud error: silently
            // treating it as an empty set would be exactly the wrong-answer
            // class this language exists to kill.
            let mut args = vec!["rev-list"];
            let parts: Vec<&str> = revspec.split_whitespace().collect();
            args.extend_from_slice(&parts);
            match run_git_loud(&args) {
                Err(e) => return gitq_error(format!("gitq in: {e}")),
                Ok(shas) => {
                    let set: HashSet<&str> = shas.iter().map(String::as_str).collect();
                    frames
                        .into_iter()
                        .filter(|f| f.commit_sha().is_some_and(|s| set.contains(s.as_ref())))
                        .collect()
                }
            }
        }

        Step::Context(n, pats) => {
            let matchers = Matchers::new(pats);
            frames
                .into_iter()
                .map(|f| trim_context(*n, &matchers, f))
                .collect()
        }
    })
}

// --- conditions ----------------------------------------------------------

/// One condition with its per-step work hoisted out of the per-frame path.
struct CompiledCond {
    field: String,
    op: Op,
    value: Value,
    /// Pre-compiled for `regex`; the parser already validated the pattern,
    /// so a failure here cannot happen and degrades to "never matches"
    /// rather than panicking.
    re: Option<Regex>,
    /// Pre-parsed for `within`.
    period_secs: Option<f64>,
}

impl CompiledCond {
    fn new(c: &Cond) -> CompiledCond {
        let re = match (&c.op, &c.value) {
            (Op::Regex, Value::Str(v)) => Regex::new(v).ok(),
            _ => None,
        };
        let period_secs = match (&c.op, &c.value) {
            (Op::Within, Value::Str(v)) => parse_period(v),
            _ => None,
        };
        CompiledCond {
            field: c.field.clone(),
            op: c.op,
            value: c.value.clone(),
            re,
            period_secs,
        }
    }

    fn eval(&self, f: &Frame, now: DateTime<Utc>) -> bool {
        let actual = f.field(&self.field);

        let num_cmp = |ord: fn(i64, i64) -> bool| match (&actual, &self.value) {
            (Some(Value::Num(a)), Value::Num(v)) => ord(*a, *v),
            _ => false,
        };
        let date_cmp = |ord: fn(DateTime<Utc>, DateTime<Utc>) -> bool| match (&actual, &self.value)
        {
            (Some(Value::Str(a)), Value::Str(v)) => match (parse_date(a), parse_date(v)) {
                (Some(ta), Some(tv)) => ord(ta, tv),
                _ => false,
            },
            _ => false,
        };

        match self.op {
            Op::Eq => actual.as_ref() == Some(&self.value),
            Op::Ne => actual.as_ref() != Some(&self.value),
            Op::Gt => num_cmp(|a, v| a > v),
            Op::Lt => num_cmp(|a, v| a < v),
            Op::Ge => num_cmp(|a, v| a >= v),
            Op::Le => num_cmp(|a, v| a <= v),
            Op::Contains => match (&actual, &self.value) {
                (Some(Value::Str(a)), Value::Str(v)) => a.contains(v.as_ref()),
                _ => false,
            },
            Op::Regex => match (&actual, &self.re) {
                (Some(Value::Str(a)), Some(re)) => re.is_match(a),
                _ => false,
            },
            Op::After => date_cmp(|a, v| a > v),
            Op::Before => date_cmp(|a, v| a < v),
            Op::Within => match (&actual, self.period_secs) {
                (Some(Value::Str(a)), Some(secs)) => match parse_date(a) {
                    Some(t) => (now - t).num_seconds() as f64 <= secs,
                    None => false,
                },
                _ => false,
            },
            Op::Is => match &self.value {
                Value::Bool(true) => Value::truthy(actual.as_ref()),
                v => actual.as_ref() == Some(v),
            },
        }
    }
}

/// Parse a date string leniently: git's ISO `%ai` format, ISO 8601, or a
/// bare year/month/day prefix.
pub fn parse_date(t: &str) -> Option<DateTime<Utc>> {
    // offset-carrying forms first
    for fmt in ["%Y-%m-%d %H:%M:%S %z", "%Y-%m-%dT%H:%M:%S%z"] {
        if let Ok(d) = DateTime::parse_from_str(t, fmt) {
            return Some(d.with_timezone(&Utc));
        }
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, fmt) {
            return Some(Utc.from_utc_datetime(&d));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    // bare year-month and year: the Haskell used parseTimeM's defaulting,
    // which fills the missing components with 1 / zero
    if let Ok(d) = NaiveDate::parse_from_str(&format!("{t}-01"), "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    if let Ok(d) = NaiveDate::parse_from_str(&format!("{t}-01-01"), "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    None
}

/// "N day/week/month/year(s)" as seconds.
fn parse_period(p: &str) -> Option<f64> {
    let mut w = p.split_whitespace();
    let n_str = w.next()?;
    let unit = w.next()?;
    if n_str.is_empty() || !n_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: f64 = n_str.parse().ok()?;
    let unit = unit.strip_suffix('s').unwrap_or(unit);
    let secs = match unit {
        "day" => 86400.0,
        "week" => 604800.0,
        "month" => 2592000.0,
        "year" => 31536000.0,
        _ => return None,
    };
    Some(n * secs)
}

// --- morphisms -----------------------------------------------------------

fn exec_via(frames: Vec<Frame>, m: &Morphism) -> R<Vec<Frame>> {
    Ok(match m {
        // Parent/commit lookups are batched: collect the wanted SHAs first,
        // fetch them all in one git process, then reassemble in the original
        // order (duplicates included, as one-process-per-SHA produced them).
        Morphism::Parent => {
            let shas: Vec<Arc<str>> = frames.iter().flat_map(|f| f.parents.clone()).collect();
            batch_lookup(&shas)
        }
        Morphism::ParentIdx(i) => {
            let shas: Vec<Arc<str>> = frames
                .iter()
                .filter_map(|f| f.parents.get(*i).cloned())
                .collect();
            batch_lookup(&shas)
        }
        Morphism::ParentStar => traverse_parents_star(&frames, false),
        Morphism::ParentPlus => traverse_parents_star(&frames, true),
        Morphism::ParentAdjoint => via_parent_adjoint(&frames),
        Morphism::Tree => frames.iter().filter_map(tree_of).collect(),
        Morphism::TreeEntries(fl) => frames.iter().flat_map(|f| entries_of(*fl, f)).collect(),
        Morphism::Diff(r) => frames
            .iter()
            .flat_map(|f| diff_of(r.as_deref(), f))
            .collect(),
        Morphism::DiffHunks => frames.iter().flat_map(hunks_of).collect(),
        Morphism::Hunks => frames.iter().flat_map(hunks_of_diff).collect(),
        Morphism::DiffLines => frames.iter().flat_map(diff_lines_of).collect(),
        Morphism::History => via_history(&frames),
        Morphism::Commit => {
            let shas: Vec<Arc<str>> = frames
                .iter()
                .filter_map(|f| str_field(f, "commit-sha"))
                .collect();
            batch_lookup(&shas)
        }
    })
}

fn batch_lookup(shas: &[Arc<str>]) -> Vec<Frame> {
    let cmap = commit_map_for(shas);
    shas.iter()
        .filter_map(|sha| cmap.get(sha.as_ref()).cloned())
        .collect()
}

/// SHA-keyed commit map for a batch of full SHAs: the in-process gix
/// backend when it works, else one subprocess batch.
fn commit_map_for(shas: &[Arc<str>]) -> BTreeMap<String, Frame> {
    let mut seen: HashSet<String> = HashSet::new();
    let uniq: Vec<Arc<str>> = shas
        .iter()
        .filter(|s| seen.insert(s.to_string()))
        .cloned()
        .collect();
    match crate::native::native_commits(false, &uniq) {
        Some(fs) => crate::native::by_sha(fs),
        None => fetch_commit_map(shas),
    }
}

fn tree_of(f: &Frame) -> Option<Frame> {
    let t = str_field(f, "tree")?;
    Some(Frame::new(
        FrameType::Tree,
        vec![("sha".to_string(), Value::Str(t))],
    ))
}

fn entries_of(fl: Option<EntryFilter>, f: &Frame) -> Vec<Frame> {
    let tree = match (f.ty, str_field(f, "tree")) {
        (FrameType::Commit, Some(t)) => Some(t),
        _ => str_field(f, "sha"),
    };
    match tree {
        Some(t) => fetch_blobs_at(&t, None, fl),
        None => Vec::new(),
    }
}

fn diff_of(refname: Option<&str>, f: &Frame) -> Vec<Frame> {
    let Some(sha) = str_field(f, "sha") else {
        return Vec::new();
    };
    // A root commit (no parents) has no "sha^" to diff against — --root
    // diffs it against the empty tree instead of erroring.
    let no_parent = refname.is_none() && f.parents.is_empty();
    let other: Option<String> = if no_parent {
        None
    } else {
        Some(
            refname
                .map(str::to_string)
                .unwrap_or_else(|| format!("{sha}^")),
        )
    };

    let paths = match &other {
        None => run_git(&[
            "diff-tree",
            "--root",
            "-r",
            "--name-only",
            "--no-commit-id",
            &sha,
        ]),
        Some(o) => run_git(&["diff-tree", "-r", "--name-only", "--no-commit-id", o, &sha]),
    };

    paths
        .iter()
        .map(|p| {
            let mut attrs = vec![
                ("sha".to_string(), Value::Str(sha.clone())),
                ("path".to_string(), s(p)),
            ];
            if let Some(o) = &other {
                attrs.push(("parent-sha".to_string(), s(o)));
            }
            Frame::new(FrameType::Diff, attrs)
        })
        .collect()
}

/// Diff arguments for a commit frame, honouring root commits.
fn diff_argv(sha: &str, root: bool) -> Vec<String> {
    if root {
        vec![
            "diff-tree".into(),
            "--root".into(),
            "-p".into(),
            "--no-commit-id".into(),
            "-r".into(),
            sha.to_string(),
        ]
    } else {
        vec![
            "diff-tree".into(),
            "-p".into(),
            "--no-commit-id".into(),
            "-r".into(),
            format!("{sha}^"),
            sha.to_string(),
        ]
    }
}

fn run_argv(argv: &[String]) -> Vec<String> {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    run_git(&refs)
}

fn hunks_of(f: &Frame) -> Vec<Frame> {
    let Some(sha) = str_field(f, "sha") else {
        return Vec::new();
    };
    // The Haskell always used `sha^` here, so hunks of a root commit came
    // back empty while diff.lines (which did pass --root) returned them.
    // Same rule for both now.
    let root = f.ty == FrameType::Commit && f.parents.is_empty();
    let ls = run_argv(&diff_argv(&sha, root));
    parse_diff_hunks(&ls, &sha, f)
}

/// `hunks` applied to an already-taken diff frame: restrict to that frame's
/// own path, which is what makes `diff.hunks == diff . hunks` hold.
fn hunks_of_diff(f: &Frame) -> Vec<Frame> {
    let (Some(sha), Some(path)) = (str_field(f, "sha"), str_field(f, "path")) else {
        return Vec::new();
    };
    let root = f.field("parent-sha").is_none();
    let ls = run_argv(&diff_argv(&sha, root));
    parse_diff_hunks(&ls, &sha, f)
        .into_iter()
        .filter(|h| str_field(h, "path").as_deref() == Some(path.as_ref()))
        .collect()
}

fn diff_lines_of(f: &Frame) -> Vec<Frame> {
    let Some(sha) = str_field(f, "sha") else {
        return Vec::new();
    };
    let root = f.ty == FrameType::Commit && f.parents.is_empty();
    let ls = run_argv(&diff_argv(&sha, root));
    parse_diff_lines(&ls, &sha, f)
}

/// History morphism: the commits that touched each frame's path.  One
/// `git log --follow` per path is inherent (--follow is single-path), but
/// resolving the resulting SHAs to frames is one batched fetch for all
/// paths together.
fn via_history(frames: &[Frame]) -> Vec<Frame> {
    let path_shas: Vec<(Arc<str>, Vec<String>)> = frames
        .iter()
        .map(|f| match str_field(f, "path") {
            Some(path) => {
                let shas = run_git(&["log", "--follow", "--format=%H", "--", &path]);
                (path, shas)
            }
            None => (Arc::from(""), Vec::new()),
        })
        .collect();

    let all: Vec<Arc<str>> = path_shas
        .iter()
        .flat_map(|(_, shas)| shas.iter().map(|s| Arc::from(s.as_str()) as Arc<str>))
        .collect();
    let cmap = commit_map_for(&all);

    let mut out = Vec::new();
    for (path, shas) in &path_shas {
        for sha in shas {
            if let Some(c) = cmap.get(sha.as_str()) {
                let mut c = c.clone();
                c.attrs.insert("path".to_string(), Value::Str(path.clone()));
                out.push(c);
            }
        }
    }
    out
}

/// Walk parent links from the given frames, returning all reachable commits
/// in discovery order.  When `exclude_start` (`parent+`), the start frames
/// themselves are excluded.
///
/// The reachable closure is materialised up front in two git processes
/// (`rev-list --stdin` for the SHA set, one batched log for their frames);
/// the walk then replays the original one-process-per-commit algorithm
/// purely in memory, preserving its discovery order exactly.
fn traverse_parents_star(frames: &[Frame], exclude_start: bool) -> Vec<Frame> {
    let start_shas: Vec<Arc<str>> = frames.iter().filter_map(|f| str_field(f, "sha")).collect();
    if start_shas.is_empty() {
        return Vec::new();
    }

    // The closure is materialised up front: in process via gix when
    // available, else in two git processes (`rev-list --stdin` for the SHA
    // set, one batched log for their frames).
    let cmap = match crate::native::native_commits(true, &start_shas) {
        Some(fs) => crate::native::by_sha(fs),
        None => {
            let mut input = String::new();
            for sha in &start_shas {
                input.push_str(sha);
                input.push('\n');
            }
            let reachable: Vec<Arc<str>> = run_git_stdin(&["rev-list", "--stdin"], &input)
                .iter()
                .map(|s| Arc::from(s.as_str()) as Arc<str>)
                .collect();
            fetch_commit_map(&reachable)
        }
    };

    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut acc: Vec<Frame> = Vec::new();

    for start in &start_shas {
        // Stack-ordered walk; parents are pushed reversed, exactly as the
        // original one-process-per-commit version discovered them.
        let mut queue: Vec<String> = vec![start.to_string()];
        while let Some(sha) = queue.pop() {
            if visited.contains(&sha) {
                continue;
            }
            visited.insert(sha.clone());
            let Some(c) = cmap.get(&sha) else { continue };
            if !(exclude_start && sha.as_str() == start.as_ref()) {
                acc.push(c.clone());
            }
            let unvisited: Vec<String> = c
                .parents
                .iter()
                .map(|p| p.to_string())
                .filter(|p| !visited.contains(p))
                .collect();
            // The Haskell pushed `reverse unvisited` onto the FRONT of a
            // list it also popped from the front.  Popping from the BACK
            // here means pushing in forward order to get the same visit
            // sequence — reversing as well would invert it, which only
            // shows up at merge commits.
            for p in unvisited {
                queue.push(p);
            }
        }
    }
    acc
}

/// Adjoint of parent: the commits whose parent is in the given frames.
fn via_parent_adjoint(frames: &[Frame]) -> Vec<Frame> {
    let targets: HashSet<String> = frames
        .iter()
        .filter_map(|f| str_field(f, "sha").map(|s| s.to_string()))
        .collect();
    fetch_commits(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.parents.iter().any(|p| targets.contains(p.as_ref())))
        .collect()
}

// --- diff parsing --------------------------------------------------------

/// `diff --git a/... b/PATH` → PATH (greedy: the last ` b/` splits, as the
/// original regex's greedy `.+` did, so paths containing spaces work).
fn diff_header_path(l: &str) -> Option<&str> {
    let rest = l.strip_prefix("diff --git a/")?;
    let idx = rest.rfind(" b/")?;
    let after = &rest[idx + 3..];
    if idx == 0 || after.is_empty() {
        None
    } else {
        Some(after)
    }
}

fn decimal(t: &str) -> Option<(i64, &str)> {
    let end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if end == 0 {
        return None;
    }
    Some((t[..end].parse().ok()?, &t[end..]))
}

/// Shared header parse: OLD start, NEW start, and the text after NEW.
fn new_side_of_hunk_header(l: &str) -> Option<(i64, i64, &str)> {
    let r0 = l.strip_prefix("@@ -")?;
    let (old, r1) = decimal(r0)?;
    let r2 = if let Some(r) = r1.strip_prefix(',') {
        let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        &r[end..]
    } else {
        r1
    };
    let r3 = r2.strip_prefix(" +")?;
    let (new, r4) = decimal(r3)?;
    Some((old, new, r4))
}

/// `@@ -a,b +START[,COUNT] @@` → (START, COUNT defaulting to 1)
fn hunk_header(l: &str) -> Option<(i64, i64)> {
    let (_, start, rest) = new_side_of_hunk_header(l)?;
    if let Some(r) = rest.strip_prefix(',') {
        let (count, r2) = decimal(r)?;
        if r2.starts_with(" @@") {
            return Some((start, count));
        }
        None
    } else if let Some(r) = rest.strip_prefix(' ') {
        if r.starts_with("@@") {
            Some((start, 1))
        } else {
            None
        }
    } else {
        None
    }
}

/// `@@ -OLD[,n] +NEW[...]` → (OLD, NEW)
fn line_hunk_header(l: &str) -> Option<(i64, i64)> {
    let (old, new, _) = new_side_of_hunk_header(l)?;
    Some((old, new))
}

/// Parse unified diff lines into hunk frames: line ranges plus the hunk's
/// full body text (context and ±lines, prefixes included) in `content`, so
/// whole hunks can be content-filtered and displayed.  The owning commit's
/// metadata rides along via [`Frame::derived`].
pub fn parse_diff_hunks(diff_lines: &[String], commit_sha: &str, parent: &Frame) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut open: Option<(String, i64, i64, Vec<String>)> = None;

    let flush = |open: &mut Option<(String, i64, i64, Vec<String>)>, out: &mut Vec<Frame>| {
        if let Some((path, start, count, body)) = open.take() {
            let mut content = body.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            out.push(Frame::derived(
                parent,
                FrameType::Hunk,
                vec![
                    ("path".to_string(), s(&path)),
                    ("start-line".to_string(), Value::Num(start)),
                    (
                        "end-line".to_string(),
                        Value::Num(start + (count - 1).max(0)),
                    ),
                    ("content".to_string(), s(&content)),
                    ("commit-sha".to_string(), s(commit_sha)),
                ],
            ));
        }
    };

    for l in diff_lines {
        if let Some(p) = diff_header_path(l) {
            flush(&mut open, &mut out);
            cur_path = Some(p.to_string());
        } else if cur_path.is_some() && hunk_header(l).is_some() {
            let (start, count) = hunk_header(l).unwrap();
            flush(&mut open, &mut out);
            open = Some((cur_path.clone().unwrap(), start, count, Vec::new()));
        } else if let Some((_, _, _, body)) = open.as_mut() {
            body.push(l.clone());
        }
    }
    flush(&mut open, &mut out);
    out
}

/// Parse unified diff lines into added/removed diff-line frames.
/// `line-number` is the new-file line for additions and the old-file line
/// for deletions.  The `+++`/`---` file headers can't be mistaken for
/// changed lines because they appear before any `@@` hunk header, when the
/// line cursors are still unset.
pub fn parse_diff_lines(diff_lines: &[String], commit_sha: &str, parent: &Frame) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut cursors: Option<(i64, i64)> = None;

    for l in diff_lines {
        if let Some(p) = diff_header_path(l) {
            cur_path = Some(p.to_string());
            cursors = None;
            continue;
        }
        let Some(path) = cur_path.as_deref() else {
            continue;
        };
        if let Some((old_n, new_n)) = line_hunk_header(l) {
            cursors = Some((old_n, new_n));
            continue;
        }
        let Some((old_n, new_n)) = cursors else {
            continue;
        };
        let mut cs = l.chars();
        match cs.next() {
            Some('+') => {
                out.push(mk_line(parent, path, "+", new_n, cs.as_str(), commit_sha));
                cursors = Some((old_n, new_n + 1));
            }
            Some('-') => {
                out.push(mk_line(parent, path, "-", old_n, cs.as_str(), commit_sha));
                cursors = Some((old_n + 1, new_n));
            }
            // "\ No newline at end of file"
            Some('\\') => {}
            _ => cursors = Some((old_n + 1, new_n + 1)),
        }
    }
    out
}

fn mk_line(
    parent: &Frame,
    path: &str,
    sign: &str,
    n: i64,
    content: &str,
    commit_sha: &str,
) -> Frame {
    Frame::derived(
        parent,
        FrameType::DiffLine,
        vec![
            ("path".to_string(), s(path)),
            ("sign".to_string(), s(sign)),
            ("line-number".to_string(), Value::Num(n)),
            ("content".to_string(), s(content)),
            ("commit-sha".to_string(), s(commit_sha)),
        ],
    )
}

// --- grep / pickaxe ------------------------------------------------------

fn exec_grep(frames: &[Frame], pat: &str, regex: bool) -> Vec<Frame> {
    let mut out = Vec::new();
    for f in frames {
        let Some(sha) = str_field(f, "sha") else {
            continue;
        };
        let mode = if regex { "-E" } else { "-F" };
        for l in run_git(&["grep", "-n", "--no-color", mode, pat, &sha]) {
            // "sha:path:line:content" — path may not contain ':', content may
            let Some((_, r1)) = l.split_once(':') else {
                continue;
            };
            let Some((path, r2)) = r1.split_once(':') else {
                continue;
            };
            let Some((n_str, content)) = r2.split_once(':') else {
                continue;
            };
            if n_str.is_empty() || !n_str.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            out.push(Frame::derived(
                f,
                FrameType::Line,
                vec![
                    ("sha".to_string(), Value::Str(sha.clone())),
                    ("path".to_string(), s(path)),
                    (
                        "line-number".to_string(),
                        Value::Num(n_str.parse().unwrap_or(0)),
                    ),
                    ("content".to_string(), s(content)),
                    ("commit-sha".to_string(), Value::Str(sha.clone())),
                ],
            ));
        }
    }
    out
}

fn exec_pickaxe(frames: Vec<Frame>, pat: &str, regex: bool) -> Vec<Frame> {
    let shas: Vec<Arc<str>> = frames.iter().filter_map(|f| str_field(f, "sha")).collect();
    if shas.is_empty() {
        return Vec::new();
    }
    // SHAs go through --stdin, never argv: a whole-history pickaxe (81k
    // SHAs on git/git) exceeds the OS argument-list limit as arguments.
    let mut input = String::new();
    for sha in &shas {
        input.push_str(sha);
        input.push('\n');
    }
    let mode = if regex { "-G" } else { "-S" };
    let hits: HashSet<String> = run_git_stdin(
        &["log", mode, pat, "--format=%H", "--no-walk", "--stdin"],
        &input,
    )
    .into_iter()
    .collect();

    frames
        .into_iter()
        .filter(|f| str_field(f, "sha").is_some_and(|s| hits.contains(s.as_ref())))
        .collect()
}

// --- context / projection / sort -----------------------------------------

/// Compiled content matchers for `context`, built once per step.
struct Matchers(Vec<Matcher>);

enum Matcher {
    Literal(String),
    Re(Regex),
}

impl Matchers {
    fn new(pats: &[(String, bool)]) -> Matchers {
        Matchers(
            pats.iter()
                .filter_map(|(p, is_re)| {
                    if *is_re {
                        Regex::new(p).ok().map(Matcher::Re)
                    } else {
                        Some(Matcher::Literal(p.clone()))
                    }
                })
                .collect(),
        )
    }

    fn any_match(&self, l: &str) -> bool {
        self.0.iter().any(|m| match m {
            Matcher::Literal(p) => l.contains(p.as_str()),
            Matcher::Re(re) => re.is_match(l),
        })
    }
}

/// Trim a frame's `content` to blocks of lines within N lines of a pattern
/// match, discontiguous blocks separated by a marker line — the gitq
/// analogue of `grep -C`.  Frames without content (or whose content has no
/// match) keep an empty content rather than disappearing.
fn trim_context(n: usize, matchers: &Matchers, mut f: Frame) -> Frame {
    let Some(Value::Str(c)) = f.field("content") else {
        return f;
    };
    let lines: Vec<&str> = c.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| matchers.any_match(l))
        .map(|(i, _)| i)
        .collect();

    let kept: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| hits.iter().any(|h| i.abs_diff(*h) <= n))
        .map(|(i, l)| (i, *l))
        .collect();

    // group contiguous runs
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut prev: Option<usize> = None;
    for (i, l) in kept {
        if prev.is_some_and(|p| i != p + 1) {
            blocks.push(std::mem::take(&mut cur));
        }
        cur.push(l);
        prev = Some(i);
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    let joined = blocks
        .iter()
        .map(|b| {
            let mut s = b.join("\n");
            s.push('\n');
            s
        })
        .collect::<Vec<_>>()
        .join("\u{b7}\u{b7}\u{b7}\n");

    f.attrs.insert("content".to_string(), s(&joined));
    f
}

/// Project each frame to only the listed fields.
fn project(fields: &[String], f: &Frame) -> Frame {
    let mut attrs = BTreeMap::new();
    for field in fields {
        if let Some(v) = f.field(field) {
            attrs.insert(field.clone(), v);
        }
    }
    Frame {
        ty: FrameType::Projection,
        parents: Vec::new(),
        attrs,
    }
}

/// Sort by a field, numeric or lexical per the field's scalar type.
fn exec_sort(mut frames: Vec<Frame>, field: &str, desc: bool) -> Vec<Frame> {
    let numeric = field_type(field) == FieldType::Number;
    // stable, like the Haskell sortBy
    frames.sort_by(|a, b| {
        let ord = if numeric {
            let as_num = |f: &Frame| match f.field(field) {
                Some(Value::Num(n)) => n,
                _ => 0,
            };
            as_num(a).cmp(&as_num(b))
        } else {
            let as_str = |f: &Frame| match f.field(field) {
                Some(Value::Str(v)) => v.to_string(),
                _ => String::new(),
            };
            as_str(a).cmp(&as_str(b))
        };
        if desc {
            ord.reverse()
        } else {
            ord
        }
    });
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> Frame {
        let mut f = Frame::new(
            FrameType::Commit,
            vec![("sha".to_string(), s(sha)), ("message".to_string(), s("m"))],
        );
        f.parents = parents.iter().map(|p| Arc::from(*p) as Arc<str>).collect();
        f
    }

    #[test]
    fn hunk_headers_parse_with_and_without_counts() {
        assert_eq!(hunk_header("@@ -1,3 +4,5 @@"), Some((4, 5)));
        assert_eq!(hunk_header("@@ -1 +4 @@"), Some((4, 1)));
        assert_eq!(hunk_header("@@ -1,3 +4,5 @@ fn foo()"), Some((4, 5)));
        assert_eq!(hunk_header("not a header"), None);
    }

    #[test]
    fn line_hunk_headers_give_both_cursors() {
        assert_eq!(line_hunk_header("@@ -7,3 +9,5 @@"), Some((7, 9)));
    }

    #[test]
    fn diff_header_paths_survive_spaces() {
        assert_eq!(
            diff_header_path("diff --git a/a.txt b/a.txt"),
            Some("a.txt")
        );
        assert_eq!(
            diff_header_path("diff --git a/my file.txt b/my file.txt"),
            Some("my file.txt")
        );
        assert_eq!(diff_header_path("index abc..def"), None);
    }

    #[test]
    fn diff_lines_track_old_and_new_cursors() {
        let parent = commit("abc", &[]);
        let diff = vec![
            "diff --git a/a.txt b/a.txt".to_string(),
            "--- a/a.txt".to_string(),
            "+++ b/a.txt".to_string(),
            "@@ -1,2 +1,3 @@".to_string(),
            " context".to_string(),
            "-removed".to_string(),
            "+added".to_string(),
            "+added2".to_string(),
        ];
        let fs = parse_diff_lines(&diff, "abc", &parent);
        assert_eq!(fs.len(), 3);
        assert_eq!(fs[0].field("sign").unwrap().as_str(), Some("-"));
        // context line advanced both cursors: old was 1, context -> 2
        assert_eq!(fs[0].field("line-number"), Some(Value::Num(2)));
        assert_eq!(fs[1].field("sign").unwrap().as_str(), Some("+"));
        assert_eq!(fs[1].field("line-number"), Some(Value::Num(2)));
        assert_eq!(fs[2].field("line-number"), Some(Value::Num(3)));
    }

    #[test]
    fn file_headers_are_never_mistaken_for_changed_lines() {
        let parent = commit("abc", &[]);
        // --- / +++ appear before any @@, so cursors are unset
        let diff = vec![
            "diff --git a/a.txt b/a.txt".to_string(),
            "--- a/a.txt".to_string(),
            "+++ b/a.txt".to_string(),
        ];
        assert!(parse_diff_lines(&diff, "abc", &parent).is_empty());
    }

    #[test]
    fn hunks_carry_body_and_commit_context() {
        let mut parent = commit("abc", &[]);
        parent.attrs.insert("author".to_string(), s("alice"));
        parent.attrs.insert("date".to_string(), s("2024-01-01"));
        let diff = vec![
            "diff --git a/a.txt b/a.txt".to_string(),
            "@@ -1,2 +1,2 @@".to_string(),
            " ctx".to_string(),
            "+new".to_string(),
        ];
        let fs = parse_diff_hunks(&diff, "abc", &parent);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].field("start-line"), Some(Value::Num(1)));
        assert_eq!(fs[0].field("end-line"), Some(Value::Num(2)));
        assert_eq!(
            fs[0].field("content").unwrap().as_str(),
            Some(" ctx\n+new\n")
        );
        // commit context reattached by construction
        assert_eq!(fs[0].field("author").unwrap().as_str(), Some("alice"));
        assert_eq!(fs[0].field("message").unwrap().as_str(), Some("m"));
    }

    #[test]
    fn dates_parse_in_every_accepted_shape() {
        assert!(parse_date("2024-01-01 10:00:00 +0000").is_some());
        assert!(parse_date("2024-01-01T10:00:00+0000").is_some());
        assert!(parse_date("2024-01-01 10:00:00").is_some());
        assert!(parse_date("2024-01-01 10:00").is_some());
        assert!(parse_date("2024-01-01").is_some());
        assert!(parse_date("2024-01").is_some());
        assert!(parse_date("2024").is_some());
        assert!(parse_date("not a date").is_none());
    }

    #[test]
    fn periods_parse_with_and_without_plurals() {
        assert_eq!(parse_period("1 day"), Some(86400.0));
        assert_eq!(parse_period("3 days"), Some(3.0 * 86400.0));
        assert_eq!(parse_period("2 weeks"), Some(2.0 * 604800.0));
        assert_eq!(parse_period("1 year"), Some(31536000.0));
        assert_eq!(parse_period("nonsense"), None);
        assert_eq!(parse_period("5 fortnights"), None);
    }

    #[test]
    fn sort_is_numeric_for_number_fields_and_lexical_otherwise() {
        // lexical sort would put 10 before 9
        let mk = |n: i64| {
            let mut f = Frame::new(FrameType::Commit, Vec::<(String, Value)>::new());
            f.attrs.insert("start-line".to_string(), Value::Num(n));
            f
        };
        let sorted = exec_sort(vec![mk(10), mk(9)], "start-line", false);
        assert_eq!(sorted[0].field("start-line"), Some(Value::Num(9)));
    }

    #[test]
    fn projection_drops_everything_unlisted() {
        let f = commit("abc", &["p"]);
        let p = project(&["sha".to_string()], &f);
        assert_eq!(p.ty, FrameType::Projection);
        assert_eq!(p.attrs.len(), 1);
        assert!(p.field("message").is_none());
        // parents are dropped, so parents-count becomes 0 not 1
        assert_eq!(p.field("parents-count"), Some(Value::Num(0)));
    }

    #[test]
    fn context_keeps_a_window_and_marks_discontiguous_blocks() {
        let mut f = Frame::new(FrameType::Hunk, Vec::<(String, Value)>::new());
        f.attrs.insert(
            "content".to_string(),
            s("a\nb\nNEEDLE\nd\ne\nf\ng\nNEEDLE\ni\n"),
        );
        let m = Matchers::new(&[("NEEDLE".to_string(), false)]);
        let out = trim_context(1, &m, f);
        let c = out.field("content").unwrap();
        let c = c.as_str().unwrap();
        assert!(c.contains("NEEDLE"));
        assert!(c.contains('\u{b7}'), "discontiguous blocks get a marker");
        assert!(!c.contains("\ne\n"), "line e is outside both windows");
    }

    #[test]
    fn context_with_no_match_yields_empty_content_not_a_dropped_frame() {
        let mut f = Frame::new(FrameType::Hunk, Vec::<(String, Value)>::new());
        f.attrs.insert("content".to_string(), s("a\nb\n"));
        let m = Matchers::new(&[("zzz".to_string(), false)]);
        let out = trim_context(1, &m, f);
        assert_eq!(out.field("content").unwrap().as_str(), Some(""));
    }

    #[test]
    fn conditions_evaluate_by_operator() {
        let now = Utc::now();
        let f = commit("abc123", &["p1", "p2"]);
        let c = |field: &str, op: Op, value: Value| {
            CompiledCond::new(&Cond {
                field: field.to_string(),
                op,
                value,
            })
            .eval(&f, now)
        };
        assert!(c("sha", Op::Contains, "abc".into()));
        assert!(!c("sha", Op::Contains, "zzz".into()));
        assert!(c("sha", Op::Eq, "abc123".into()));
        assert!(c("sha", Op::Ne, "other".into()));
        assert!(c("sha", Op::Regex, "^abc".into()));
        assert!(c("parents-count", Op::Eq, Value::Num(2)));
        assert!(c("parents-count", Op::Gt, Value::Num(1)));
        assert!(!c("parents-count", Op::Gt, Value::Num(2)));
        // a missing field never matches
        assert!(!c("nosuch", Op::Contains, "x".into()));
    }
}
