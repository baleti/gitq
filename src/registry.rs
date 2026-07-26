//! The registries: single sources of truth shared by the parser, the type
//! checker, the completion engine, and the executor — so they can never
//! disagree about which fields, morphisms, operators, or terminals exist.
//!
//! The Haskell original expressed these as association lists walked with
//! `lookup`, an elisp-era alist idiom. They are `match` arms here: same
//! single-source-of-truth property, but the compiler sees the whole table.

use crate::ast::{EntryFilter, Morphism, Source};

/// Scalar type of a field: decides which where-operators apply and which
/// sort comparator is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Str,
    Sha,
    Date,
    Number,
    Flag,
}

/// The closed set of field names `where`, `sort`, and `pick` accept.
/// Used for completion and lexical disambiguation; validation of any
/// particular reference is against the narrower frame-shape field-sets
/// below, threaded through the pipeline as the current field-set.
pub const FIELD_NAMES: &[&str] = &[
    "sha",
    "author",
    "email",
    "date",
    "message",
    "path",
    "name",
    "branch",
    "parents-count",
    "modified",
    "staged",
    "untracked",
    "tree",
    "reftype",
    "detached",
    "mode",
    "parent-sha",
    "commit-sha",
    "start-line",
    "end-line",
    "line-number",
    "content",
    "sign",
];

/// Scalar type of each field.  Unknown fields default to string — the
/// weakest assumption — because `pick` projections are open-ended.
pub fn field_type(f: &str) -> FieldType {
    match f {
        "date" => FieldType::Date,
        "sha" | "tree" | "parent-sha" | "commit-sha" => FieldType::Sha,
        "parents-count" | "start-line" | "end-line" | "line-number" => FieldType::Number,
        "modified" | "staged" | "untracked" | "detached" => FieldType::Flag,
        _ => FieldType::Str,
    }
}

/// The closed set of where-operators, in registry order.  Completion emits
/// them in this order, so it is part of the observable interface.
pub const OPERATOR_NAMES: &[&str] = &[
    "==", "!=", ">", "<", ">=", "<=", "regex", "after", "before", "within", "is",
];

/// For each where-operator, the field scalar types it accepts.  There is no
/// `contains` entry: a value right after a field with no recognized operator
/// between them is an implicit substring match instead.
pub fn operator_signature(op: &str) -> Option<&'static [FieldType]> {
    use FieldType::*;
    Some(match op {
        "==" | "!=" => &[Str, Sha, Date, Number, Flag],
        ">" | "<" | ">=" | "<=" => &[Number],
        "regex" => &[Str, Sha],
        "after" | "before" | "within" => &[Date],
        "is" => &[Flag],
        _ => return None,
    })
}

/// The implicit operator applied when the token right after a field is a
/// value rather than a recognized operator keyword: substring match for
/// text-shaped fields (`where author bal`, `where date 2026-07`), equality
/// for numbers (`where parents-count 2` — substring over digits would make
/// 2 match 12, a footgun).  Flag fields have no implicit operator: they are
/// bare conditions (`where modified`), and an unrecognized token after one
/// is still an unknown-operator parse error.
pub fn implicit_op(t: FieldType) -> Option<crate::ast::Op> {
    use crate::ast::Op;
    match t {
        FieldType::Str | FieldType::Sha | FieldType::Date => Some(Op::Contains),
        FieldType::Number => Some(Op::Eq),
        FieldType::Flag => None,
    }
}

/// Reserved step keywords: these always start a new stage and must be
/// quoted when used as string values.
pub const STEP_KEYWORDS: &[&str] = &[
    "via", "where", "grep", "pickaxe", "path", "pick", "sort", "context",
];

// Structural field-set typing: the exact set of fields each frame shape
// carries, taken from where that shape is constructed in git.rs/exec.rs.

pub const COMMIT_FIELDS: &[&str] = &[
    "sha",
    "author",
    "email",
    "date",
    "message",
    "tree",
    "parents-count",
];

pub const REF_FIELDS: &[&str] = &["sha", "name", "reftype"];

/// Unlike the Haskell build, `modified`/`staged`/`untracked` are actually
/// populated by the worktree fetcher — see the gap fixes in this port.
pub const WORKTREE_FIELDS: &[&str] = &[
    "path",
    "sha",
    "branch",
    "detached",
    "modified",
    "staged",
    "untracked",
];

pub const BLOB_FIELDS: &[&str] = &["sha", "path", "mode"];

pub const TREE_OBJECT_FIELDS: &[&str] = &["sha"];

pub const DIFF_FIELDS: &[&str] = &["sha", "path", "parent-sha"];

/// Hunk frames carry the whole hunk body in `content` and the owning
/// commit's author/date/message; still no `sha` of their own, so
/// grep/pickaxe cannot follow.
pub const HUNK_FIELDS: &[&str] = &[
    "path",
    "start-line",
    "end-line",
    "content",
    "commit-sha",
    "author",
    "date",
    "message",
];

/// Line frames (grep output) carry the owning commit's metadata, like every
/// commit-derived shape — `Frame::derived` makes this structural.
pub const LINE_FIELDS: &[&str] = &[
    "sha",
    "path",
    "line-number",
    "content",
    "commit-sha",
    "author",
    "date",
    "message",
];

/// Diff-line frames also carry the owning commit's metadata.
pub const DIFF_LINE_FIELDS: &[&str] = &[
    "path",
    "line-number",
    "content",
    "sign",
    "commit-sha",
    "author",
    "date",
    "message",
];

/// The field-set each source's frames start the pipeline with.
pub fn source_fields(src: &Source) -> &'static [&'static str] {
    match src {
        // An SRef resolves to exactly one commit.
        Source::Commits(_) | Source::Ref(_) => COMMIT_FIELDS,
        Source::Branches | Source::Tags | Source::Refs => REF_FIELDS,
        Source::Worktrees => WORKTREE_FIELDS,
        Source::Blobs => BLOB_FIELDS,
    }
}

/// The field a morphism's input shape must carry (its domain).
pub fn morphism_requires(m: &Morphism) -> &'static str {
    match m {
        Morphism::Parent | Morphism::ParentIdx(_) | Morphism::ParentStar | Morphism::ParentPlus => {
            "parents-count"
        }
        Morphism::ParentAdjoint => "sha",
        Morphism::Tree => "tree",
        Morphism::TreeEntries(_) => "sha",
        Morphism::Diff(_) | Morphism::DiffHunks | Morphism::DiffLines => "sha",
        // The standalone factor of `diff.hunks` consumes a diff-shaped
        // frame, which is what makes `diff.hunks == diff . hunks` typecheck.
        // Its domain is `parent-sha` rather than the more obvious `path`:
        // under structural typing a shape IS its field-set, and `path` also
        // appears on blob, hunk, and line shapes, so requiring it would let
        // `blobs via hunks` typecheck into nonsense.  `parent-sha` occurs in
        // exactly one field-set, DIFF_FIELDS.
        Morphism::Hunks => "parent-sha",
        Morphism::History => "path",
        Morphism::Commit => "commit-sha",
    }
}

/// The field-set a morphism's output frames carry (its codomain), which
/// becomes the current field-set for the rest of the pipeline.
pub fn morphism_yields(m: &Morphism) -> &'static [&'static str] {
    match m {
        Morphism::Parent
        | Morphism::ParentIdx(_)
        | Morphism::ParentStar
        | Morphism::ParentPlus
        | Morphism::ParentAdjoint
        | Morphism::History
        | Morphism::Commit => COMMIT_FIELDS,
        Morphism::Tree => TREE_OBJECT_FIELDS,
        Morphism::TreeEntries(_) => BLOB_FIELDS,
        Morphism::Diff(_) => DIFF_FIELDS,
        Morphism::DiffHunks | Morphism::Hunks => HUNK_FIELDS,
        Morphism::DiffLines => DIFF_LINE_FIELDS,
    }
}

/// Surface forms a morphism path is built from, matched greedily
/// longest-first at each position.  Order here is irrelevant (the match is
/// by length), but the list is kept in the Haskell original's order so the
/// two can be diffed by eye.
const LITERAL_FORMS: &[(&str, Morphism)] = &[
    (".parent*", Morphism::ParentStar),
    (".parent+", Morphism::ParentPlus),
    (".parent\u{2020}", Morphism::ParentAdjoint),
    (".parent", Morphism::Parent),
    (
        ".tree.entries[Blob]",
        Morphism::TreeEntries(Some(EntryFilter::Blob)),
    ),
    (
        ".tree.entries[Tree]",
        Morphism::TreeEntries(Some(EntryFilter::Tree)),
    ),
    (".tree.entries", Morphism::TreeEntries(None)),
    (
        ".tree.blobs",
        Morphism::TreeEntries(Some(EntryFilter::Blob)),
    ),
    (
        ".tree.subtrees",
        Morphism::TreeEntries(Some(EntryFilter::Tree)),
    ),
    (".tree", Morphism::Tree),
    (
        ".entries[Blob]",
        Morphism::TreeEntries(Some(EntryFilter::Blob)),
    ),
    (
        ".entries[Tree]",
        Morphism::TreeEntries(Some(EntryFilter::Tree)),
    ),
    (".entries", Morphism::TreeEntries(None)),
    (".diff.hunks", Morphism::DiffHunks),
    (".diff.lines", Morphism::DiffLines),
    (".diff", Morphism::Diff(None)),
    // Gap fix: the standalone factor the Haskell build never exposed.
    (".hunks", Morphism::Hunks),
    (".history", Morphism::History),
    (".commit", Morphism::Commit),
];

/// A segment boundary is a `.` or the end of the path.
fn boundary_at(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with('.')
}

/// Parse a morphism path.  A path may be written bare (`parent`,
/// `tree.entries[Blob]`, `parent.tree`) or with the historical leading dot
/// (`.parent`, ...) — both normalize to the dotted form before matching.
/// Errors on the first unrecognizable segment, naming it and the full path.
pub fn parse_morphism_path(raw: &str) -> Result<Vec<Morphism>, String> {
    let owned;
    let path: &str = if raw.starts_with('.') {
        raw
    } else {
        owned = format!(".{raw}");
        &owned
    };

    let mut out = Vec::new();
    let mut rest = path;

    while !rest.is_empty() {
        match longest_match_at(rest) {
            Some((consumed, m)) => {
                out.push(m);
                rest = &rest[consumed..];
            }
            None => {
                let mut msg = format!("gitq: unknown morphism '{rest}'");
                if rest.len() < path.len() {
                    msg.push_str(&format!(" (in '{raw}')"));
                }
                return Err(msg);
            }
        }
    }
    Ok(out)
}

/// Every form matching at the head of `rest` with a valid boundary, longest
/// winning.  `parent[N]` is generated rather than listed.
fn longest_match_at(rest: &str) -> Option<(usize, Morphism)> {
    let mut best: Option<(usize, Morphism)> = None;

    for (form, m) in LITERAL_FORMS {
        if rest.starts_with(form)
            && boundary_at(&rest[form.len()..])
            && best.as_ref().is_none_or(|(n, _)| form.len() > *n)
        {
            best = Some((form.len(), m.clone()));
        }
    }

    if let Some(after) = rest.strip_prefix(".parent[") {
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let tail = &after[digits.len()..];
        if !digits.is_empty() {
            if let Some(tail) = tail.strip_prefix(']') {
                // A non-parsing index (absurdly long digit run) is left
                // unmatched, so it surfaces as `unknown morphism` rather
                // than silently wrapping or panicking.
                if boundary_at(tail) {
                    if let Ok(n) = digits.parse::<usize>() {
                        let len = ".parent[".len() + digits.len() + 1;
                        if best.as_ref().is_none_or(|(b, _)| len > *b) {
                            best = Some((len, Morphism::ParentIdx(n)));
                        }
                    }
                }
            }
        }
    }

    best
}

/// The terminal registry's names, in registry order.  The completion
/// candidate list derives from this, so completion can never offer a
/// terminal the parser rejects.
pub const TERMINAL_NAMES: &[&str] = &[
    "show",
    "copy",
    "insert",
    "count",
    "remove",
    "delete",
    "stage",
    "branch-off",
    "amend",
    "squash",
    "reword",
    "commit",
    "mark",
    "worktree",
];

// Completion candidate sets ----------------------------------------------

pub const COMPLETE_SOURCE_KEYWORDS: &[&str] = &[
    "commits",
    "branches",
    "tags",
    "refs",
    "worktrees",
    "blobs",
    "HEAD",
];

/// Canonical single-morphism forms offered after `via`; compositions
/// (`parent.tree`, ...) are typed by hand and parsed generically.
pub const COMPLETE_MORPHISMS: &[&str] = &[
    "parent",
    "parent*",
    "parent+",
    "parent\u{2020}",
    "tree",
    "tree.blobs",
    "tree.subtrees",
    "tree.entries",
    "tree.entries[Blob]",
    "tree.entries[Tree]",
    "diff",
    "diff.hunks",
    "diff.lines",
    "hunks",
    "history",
    "commit",
];

pub const COMPLETE_WHERE_OPERATORS: &[&str] = OPERATOR_NAMES;

pub const COMPLETE_DATE_WITHIN_EXAMPLES: &[&str] = &[
    "1 day", "3 days", "1 week", "2 weeks", "1 month", "3 months", "6 months", "1 year",
];

pub fn complete_terminals() -> Vec<String> {
    TERMINAL_NAMES.iter().map(|t| format!("/{t}")).collect()
}

/// Category label of a completion candidate, reflecting gitq's own grammar:
/// source, step, morphism, field, operator, or terminal.  A leading `-`
/// (sort negation) is ignored.  Checked in the same order as the Emacs Lisp
/// original's `gitq--token-kind`, so `path` (both a step keyword and a
/// field) classifies as a step.
pub fn token_kind(cand: &str) -> Option<&'static str> {
    let key = match cand.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => rest,
        _ => cand,
    };
    if COMPLETE_SOURCE_KEYWORDS.contains(&key) {
        Some("source")
    } else if STEP_KEYWORDS.contains(&key) {
        Some("step")
    } else if COMPLETE_MORPHISMS.contains(&key) {
        Some("morphism")
    } else if FIELD_NAMES.contains(&key) {
        Some("field")
    } else if COMPLETE_WHERE_OPERATORS.contains(&key) {
        Some("operator")
    } else if key.starts_with('/') && TERMINAL_NAMES.contains(&&key[1..]) {
        Some("terminal")
    } else {
        None
    }
}

/// Short description shown as a completion annotation for a token.
pub fn describe_token(tok: &str) -> Option<&'static str> {
    Some(match tok {
        // sources
        "commits" => "commits reachable from HEAD",
        "branches" => "local branch refs",
        "tags" => "tag refs",
        "refs" => "all refs (branches, tags, ...)",
        "worktrees" => "linked worktrees",
        "blobs" => "blob/tree entries under HEAD's tree",
        "HEAD" => "the current commit",
        "refspec" => "keep only commits in a git range, e.g. main..HEAD",
        // steps
        "via" => "traverse a morphism (parent, tree, diff, ...)",
        "where" => "filter by field conditions",
        "grep" => "search blob/commit content for a pattern",
        "pickaxe" => "filter commits whose diff adds/removes a pattern",
        "path" => "path glob step, or the file-path field",
        "pick" => "project onto specific fields",
        "sort" => "sort by field (prefix with - for descending)",
        "context" => "trim content to N lines around matches (like grep -C)",
        // morphisms
        "parent" => "first parent commit",
        "parent*" => "all reachable ancestors, inclusive",
        "parent+" => "all reachable ancestors, exclusive",
        "parent\u{2020}" => "children-of: commits whose parent is in the result",
        "tree" => "the commit's tree, or (as a field) its SHA",
        "tree.blobs" => "blob entries in the tree",
        "tree.subtrees" => "subtree entries in the tree",
        "tree.entries" => "all tree entries",
        "tree.entries[Blob]" => "blob entries only",
        "tree.entries[Tree]" => "subtree entries only",
        "diff" => "paths changed vs. parent (or REF)",
        "diff.hunks" => "line ranges changed vs. parent",
        "diff.lines" => "actual +/- diff lines vs. parent, with content",
        "hunks" => "line ranges of an already-taken diff",
        "history" => "commits that touched this path",
        "commit" => "resolve to the referenced commit",
        // fields
        "sha" => "commit SHA",
        "author" => "author name",
        "email" => "author email",
        "date" => "commit date",
        "message" => "commit message",
        "name" => "ref/branch name",
        "branch" => "worktree's branch",
        "parents-count" => "number of parents",
        "modified" => "has modified/unstaged changes",
        "staged" => "has staged changes",
        "untracked" => "has untracked files",
        "reftype" => "ref kind (branch or tag)",
        "detached" => "worktree HEAD is detached",
        "mode" => "tree entry file mode",
        "parent-sha" => "the ref/SHA a diff was compared against",
        "commit-sha" => "commit a hunk/grep line belongs to",
        "start-line" => "hunk's first changed line",
        "end-line" => "hunk's last changed line",
        "line-number" => "grep/diff-line match's line number",
        "content" => "grep/diff-line match's line content",
        "sign" => "\"+\" (added) or \"-\" (removed) diff line",
        // operators
        "==" => "equals",
        "!=" => "not equals",
        ">" => "greater than",
        "<" => "less than",
        ">=" => "greater or equal",
        "<=" => "less or equal",
        // DEVIATION from 0.7.0, which said "(POSIX ERE)".  This port swaps
        // regex-tdfa for the Rust regex crate, so the annotation would
        // otherwise describe an engine that is no longer there.  Expect this
        // string to differ in the golden corpus; that is the point.
        "regex" => "regex match (Rust regex syntax)",
        "after" => "date is after value",
        "before" => "date is before value",
        "within" => "date is within \"N day/week/month/year(s)\"",
        "is" => "boolean flag is true",
        // terminals
        "/show" => "print/display results",
        "/copy" => "copy the SHA of the first result",
        "/insert" => "insert the SHA of the first result",
        "/count" => "show the result count",
        "/branch-off" => "create a branch from the first result",
        "/amend" => "amend HEAD with the first result",
        "/squash" => "squash results into one commit",
        "/reword" => "reword the first result's commit message",
        "/remove" => "remove the first result's commit",
        "/delete" => "delete the first result's commit",
        "/commit" => "create a commit",
        "/stage" => "stage modified files",
        "/mark" => "attach a git note label",
        "/worktree" => "add a worktree",
        "no-edit" => "reuse HEAD's existing commit message",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morphism_paths_parse_greedily_longest_first() {
        use Morphism::*;
        let cases: &[(&str, Vec<Morphism>)] = &[
            ("parent", vec![Parent]),
            ("parent*", vec![ParentStar]),
            ("parent+", vec![ParentPlus]),
            ("parent\u{2020}", vec![ParentAdjoint]),
            ("parent[0]", vec![ParentIdx(0)]),
            ("parent[12]", vec![ParentIdx(12)]),
            ("tree", vec![Tree]),
            ("tree.entries", vec![TreeEntries(None)]),
            (
                "tree.entries[Blob]",
                vec![TreeEntries(Some(EntryFilter::Blob))],
            ),
            ("diff.hunks", vec![DiffHunks]),
            ("diff.lines", vec![DiffLines]),
            ("diff", vec![Diff(None)]),
            ("parent.tree", vec![Parent, Tree]),
            // the historical leading-dot spelling normalizes to the same AST
            (".parent.tree", vec![Parent, Tree]),
        ];
        for (input, want) in cases {
            assert_eq!(&parse_morphism_path(input).unwrap(), want, "path {input:?}");
        }
    }

    #[test]
    fn tree_dot_entries_beats_tree_then_entries() {
        // `.tree.entries` must match as ONE morphism, not Tree followed by
        // Entries — longest-match is what makes the fused form win.
        assert_eq!(
            parse_morphism_path("tree.entries").unwrap(),
            vec![Morphism::TreeEntries(None)]
        );
    }

    #[test]
    fn unknown_morphisms_name_themselves_and_their_path() {
        let e = parse_morphism_path("nosuch").unwrap_err();
        assert!(e.contains("unknown morphism"), "{e}");
        // a partially-consumed path also names the whole path it came from
        let e = parse_morphism_path("parent.nosuch").unwrap_err();
        assert!(e.contains("unknown morphism"), "{e}");
        assert!(e.contains("parent.nosuch"), "{e}");
    }

    #[test]
    fn hunks_domain_is_unique_to_the_diff_shape() {
        // `hunks` is the standalone factor of `diff.hunks`; under structural
        // typing its domain must be a field that ONLY diff frames carry, or
        // nonsense like `blobs via hunks` would typecheck.
        let dom = morphism_requires(&Morphism::Hunks);
        let carriers: Vec<&&[&str]> = [
            &COMMIT_FIELDS,
            &REF_FIELDS,
            &WORKTREE_FIELDS,
            &BLOB_FIELDS,
            &TREE_OBJECT_FIELDS,
            &DIFF_FIELDS,
            &HUNK_FIELDS,
            &LINE_FIELDS,
            &DIFF_LINE_FIELDS,
        ]
        .into_iter()
        .filter(|fs| fs.contains(&dom))
        .collect();
        assert_eq!(
            carriers.len(),
            1,
            "{dom} is carried by {} shapes",
            carriers.len()
        );
        assert!(DIFF_FIELDS.contains(&dom));
    }

    #[test]
    fn diff_hunks_equals_diff_then_hunks() {
        // Coherence law: the fused path and the composed path must agree on
        // both ends. The Haskell suite asserted the squares it could build;
        // this one only became expressible once `hunks` existed.
        assert_eq!(
            morphism_yields(&Morphism::DiffHunks),
            morphism_yields(&Morphism::Hunks)
        );
        assert_eq!(
            morphism_requires(&Morphism::Diff(None)),
            morphism_requires(&Morphism::DiffHunks)
        );
        // and the composite must actually typecheck: diff's codomain has to
        // carry what hunks demands
        assert!(
            morphism_yields(&Morphism::Diff(None)).contains(&morphism_requires(&Morphism::Hunks))
        );
    }

    #[test]
    fn every_morphism_domain_is_a_known_field() {
        use Morphism::*;
        for m in [
            Parent,
            ParentIdx(0),
            ParentStar,
            ParentPlus,
            ParentAdjoint,
            Tree,
            TreeEntries(None),
            Diff(None),
            DiffHunks,
            DiffLines,
            Hunks,
            History,
            Commit,
        ] {
            let d = morphism_requires(&m);
            assert!(FIELD_NAMES.contains(&d), "{m:?} requires unknown field {d}");
        }
    }

    #[test]
    fn every_completion_candidate_classifies() {
        // Completion must never offer a token whose kind it cannot name —
        // the annotator would render a blank column.
        let mut all: Vec<String> = Vec::new();
        all.extend(COMPLETE_SOURCE_KEYWORDS.iter().map(|s| s.to_string()));
        all.extend(STEP_KEYWORDS.iter().map(|s| s.to_string()));
        all.extend(COMPLETE_MORPHISMS.iter().map(|s| s.to_string()));
        all.extend(FIELD_NAMES.iter().map(|s| s.to_string()));
        all.extend(COMPLETE_WHERE_OPERATORS.iter().map(|s| s.to_string()));
        all.extend(complete_terminals());
        for c in all {
            assert!(token_kind(&c).is_some(), "no kind for candidate {c:?}");
        }
    }

    #[test]
    fn every_completion_candidate_is_described() {
        let mut all: Vec<String> = Vec::new();
        all.extend(COMPLETE_SOURCE_KEYWORDS.iter().map(|s| s.to_string()));
        all.extend(STEP_KEYWORDS.iter().map(|s| s.to_string()));
        all.extend(COMPLETE_MORPHISMS.iter().map(|s| s.to_string()));
        all.extend(FIELD_NAMES.iter().map(|s| s.to_string()));
        all.extend(COMPLETE_WHERE_OPERATORS.iter().map(|s| s.to_string()));
        all.extend(complete_terminals());
        for c in all {
            assert!(describe_token(&c).is_some(), "no description for {c:?}");
        }
    }

    #[test]
    fn every_morphism_surface_form_parses() {
        // The completion list and the parser must agree: anything offered
        // after `via` has to be something the parser accepts.
        for m in COMPLETE_MORPHISMS {
            assert!(
                parse_morphism_path(m).is_ok(),
                "completion offers {m:?} but the parser rejects it"
            );
        }
    }

    #[test]
    fn every_operator_has_a_signature() {
        for op in OPERATOR_NAMES {
            assert!(operator_signature(op).is_some(), "no signature for {op}");
        }
    }

    #[test]
    fn path_classifies_as_a_step_not_a_field() {
        // `path` is both a step keyword and a field name; the elisp original
        // resolved the collision in favour of the step, and the order of the
        // checks in token_kind is what preserves that.
        assert_eq!(token_kind("path"), Some("step"));
    }

    #[test]
    fn sort_negation_classifies_as_its_underlying_field() {
        assert_eq!(token_kind("-date"), Some("field"));
        assert_eq!(token_kind("-"), None);
    }
}
