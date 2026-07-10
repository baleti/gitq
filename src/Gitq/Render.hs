{-# LANGUAGE OverloadedStrings #-}
-- | Frame rendering: plain text for the terminal, s-expressions for the
-- Emacs integration (@--sexp@).  Output is 'Text'; callers encode it as
-- UTF-8 bytes for stdout (locale-proof, symmetric with lenient input).
module Gitq.Render
  ( renderFrameLine
  , renderFramesText
  , renderFrameSexp
  , renderFramesSexp
  , putUtf8
  ) where

import qualified Data.ByteString as BS
import qualified Data.Map.Strict as M
import qualified Data.Text as T
import qualified Data.Text.Encoding as TE
import Data.Text (Text)
import Gitq.Frame

-- | Write text to stdout as UTF-8 bytes, bypassing the locale — output is
-- lenient-UTF-8-in, UTF-8-out, symmetrically.
putUtf8 :: Text -> IO ()
putUtf8 = BS.putStr . TE.encodeUtf8

pad :: Int -> Text -> Text
pad n = T.justifyLeft n ' '

str :: Frame -> Text -> Text
str f k = case frameField f k of
  Just (VStr s)  -> s
  Just (VNum n)  -> T.pack (show n)
  Just (VBool b) -> if b then "true" else "false"
  Nothing        -> "?"

num :: Frame -> Text -> Text
num f k = case frameField f k of
  Just (VNum n) -> T.pack (show n)
  _             -> "0"

-- | One plain-text line per frame, matching the original CLI's format.
renderFrameLine :: Frame -> Text
renderFrameLine f = case frameType f of
  "commit" -> T.concat
    [ T.take 8 (str f "sha"), "  ", pad 20 (T.take 20 (str f "author"))
    , "  ", T.take 10 (str f "date"), "  ", orEmpty "message" ]
  "ref" -> T.concat [pad 40 (str f "name"), "  ", T.take 8 (str f "sha")]
  "blob" -> str f "path"
  "tree" -> case frameField f "path" of
    Just (VStr p) -> p                       -- subtree entry
    _             -> T.concat ["(tree ", str f "sha", ")"]
  "worktree" -> T.concat
    [ pad 40 (str f "path"), "  "
    , case frameField f "branch" of
        Just (VStr b) -> b
        _             -> "(detached)" ]
  "line" -> T.concat
    [str f "path", ":", num f "line-number", ": ", orEmpty "content"]
  "hunk" -> T.concat
    [ case frameField f "commit-sha" of
        Just (VStr c) -> T.take 8 c <> "  "
        _             -> ""
    , str f "path", ":", num f "start-line", "-", num f "end-line"
    , case (frameField f "author", frameField f "date") of
        (Just (VStr a), Just (VStr d)) -> "  " <> a <> "  " <> T.take 10 d
        _ -> ""
    , case frameField f "content" of
        Just (VStr c) | not (T.null c) -> "\n" <> T.stripEnd c
        _ -> "" ]
  "diff-line" -> T.concat
    [ case frameField f "commit-sha" of
        Just (VStr c) -> T.take 8 c <> "  "
        _             -> ""
    , str f "path", ":", num f "line-number", ": "
    , str f "sign", orEmpty "content" ]
  "diff" -> str f "path"
  -- projected or unknown — key:value pairs
  _ -> T.unwords [k <> ":" <> showVal v | (k, v) <- M.toList (frameAttrs f)]
 where
  orEmpty k = case frameField f k of
    Just (VStr s) -> s
    _             -> ""
  showVal (VStr s)  = s
  showVal (VNum n)  = T.pack (show n)
  showVal (VBool b) = if b then "t" else "nil"

renderFramesText :: [Frame] -> Text
renderFramesText = T.unlines . map renderFrameLine

-- | A frame as an Emacs Lisp plist, one per line — the Emacs integration
-- reads these to rebuild frames with text properties.
renderFrameSexp :: Frame -> Text
renderFrameSexp f = T.concat
  [ "(:type ", frameType f
  , if null (frameParents f)
      then ""
      else " :parents (" <> T.unwords (map quote (frameParents f)) <> ")"
  , T.concat (map attr (M.toList (frameAttrs f)))
  , ")"
  ]
 where
  attr (k, v) = T.concat [" :", k, " ", val v]
  val (VStr s)  = quote s
  val (VNum n)  = T.pack (show n)
  val (VBool b) = if b then "t" else "nil"
  quote s = "\"" <> T.concatMap esc s <> "\""
  esc '"'  = "\\\""
  esc '\\' = "\\\\"
  esc c    = T.singleton c

renderFramesSexp :: [Frame] -> Text
renderFramesSexp = T.unlines . map renderFrameSexp
