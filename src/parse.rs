//! The flat-syntax pipeline parser.
//!
//! Grammar:
//!
//! ```text
//! pipeline ::= source step* terminal?
//! source   ::= "commits" ["in" range-tokens] | "HEAD" | BRANCH
//!            | "branches" | "tags" | "refs" | "worktrees" | "blobs"
//! step     ::= "via" MORPHISM-PATH | "where" conditions | "grep" PATTERN
//!            | "pickaxe" PATTERN ["regex"] | "path" GLOB
//!            | "pick" FIELD[,...] | "[" SELECTORS "]"
//!            | "first" | "last" | "sort" ["-"]FIELD
//!            | "context" N [PATTERN] | "in" range-tokens
//! terminal ::= "/show" | "/copy" | ... (the closed terminal registry)
//! ```
//!
//! Typing happens at parse time, in two layers: structural field-sets
//! (threaded through every stage as the current shape) and scalar types
//! (each field's type × each operator's signature).  A query that cannot
//! mean what it says errors here, loudly — see `doc/gitq.org`, "Fail Loud".
//!
//! Structurally this is the Haskell parser with two elisp-era habits
//! removed: it walks a `&[Token]` slice instead of destructuring a cons
//! list, and it reads token *kinds* instead of re-inspecting token text.

use crate::ast::*;
use crate::frame::Value;
use crate::registry::*;
use crate::tokenize::{tokenize, Token};

/// A parse failure.
///
/// Deliberately a wrapper around the rendered message rather than an enum
/// of ~30 variants: every message is heavily parameterised, nothing
/// dispatches on the case (the Emacs client and the CLI both just print
/// it), and the tests assert on text. An enum would have been ceremony that
/// bought no checking. Construction still funnels through this module, so
/// the catalogue stays in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

type P<T> = Result<T, ParseError>;

fn perr<T>(msg: impl Into<String>) -> P<T> {
    Err(ParseError(msg.into()))
}

/// Field-sets are rendered into error messages constantly; keep it in one
/// place so the phrasing cannot drift between call sites.
fn list(fields: &[String]) -> String {
    fields.join(", ")
}

fn owned(fields: &[&str]) -> Vec<String> {
    fields.iter().map(|s| (*s).to_string()).collect()
}

/// Whether a token names a field in the current shape.  Compared by text
/// and not by kind on purpose: `path` is both a step keyword and a field,
/// so `pick path, author` and `where path == "x"` must read it as a field.
fn is_field(t: Option<&Token>, fields: &[String]) -> bool {
    t.is_some_and(|t| fields.iter().any(|f| f == t.text()))
}

fn is_step_kw(t: Option<&Token>) -> bool {
    matches!(t, Some(Token::Step(_)))
}

fn is_terminal(t: Option<&Token>) -> bool {
    matches!(t, Some(Token::Terminal(_)))
}

fn ftype_name(t: FieldType) -> &'static str {
    match t {
        FieldType::Str => "string",
        FieldType::Sha => "sha",
        FieldType::Date => "date",
        FieldType::Number => "number",
        FieldType::Flag => "flag",
    }
}

/// The operators applicable to a field type, in registry order — used to
/// make "does not apply" errors actionable.
fn ops_for(ft: FieldType) -> String {
    OPERATOR_NAMES
        .iter()
        .filter(|op| operator_signature(op).is_some_and(|sig| sig.contains(&ft)))
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Error unless the field is in the current field-set, naming the context.
fn require_field(fields: &[String], field: &str, context: &str) -> P<()> {
    if fields.iter().any(|f| f == field) {
        Ok(())
    } else {
        perr(format!(
            "gitq: '{context}' needs a '{field}' field, but the current frame only has: {}",
            list(fields)
        ))
    }
}

// --- entry point ---------------------------------------------------------

/// Parse a full pipeline string.
pub fn parse_pipeline(input: &str) -> P<Pipeline> {
    let tokens = tokenize(input.trim()).map_err(|e| ParseError(e.message))?;
    if tokens.is_empty() {
        return perr("gitq: empty pipeline");
    }

    let (source, rest) = parse_source(&tokens)?;
    let fields = owned(source_fields(&source));
    let (steps, terminal) = parse_rest(rest, fields)?;
    let steps = resolve_contexts(steps)?;

    Ok(Pipeline {
        source,
        steps,
        terminal,
    })
}

/// Whether a token is a bracket selection like `[0:10]` or `[-1]`.
fn is_selection(t: &Token) -> bool {
    let s = t.text();
    s.len() >= 2 && s.starts_with('[') && s.ends_with(']')
}

fn parse_rest(mut toks: &[Token], mut fields: Vec<String>) -> P<(Vec<Step>, Option<Terminal>)> {
    let mut steps = Vec::new();
    loop {
        let Some(head) = toks.first() else {
            return Ok((steps, None));
        };
        match head {
            Token::Terminal(name) => {
                let term = parse_terminal(name, &toks[1..])?;
                return Ok((steps, Some(term)));
            }
            Token::Step(_) => {
                let (new_steps, rest, new_fields) = parse_step(toks, &fields)?;
                steps.extend(new_steps);
                toks = rest;
                fields = new_fields;
            }
            // A bracket token is positional selection.  It is recognised by
            // *shape* rather than by a keyword, which is what lets it stay
            // punctuation — and it can only be read this way here, at step
            // position: brackets inside a `via` path index a morphism
            // (`parent[0]`), and brackets inside a value belong to that value
            // (`regex "^a[0-9]"`), because both are already part of a single
            // token by the time we get here.
            other if is_selection(other) => {
                let body = other.text();
                let body = &body[1..body.len() - 1];
                let sels = crate::slice::parse_selectors(body).map_err(ParseError)?;
                steps.push(Step::Slice(sels));
                toks = &toks[1..];
            }
            other => {
                let got = other.display();
                // The commonest cause is an unquoted value with a space in
                // it: `where date 2026-07-25 10:10:24 +0100` tokenizes as
                // three words, and only the first reaches the condition.  Say
                // so when the stray token looks like part of a value rather
                // than a mistyped keyword, so the fix is in the message
                // instead of the manual.
                let value_ish = got
                    .chars()
                    .any(|c| c.is_ascii_digit() || matches!(c, ':' | '+'));
                return perr(if value_ish {
                    format!(
                        "gitq: expected step keyword or /terminal, got '{got}' \
                         — if this is part of a value, quote the whole value: \
                         where date \"2026-07-25 10:10:24 +0100\""
                    )
                } else {
                    format!("gitq: expected step keyword or /terminal, got '{got}'")
                });
            }
        }
    }
}

// --- source --------------------------------------------------------------

/// Parse the source (first stage), returning it and the remaining tokens.
pub fn parse_source(toks: &[Token]) -> P<(Source, &[Token])> {
    let Some(kw) = toks.first() else {
        return perr("gitq: empty pipeline");
    };
    let rest = &toks[1..];
    let name = kw.text();

    Ok(match name {
        "commits" | "commit" => {
            // `commits in <range>` is the source-level range form; the range
            // runs until the next stage boundary.
            if rest.first().map(Token::text) == Some("in") {
                let after = &rest[1..];
                let end = after
                    .iter()
                    .position(|t| t.is_boundary())
                    .unwrap_or(after.len());
                // joined with spaces: the revspec is a list of arguments to
                // git (`main ^v0.6.0` is two revisions), and concatenating
                // them produced the single nonexistent revision
                // `main^v0.6.0`
                let range: String = after[..end]
                    .iter()
                    .map(Token::display)
                    .collect::<Vec<_>>()
                    .join(" ");
                if range.is_empty() {
                    return perr(
                        "gitq: 'in' requires a revision range, e.g. commits in main..HEAD",
                    );
                }
                (Source::Commits(Some(range)), &after[end..])
            } else {
                (Source::Commits(None), rest)
            }
        }
        "branches" => (Source::Branches, rest),
        "tags" => (Source::Tags, rest),
        "worktrees" | "worktree" => (Source::Worktrees, rest),
        "blobs" => (Source::Blobs, rest),
        "refs" => (Source::Refs, rest),
        _ => (Source::Ref(kw.display()), rest),
    })
}

// --- steps ---------------------------------------------------------------

/// Parse one step (first token must be a step keyword), threading the
/// current field-set.  Returns the parsed steps (a `via` path composes
/// several morphisms), the remaining tokens, and the new field-set.
pub fn parse_step<'a>(
    toks: &'a [Token],
    fields: &[String],
) -> P<(Vec<Step>, &'a [Token], Vec<String>)> {
    let Some(kw) = toks.first() else {
        return perr("gitq: internal error: parse_step on empty input");
    };
    let kw = kw.text().to_string();
    let toks = &toks[1..];
    let f = fields.to_vec();

    match kw.as_str() {
        "via" => {
            let (morphs, rest) = parse_via(toks)?;
            let path_tok = toks.first().map(Token::display).unwrap_or_default();
            // Type-check the chain by folding each morphism's registry
            // signature: its required field must be in the set yielded by
            // the previous one.
            let mut cur = f;
            for m in &morphs {
                require_field(&cur, morphism_requires(m), &format!("via {path_tok}"))?;
                cur = owned(morphism_yields(m));
            }
            Ok((morphs.into_iter().map(Step::Via).collect(), rest, cur))
        }

        "where" => {
            let (conds, rest) = parse_where(toks, &f)?;
            // Gap fix: 0.7.0 let a condition-less `where` through as a
            // keep-everything no-op — the only step that did; pick, sort,
            // grep and skip all errored.  Silently doing nothing is exactly
            // what "fail loud" forbids.
            if conds.is_empty() {
                let got = match toks.first() {
                    Some(t) => format!("'{}'", t.display()),
                    None => "end of input".to_string(),
                };
                return perr(format!(
                    "gitq: 'where' requires at least one condition, got {got}"
                ));
            }
            Ok((vec![Step::Where(conds)], rest, f))
        }

        "grep" => {
            require_field(&f, "sha", "grep")?;
            let Some(pat_tok) = toks.first() else {
                return perr("gitq: 'grep' requires a pattern");
            };
            let is_re = matches!(pat_tok, Token::Regex(_));
            Ok((
                vec![Step::Grep(pat_tok.text().to_string(), is_re)],
                &toks[1..],
                owned(LINE_FIELDS),
            ))
        }

        "pickaxe" => {
            require_field(&f, "sha", "pickaxe")?;
            let Some(pat_tok) = toks.first() else {
                return perr("gitq: 'pickaxe' requires a pattern");
            };
            let slash_re = matches!(pat_tok, Token::Regex(_));
            let rest0 = &toks[1..];
            let kw_re = rest0.first().map(Token::text) == Some("regex");
            let rest = if kw_re { &rest0[1..] } else { rest0 };
            Ok((
                vec![Step::Pickaxe(pat_tok.text().to_string(), slash_re || kw_re)],
                rest,
                f,
            ))
        }

        "path" => {
            require_field(&f, "path", "path")?;
            let Some(t) = toks.first() else {
                return perr("gitq: 'path' requires a glob pattern");
            };
            Ok((vec![Step::Path(t.text().to_string())], &toks[1..], f))
        }

        "pick" => {
            // Driven by field-list membership (plus comma), not the generic
            // boundary check: `path` is both a step keyword and a field, and
            // `pick path, author` must read it as a field here.
            let mut picked: Vec<String> = Vec::new();
            let mut i = 0;
            while i < toks.len() {
                if matches!(toks[i], Token::Comma) {
                    i += 1;
                } else if f.iter().any(|x| x == toks[i].text()) {
                    picked.push(toks[i].text().to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            let remaining = &toks[i..];
            if picked.is_empty() {
                let got = match remaining.first() {
                    Some(t) => format!("'{}'", t.display()),
                    None => "end of input".to_string(),
                };
                return perr(format!(
                    "gitq: 'pick' requires at least one field name, got {got}"
                ));
            }
            let new_fields = picked.clone();
            Ok((vec![Step::Pick(picked)], remaining, new_fields))
        }

        "in" => {
            // Mid-pipeline range restriction; needs a commit-identifying
            // field (hunk/line/diff-line frames carry commit-sha, not sha).
            if !f.iter().any(|x| x == "sha" || x == "commit-sha") {
                return perr(format!(
                    "gitq: 'in' needs a 'sha' or 'commit-sha' field, but the current frame only has: {}",
                    list(&f)
                ));
            }
            let end = toks
                .iter()
                .position(|t| t.is_boundary())
                .unwrap_or(toks.len());
            if end == 0 {
                return perr("gitq: 'in' requires a revision range");
            }
            // Space-joined, split back into argv at exec: multi-token
            // revspecs ("HEAD --not v1", "a b ^c") must reach rev-list as
            // separate arguments.
            let range = toks[..end]
                .iter()
                .map(Token::display)
                .collect::<Vec<_>>()
                .join(" ");
            Ok((vec![Step::InRange(range)], &toks[end..], f))
        }

        "context" => {
            require_field(&f, "content", "context")?;
            let (n, rest0) = parse_count(toks, "context")?;
            match rest0.first() {
                Some(t) if !t.is_boundary() => {
                    let is_re = matches!(t, Token::Regex(_));
                    Ok((
                        vec![Step::Context(n, vec![(t.text().to_string(), is_re)])],
                        &rest0[1..],
                        f,
                    ))
                }
                _ => Ok((vec![Step::Context(n, Vec::new())], rest0, f)),
            }
        }

        "sort" => {
            let Some(t) = toks.first() else {
                return perr("gitq: 'sort' requires a field name");
            };
            let (name, desc) = match t {
                Token::NegField(n) => (n.clone(), true),
                other => (other.text().to_string(), false),
            };
            if f.contains(&name) {
                Ok((vec![Step::Sort(name, desc)], &toks[1..], f))
            } else {
                perr(format!(
                    "gitq: field '{name}' not valid here after 'sort' (current frame has: {})",
                    list(&f)
                ))
            }
        }

        other => perr(format!("gitq: unknown step keyword '{other}'")),
    }
}

/// Parse a via-step morphism path (one token, possibly composing several
/// morphisms).  When the final morphism is `diff`, its optional REF
/// argument is consumed from the following token, unless that token is a
/// stage boundary or another morphism path.
fn parse_via(toks: &[Token]) -> P<(Vec<Morphism>, &[Token])> {
    let Some(path_tok) = toks.first() else {
        return perr("gitq: 'via' requires a morphism");
    };
    let mut morphs = parse_morphism_path(path_tok.text()).map_err(ParseError)?;
    let rest = &toks[1..];

    if matches!(morphs.last(), Some(Morphism::Diff(None))) {
        if let Some(r) = rest.first() {
            if !r.is_boundary() && !r.text().starts_with('.') {
                let n = morphs.len();
                morphs[n - 1] = Morphism::Diff(Some(r.display()));
                return Ok((morphs, &rest[1..]));
            }
        }
    }
    Ok((morphs, rest))
}

/// Parse a non-negative integer count for skip, erroring on tokens
/// like `5x` rather than silently truncating them.
fn parse_count<'a>(toks: &'a [Token], step_name: &str) -> P<(usize, &'a [Token])> {
    let bad = |got: String| -> P<(usize, &'a [Token])> {
        perr(format!("gitq: '{step_name}' requires a number, got {got}"))
    };
    match toks.first() {
        Some(t) => {
            let s = t.text();
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                match s.parse::<usize>() {
                    Ok(n) => Ok((n, &toks[1..])),
                    Err(_) => bad(format!("'{s}'")),
                }
            } else {
                bad(format!("'{}'", t.display()))
            }
        }
        None => bad("end of input".to_string()),
    }
}

/// Parse a where-condition's raw value token into its runtime value.
pub fn parse_where_value(tok: &Token) -> Value {
    match tok {
        // Quoting is the escape hatch: a quoted value is always a string,
        // never coerced to a number.
        Token::Quoted(s) | Token::Regex(s) => Value::Str(s.as_str().into()),
        // A digit run too long for i64 stays a string rather than
        // saturating or wrapping to a number the user never wrote.
        Token::Word(s)
            if !s.is_empty()
                && s.chars().all(|c| c.is_ascii_digit())
                && s.parse::<i64>().is_ok() =>
        {
            Value::Num(s.parse().unwrap())
        }
        // Prefixed forms keep their prefix in value position: `-foo` is the
        // string "-foo", not the field `foo`.
        other => Value::Str(other.display().as_str().into()),
    }
}

// --- where ---------------------------------------------------------------

/// Parse where-conditions.  Step keywords and /terminals act as stage
/// boundaries and are never consumed as condition values.  Fields must be
/// members of the current field-set.  There is no explicit `contains`
/// keyword: a token right after a field that isn't a recognized operator is
/// taken directly as the value with an implicit substring condition, for
/// field types where that's sensible.
fn parse_where<'a>(toks: &'a [Token], fields: &[String]) -> P<(Vec<Cond>, &'a [Token])> {
    if let Some(t) = toks.first() {
        if !is_field(Some(t), fields) && !t.is_boundary() {
            return perr(format!(
                "gitq: field '{}' not valid here after 'where' (current frame has: {})",
                t.display(),
                list(fields)
            ));
        }
    }

    let mut acc: Vec<Cond> = Vec::new();
    let mut rest = toks;

    loop {
        // A condition starts only where a field name does — but `path` is
        // both a field and a step keyword, so an un-separated `... path
        // GLOB` after a condition is read as the STEP, not as another
        // condition substring-matching the literal glob.  0.7.0 resolved
        // this the other way and `via diff.lines where content x path
        // "*.txt"` silently returned nothing.  A comma still forces the
        // field reading (`where content x, path == "y"`), so nothing that
        // worked before stops working.
        if !is_field(rest.first(), fields) || is_step_kw(rest.first()) {
            return Ok((acc, rest));
        }
        let field_tok = rest[0].text().to_string();
        let (cond, after) = parse_condition(&field_tok, &rest[1..], fields)?;
        acc.push(cond);
        rest = after;

        // a comma demands another field
        if matches!(rest.first(), Some(Token::Comma)) {
            match rest.get(1) {
                Some(t) if is_field(Some(t), fields) => {
                    rest = &rest[1..];
                }
                Some(t) => {
                    return perr(format!(
                        "gitq: expected a field name after ',' in 'where', got '{}'",
                        t.display()
                    ))
                }
                None => {
                    return perr(
                        "gitq: expected a field name after ',' in 'where', got 'end of input'",
                    )
                }
            }
        }
    }
}

/// True where a condition clause ends: end of input, a comma, another
/// field, or a /terminal.
fn clause_ends(t: Option<&Token>, fields: &[String]) -> bool {
    t.is_none() || matches!(t, Some(Token::Comma)) || is_field(t, fields) || is_terminal(t)
}

/// A clock token: `10:10` or `10:10:24`.
fn is_clock(t: &Token) -> bool {
    let Token::Word(w) = t else { return false };
    let parts: Vec<&str> = w.split(':').collect();
    (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| (1..=2).contains(&p.len()) && p.chars().all(|c| c.is_ascii_digit()))
}

/// A UTC offset token: `+0100`, `-0500`.
fn is_offset(t: &Token) -> bool {
    let Token::Word(w) = t else { return false };
    w.len() == 5
        && matches!(w.as_bytes()[0], b'+' | b'-')
        && w[1..].chars().all(|c| c.is_ascii_digit())
}

/// Absorb a clock (and its UTC offset) that follows a date value.
///
/// git prints dates as `2026-07-25 10:10:24 +0100`, so that is the shape
/// people paste back in — but whitespace makes it three tokens, and only the
/// first reached the condition.  Demanding quotes for the one format the tool
/// itself emits is a poor trade, so on a date field the pieces are rejoined.
///
/// Scoped to date fields and to tokens actually shaped like a clock, so no
/// other field can swallow the step that follows it.
fn absorb_clock(ft: FieldType, val: Value, rest: &[Token]) -> (Value, &[Token]) {
    if ft != FieldType::Date {
        return (val, rest);
    }
    let Value::Str(s) = &val else {
        return (val, rest);
    };
    if !rest.first().is_some_and(is_clock) {
        return (val, rest);
    }
    let mut text = format!("{} {}", s.as_ref(), rest[0].text());
    let mut r = &rest[1..];
    if r.first().is_some_and(is_offset) {
        text.push(' ');
        text.push_str(r[0].text());
        r = &r[1..];
    }
    (Value::Str(text.as_str().into()), r)
}

fn parse_condition<'a>(
    field_tok: &str,
    rest: &'a [Token],
    fields: &[String],
) -> P<(Cond, &'a [Token])> {
    let ft = field_type(field_tok);
    let next = rest.first();

    // Bare flag.
    if clause_ends(next, fields) {
        return if ft == FieldType::Flag {
            Ok((
                Cond {
                    field: field_tok.to_string(),
                    op: Op::Is,
                    value: Value::Bool(true),
                },
                rest,
            ))
        } else {
            perr(format!(
                "gitq: bare 'where {field_tok}' tests a flag, but '{field_tok}' is a {} field (add an operator and value)",
                ftype_name(ft)
            ))
        };
    }

    // Step keyword next: ends a bare flag cleanly; for any other field type
    // it's an unquoted value that needed quotes.
    if is_step_kw(next) {
        let n = next.unwrap().text();
        return if ft == FieldType::Flag {
            Ok((
                Cond {
                    field: field_tok.to_string(),
                    op: Op::Is,
                    value: Value::Bool(true),
                },
                rest,
            ))
        } else {
            perr(format!(
                "gitq: 'where {field_tok}' requires a value; step keyword '{n}' must be quoted: \"{n}\""
            ))
        };
    }

    let next = next.unwrap();

    // Recognized operator keyword.  Word-shaped operators (regex, after,
    // before, within, is) arrive as Word tokens; punctuation ones as
    // Operator tokens.  Only an unquoted token can be an operator — `where
    // message "is"` is a value.
    let op_name = match next {
        Token::Operator(s) => Some(s.as_str()),
        Token::Word(s) => Some(s.as_str()),
        _ => None,
    };
    if let Some(op_tok) = op_name {
        if let Some(sig) = operator_signature(op_tok) {
            if !sig.contains(&ft) {
                return perr(format!(
                    "gitq: operator '{op_tok}' does not apply to '{field_tok}' (a {} field; try: {})",
                    ftype_name(ft),
                    ops_for(ft)
                ));
            }
            let rest1 = &rest[1..];
            let next2 = rest1.first();

            if is_step_kw(next2) {
                let n2 = next2.unwrap().text();
                return perr(format!(
                    "gitq: '{op_tok}' requires a value; step keyword '{n2}' must be quoted: \"{n2}\""
                ));
            }

            // No value after the operator: only `is` works valueless.
            if clause_ends(next2, fields) {
                return if op_tok == "is" {
                    Ok((
                        Cond {
                            field: field_tok.to_string(),
                            op: Op::Is,
                            value: Value::Bool(true),
                        },
                        rest1,
                    ))
                } else {
                    let got = match next2 {
                        Some(t) => format!("'{}'", t.display()),
                        None => "end of input".to_string(),
                    };
                    perr(format!(
                        "gitq: operator '{op_tok}' requires a value, got {got}"
                    ))
                };
            }

            let val_tok = next2.unwrap();
            let mut val = parse_where_value(val_tok);
            // `is` takes a boolean.  0.7.0 compared the flag's Bool against
            // the literal STRING "true", so `where modified is true` — the
            // most natural spelling — silently matched nothing while the
            // bare `where modified` worked.
            if op_tok == "is" {
                match val_tok.text() {
                    "true" => val = Value::Bool(true),
                    "false" => val = Value::Bool(false),
                    _ => {}
                }
            }
            if ft == FieldType::Number && !matches!(val, Value::Num(_)) {
                return perr(format!(
                    "gitq: '{field_tok}' is a number field; '{}' is not a number",
                    val_tok.display()
                ));
            }
            let op = op_from_name(op_tok)?;
            // A pattern that can't compile must fail here, not when the
            // executor matches the first frame.
            if op == Op::Regex {
                if let Value::Str(pat) = &val {
                    if let Err(e) = regex::Regex::new(pat) {
                        let flat = e
                            .to_string()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        return perr(format!("gitq: invalid regex '{pat}': {flat}"));
                    }
                }
            }
            let (val, after) = absorb_clock(ft, val, &rest1[1..]);
            return Ok((
                Cond {
                    field: field_tok.to_string(),
                    op,
                    value: val,
                },
                after,
            ));
        }
    }

    // Implicit operator: the token is the value directly (substring match
    // for text-shaped fields, equality for numbers).
    if let Some(iop) = implicit_op(ft) {
        let raw = parse_where_value(next);
        // An all-digit token on a text-shaped field is a substring, not a
        // number: `where sha 95866` must match, rather than silently
        // comparing a number against a string forever.
        let val = match (iop, &raw) {
            (Op::Contains, Value::Num(_)) => Value::Str(next.display().as_str().into()),
            _ => raw,
        };
        if iop == Op::Eq && !matches!(val, Value::Num(_)) {
            return perr(format!(
                "gitq: '{field_tok}' is a number field; '{}' is not a number",
                next.display()
            ));
        }
        let (val, after) = absorb_clock(ft, val, &rest[1..]);
        return Ok((
            Cond {
                field: field_tok.to_string(),
                op: iop,
                value: val,
            },
            after,
        ));
    }

    // Flag field with an unrecognized operator token.
    perr(format!(
        "gitq: unknown where operator '{}' (expected one of: {})",
        next.display(),
        OPERATOR_NAMES.join(", ")
    ))
}

fn op_from_name(n: &str) -> P<Op> {
    Ok(match n {
        "==" => Op::Eq,
        "!=" => Op::Ne,
        ">" => Op::Gt,
        "<" => Op::Lt,
        ">=" => Op::Ge,
        "<=" => Op::Le,
        "regex" => Op::Regex,
        "after" => Op::After,
        "before" => Op::Before,
        "within" => Op::Within,
        "is" => Op::Is,
        _ => return perr(format!("gitq: unknown where operator '{n}'")),
    })
}

// --- context resolution --------------------------------------------------

/// Resolve a patternless `context N` against the steps before it: it centers
/// on whatever the pipeline already searched for — `where content` values,
/// `grep`, and `pickaxe` patterns.  Nothing to center on is a parse error,
/// not an empty result.
fn resolve_contexts(steps: Vec<Step>) -> P<Vec<Step>> {
    let mut out: Vec<Step> = Vec::with_capacity(steps.len());
    for st in steps {
        match st {
            Step::Context(n, pats) if pats.is_empty() => {
                let inherited: Vec<(String, bool)> = out.iter().flat_map(search_patterns).collect();
                if inherited.is_empty() {
                    return perr(
                        "gitq: 'context' has no pattern to center on — give one (context 3 \"term\") or precede it with a content filter, grep, or pickaxe",
                    );
                }
                out.push(Step::Context(n, inherited));
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn search_patterns(st: &Step) -> Vec<(String, bool)> {
    match st {
        Step::Where(conds) => conds
            .iter()
            .filter_map(|c| {
                if c.field != "content" {
                    return None;
                }
                let Value::Str(v) = &c.value else { return None };
                let is_re = match c.op {
                    Op::Regex => true,
                    Op::Contains | Op::Eq => false,
                    _ => return None,
                };
                Some((v.to_string(), is_re))
            })
            .collect(),
        Step::Grep(p, re) | Step::Pickaxe(p, re) => vec![(p.clone(), *re)],
        _ => Vec::new(),
    }
}

// --- terminals -----------------------------------------------------------

/// Signal an error if tokens remain, naming the context (a terminal
/// keyword).  A terminal always ends the pipeline, so leftovers almost
/// always mean a multi-word value that needed double-quotes.
fn expect_no_more(toks: &[Token], ctx: &str) -> P<()> {
    match toks.first() {
        None => Ok(()),
        Some(t) => perr(format!(
            "gitq: unexpected token '{}' after '{ctx}' (missing quotes around a value?)",
            t.display()
        )),
    }
}

fn opt_quoted(toks: &[Token]) -> (Option<String>, &[Token]) {
    match toks.first() {
        Some(Token::Quoted(s)) => (Some(s.clone()), &toks[1..]),
        _ => (None, toks),
    }
}

/// Parse a terminal by name (leading `/` already stripped) with its
/// remaining tokens.  Every parser consumes all tokens it is given and
/// errors on leftovers — a terminal always ends the pipeline.
pub fn parse_terminal(kw: &str, toks: &[Token]) -> P<Terminal> {
    let simple = |t: Terminal| -> P<Terminal> {
        expect_no_more(toks, kw)?;
        Ok(t)
    };
    let optional_msg = |mk: fn(Option<String>) -> Terminal| -> P<Terminal> {
        let (msg, rest) = opt_quoted(toks);
        expect_no_more(rest, kw)?;
        Ok(mk(msg))
    };

    match kw {
        "show" => simple(Terminal::Show),
        "copy" => simple(Terminal::Copy),
        "insert" => simple(Terminal::Insert),
        "count" => simple(Terminal::Count),
        // /delete is a true alias of /remove: it parses to the same op, so
        // it can never parse successfully and then fall through to a silent
        // no-op.
        "remove" | "delete" => simple(Terminal::Remove),
        "stage" => simple(Terminal::Stage),
        "branch-off" => {
            let (name, rest1) = opt_quoted(toks);
            let (wt, rest2) = match (rest1.first(), rest1.get(1)) {
                (Some(t), Some(p)) if t.text() == "worktree" => {
                    (Some(p.text().to_string()), &rest1[2..])
                }
                _ => (None, rest1),
            };
            expect_no_more(rest2, kw)?;
            Ok(Terminal::BranchOff(name, wt))
        }
        "amend" => match toks.first() {
            Some(t) if t.text() == "no-edit" => {
                expect_no_more(&toks[1..], kw)?;
                Ok(Terminal::Amend(true, None))
            }
            Some(Token::Quoted(s)) => {
                expect_no_more(&toks[1..], kw)?;
                Ok(Terminal::Amend(false, Some(s.clone())))
            }
            _ => {
                expect_no_more(toks, kw)?;
                Ok(Terminal::Amend(false, None))
            }
        },
        "squash" => optional_msg(Terminal::Squash),
        "reword" => optional_msg(Terminal::Reword),
        "commit" => optional_msg(Terminal::Commit),
        "mark" => match toks.first() {
            Some(t) => {
                expect_no_more(&toks[1..], kw)?;
                Ok(Terminal::Mark(Some(t.text().to_string())))
            }
            None => Ok(Terminal::Mark(None)),
        },
        "worktree" => {
            let (path, rest) = opt_quoted(toks);
            expect_no_more(rest, kw)?;
            Ok(Terminal::Worktree(path))
        }
        _ => perr(format!(
            "gitq: unknown terminal operation '{kw}' (expected one of: {})",
            TERMINAL_NAMES.join(", ")
        )),
    }
}

// --- completion support --------------------------------------------------

/// The field-set active after the fully-typed tokens of a (possibly
/// incomplete) pipeline prefix.  Replays the real parser stage by stage so
/// completion and the strict parser can never drift apart; the first stage
/// that can't be parsed from what's typed so far just stops the walk,
/// returning the last successfully-computed field-set.
pub fn infer_fields(ctx: &[Token]) -> Vec<String> {
    let Ok((src, rest)) = parse_source(ctx) else {
        return owned(COMMIT_FIELDS);
    };
    let mut fields = owned(source_fields(&src));
    let mut toks = rest;
    loop {
        match toks.first() {
            None => return fields,
            Some(Token::Terminal(_)) => return fields,
            Some(Token::Step(_)) => match parse_step(toks, &fields) {
                Ok((_, rest, new_fields)) => {
                    toks = rest;
                    fields = new_fields;
                }
                Err(_) => return fields,
            },
            _ => return fields,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(q: &str) -> P<Pipeline> {
        parse_pipeline(q)
    }

    fn ok(q: &str) -> Pipeline {
        p(q).unwrap_or_else(|e| panic!("{q:?} should parse, got: {e}"))
    }

    /// Assert a query fails with a message containing `needle`.  The
    /// fail-loud catalogue is asserted by substring, exactly as the Haskell
    /// suite did.
    fn err(q: &str, needle: &str) {
        match p(q) {
            Ok(_) => panic!("{q:?} should have failed with {needle:?}, but parsed"),
            Err(e) => assert!(
                e.0.contains(needle),
                "{q:?}\n  expected substring: {needle:?}\n  actual message:    {}",
                e.0
            ),
        }
    }

    // --- sources ---------------------------------------------------------

    #[test]
    fn sources_parse() {
        assert_eq!(ok("commits").source, Source::Commits(None));
        assert_eq!(ok("branches").source, Source::Branches);
        assert_eq!(ok("tags").source, Source::Tags);
        assert_eq!(ok("refs").source, Source::Refs);
        assert_eq!(ok("worktrees").source, Source::Worktrees);
        assert_eq!(ok("blobs").source, Source::Blobs);
        assert_eq!(ok("HEAD").source, Source::Ref("HEAD".into()));
        assert_eq!(ok("my-branch").source, Source::Ref("my-branch".into()));
    }

    #[test]
    fn source_level_range() {
        assert_eq!(
            ok("commits in main..HEAD").source,
            Source::Commits(Some("main..HEAD".into()))
        );
    }

    // --- morphism chains typecheck --------------------------------------

    #[test]
    fn via_chains_thread_the_field_set() {
        assert_eq!(
            ok("HEAD via parent").steps,
            vec![Step::Via(Morphism::Parent)]
        );
        assert_eq!(
            ok("HEAD via parent.tree").steps,
            vec![Step::Via(Morphism::Parent), Step::Via(Morphism::Tree)]
        );
    }

    #[test]
    fn via_rejects_a_morphism_the_shape_cannot_feed() {
        // ref frames carry sha/name/reftype — no parents-count
        err("branches via parent", "needs a 'parents-count' field");
        // tree entries are blobs; they have no parents-count either
        err("commits via tree.entries via parent", "needs a");
    }

    #[test]
    fn unknown_morphisms_fail_loudly() {
        err("commits via nosuchmorphism", "unknown morphism");
        err("commits via parent.nosuch", "unknown morphism");
    }

    #[test]
    fn diff_takes_an_optional_ref_argument() {
        assert_eq!(
            ok("HEAD via diff main").steps,
            vec![Step::Via(Morphism::Diff(Some("main".into())))]
        );
        // a boundary must NOT be eaten as the ref
        assert_eq!(
            ok("HEAD via diff /count").steps,
            vec![Step::Via(Morphism::Diff(None))]
        );
    }

    #[test]
    fn standalone_hunks_composes_with_diff() {
        // the gap fix: `diff.hunks` had no factors before
        assert_eq!(
            ok("HEAD via diff via hunks").steps,
            vec![Step::Via(Morphism::Diff(None)), Step::Via(Morphism::Hunks)]
        );
        // and it must not typecheck on a shape that merely has `path`
        err("blobs via hunks", "needs a 'parent-sha' field");
    }

    // --- where -----------------------------------------------------------

    #[test]
    fn implicit_operators_by_field_type() {
        let s = &ok("commits where author alice").steps[0];
        assert_eq!(
            s,
            &Step::Where(vec![Cond {
                field: "author".into(),
                op: Op::Contains,
                value: "alice".into()
            }])
        );
        // numbers get equality, not substring
        let s = &ok("commits where parents-count 2").steps[0];
        assert_eq!(
            s,
            &Step::Where(vec![Cond {
                field: "parents-count".into(),
                op: Op::Eq,
                value: Value::Num(2)
            }])
        );
    }

    #[test]
    fn all_digit_values_on_text_fields_stay_strings() {
        // 0.7.0's inherited elisp bug: `where sha 95866` parsed 95866 as a
        // number, so the implicit substring match compared a number to a
        // string and was silently always false.
        let Step::Where(conds) = &ok("commits where sha 95866").steps[0] else {
            panic!()
        };
        assert_eq!(conds[0].value, "95866".into());
        assert_eq!(conds[0].op, Op::Contains);
    }

    #[test]
    fn bare_flag_conditions() {
        let Step::Where(conds) = &ok("worktrees where modified").steps[0] else {
            panic!()
        };
        assert_eq!(conds[0].op, Op::Is);
        assert_eq!(conds[0].value, Value::Bool(true));
    }

    #[test]
    fn bare_non_flag_field_is_an_error() {
        err("commits where author", "tests a flag");
        err("commits where message", "is a string field");
    }

    #[test]
    fn multiple_conditions_need_a_field_after_the_comma() {
        assert!(matches!(
            &ok("commits where author alice, parents-count 1").steps[0],
            Step::Where(c) if c.len() == 2
        ));
        err(
            "commits where author alice,",
            "expected a field name after ','",
        );
    }

    #[test]
    fn operator_signatures_are_enforced() {
        err("commits where author >= alice", "does not apply");
        err("commits where parents-count regex 2", "does not apply");
        err("commits where date is true", "does not apply");
    }

    #[test]
    fn unknown_fields_and_operators() {
        err(
            "commits where nosuchfield alice",
            "not valid here after 'where'",
        );
        // a field absent from the SHAPE is caught first, before its type is
        // ever consulted — `modified` is not in commitFields
        err(
            "commits where modified alice",
            "not valid here after 'where'",
        );
        // to reach the unknown-operator branch the flag must be in scope
        err("worktrees where modified alice", "unknown where operator");
    }

    #[test]
    fn step_keywords_in_value_position_demand_quotes() {
        err("commits where message sort", "must be quoted");
        err("commits where message == sort", "must be quoted");
    }

    #[test]
    fn invalid_regex_fails_at_parse_time_not_on_first_frame() {
        err("commits where message regex \"[\"", "invalid regex");
    }

    #[test]
    fn empty_where_is_an_error() {
        // Gap fix.  0.7.0 accepted this as a keep-everything no-op — the
        // only step that did — and returned every commit with exit 0.
        err("commits where", "'where' requires at least one condition");
        err(
            "commits where /count",
            "'where' requires at least one condition",
        );
    }

    // --- relational steps ------------------------------------------------

    #[test]
    fn counts_reject_non_numbers() {
        // take/skip/first/last are gone: positional selection replaced them
        for old in [
            "commits take 3",
            "commits skip 3",
            "commits first",
            "commits last",
        ] {
            err(old, "expected step keyword");
        }
    }

    // --- positional selection, and its ambiguity with regex --------------

    #[test]
    fn selection_parses_as_a_slice_step() {
        use crate::ast::Sel;
        assert_eq!(
            ok("commits [0..3]").steps,
            vec![Step::Slice(vec![Sel::Range {
                start: Some(0),
                stop: Some(3),
                step: None
            }])]
        );
        assert_eq!(
            ok("commits [-1]").steps,
            vec![Step::Slice(vec![Sel::Index(-1)])]
        );
    }

    #[test]
    fn a_comma_inside_brackets_separates_selectors_not_list_items() {
        use crate::ast::Sel;
        // the shape a multi-row selection emits; a bare comma would have
        // ended the token and left `[0` dangling
        assert_eq!(
            ok("commits [0..2,4]").steps,
            vec![Step::Slice(vec![
                Sel::Range {
                    start: Some(0),
                    stop: Some(2),
                    step: None
                },
                Sel::Index(4),
            ])]
        );
    }

    #[test]
    fn brackets_after_an_unquoted_regex_are_selection_not_regex() {
        // the disambiguation rule: whitespace ends the value, so a bracket
        // token standing on its own is positional selection
        let p = ok("commits where sha regex ^5 [0..1]");
        assert_eq!(p.steps.len(), 2);
        assert!(matches!(p.steps[1], Step::Slice(_)));
    }

    #[test]
    fn brackets_inside_a_quoted_value_belong_to_the_value() {
        // quoting is what makes it literal — no slice step is produced
        let p = ok(r#"commits where sha regex "^[45]""#);
        assert_eq!(p.steps.len(), 1);
        assert!(!matches!(p.steps[0], Step::Slice(_)));
    }

    #[test]
    fn brackets_touching_a_value_are_part_of_it_a_regex_character_class() {
        // no whitespace => one token => a regex character class, which is
        // what `regex ^5[0-9a-f]` has always meant and must keep meaning
        let p = ok("commits where sha regex ^5[0-9a-f]");
        assert_eq!(p.steps.len(), 1);
        assert!(!matches!(p.steps[0], Step::Slice(_)));
    }

    #[test]
    fn a_morphism_index_is_not_read_as_selection() {
        // brackets inside a `via` path index a morphism; they are consumed
        // by the path, never by the step dispatcher
        let p = ok("HEAD via parent[0]");
        assert!(!p.steps.iter().any(|s| matches!(s, Step::Slice(_))));
        let p = ok("HEAD via tree.entries[Blob]");
        assert!(!p.steps.iter().any(|s| matches!(s, Step::Slice(_))));
    }

    #[test]
    fn a_date_absorbs_the_clock_that_follows_it() {
        // git prints `2026-07-25 10:10:24 +0100`; pasting it back must work
        let p = ok("commits where date 2026-07-25 10:10:24");
        match &p.steps[0] {
            Step::Where(cs) => assert_eq!(
                cs[0].value,
                Value::Str("2026-07-25 10:10:24".into()),
                "the clock was dropped or left as a separate token"
            ),
            other => panic!("expected a where step, got {other:?}"),
        }
    }

    #[test]
    fn a_date_absorbs_the_utc_offset_too() {
        let p = ok("commits where date 2026-07-25 10:10:24 +0100");
        match &p.steps[0] {
            Step::Where(cs) => {
                assert_eq!(cs[0].value, Value::Str("2026-07-25 10:10:24 +0100".into()))
            }
            other => panic!("expected a where step, got {other:?}"),
        }
    }

    #[test]
    fn absorbing_a_clock_does_not_swallow_the_step_after_it() {
        let p = ok("commits where date 2026-07-25 10:10:24 [0..1]");
        assert_eq!(p.steps.len(), 2);
        assert!(matches!(p.steps[1], Step::Slice(_)));
    }

    #[test]
    fn only_clock_shaped_tokens_are_absorbed_by_a_date() {
        // a following step must still be a step
        let p = ok("commits where date 2026-07-25 sort -date");
        assert_eq!(p.steps.len(), 2);
        assert!(matches!(p.steps[1], Step::Sort(_, _)));
    }

    #[test]
    fn only_date_fields_absorb_a_clock() {
        // scoped deliberately: on a string field the clock is still a stray
        // token, and saying so beats silently gluing it onto the value
        err(
            "commits where message fix 10:10:24",
            "expected step keyword",
        );
    }

    #[test]
    fn an_unquoted_value_with_spaces_is_told_to_quote_it() {
        // the tokenizer splits on whitespace, so only `fix` reaches the
        // condition and the rest lands where a step should be.  (A *date*
        // field rejoins its clock; every other field still needs quotes.)
        err(
            "commits where message fix 10:10:24",
            "quote the whole value",
        );
    }

    #[test]
    fn a_plain_mistyped_keyword_does_not_get_the_quoting_hint() {
        let e = parse_pipeline("commits nonsense").unwrap_err().to_string();
        assert!(e.contains("expected step keyword"), "{e}");
        assert!(!e.contains("quote the whole value"), "noisy hint: {e}");
    }

    #[test]
    fn revspec_punctuation_survives_tokenizing() {
        // HEAD^, HEAD^2, HEAD^! must reach git whole rather than being split
        // at the caret
        for r in ["HEAD^..HEAD", "HEAD^^..HEAD", "HEAD~2^..HEAD", "HEAD^!"] {
            let p = ok(&format!("commits in {r}"));
            match &p.source {
                Source::Commits(Some(got)) => assert_eq!(got, r),
                other => panic!("expected a ranged commits source, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_multi_rev_range_keeps_its_spaces() {
        // `main ^v0.6.0` is two arguments to git; joining them without a
        // separator asked git for one revision named `main^v0.6.0`
        let p = ok("commits in main ^v0.6.0");
        match &p.source {
            Source::Commits(Some(got)) => assert_eq!(got, "main ^v0.6.0"),
            other => panic!("expected a ranged commits source, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_in_with_no_range_is_an_error() {
        // it used to fall through to an empty range, which ran as plain
        // `git log` and answered "all commits" to a query that named none
        err("commits in", "requires a revision range");
        err("commits in /count", "requires a revision range");
    }

    #[test]
    fn a_dash_range_is_rejected_with_a_message_naming_the_dotted_form() {
        err("commits [20-30]", "'..' not '-'");
    }

    #[test]
    fn malformed_selections_fail_loud() {
        err("commits []", "empty selection");
        err("commits [....0]", "step cannot be 0");
        err("commits [1..2..3..4]", "start..stop..step");
        err("commits [abc]", "not a number");
    }

    #[test]
    fn sort_accepts_descending_and_checks_the_field() {
        assert_eq!(
            ok("commits sort date").steps,
            vec![Step::Sort("date".into(), false)]
        );
        assert_eq!(
            ok("commits sort -date").steps,
            vec![Step::Sort("date".into(), true)]
        );
        err("commits sort", "requires a field name");
        err("commits sort nosuch", "not valid here after 'sort'");
    }

    #[test]
    fn pick_reads_path_as_a_field_not_a_step_keyword() {
        // `path` is in both registries; inside `pick` the field wins
        let pipe = ok("commits via diff pick path,sha");
        assert_eq!(
            pipe.steps.last(),
            Some(&Step::Pick(vec!["path".into(), "sha".into()]))
        );
        err("commits pick", "requires at least one field name");
        err("commits pick nosuch", "requires at least one field name");
    }

    #[test]
    fn pick_narrows_the_field_set_for_later_steps() {
        // after `pick sha`, `sort author` must no longer typecheck
        err(
            "commits pick sha sort author",
            "not valid here after 'sort'",
        );
    }

    #[test]
    fn path_step_needs_a_path_carrying_shape() {
        err("commits path a.txt", "needs a 'path' field");
        assert!(p("commits via diff path \"*.txt\"").is_ok());
    }

    #[test]
    fn grep_and_pickaxe_need_a_sha() {
        assert!(p("commits grep needle").is_ok());
        err("commits grep", "requires a pattern");
        // regex literal form sets the flag
        assert_eq!(
            ok("commits pickaxe /need.*/").steps,
            vec![Step::Pickaxe("need.*".into(), true)]
        );
        // and so does the trailing keyword form
        assert_eq!(
            ok("commits pickaxe needle regex").steps,
            vec![Step::Pickaxe("needle".into(), true)]
        );
    }

    #[test]
    fn context_inherits_patterns_from_preceding_searches() {
        let pipe = ok("commits grep needle context 2");
        assert_eq!(
            pipe.steps.last(),
            Some(&Step::Context(2, vec![("needle".into(), false)]))
        );
        // an explicit pattern wins
        let pipe = ok("commits grep needle context 2 other");
        assert_eq!(
            pipe.steps.last(),
            Some(&Step::Context(2, vec![("other".into(), false)]))
        );
    }

    #[test]
    fn patternless_context_with_nothing_to_inherit_is_an_error() {
        err(
            "commits via diff.lines context 2",
            "no pattern to center on",
        );
    }

    #[test]
    fn mid_pipeline_in_needs_a_commit_identifying_field() {
        assert!(p("commits in v1..HEAD").is_ok());
        // every source shape carries `sha` or `commit-sha`, so the only way
        // to lose it is to project it away
        err(
            "commits pick author in v1..HEAD",
            "needs a 'sha' or 'commit-sha' field",
        );
    }

    #[test]
    fn revspec_vocabulary_survives_into_the_range() {
        // the tokenizer classifies --not and ^rev; the range must be
        // reassembled with them intact, since git parses the string
        let Step::InRange(r) = &ok("commits via parent in HEAD --not v1").steps[1] else {
            panic!("expected InRange")
        };
        assert_eq!(r, "HEAD --not v1");
        let Step::InRange(r) = &ok("commits via parent in HEAD ^v1").steps[1] else {
            panic!("expected InRange")
        };
        assert_eq!(r, "HEAD ^v1");
    }

    // --- terminals -------------------------------------------------------

    #[test]
    fn terminals_parse() {
        assert_eq!(ok("commits /show").terminal, Some(Terminal::Show));
        assert_eq!(ok("commits /count").terminal, Some(Terminal::Count));
        // /delete is a true alias, not a separate op that could no-op
        assert_eq!(ok("commits /delete").terminal, Some(Terminal::Remove));
        assert_eq!(ok("commits /remove").terminal, Some(Terminal::Remove));
    }

    #[test]
    fn terminal_arguments() {
        assert_eq!(
            ok("commits /branch-off \"feat\"").terminal,
            Some(Terminal::BranchOff(Some("feat".into()), None))
        );
        assert_eq!(
            ok("commits /amend no-edit").terminal,
            Some(Terminal::Amend(true, None))
        );
        assert_eq!(
            ok("commits /commit \"msg\"").terminal,
            Some(Terminal::Commit(Some("msg".into())))
        );
    }

    #[test]
    fn terminals_consume_everything_or_error() {
        err(
            "commits /show extra",
            "unexpected token 'extra' after 'show'",
        );
        err("commits /nosuchterminal", "unknown terminal operation");
    }

    // --- top level -------------------------------------------------------

    #[test]
    fn empty_and_garbage_pipelines() {
        err("", "empty pipeline");
        err("commits nonsense", "expected step keyword or /terminal");
    }

    #[test]
    fn the_at_sign_query_now_means_what_it_says() {
        // 0.7.0 dropped the '@' here and searched for "lice" instead
        let Step::Where(conds) = &ok("commits where author @lice").steps[0] else {
            panic!()
        };
        assert_eq!(conds[0].value, "@lice".into());
    }

    #[test]
    fn bare_plus_is_usable_as_a_sign_value() {
        // needed quoting in 0.7.0, because a bare '+' was silently dropped
        let Step::Where(conds) = &ok("commits via diff.lines where sign +").steps[1] else {
            panic!()
        };
        assert_eq!(conds[0].value, "+".into());
    }

    // --- completion support ----------------------------------------------

    #[test]
    fn infer_fields_replays_the_parser() {
        let f = |q: &str| infer_fields(&tokenize(q).unwrap());
        assert!(f("commits").contains(&"parents-count".to_string()));
        assert!(f("commits via diff").contains(&"parent-sha".to_string()));
        assert!(f("commits via diff.lines").contains(&"sign".to_string()));
        // pick narrows
        assert_eq!(f("commits pick sha"), vec!["sha".to_string()]);
        // an unparseable tail stops the walk without erroring
        assert!(f("commits where ").contains(&"author".to_string()));
    }
}
