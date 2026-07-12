{-# LANGUAGE OverloadedStrings #-}
-- | Frames: the flat, typed records every pipeline value is a list of.
--
-- A frame's /shape/ is the set of fields it carries; shapes are identified
-- structurally (by field-set), not nominally — see doc/gitq.org.
--
-- All textual data is strict 'Text' (UTF-8 internally since text-2.0):
-- field values are usually zero-copy slices of the one decoded git-output
-- buffer, which is what keeps 81k-commit histories in memory-bandwidth
-- territory instead of cons-cell territory.
module Gitq.Frame
  ( Value (..)
  , Frame (..)
  , frame
  , derivedFrame
  , commitContext
  , frameField
  , frameCommitSha
  , valueString
  , truthy
  ) where

import qualified Data.Map.Strict as M
import Data.Text (Text)

-- | A scalar field value.  Dates and SHAs are text at runtime; their
-- scalar /type/ (which operators apply) lives in "Gitq.Registry".
data Value
  = VStr !Text
  | VNum !Int
  | VBool !Bool
  deriving (Eq, Ord, Show)

-- | One record flowing through a pipeline.  @frameParents@ is only ever
-- non-empty on commit frames; it backs the computed @parents-count@ field
-- and the @parent@ morphism.
data Frame = Frame
  { frameType    :: !Text             -- ^ runtime tag: commit, ref, worktree, blob, tree, diff, hunk, line, diff-line, projection
  , frameParents :: ![Text]
  , frameAttrs   :: !(M.Map Text Value)
  } deriving (Eq, Show)

-- | Build a frame from a tag and attribute pairs.
frame :: Text -> [(Text, Value)] -> Frame
frame tag attrs = Frame tag [] (M.fromList attrs)

-- | The shared commit context a derived frame carries along: the owning
-- commit's author, date, and message (whichever are present on the
-- parent).  For the categorically minded, frames-with-context form an
-- environment comonad; this is its extract-and-reattach.
commitContext :: Frame -> [(Text, Value)]
commitContext parent =
  [ (k, v) | k <- ["author", "date", "message"]
  , Just v <- [frameField parent k] ]

-- | Build a frame derived from a parent frame — hunks, diff lines, grep
-- lines.  Context propagation happens here, as a property of
-- construction, so a future derived shape cannot forget to carry the
-- commit metadata over (grep's line frames did exactly that for two
-- releases while hunk and diff-line frames carried it).
derivedFrame :: Frame -> Text -> [(Text, Value)] -> Frame
derivedFrame parent tag attrs = frame tag (attrs ++ commitContext parent)

-- | Extract a field from a frame.  @author@ falls back to @name@ (so ref
-- frames answer author-flavored queries with their name); @parents-count@
-- is computed from the parents list.
frameField :: Frame -> Text -> Maybe Value
frameField f "author" =
  case M.lookup "author" (frameAttrs f) of
    Just v  -> Just v
    Nothing -> M.lookup "name" (frameAttrs f)
frameField f "parents-count" =
  case M.lookup "parents-count" (frameAttrs f) of
    Just v  -> Just v                 -- a pick-projected parents-count
    Nothing -> Just (VNum (length (frameParents f)))
frameField f field = M.lookup field (frameAttrs f)

-- | The commit SHA a frame refers to: its own @commit-sha@ back-pointer if
-- it has one (hunk, line, diff-line frames), else its @sha@.
frameCommitSha :: Frame -> Maybe Text
frameCommitSha f =
  case frameField f "commit-sha" of
    Just (VStr s) -> Just s
    _ ->
      case frameField f "sha" of
        Just (VStr s) -> Just s
        _             -> Nothing

-- | Text content of a value, if it is one.
valueString :: Value -> Maybe Text
valueString (VStr s) = Just s
valueString _        = Nothing

-- | Elisp-style truthiness: everything but an explicit false is true.
truthy :: Maybe Value -> Bool
truthy Nothing            = False
truthy (Just (VBool b))   = b
truthy (Just _)           = True
