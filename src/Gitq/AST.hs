-- | The pipeline AST: @source step* terminal?@.
module Gitq.AST
  ( Pipeline (..)
  , Source (..)
  , Step (..)
  , Morphism (..)
  , EntryFilter (..)
  , Cond (..)
  , Op (..)
  , Terminal (..)
  ) where

import Gitq.Frame (Value)

data Pipeline = Pipeline
  { pipeSource   :: Source
  , pipeSteps    :: [Step]
  , pipeTerminal :: Maybe Terminal
  } deriving (Eq, Show)

data Source
  = SCommits (Maybe String)   -- ^ optional revision range (@commits in main..HEAD@)
  | SBranches
  | STags
  | SRefs
  | SWorktrees
  | SBlobs
  | SRef String               -- ^ @HEAD@, a branch, tag, or SHA — resolves to one commit
  deriving (Eq, Show)

data EntryFilter = FBlob | FTree
  deriving (Eq, Show)

-- | A typed map from one frame shape to a /list/ of frames of another.
-- Composition is generic (Kleisli over the list monad); the registry in
-- "Gitq.Registry" carries each morphism's domain and codomain.
data Morphism
  = MParent                         -- ^ all parents
  | MParentIdx Int                  -- ^ @parent[N]@
  | MParentStar                     -- ^ ancestor closure, inclusive
  | MParentPlus                     -- ^ ancestor closure, exclusive
  | MParentAdjoint                  -- ^ @parent†@ — children-of
  | MTree
  | MTreeEntries (Maybe EntryFilter)
  | MDiff (Maybe String)            -- ^ optional REF to diff against
  | MDiffHunks
  | MDiffLines
  | MHistory
  | MCommit
  deriving (Eq, Show)

data Step
  = StVia Morphism
  | StWhere [Cond]
  | StGrep String Bool              -- ^ pattern, regex?
  | StPickaxe String Bool           -- ^ pattern, regex?
  | StPath String                   -- ^ glob
  | StPick [String]
  | StTake Int
  | StSkip Int
  | StFirst
  | StLast
  | StSort String Bool              -- ^ field, descending?
  | StInRange String                -- ^ restrict to commits reachable per a
                                    --   raw revspec (mid-pipeline @in@;
                                    --   git parses the string, we don't)
  | StContext Int [(String, Bool)]  -- ^ keep N lines around pattern matches
                                    --   in @content@; patterns (with
                                    --   regex flags) are baked in at parse
                                    --   time — explicit, or inherited from
                                    --   preceding content searches
  deriving (Eq, Show)

data Cond = Cond
  { condField :: String
  , condOp    :: Op
  , condValue :: Value
  } deriving (Eq, Show)

data Op
  = OpEq | OpNe | OpGt | OpLt | OpGe | OpLe
  | OpContains        -- ^ never typed explicitly: the implicit substring match
  | OpRegex
  | OpAfter | OpBefore | OpWithin
  | OpIs
  deriving (Eq, Show)

data Terminal
  = TShow
  | TCopy
  | TInsert
  | TCount
  | TRemove                          -- ^ /delete is a parse-time alias
  | TStage
  | TBranchOff (Maybe String) (Maybe String)  -- ^ name, worktree path
  | TAmend Bool (Maybe String)                -- ^ no-edit?, message
  | TSquash (Maybe String)
  | TReword (Maybe String)
  | TCommit (Maybe String)
  | TMark (Maybe String)
  | TWorktree (Maybe String)
  deriving (Eq, Show)
