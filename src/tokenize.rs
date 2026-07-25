//! The flat-pipeline tokenizer.
//!
//! Whitespace separates tokens; the special cases are quoted strings,
//! `/regex/` literals vs. `/terminal` commands, two-character comparison
//! operators, sort negation (`-date`), and the widened bare-word class that
//! lets morphism paths (`parent*`, `tree.entries[Blob]`, `diff.hunks`)
//! tokenize as single words.
//!
//! # What changed from the Haskell tokenizer
//!
//! Two things, both structural rather than cosmetic.
//!
//! **Tokens are typed.**  The Haskell tokenizer returned `[String]` and the
//! parser recovered the kind afterwards by re-inspecting the characters —
//! `isTerminalToken`, `isStepKeyword`, `isBoundary`, `unquote`, `unregex`.
//! That is an elisp-era shape: everything is a string and you ask it
//! questions later.  A [`Token`] enum carries the decision the tokenizer
//! already made, and those five functions disappear.
//!
//! **Unrecognized input is an error, not a silent skip.**  The Haskell
//! tokenizer ended with `| otherwise = go rest`, dropping any character it
//! did not recognize.  Because a bare word could only *start* with a letter,
//! digit, or `_`, this silently ate `+ % & ! ? : ; ( ) # $ = @` — and ate
//! them mid-token, so the query became a *different query*:
//!
//! ```text
//! commits where author "@lice" /count   =>  0   (correct)
//! commits where author @lice   /count   =>  4   (searched for "lice")
//! ```
//!
//! A query language that silently answers a question you did not ask is
//! worse than one that refuses. This returns [`TokenizeError`] instead.

use crate::registry::STEP_KEYWORDS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A reserved step keyword (`via`, `where`, `sort`, ...).
    Step(String),
    /// A bare word: identifiers, SHAs, dates, values, morphism paths.
    Word(String),
    /// A double-quoted string, already unescaped and unquoted.
    Quoted(String),
    /// A `/pattern/` literal, already stripped of its slashes.
    Regex(String),
    /// A `/command` terminal, without the leading slash.
    Terminal(String),
    /// A comparison or word operator (`==`, `>=`, `regex`, `is`, ...).
    /// Word-shaped operators are classified by the parser, not here — the
    /// tokenizer only knows the punctuation forms.
    Operator(String),
    /// `,` — the condition separator in `where`.
    Comma,
    /// A `--flag` from git's revspec vocabulary (`--not`, `--all`).
    Flag(String),
    /// A `^rev` revspec exclusion.
    ExcludeRev(String),
    /// `-field`, the descending form in `sort`.
    NegField(String),
}

impl Token {
    /// The surface text of a token, as the user typed it minus its
    /// delimiters.  This is what parser error messages quote.
    pub fn text(&self) -> &str {
        match self {
            Token::Step(s)
            | Token::Word(s)
            | Token::Quoted(s)
            | Token::Regex(s)
            | Token::Terminal(s)
            | Token::Operator(s)
            | Token::Flag(s)
            | Token::ExcludeRev(s)
            | Token::NegField(s) => s,
            Token::Comma => ",",
        }
    }

    /// The token as it appeared in the source, delimiters included.  Error
    /// messages in the 0.7.0 catalogue quote tokens in this form.
    pub fn display(&self) -> String {
        match self {
            Token::Quoted(s) => format!("\"{s}\""),
            Token::Regex(s) => format!("/{s}/"),
            Token::Terminal(s) => format!("/{s}"),
            Token::Flag(s) => format!("--{s}"),
            Token::ExcludeRev(s) => format!("^{s}"),
            Token::NegField(s) => format!("-{s}"),
            other => other.text().to_string(),
        }
    }

    /// A stage boundary: a step keyword or a `/terminal`.
    pub fn is_boundary(&self) -> bool {
        matches!(self, Token::Step(_) | Token::Terminal(_))
    }

    /// Whether this token can stand as a `where` value or a revspec word.
    pub fn is_value(&self) -> bool {
        matches!(
            self,
            Token::Word(_) | Token::Quoted(_) | Token::Regex(_) | Token::NegField(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizeError {
    pub message: String,
}

impl std::fmt::Display for TokenizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The extended bare-word continuation class: letters, digits, and the
/// characters that let SHAs, dates, ranges, refs, and bare morphism paths
/// (`parent*`, `tree.entries[Blob]`) tokenize as one word.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '-' | '_' | '/' | '~' | '@' | '{' | '}' | '.' | '*' | '+' | '[' | ']')
        // † (parent adjoint)
        || c == '\u{2020}'
}

fn is_cmd_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_')
}

fn word(s: &str) -> Token {
    if STEP_KEYWORDS.contains(&s) {
        Token::Step(s.to_string())
    } else {
        Token::Word(s.to_string())
    }
}

/// Tokenize a pipeline string.
///
/// Runs on every keystroke via live completion, so an in-progress,
/// still-unterminated quote must never error — only genuinely unusable
/// characters do.
pub fn tokenize(input: &str) -> Result<Vec<Token>, TokenizeError> {
    let cs: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    let take_while_from = |start: usize, pred: &dyn Fn(char) -> bool| -> usize {
        let mut j = start;
        while j < cs.len() && pred(cs[j]) {
            j += 1;
        }
        j
    };
    let slice = |a: usize, b: usize| -> String { cs[a..b].iter().collect() };

    while i < cs.len() {
        let c = cs[i];

        // whitespace
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // Quoted string.  Only consume the closing quote if one was actually
        // found — an unterminated quote mid-typing consumes to end of input
        // and is still a valid token.
        if c == '"' {
            let (text, next) = quoted(&cs, i + 1);
            out.push(Token::Quoted(text));
            i = next;
            continue;
        }

        // '/' starts either a /regex/ literal (a matching closing slash lies
        // ahead) or a /command terminal token (it does not).  The scan for
        // the closing slash must stop at a quote character — otherwise a
        // terminal argument like /branch-off "feature/x" misreads the '/'
        // inside the branch name as this token's own closing slash.
        if c == '/' {
            let mut j = i + 1;
            while j < cs.len() && cs[j] != '/' && cs[j] != '"' {
                j += 1;
            }
            if j < cs.len() && cs[j] == '/' {
                out.push(Token::Regex(slice(i + 1, j)));
                i = j + 1;
            } else {
                let end = take_while_from(i + 1, &is_cmd_char);
                out.push(Token::Terminal(slice(i + 1, end)));
                i = end;
            }
            continue;
        }

        if c == ',' {
            out.push(Token::Comma);
            i += 1;
            continue;
        }

        // two-character comparison operators
        if i + 1 < cs.len() {
            let pair: String = cs[i..i + 2].iter().collect();
            if matches!(pair.as_str(), "==" | "!=" | ">=" | "<=") {
                out.push(Token::Operator(pair));
                i += 2;
                continue;
            }
        }

        if c == '>' || c == '<' {
            out.push(Token::Operator(c.to_string()));
            i += 1;
            continue;
        }

        // Double-dash flag (--not, --all: revspec vocabulary for `in`)
        if c == '-' && i + 2 < cs.len() && cs[i + 1] == '-' && cs[i + 2].is_alphabetic() {
            let end = take_while_from(i + 2, &is_word_char);
            out.push(Token::Flag(slice(i + 2, end)));
            i = end;
            continue;
        }

        // Caret-prefixed rev (^v1.0: revspec exclusion for `in`)
        if c == '^' && i + 1 < cs.len() && cs[i + 1].is_alphanumeric() {
            let end = take_while_from(i + 1, &is_word_char);
            out.push(Token::ExcludeRev(slice(i + 1, end)));
            i = end;
            continue;
        }

        // Negated field name: -date (used in `sort -date`)
        if c == '-' && i + 1 < cs.len() && (cs[i + 1].is_alphabetic() || cs[i + 1] == '_') {
            let end = take_while_from(i + 1, &is_word_char);
            out.push(Token::NegField(slice(i + 1, end)));
            i = end;
            continue;
        }

        // Historical leading-dot morphism path (.parent, .tree.entries[Blob])
        if c == '.' {
            let end = take_while_from(i + 1, &is_word_char);
            out.push(word(&slice(i, end)));
            i = end;
            continue;
        }

        // Bare word.  The Haskell tokenizer required a word to START with a
        // letter, digit, or `_`, while accepting a much wider continuation
        // class — the asymmetry that made `@lice` decay to `lice` and `+`
        // vanish entirely.  Here a word may start with any character in its
        // own continuation class, checked after the special forms above have
        // had their say, so `@{upstream}` and a bare `+` sign value tokenize
        // as themselves while `parent*` and `-date` keep their meanings.
        if is_word_char(c) {
            let end = take_while_from(i, &is_word_char);
            out.push(word(&slice(i, end)));
            i = end;
            continue;
        }

        // Anything else is unusable.  The Haskell tokenizer dropped it and
        // carried on, which turned `where author @lice` into a search for
        // "lice" and returned confident, wrong results.
        return Err(TokenizeError {
            message: format!(
                "gitq: unexpected character '{c}' in query \
                 (quote it if you meant it literally: \"{c}\")"
            ),
        });
    }

    Ok(out)
}

/// Consume up to (and including) a closing quote, honoring backslash
/// escapes.  An unterminated quote consumes to end of input.  Returns the
/// unescaped body and the index just past the closing quote.
fn quoted(cs: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut i = start;
    while i < cs.len() {
        match cs[i] {
            '\\' if i + 1 < cs.len() => {
                out.push(cs[i + 1]);
                i += 2;
            }
            '"' => return (out, i + 1),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Token> {
        tokenize(s).expect("should tokenize")
    }

    #[test]
    fn splits_on_whitespace_and_tags_step_keywords() {
        assert_eq!(
            toks("commits where author alice"),
            vec![
                Token::Word("commits".into()),
                Token::Step("where".into()),
                Token::Word("author".into()),
                Token::Word("alice".into()),
            ]
        );
    }

    #[test]
    fn quoted_strings_are_unquoted_and_unescaped() {
        assert_eq!(toks(r#""add b""#), vec![Token::Quoted("add b".into())]);
        assert_eq!(toks(r#""a\"b""#), vec![Token::Quoted(r#"a"b"#.into())]);
    }

    #[test]
    fn unterminated_quote_is_not_an_error() {
        // live completion tokenizes on every keystroke
        assert_eq!(toks(r#"where message "fi"#).len(), 3);
    }

    #[test]
    fn regex_literal_versus_terminal_command() {
        assert_eq!(toks("/needle/"), vec![Token::Regex("needle".into())]);
        assert_eq!(toks("/show"), vec![Token::Terminal("show".into())]);
        assert_eq!(
            toks("/branch-off"),
            vec![Token::Terminal("branch-off".into())]
        );
    }

    #[test]
    fn terminal_argument_slash_is_not_a_regex_close() {
        assert_eq!(
            toks(r#"/branch-off "feature/x""#),
            vec![
                Token::Terminal("branch-off".into()),
                Token::Quoted("feature/x".into()),
            ]
        );
    }

    #[test]
    fn operators_and_revspec_vocabulary() {
        assert_eq!(toks("=="), vec![Token::Operator("==".into())]);
        assert_eq!(toks(">="), vec![Token::Operator(">=".into())]);
        assert_eq!(toks(">"), vec![Token::Operator(">".into())]);
        assert_eq!(toks("--not"), vec![Token::Flag("not".into())]);
        assert_eq!(toks("^v1"), vec![Token::ExcludeRev("v1".into())]);
        assert_eq!(toks("-date"), vec![Token::NegField("date".into())]);
    }

    #[test]
    fn morphism_paths_stay_one_token() {
        for p in [
            "parent*",
            "parent+",
            "parent†",
            "tree.entries[Blob]",
            "diff.hunks",
        ] {
            assert_eq!(toks(p).len(), 1, "{p} should be one token");
            assert_eq!(toks(p)[0].text(), p);
        }
        assert_eq!(toks(".parent")[0].text(), ".parent");
    }

    #[test]
    fn shas_and_dates_stay_one_token() {
        assert_eq!(toks("062062e9")[0].text(), "062062e9");
        assert_eq!(toks("2026-05-25")[0].text(), "2026-05-25");
        assert_eq!(toks("main..HEAD")[0].text(), "main..HEAD");
    }

    // --- the 0.7.0 silent-drop bug -------------------------------------

    #[test]
    fn unusable_characters_are_an_error_not_a_silent_drop() {
        // 0.7.0 dropped every one of these on the floor.
        for c in ['%', '&', '!', '?', ':', ';', '(', ')', '#', '$', '='] {
            let q = format!("commits where author {c}");
            assert!(
                tokenize(&q).is_err(),
                "0.7.0 silently dropped {c:?}; it must now be an error"
            );
        }
    }

    #[test]
    fn word_class_characters_may_now_start_a_word() {
        // 0.7.0 dropped a bare '+' because a word could not start with one,
        // so `where sign +` reported "bare 'where sign' tests a flag".
        assert_eq!(toks("+"), vec![Token::Word("+".into())]);
        assert_eq!(toks("@{upstream}"), vec![Token::Word("@{upstream}".into())]);
        // and the special forms still win over the widened word class
        assert_eq!(toks("-date"), vec![Token::NegField("date".into())]);
        assert_eq!(toks(".parent"), vec![Token::Word(".parent".into())]);
        assert_eq!(toks("/show"), vec![Token::Terminal("show".into())]);
    }

    #[test]
    fn at_sign_no_longer_corrupts_the_query() {
        // `@` is a word CONTINUATION char but cannot START a word, so 0.7.0
        // dropped the '@' and searched for "lice" — matching "alice" and
        // returning 4 results where the correct answer was 0.
        let t = toks("commits where author @lice");
        assert_eq!(
            t.last().unwrap().text(),
            "@lice",
            "the value must survive intact, not decay to \"lice\""
        );
    }

    #[test]
    fn quoting_remains_the_escape_hatch() {
        assert_eq!(
            toks(r#"where sign "+""#).last().unwrap(),
            &Token::Quoted("+".into())
        );
    }
}
