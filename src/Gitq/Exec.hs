{-# LANGUAGE OverloadedStrings #-}
-- | Pipeline execution: sources, morphisms (via), and relational steps.
--
-- Each morphism maps one frame to a /list/ of frames; execution lifts that
-- map pointwise over the incoming list and appends the results — Kleisli
-- composition over the list monad, which is why chaining needs no plumbing.
--
-- Query-side strings (field names, patterns, values from the parser) are
-- packed to 'Text' once per step or condition, never per frame.
module Gitq.Exec
  ( execPipeline
  , execSource
  , execStep
  , evalCondition
  , parseDiffHunks
  , parseDiffLines
  ) where

import Data.List (sortBy)
import qualified Data.Map.Strict as M
import Data.Maybe (fromMaybe, mapMaybe)
import Data.Ord (comparing)
import qualified Data.Set as S
import qualified Data.Text as T
import Data.Text (Text)
import Data.Time (UTCTime, defaultTimeLocale, diffUTCTime, getCurrentTime, parseTimeM)
import Text.Regex.TDFA (Regex, makeRegex, match, matchTest)
import Text.Regex.TDFA.Text ()
import Gitq.AST
import Gitq.Frame
import Gitq.Git
import Gitq.Native (nativeCommits)
import Gitq.Registry (FieldType (..), fieldType)

-- | Execute a parsed pipeline's source and steps, returning the final
-- frames and the (unapplied) terminal.  The terminal is identified but
-- never run here — callers decide whether to apply it for real or ignore
-- it for a read-only preview.
execPipeline :: Pipeline -> IO ([Frame], Maybe Terminal)
execPipeline (Pipeline src steps term) = do
  frames0 <- execSource src
  frames <- foldM' execStep frames0 steps
  pure (frames, term)
 where
  foldM' _ acc [] = pure acc
  foldM' f acc (x : xs) = f acc x >>= \acc' -> foldM' f acc' xs

execSource :: Source -> IO [Frame]
execSource src = case src of
  SCommits range -> fetchCommits range
  SRef ref -> maybe [] (: []) <$> fetchCommit ref
  SBranches -> fetchBranches
  STags -> fetchTags
  SRefs -> fetchRefs
  SWorktrees -> fetchWorktrees
  SBlobs -> do
    mtree <- runGitString ["rev-parse", "HEAD^{tree}"]
    case mtree of
      Just tree -> fetchBlobsAt tree Nothing Nothing
      Nothing   -> pure []

execStep :: [Frame] -> Step -> IO [Frame]
execStep frames step = case step of
  StVia m         -> execVia frames m
  StWhere conds   -> do
    -- compile each condition once (field name packed, regex pattern
    -- compiled) — never per frame
    let preds = map compileCond conds
    results <- mapM (\f -> and <$> mapM ($ f) preds) frames
    pure [f | (f, ok) <- zip frames results, ok]
  StGrep pat re   -> execGrep frames pat re
  StPickaxe p re  -> execPickaxe frames p re
  StPath pat      ->
    let patT = T.pack pat
    in pure (filter (matchPath patT) frames)
  StPick fields   ->
    let fieldsT = map T.pack fields
    in pure (map (project fieldsT) frames)
  StTake n        -> pure (take n frames)
  StSkip n        -> pure (drop n frames)
  StFirst         -> pure (take 1 frames)
  StLast          -> pure (case frames of [] -> []; fs -> [last fs])
  StSort field d  -> pure (execSort frames field d)
 where
  matchPath pat f = case frameField f "path" of
    Just (VStr p) -> pathMatches pat p
    _             -> False

-- | One condition as a frame predicate, with per-condition work (field
-- name packing, regex compilation) hoisted out of the per-frame path.
-- The compiled regex is a lazy thunk captured by the closure: forced on
-- the first frame, shared by the rest.  (The parser already validated the
-- pattern, so forcing it cannot error.)
compileCond :: Cond -> (Frame -> IO Bool)
compileCond (Cond field OpRegex (VStr v)) =
  let re = makeRegex v :: Regex
      fieldT = T.pack field
  in \f -> pure $ case frameField f fieldT of
       Just (VStr a) -> match re a
       _             -> False
compileCond c@(Cond field _ _) =
  let fieldT = T.pack field
  in \f -> evalCondition f fieldT c

-- Morphism executors -----------------------------------------------------

execVia :: [Frame] -> Morphism -> IO [Frame]
execVia frames m = case m of
  -- Parent/commit lookups are batched: collect the wanted SHAs first, fetch
  -- them all in one git process ('fetchCommitMap'), then reassemble in the
  -- original order (duplicates included, as the one-process-per-SHA version
  -- produced them).
  MParent        -> batchLookup (concatMap frameParents frames)
  MParentIdx i   -> batchLookup [p | f <- frames, (p : _) <- [drop i (frameParents f)]]
  MParentStar    -> traverseParentsStar frames False
  MParentPlus    -> traverseParentsStar frames True
  MParentAdjoint -> viaParentAdjoint frames
  MTree          -> pure (mapMaybe treeOf frames)
  MTreeEntries fl -> concatMapM (entriesOf fl) frames
  MDiff ref      -> concatMapM (diffOf ref) frames
  MDiffHunks     -> concatMapM hunksOf frames
  MDiffLines     -> concatMapM diffLinesOf frames
  MHistory       -> viaHistory frames
  MCommit        -> batchLookup [s | f <- frames, Just s <- [strField f "commit-sha"]]
 where
  concatMapM f = fmap concat . mapM f

  batchLookup shas = do
    cmap <- commitMapFor shas
    pure [c | sha <- shas, Just c <- [M.lookup sha cmap]]

  treeOf f = case frameField f "tree" of
    Just (VStr t) -> Just (frame "tree" [("sha", VStr t)])
    _             -> Nothing

  entriesOf fl f = do
    let mtree = case (frameType f, frameField f "tree") of
          ("commit", Just (VStr t)) -> Just t
          _ -> case frameField f "sha" of
                 Just (VStr s) -> Just s
                 _             -> Nothing
    case mtree of
      Just tree -> fetchBlobsAt tree Nothing fl
      Nothing   -> pure []

  diffOf ref f = case strField f "sha" of
    Nothing -> pure []
    Just sha -> do
      -- A root commit (no parents) has no "sha^" to diff against —
      -- --root diffs it against the empty tree instead of erroring.
      let noParent = ref == Nothing && null (frameParents f)
          other = if noParent
                    then Nothing
                    else Just (fromMaybe (T.unpack sha ++ "^") ref)
      paths <- case other of
        Nothing -> runGit ["diff-tree", "--root", "-r", "--name-only", "--no-commit-id", T.unpack sha]
        Just o  -> runGit ["diff-tree", "-r", "--name-only", "--no-commit-id", o, T.unpack sha]
      pure [ frame "diff"
               ([("sha", VStr sha), ("path", VStr p)]
                ++ [("parent-sha", VStr (T.pack o)) | Just o <- [other]])
           | p <- paths ]

  hunksOf f = case strField f "sha" of
    Nothing -> pure []
    Just sha -> do
      ls <- runGit ["diff-tree", "-p", "--no-commit-id", "-r", T.unpack sha ++ "^", T.unpack sha]
      pure (parseDiffHunks ls sha (commitMeta f))

  diffLinesOf f = case strField f "sha" of
    Nothing -> pure []
    Just sha -> do
      let root = frameType f == "commit" && null (frameParents f)
          args = if root
                   then ["diff-tree", "--root", "-p", "--no-commit-id", "-r", T.unpack sha]
                   else ["diff-tree", "-p", "--no-commit-id", "-r", T.unpack sha ++ "^", T.unpack sha]
      ls <- runGit args
      pure (parseDiffLines ls sha (commitMeta f))

  -- the owning commit's metadata rides along on its hunk/diff-line
  -- frames, so they can be filtered, sorted, and displayed by author,
  -- date, or subject without a lookup
  commitMeta f =
    [ (k, v) | k <- ["author", "date", "message"], Just v <- [frameField f k] ]

  strField f k = case frameField f k of
    Just (VStr s) -> Just s
    _             -> Nothing

-- | History morphism: the commits that touched each frame's path.  One
-- @git log --follow@ per path is inherent (--follow is single-path), but
-- resolving the resulting SHAs to frames is one batched fetch for all
-- paths together.
viaHistory :: [Frame] -> IO [Frame]
viaHistory frames = do
  pathShas <- mapM shasOf frames
  cmap <- commitMapFor (concatMap snd pathShas)
  pure [ c { frameAttrs = M.insert "path" (VStr path) (frameAttrs c) }
       | (path, shas) <- pathShas
       , sha <- shas
       , Just c <- [M.lookup sha cmap]
       ]
 where
  shasOf f = case frameField f "path" of
    Just (VStr path) -> do
      shas <- runGit ["log", "--follow", "--format=%H", "--", T.unpack path]
      pure (path, shas)
    _ -> pure ("", [])

-- | SHA-keyed commit map for a batch of full SHAs: the in-process native
-- backend when linked and working, else one subprocess ('fetchCommitMap').
commitMapFor :: [Text] -> IO (M.Map Text Frame)
commitMapFor shas = do
  let uniq = S.toList (S.fromList shas)
  m <- nativeCommits False uniq
  case m of
    Just fs -> pure (mapBySha fs)
    Nothing -> fetchCommitMap shas

mapBySha :: [Frame] -> M.Map Text Frame
mapBySha fs =
  M.fromList [(sha, f) | f <- fs, Just (VStr sha) <- [frameField f "sha"]]

-- | Walk parent links from the given frames, returning all reachable
-- commits in discovery order.  When @excludeStart@ ('parent+'), the start
-- frames themselves are excluded.
--
-- The reachable closure is materialized up front — in process via the
-- native backend when available, else in two git processes (@rev-list
-- --stdin@ for the SHA set, one batched log for their frames); the walk
-- itself then replays the original one-process-per-commit algorithm purely
-- in memory, preserving its discovery order exactly.
traverseParentsStar :: [Frame] -> Bool -> IO [Frame]
traverseParentsStar frames excludeStart = do
  let startShas = [s | f <- frames, Just (VStr s) <- [frameField f "sha"]]
  if null startShas
    then pure []
    else do
      mNative <- nativeCommits True startShas
      cmap <- case mNative of
        Just fs -> pure (mapBySha fs)
        Nothing -> do
          reachable <- runGitStdin ["rev-list", "--stdin"] (T.unlines startShas)
          fetchCommitMap reachable
      pure (reverse (goStarts cmap startShas S.empty []))
 where
  goStarts _ [] _ acc = acc
  goStarts cmap (start : rest) visited acc =
    let (visited', acc') = walk cmap start [start] visited acc
    in goStarts cmap rest visited' acc'
  walk _ _ [] visited acc = (visited, acc)
  walk cmap startSha (sha : queue) visited acc
    | sha `S.member` visited = walk cmap startSha queue visited acc
    | otherwise =
        let visited' = S.insert sha visited
        in case M.lookup sha cmap of
             Nothing -> walk cmap startSha queue visited' acc
             Just c ->
               let acc' = if excludeStart && sha == startSha then acc else c : acc
                   unvisited = [p | p <- frameParents c, not (p `S.member` visited')]
               -- parents are prepended (stack order), matching the original
               in walk cmap startSha (reverse unvisited ++ queue) visited' acc'

-- | Adjoint of parent: the commits whose parent is in the given frames.
viaParentAdjoint :: [Frame] -> IO [Frame]
viaParentAdjoint frames = do
  let targets = S.fromList [s | f <- frames, Just (VStr s) <- [frameField f "sha"]]
  allCommits <- fetchCommits Nothing
  pure [c | c <- allCommits, any (`S.member` targets) (frameParents c)]

-- Diff parsing ------------------------------------------------------------

-- | Parse unified diff lines into hunk frames: line ranges plus the
-- hunk's full body text (context and ±lines, prefixes included) in
-- @content@, so whole hunks can be content-filtered and displayed.
-- EXTRA attrs (the owning commit's metadata) are attached to each frame.
parseDiffHunks :: [Text] -> Text -> [(Text, Value)] -> [Frame]
parseDiffHunks diffLines commitSha extra = go diffLines Nothing Nothing []
 where
  go [] _ open acc = reverse (flush open acc)
  go (l : rest) curPath open acc
    | Just p <- diffHeaderPath l = go rest (Just p) Nothing (flush open acc)
    | Just path <- curPath
    , Just (start, count) <- hunkHeader l =
        go rest curPath (Just (path, start, count, [])) (flush open acc)
    | Just (path, start, count, body) <- open =
        go rest curPath (Just (path, start, count, l : body)) acc
    | otherwise = go rest curPath open acc
  flush Nothing acc = acc
  flush (Just (path, start, count, body)) acc =
    frame "hunk"
      ([ ("path", VStr path)
       , ("start-line", VNum start)
       , ("end-line", VNum (start + max 0 (count - 1)))
       , ("content", VStr (T.unlines (reverse body)))
       , ("commit-sha", VStr commitSha)
       ] ++ extra)
    : acc

-- | Parse unified diff lines into added/removed diff-line frames.
-- line-number is the new-file line for additions and the old-file line for
-- deletions.  The +++\/--- file headers can't be mistaken for changed
-- lines because they appear before any @@ hunk header, when the line
-- cursors are still unset.
parseDiffLines :: [Text] -> Text -> [(Text, Value)] -> [Frame]
parseDiffLines diffLines commitSha extra = go diffLines Nothing Nothing []
 where
  go [] _ _ acc = reverse acc
  go (l : rest) curPath cursors acc
    | Just p <- diffHeaderPath l = go rest (Just p) Nothing acc
    | Just path <- curPath
    , Just (oldN, newN) <- lineHunkHeader l =
        go rest (Just path) (Just (oldN, newN)) acc
    | Just path <- curPath
    , Just (oldN, newN) <- cursors =
        case T.uncons l of
          Just ('+', content) ->
            go rest curPath (Just (oldN, newN + 1))
               (mkLine path "+" newN content : acc)
          Just ('-', content) ->
            go rest curPath (Just (oldN + 1, newN))
               (mkLine path "-" oldN content : acc)
          Just ('\\', _) -> go rest curPath cursors acc   -- "\ No newline ..."
          _ -> go rest curPath (Just (oldN + 1, newN + 1)) acc
    | otherwise = go rest curPath cursors acc
  mkLine path sign n content =
    frame "diff-line"
      ([ ("path", VStr path), ("sign", VStr sign)
       , ("line-number", VNum n), ("content", VStr content)
       , ("commit-sha", VStr commitSha)
       ] ++ extra)

-- | @diff --git a\/... b\/PATH@ → PATH (greedy: the last @ b/@ splits, as
-- the original regex's greedy @.+@ did, so paths containing spaces work).
diffHeaderPath :: Text -> Maybe Text
diffHeaderPath l = do
  rest <- T.stripPrefix "diff --git a/" l
  let (before, after) = T.breakOnEnd " b/" rest
  if T.null before || T.null after then Nothing else Just after

-- | @\@\@ -a,b +START[,COUNT] \@\@@ → (START, COUNT defaulting to 1)
hunkHeader :: Text -> Maybe (Int, Int)
hunkHeader l = do
  (_, start, rest) <- newSideOfHunkHeader l
  case T.uncons rest of
    Just (',', r) -> do
      (count, r') <- decimal r
      if " @@" `T.isPrefixOf` r' then Just (start, count) else Nothing
    Just (' ', r) | "@@" `T.isPrefixOf` r -> Just (start, 1)
    _ -> Nothing

-- | @\@\@ -OLD[,n] +NEW[...]@ → (OLD, NEW)
lineHunkHeader :: Text -> Maybe (Int, Int)
lineHunkHeader l = do
  (old, new, _) <- newSideOfHunkHeader l
  Just (old, new)

-- | Shared header parse: OLD start, NEW start, and the text after NEW.
newSideOfHunkHeader :: Text -> Maybe (Int, Int, Text)
newSideOfHunkHeader l = do
  r0 <- T.stripPrefix "@@ -" l
  (old, r1) <- decimal r0
  let r2 = case T.uncons r1 of
        Just (',', r) -> snd (T.span (`elem` ("0123456789" :: String)) r)
        _             -> r1
  r3 <- T.stripPrefix " +" r2
  (new, r4) <- decimal r3
  Just (old, new, r4)

decimal :: Text -> Maybe (Int, Text)
decimal t =
  let (ds, rest) = T.span (`elem` ("0123456789" :: String)) t
  in if T.null ds then Nothing else Just (read (T.unpack ds), rest)

-- Relational steps ---------------------------------------------------------

-- | Evaluate one where-condition against a frame.  Date comparisons and
-- @within@ need the clock, hence IO.  The field name is passed pre-packed
-- by 'compileCond' so it isn't re-packed per frame.
evalCondition :: Frame -> Text -> Cond -> IO Bool
evalCondition f fieldT (Cond _ op value) = do
  let actual = frameField f fieldT
  case op of
    OpEq -> pure (actual == Just value)
    OpNe -> pure (actual /= Just value)
    OpGt -> pure (numCmp (>) actual)
    OpLt -> pure (numCmp (<) actual)
    OpGe -> pure (numCmp (>=) actual)
    OpLe -> pure (numCmp (<=) actual)
    OpContains -> pure $ case (actual, value) of
      (Just (VStr a), VStr v) -> v `T.isInfixOf` a
      _                       -> False
    OpRegex -> pure $ case (actual, value) of
      (Just (VStr a), VStr v) -> matchTest (makeRegex v :: Regex) a
      _                       -> False
    OpAfter  -> pure (dateCmp (>) actual)
    OpBefore -> pure (dateCmp (<) actual)
    OpWithin -> case (actual, value) of
      (Just (VStr a), VStr period) -> dateWithin a period
      _                            -> pure False
    OpIs -> pure $ case value of
      VBool True -> truthy actual
      _          -> actual == Just value
 where
  numCmp cmp actual = case (actual, value) of
    (Just (VNum a), VNum v) -> a `cmp` v
    _                       -> False
  dateCmp cmp actual = case (actual, value) of
    (Just (VStr a), VStr v) ->
      case (parseDate a, parseDate v) of
        (Just ta, Just tv) -> ta `cmp` tv
        _                  -> False
    _ -> False

-- | Parse a date string leniently: git's ISO %ai format, ISO 8601, or a
-- bare year\/month\/day prefix.
parseDate :: Text -> Maybe UTCTime
parseDate t = firstJust
  [ p "%Y-%m-%d %H:%M:%S %z", p "%Y-%m-%dT%H:%M:%S%z", p "%Y-%m-%d %H:%M:%S"
  , p "%Y-%m-%d %H:%M", p "%Y-%m-%d", p "%Y-%m", p "%Y"
  ]
 where
  s = T.unpack t
  p fmt = parseTimeM True defaultTimeLocale fmt s
  firstJust xs = case [x | Just x <- xs] of (x : _) -> Just x; [] -> Nothing

-- | Does the date fall within "N day\/week\/month\/year(s)" of now?
dateWithin :: Text -> Text -> IO Bool
dateWithin dateStr period =
  case parsePeriod (T.unpack period) of
    Nothing -> pure False
    Just secs ->
      case parseDate dateStr of
        Nothing -> pure False
        Just t -> do
          now <- getCurrentTime
          pure (realToFrac (diffUTCTime now t) <= (secs :: Double))
 where
  parsePeriod str = case words str of
    (nStr : unit : _) | all (`elem` ("0123456789" :: String)) nStr, not (null nStr) ->
      (* read nStr) <$> unitSecs (stripS unit)
    _ -> Nothing
  stripS u = if not (null u) && last u == 's' then init u else u
  unitSecs u = lookup u [("day", 86400), ("week", 604800), ("month", 2592000), ("year", 31536000)]

execGrep :: [Frame] -> String -> Bool -> IO [Frame]
execGrep frames pat regex = fmap concat (mapM grepOne frames)
 where
  grepOne f = case frameField f "sha" of
    Just (VStr sha) -> do
      ls <- runGit ["grep", "-n", "--no-color", if regex then "-E" else "-F", pat, T.unpack sha]
      pure (mapMaybe (parseLine sha) ls)
    _ -> pure []
  -- "sha:path:line:content" — path may not contain ':', content may
  parseLine sha l = do
    (_, r1) <- breakColon l
    (path, r2) <- breakColon r1
    (nStr, content) <- breakColon r2
    if not (T.null nStr) && T.all (`elem` ("0123456789" :: String)) nStr
      then Just (frame "line"
                  [ ("sha", VStr sha), ("path", VStr path)
                  , ("line-number", VNum (read (T.unpack nStr)))
                  , ("content", VStr content)
                  , ("commit-sha", VStr sha)
                  ])
      else Nothing
  breakColon s = case T.break (== ':') s of
    (a, rest) | Just r <- T.stripPrefix ":" rest -> Just (a, r)
    _ -> Nothing

execPickaxe :: [Frame] -> String -> Bool -> IO [Frame]
execPickaxe frames pat regex = do
  let shas = [s | f <- frames, Just (VStr s) <- [frameField f "sha"]]
  if null shas
    then pure []
    else do
      -- SHAs go through --stdin, never argv: a whole-history pickaxe (81k
      -- SHAs on git/git) exceeds the OS argument-list limit as arguments
      hits <- runGitStdin
                ["log", if regex then "-G" else "-S", pat, "--format=%H", "--no-walk", "--stdin"]
                (T.unlines shas)
      let hitSet = S.fromList hits
      pure [ f | f <- frames
           , Just (VStr s) <- [frameField f "sha"], s `S.member` hitSet ]

-- | Project each frame to only the listed fields.
project :: [Text] -> Frame -> Frame
project fields f = Frame
  { frameType = "projection"
  , frameParents = []
  , frameAttrs = M.fromList
      [ (field, v) | field <- fields, Just v <- [frameField f field] ]
  }

-- | Sort by a field, numeric or lexical per the field's scalar type.
execSort :: [Frame] -> String -> Bool -> [Frame]
execSort frames field desc = sortBy cmp frames
 where
  fieldT = T.pack field
  numeric = fieldType field == TNumber
  cmp a b =
    let va = frameField a fieldT
        vb = frameField b fieldT
        ord = if numeric
                then comparing asNum va vb
                else comparing asStr va vb
    in if desc then flipOrd ord else ord
  asNum (Just (VNum n)) = n
  asNum _               = 0
  asStr (Just (VStr s)) = s
  asStr _               = ""
  flipOrd LT = GT
  flipOrd GT = LT
  flipOrd EQ = EQ
