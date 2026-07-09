-- | Pipeline execution: sources, morphisms (via), and relational steps.
--
-- Each morphism maps one frame to a /list/ of frames; execution lifts that
-- map pointwise over the incoming list and appends the results — Kleisli
-- composition over the list monad, which is why chaining needs no plumbing.
module Gitq.Exec
  ( execPipeline
  , execSource
  , execStep
  , evalCondition
  , parseDiffHunks
  , parseDiffLines
  ) where

import Data.List (isInfixOf, sortBy)
import qualified Data.Map.Strict as M
import Data.Maybe (fromMaybe, mapMaybe)
import Data.Ord (comparing)
import qualified Data.Set as S
import Data.Time (UTCTime, defaultTimeLocale, diffUTCTime, getCurrentTime, parseTimeM)
import Text.Regex.TDFA (Regex, makeRegex, match, (=~))
import Gitq.AST
import Gitq.Frame
import Gitq.Git
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
    -- compile each condition once (a regex condition compiles its pattern
    -- here, not per frame — matching 10k frames used to recompile the
    -- regex 10k times)
    let preds = map compileCond conds
    results <- mapM (\f -> and <$> mapM ($ f) preds) frames
    pure [f | (f, ok) <- zip frames results, ok]
  StGrep pat re   -> execGrep frames pat re
  StPickaxe p re  -> execPickaxe frames p re
  StPath pat      -> pure (filter (matchPath pat) frames)
  StPick fields   -> pure (map (project fields) frames)
  StTake n        -> pure (take n frames)
  StSkip n        -> pure (drop n frames)
  StFirst         -> pure (take 1 frames)
  StLast          -> pure (case frames of [] -> []; fs -> [last fs])
  StSort field d  -> pure (execSort frames field d)
 where
  matchPath pat f = case frameField f "path" of
    Just (VStr p) -> pathMatches pat p
    _             -> False

-- | One condition as a frame predicate, with per-condition work (regex
-- compilation) hoisted out of the per-frame path.  The compiled regex is a
-- lazy thunk captured by the closure: forced on the first frame, shared by
-- the rest.  (The parser already validated the pattern, so forcing it
-- cannot error.)
compileCond :: Cond -> (Frame -> IO Bool)
compileCond (Cond field OpRegex (VStr v)) =
  let re = makeRegex v :: Regex
  in \f -> pure $ case frameField f field of
       Just (VStr a) -> match re a
       _             -> False
compileCond c = \f -> evalCondition f c

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
    cmap <- fetchCommitMap shas
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

  strField f k = case frameField f k of
    Just (VStr s) -> Just s
    _             -> Nothing

  diffOf ref f = case strField f "sha" of
    Nothing -> pure []
    Just sha -> do
      -- A root commit (no parents) has no "sha^" to diff against —
      -- --root diffs it against the empty tree instead of erroring.
      let noParent = ref == Nothing && null (frameParents f)
          other = if noParent then Nothing else Just (fromMaybe (sha ++ "^") ref)
      paths <- case other of
        Nothing -> runGit ["diff-tree", "--root", "-r", "--name-only", "--no-commit-id", sha]
        Just o  -> runGit ["diff-tree", "-r", "--name-only", "--no-commit-id", o, sha]
      pure [ frame "diff"
               ([("sha", VStr sha), ("path", VStr p)]
                ++ [("parent-sha", VStr o) | Just o <- [other]])
           | p <- paths ]

  hunksOf f = case strField f "sha" of
    Nothing -> pure []
    Just sha -> do
      ls <- runGit ["diff-tree", "-p", "--no-commit-id", "-r", sha ++ "^", sha]
      pure (parseDiffHunks (unlines ls) sha)

  diffLinesOf f = case strField f "sha" of
    Nothing -> pure []
    Just sha -> do
      let root = frameType f == "commit" && null (frameParents f)
          args = if root
                   then ["diff-tree", "--root", "-p", "--no-commit-id", "-r", sha]
                   else ["diff-tree", "-p", "--no-commit-id", "-r", sha ++ "^", sha]
      ls <- runGit args
      pure (parseDiffLines (unlines ls) sha)

-- | History morphism: the commits that touched each frame's path.  One
-- @git log --follow@ per path is inherent (--follow is single-path), but
-- resolving the resulting SHAs to frames is one batched fetch for all
-- paths together.
viaHistory :: [Frame] -> IO [Frame]
viaHistory frames = do
  pathShas <- mapM shasOf frames
  cmap <- fetchCommitMap (concatMap snd pathShas)
  pure [ c { frameAttrs = M.insert "path" (VStr path) (frameAttrs c) }
       | (path, shas) <- pathShas
       , sha <- shas
       , Just c <- [M.lookup sha cmap]
       ]
 where
  shasOf f = case frameField f "path" of
    Just (VStr path) -> do
      shas <- runGit ["log", "--follow", "--format=%H", "--", path]
      pure (path, shas)
    _ -> pure ("", [])

-- | Walk parent links from the given frames, returning all reachable
-- commits in discovery order.  When @excludeStart@ ('parent+'), the start
-- frames themselves are excluded.
--
-- The reachable closure is materialized up front in two git processes
-- (@rev-list --stdin@ for the SHA set, one batched log for their frames);
-- the walk itself then replays the original one-process-per-commit
-- algorithm purely in memory, preserving its discovery order exactly.
traverseParentsStar :: [Frame] -> Bool -> IO [Frame]
traverseParentsStar frames excludeStart = do
  let startShas = [s | f <- frames, Just (VStr s) <- [frameField f "sha"]]
  if null startShas
    then pure []
    else do
      reachable <- runGitStdin ["rev-list", "--stdin"] (unlines startShas)
      cmap <- fetchCommitMap reachable
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

-- | Parse unified diff text into hunk frames (line ranges only).
parseDiffHunks :: String -> String -> [Frame]
parseDiffHunks diffText commitSha = go (lines diffText) Nothing []
 where
  go [] _ acc = reverse acc
  go (l : rest) curPath acc
    | Just p <- diffHeaderPath l = go rest (Just p) acc
    | Just path <- curPath
    , Just (start, count) <- hunkHeader l =
        let hunk = frame "hunk"
              [ ("path", VStr path)
              , ("start-line", VNum start)
              , ("end-line", VNum (start + max 0 (count - 1)))
              , ("commit-sha", VStr commitSha)
              ]
        in go rest curPath (hunk : acc)
    | otherwise = go rest curPath acc

-- | Parse unified diff text into added/removed diff-line frames.
-- line-number is the new-file line for additions and the old-file line for
-- deletions.  The +++\/--- file headers can't be mistaken for changed
-- lines because they appear before any @@ hunk header, when the line
-- cursors are still unset.
parseDiffLines :: String -> String -> [Frame]
parseDiffLines diffText commitSha = go (lines diffText) Nothing Nothing []
 where
  go [] _ _ acc = reverse acc
  go (l : rest) curPath cursors acc
    | Just p <- diffHeaderPath l = go rest (Just p) Nothing acc
    | Just path <- curPath
    , Just (oldN, newN) <- lineHunkHeader l =
        go rest (Just path) (Just (oldN, newN)) acc
    | Just path <- curPath
    , Just (oldN, newN) <- cursors =
        case l of
          ('+' : content) ->
            go rest curPath (Just (oldN, newN + 1))
               (mkLine path "+" newN content : acc)
          ('-' : content) ->
            go rest curPath (Just (oldN + 1, newN))
               (mkLine path "-" oldN content : acc)
          ('\\' : _) -> go rest curPath cursors acc   -- "\ No newline ..."
          _ -> go rest curPath (Just (oldN + 1, newN + 1)) acc
    | otherwise = go rest curPath cursors acc
  mkLine path sign n content =
    frame "diff-line"
      [ ("path", VStr path), ("sign", VStr sign)
      , ("line-number", VNum n), ("content", VStr content)
      , ("commit-sha", VStr commitSha)
      ]

-- | @diff --git a\/... b\/PATH@ → PATH
diffHeaderPath :: String -> Maybe String
diffHeaderPath l =
  case l =~ "^diff --git a/.+ b/(.+)$" :: (String, String, String, [String]) of
    (_, _, _, [p]) -> Just p
    _              -> Nothing

-- | @\@\@ -a,b +START,COUNT \@\@@ → (START, COUNT)
hunkHeader :: String -> Maybe (Int, Int)
hunkHeader l =
  case l =~ "^@@ -[0-9,]+ \\+([0-9]+)(,([0-9]+))? @@" :: (String, String, String, [String]) of
    (_, _, _, [start, _, ""])    -> Just (read start, 1)
    (_, _, _, [start, _, count]) -> Just (read start, read count)
    _                            -> Nothing

-- | @\@\@ -OLD[,n] +NEW[...]@ → (OLD, NEW)
lineHunkHeader :: String -> Maybe (Int, Int)
lineHunkHeader l =
  case l =~ "^@@ -([0-9]+)(,[0-9]+)? \\+([0-9]+)" :: (String, String, String, [String]) of
    (_, _, _, [old, _, new]) -> Just (read old, read new)
    _                        -> Nothing

-- Relational steps ---------------------------------------------------------

-- | Evaluate one where-condition against a frame.  Date comparisons and
-- @within@ need the clock, hence IO.
evalCondition :: Frame -> Cond -> IO Bool
evalCondition f (Cond field op value) = do
  let actual = frameField f field
  case op of
    OpEq -> pure (actual == Just value)
    OpNe -> pure (actual /= Just value)
    OpGt -> pure (numCmp (>) actual)
    OpLt -> pure (numCmp (<) actual)
    OpGe -> pure (numCmp (>=) actual)
    OpLe -> pure (numCmp (<=) actual)
    OpContains -> pure $ case (actual, value) of
      (Just (VStr a), VStr v) -> v `isInfixOf` a
      _                       -> False
    OpRegex -> pure $ case (actual, value) of
      (Just (VStr a), VStr v) -> a =~ v
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
parseDate :: String -> Maybe UTCTime
parseDate s = firstJust
  [ p "%Y-%m-%d %H:%M:%S %z", p "%Y-%m-%dT%H:%M:%S%z", p "%Y-%m-%d %H:%M:%S"
  , p "%Y-%m-%d %H:%M", p "%Y-%m-%d", p "%Y-%m", p "%Y"
  ]
 where
  p fmt = parseTimeM True defaultTimeLocale fmt s
  firstJust xs = case [x | Just x <- xs] of (x : _) -> Just x; [] -> Nothing

-- | Does the date fall within "N day\/week\/month\/year(s)" of now?
dateWithin :: String -> String -> IO Bool
dateWithin dateStr period =
  case parsePeriod period of
    Nothing -> pure False
    Just secs ->
      case parseDate dateStr of
        Nothing -> pure False
        Just t -> do
          now <- getCurrentTime
          pure (realToFrac (diffUTCTime now t) <= (secs :: Double))
 where
  parsePeriod str = case words str of
    (nStr : unit : _) | all (`elem` "0123456789") nStr, not (null nStr) ->
      (* read nStr) <$> unitSecs (stripS unit)
    _ -> Nothing
  stripS u = if not (null u) && last u == 's' then init u else u
  unitSecs u = lookup u [("day", 86400), ("week", 604800), ("month", 2592000), ("year", 31536000)]

execGrep :: [Frame] -> String -> Bool -> IO [Frame]
execGrep frames pat regex = fmap concat (mapM grepOne frames)
 where
  grepOne f = case frameField f "sha" of
    Just (VStr sha) -> do
      ls <- runGit (["grep", "-n", "--no-color", if regex then "-E" else "-F", pat, sha])
      pure (mapMaybe (parseLine sha) ls)
    _ -> pure []
  -- "sha:path:line:content" — path may not contain ':', content may
  parseLine sha l = case splitN l of
    Just (path, n, content) ->
      Just (frame "line"
              [ ("sha", VStr sha), ("path", VStr path)
              , ("line-number", VNum n), ("content", VStr content)
              , ("commit-sha", VStr sha)
              ])
    Nothing -> Nothing
  splitN l = do
    (_, r1) <- breakColon l
    (path, r2) <- breakColon r1
    (nStr, content) <- breakColon r2
    if not (null nStr) && all (`elem` "0123456789") nStr
      then Just (path, read nStr, content)
      else Nothing
  breakColon s = case break (== ':') s of
    (a, ':' : rest) -> Just (a, rest)
    _               -> Nothing

execPickaxe :: [Frame] -> String -> Bool -> IO [Frame]
execPickaxe frames pat regex = do
  let shas = [s | f <- frames, Just (VStr s) <- [frameField f "sha"]]
  if null shas
    then pure []
    else do
      hits <- runGit (["log", if regex then "-G" else "-S", pat, "--format=%H", "--no-walk"] ++ shas)
      let hitSet = S.fromList hits
      pure [ f | f <- frames
           , Just (VStr s) <- [frameField f "sha"], s `S.member` hitSet ]

-- | Project each frame to only the listed fields.
project :: [String] -> Frame -> Frame
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
  cmp a b =
    let va = frameField a field
        vb = frameField b field
        ord = if fieldType field == TNumber
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
