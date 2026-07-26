//! Positional selection with Python's slice semantics.
//!
//! `[0..10]`, `[5]`, `[-1]`, `[....2]`, `[....-1]`, and — a gitq extension
//! Python has no syntax for — a comma-separated union: `[0..3,7,-2..]`.
//!
//! The separator is `..`, as in git revision ranges and Rust, rather than
//! Python's `:`.  The cost falls on the third component: a bare step has to
//! write both empty bounds, so "every other" is `[....2]` and "reversed" is
//! `[....-1]`.  Those parse cleanly (`"....2".split("..")` is `["", "", "2"]`)
//! but read poorly, and they are the one place this syntax is worse than the
//! one it replaced.
//!
//! Python's rules are followed deliberately rather than approximately,
//! because half-remembered slicing is worse than none: half-open ranges,
//! negative indices counting from the end, negative steps walking backwards,
//! and silent clamping of out-of-range *slice* bounds.  The one place Python
//! raises — a bare index past the end — raises here too, since asking for one
//! specific row that does not exist is a mistake rather than an empty result.
//!
//! Selectors are evaluated left to right and concatenated, so order and
//! repetition are preserved: `[::-1]` reverses, `[0,0]` yields the first row
//! twice.  That keeps the step honest as a general reordering tool rather
//! than only a set-picker.

use crate::ast::Sel;

/// Parse the body of a bracket token (everything between `[` and `]`).
///
/// Returns a message rather than a bare `None` so the parser can fail loud
/// with something that explains itself.
pub fn parse_selectors(body: &str) -> Result<Vec<Sel>, String> {
    if body.trim().is_empty() {
        return Err("gitq: empty selection '[]' — use '[..]' for everything".into());
    }
    body.split(',').map(|p| parse_one(p.trim())).collect()
}

fn parse_int(t: &str, whole: &str) -> Result<isize, String> {
    t.parse::<isize>().map_err(|_| {
        // the two likely spellings: a dash reads naturally as a range, and a
        // colon is what Python (and gitq before this) used
        if t.contains(':') {
            let fixed = whole.replace(':', "..");
            format!("gitq: bad selection '[{whole}]' — ranges use '..', so '[{fixed}]'")
        } else if t.matches('-').count() > 1 || t[1..].contains('-') {
            let fixed = t.replacen('-', "..", 1);
            format!(
                "gitq: bad selection '[{whole}]' — ranges use '..' not '-' \
                 (a dash is a negative index, so '[{fixed}]')"
            )
        } else {
            format!("gitq: bad selection '[{whole}]' — '{t}' is not a number")
        }
    })
}

fn parse_one(part: &str) -> Result<Sel, String> {
    if part.is_empty() {
        return Err("gitq: empty selector in '[...]'".into());
    }
    if !part.contains("..") {
        return Ok(Sel::Index(parse_int(part, part)?));
    }

    let bits: Vec<&str> = part.split("..").collect();
    if bits.len() > 3 {
        return Err(format!(
            "gitq: bad selection '[{part}]' — at most 'start..stop..step'"
        ));
    }
    let field = |i: usize| -> Result<Option<isize>, String> {
        match bits.get(i).map(|b| b.trim()) {
            None | Some("") => Ok(None),
            Some(t) => Ok(Some(parse_int(t, part)?)),
        }
    };
    let step = field(2)?;
    if step == Some(0) {
        return Err(format!("gitq: bad selection '[{part}]' — step cannot be 0"));
    }
    Ok(Sel::Range {
        start: field(0)?,
        stop: field(1)?,
        step,
    })
}

/// Python's `slice.indices`: resolve possibly-negative, possibly-absent
/// bounds against a concrete length, clamping rather than failing.
fn resolve(start: Option<isize>, stop: Option<isize>, step: isize, len: isize) -> (isize, isize) {
    let clamp = |v: isize, lo: isize, hi: isize| v.max(lo).min(hi);
    let start = match start {
        None => {
            if step < 0 {
                len - 1
            } else {
                0
            }
        }
        Some(v) => {
            let v = if v < 0 { v + len } else { v };
            if step < 0 {
                clamp(v, -1, len - 1)
            } else {
                clamp(v, 0, len)
            }
        }
    };
    let stop = match stop {
        None => {
            if step < 0 {
                -1
            } else {
                len
            }
        }
        Some(v) => {
            let v = if v < 0 { v + len } else { v };
            if step < 0 {
                clamp(v, -1, len - 1)
            } else {
                clamp(v, 0, len)
            }
        }
    };
    (start, stop)
}

/// The positions SELECTORS pick out of a list of LEN items, in order.
///
/// An out-of-range bare index is the one hard error; everything else clamps.
pub fn positions(sels: &[Sel], len: usize) -> Result<Vec<usize>, String> {
    let l = len as isize;
    let mut out = Vec::new();
    for s in sels {
        match s {
            Sel::Index(i) => {
                let r = if *i < 0 { *i + l } else { *i };
                if r < 0 || r >= l {
                    return Err(format!(
                        "gitq: selection [{i}] is out of range — {len} result{} to choose from",
                        if len == 1 { "" } else { "s" }
                    ));
                }
                out.push(r as usize);
            }
            Sel::Range { start, stop, step } => {
                let st = step.unwrap_or(1);
                let (from, to) = resolve(*start, *stop, st, l);
                let mut i = from;
                while (st > 0 && i < to) || (st < 0 && i > to) {
                    // resolve() has already clamped into range
                    out.push(i as usize);
                    i += st;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(body: &str) -> Vec<Sel> {
        parse_selectors(body).expect("should parse")
    }
    /// The positions `body` picks out of `n` items — compared against what
    /// Python would give for the same slice.
    fn pick(body: &str, n: usize) -> Vec<usize> {
        positions(&sel(body), n).expect("should resolve")
    }

    #[test]
    fn a_bare_index_is_zero_based() {
        assert_eq!(pick("0", 5), vec![0]);
        assert_eq!(pick("4", 5), vec![4]);
    }

    #[test]
    fn negative_indices_count_from_the_end() {
        assert_eq!(pick("-1", 5), vec![4]);
        assert_eq!(pick("-5", 5), vec![0]);
    }

    #[test]
    fn ranges_are_half_open_like_python() {
        assert_eq!(pick("0..3", 5), vec![0, 1, 2]);
        assert_eq!(pick("1..2", 5), vec![1]);
        assert_eq!(pick("3..3", 5), Vec::<usize>::new());
    }

    #[test]
    fn omitted_bounds_mean_the_ends() {
        assert_eq!(pick("..3", 5), vec![0, 1, 2]);
        assert_eq!(pick("3..", 5), vec![3, 4]);
        assert_eq!(pick("..", 3), vec![0, 1, 2]);
    }

    #[test]
    fn steps_and_reversal() {
        assert_eq!(pick("....2", 6), vec![0, 2, 4]);
        assert_eq!(pick("1....2", 6), vec![1, 3, 5]);
        assert_eq!(pick("....-1", 4), vec![3, 2, 1, 0]);
        assert_eq!(pick("....-2", 5), vec![4, 2, 0]);
        assert_eq!(pick("4..1..-1", 6), vec![4, 3, 2]);
    }

    #[test]
    fn negative_bounds_inside_ranges() {
        assert_eq!(pick("-2..", 5), vec![3, 4]);
        assert_eq!(pick("..-2", 5), vec![0, 1, 2]);
        assert_eq!(pick("-3..-1", 5), vec![2, 3]);
    }

    #[test]
    fn slice_bounds_clamp_rather_than_failing() {
        // python: [0:1000] on five items is five items
        assert_eq!(pick("0..1000", 5), vec![0, 1, 2, 3, 4]);
        assert_eq!(pick("-1000..2", 5), vec![0, 1]);
        assert_eq!(pick("10..20", 5), Vec::<usize>::new());
    }

    #[test]
    fn a_union_concatenates_in_the_order_written() {
        assert_eq!(pick("0..2,4", 6), vec![0, 1, 4]);
        assert_eq!(pick("4,0..2", 6), vec![4, 0, 1]);
        // the shape the TUI emits for a multi-row selection
        assert_eq!(pick("1..3,5..7", 10), vec![1, 2, 5, 6]);
    }

    #[test]
    fn repetition_is_preserved_rather_than_deduped() {
        // it is a concatenation, not a set — so it can reorder too
        assert_eq!(pick("0,0", 3), vec![0, 0]);
    }

    #[test]
    fn an_out_of_range_bare_index_is_an_error_but_a_range_is_not() {
        let e = positions(&sel("9"), 5).unwrap_err();
        assert!(e.contains("out of range"), "{e}");
        assert!(positions(&sel("9..20"), 5).is_ok());
        // and it counts correctly from the other end
        assert!(positions(&sel("-6"), 5).is_err());
        assert!(positions(&sel("-5"), 5).is_ok());
    }

    #[test]
    fn an_empty_selection_says_what_to_write_instead() {
        let e = parse_selectors("  ").unwrap_err();
        assert!(e.contains("[..]"), "{e}");
    }

    #[test]
    fn a_dash_range_is_rejected_with_the_dotted_spelling() {
        // the most likely typo: ranges read naturally with a dash
        let e = parse_selectors("20-30").unwrap_err();
        assert!(e.contains("'..' not '-'"), "{e}");
        assert!(e.contains("[20..30]"), "{e}");
    }

    #[test]
    fn the_old_colon_spelling_is_rejected_with_the_dotted_one() {
        // gitq used Python's ':' before; say so rather than "not a number"
        let e = parse_selectors("1:20").unwrap_err();
        assert!(e.contains("ranges use '..'"), "{e}");
        assert!(e.contains("[1..20]"), "{e}");
    }

    #[test]
    fn a_zero_step_is_rejected() {
        assert!(parse_selectors("....0")
            .unwrap_err()
            .contains("step cannot be 0"));
    }

    #[test]
    fn too_many_components_is_rejected() {
        assert!(parse_selectors("1..2..3..4")
            .unwrap_err()
            .contains("start..stop..step"));
    }

    #[test]
    fn selection_over_an_empty_result_is_empty_not_a_crash() {
        assert_eq!(pick("..", 0), Vec::<usize>::new());
        assert_eq!(pick("....-1", 0), Vec::<usize>::new());
        assert!(positions(&sel("0"), 0).is_err());
    }
}
