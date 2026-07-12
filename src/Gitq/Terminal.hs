-- | Terminal application for the CLI.  A terminal consumes the pipeline's
-- final frames and performs an effect: display, clipboard, or repository
-- mutation.  A terminal that cannot do what it says errors before doing
-- something else (the fail-loud principle) — a few Emacs-only behaviors of
-- the original are adapted for a real terminal and documented in README.
module Gitq.Terminal
  ( applyTerminal
  ) where

import Control.Exception (try, SomeException)
import qualified Data.Text as T
import System.Directory (findExecutable)
import System.FilePath ((</>))
import System.Process (readCreateProcess, proc)
import Gitq.AST
import Gitq.Frame
import Gitq.Git
import Gitq.Render (putUtf8, renderFramesText)

-- Guards: named preconditions a terminal stacks ahead of its effect.
-- Sequencing IO actions that throw GitqError already is the Kleisli
-- composition of the fail-loud discipline (the CLI catches GitqError
-- once, at the boundary) — no transformer needed; what matters is that
-- the vocabulary is shared and a new terminal composes requirements
-- instead of growing bespoke nested conditionals.

-- | The first frame's commit SHA, or a loud error naming the terminal.
firstSha :: String -> [Frame] -> IO String
firstSha what frames = case frames of
  (f : _) | Just sha <- frameCommitSha f -> pure (T.unpack sha)
  _ -> gitqError ("gitq " ++ what ++ ": no commit in result")

-- | Refuse to act over uncommitted work: a conflicted rewrite on top of
-- a dirty tree would be doing more than the query says.
requireCleanTree :: String -> IO ()
requireCleanTree what = do
  dirty <- runGit ["status", "--porcelain"]
  case dirty of
    [] -> pure ()
    _  -> gitqError ("gitq " ++ what ++ ": working tree is not clean; commit or stash first")

-- | Refuse to rewrite history the current branch doesn't contain.
requireAncestorOfHead :: String -> String -> IO ()
requireAncestorOfHead what sha = do
  (code, _, _) <- rawGit ["merge-base", "--is-ancestor", sha, "HEAD"]
  case code of
    0 -> pure ()
    _ -> gitqError ("gitq " ++ what ++ ": " ++ take 8 sha ++ " is not an ancestor of HEAD")

applyTerminal :: [Frame] -> Terminal -> String -> IO ()
applyTerminal frames term pipelineStr = case term of
  TShow ->
    if null frames
      then putStrLn ("gitq: (no results) — " ++ pipelineStr)
      else putUtf8 (renderFramesText frames)

  TCopy -> do
    sha <- firstSha "copy" frames
    copied <- copyToClipboard sha
    if copied
      then putStrLn ("gitq: copied " ++ take 8 sha)
      else gitqError "gitq copy: no clipboard tool found (wl-copy, xclip, xsel, pbcopy)"

  TInsert -> do
    sha <- firstSha "insert" frames
    putStrLn sha

  TCount -> print (length frames)

  TBranchOff mname wt -> do
    sha <- firstSha "branch-off" frames
    name <- case mname of
      Just n -> pure n
      Nothing -> gitqError "gitq branch-off: a branch name is required (/branch-off \"NAME\")"
    _ <- case wt of
      Just path -> runGit ["worktree", "add", "-b", name, path, sha]
      Nothing   -> runGit ["checkout", "-b", name, sha]
    putStrLn ("gitq: created branch '" ++ name ++ "'")

  TAmend noEdit msg -> do
    -- git commit --amend only ever rewrites HEAD.  If the pipeline
    -- selected some other commit, silently amending HEAD instead would be
    -- doing something different from what the query says.
    mhead <- runGitString ["rev-parse", "HEAD"]
    sel <- case frames of
      (f : _) -> pure (T.unpack <$> frameCommitSha f)
      []      -> pure Nothing
    case (sel, mhead) of
      (Just s, Just h) -> do
        rs <- runGitString ["rev-parse", s]
        case rs of
          Just rsha | rsha /= h ->
            gitqError ("gitq amend: selected commit " ++ take 8 s
                       ++ " is not HEAD (amend only rewrites HEAD; use /reword for older commits)")
          _ -> pure ()
      _ -> pure ()
    case (noEdit, msg) of
      (True, _)       -> runGitInherit ["commit", "--amend", "--no-edit"]
      (_, Just m)     -> runGitInherit ["commit", "--amend", "-m", m]
      (False, Nothing) -> runGitInherit ["commit", "--amend"]

  TReword msg -> do
    sha <- firstSha "reword" frames
    mhead <- runGitString ["rev-parse", "HEAD"]
    full <- runGitString ["rev-parse", sha]
    if full == mhead
      then case msg of
        Just m  -> runGitInherit ["commit", "--amend", "-m", m]
        Nothing -> runGitInherit ["commit", "--amend"]
      else gitqError ("gitq reword: rewording a non-HEAD commit is not implemented in the CLI yet (selected "
                      ++ take 8 sha ++ ")")

  TSquash msg ->
    -- inherited stub: reports what it would do
    putStrLn ("gitq squash: " ++ show (length frames) ++ " commits"
              ++ maybe "" (\m -> " -> \"" ++ m ++ "\"") msg
              ++ " — not implemented yet")

  TRemove -> do
    sha <- firstSha "remove" frames
    full <- maybe (pure sha) (pure . T.unpack) =<< runGitString ["rev-parse", sha]
    requireCleanTree "remove"
    requireAncestorOfHead "remove" full
    runGitInherit ["rebase", "--onto", full ++ "^", full]
    putStrLn ("gitq: removed commit " ++ take 8 full)

  TCommit msg -> case msg of
    Just m  -> runGitInherit ["commit", "-m", m]
    Nothing -> runGitInherit ["commit"]

  TStage -> do
    _ <- runGit ["add", "--update"]
    putStrLn "gitq: staged modified files"

  TMark mlabel -> do
    sha <- firstSha "mark" frames
    case mlabel of
      Nothing -> gitqError "gitq mark: a label is required (/mark LABEL)"
      Just label -> do
        _ <- runGit ["notes", "add", "-m", label, sha]
        putStrLn ("gitq: marked " ++ take 8 sha ++ " with '" ++ label ++ "'")

  TWorktree mpath -> do
    sha <- firstSha "worktree" frames
    full <- maybe (pure sha) (pure . T.unpack) =<< runGitString ["rev-parse", sha]
    path <- case mpath of
      Just p  -> pure p
      Nothing -> do
        -- default path follows the worktree convention:
        -- <repo-root>/.worktree/<full-40-char-hash>
        top <- toplevel
        pure (top </> ".worktree" </> full)
    _ <- runGit ["worktree", "add", "--detach", path, full]
    putStrLn ("gitq: added worktree at " ++ path)

-- | Exit code of a git command (stdout/stderr captured and dropped).
rawGit :: [String] -> IO (Int, String, String)
rawGit args = do
  r <- try (readCreateProcess (proc "git" (args ++ [])) "") :: IO (Either SomeException String)
  case r of
    Right out -> pure (0, out, "")
    Left _    -> pure (1, "", "")

-- | Try the common clipboard tools in order; True if one accepted the text.
copyToClipboard :: String -> IO Bool
copyToClipboard text = go tools
 where
  tools =
    [ ("wl-copy", [])
    , ("xclip", ["-selection", "clipboard"])
    , ("xsel", ["--clipboard", "--input"])
    , ("pbcopy", [])
    ]
  go [] = pure False
  go ((cmd, args) : rest) = do
    found <- findExecutable cmd
    case found of
      Nothing -> go rest
      Just _ -> do
        r <- try (readCreateProcess (proc cmd args) text) :: IO (Either SomeException String)
        case r of
          Right _ -> pure True
          Left _  -> go rest
