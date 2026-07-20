-- | gitq — standalone CLI for the GitQ pipeline language.
module Main (main) where

import Control.Exception (catch)
import qualified Data.Text as T
import System.Directory (setCurrentDirectory)
import System.Environment (getArgs, lookupEnv)
import System.Exit (exitFailure, exitSuccess)
import System.IO (hPutStrLn, stderr)
import Gitq.Complete (completeCandidates)
import Gitq.Exec (execPipeline)
import Gitq.Git (GitqError (..), toplevel)
import Gitq.Parse (parsePipeline)
import Gitq.Registry (describeToken, tokenKind)
import Gitq.Render (putUtf8, renderFramesSexp, renderFramesText)
import Gitq.Scrollback.Capture (CaptureTarget (..), captureScrollback)
import Gitq.Scrollback.Entry (defaultPromptRegex, parseEntriesWith)
import Gitq.Scrollback.Render (renderEntriesSexp, renderEntriesText)
import Gitq.Terminal (applyTerminal)

usage :: IO ()
usage = mapM_ (hPutStrLn stderr)
  [ "Usage: gitq [--sexp] [--preview] <pipeline>"
  , "       gitq --complete <prefix>"
  , "       gitq --scrollback [--sexp] [--tmux-target TARGET]"
  , ""
  , "Examples:"
  , "  gitq 'commits take 10'"
  , "  gitq 'commits where author alice /show'"
  , "  gitq 'commits in main..HEAD /count'"
  , "  gitq 'HEAD via parent* where message \"fix\"'"
  , "  gitq 'commits pickaxe \"needle\" via diff.lines where content \"needle\"'"
  , ""
  , "Flags:"
  , "  --sexp          print frames as Emacs Lisp plists (for the Emacs integration)"
  , "  --preview       parse and run the source and steps, but never apply a terminal"
  , "  --complete      print completion candidates for the given pipeline prefix"
  , "  --scrollback    capture the current tmux pane's scrollback and print entries"
  , "  --tmux-target   with --scrollback, capture a specific tmux pane"
  ]

main :: IO ()
main = do
  args <- getArgs
  case filter (/= "--") args of
    [] -> usage >> exitFailure
    (a : _) | a `elem` ["-h", "--help"] -> usage >> exitSuccess
    ("--complete" : rest) -> complete False rest
    ("--complete-annotated" : rest) -> complete True rest
    args' | "--scrollback" `elem` args' -> do
      let (sexp, rest1) = takeFlag "--sexp" (filter (/= "--scrollback") args')
          (target, _)   = takeValFlag "--tmux-target" rest1
      scrollback sexp target
        `catch` \(GitqError msg) -> do
          hPutStrLn stderr msg
          exitFailure
    rest0 -> do
      let (sexp, rest1) = takeFlag "--sexp" rest0
          (preview, rest2) = takeFlag "--preview" rest1
          pipeline = unwords rest2
      run sexp preview pipeline
        `catch` \(GitqError msg) -> do
          hPutStrLn stderr msg
          exitFailure
 where
  takeFlag f xs = (f `elem` xs, filter (/= f) xs)
  -- a flag that consumes the following argument as its value
  takeValFlag f xs = case break (== f) xs of
    (before, _ : v : after) -> (Just v, before ++ after)
    (before, _ : after)     -> (Nothing, before ++ after)  -- flag with no value
    (before, [])            -> (Nothing, before)
  -- completion must never error mid-keystroke; just print what we can.
  -- annotated mode prints "candidate\tkind\tdescription" so callers need
  -- neither their own description registry nor their own grammar
  -- classification.
  complete annotated rest = do
    (do top <- toplevel
        setCurrentDirectory top
        cands <- completeCandidates (unwords rest)
        mapM_ (putStrLn . decorate) cands)
      `catch` \(GitqError _) -> pure ()
    exitSuccess
   where
    decorate c
      | annotated =
          c ++ "\t" ++ maybe "" id (tokenKind c)
            ++ "\t" ++ maybe "" id (describeToken (dropDash c))
      | otherwise = c
    dropDash ('-' : c@(_ : _)) = c
    dropDash c                 = c

-- | Capture the tmux pane's scrollback, split it into entries, and print
-- them — human-readable, or as Emacs Lisp plists with @--sexp@.  Unlike a
-- pipeline this needs no git repository; it reads the terminal, not git.
scrollback :: Bool -> Maybe String -> IO ()
scrollback sexp mtarget = do
  let target = maybe CurrentPane NamedPane mtarget
  raw <- captureScrollback target
  promptRx <- maybe defaultPromptRegex T.pack <$> lookupEnv "GITQ_SCROLLBACK_PROMPT_REGEX"
  let entries = parseEntriesWith promptRx raw
  putUtf8 (if sexp then renderEntriesSexp entries else renderEntriesText entries)

run :: Bool -> Bool -> String -> IO ()
run sexp preview pipeline = do
  parsed <- case parsePipeline pipeline of
    Right p  -> pure p
    Left err -> do
      hPutStrLn stderr err
      exitFailure
  top <- toplevel
  setCurrentDirectory top
  (frames, mterm) <- execPipeline parsed
  let display
        | sexp = putUtf8 (renderFramesSexp frames)
        | null frames = putStrLn ("gitq: (no results) — " ++ pipeline)
        | otherwise = putUtf8 (renderFramesText frames)
  case (preview, mterm) of
    (True, _) -> display
    (False, Just term)
      | sexp -> display    -- structured consumers never trigger effects
      | otherwise -> applyTerminal frames term pipeline
    (False, Nothing) -> display
