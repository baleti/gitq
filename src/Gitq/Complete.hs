-- | Context-aware completion: candidates for the token being typed,
-- derived from the same registries and (via 'inferFields') the same parser
-- the strict pipeline parser uses — so completion can never offer
-- something the parser would then reject.
module Gitq.Complete
  ( completeCandidates
  ) where

import qualified Data.Set as Set
import qualified Data.Text as T
import Gitq.Parse (inferFields)
import Gitq.Registry
import Gitq.Tokenize
import Gitq.Git (runGit)
import Gitq.AST (Morphism)

-- | Candidates for the pipeline string typed so far.  Completions extend
-- the last partial word; the caller (zsh, Emacs) filters against it.
completeCandidates :: String -> IO [String]
completeCandidates input = do
  let trimmed = reverse (dropWhile (`elem` " \t\n\r") (reverse input))
      trailing = trimmed /= input
      tokens = tokenize trimmed
      -- fully-typed tokens (the final partial word is being completed)
      ctx = if trailing then tokens else dropLast tokens
      n = length ctx
      lastCtx = if n > 0 then Just (ctx !! (n - 1)) else Nothing
      prevCtx = if n > 1 then Just (ctx !! (n - 2)) else Nothing
  case () of
    _ | n == 0 -> pure completeSourceKeywords
      -- after "commits" → "in" or steps/terminals
      | n == 1, lastCtx == Just "commits" ->
          pure ("in" : stepKeywords ++ completeTerminals)
      -- after "commits in" → refs
      | lastCtx == Just "in", prevCtx == Just "commits" -> completeRefs
      -- after "via" → morphisms valid for the frame type flowing in
      | lastCtx == Just "via" -> do
          let fields = inferFields (dropLast ctx)
          pure [ m | m <- completeMorphisms
               , maybe True (`elem` fields) (headRequires m) ]
      -- after "via diff" → optional REF, or skip ahead
      | lastCtx `elem` [Just "diff", Just ".diff"], prevCtx == Just "via" -> do
          refs <- completeRefs
          pure (refs ++ stepKeywords ++ completeTerminals)
      -- after "where" or "," → fields of the current frame type (a comma
      -- inside pick lands here too, with the same answer)
      | lastCtx == Just "where" || lastCtx == Just "," ->
          pure (currentTypeFields ctx)
      -- after a field inside a where clause → operators and (for
      -- implicit-contains-eligible types) value candidates
      | Just fieldTok <- lastCtx
      , enclosingStep ctx == Just "where"
      , fieldTok `elem` currentTypeFields ctx -> do
          vals <- if fieldType fieldTok `elem` implicitContainsTypes
                    then completeWhereValues fieldTok Nothing
                    else pure []
          pure (completeWhereOperators ++ vals)
      -- after "sort" → fields with optional - prefix
      | lastCtx == Just "sort" ->
          let fields = currentTypeFields ctx
          in pure (fields ++ map ('-' :) fields)
      -- after "pick" → fields flowing into pick
      | lastCtx == Just "pick" -> pure (currentTypeFields ctx)
      -- after a where-operator → dynamic values
      | Just op <- lastCtx, op `elem` completeWhereOperators ->
          case prevCtx of
            Just field -> completeWhereValues field (Just op)
            Nothing    -> pure []
      -- after a terminal: only its own optional argument may follow
      | Just t <- lastCtx, isTerminalToken t ->
          pure (if t == "/amend" then ["no-edit"] else [])
      -- otherwise → steps + terminals (+ "," inside where/pick)
      | otherwise ->
          pure ((if enclosingStep ctx `elem` [Just "where", Just "pick"] then [","] else [])
                ++ stepKeywords ++ completeTerminals)
 where
  dropLast xs = take (max 0 (length xs - 1)) xs

-- | The field the head morphism of a completion candidate path requires,
-- via the same path parser and registry the pipeline parser uses.
headRequires :: String -> Maybe String
headRequires path = case parseMorphismPath path of
  Right (m : _) -> Just (morphismRequires (m :: Morphism))
  _             -> Nothing

-- | The most recent step keyword in the context, walking in order so that
-- @path@ right after @where@\/@pick@, a comma, or another field is treated
-- as a field reference continuing that stage, not a fresh @path@ step.
enclosingStep :: [String] -> Maybe String
enclosingStep ctx = go (zip [0 ..] ctx) Nothing
 where
  go [] acc = acc
  go ((i, tok) : rest) acc
    | tok `elem` stepKeywords
    , not (fieldContinuation i tok acc) = go rest (Just tok)
    | otherwise = go rest acc
  fieldContinuation i tok acc =
    tok `elem` fieldNames
      && acc `elem` [Just "where", Just "pick"]
      && (let prev = if i > 0 then Just (ctx !! (i - 1)) else Nothing
          in prev `elem` [Just "where", Just "pick", Just ","]
             || maybe False (`elem` fieldNames) prev)

-- | Fields valid to offer as where\/sort\/pick candidates at the end of
-- the context: the field-set flowing /into/ the enclosing stage.
currentTypeFields :: [String] -> [String]
currentTypeFields ctx =
  case enclosingStep ctx of
    Just stage ->
      case lastIndexOf stage ctx of
        Just i  -> inferFields (take i ctx)
        Nothing -> inferFields ctx
    Nothing -> inferFields ctx
 where
  lastIndexOf x xs =
    case [i | (i, t) <- zip [0 ..] xs, t == x] of
      [] -> Nothing
      is -> Just (last is)

-- | Local branch and tag names, for contexts expecting a ref.
completeRefs :: IO [String]
completeRefs = do
  branches <- runGit ["branch", "--format=%(refname:short)"]
  tags <- runGit ["tag", "--list"]
  pure (map T.unpack (branches ++ tags))

-- | Value candidates for a where-condition, or [] for fields with no
-- natural git-derivable value domain.
completeWhereValues :: String -> Maybe String -> IO [String]
completeWhereValues field op = case (field, op) of
  ("date", Just "within") -> pure completeDateWithinExamples
  ("author", _) -> dedup ["log", "--format=%an", "--all"]
  ("email", _)  -> dedup ["log", "--format=%ae", "--all"]
  ("date", _)   -> dedup ["log", "--format=%ai", "--all"]
  (f, _) | f `elem` ["sha", "commit-sha"] -> dedup ["log", "--format=%h", "--all"]
  ("path", _)   -> dedup ["log", "--all", "--name-only", "--format="]
  (f, _) | f `elem` ["name", "branch"] -> completeRefs
  _ -> pure []
 where
  -- order-preserving Set-based dedup: plain nub is quadratic, and value
  -- candidates can be one line per commit in the repo
  dedup args = ordNub . map T.unpack <$> runGit args
  ordNub = go Set.empty
   where
    go _ [] = []
    go seen (x : xs)
      | x `Set.member` seen = go seen xs
      | otherwise           = x : go (Set.insert x seen) xs
