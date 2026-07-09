-- | Git execution layer and data fetchers.
module Gitq.Git
  ( runGit
  , runGitStdin
  , runGitString
  , runGitInherit
  , toplevel
  , GitqError (..)
  , gitqError
  , fetchCommits
  , fetchCommit
  , fetchCommitMap
  , fetchBranches
  , fetchTags
  , fetchRefs
  , fetchWorktrees
  , fetchBlobsAt
  , pathMatches
  , logFormat
  ) where

import Control.Exception (Exception, throwIO)
import qualified Data.Map.Strict as M
import qualified Data.Set as S
import Data.List (isInfixOf, stripPrefix, tails)
import System.Exit (ExitCode (..))
import System.Process (proc, readCreateProcessWithExitCode, waitForProcess, withCreateProcess)
import Gitq.AST (EntryFilter (..))
import Gitq.Frame

-- | A user-facing gitq error (parse error, missing repo, guarded terminal).
newtype GitqError = GitqError String
  deriving (Show)

instance Exception GitqError

gitqError :: String -> IO a
gitqError = throwIO . GitqError

-- | Run git; return output as a list of non-empty lines.  Stderr is
-- discarded, not mixed into the captured output — otherwise a git error
-- message (e.g. an invalid revision) gets split into lines and silently
-- returned as if it were real data.
runGit :: [String] -> IO [String]
runGit args = runGitStdin args ""

-- | Like 'runGit', feeding the given input to git's stdin — used with
-- @--stdin@ to pass arbitrarily many revisions in a single process,
-- bypassing the argv length limit.
runGitStdin :: [String] -> String -> IO [String]
runGitStdin args input = do
  (_code, out, _err) <- readCreateProcessWithExitCode (proc "git" args) input
  pure (filter (not . null) (lines out))

-- | Run git; return the first line of output, or Nothing.
runGitString :: [String] -> IO (Maybe String)
runGitString args = do
  ls <- runGit args
  pure (case ls of (l : _) -> Just l; [] -> Nothing)

-- | Run git with the terminal's stdio inherited (lets @git commit@ open
-- the user's editor).  Errors loudly on a non-zero exit.
runGitInherit :: [String] -> IO ()
runGitInherit args =
  withCreateProcess (proc "git" args) $ \_ _ _ ph -> do
    code <- waitForProcess ph
    case code of
      ExitSuccess   -> pure ()
      ExitFailure n ->
        gitqError ("git " ++ unwords (take 2 args) ++ " exited with status " ++ show n)

-- | The git toplevel, or a loud error when not in a repository.
toplevel :: IO FilePath
toplevel = do
  top <- runGitString ["rev-parse", "--show-toplevel"]
  case top of
    Just t  -> pure t
    Nothing -> gitqError "gitq: not in a git repository"

-- | NUL-delimited log format using git's %x00 escape (safe as a CLI arg).
logFormat :: String
logFormat = "%H%x00%ae%x00%an%x00%ai%x00%P%x00%T%x00%s"

-- | Split on a character (no regex, keeps empty fields).
splitOn :: Char -> String -> [String]
splitOn c s = case break (== c) s of
  (a, [])       -> [a]
  (a, _ : rest) -> a : splitOn c rest

-- | Parse a NUL-delimited commit log line into a commit frame, or Nothing.
parseCommitLine :: String -> Maybe Frame
parseCommitLine line =
  case splitOn '\NUL' line of
    (sha : email : author : date : parents : tree : msgParts)
      | not (null sha), not (null msgParts) ->
        Just Frame
          { frameType = "commit"
          , frameParents = words parents
          , frameAttrs = M.fromList
              [ ("sha", VStr sha), ("email", VStr email)
              , ("author", VStr author), ("date", VStr date)
              , ("tree", VStr tree)
              -- a NUL inside the subject would have split further; rejoin
              , ("message", VStr (concat msgParts))
              ]
          }
    _ -> Nothing

-- | Commits reachable from HEAD (or within a range) as commit frames.
fetchCommits :: Maybe String -> IO [Frame]
fetchCommits range = do
  let fmt = "--format=" ++ logFormat
      args = case range of
        Just r  -> ["log", fmt, r]
        Nothing -> ["log", fmt]
  ls <- runGit args
  pure [f | Just f <- map parseCommitLine ls]

-- | Many commits by full SHA in a single git process, as a SHA-keyed map.
-- One @git log --no-walk --stdin@ replaces one process per commit — the
-- difference between milliseconds and minutes on ancestor-closure walks.
-- Callers must pass full SHAs (the %H\/%P values git itself printed);
-- unresolvable input would fail the whole batch, unlike 'fetchCommit'
-- which probes one tolerant rev-parse.
fetchCommitMap :: [String] -> IO (M.Map String Frame)
fetchCommitMap shas
  | null shas = pure M.empty
  | otherwise = do
      let uniq = S.toList (S.fromList shas)
      ls <- runGitStdin ["log", "--no-walk=unsorted", "--format=" ++ logFormat, "--stdin"]
                        (unlines uniq)
      pure (M.fromList
              [ (sha, f)
              | Just f <- map parseCommitLine ls
              , Just (VStr sha) <- [M.lookup "sha" (frameAttrs f)]
              ])

-- | A single commit by SHA or ref, or Nothing.
fetchCommit :: String -> IO (Maybe Frame)
fetchCommit shaOrRef = do
  msha <- runGitString ["rev-parse", "--verify", shaOrRef]
  case msha of
    Nothing  -> pure Nothing
    Just sha -> do
      ls <- runGit ["log", "--no-walk", "--format=" ++ logFormat, sha]
      pure (case [f | Just f <- map parseCommitLine ls] of
              (f : _) -> Just f
              []      -> Nothing)

parseRefLine :: Maybe String -> String -> Maybe Frame
parseRefLine reftype line =
  case break (== ' ') line of
    (sha, ' ' : name)
      | length sha >= 40, all (`elem` "0123456789abcdef") sha, not (null name) ->
        Just (frame "ref"
                ([("sha", VStr sha), ("name", VStr name)]
                 ++ [("reftype", VStr rt) | Just rt <- [reftype]]))
    _ -> Nothing

fetchForEachRef :: Maybe String -> [String] -> IO [Frame]
fetchForEachRef reftype patterns = do
  ls <- runGit (["for-each-ref", "--format=%(objectname) %(refname:short)"] ++ patterns)
  pure [f | Just f <- map (parseRefLine reftype) ls]

fetchBranches :: IO [Frame]
fetchBranches = fetchForEachRef (Just "branch") ["refs/heads/"]

fetchTags :: IO [Frame]
fetchTags = fetchForEachRef (Just "tag") ["refs/tags/"]

fetchRefs :: IO [Frame]
fetchRefs = fetchForEachRef Nothing []

-- | All worktrees, from @git worktree list --porcelain@.
fetchWorktrees :: IO [Frame]
fetchWorktrees = do
  ls <- runGit ["worktree", "list", "--porcelain"]
  pure (go ls Nothing)
 where
  go [] cur = flush cur
  go (l : rest) cur
    | Just p <- stripPrefix "worktree " l =
        flush cur ++ go rest (Just [("path", VStr p)])
    | Just s <- stripPrefix "HEAD " l = go rest (add ("sha", VStr s) cur)
    | Just b <- stripPrefix "branch " l =
        let short = maybe b id (stripPrefix "refs/heads/" b)
        in go rest (add ("branch", VStr short) cur)
    | l == "detached" = go rest (add ("detached", VBool True) cur)
    | otherwise = go rest cur
  add kv = fmap (++ [kv])
  flush Nothing      = []
  flush (Just attrs) = [frame "worktree" attrs]

-- | Blob/tree entries under a tree SHA, optionally filtered by entry type
-- and path glob.
fetchBlobsAt :: String -> Maybe String -> Maybe EntryFilter -> IO [Frame]
fetchBlobsAt treeSha pathFilter typeFilter = do
  ls <- runGit ["ls-tree", "-r", treeSha]
  pure [f | Just f <- map parseEntry ls]
 where
  parseEntry line =
    -- format: "<mode> <type> <sha>\t<path>"
    case break (== '\t') line of
      (meta, '\t' : path) ->
        case words meta of
          [mode, ftype, sha]
            | ftype `elem` ["blob", "tree"]
            , maybe True (\tf -> kindName tf == ftype) typeFilter
            , maybe True (`pathMatches` path) pathFilter ->
              Just (frame ftype [("sha", VStr sha), ("path", VStr path), ("mode", VStr mode)])
          _ -> Nothing
      _ -> Nothing
  kindName FBlob = "blob"
  kindName FTree = "tree"

-- | Glob match (shell wildcards @*@, @?@, @[...]@ — @*@ crosses @/@, same
-- as Emacs's wildcard-to-regexp), with a literal-substring fallback.
pathMatches :: String -> String -> Bool
pathMatches pattern path = globMatch pattern path || pattern `isInfixOf` path
 where
  globMatch [] [] = True
  globMatch ('*' : ps) s = any (globMatch ps) (tails s)
  globMatch ('?' : ps) (_ : ss) = globMatch ps ss
  globMatch ('[' : ps) (c : ss) =
    case break (== ']') ps of
      (klass, ']' : ps') | not (null klass) -> classMatch klass c && globMatch ps' ss
      _ -> False
  globMatch (p : ps) (c : ss) = p == c && globMatch ps ss
  globMatch _ _ = False
  classMatch ('!' : klass) c = not (classMatch klass c)
  classMatch klass c = go klass
   where
    go (a : '-' : b : rest) = (a <= c && c <= b) || go rest
    go (a : rest)           = a == c || go rest
    go []                   = False
