-- | Frame rendering: plain text for the terminal, s-expressions for the
-- Emacs integration (@--sexp@).
module Gitq.Render
  ( renderFrameLine
  , renderFramesText
  , renderFrameSexp
  , renderFramesSexp
  ) where

import qualified Data.Map.Strict as M
import Gitq.Frame

pad :: Int -> String -> String
pad n s = take n (s ++ repeat ' ')

shorten :: Int -> String -> String
shorten = take

str :: Frame -> String -> String
str f k = case frameField f k of
  Just (VStr s) -> s
  Just (VNum n) -> show n
  Just (VBool b) -> if b then "true" else "false"
  Nothing -> "?"

num :: Frame -> String -> Int
num f k = case frameField f k of
  Just (VNum n) -> n
  _             -> 0

-- | One plain-text line per frame, matching the original CLI's format.
renderFrameLine :: Frame -> String
renderFrameLine f = case frameType f of
  "commit" ->
    shorten 8 (str f "sha") ++ "  " ++ pad 20 (shorten 20 (str f "author"))
      ++ "  " ++ shorten 10 (str f "date") ++ "  " ++ orEmpty "message"
  "ref" -> pad 40 (str f "name") ++ "  " ++ shorten 8 (str f "sha")
  "blob" -> str f "path"
  "tree" -> case frameField f "path" of
    Just (VStr p) -> p                       -- subtree entry
    _             -> "(tree " ++ str f "sha" ++ ")"
  "worktree" ->
    pad 40 (str f "path") ++ "  "
      ++ (case frameField f "branch" of
            Just (VStr b) -> b
            _             -> "(detached)")
  "line" ->
    str f "path" ++ ":" ++ show (num f "line-number") ++ ": " ++ orEmpty "content"
  "hunk" ->
    str f "path" ++ ":" ++ show (num f "start-line") ++ "-" ++ show (num f "end-line")
  "diff-line" ->
    (case frameField f "commit-sha" of
       Just (VStr c) -> shorten 8 c ++ "  "
       _             -> "")
      ++ str f "path" ++ ":" ++ show (num f "line-number") ++ ": "
      ++ str f "sign" ++ orEmpty "content"
  "diff" -> str f "path"
  -- projected or unknown — key:value pairs
  _ -> unwords [k ++ ":" ++ showVal v | (k, v) <- M.toList (frameAttrs f)]
 where
  orEmpty k = case frameField f k of
    Just (VStr s) -> s
    _             -> ""
  showVal (VStr s)  = s
  showVal (VNum n)  = show n
  showVal (VBool b) = if b then "t" else "nil"

renderFramesText :: [Frame] -> String
renderFramesText = unlines . map renderFrameLine

-- | A frame as an Emacs Lisp plist, one per line — the Emacs integration
-- reads these to rebuild frames with text properties.
renderFrameSexp :: Frame -> String
renderFrameSexp f =
  "(:type " ++ frameType f
    ++ concat [" :parents (" ++ unwords (map quote (frameParents f)) ++ ")"
              | not (null (frameParents f))]
    ++ concatMap attr (M.toList (frameAttrs f))
    ++ ")"
 where
  attr (k, v) = " :" ++ k ++ " " ++ val v
  val (VStr s)  = quote s
  val (VNum n)  = show n
  val (VBool b) = if b then "t" else "nil"
  quote s = '"' : concatMap esc s ++ "\""
  esc '"'  = "\\\""
  esc '\\' = "\\\\"
  esc c    = [c]

renderFramesSexp :: [Frame] -> String
renderFramesSexp fs = unlines (map renderFrameSexp fs)
