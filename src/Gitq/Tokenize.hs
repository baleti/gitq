-- | The flat-pipeline tokenizer.  Whitespace separates tokens; the special
-- cases are quoted strings, \/regex\/ literals vs. \/terminal commands,
-- two-character comparison operators, sort negation (@-date@), and the
-- widened bare-word class that lets morphism paths (@parent*@,
-- @tree.entries[Blob]@, @diff.hunks@) tokenize as single words.
module Gitq.Tokenize
  ( tokenize
  , isStepKeyword
  , isTerminalToken
  , isBoundary
  , unquote
  , unregex
  ) where

import Data.Char (isAlpha, isDigit)
import Gitq.Registry (stepKeywords)

-- | Strip surrounding double-quotes.
unquote :: String -> String
unquote ('"' : rest@(_:_)) = init rest
unquote s                  = s

-- | Extract the pattern from a @\/pattern\/@ token.
unregex :: String -> String
unregex ('/' : rest@(_:_)) = init rest
unregex s                  = s

isStepKeyword :: String -> Bool
isStepKeyword = (`elem` stepKeywords)

-- | A \/command terminal token (not a \/regex\/ literal): starts with @\/@
-- and does not end with one.
isTerminalToken :: String -> Bool
isTerminalToken tok@('/' : _ : _) = last tok /= '/'
isTerminalToken _                 = False

-- | A stage boundary: end of input, a step keyword, or a \/terminal.
isBoundary :: Maybe String -> Bool
isBoundary Nothing    = True
isBoundary (Just tok) = isStepKeyword tok || isTerminalToken tok

-- | The extended bare-word continuation class: letters, digits, and the
-- characters that let SHAs, dates, ranges, refs, and bare morphism paths
-- (@parent*@, @tree.entries[Blob]@) tokenize as one word.
isWordChar :: Char -> Bool
isWordChar c =
  isAlpha c || isDigit c
    || c `elem` "-_/~@{}.*+[]"
    || c == '\x2020'                  -- † (parent adjoint)

tokenize :: String -> [String]
tokenize = go
 where
  go [] = []
  go s@(c : rest)
    | c `elem` " \t\n\r" = go rest
    -- Quoted string.  Only consume the closing quote if one was actually
    -- found — this runs on every keystroke via live completion, so an
    -- in-progress, still-unterminated quote must never error here.
    | c == '"' =
        let (tok, rest') = quoted rest
        in ('"' : tok) : go rest'
    -- '/' starts either a /regex/ literal (a matching closing slash lies
    -- ahead) or a /command terminal token (it does not).  The scan for the
    -- closing slash must stop at a quote character — otherwise a terminal
    -- argument like /branch-off "feature/x" misreads the / inside the
    -- branch name as this token's own closing slash.
    | c == '/' =
        case break (`elem` "/\"") rest of
          (body, '/' : rest') -> ('/' : body ++ "/") : go rest'
          _ ->
            let (word, rest') = span isCmdChar rest
            in ('/' : word) : go rest'
    | c == ',' = "," : go rest
    -- two-character comparison operators
    | [c] ++ take 1 rest `elem` ["==", "!=", ">=", "<="] =
        (c : take 1 rest) : go (drop 1 rest)
    | c `elem` "><" = [c] : go rest
    -- Double-dash flag (--not, --all: revspec vocabulary for `in`)
    | c == '-', ('-' : n : _) <- rest, isAlpha n =
        let (word, rest') = span isWordChar (drop 1 rest)
        in ("--" ++ word) : go rest'
    -- Caret-prefixed rev (^v1.0: revspec exclusion for `in`)
    | c == '^', (n : _) <- rest, isAlpha n || isDigit n =
        let (word, rest') = span isWordChar rest
        in ('^' : word) : go rest'
    -- Negated field name: -date (used in `sort -date`)
    | c == '-', (n : _) <- rest, isAlpha n || n == '_' =
        let (word, rest') = span isWordChar rest
        in ('-' : word) : go rest'
    -- Historical leading-dot morphism path (.parent, .tree.entries[Blob])
    | c == '.' =
        let (word, rest') = span isWordChar rest
        in ('.' : word) : go rest'
    -- Bare word (digit- or letter-starting: same continuation class, so
    -- SHA prefixes like 062062e9 and dates like 2026-05-25 stay one token)
    | isDigit c || isAlpha c || c == '_' =
        let (word, rest') = span isWordChar s
        in word : go rest'
    | otherwise = go rest
  isCmdChar ch = isAlpha ch || isDigit ch || ch `elem` "-_"
  -- consume up to (and including) a closing quote, honoring backslash
  -- escapes; an unterminated quote consumes to end of input
  quoted = quotedGo
  quotedGo [] = ("", "")
  quotedGo ('\\' : x : rest) =
    let (tok, rest') = quotedGo rest in ('\\' : x : tok, rest')
  quotedGo ('"' : rest) = ("\"", rest)
  quotedGo (x : rest) =
    let (tok, rest') = quotedGo rest in (x : tok, rest')
