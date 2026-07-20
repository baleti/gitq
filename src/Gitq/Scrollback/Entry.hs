{-# LANGUAGE OverloadedStrings #-}
-- | Splitting a captured scrollback buffer into /entries/ — one shell
-- command plus its output.
--
-- The boundary problem has two strategies, and which one runs is decided
-- per buffer, never mixed:
--
--   * __Heuristic prompt detection__ (the real primary path under tmux):
--     a line that matches a prompt regex starts a new entry; the visible
--     text after the prompt is the command, the lines until the next
--     prompt are the output.  Best-effort by nature — a command line that
--     itself looks like a prompt fools it — and configurable via
--     @GITQ_SCROLLBACK_PROMPT_REGEX@ (a POSIX ERE), which the CLI passes
--     in.
--
--   * __OSC-133 markers__ (exact, but /not/ recoverable from a tmux
--     capture — see doc/scrollback.org: tmux consumes OSC-133).  Kept for
--     buffers from any future marker-preserving source; it only engages
--     when @ESC ] 133 ; A@ actually appears, so under tmux it never does.
--
-- An 'Entry' is deliberately not a 'Gitq.Frame.Frame' — a scrollback
-- entry is not a git object — so it gets its own record and renderers.
module Gitq.Scrollback.Entry
  ( Entry (..)
  , EntrySource (..)
  , defaultPromptRegex
  , parseEntries
  , parseEntriesWith
  ) where

import qualified Data.Text as T
import Data.Text (Text)
import Text.Regex.TDFA (Regex, makeRegex, match)
import Text.Regex.TDFA.Text ()
import Gitq.Scrollback.Ansi (StyledSpan, defaultStyle, parseAnsiLine, visibleText)

data EntrySource = FromMarkers | FromHeuristic
  deriving (Eq, Show)

-- | One shell command and its output.
data Entry = Entry
  { entryIndex    :: !Int             -- ^ 0-based, oldest first
  , entryCommand  :: !(Maybe Text)    -- ^ 'Nothing' when no command line was recoverable
  , entryOutput   :: ![[StyledSpan]]  -- ^ styled output lines, in order
  , entryExitCode :: !(Maybe Int)     -- ^ from OSC-133;D; always 'Nothing' on the heuristic path
  , entrySource   :: !EntrySource
  } deriving (Eq, Show)

-- | Default prompt matcher, anchored at the start of the (ANSI-stripped)
-- line.  Catches the common interactive shapes — a no-space run ending in
-- a @$@ or @%@ sigil (@demo$ @, @user\@host:~/proj$ @, bare @$ @), or a
-- non-empty run ending in @#@ or @>@ (@root# @, @host> @).  @#@/@>@ require
-- a non-empty prefix so leading markdown headings and quoted lines in
-- captured output are less likely to be mistaken for prompts.  Deliberately
-- best-effort — see the module note; override with
-- @GITQ_SCROLLBACK_PROMPT_REGEX@.
defaultPromptRegex :: Text
defaultPromptRegex = "^([^[:space:]]*[$%] |[^[:space:]]+[#>] )"

-- | Split a buffer with the default prompt regex.
parseEntries :: Text -> [Entry]
parseEntries = parseEntriesWith defaultPromptRegex

-- | Split a buffer, choosing the strategy by content: markers if any
-- @ESC ] 133 ; A@ is present anywhere, otherwise the heuristic with the
-- given prompt regex.  A buffer with no boundary at all yields a single
-- entry covering everything, never an empty list or a crash.
parseEntriesWith :: Text -> Text -> [Entry]
parseEntriesWith promptRx raw
  | hasMarkers raw = reindex (parseMarkers raw)
  | otherwise      = reindex (parseHeuristic (makeRegex promptRx :: Regex) raw)
 where
  reindex = zipWith (\i e -> e { entryIndex = i }) [0 ..]

-- Heuristic path ------------------------------------------------------------

-- | A group is a command (from a prompt line) plus the raw output lines
-- that follow it, until the next prompt.  Leading output with no owning
-- command becomes a group with 'Nothing'.
parseHeuristic :: Regex -> Text -> [Entry]
parseHeuristic rx raw = map mk (groups Nothing [] (trimTrailingBlank (T.lines raw)))
 where
  -- tmux pads the capture with the pane's blank rows below the cursor;
  -- drop trailing whitespace-only lines so the bare trailing prompt
  -- collapses to an empty, dropped group instead of a "(no command)" entry
  trimTrailingBlank = reverse . dropWhile (T.null . T.strip . visibleText) . reverse
  groups cmd acc [] = emit cmd acc []
  groups cmd acc (l : ls) = case promptCommand rx l of
    -- an empty command (bare prompt, e.g. the trailing live prompt every
    -- capture ends on) becomes Nothing, so a content-free group is dropped
    Just command -> emit cmd acc (groups (emptyToNothing command) [] ls)
    Nothing      -> groups cmd (l : acc) ls
  emptyToNothing t = if T.null t then Nothing else Just t
  -- yield the in-progress group only if it carries something
  emit cmd acc rest
    | cmd == Nothing && null acc = rest
    | otherwise                  = (cmd, reverse acc) : rest

  mk (cmd, outLines) = Entry
    { entryIndex    = 0    -- set by parseEntriesWith
    , entryCommand  = cmd
    , entryOutput   = styleLines outLines
    , entryExitCode = Nothing
    , entrySource   = FromHeuristic
    }

-- | If a line looks like a prompt, the command typed after it (the visible
-- remainder), else 'Nothing'.  Runs against the ANSI-stripped line so a
-- coloured prompt still matches.
promptCommand :: Regex -> Text -> Maybe Text
promptCommand rx line =
  let vis = visibleText line
      (before, matched, after) = match rx vis :: (Text, Text, Text)
  in if T.null matched || not (T.null before)
       then Nothing
       else Just (T.strip after)

-- Marker path (OSC-133) -----------------------------------------------------

introMarker :: Text
introMarker = "\ESC]133;"

hasMarkers :: Text -> Bool
hasMarkers t = ("\ESC]133;A" `T.isInfixOf` t)

-- | A recognised 133 marker.
data Mark = MPromptStart | MCmdStart | MOutputStart | MCmdEnd (Maybe Int)

-- | Walk the 133 token stream, building entries.  Text between @;B@ and
-- @;C@ is the command; between @;C@ and @;D@ is output; @;D;code@ closes
-- the entry with its exit code.  Prompt text (@;A@..@;B@) is discarded.
parseMarkers :: Text -> [Entry]
parseMarkers raw = build Nothing [] (tokenize raw)
 where
  -- build carries the current command and the reversed output text so far.
  -- Text between ;B and ;C accumulates as the command; between ;C and ;D as
  -- output; ;A and ;D close the current entry.
  build cmd out [] = closeIf cmd out []
  build cmd out (tok : toks) = case tok of
    Left txt             -> build cmd (txt : out) toks
    Right MPromptStart   -> closeIf cmd out (build Nothing [] toks)
    Right MCmdStart      -> build (Just "") [] toks       -- discard prompt text, await command
    Right MOutputStart   -> build (grab cmd out) [] toks  -- accumulated text was the command
    Right (MCmdEnd code) -> entry cmd code out : build Nothing [] toks
   where
    grab (Just _) acc = Just (T.strip (T.concat (reverse acc)))
    grab Nothing  _   = Nothing

  closeIf Nothing [] rest = rest
  closeIf cmd     out rest = entry cmd Nothing out : rest

  entry cmd code out = Entry
    { entryIndex    = 0
    , entryCommand  = cmd
    , entryOutput   = styleLines (T.lines (T.concat (reverse out)))
    , entryExitCode = code
    , entrySource   = FromMarkers
    }

-- | Tokenise a buffer into interleaved plain text ('Left') and 133 markers
-- ('Right'), splitting on @ESC ] 133 ;@ up to each OSC terminator.
tokenize :: Text -> [Either Text Mark]
tokenize t =
  case T.breakOn introMarker t of
    (pre, rest)
      | T.null rest -> [Left pre | not (T.null pre)]
      | otherwise ->
          let body0 = T.drop (T.length introMarker) rest
              (payload, afterTerm) = breakOscTerm body0
          in [Left pre | not (T.null pre)]
               ++ maybe [] (\m -> [Right m]) (parseMark payload)
               ++ tokenize afterTerm

-- | Break an OSC body at its terminator (ST @ESC \\@ or BEL), returning the
-- payload before it and the text after.
breakOscTerm :: Text -> (Text, Text)
breakOscTerm = go T.empty
 where
  go acc t = case T.uncons t of
    Nothing -> (acc, T.empty)
    Just ('\a', rest) -> (acc, rest)
    Just ('\ESC', rest) -> case T.uncons rest of
      Just ('\\', rest') -> (acc, rest')
      _                  -> go (T.snoc acc '\ESC') rest
    Just (c, rest) -> go (T.snoc acc c) rest

parseMark :: Text -> Maybe Mark
parseMark payload = case T.uncons payload of
  Just ('A', _) -> Just MPromptStart
  Just ('B', _) -> Just MCmdStart
  Just ('C', _) -> Just MOutputStart
  Just ('D', rest) -> Just (MCmdEnd (parseExit rest))
  _             -> Nothing
 where
  parseExit r = case T.stripPrefix ";" r of
    Just codeT | not (T.null codeT), T.all (`elem` ("0123456789" :: String)) codeT
               -> Just (read (T.unpack codeT))
    _          -> Nothing

-- Shared --------------------------------------------------------------------

-- | Parse raw output lines into styled spans, threading SGR state across
-- the lines of a single entry (reset to default at each entry boundary).
styleLines :: [Text] -> [[StyledSpan]]
styleLines = go defaultStyle
 where
  go _   []       = []
  go sty (l : ls) = let (sty', spans) = parseAnsiLine sty l in spans : go sty' ls
