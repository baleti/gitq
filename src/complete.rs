//! Context-aware completion: candidates for the token being typed, derived
//! from the same registries and (via [`infer_fields`]) the same parser the
//! strict pipeline parser uses — so completion can never offer something
//! the parser would then reject.

use std::collections::HashSet;

use crate::git::run_git;
use crate::parse::infer_fields;
use crate::registry::*;
use crate::tokenize::{tokenize, Token};

/// Candidates for the pipeline string typed so far.  Completions extend the
/// last partial word; the caller (zsh, Emacs) filters against it.
pub fn complete_candidates(input: &str) -> Vec<String> {
    let trimmed = input.trim_end();
    let trailing = trimmed != input;
    // An unusable character mid-typing yields no candidates rather than an
    // error: completion runs on every keystroke.
    let Ok(tokens) = tokenize(trimmed) else {
        return Vec::new();
    };

    // Fully-typed tokens; the final partial word is what's being completed.
    let ctx: &[Token] = if trailing {
        &tokens
    } else {
        &tokens[..tokens.len().saturating_sub(1)]
    };
    let n = ctx.len();
    let last = ctx.last();
    let prev = if n > 1 { ctx.get(n - 2) } else { None };
    let last_text = last.map(Token::text);
    let last_display = last.map(Token::display);

    // no tokens yet → the sources
    if n == 0 {
        return strs(COMPLETE_SOURCE_KEYWORDS);
    }

    // after "commits" → "in" or steps/terminals
    if n == 1 && last_text == Some("commits") {
        let mut out = vec!["in".to_string()];
        out.extend(strs(STEP_KEYWORDS));
        out.extend(complete_terminals());
        return out;
    }

    // after "in" (source modifier or mid-pipeline step) → ranges, then refs
    if last_text == Some("in") {
        return complete_ranges();
    }

    // after "via" → morphisms valid for the frame type flowing in
    if last_text == Some("via") {
        let fields = infer_fields(&ctx[..n - 1]);
        return COMPLETE_MORPHISMS
            .iter()
            .filter(|m| match head_requires(m) {
                Some(req) => fields.iter().any(|f| f == req),
                None => true,
            })
            .map(|m| m.to_string())
            .collect();
    }

    // after "via diff" → optional REF, or skip ahead
    if matches!(last_text, Some("diff") | Some(".diff")) && prev.map(Token::text) == Some("via") {
        let mut out = complete_refs();
        out.extend(strs(STEP_KEYWORDS));
        out.extend(complete_terminals());
        return out;
    }

    // after "where" or "," → fields of the current frame type (a comma
    // inside pick lands here too, with the same answer)
    if last_text == Some("where") || matches!(last, Some(Token::Comma)) {
        return current_type_fields(ctx);
    }

    // after a field inside a where clause → operators and (for
    // implicit-contains-eligible types) value candidates
    if let Some(field_tok) = last_text {
        let fields = current_type_fields(ctx);
        if enclosing_step(ctx).as_deref() == Some("where") && fields.iter().any(|f| f == field_tok)
        {
            let mut out = strs(COMPLETE_WHERE_OPERATORS);
            if implicit_op(field_type(field_tok)).is_some() {
                out.extend(complete_where_values(field_tok, None));
            }
            return out;
        }
    }

    // after "sort" → fields with optional - prefix
    if last_text == Some("sort") {
        let fields = current_type_fields(ctx);
        let mut out = fields.clone();
        out.extend(fields.iter().map(|f| format!("-{f}")));
        return out;
    }

    // after "pick" → fields flowing into pick
    if last_text == Some("pick") {
        return current_type_fields(ctx);
    }

    // after a where-operator → dynamic values
    if let Some(op) = last_text {
        if COMPLETE_WHERE_OPERATORS.contains(&op) {
            return match prev.map(Token::text) {
                Some(field) => complete_where_values(field, Some(op)),
                None => Vec::new(),
            };
        }
    }

    // after a terminal: only its own optional argument may follow
    if matches!(last, Some(Token::Terminal(_))) {
        return if last_display.as_deref() == Some("/amend") {
            vec!["no-edit".to_string()]
        } else {
            Vec::new()
        };
    }

    // otherwise → steps + terminals (+ "," inside where/pick)
    let mut out = Vec::new();
    if matches!(enclosing_step(ctx).as_deref(), Some("where") | Some("pick")) {
        out.push(",".to_string());
    }
    out.extend(strs(STEP_KEYWORDS));
    out.extend(complete_terminals());
    out
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// The field the head morphism of a completion candidate path requires, via
/// the same path parser and registry the pipeline parser uses.
fn head_requires(path: &str) -> Option<&'static str> {
    parse_morphism_path(path)
        .ok()?
        .first()
        .map(morphism_requires)
}

/// The most recent step keyword in the context, walking in order so that
/// `path` right after `where`/`pick`, a comma, or another field is treated
/// as a field reference continuing that stage, not a fresh `path` step.
fn enclosing_step(ctx: &[Token]) -> Option<String> {
    let mut acc: Option<String> = None;
    for (i, tok) in ctx.iter().enumerate() {
        let t = tok.text();
        if STEP_KEYWORDS.contains(&t) && !field_continuation(ctx, i, t, acc.as_deref()) {
            acc = Some(t.to_string());
        }
    }
    acc
}

fn field_continuation(ctx: &[Token], i: usize, tok: &str, acc: Option<&str>) -> bool {
    if !FIELD_NAMES.contains(&tok) {
        return false;
    }
    if !matches!(acc, Some("where") | Some("pick")) {
        return false;
    }
    let prev = if i > 0 { Some(ctx[i - 1].text()) } else { None };
    matches!(prev, Some("where") | Some("pick") | Some(","))
        || prev.is_some_and(|p| FIELD_NAMES.contains(&p))
}

/// Fields valid to offer as where/sort/pick candidates at the end of the
/// context: the field-set flowing *into* the enclosing stage.
fn current_type_fields(ctx: &[Token]) -> Vec<String> {
    match enclosing_step(ctx) {
        Some(stage) => match ctx.iter().rposition(|t| t.text() == stage) {
            Some(i) => infer_fields(&ctx[..i]),
            None => infer_fields(ctx),
        },
        None => infer_fields(ctx),
    }
}

/// Local branch and tag names, for contexts expecting a ref.
/// Candidates after `in`: `REF..HEAD` ranges first, then the bare refs.
///
/// A bare ref is a legal revspec, but it is the degenerate one — `in main`
/// from a branch merged into main keeps everything and reads as a no-op,
/// which is how `in` gets mistaken for a filter on some `branch` field.  The
/// range is what the step is *for*, so it goes first and is visible without
/// reading the manual.
///
/// Ranges are offered as `REF..HEAD` rather than every REF..REF pairing:
/// pairings are quadratic and a repo with a few hundred refs would bury the
/// list, while `..HEAD` is the overwhelmingly common shape.  Fuzzy filtering
/// plus editing the tail covers the rest.
fn complete_ranges() -> Vec<String> {
    let refs = complete_refs();
    let mut out: Vec<String> = refs
        .iter()
        .filter(|r| *r != "HEAD")
        .map(|r| format!("{r}..HEAD"))
        .collect();
    out.extend(refs);
    out
}

fn complete_refs() -> Vec<String> {
    let mut out = run_git(&["branch", "--format=%(refname:short)"]);
    out.extend(run_git(&["tag", "--list"]));
    out
}

/// Value candidates for a where-condition, or empty for fields with no
/// natural git-derivable value domain.
fn complete_where_values(field: &str, op: Option<&str>) -> Vec<String> {
    match (field, op) {
        ("date", Some("within")) => strs(COMPLETE_DATE_WITHIN_EXAMPLES),
        ("author", _) => dedup(&["log", "--format=%an", "--all"]),
        ("email", _) => dedup(&["log", "--format=%ae", "--all"]),
        ("date", _) => dedup(&["log", "--format=%ai", "--all"]),
        ("sha", _) | ("commit-sha", _) => dedup(&["log", "--format=%h", "--all"]),
        ("path", _) => dedup(&["log", "--all", "--name-only", "--format="]),
        ("name", _) | ("branch", _) => complete_refs(),
        _ => Vec::new(),
    }
}

/// Order-preserving dedup: value candidates can be one line per commit in
/// the repo, so this must not be quadratic.
fn dedup(args: &[&str]) -> Vec<String> {
    let mut seen = HashSet::new();
    run_git(args)
        .into_iter()
        .filter(|x| seen.insert(x.clone()))
        .collect()
}

/// A candidate paired with its registry kind and description, for the
/// `--complete-annotated` protocol the Emacs client consumes.
pub fn annotate(cand: &str) -> (String, &'static str, &'static str) {
    (
        cand.to_string(),
        token_kind(cand).unwrap_or(""),
        describe_token(cand).unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(input: &str) -> Vec<String> {
        complete_candidates(input)
    }

    #[test]
    fn empty_input_offers_sources() {
        assert_eq!(c(""), strs(COMPLETE_SOURCE_KEYWORDS));
    }

    #[test]
    fn in_offers_ranges_before_bare_refs() {
        // the bare ref is the degenerate revspec and reads as a no-op; the
        // range is what the step is for, so it must be the visible default
        let out = c("commits in ");
        if out.is_empty() {
            return; // no refs in this checkout
        }
        assert!(
            out[0].contains(".."),
            "a bare ref led the list: {:?}",
            &out[..out.len().min(3)]
        );
        // and the plain refs are still reachable further down
        assert!(out.iter().any(|r| !r.contains("..")));
    }

    #[test]
    fn after_a_source_comes_in_steps_and_terminals() {
        let out = c("commits ");
        assert!(out.contains(&"in".to_string()));
        assert!(out.contains(&"where".to_string()));
        assert!(out.contains(&"/count".to_string()));
    }

    #[test]
    fn via_offers_only_morphisms_the_shape_can_feed() {
        let out = c("commits via ");
        assert!(out.contains(&"parent".to_string()));
        assert!(out.contains(&"diff.hunks".to_string()));
        // ref frames have no parents-count, so parent must not be offered
        let out = c("branches via ");
        assert!(!out.contains(&"parent".to_string()), "{out:?}");
    }

    #[test]
    fn where_offers_the_current_shapes_fields() {
        let out = c("commits where ");
        assert!(out.contains(&"author".to_string()));
        assert!(!out.contains(&"sign".to_string()));
        // after a morphism the field-set follows
        let out = c("commits via diff.lines where ");
        assert!(out.contains(&"sign".to_string()));
        assert!(!out.contains(&"email".to_string()));
    }

    #[test]
    fn after_a_field_come_the_operators() {
        let out = c("commits where author ");
        for op in OPERATOR_NAMES {
            assert!(out.contains(&op.to_string()), "missing {op}");
        }
    }

    #[test]
    fn sort_offers_ascending_and_descending_forms() {
        let out = c("commits sort ");
        assert!(out.contains(&"date".to_string()));
        assert!(out.contains(&"-date".to_string()));
    }

    #[test]
    fn within_offers_period_examples() {
        let out = c("commits where date within ");
        assert!(out.contains(&"1 week".to_string()));
    }

    #[test]
    fn amend_is_the_only_terminal_with_an_argument() {
        assert_eq!(c("commits /amend "), vec!["no-edit".to_string()]);
        assert!(c("commits /show ").is_empty());
    }

    #[test]
    fn a_partial_word_is_not_treated_as_context() {
        // "wh" is being typed, so the context is just "commits"
        let out = c("commits wh");
        assert!(out.contains(&"where".to_string()));
    }

    #[test]
    fn unusable_input_yields_no_candidates_rather_than_erroring() {
        // completion runs on every keystroke; it must never blow up
        assert!(c("commits where author %").is_empty());
    }

    #[test]
    fn every_offered_candidate_classifies_and_describes() {
        // the annotated protocol must never emit a blank kind column for a
        // candidate we ourselves offered
        for input in [
            "",
            "commits ",
            "commits via ",
            "commits where ",
            "commits sort ",
        ] {
            for cand in c(input) {
                // dynamic values (author names, shas) legitimately have no
                // kind; static grammar tokens must always have one
                if STEP_KEYWORDS.contains(&cand.as_str())
                    || COMPLETE_MORPHISMS.contains(&cand.as_str())
                    || cand.starts_with('/')
                {
                    let (_, kind, desc) = annotate(&cand);
                    assert!(!kind.is_empty(), "no kind for {cand:?} from {input:?}");
                    assert!(!desc.is_empty(), "no description for {cand:?}");
                }
            }
        }
    }
}
