{-# LANGUAGE OverloadedStrings #-}
-- | Rendering 'Entry' values: plain text (colour re-rendered as real ANSI,
-- so @gitq --scrollback | less -R@ works) and Emacs Lisp plists (@--sexp@).
--
-- The plist shape mirrors "Gitq.Render"'s frame plists so the Emacs side
-- reads them the same way, but is written for 'Entry' directly — a
-- scrollback entry is not a git frame, so it is not routed through the
-- frame renderer.  @:output@ stays one ANSI-laden string per entry; Emacs
-- owns the ANSI→face mapping (@ansi-color.el@), so it is not duplicated
-- here.
module Gitq.Scrollback.Render
  ( renderEntriesText
  , renderEntriesSexp
  ) where

import qualified Data.Text as T
import Data.Text (Text)
import Gitq.Scrollback.Ansi (spansToAnsi)
import Gitq.Scrollback.Entry (Entry (..))

-- | One human-readable block per entry: a header naming its index, exit
-- code (when known) and boundary source, then the command, then the
-- output with colour restored.
renderEntriesText :: [Entry] -> Text
renderEntriesText = T.concat . map renderOne
 where
  renderOne e = T.concat
    [ header e, "\n"
    , "$ ", maybe "(no command)" id (entryCommand e), "\n"
    , outputText e
    ]
  header e = T.concat
    [ "=== entry ", tShow (entryIndex e)
    , case entryExitCode e of
        Just c  -> " (exit " <> tShow c <> ")"
        Nothing -> ""
    , " ===" ]
  outputText e = T.unlines (map spansToAnsi (entryOutput e))

-- | One Emacs Lisp plist per entry, one per line, e.g.
-- @(:index 0 :command "git status" :exit-code nil :output "…")@.
renderEntriesSexp :: [Entry] -> Text
renderEntriesSexp = T.unlines . map renderOne
 where
  renderOne e = T.concat
    [ "(:index ", tShow (entryIndex e)
    , " :command ", maybe "nil" quote (entryCommand e)
    , " :exit-code ", maybe "nil" tShow (entryExitCode e)
    , " :output ", quote (outputStr e)
    , ")" ]
  outputStr e = T.concat [ spansToAnsi l <> "\n" | l <- entryOutput e ]

tShow :: Show a => a -> Text
tShow = T.pack . show

-- | An Emacs-Lisp string literal, escaping @"@, backslash, and the escape
-- byte so the reader accepts the ANSI-laden output payload.
quote :: Text -> Text
quote s = "\"" <> T.concatMap esc s <> "\""
 where
  esc '"'    = "\\\""
  esc '\\'   = "\\\\"
  esc '\ESC' = "\\e"
  esc '\n'   = "\\n"
  esc c      = T.singleton c
