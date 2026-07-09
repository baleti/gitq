-- | gitq — standalone CLI for the GitQ pipeline language.
module Main (main) where

import Control.Exception (catch)
import System.Directory (setCurrentDirectory)
import System.Environment (getArgs)
import System.Exit (exitFailure, exitSuccess)
import System.IO (hPutStrLn, stderr)
import Gitq.Complete (completeCandidates)
import Gitq.Exec (execPipeline)
import Gitq.Git (GitqError (..), toplevel)
import Gitq.Parse (parsePipeline)
import Gitq.Registry (describeToken)
import Gitq.Render (renderFramesSexp, renderFramesText)
import Gitq.Terminal (applyTerminal)

usage :: IO ()
usage = mapM_ (hPutStrLn stderr)
  [ "Usage: gitq [--sexp] [--preview] <pipeline>"
  , "       gitq --complete <prefix>"
  , ""
  , "Examples:"
  , "  gitq 'commits take 10'"
  , "  gitq 'commits where author alice /show'"
  , "  gitq 'commits in main..HEAD /count'"
  , "  gitq 'HEAD via parent* where message \"fix\"'"
  , "  gitq 'commits pickaxe \"needle\" via diff.lines where content \"needle\"'"
  , ""
  , "Flags:"
  , "  --sexp      print frames as Emacs Lisp plists (for the Emacs integration)"
  , "  --preview   parse and run the source and steps, but never apply a terminal"
  , "  --complete  print completion candidates for the given pipeline prefix"
  ]

main :: IO ()
main = do
  args <- getArgs
  case filter (/= "--") args of
    [] -> usage >> exitFailure
    (a : _) | a `elem` ["-h", "--help"] -> usage >> exitSuccess
    ("--complete" : rest) -> complete False rest
    ("--complete-annotated" : rest) -> complete True rest
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
  -- completion must never error mid-keystroke; just print what we can.
  -- annotated mode prints "candidate\tdescription" so callers don't need
  -- their own copy of the description registry.
  complete annotated rest = do
    (do top <- toplevel
        setCurrentDirectory top
        cands <- completeCandidates (unwords rest)
        mapM_ (putStrLn . decorate) cands)
      `catch` \(GitqError _) -> pure ()
    exitSuccess
   where
    decorate c
      | annotated = c ++ "\t" ++ maybe "" id (describeToken (dropDash c))
      | otherwise = c
    dropDash ('-' : c@(_ : _)) = c
    dropDash c                 = c

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
        | sexp = putStr (renderFramesSexp frames)
        | null frames = putStrLn ("gitq: (no results) — " ++ pipeline)
        | otherwise = putStr (renderFramesText frames)
  case (preview, mterm) of
    (True, _) -> display
    (False, Just term)
      | sexp -> display    -- structured consumers never trigger effects
      | otherwise -> applyTerminal frames term pipeline
    (False, Nothing) -> display
