{-# LANGUAGE OverloadedStrings #-}
-- | Git execution layer and data fetchers.
--
-- All git output is read as bytes, decoded as lenient UTF-8 once, and
-- split into zero-copy 'Text' slices — frame fields share the decoded
-- buffer rather than materializing per-field strings.
module Gitq.Git
  ( runGit
  , runGitLoud
  , runGitStdin
  , runGitString
  , runGitInherit
  , toplevel
  , GitqError (..)
  , gitqError
  , fetchCommits
  , fetchCommit
  , fetchCommitMap
  , parseCommitLine
  , fetchBranches
  , fetchTags
  , fetchRefs
  , fetchWorktrees
  , fetchBlobsAt
  , pathMatches
  , logFormat
  ) where

import Control.Concurrent (forkIO)
import Control.Concurrent.MVar (newEmptyMVar, putMVar, takeMVar)
import Control.Exception (Exception, throwIO)
import qualified Data.ByteString as BS
import qualified Data.Map.Strict as M
import qualified Data.Set as S
import qualified Data.Text as T
import qualified Data.Text.Encoding as TE
import qualified Data.Text.Encoding.Error as TEE
import Data.Text (Text)
import System.Exit (ExitCode (..))
import System.IO (IOMode (WriteMode), hClose, openFile)
import System.Process
  ( CreateProcess (..), StdStream (..), createProcess, proc, waitForProcess
  , withCreateProcess )
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
runGit :: [String] -> IO [Text]
runGit args = runGitStdin args ""

-- | Like 'runGit', feeding the given input to git's stdin — used with
-- @--stdin@ to pass arbitrarily many revisions in a single process,
-- bypassing the argv length limit.
--
-- Output is read as bytes and decoded as UTF-8 /leniently/: real
-- histories contain latin-1 commit metadata (git.git does), and strict
-- locale decoding throws @invalid argument@ on the first such byte.
-- Lenient decoding here also matches what the native backend does, so
-- both produce the same frames.  'T.lines' and downstream 'T.split' give
-- zero-copy slices of the one decoded buffer.
runGitStdin :: [String] -> Text -> IO [Text]
runGitStdin args input = do
  devNull <- openFile "/dev/null" WriteMode
  (Just hin, Just hout, _, ph) <-
    createProcess (proc "git" args)
      { std_in = CreatePipe, std_out = CreatePipe, std_err = UseHandle devNull }
  -- write stdin from a separate thread: a large --stdin batch could
  -- otherwise deadlock against a filling stdout pipe
  _ <- forkIO (BS.hPutStr hin (TE.encodeUtf8 input) >> hClose hin)
  out <- BS.hGetContents hout
  _ <- waitForProcess ph
  hClose devNull
  pure (filter (not . T.null) (T.lines (TE.decodeUtf8With TEE.lenientDecode out)))

-- | Like 'runGit', but a git failure is surfaced instead of swallowed:
-- Left carries git's own stderr.  For steps where an invalid argument
-- (e.g. a bad revspec) must be a loud error, not a silently-empty
-- result.  Stderr is drained concurrently so a chatty failure can't
-- deadlock against the stdout pipe.
runGitLoud :: [String] -> IO (Either String [Text])
runGitLoud args = do
  (Just hin, Just hout, Just herr, ph) <-
    createProcess (proc "git" args)
      { std_in = CreatePipe, std_out = CreatePipe, std_err = CreatePipe }
  hClose hin
  errv <- newEmptyMVar
  _ <- forkIO (BS.hGetContents herr >>= putMVar errv)
  out <- BS.hGetContents hout
  err <- takeMVar errv
  code <- waitForProcess ph
  case code of
    ExitSuccess ->
      pure (Right (filter (not . T.null)
                     (T.lines (TE.decodeUtf8With TEE.lenientDecode out))))
    ExitFailure _ ->
      pure (Left (T.unpack (T.strip (TE.decodeUtf8With TEE.lenientDecode err))))

-- | Run git; return the first line of output, or Nothing.
runGitString :: [String] -> IO (Maybe Text)
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
    Just t  -> pure (T.unpack t)
    Nothing -> gitqError "gitq: not in a git repository"

-- | NUL-delimited log format using git's %x00 escape (safe as a CLI arg).
logFormat :: String
logFormat = "%H%x00%ae%x00%an%x00%ai%x00%P%x00%T%x00%s"

-- | Parse a NUL-delimited commit log line into a commit frame, or Nothing.
-- The fields are zero-copy slices of the line (itself a slice of the whole
-- decoded output).
parseCommitLine :: Text -> Maybe Frame
parseCommitLine line =
  case T.split (== '\NUL') line of
    (sha : email : author : date : parents : tree : msgParts)
      | not (T.null sha), not (null msgParts) ->
        Just Frame
          { frameType = "commit"
          , frameParents = T.words parents
          , frameAttrs = M.fromList
              [ ("sha", VStr sha), ("email", VStr email)
              , ("author", VStr author), ("date", VStr date)
              , ("tree", VStr tree)
              -- a NUL inside the subject would have split further; rejoin
              , ("message", VStr (T.concat msgParts))
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
fetchCommitMap :: [Text] -> IO (M.Map Text Frame)
fetchCommitMap shas
  | null shas = pure M.empty
  | otherwise = do
      let uniq = S.toList (S.fromList shas)
      ls <- runGitStdin ["log", "--no-walk=unsorted", "--format=" ++ logFormat, "--stdin"]
                        (T.unlines uniq)
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
      ls <- runGit ["log", "--no-walk", "--format=" ++ logFormat, T.unpack sha]
      pure (case [f | Just f <- map parseCommitLine ls] of
              (f : _) -> Just f
              []      -> Nothing)

parseRefLine :: Maybe Text -> Text -> Maybe Frame
parseRefLine reftype line =
  case T.break (== ' ') line of
    (sha, rest)
      | Just name <- T.stripPrefix " " rest
      , T.length sha >= 40
      , T.all (`elem` ("0123456789abcdef" :: String)) sha
      , not (T.null name) ->
        Just (frame "ref"
                ([("sha", VStr sha), ("name", VStr name)]
                 ++ [("reftype", VStr rt) | Just rt <- [reftype]]))
    _ -> Nothing

fetchForEachRef :: Maybe Text -> [String] -> IO [Frame]
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
    | Just p <- T.stripPrefix "worktree " l =
        flush cur ++ go rest (Just [("path", VStr p)])
    | Just s <- T.stripPrefix "HEAD " l = go rest (add ("sha", VStr s) cur)
    | Just b <- T.stripPrefix "branch " l =
        let short = maybe b id (T.stripPrefix "refs/heads/" b)
        in go rest (add ("branch", VStr short) cur)
    | l == "detached" = go rest (add ("detached", VBool True) cur)
    | otherwise = go rest cur
  add kv = fmap (++ [kv])
  flush Nothing      = []
  flush (Just attrs) = [frame "worktree" attrs]

-- | Blob/tree entries under a tree SHA, optionally filtered by entry type
-- and path glob.
fetchBlobsAt :: Text -> Maybe Text -> Maybe EntryFilter -> IO [Frame]
fetchBlobsAt treeSha pathFilter typeFilter = do
  ls <- runGit ["ls-tree", "-r", T.unpack treeSha]
  pure [f | Just f <- map parseEntry ls]
 where
  parseEntry line =
    -- format: "<mode> <type> <sha>\t<path>"
    case T.break (== '\t') line of
      (meta, tabPath) | Just path <- T.stripPrefix "\t" tabPath ->
        case T.words meta of
          [mode, ftype, sha]
            | ftype `elem` (["blob", "tree"] :: [Text])
            , maybe True (\tf -> kindName tf == ftype) typeFilter
            , maybe True (`pathMatches` path) pathFilter ->
              Just (frame ftype [("sha", VStr sha), ("path", VStr path), ("mode", VStr mode)])
          _ -> Nothing
      _ -> Nothing
  kindName FBlob = "blob"
  kindName FTree = "tree"

-- | Glob match (shell wildcards @*@, @?@, @[...]@ — @*@ crosses @/@, same
-- as Emacs's wildcard-to-regexp), with a literal-substring fallback.
pathMatches :: Text -> Text -> Bool
pathMatches pattern path = globMatch pattern path || pattern `T.isInfixOf` path
 where
  globMatch p s = case T.uncons p of
    Nothing -> T.null s
    Just ('*', ps) -> any (globMatch ps) (T.tails s)
    Just ('?', ps) -> case T.uncons s of
      Just (_, ss) -> globMatch ps ss
      Nothing      -> False
    Just ('[', ps) -> case T.break (== ']') ps of
      (klass, rest)
        | Just ps' <- T.stripPrefix "]" rest
        , not (T.null klass) ->
          case T.uncons s of
            Just (c, ss) -> classMatch (T.unpack klass) c && globMatch ps' ss
            Nothing      -> False
      _ -> False
    Just (pc, ps) -> case T.uncons s of
      Just (c, ss) -> pc == c && globMatch ps ss
      Nothing      -> False
  classMatch ('!' : klass) c = not (classMatch klass c)
  classMatch klass c = go klass
   where
    go (a : '-' : b : rest) = (a <= c && c <= b) || go rest
    go (a : rest)           = a == c || go rest
    go []                   = False
