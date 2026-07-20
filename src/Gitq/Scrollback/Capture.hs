{-# LANGUAGE OverloadedStrings #-}
{-# LANGUAGE ScopedTypeVariables #-}
-- | Capture a tmux pane's scrollback as raw 'Text'.
--
-- The whole scrollback subsystem is gated on tmux: outside it there is no
-- portable way for a program to read back previously-printed terminal
-- output.  So this module shells out to @tmux capture-pane@ and fails
-- loud (the house 'GitqError') when @$TMUX@ or the @tmux@ binary is
-- missing, rather than degrading to some emulator-specific hack.
--
-- Bytes in, lenient-UTF-8 decoded once — same discipline as "Gitq.Git",
-- so a latin-1 byte in captured command output can't throw.
module Gitq.Scrollback.Capture
  ( CaptureTarget (..)
  , captureScrollback
  ) where

import Control.Exception (IOException, try)
import qualified Data.ByteString as BS
import qualified Data.Text.Encoding as TE
import qualified Data.Text.Encoding.Error as TEE
import Data.Text (Text)
import System.Environment (lookupEnv)
import System.Exit (ExitCode (..))
import System.Process
  ( CreateProcess (..), StdStream (..), createProcess, proc, waitForProcess )
import Gitq.Git (gitqError)

-- | Which tmux pane to read.  'CurrentPane' resolves to whatever pane
-- tmux considers current for the invoking client — what you want when a
-- zsh widget inside a pane calls gitq.
data CaptureTarget
  = CurrentPane
  | NamedPane String   -- ^ tmux target-pane syntax, e.g. @session:0.1@

-- | Capture full scrollback + visible screen with SGR escape sequences
-- intact, so "Gitq.Scrollback.Ansi" can recover color/attributes.  Errors
-- loudly when tmux isn't available.
--
-- Flags: @-e@ keeps SGR escapes; @-p@ prints to stdout; @-S -@ starts at
-- the beginning of history (full scrollback); @-J@ joins wrapped lines so
-- an 80-column-wrapped command line stays one logical line — without it,
-- boundary detection would split on the pane's soft wraps.  Note that
-- tmux consumes OSC sequences (including OSC-133 shell-integration
-- markers) rather than reproducing them in the grid capture; only SGR and
-- anchored OSC-8 hyperlinks round-trip.  See doc/scrollback.org.
captureScrollback :: CaptureTarget -> IO Text
captureScrollback target = do
  mtmux <- lookupEnv "TMUX"
  case mtmux of
    Nothing -> gitqError
      "gitq: scrollback needs tmux ($TMUX not set) — run inside a tmux session"
    Just _ -> do
      let targetArgs = case target of
            CurrentPane   -> []
            NamedPane tgt -> ["-t", tgt]
          args = ["capture-pane", "-e", "-p", "-S", "-", "-J"] ++ targetArgs
      spawned <- try (createProcess (proc "tmux" args) { std_out = CreatePipe })
      case spawned of
        Left (_ :: IOException) ->
          gitqError "gitq: scrollback needs the tmux binary on $PATH"
        Right (_, mout, _, ph) -> do
          out <- maybe (pure BS.empty) BS.hGetContents mout
          code <- waitForProcess ph
          case code of
            ExitSuccess -> pure (TE.decodeUtf8With TEE.lenientDecode out)
            _ -> gitqError
              "gitq: scrollback capture failed (tmux capture-pane returned an error)"
