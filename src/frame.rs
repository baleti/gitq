//! Frames: the flat, typed records every pipeline value is a list of.
//!
//! A frame's *shape* is the set of fields it carries; shapes are identified
//! structurally (by field-set), not nominally — see `doc/gitq.org`.
//!
//! Field values are `Arc<str>` slices rather than owned `String`s.  The
//! Haskell build got its speed from every value being a zero-copy slice of
//! one decoded git-output buffer; `Arc<str>` is the ownership-checked
//! equivalent — cloning a value is a refcount bump, and the buffer lives
//! exactly as long as the frames that point into it.

use std::collections::BTreeMap;
use std::sync::Arc;

/// A scalar field value.  Dates and SHAs are text at runtime; their scalar
/// *type* (which decides the operators that apply) lives in
/// [`crate::registry`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Str(Arc<str>),
    Num(i64),
    Bool(bool),
}

impl Value {
    /// Text content of a value, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Everything but an explicit `false` is true.
    ///
    /// Inherited from the elisp original's truthiness, and deliberately
    /// kept: it is now what `where <flag> is true` means in the language,
    /// not an accident of the host.
    pub fn truthy(v: Option<&Value>) -> bool {
        match v {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => true,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(Arc::from(s))
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(Arc::from(s))
    }
}

/// The runtime shape tag.  Structural field-sets do the type-checking; this
/// tag only drives rendering and the handful of executor branches that need
/// to know what they are looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Commit,
    Ref,
    Worktree,
    Blob,
    Tree,
    Diff,
    Hunk,
    Line,
    DiffLine,
    Projection,
}

impl FrameType {
    pub fn as_str(self) -> &'static str {
        match self {
            FrameType::Commit => "commit",
            FrameType::Ref => "ref",
            FrameType::Worktree => "worktree",
            FrameType::Blob => "blob",
            FrameType::Tree => "tree",
            FrameType::Diff => "diff",
            FrameType::Hunk => "hunk",
            FrameType::Line => "line",
            FrameType::DiffLine => "diff-line",
            FrameType::Projection => "projection",
        }
    }
}

/// One record flowing through a pipeline.  `parents` is only ever non-empty
/// on commit frames; it backs the computed `parents-count` field and the
/// `parent` morphism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: FrameType,
    pub parents: Vec<Arc<str>>,
    /// Ordered so that `pick` projections and `--sexp` output keep a stable
    /// field order across runs without a separate sort.
    pub attrs: BTreeMap<String, Value>,
}

impl Frame {
    /// Build a frame from a tag and attribute pairs.
    pub fn new<I, K>(ty: FrameType, attrs: I) -> Frame
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        Frame {
            ty,
            parents: Vec::new(),
            attrs: attrs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }

    /// Build a frame derived from a parent frame — hunks, diff lines, grep
    /// lines.  Context propagation happens here, as a property of
    /// construction, so a future derived shape cannot forget to carry the
    /// commit metadata over (grep's line frames did exactly that for two
    /// releases while hunk and diff-line frames carried it).
    pub fn derived<I, K>(parent: &Frame, ty: FrameType, attrs: I) -> Frame
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        let mut f = Frame::new(ty, attrs);
        // Haskell built this as `frame tag (attrs ++ commitContext parent)`
        // through `M.fromList`, where the LAST binding of a duplicate key
        // wins — so commit context overrides, it does not defer.  No shape
        // currently collides, but keep the precedence identical.
        for (k, v) in parent.commit_context() {
            f.attrs.insert(k, v);
        }
        f
    }

    /// The shared commit context a derived frame carries along: the owning
    /// commit's author, date, and message (whichever are present).
    pub fn commit_context(&self) -> Vec<(String, Value)> {
        ["author", "date", "message"]
            .iter()
            .filter_map(|k| self.field(k).map(|v| ((*k).to_string(), v)))
            .collect()
    }

    /// Extract a field.  `author` falls back to `name` (so ref frames answer
    /// author-flavored queries with their name); `parents-count` is computed
    /// from the parents list unless a `pick` projection already fixed it,
    /// and so is always present on every frame.
    ///
    /// Returns an owned `Value` rather than a reference precisely because
    /// `parents-count` is synthesised. Splitting that into a second
    /// by-reference accessor would leave two functions disagreeing about
    /// whether an absent `parents-count` is `None` or `Num(0)` — the kind of
    /// silent divergence this port exists to avoid. Cloning is an `Arc`
    /// bump.
    pub fn field(&self, name: &str) -> Option<Value> {
        match name {
            "author" => self
                .attrs
                .get("author")
                .or_else(|| self.attrs.get("name"))
                .cloned(),
            "parents-count" => Some(
                self.attrs
                    .get("parents-count")
                    .cloned()
                    .unwrap_or(Value::Num(self.parents.len() as i64)),
            ),
            _ => self.attrs.get(name).cloned(),
        }
    }

    /// The commit SHA a frame refers to: its own `commit-sha` back-pointer
    /// if it has one (hunk, line, diff-line frames), else its `sha`.
    pub fn commit_sha(&self) -> Option<Arc<str>> {
        match self.field("commit-sha") {
            Some(Value::Str(s)) => Some(s),
            _ => match self.field("sha") {
                Some(Value::Str(s)) => Some(s),
                _ => None,
            },
        }
    }
}
