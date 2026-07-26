//! The pipeline AST: `source step* terminal?`.

use crate::frame::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub source: Source,
    pub steps: Vec<Step>,
    pub terminal: Option<Terminal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Commits(Option<String>),
    Branches,
    Tags,
    Refs,
    Worktrees,
    Blobs,
    /// Every hunk of every commit — the same frames `commits via diff.hunks`
    /// produces, as a source, because searching hunks is the common case and
    /// commits are usually the filter rather than the subject.
    Hunks,
    /// `HEAD`, a branch, tag, or SHA — resolves to one commit.
    Ref(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryFilter {
    Blob,
    Tree,
}

/// A typed map from one frame shape to a *list* of frames of another.
/// Composition is generic — `flat_map` over the frame list — and
/// [`crate::registry`] carries each morphism's domain and codomain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Morphism {
    /// All parents.
    Parent,
    /// `parent[N]`.
    ParentIdx(usize),
    /// Ancestor closure, inclusive.
    ParentStar,
    /// Ancestor closure, exclusive.
    ParentPlus,
    /// `parent†` — children-of.
    ParentAdjoint,
    Tree,
    TreeEntries(Option<EntryFilter>),
    /// Optional REF to diff against.
    Diff(Option<String>),
    DiffHunks,
    DiffLines,
    /// Standalone `hunks` on a diff-shaped frame.  The Haskell build only
    /// ever offered the fused `diff.hunks`; this is the missing factor, and
    /// `diff.hunks == diff . hunks` is asserted as a coherence law.
    Hunks,
    History,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Via(Morphism),
    Where(Vec<Cond>),
    /// Pattern, is-regex.
    Grep(String, bool),
    /// Pattern, is-regex.
    Pickaxe(String, bool),
    /// Glob.
    Path(String),
    Pick(Vec<String>),
    /// Positional selection, Python-slice semantics: a union of selectors
    /// evaluated left to right, so `[0:3,-1]` is "the first three, then the
    /// last" and `[::-1]` reverses.
    Slice(Vec<Sel>),
    /// Field, descending.
    Sort(String, bool),
    /// Restrict to commits reachable per a raw revspec (mid-pipeline `in`).
    /// Git parses the string, we don't.
    InRange(String),
    /// Keep N lines around pattern matches in `content`.  Patterns (with
    /// their regex flags) are baked in at parse time — either written
    /// explicitly, or inherited from preceding content searches.
    Context(usize, Vec<(String, bool)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    pub field: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    /// Never typed explicitly: the implicit substring match.
    Contains,
    Regex,
    After,
    Before,
    Within,
    Is,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    Show,
    Copy,
    Insert,
    Count,
    /// `/delete` is a parse-time alias.
    Remove,
    Stage,
    /// Name, worktree path.
    BranchOff(Option<String>, Option<String>),
    /// No-edit, message.
    Amend(bool, Option<String>),
    Squash(Option<String>),
    Reword(Option<String>),
    Commit(Option<String>),
    Mark(Option<String>),
    Worktree(Option<String>),
}

/// One selector inside a [`Step::Slice`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sel {
    /// A single position; negative counts from the end.  Out of range is an
    /// error, as in Python — asking for one specific row that isn't there is
    /// a mistake, not an empty result.
    Index(isize),
    /// `start:stop:step`, each optional.  Half-open and clamping, as in
    /// Python: `[0:1000]` on ten rows is ten rows, not an error.
    Range {
        start: Option<isize>,
        stop: Option<isize>,
        step: Option<isize>,
    },
}
