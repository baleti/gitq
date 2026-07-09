-- | GitQ test suite: tokenizer, parser (incl. the fail-loud error
-- catalogue and the P1–P7 grammar disambiguation properties), registry
-- coherence, completion, and integration against a real scratch git repo.
module Main (main) where

import Control.Monad (forM_, unless, when)
import Data.IORef
import Data.List (isInfixOf, isPrefixOf, sort)
import qualified Data.Map.Strict as M
import System.Directory
import System.Exit (exitFailure, exitSuccess)
import System.FilePath ((</>))
import System.Process (readCreateProcess, proc, CreateProcess (..))

import Gitq.AST
import Gitq.Complete (completeCandidates)
import Gitq.Exec (execPipeline)
import Gitq.Frame
import Gitq.Parse
import Gitq.Registry
import Gitq.Render (renderFrameSexp)
import Gitq.Tokenize

-- Tiny harness -------------------------------------------------------------

type Counter = IORef (Int, Int)

check :: Counter -> String -> Bool -> IO ()
check ref name ok = do
  modifyIORef' ref (\(p, f) -> if ok then (p + 1, f) else (p, f + 1))
  unless ok (putStrLn ("FAIL: " ++ name))

-- | Expect a parse error whose message contains a substring.
perr :: Counter -> String -> String -> IO ()
perr ref pipeline needle = case parsePipeline pipeline of
  Left msg -> check ref ("error <" ++ pipeline ++ "> mentions " ++ show needle)
                        (needle `isInfixOf` msg)
  Right _ -> check ref ("error <" ++ pipeline ++ "> (expected failure, parsed fine)") False

-- | Expect a pipeline to parse.
pok :: Counter -> String -> IO ()
pok ref pipeline = check ref ("parses <" ++ pipeline ++ ">")
                             (either (const False) (const True) (parsePipeline pipeline))

parsed :: String -> Pipeline
parsed s = either error id (parsePipeline s)

main :: IO ()
main = do
  ref <- newIORef (0, 0)
  tokenizerTests ref
  parserTests ref
  failLoudTests ref
  propertyTests ref
  registryTests ref
  inferTests ref
  renderTests ref
  integrationTests ref
  (p, f) <- readIORef ref
  putStrLn (show p ++ " passed, " ++ show f ++ " failed")
  if f == 0 then exitSuccess else exitFailure

-- Tokenizer -----------------------------------------------------------------

tokenizerTests :: Counter -> IO ()
tokenizerTests ref = do
  let t = tokenize
  check ref "basic tokens" (t "commits take 5" == ["commits", "take", "5"])
  check ref "quoted string one token"
    (t "where author \"alice bob\"" == ["where", "author", "\"alice bob\""])
  check ref "terminal token" (t "commits /show" == ["commits", "/show"])
  check ref "regex literal keeps slashes" (t "grep /fix.*bug/" == ["grep", "/fix.*bug/"])
  check ref "regex vs terminal predicate"
    (isTerminalToken "/show" && not (isTerminalToken "/fix/"))
  check ref "slash scan stops at quote"
    (t "/branch-off \"feature/x\"" == ["/branch-off", "\"feature/x\""])
  check ref "sort negation" (t "sort -date" == ["sort", "-date"])
  check ref "digit-starting sha prefix one token" (t "where sha 062062e9af" == ["where", "sha", "062062e9af"])
  check ref "digit-starting date one token" (t "where date == 2026-05-25" == ["where", "date", "==", "2026-05-25"])
  check ref "bare morphism star" (t "via parent*" == ["via", "parent*"])
  check ref "bare morphism brackets" (t "via tree.entries[Blob]" == ["via", "tree.entries[Blob]"])
  check ref "bare morphism dagger" (t "via parent\x2020" == ["via", "parent\x2020"])
  check ref "dotted legacy path" (t "via .parent.tree" == ["via", ".parent.tree"])
  check ref "two-char operators" (t "where parents-count >= 2" == ["where", "parents-count", ">=", "2"])
  check ref "comma splits" (t "pick sha, author" == ["pick", "sha", ",", "author"])
  check ref "range one token" (t "commits in main..HEAD" == ["commits", "in", "main..HEAD"])
  check ref "unterminated quote survives" (t "where author \"ali" == ["where", "author", "\"ali"])
  check ref "unquote" (unquote "\"abc\"" == "abc" && unquote "abc" == "abc")
  check ref "unregex" (unregex "/a.b/" == "a.b")

-- Parser (positive) ----------------------------------------------------------

parserTests :: Counter -> IO ()
parserTests ref = do
  let p = parsed
  check ref "source commits"
    (pipeSource (p "commits") == SCommits Nothing)
  check ref "source range"
    (pipeSource (p "commits in main..HEAD /count") == SCommits (Just "main..HEAD"))
  check ref "range stops at step keyword"
    (p "commits in main..HEAD take 2"
       == Pipeline (SCommits (Just "main..HEAD")) [StTake 2] Nothing)
  check ref "source HEAD is ref" (pipeSource (p "HEAD") == SRef "HEAD")
  check ref "source named ref" (pipeSource (p "mybranch first") == SRef "mybranch")
  check ref "sources" (map (pipeSource . p) ["branches", "tags", "refs", "worktrees", "blobs"]
                        == [SBranches, STags, SRefs, SWorktrees, SBlobs])
  check ref "via parent star + implicit contains"
    (p "HEAD via parent* where message \"fix\""
       == Pipeline (SRef "HEAD") [StVia MParentStar, StWhere [Cond "message" OpContains (VStr "fix")]] Nothing)
  check ref "implicit contains bare word"
    (pipeSteps (p "commits where author alice")
       == [StWhere [Cond "author" OpContains (VStr "alice")]])
  check ref "numeric comparison"
    (pipeSteps (p "commits where parents-count > 1")
       == [StWhere [Cond "parents-count" OpGt (VNum 1)]])
  check ref "bare flag"
    (pipeSteps (p "worktrees where detached")
       == [StWhere [Cond "detached" OpIs (VBool True)]])
  check ref "bare flag before step keyword"
    (pipeSteps (p "worktrees where modified take 5")
       == [StWhere [Cond "modified" OpIs (VBool True)], StTake 5])
  check ref "multi condition with comma"
    (pipeSteps (p "commits where author alice, message fix")
       == [StWhere [Cond "author" OpContains (VStr "alice"), Cond "message" OpContains (VStr "fix")]])
  check ref "regex operator"
    (pipeSteps (p "commits where message regex /fix(es)?/")
       == [StWhere [Cond "message" OpRegex (VStr "fix(es)?")]])
  check ref "morphism composition"
    (pipeSteps (p "HEAD via parent.tree") == [StVia MParent, StVia MTree])
  check ref "morphism indexed + entries filter"
    (pipeSteps (p "HEAD via parent[0].tree.entries[Blob]")
       == [StVia (MParentIdx 0), StVia (MTreeEntries (Just FBlob))])
  check ref "dotted legacy spelling"
    (pipeSteps (p "HEAD via .parent") == [StVia MParent])
  check ref "diff with explicit ref"
    (pipeSteps (p "commits via diff main") == [StVia (MDiff (Just "main"))])
  check ref "diff ref not consumed from step keyword"
    (pipeSteps (p "commits via diff take 2") == [StVia (MDiff Nothing), StTake 2])
  check ref "diff ref not consumed from terminal"
    (p "commits via diff /count"
       == Pipeline (SCommits Nothing) [StVia (MDiff Nothing)] (Just TCount))
  check ref "grep regex literal"
    (pipeSteps (p "HEAD grep /foo.*bar/") == [StGrep "foo.*bar" True])
  check ref "pickaxe with regex keyword"
    (pipeSteps (p "commits pickaxe \"needle\" regex") == [StPickaxe "needle" True])
  check ref "pick fields"
    (pipeSteps (p "commits pick sha, author") == [StPick ["sha", "author"]])
  check ref "pick comma-less"
    (pipeSteps (p "commits pick sha author") == [StPick ["sha", "author"]])
  check ref "sort negation"
    (pipeSteps (p "commits sort -date") == [StSort "date" True])
  check ref "path both field and step"
    (pipeSteps (p "blobs path \"*.el\" where path contains-nothing" )
       == [StPath "*.el", StWhere [Cond "path" OpContains (VStr "contains-nothing")]])
  -- terminals
  check ref "terminal branch-off name+worktree"
    (pipeTerminal (p "commits first /branch-off \"feat\" worktree \"/tmp/wt\"")
       == Just (TBranchOff (Just "feat") (Just "/tmp/wt")))
  check ref "terminal amend no-edit"
    (pipeTerminal (p "HEAD /amend no-edit") == Just (TAmend True Nothing))
  check ref "terminal amend msg"
    (pipeTerminal (p "HEAD /amend \"new msg\"") == Just (TAmend False (Just "new msg")))
  check ref "terminal squash msg"
    (pipeTerminal (p "commits take 3 /squash \"combined\"") == Just (TSquash (Just "combined")))
  check ref "terminal commit msg"
    (pipeTerminal (p "commits /commit \"wip\"") == Just (TCommit (Just "wip")))
  check ref "terminal mark label"
    (pipeTerminal (p "HEAD /mark important") == Just (TMark (Just "important")))
  check ref "terminal delete aliases remove"
    (pipeTerminal (p "HEAD /delete") == Just TRemove)
  forM_ terminalNames $ \tn ->
    pok ref ("commits /" ++ tn)

-- Fail-loud error catalogue ---------------------------------------------------

failLoudTests :: Counter -> IO ()
failLoudTests ref = do
  perr ref "" "empty pipeline"
  perr ref "commits frobnicate" "expected step keyword or /terminal, got 'frobnicate'"
  perr ref "commits where name == x" "field 'name' not valid here after 'where'"
  perr ref "commits sort name" "field 'name' not valid here after 'sort'"
  perr ref "commits where date > \"2020\"" "operator '>' does not apply to 'date'"
  perr ref "commits where parents-count == two" "'two' is not a number"
  perr ref "commits where author" "tests a flag, but 'author' is a string field"
  perr ref "commits where message take take 5" "step keyword 'take' must be quoted"
  perr ref "commits where message == take 5" "step keyword 'take' must be quoted"
  perr ref "commits where message ==" "operator '==' requires a value"
  perr ref "commits where date within" "operator 'within' requires a value"
  perr ref "commits where date frob \"x\"" "unknown where operator 'frob'"
  perr ref "commits take 5x" "'take' requires a number, got '5x'"
  perr ref "commits take" "'take' requires a number, got end of input"
  perr ref "commits skip x" "'skip' requires a number"
  perr ref "commits pick" "'pick' requires at least one field"
  perr ref "commits pick take 5" "'pick' requires at least one field"
  perr ref "branches via tree" "'via tree' needs a 'tree' field"
  perr ref "commits via diff.hunks grep x" "'grep' needs a 'sha' field"
  perr ref "commits via diff.lines pickaxe x" "'pickaxe' needs a 'sha' field"
  perr ref "branches via parent" "'via parent' needs a 'parents-count' field"
  perr ref "commits via frobnicate" "unknown morphism"
  perr ref "commits /frobnicate" "unknown terminal operation 'frobnicate'"
  perr ref "commits /show extra" "unexpected token 'extra' after 'show'"
  perr ref "commits /count 5" "unexpected token '5' after 'count'"
  perr ref "commits where author alice, take" "expected a field name after ','"
  perr ref "commits pick sha where author alice" "field 'author' not valid here after 'where'"
  perr ref "via parent" "expected step keyword"  -- 'via' parses as a ref source

-- P1–P7 grammar disambiguation properties -------------------------------------

propertyTests :: Counter -> IO ()
propertyTests ref = do
  -- P1: terminals start with / and have no closing /
  check ref "P1 terminal predicate all"
    (all (isTerminalToken . ('/' :)) terminalNames)
  check ref "P1 regex not terminal" (not (isTerminalToken "/fix/"))
  -- P2: /regex/ literals survive tokenization
  check ref "P2 regex preserved" (tokenize "where message regex /a|b/" !! 3 == "/a|b/")
  -- P3: step keywords always start a new stage
  pok ref "commits where message \"take\" take 5"
  perr ref "commits where message take" "must be quoted"
  -- P4: former terminal identifiers are plain values now
  pok ref "commits where message commit"
  pok ref "commits where message show /count"
  -- P5: bare flags end cleanly at every boundary kind
  pok ref "worktrees where detached /show"
  pok ref "worktrees where detached, modified"
  pok ref "worktrees where modified first"
  -- P6: range consumption
  check ref "P6 range then terminal"
    (parsePipeline "commits in v1..v2 /count"
       == Right (Pipeline (SCommits (Just "v1..v2")) [] (Just TCount)))
  -- P7: via diff ref consumption rules (also covered in parserTests)
  pok ref "commits via diff main path \"*.hs\""

-- Registry coherence -----------------------------------------------------------

registryTests :: Counter -> IO ()
registryTests ref = do
  -- every completable morphism parses and is registered
  forM_ completeMorphisms $ \m ->
    check ref ("morphism candidate parses: " ++ m)
      (case parseMorphismPath m of
         Right (_ : _) -> True
         _             -> False)
  -- every completable morphism type-checks from a source that has its domain
  forM_ completeMorphisms $ \m ->
    check ref ("morphism candidate has domain: " ++ m)
      (case parseMorphismPath m of
         Right (h : _) -> morphismRequires h `elem` fieldNames
         _             -> False)
  -- every completable terminal parses
  forM_ completeTerminals $ \t ->
    pok ref ("commits " ++ t)
  -- every field has a description and a scalar type consistent with docs
  forM_ fieldNames $ \f -> do
    check ref ("field described: " ++ f)
      (describeToken f /= Nothing || f == "tree" || f == "path")
  -- operator completion list == signature table
  check ref "operator lists agree" (completeWhereOperators == operatorNames)
  forM_ operatorNames $ \op ->
    check ref ("operator has signature: " ++ op) (operatorSignature op /= Nothing)
  -- source keyword field-sets are non-empty
  forM_ completeSourceKeywords $ \s ->
    check ref ("source has fields: " ++ s) (not (null (inferFields [s])))

-- Field-set inference -----------------------------------------------------------

inferTests :: Counter -> IO ()
inferTests ref = do
  check ref "infer commits" (inferFields ["commits"] == commitFields)
  check ref "infer branches" (inferFields ["branches"] == refFields)
  check ref "infer via diff" (inferFields ["commits", "via", "diff"] == diffFields)
  check ref "infer via diff.lines" (inferFields ["commits", "via", "diff.lines"] == diffLineFields)
  check ref "infer grep" (inferFields ["commits", "grep", "x"] == lineFields)
  check ref "infer pick narrows" (inferFields ["commits", "pick", "sha"] == ["sha"])
  check ref "infer stops mid-stage" (inferFields ["commits", "via"] == commitFields)
  check ref "infer tree object" (inferFields ["commits", "via", "tree"] == treeObjectFields)

-- Rendering ----------------------------------------------------------------------

renderTests :: Counter -> IO ()
renderTests ref = do
  let f = Frame "commit" ["p1", "p2"]
            (M.fromList [("sha", VStr "abc"), ("message", VStr "say \"hi\"")])
      sexp = renderFrameSexp f
  check ref "sexp has type" ("(:type commit" `isPrefixOf` sexp)
  check ref "sexp escapes quotes" ("\\\"hi\\\"" `isInfixOf` sexp)
  check ref "sexp parents" (":parents (\"p1\" \"p2\")" `isInfixOf` sexp)

-- Integration against a real scratch repo -----------------------------------------

git :: [String] -> IO String
git args = readCreateProcess (proc "git" args) ""

gitIn :: FilePath -> [String] -> IO String
gitIn dir args = readCreateProcess ((proc "git" args) { cwd = Just dir }) ""

integrationTests :: Counter -> IO ()
integrationTests ref = do
  tmp <- getTemporaryDirectory
  let dir = tmp </> "gitq-test-scratch"
  exists <- doesDirectoryExist dir
  when exists (removeDirectoryRecursive dir)
  createDirectoryIfMissing True dir
  _ <- gitIn dir ["init", "-q", "-b", "main"]
  _ <- gitIn dir ["config", "user.name", "alice"]
  _ <- gitIn dir ["config", "user.email", "alice@example.com"]
  _ <- gitIn dir ["config", "commit.gpgsign", "false"]
  -- commit 1 (root, by alice): a.txt with a needle line
  writeFile (dir </> "a.txt") "hello\nneedle-alpha\n"
  _ <- gitIn dir ["add", "a.txt"]
  _ <- gitIn dir ["commit", "-q", "-m", "initial commit", "--date", "2024-01-01T10:00:00+0000"]
  _ <- gitIn dir ["tag", "v1"]
  -- commit 2 (by bob): b.txt
  writeFile (dir </> "b.txt") "world\n"
  _ <- gitIn dir ["add", "b.txt"]
  _ <- gitIn dir ["-c", "user.name=bob", "-c", "user.email=bob@example.com",
                  "commit", "-q", "-m", "add b", "--date", "2024-02-01T10:00:00+0000"]
  -- commit 3 (by alice): fix a.txt
  writeFile (dir </> "a.txt") "hello\nneedle-alpha\nneedle-beta\n"
  _ <- gitIn dir ["add", "a.txt"]
  _ <- gitIn dir ["commit", "-q", "-m", "fix a needle-beta", "--date", "2024-03-01T10:00:00+0000"]

  old <- getCurrentDirectory
  setCurrentDirectory dir
  let run q = execPipeline (parsed q)
      frames q = fst <$> run q
      shasOf fs = [s | fr <- fs, Just s <- [frameCommitSha fr]]
      fieldStrs k fs = [s | fr <- fs, Just (VStr s) <- [frameField fr k]]

  headSha <- head . lines <$> git ["rev-parse", "HEAD"]
  rootSha <- head . lines <$> git ["rev-list", "--max-parents=0", "HEAD"]

  fs1 <- frames "commits"
  check ref "commits count 3" (length fs1 == 3)
  check ref "commits newest first" (take 1 (shasOf fs1) == [headSha])

  fs2 <- frames "commits where author alice"
  check ref "where author alice = 2" (length fs2 == 2)

  fs3 <- frames "commits where author == alice"
  check ref "where author == alice exact = 2" (length fs3 == 2)

  fs4 <- frames "HEAD via parent"
  check ref "HEAD parent is commit 2" (fieldStrs "message" fs4 == ["add b"])

  fs5 <- frames "HEAD via parent*"
  check ref "parent* inclusive = 3" (length fs5 == 3)

  fs6 <- frames "HEAD via parent+"
  check ref "parent+ exclusive = 2" (length fs6 == 2)

  fs7 <- frames ("commits where sha " ++ take 8 rootSha)
  check ref "sha prefix contains-match" (shasOf fs7 == [rootSha])

  fs8 <- frames "commits where message fix first"
  check ref "message fix first" (fieldStrs "message" fs8 == ["fix a needle-beta"])

  fs9 <- frames "commits sort date first"
  check ref "sort date ascending puts root first" (shasOf fs9 == [rootSha])

  fs10 <- frames "commits where parents-count == 0"
  check ref "root has zero parents" (shasOf fs10 == [rootSha])

  fs11 <- frames "branches"
  check ref "branches has main" ("main" `elem` fieldStrs "name" fs11)

  fs12 <- frames "tags"
  check ref "tags has v1" (fieldStrs "name" fs12 == ["v1"])

  fs13 <- frames "blobs"
  check ref "blobs lists both files" (sort (fieldStrs "path" fs13) == ["a.txt", "b.txt"])

  fs14 <- frames "blobs path \"*.txt\""
  check ref "path glob keeps txt" (length fs14 == 2)

  fs15 <- frames "commits via diff"
  check ref "via diff includes root files" ("a.txt" `elem` fieldStrs "path" fs15)

  fs16 <- frames "commits via diff.lines where content needle-beta"
  check ref "diff.lines finds added content"
    (fieldStrs "sign" fs16 == ["+"] && fieldStrs "content" fs16 == ["needle-beta"])

  fs17 <- frames "commits pickaxe \"needle-beta\""
  check ref "pickaxe narrows to the adding commit" (shasOf fs17 == [headSha])

  fs18 <- frames "HEAD grep needle"
  check ref "grep finds both needle lines" (length fs18 == 2)

  fs19 <- frames "HEAD via tree.entries[Blob]"
  check ref "tree entries blobs" (sort (fieldStrs "path" fs19) == ["a.txt", "b.txt"])

  fs20 <- frames "blobs path \"a.txt\" via history"
  check ref "history of a.txt = 2 commits" (length fs20 == 2)

  fs21 <- frames ("commits where sha " ++ take 8 rootSha ++ " via parent\x2020")
  check ref "parent adjoint finds child" (fieldStrs "message" fs21 == ["add b"])

  fs22 <- frames "commits pick sha"
  check ref "pick projects to sha only"
    (all (\fr -> M.keys (frameAttrs fr) == ["sha"]) fs22)

  fs23 <- frames "worktrees"
  check ref "one worktree on main" (fieldStrs "branch" fs23 == ["main"])

  fs24 <- frames "commits in v1..HEAD"
  check ref "range v1..HEAD = 2" (length fs24 == 2)

  fs25 <- frames "HEAD via tree"
  check ref "via tree yields tree object" (map frameType fs25 == ["tree"])

  fs26 <- frames "commits where date after \"2024-01-15\""
  check ref "date after filters" (length fs26 == 2)

  fs27 <- frames "commits where date before \"2024-01-15\""
  check ref "date before filters" (shasOf fs27 == [rootSha])

  fs28 <- frames "commits via diff.hunks"
  check ref "diff.hunks non-root commits have hunks" (not (null fs28))

  fs29 <- frames "commits where message regex /needle-(beta|gamma)/"
  check ref "regex operator matches" (length fs29 == 1)

  (_, term30) <- run "commits take 2 /count"
  check ref "terminal identified not applied" (term30 == Just TCount)

  -- completion inside the scratch repo
  c1 <- completeCandidates ""
  check ref "complete start = sources" (c1 == completeSourceKeywords)
  c2 <- completeCandidates "commits "
  check ref "complete after commits has in" ("in" `elem` c2)
  c3 <- completeCandidates "commits via "
  check ref "complete via filters by domain"
    ("parent" `elem` c3 && "history" `notElem` c3)
  c4 <- completeCandidates "branches via "
  check ref "complete via on branches excludes parent, keeps tree.entries"
    ("parent" `notElem` c4 && "tree.entries" `elem` c4)
  c5 <- completeCandidates "commits where "
  check ref "complete where = commit fields" (c5 == commitFields)
  c6 <- completeCandidates "commits where author "
  check ref "complete after author has operators and authors"
    (all (`elem` c6) operatorNames && "alice" `elem` c6 && "bob" `elem` c6)
  c7 <- completeCandidates "commits /amend "
  check ref "complete after /amend = no-edit" (c7 == ["no-edit"])
  c8 <- completeCandidates "commits in "
  check ref "complete refs after in" ("main" `elem` c8 && "v1" `elem` c8)
  c9 <- completeCandidates "commits where date within "
  check ref "complete within examples" (c9 == completeDateWithinExamples)
  c10 <- completeCandidates "worktrees where "
  check ref "complete worktree fields" (c10 == worktreeFields)

  setCurrentDirectory old
