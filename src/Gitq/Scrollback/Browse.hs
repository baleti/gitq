{-# LANGUAGE OverloadedStrings #-}
{-# LANGUAGE ScopedTypeVariables #-}
-- | The interactive scrollback browser: a brick/vty TUI whose movement is
-- /entry-based/, not line-based — @j@/@k@ jump whole command+output
-- entries, which is the entire point of the feature (vim keys operating on
-- entry boundaries instead of the terminal's glyph grid).
--
-- brick + vty are the one heavy-dependency exception in an otherwise
-- stdlib-light codebase; hand-rolling raw-mode terminal handling is the
-- actual reliability risk brick exists to remove.
module Gitq.Scrollback.Browse
  ( runBrowser
  ) where

import Control.Exception (IOException, try)
import Control.Monad (void)
import Control.Monad.IO.Class (liftIO)
import Control.Monad.State (get, modify)
import Data.Maybe (fromMaybe)
import qualified Data.Set as Set
import Data.Set (Set)
import qualified Data.Text as T
import qualified Data.Text.IO as TIO
import Data.Text (Text)
import System.Directory (findExecutable, getTemporaryDirectory)
import System.IO (hClose, openTempFile)
import System.Process (readCreateProcess, proc)

import qualified Brick as B
import qualified Brick.Widgets.Border as Border
import qualified Brick.Widgets.Center as Center
import qualified Graphics.Vty as V

import Gitq.Scrollback.Ansi
  ( Style (..), StyledSpan (..) )
import Gitq.Scrollback.Entry (Entry (..))
import Gitq.Scrollback.Render (renderEntriesSexp)
import Gitq.Terminal (copyToClipboard)

-- | Widget name for the single scrolling viewport.  The selected entry is
-- marked 'B.visible' so brick scrolls it into view within this viewport.
data Name = OutputVp
  deriving (Eq, Ord, Show)

data Mode = Normal | Searching
  deriving (Eq, Show)

data BrowserState = BrowserState
  { bsEntries  :: ![Entry]
  , bsSelected :: !Int              -- ^ index into the *visible* entries
  , bsFolded   :: !(Set Int)        -- ^ entryIndex values whose output is collapsed
  , bsSearch   :: !(Maybe Text)     -- ^ active filter
  , bsMode     :: !Mode
  , bsInput    :: !Text             -- ^ in-progress search text
  , bsPendingG :: !Bool             -- ^ first 'g' of a 'gg' pressed
  , bsStatus   :: !Text
  }

-- | Launch the browser over a list of entries.  Returns when the user
-- quits; a no-op on an empty list (nothing to browse).
runBrowser :: [Entry] -> IO ()
runBrowser [] = putStrLn "gitq: (no scrollback entries)"
runBrowser entries = void (B.defaultMain app initial)
 where
  initial = BrowserState
    { bsEntries = entries, bsSelected = 0, bsFolded = Set.empty
    , bsSearch = Nothing, bsMode = Normal, bsInput = ""
    , bsPendingG = False, bsStatus = "" }
  app = B.App
    { B.appDraw         = draw
    , B.appChooseCursor = B.neverShowCursor
    , B.appHandleEvent  = handleEvent
    , B.appStartEvent   = pure ()
    , B.appAttrMap      = const attrs
    }

-- Visible entries under the active search filter ---------------------------

visibleEntries :: BrowserState -> [Entry]
visibleEntries st = case bsSearch st of
  Nothing -> bsEntries st
  Just q  -> filter (entryMatches q) (bsEntries st)

entryMatches :: Text -> Entry -> Bool
entryMatches q e =
  let ql = T.toLower q
      inCmd = maybe False (T.isInfixOf ql . T.toLower) (entryCommand e)
      inOut = any (T.isInfixOf ql . T.toLower . lineText) (entryOutput e)
  in inCmd || inOut

lineText :: [StyledSpan] -> Text
lineText = T.concat . map spanText

-- Drawing ------------------------------------------------------------------

draw :: BrowserState -> [B.Widget Name]
draw st = [B.vBox [body, statusBar st]]
 where
  vis = visibleEntries st
  body
    | null vis  = Center.center (B.txt "(no matching entries)")
    | otherwise = B.viewport OutputVp B.Vertical $
        B.vBox [ drawEntry st (i == bsSelected st) e | (i, e) <- zip [0 ..] vis ]

drawEntry :: BrowserState -> Bool -> Entry -> B.Widget Name
drawEntry st selected e = markSel (B.vBox (headerW : outputW))
 where
  markSel w
    | selected  = B.visible (B.withDefAttr selAttr w)
    | otherwise = w
  folded  = Set.member (entryIndex e) (bsFolded st)
  headerW = B.hBox
    [ B.withAttr idxAttr (B.txt (T.pack (pad 4 ('[' : show (entryIndex e) ++ "]"))))
    , B.txt " "
    , B.txt (foldMark folded)
    , B.txt " "
    , B.txt (fromMaybe "(no command)" (entryCommand e))
    , exitBadge (entryExitCode e)
    ]
  outputW
    | folded    = []
    | otherwise = map drawLine (entryOutput e)
  foldMark True  = "▸"
  foldMark False = "▾"

drawLine :: [StyledSpan] -> B.Widget Name
drawLine [] = B.txt " "                 -- keep blank output lines visible
drawLine spans = B.hBox (map drawSpan spans)
 where
  drawSpan (StyledSpan s t) = B.modifyDefAttr (const (styleToAttr s)) (B.txt (safe t))
  safe t = if T.null t then " " else t

exitBadge :: Maybe Int -> B.Widget Name
exitBadge Nothing  = B.emptyWidget
exitBadge (Just c) =
  let a = if c == 0 then okAttr else errAttr
  in B.hBox [B.txt "  ", B.withAttr a (B.txt (T.pack ("exit " ++ show c)))]

statusBar :: BrowserState -> B.Widget Name
statusBar st = Border.hBorder B.<=> B.withAttr statusAttr (B.txt line)
 where
  line = case bsMode st of
    Searching -> "/" <> bsInput st
    Normal    -> T.intercalate "   "
      ([ counter ] ++ [ st' | let st' = bsStatus st, not (T.null st') ]
        ++ [ "j/k move  h/l fold  gg/G ends  / search  y yank  e emacs  q quit" ])
  counter =
    let vis = visibleEntries st
    in T.pack (show (min (bsSelected st + 1) (max 1 (length vis)))
               ++ "/" ++ show (length vis))
      <> maybe "" (\q -> "  filter:" <> q) (bsSearch st)

-- Events -------------------------------------------------------------------

handleEvent :: B.BrickEvent Name e -> B.EventM Name BrowserState ()
handleEvent (B.VtyEvent ev) = do
  st <- get
  case bsMode st of
    Searching -> handleSearchKey ev
    Normal    -> handleNormalKey ev
handleEvent _ = pure ()

handleNormalKey :: V.Event -> B.EventM Name BrowserState ()
handleNormalKey ev = do
  st <- get
  let pendingG = bsPendingG st
  -- any key other than a second 'g' clears a pending 'gg'
  modify (\s -> s { bsPendingG = False, bsStatus = "" })
  case ev of
    V.EvKey (V.KChar 'g') [] | pendingG -> gotoFirst
                             | otherwise -> modify (\s -> s { bsPendingG = True })
    V.EvKey (V.KChar 'G') []  -> gotoLast
    V.EvKey (V.KChar 'j') []  -> moveSel 1
    V.EvKey (V.KChar 'k') []  -> moveSel (-1)
    V.EvKey V.KDown []        -> moveSel 1
    V.EvKey V.KUp   []        -> moveSel (-1)
    V.EvKey (V.KChar 'h') []  -> setFold True
    V.EvKey (V.KChar 'l') []  -> setFold False
    V.EvKey (V.KChar 'y') []  -> yankCommand
    V.EvKey (V.KChar 'e') []  -> sendToEmacs
    V.EvKey (V.KChar '/') []  -> modify (\s -> s { bsMode = Searching, bsInput = "" })
    V.EvKey (V.KChar 'q') []  -> B.halt
    V.EvKey V.KEsc []         -> modify (\s -> s { bsSearch = Nothing, bsSelected = 0 })
    _                         -> pure ()

handleSearchKey :: V.Event -> B.EventM Name BrowserState ()
handleSearchKey ev = case ev of
  V.EvKey V.KEnter [] -> modify commitSearch
  V.EvKey V.KEsc []   -> modify (\s -> s { bsMode = Normal, bsInput = "" })
  V.EvKey (V.KChar c) [] -> modify (\s -> s { bsInput = T.snoc (bsInput s) c })
  V.EvKey V.KBS [] -> modify (\s -> s { bsInput = dropLast (bsInput s) })
  _ -> pure ()
 where
  dropLast t = if T.null t then t else T.init t
  commitSearch s = s
    { bsMode = Normal
    , bsSearch = if T.null (bsInput s) then Nothing else Just (bsInput s)
    , bsSelected = 0 }

-- Movement / folding operate on the *visible* list ------------------------

moveSel :: Int -> B.EventM Name BrowserState ()
moveSel d = modify $ \s ->
  let n = length (visibleEntries s)
      i = clamp 0 (n - 1) (bsSelected s + d)
  in s { bsSelected = i }

gotoFirst :: B.EventM Name BrowserState ()
gotoFirst = modify (\s -> s { bsSelected = 0 })

gotoLast :: B.EventM Name BrowserState ()
gotoLast = modify (\s -> s { bsSelected = max 0 (length (visibleEntries s) - 1) })

setFold :: Bool -> B.EventM Name BrowserState ()
setFold fold = modify $ \s ->
  case selectedEntry s of
    Nothing -> s
    Just e  ->
      let ix = entryIndex e
      in s { bsFolded = (if fold then Set.insert else Set.delete) ix (bsFolded s) }

selectedEntry :: BrowserState -> Maybe Entry
selectedEntry s = case drop (bsSelected s) (visibleEntries s) of
  (e : _) -> Just e
  []      -> Nothing

-- Actions ------------------------------------------------------------------

yankCommand :: B.EventM Name BrowserState ()
yankCommand = do
  st <- get
  case selectedEntry st >>= entryCommand of
    Nothing  -> modify (\s -> s { bsStatus = "nothing to yank" })
    Just cmd -> do
      ok <- liftIO (copyToClipboard (T.unpack cmd))
      modify (\s -> s { bsStatus = if ok then "yanked command" else "no clipboard tool" })

-- | Send the selected entry to Emacs via emacsclient, using the same sexp
-- the CLI's @--scrollback --sexp@ produces.  A temp file sidesteps
-- emacsclient's argv escaping/length limits; Emacs deletes it after
-- reading (see gitq-scrollback-open-from-file).
sendToEmacs :: B.EventM Name BrowserState ()
sendToEmacs = do
  st <- get
  case selectedEntry st of
    Nothing -> modify (\s -> s { bsStatus = "no entry selected" })
    Just e  -> do
      found <- liftIO (findExecutable "emacsclient")
      case found of
        Nothing -> modify (\s -> s { bsStatus = "emacsclient not found" })
        Just _  -> do
          ok <- liftIO (openInEmacs [e])
          modify (\s -> s { bsStatus = if ok then "sent to Emacs" else "emacsclient failed" })

openInEmacs :: [Entry] -> IO Bool
openInEmacs es = do
  tmp <- getTemporaryDirectory
  (path, h) <- openTempFile tmp "gitq-scrollback.el"
  TIO.hPutStr h (renderEntriesSexp es)
  hClose h
  -- Emacs deletes the temp file after reading it, so we don't remove it here.
  let form = "(gitq-scrollback-open-from-file " ++ show path ++ ")"
  result <- try (readCreateProcess (proc "emacsclient" ["-e", form]) "")
  pure $ case result of
    Right _                 -> True
    Left (_ :: IOException) -> False

-- small helpers ------------------------------------------------------------

clamp :: Ord a => a -> a -> a -> a
clamp lo hi = max lo . min hi

pad :: Int -> String -> String
pad n s = s ++ replicate (max 0 (n - length s)) ' '

-- Attributes ---------------------------------------------------------------

selAttr, idxAttr, okAttr, errAttr, statusAttr :: B.AttrName
selAttr    = B.attrName "selected"
idxAttr    = B.attrName "index"
okAttr     = B.attrName "ok"
errAttr    = B.attrName "err"
statusAttr = B.attrName "status"

attrs :: B.AttrMap
attrs = B.attrMap V.defAttr
  [ (selAttr,    V.defAttr `V.withStyle` V.reverseVideo)
  , (idxAttr,    V.defAttr `V.withForeColor` V.brightBlack)
  , (okAttr,     V.defAttr `V.withForeColor` V.green)
  , (errAttr,    V.defAttr `V.withForeColor` V.red)
  , (statusAttr, V.defAttr `V.withStyle` V.bold)
  ]

-- | Map a parsed 'Style' to a vty attribute: SGR palette indices to vty
-- colours (ISO 0–15, Color240 for 16–255), and bold/underline/reverse to
-- vty styles.
styleToAttr :: Style -> V.Attr
styleToAttr s =
  let withMaybe f mc a = maybe a (\c -> f a (paletteColor c)) mc
      a0 = V.defAttr
      a1 = withMaybe V.withForeColor (styleFg s) a0
      a2 = withMaybe V.withBackColor (styleBg s) a1
      a3 = if styleBold s      then a2 `V.withStyle` V.bold        else a2
      a4 = if styleUnderline s then a3 `V.withStyle` V.underline   else a3
      a5 = if styleReverse s   then a4 `V.withStyle` V.reverseVideo else a4
  in a5

paletteColor :: Int -> V.Color
paletteColor c
  | c < 16    = V.ISOColor (fromIntegral c)
  | c < 256   = V.Color240 (fromIntegral (c - 16))
  | otherwise = V.ISOColor 7
