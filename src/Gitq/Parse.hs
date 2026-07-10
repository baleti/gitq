-- | The flat-syntax pipeline parser.
--
-- Grammar:
--
-- >  pipeline ::= source step* terminal?
-- >  source   ::= "commits" ["in" range-tokens] | "HEAD" | BRANCH
-- >              | "branches" | "tags" | "refs" | "worktrees" | "blobs"
-- >  step     ::= "via" MORPHISM-PATH | "where" conditions | "grep" PATTERN
-- >              | "pickaxe" PATTERN ["regex"] | "path" GLOB
-- >              | "pick" FIELD[,...] | "take" N | "skip" N
-- >              | "first" | "last" | "sort" ["-"]FIELD
-- >  terminal ::= "/show" | "/copy" | ... (the closed terminal registry)
--
-- Typing happens at parse time, in two layers: structural field-sets
-- (threaded through every stage as the current shape) and scalar types
-- (each field's type × each operator's signature).  A query that cannot
-- mean what it says errors here, loudly — see doc/gitq.org, \"Fail Loud\".
module Gitq.Parse
  ( parsePipeline
  , parseSource
  , parseStep
  , parseWhereValue
  , inferFields
  ) where

import Data.Char (isDigit)
import Data.List (intercalate, isPrefixOf)
import qualified Data.Text as T
import qualified Text.Regex.TDFA.ReadRegex as RE
import Gitq.AST
import Gitq.Frame (Value (..))
import Gitq.Registry
import Gitq.Tokenize

type P a = Either String a

perr :: String -> P a
perr = Left

-- | Signal an error if tokens remain, naming the context (a terminal
-- keyword).  A terminal always ends the pipeline, so leftovers almost
-- always mean a multi-word value that needed double-quotes.
expectNoMore :: [String] -> String -> P ()
expectNoMore [] _ = Right ()
expectNoMore (t : _) ctx =
  perr ("gitq: unexpected token '" ++ t ++ "' after '" ++ ctx
        ++ "' (missing quotes around a value?)")

-- | Parse a full pipeline string.
parsePipeline :: String -> P Pipeline
parsePipeline input =
  case tokenize (trim input) of
    [] -> perr "gitq: empty pipeline"
    tokens -> do
      (src, rest) <- parseSource tokens
      (steps, term) <- go rest (sourceFields src)
      Right (Pipeline src steps term)
 where
  trim = dropWhile (`elem` " \t\n\r")
       . reverse . dropWhile (`elem` " \t\n\r") . reverse
  go [] _ = Right ([], Nothing)
  go (tok : rest) fields
    | isTerminalToken tok = do
        term <- parseTerminal (drop 1 tok) rest
        Right ([], Just term)
    | isStepKeyword tok = do
        (steps, rest', fields') <- parseStep (tok : rest) fields
        (more, term) <- go rest' fields'
        Right (steps ++ more, term)
    | otherwise =
        perr ("gitq: expected step keyword or /terminal, got '" ++ tok ++ "'")

-- | Parse the source (first stage), returning it and the remaining tokens.
parseSource :: [String] -> P (Source, [String])
parseSource [] = perr "gitq: empty pipeline"
parseSource (kw : rest)
  | kw `elem` ["commits", "commit"] =
      case rest of
        ("in" : rest') ->
          let (rangeParts, rest'') = break (\t -> isBoundary (Just t)) rest'
          in Right (SCommits (Just (concat rangeParts)), rest'')
        _ -> Right (SCommits Nothing, rest)
  | kw == "branches" = Right (SBranches, rest)
  | kw == "tags"     = Right (STags, rest)
  | kw `elem` ["worktrees", "worktree"] = Right (SWorktrees, rest)
  | kw == "blobs"    = Right (SBlobs, rest)
  | kw == "refs"     = Right (SRefs, rest)
  | otherwise        = Right (SRef kw, rest)

-- | Parse one step (first token must be a step keyword), threading the
-- current field-set.  Returns the parsed steps (a @via@ path composes
-- several morphisms), the remaining tokens, and the new field-set.
parseStep :: [String] -> [String] -> P ([Step], [String], [String])
parseStep [] _ = perr "gitq: internal error: parseStep on empty input"
parseStep (kw : tokens) fields = case kw of
  "via" -> do
    (morphs, rest) <- parseVia tokens
    -- Type-check the chain by folding each morphism's registry signature:
    -- its required field must be in the set yielded by the previous one.
    let pathTok = case tokens of (t : _) -> t; [] -> ""
    fields' <- foldChain pathTok fields morphs
    Right (map StVia morphs, rest, fields')
  "where" -> do
    (conds, rest) <- parseWhere tokens fields
    Right ([StWhere conds], rest, fields)
  "grep" -> do
    requireField fields "sha" "grep"
    case tokens of
      [] -> perr "gitq: 'grep' requires a pattern"
      (patTok : rest) ->
        let isRe = "/" `isPrefixOf` patTok
            pat  = if isRe then unregex patTok else unquote patTok
        in Right ([StGrep pat isRe], rest, lineFields)
  "pickaxe" -> do
    requireField fields "sha" "pickaxe"
    case tokens of
      [] -> perr "gitq: 'pickaxe' requires a pattern"
      (patTok : rest0) ->
        let slashRe = "/" `isPrefixOf` patTok
            kwRe    = take 1 rest0 == ["regex"]
            pat     = if slashRe then unregex patTok else unquote patTok
            rest    = if kwRe then drop 1 rest0 else rest0
        in Right ([StPickaxe pat (slashRe || kwRe)], rest, fields)
  "path" -> do
    requireField fields "path" "path"
    case tokens of
      [] -> perr "gitq: 'path' requires a glob pattern"
      (t : rest) -> Right ([StPath (unquote t)], rest, fields)
  "pick" ->
    -- Driven by field-list membership (plus comma), not the generic
    -- boundary check: `path` is both a step keyword and a field, and
    -- `pick path, author` must read it as a field here.
    let go acc (t : rest)
          | t == ","          = go acc rest
          | t `elem` fields   = go (t : acc) rest
        go acc rest = (reverse acc, rest)
        (picked, remaining) = go [] tokens
    in if null picked
         then perr ("gitq: 'pick' requires at least one field name, got "
                    ++ (case remaining of
                          (t : _) -> "'" ++ t ++ "'"
                          []      -> "end of input"))
         else Right ([StPick picked], remaining, picked)
  "take" -> do
    (n, rest) <- parseCount tokens "take"
    Right ([StTake n], rest, fields)
  "skip" -> do
    (n, rest) <- parseCount tokens "skip"
    Right ([StSkip n], rest, fields)
  "first" -> Right ([StFirst], tokens, fields)
  "last"  -> Right ([StLast], tokens, fields)
  "sort" -> case tokens of
    [] -> perr "gitq: 'sort' requires a field name"
    (f : rest) ->
      let desc = "-" `isPrefixOf` f
          name = if desc then drop 1 f else f
      in if name `elem` fields
           then Right ([StSort name desc], rest, fields)
           else perr ("gitq: field '" ++ name
                      ++ "' not valid here after 'sort' (current frame has: "
                      ++ intercalate ", " fields ++ ")")
  _ -> perr ("gitq: unknown step keyword '" ++ kw ++ "'")
 where
  foldChain _ fs [] = Right fs
  foldChain pathTok fs (m : ms) = do
    requireField fs (morphismRequires m) ("via " ++ pathTok)
    foldChain pathTok (morphismYields m) ms

-- | Error unless the field is in the current field-set, naming the context.
requireField :: [String] -> String -> String -> P ()
requireField fields field context
  | field `elem` fields = Right ()
  | otherwise =
      perr ("gitq: '" ++ context ++ "' needs a '" ++ field
            ++ "' field, but the current frame only has: "
            ++ intercalate ", " fields)

-- | Parse a via-step morphism path (one token, possibly composing several
-- morphisms).  When the final morphism is @diff@, its optional REF
-- argument is consumed from the following token, unless that token is a
-- stage boundary or another morphism path.
parseVia :: [String] -> P ([Morphism], [String])
parseVia [] = perr "gitq: 'via' requires a morphism"
parseVia (pathTok : rest) = do
  morphs <- parseMorphismPath pathTok
  case (reverse morphs, rest) of
    (MDiff Nothing : before, r : rest')
      | not (isBoundary (Just r)), not ("." `isPrefixOf` r) ->
          Right (reverse (MDiff (Just r) : before), rest')
    _ -> Right (morphs, rest)

-- | Parse a non-negative integer count for take\/skip, erroring on tokens
-- like @5x@ rather than silently truncating them.
parseCount :: [String] -> String -> P (Int, [String])
parseCount tokens stepName = case tokens of
  (t : rest) | not (null t), all isDigit t -> Right (read t, rest)
  (t : _) -> bad ("'" ++ t ++ "'")
  []      -> bad "end of input"
 where
  bad got = perr ("gitq: '" ++ stepName ++ "' requires a number, got " ++ got)

-- | Parse a where-condition's raw value token into its runtime value.
parseWhereValue :: String -> Value
parseWhereValue tok
  | "\"" `isPrefixOf` tok            = VStr (T.pack (unquote tok))
  | "/" `isPrefixOf` tok             = VStr (T.pack (unregex tok))
  | not (null tok), all isDigit tok  = VNum (read tok)
  | otherwise                        = VStr (T.pack tok)

-- | Parse where-conditions.  Step keywords and \/terminals act as stage
-- boundaries and are never consumed as condition values.  Fields must be
-- members of the current field-set.  There is no explicit @contains@
-- keyword: a token right after a field that isn't a recognized operator is
-- taken directly as the value with an implicit substring condition, for
-- field types where that's sensible.
parseWhere :: [String] -> [String] -> P ([Cond], [String])
parseWhere tokens fields =
  case tokens of
    (t : _) | t `notElem` fields, not (isBoundary (Just t)) ->
      perr ("gitq: field '" ++ t ++ "' not valid here after 'where' (current frame has: "
            ++ intercalate ", " fields ++ ")")
    _ -> go tokens []
 where
  go toks acc = case toks of
    (fieldTok : rest) | fieldTok `elem` fields -> do
      (cond, rest') <- condition fieldTok rest
      afterComma (cond : acc) rest'
    _ -> Right (reverse acc, toks)

  afterComma acc ("," : rest) = case rest of
    (t : _) | t `elem` fields -> go rest acc
    (t : _) -> perr ("gitq: expected a field name after ',' in 'where', got '" ++ t ++ "'")
    []      -> perr "gitq: expected a field name after ',' in 'where', got 'end of input'"
  afterComma acc rest = go rest acc

  ftypeName t = case t of
    TString -> "string"; TSha -> "sha"; TDate -> "date"
    TNumber -> "number"; TFlag -> "flag"

  opsFor ft =
    intercalate ", "
      [ op | op <- operatorNames
      , Just sig <- [operatorSignature op], ft `elem` sig ]

  condition fieldTok rest =
    let ft   = fieldType fieldTok
        next = case rest of (t : _) -> Just t; [] -> Nothing
    in case next of
      -- Bare flag: clause ends at nil, comma, another field, or a /terminal.
      n | n == Nothing || n == Just ","
          || maybe False (`elem` fields) n
          || maybe False isTerminalToken n ->
        if ft == TFlag
          then Right (Cond fieldTok OpIs (VBool True), rest)
          else perr ("gitq: bare 'where " ++ fieldTok ++ "' tests a flag, but '"
                     ++ fieldTok ++ "' is a " ++ ftypeName ft
                     ++ " field (add an operator and value)")
      -- Step keyword next: ends a bare flag cleanly; for any other field
      -- type it's an unquoted value that needed quotes.
      Just n | isStepKeyword n ->
        if ft == TFlag
          then Right (Cond fieldTok OpIs (VBool True), rest)
          else perr ("gitq: 'where " ++ fieldTok ++ "' requires a value; step keyword '"
                     ++ n ++ "' must be quoted: \"" ++ n ++ "\"")
      -- Recognized operator keyword
      Just opTok | Just sig <- operatorSignature opTok ->
        if ft `notElem` sig
          then perr ("gitq: operator '" ++ opTok ++ "' does not apply to '"
                     ++ fieldTok ++ "' (a " ++ ftypeName ft ++ " field; try: "
                     ++ opsFor ft ++ ")")
          else
            let rest1 = drop 1 rest
                next2 = case rest1 of (t : _) -> Just t; [] -> Nothing
            in case next2 of
              Just n2 | isStepKeyword n2 ->
                perr ("gitq: '" ++ opTok ++ "' requires a value; step keyword '"
                      ++ n2 ++ "' must be quoted: \"" ++ n2 ++ "\"")
              -- No value after the operator: only `is` works valueless.
              n2 | n2 == Nothing || n2 == Just ","
                   || maybe False (`elem` fields) n2
                   || maybe False isTerminalToken n2 ->
                if opTok == "is"
                  then Right (Cond fieldTok OpIs (VBool True), rest1)
                  else perr ("gitq: operator '" ++ opTok ++ "' requires a value, got "
                             ++ maybe "end of input" (\t -> "'" ++ t ++ "'") n2)
              Just valTok ->
                let val = parseWhereValue valTok
                in if ft == TNumber && (case val of VNum _ -> False; _ -> True)
                     then perr ("gitq: '" ++ fieldTok ++ "' is a number field; '"
                                ++ valTok ++ "' is not a number")
                     else do
                       op <- opFromName opTok
                       -- a pattern that can't compile must fail here, not
                       -- when the executor matches the first frame
                       case (op, val) of
                         (OpRegex, VStr pat)
                           | Left err <- RE.parseRegex (T.unpack pat) ->
                               perr ("gitq: invalid regex '" ++ T.unpack pat ++ "': "
                                     ++ unwords (words (show err)))
                         _ -> Right ()
                       Right (Cond fieldTok op val, drop 1 rest1)
              Nothing -> perr "gitq: internal error: unreachable where state"
      -- Implicit operator: the token is the value directly (substring
      -- match for text-shaped fields, equality for numbers)
      Just valTok | Just iop <- implicitOp ft ->
        let val = case (iop, parseWhereValue valTok) of
              -- an all-digit token on a text-shaped field is a substring,
              -- not a number: `where sha 95866` must match, not silently
              -- compare a number against a string forever
              (OpContains, VNum _) -> VStr (T.pack valTok)
              (_, v)               -> v
        in case (iop, val) of
             (OpEq, VNum _) -> Right (Cond fieldTok iop val, drop 1 rest)
             (OpEq, _) ->
               perr ("gitq: '" ++ fieldTok ++ "' is a number field; '"
                     ++ valTok ++ "' is not a number")
             _ -> Right (Cond fieldTok iop val, drop 1 rest)
      -- flag field with an unrecognized operator token
      Just opTok ->
        perr ("gitq: unknown where operator '" ++ opTok ++ "' (expected one of: "
              ++ intercalate ", " operatorNames ++ ")")
      Nothing -> perr "gitq: internal error: unreachable where state"

  opFromName n = case n of
    "=="     -> Right OpEq
    "!="     -> Right OpNe
    ">"      -> Right OpGt
    "<"      -> Right OpLt
    ">="     -> Right OpGe
    "<="     -> Right OpLe
    "regex"  -> Right OpRegex
    "after"  -> Right OpAfter
    "before" -> Right OpBefore
    "within" -> Right OpWithin
    "is"     -> Right OpIs
    _        -> perr ("gitq: unknown where operator '" ++ n ++ "'")

-- | Parse a terminal by name (leading @\/@ already stripped) with its
-- remaining tokens.  Every parser consumes all tokens it is given and
-- errors on leftovers — a terminal always ends the pipeline.
parseTerminal :: String -> [String] -> P Terminal
parseTerminal kw tokens = case kw of
  "show"   -> simple TShow
  "copy"   -> simple TCopy
  "insert" -> simple TInsert
  "count"  -> simple TCount
  -- /delete is a true alias of /remove: it parses to the same op, so it
  -- can never parse successfully and then fall through to a silent no-op.
  "remove" -> simple TRemove
  "delete" -> simple TRemove
  "stage"  -> simple TStage
  "branch-off" ->
    let (name, rest1) = optQuoted tokens
        (wt, rest2) = case rest1 of
          ("worktree" : p : rest') -> (Just (unquote p), rest')
          _                        -> (Nothing, rest1)
    in expectNoMore rest2 kw >> Right (TBranchOff name wt)
  "amend" -> case tokens of
    ("no-edit" : rest) -> expectNoMore rest kw >> Right (TAmend True Nothing)
    (t : rest) | "\"" `isPrefixOf` t ->
      expectNoMore rest kw >> Right (TAmend False (Just (unquote t)))
    rest -> expectNoMore rest kw >> Right (TAmend False Nothing)
  "squash" -> optionalMsg TSquash
  "reword" -> optionalMsg TReword
  "commit" -> optionalMsg TCommit
  "mark" -> case tokens of
    (t : rest) -> expectNoMore rest kw >> Right (TMark (Just (unquote t)))
    []         -> Right (TMark Nothing)
  "worktree" ->
    let (path, rest) = optQuoted tokens
    in expectNoMore rest kw >> Right (TWorktree path)
  _ ->
    perr ("gitq: unknown terminal operation '" ++ kw ++ "' (expected one of: "
          ++ intercalate ", " terminalNames ++ ")")
 where
  simple t = expectNoMore tokens kw >> Right t
  optionalMsg mk = case tokens of
    (t : rest) | "\"" `isPrefixOf` t ->
      expectNoMore rest kw >> Right (mk (Just (unquote t)))
    rest -> expectNoMore rest kw >> Right (mk Nothing)
  optQuoted (t : rest) | "\"" `isPrefixOf` t = (Just (unquote t), rest)
  optQuoted rest = (Nothing, rest)

-- | The field-set active after the fully-typed tokens of a (possibly
-- incomplete) pipeline prefix.  Replays the real parser stage by stage so
-- completion and the strict parser can never drift apart; the first stage
-- that can't be parsed from what's typed so far just stops the walk,
-- returning the last successfully-computed field-set.
inferFields :: [String] -> [String]
inferFields ctx =
  case parseSource ctx of
    Left _ -> commitFields
    Right (src, rest) -> walk rest (sourceFields src)
 where
  walk [] fields = fields
  walk toks@(t : _) fields
    | isTerminalToken t = fields
    | isStepKeyword t =
        case parseStep toks fields of
          Right (_, rest, fields') -> walk rest fields'
          Left _                   -> fields
    | otherwise = fields
