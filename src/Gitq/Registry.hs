-- | The registries: single sources of truth shared by the parser, the type
-- checker, the completion engine, and the executor — so they can never
-- disagree about which fields, morphisms, operators, or terminals exist.
module Gitq.Registry
  ( FieldType (..)
  , fieldNames
  , fieldType
  , operatorNames
  , operatorSignature
  , implicitOp
  , stepKeywords
  , commitFields, refFields, worktreeFields, blobFields
  , treeObjectFields, diffFields, hunkFields, lineFields, diffLineFields
  , sourceFields
  , morphismRequires
  , morphismYields
  , parseMorphismPath
  , terminalNames
  , completeSourceKeywords
  , completeMorphisms
  , completeWhereOperators
  , completeTerminals
  , completeDateWithinExamples
  , describeToken
  , tokenKind
  ) where

import Data.Char (isDigit)
import Data.List (isPrefixOf, stripPrefix)
import Gitq.AST

-- | Scalar type of a field: decides which where-operators apply and which
-- sort comparator is used.
data FieldType = TString | TSha | TDate | TNumber | TFlag
  deriving (Eq, Show)

-- | The closed set of field names @where@, @sort@, and @pick@ accept.
-- Used for completion and lexical disambiguation; validation of any
-- particular reference is against the narrower frame-shape field-sets
-- below, threaded through the pipeline as the current field-set.
fieldNames :: [String]
fieldNames =
  [ "sha", "author", "email", "date", "message", "path", "name", "branch"
  , "parents-count", "modified", "staged", "untracked"
  , "tree", "reftype", "detached", "mode", "parent-sha", "commit-sha"
  , "start-line", "end-line", "line-number", "content", "sign"
  ]

-- | Scalar type of each field.  Unknown fields default to string — the
-- weakest assumption — because @pick@ projections are open-ended.
fieldType :: String -> FieldType
fieldType f = case lookup f table of
  Just t  -> t
  Nothing -> TString
 where
  table =
    [ ("sha", TSha), ("author", TString), ("email", TString), ("date", TDate)
    , ("message", TString), ("path", TString), ("name", TString)
    , ("branch", TString), ("parents-count", TNumber), ("modified", TFlag)
    , ("staged", TFlag), ("untracked", TFlag), ("tree", TSha)
    , ("reftype", TString), ("detached", TFlag), ("mode", TString)
    , ("parent-sha", TSha), ("commit-sha", TSha), ("start-line", TNumber)
    , ("end-line", TNumber), ("line-number", TNumber), ("content", TString)
    , ("sign", TString)
    ]

-- | The closed set of where-operators, in registry order.
operatorNames :: [String]
operatorNames = ["==", "!=", ">", "<", ">=", "<=", "regex", "after", "before", "within", "is"]

-- | For each where-operator, the field scalar types it accepts.  There is
-- no @contains@ entry: a value right after a field with no recognized
-- operator between them is an implicit substring match instead.
operatorSignature :: String -> Maybe [FieldType]
operatorSignature op = lookup op table
 where
  table =
    [ ("==",     [TString, TSha, TDate, TNumber, TFlag])
    , ("!=",     [TString, TSha, TDate, TNumber, TFlag])
    , (">",      [TNumber])
    , ("<",      [TNumber])
    , (">=",     [TNumber])
    , ("<=",     [TNumber])
    , ("regex",  [TString, TSha])
    , ("after",  [TDate])
    , ("before", [TDate])
    , ("within", [TDate])
    , ("is",     [TFlag])
    ]

-- | The implicit operator applied when the token right after a field is a
-- value rather than a recognized operator keyword: substring match for
-- text-shaped fields (@where author bal@, @where date 2026-07@), equality
-- for numbers (@where parents-count 2@ — substring over digits would make
-- 2 match 12, a footgun).  Flag fields have no implicit operator: they
-- are bare conditions (@where modified@), and an unrecognized token after
-- one is still an unknown-operator parse error.
implicitOp :: FieldType -> Maybe Op
implicitOp TString = Just OpContains
implicitOp TSha    = Just OpContains
implicitOp TDate   = Just OpContains
implicitOp TNumber = Just OpEq
implicitOp TFlag   = Nothing

-- | Reserved step keywords: these always start a new stage and must be
-- quoted when used as string values.
stepKeywords :: [String]
stepKeywords = ["via", "where", "grep", "pickaxe", "path", "pick", "take", "skip", "first", "last", "sort"]

-- Structural field-set typing: the exact set of fields each frame shape
-- carries, taken from where that shape is constructed in Gitq.Git/Gitq.Exec.

commitFields :: [String]
commitFields = ["sha", "author", "email", "date", "message", "tree", "parents-count"]

refFields :: [String]
refFields = ["sha", "name", "reftype"]

-- | NOTE: modified\/staged\/untracked are declared but not yet populated by
-- the worktree fetcher — they type-check and always read as false until
-- working-tree status is wired in (inherited limitation, see doc/gitq.org).
worktreeFields :: [String]
worktreeFields = ["path", "sha", "branch", "detached", "modified", "staged", "untracked"]

blobFields :: [String]
blobFields = ["sha", "path", "mode"]

treeObjectFields :: [String]
treeObjectFields = ["sha"]

diffFields :: [String]
diffFields = ["sha", "path", "parent-sha"]

-- | Hunk frames carry the whole hunk body in @content@ and the owning
-- commit's author\/date\/message; still no @sha@ of their own, so
-- grep\/pickaxe cannot follow.
hunkFields :: [String]
hunkFields = ["path", "start-line", "end-line", "content", "commit-sha", "author", "date", "message"]

lineFields :: [String]
lineFields = ["sha", "path", "line-number", "content", "commit-sha"]

-- | Diff-line frames also carry the owning commit's metadata.
diffLineFields :: [String]
diffLineFields = ["path", "line-number", "content", "sign", "commit-sha", "author", "date", "message"]

-- | The field-set each source's frames start the pipeline with.
sourceFields :: Source -> [String]
sourceFields (SCommits _) = commitFields
sourceFields (SRef _)     = commitFields  -- resolves to one commit
sourceFields SBranches    = refFields
sourceFields STags        = refFields
sourceFields SRefs        = refFields
sourceFields SWorktrees   = worktreeFields
sourceFields SBlobs       = blobFields

-- | The field a morphism's input shape must carry (its domain).
morphismRequires :: Morphism -> String
morphismRequires m = case m of
  MParent        -> "parents-count"
  MParentIdx _   -> "parents-count"
  MParentStar    -> "parents-count"
  MParentPlus    -> "parents-count"
  MParentAdjoint -> "sha"
  MTree          -> "tree"
  MTreeEntries _ -> "sha"
  MDiff _        -> "sha"
  MDiffHunks     -> "sha"
  MDiffLines     -> "sha"
  MHistory       -> "path"
  MCommit        -> "commit-sha"

-- | The field-set a morphism's output frames carry (its codomain), which
-- becomes the current field-set for the rest of the pipeline.
morphismYields :: Morphism -> [String]
morphismYields m = case m of
  MParent        -> commitFields
  MParentIdx _   -> commitFields
  MParentStar    -> commitFields
  MParentPlus    -> commitFields
  MParentAdjoint -> commitFields
  MTree          -> treeObjectFields
  MTreeEntries _ -> blobFields
  MDiff _        -> diffFields
  MDiffHunks     -> hunkFields
  MDiffLines     -> diffLineFields
  MHistory       -> commitFields
  MCommit        -> commitFields

-- | Surface forms a morphism path is built from, parsed by greedy
-- longest-match at each position.  A path may be written bare (@parent@,
-- @tree.entries[Blob]@, @parent.tree@) or with the historical leading dot
-- (@.parent@, ...) — both are normalized to the dotted form before
-- matching.  A segment boundary must be a @.@ or the end of the path.
-- Errors on the first unrecognizable segment, naming it and the full path.
parseMorphismPath :: String -> Either String [Morphism]
parseMorphismPath raw = go path []
 where
  path = if "." `isPrefixOf` raw then raw else '.' : raw
  go "" acc = Right (reverse acc)
  go rest acc =
    case longest (matchesAt rest) of
      Just (consumed, m) -> go (drop consumed rest) (m : acc)
      Nothing ->
        Left ("gitq: unknown morphism '" ++ rest ++ "'"
              ++ (if length rest < length path then " (in '" ++ raw ++ "')" else ""))
  longest [] = Nothing
  longest xs = Just (foldr1 (\a b -> if fst a >= fst b then a else b) xs)
  -- every form that matches at the head of REST with a valid boundary
  matchesAt rest =
    [ (length form, m)
    | (form, m) <- literalForms
    , form `isPrefixOf` rest
    , boundaryAt (drop (length form) rest)
    ] ++ parentIdxAt rest
  boundaryAt ""      = True
  boundaryAt ('.':_) = True
  boundaryAt _       = False
  parentIdxAt rest =
    case stripPrefix ".parent[" rest of
      Just after ->
        let (digits, rest') = span isDigit after
        in case (digits, rest') of
             (_:_, ']':rest'') | boundaryAt rest'' ->
               [(length ".parent[" + length digits + 1, MParentIdx (read digits))]
             _ -> []
      Nothing -> []
  literalForms =
    [ (".parent*",           MParentStar)
    , (".parent+",           MParentPlus)
    , (".parent\x2020",      MParentAdjoint)
    , (".parent",            MParent)
    , (".tree.entries[Blob]", MTreeEntries (Just FBlob))
    , (".tree.entries[Tree]", MTreeEntries (Just FTree))
    , (".tree.entries",      MTreeEntries Nothing)
    , (".tree.blobs",        MTreeEntries (Just FBlob))
    , (".tree.subtrees",     MTreeEntries (Just FTree))
    , (".tree",              MTree)
    , (".entries[Blob]",     MTreeEntries (Just FBlob))
    , (".entries[Tree]",     MTreeEntries (Just FTree))
    , (".entries",           MTreeEntries Nothing)
    , (".diff.hunks",        MDiffHunks)
    , (".diff.lines",        MDiffLines)
    , (".diff",              MDiff Nothing)
    , (".history",           MHistory)
    , (".commit",            MCommit)
    ]

-- | The terminal registry's names, in registry order.  The completion
-- candidate list derives from this, so completion can never offer a
-- terminal the parser rejects.
terminalNames :: [String]
terminalNames =
  [ "show", "copy", "insert", "count", "remove", "delete", "stage"
  , "branch-off", "amend", "squash", "reword", "commit", "mark", "worktree"
  ]

-- Completion candidate sets ---------------------------------------------

completeSourceKeywords :: [String]
completeSourceKeywords = ["commits", "branches", "tags", "refs", "worktrees", "blobs", "HEAD"]

-- | Canonical single-morphism forms offered after @via@; compositions
-- (@parent.tree@, ...) are typed by hand and parsed generically.
completeMorphisms :: [String]
completeMorphisms =
  [ "parent", "parent*", "parent+", "parent\x2020", "tree", "tree.blobs"
  , "tree.subtrees", "tree.entries", "tree.entries[Blob]"
  , "tree.entries[Tree]", "diff", "diff.hunks", "diff.lines"
  , "history", "commit"
  ]

completeWhereOperators :: [String]
completeWhereOperators = operatorNames

completeTerminals :: [String]
completeTerminals = map ('/' :) terminalNames

completeDateWithinExamples :: [String]
completeDateWithinExamples =
  ["1 day", "3 days", "1 week", "2 weeks", "1 month", "3 months", "6 months", "1 year"]

-- | Category label of a completion candidate, reflecting gitq's own
-- grammar: source, step, morphism, field, operator, or terminal.  A
-- leading @-@ (sort negation) is ignored.  Checked in the same order as
-- the Emacs Lisp original's gitq--token-kind, so @path@ (both a step
-- keyword and a field) classifies as a step.
tokenKind :: String -> Maybe String
tokenKind cand
  | key == "in" || key `elem` completeSourceKeywords = Just "source"
  | key `elem` stepKeywords                          = Just "step"
  | key `elem` completeMorphisms                     = Just "morphism"
  | key `elem` fieldNames                            = Just "field"
  | key `elem` completeWhereOperators                = Just "operator"
  | key `elem` completeTerminals                     = Just "terminal"
  | otherwise                                        = Nothing
 where
  key = case cand of ('-' : rest@(_ : _)) -> rest; _ -> cand

-- | Short description shown as a completion annotation for a token.
describeToken :: String -> Maybe String
describeToken tok = lookup tok table
 where
  table =
    [ -- sources
      ("commits",   "commits reachable from HEAD")
    , ("branches",  "local branch refs")
    , ("tags",      "tag refs")
    , ("refs",      "all refs (branches, tags, ...)")
    , ("worktrees", "linked worktrees")
    , ("blobs",     "blob/tree entries under HEAD's tree")
    , ("HEAD",      "the current commit")
    , ("in",        "restrict commits to a revision range")
      -- steps
    , ("via",     "traverse a morphism (parent, tree, diff, ...)")
    , ("where",   "filter by field conditions")
    , ("grep",    "search blob/commit content for a pattern")
    , ("pickaxe", "filter commits whose diff adds/removes a pattern")
    , ("path",    "path glob step, or the file-path field")
    , ("pick",    "project onto specific fields")
    , ("take",    "keep the first N results")
    , ("skip",    "drop the first N results")
    , ("first",   "keep only the first result")
    , ("last",    "keep only the last result")
    , ("sort",    "sort by field (prefix with - for descending)")
      -- morphisms
    , ("parent",             "first parent commit")
    , ("parent*",            "all reachable ancestors, inclusive")
    , ("parent+",            "all reachable ancestors, exclusive")
    , ("parent\x2020",       "children-of: commits whose parent is in the result")
    , ("tree",               "the commit's tree, or (as a field) its SHA")
    , ("tree.blobs",         "blob entries in the tree")
    , ("tree.subtrees",      "subtree entries in the tree")
    , ("tree.entries",       "all tree entries")
    , ("tree.entries[Blob]", "blob entries only")
    , ("tree.entries[Tree]", "subtree entries only")
    , ("diff",               "paths changed vs. parent (or REF)")
    , ("diff.hunks",         "line ranges changed vs. parent")
    , ("diff.lines",         "actual +/- diff lines vs. parent, with content")
    , ("history",            "commits that touched this path")
    , ("commit",             "resolve to the referenced commit")
      -- fields
    , ("sha",           "commit SHA")
    , ("author",        "author name")
    , ("email",         "author email")
    , ("date",          "commit date")
    , ("message",       "commit message")
    , ("name",          "ref/branch name")
    , ("branch",        "worktree's branch")
    , ("parents-count", "number of parents")
    , ("modified",      "has modified/unstaged changes")
    , ("staged",        "has staged changes")
    , ("untracked",     "has untracked files")
    , ("reftype",       "ref kind (branch or tag)")
    , ("detached",      "worktree HEAD is detached")
    , ("mode",          "tree entry file mode")
    , ("parent-sha",    "the ref/SHA a diff was compared against")
    , ("commit-sha",    "commit a hunk/grep line belongs to")
    , ("start-line",    "hunk's first changed line")
    , ("end-line",      "hunk's last changed line")
    , ("line-number",   "grep/diff-line match's line number")
    , ("content",       "grep/diff-line match's line content")
    , ("sign",          "\"+\" (added) or \"-\" (removed) diff line")
      -- operators
    , ("==",     "equals")
    , ("!=",     "not equals")
    , (">",      "greater than")
    , ("<",      "less than")
    , (">=",     "greater or equal")
    , ("<=",     "less or equal")
    , ("regex",  "regex match (POSIX ERE)")
    , ("after",  "date is after value")
    , ("before", "date is before value")
    , ("within", "date is within \"N day/week/month/year(s)\"")
    , ("is",     "boolean flag is true")
      -- terminals
    , ("/show",       "print/display results")
    , ("/copy",       "copy the SHA of the first result")
    , ("/insert",     "insert the SHA of the first result")
    , ("/count",      "show the result count")
    , ("/branch-off", "create a branch from the first result")
    , ("/amend",      "amend HEAD with the first result")
    , ("/squash",     "squash results into one commit")
    , ("/reword",     "reword the first result's commit message")
    , ("/remove",     "remove the first result's commit")
    , ("/delete",     "delete the first result's commit")
    , ("/commit",     "create a commit")
    , ("/stage",      "stage modified files")
    , ("/mark",       "attach a git note label")
    , ("/worktree",   "add a worktree")
    , ("no-edit",     "reuse HEAD's existing commit message")
    ]
