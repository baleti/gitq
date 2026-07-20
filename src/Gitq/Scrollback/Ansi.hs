{-# LANGUAGE OverloadedStrings #-}
-- | A small SGR (Select Graphic Rendition) scanner over captured pane
-- text.
--
-- @tmux capture-pane -e@ has already done the hard part — it resolved
-- cursor motion, overwrites, and scrolling into a flat grid, then
-- re-emitted the visible attributes as plain SGR.  What is left in the
-- text is SGR colour/attribute codes (and the occasional stray non-SGR
-- CSI or OSC that survived), not a live terminal.  So this is a ~150-line
-- hand-rolled scanner, deliberately /not/ a VT100 emulator: it turns SGR
-- runs into styled spans and silently drops everything else, keeping the
-- "stay in Haskell, no heavy deps" property.
--
-- tmux emits SGR split and restated per line (an emitted @ESC[1;31m@ comes
-- back as @ESC[1m ESC[31m@, and the active attribute is repeated at the
-- start of each captured line), so callers thread the returned 'Style'
-- into the next line — see 'parseAnsiLine'.
module Gitq.Scrollback.Ansi
  ( Style (..)
  , defaultStyle
  , StyledSpan (..)
  , parseAnsiLine
  , visibleText
  , spansToAnsi
  ) where

import Data.Char (isDigit)
import qualified Data.Text as T
import Data.Text (Text)

-- | The graphic attributes a span carries.  Colours are SGR palette
-- indices (0–15 for the 8+bright set, 0–255 for 256-colour); 'Nothing'
-- means the terminal default.  Truecolour (@38;2;r;g;b@) is out of scope
-- and clears the colour to default rather than approximating it.
data Style = Style
  { styleFg        :: !(Maybe Int)
  , styleBg        :: !(Maybe Int)
  , styleBold      :: !Bool
  , styleUnderline :: !Bool
  , styleReverse   :: !Bool
  } deriving (Eq, Show)

defaultStyle :: Style
defaultStyle = Style Nothing Nothing False False False

data StyledSpan = StyledSpan
  { spanStyle :: !Style
  , spanText  :: !Text
  } deriving (Eq, Show)

-- | Parse one line of possibly-SGR-laden text into styled spans, carrying
-- the style forward from the end of the previous line (SGR state persists
-- across newlines in a real terminal).  The returned 'Style' is that
-- trailing state; pass it to the next line's call to thread a whole
-- buffer.  Adjacent runs with the same style are coalesced (so a dropped
-- CSI/OSC in the middle of otherwise-uniform text does not split a span).
-- | The visible text of a line, with all escape sequences stripped — used
-- to test a captured line against the prompt regex and to recover the
-- typed command from a prompt line.
visibleText :: Text -> Text
visibleText = T.concat . map spanText . snd . parseAnsiLine defaultStyle

-- | Re-render styled spans back to text with real SGR escapes, so
-- @--scrollback@ output piped to @less -R@ shows colour, and the @--sexp@
-- @:output@ string carries ANSI for Emacs's @ansi-color@ to apply.  A
-- default-styled span is emitted bare; a styled one is wrapped in its SGR
-- and a trailing reset, so spans stay self-contained.
spansToAnsi :: [StyledSpan] -> Text
spansToAnsi = T.concat . map render
 where
  render (StyledSpan s t)
    | s == defaultStyle = t
    | otherwise         = styleSgr s <> t <> "\ESC[0m"

-- | The SGR sequence that sets exactly this style starting from default.
styleSgr :: Style -> Text
styleSgr s = "\ESC[" <> T.intercalate ";" codes <> "m"
 where
  codes = concat
    [ ["1" | styleBold s]
    , ["4" | styleUnderline s]
    , ["7" | styleReverse s]
    , colorCodes "3" "9" "38" (styleFg s)
    , colorCodes "4" "10" "48" (styleBg s)
    ]
  -- normal 0-7 use prefix <n0>+index, bright 8-15 use <n1>+(index-8),
  -- 256 use <ext>;5;index
  colorCodes n0 n1 ext = \mc -> case mc of
    Nothing -> []
    Just c
      | c < 8     -> [n0 <> tShow c]
      | c < 16    -> [n1 <> tShow (c - 8)]
      | otherwise -> [ext <> ";5;" <> tShow c]
  tShow = T.pack . show

parseAnsiLine :: Style -> Text -> (Style, [StyledSpan])
parseAnsiLine sty0 t0 =
  let (sty, spansRev) = go sty0 [] [] t0 in (sty, reverse spansRev)
 where
  -- pend: reversed text chunks of the still-open span; spans: completed
  -- spans in reverse.  A style change flushes the open span under the
  -- *old* style before the new run starts.
  go :: Style -> [Text] -> [StyledSpan] -> Text -> (Style, [StyledSpan])
  go sty pend spans t =
    let (chunk, rest) = T.break (== '\ESC') t
        pend' = if T.null chunk then pend else chunk : pend
    in case T.uncons rest of
         Nothing -> (sty, flush sty pend' spans)
         Just (_esc, afterEsc) -> case scanEscape afterEsc of
           SgrParams params rest' ->
             let sty' = applySgr sty (parseParams params)
             in if sty' == sty
                  then go sty pend' spans rest'                 -- no visible change
                  else go sty' [] (flush sty pend' spans) rest' -- close old, open new
           Dropped rest' -> go sty pend' spans rest'            -- merge across dropped seq
           NoSeq         -> go sty pend' spans afterEsc         -- lone ESC, drop it
   where
    flush s p sp = case T.concat (reverse p) of
      j | T.null j  -> sp
        | otherwise -> StyledSpan s j : sp

-- | An escape sequence recognised (or ignored) by the scanner.
data Esc
  = SgrParams !Text !Text  -- ^ SGR parameter text, and the remainder after final @m@
  | Dropped !Text          -- ^ a non-SGR CSI / OSC sequence; carries the remainder to keep scanning
  | NoSeq                  -- ^ the ESC did not begin a recognised sequence

-- | Classify the text immediately after an @ESC@.  A CSI (@ESC [@) ending
-- in @m@ is SGR; any other CSI final byte, or an OSC (@ESC ]@ … ST/BEL),
-- is dropped.  Anything else is 'NoSeq'.
scanEscape :: Text -> Esc
scanEscape t = case T.uncons t of
  Just ('[', rest) ->
    let (params, afterParams) = T.span isParamByte rest
        (_inter, afterInter)  = T.span isInterByte afterParams
    in case T.uncons afterInter of
         Just (fin, rest')
           | fin == 'm'   -> SgrParams params rest'
           | isFinalByte fin -> Dropped rest'
           | otherwise    -> Dropped afterInter   -- malformed; resync past it
         Nothing          -> Dropped ""           -- truncated CSI at end of text
  Just (']', rest) -> Dropped (dropOsc rest)
  _                -> NoSeq
 where
  isParamByte c = c >= '\x30' && c <= '\x3f'      -- 0-9 : ; < = > ?
  isInterByte c = c >= '\x20' && c <= '\x2f'      -- space ! " # ... /
  isFinalByte c = c >= '\x40' && c <= '\x7e'      -- @ A-Z [ ... ~

-- | Skip an OSC body up to its terminator (ST = @ESC \\@, or a bare BEL),
-- returning the text after it.  An unterminated OSC swallows the rest.
dropOsc :: Text -> Text
dropOsc = go
 where
  go t = case T.uncons t of
    Nothing -> T.empty
    Just ('\a', rest) -> rest                       -- BEL terminator
    Just ('\ESC', rest) -> case T.uncons rest of
      Just ('\\', rest') -> rest'                    -- ST terminator (ESC \)
      _                  -> go rest
    Just (_, rest) -> go rest

-- | Split an SGR parameter string on @;@ into codes; an empty field (or a
-- wholly empty parameter string, i.e. @ESC[m@) is 0 (reset).
parseParams :: Text -> [Int]
parseParams ps
  | T.null ps = [0]
  | otherwise = map field (T.splitOn ";" ps)
 where
  field f
    | T.null f              = 0
    | T.all isDigit f       = read (T.unpack f)
    | otherwise             = 0   -- e.g. a private @:@-subparam; treat as 0

-- | Fold a list of SGR codes into a 'Style'.  38/48 consume their
-- @5;N@ (256-colour) or @2;r;g;b@ (truecolour) operands from the tail.
applySgr :: Style -> [Int] -> Style
applySgr = go
 where
  go s [] = s
  go s (c : cs) = case c of
    0  -> go defaultStyle cs
    1  -> go s { styleBold = True } cs
    4  -> go s { styleUnderline = True } cs
    7  -> go s { styleReverse = True } cs
    22 -> go s { styleBold = False } cs
    24 -> go s { styleUnderline = False } cs
    27 -> go s { styleReverse = False } cs
    39 -> go s { styleFg = Nothing } cs
    49 -> go s { styleBg = Nothing } cs
    38 -> case cs of
            (5 : n : rest)        -> go s { styleFg = Just n } rest
            (2 : _ : _ : _ : rest) -> go s { styleFg = Nothing } rest
            _                     -> go s cs
    48 -> case cs of
            (5 : n : rest)        -> go s { styleBg = Just n } rest
            (2 : _ : _ : _ : rest) -> go s { styleBg = Nothing } rest
            _                     -> go s cs
    _  | c >= 30 && c <= 37   -> go s { styleFg = Just (c - 30) } cs
       | c >= 90 && c <= 97   -> go s { styleFg = Just (c - 90 + 8) } cs
       | c >= 40 && c <= 47   -> go s { styleBg = Just (c - 40) } cs
       | c >= 100 && c <= 107 -> go s { styleBg = Just (c - 100 + 8) } cs
       | otherwise            -> go s cs
