-- | Custom Setup: when the @native@ flag is set, resolve the Rust static
-- library's directory to an absolute path (ghc-pkg rejects relative
-- extra-lib-dirs) and build the crate first if its artifact is missing.
import Control.Monad (unless)
import Distribution.PackageDescription
import Distribution.Simple
import Distribution.Simple.LocalBuildInfo (localPkgDescr)
import Distribution.Simple.Setup (configConfigurationsFlags)
import Distribution.Types.Flag (lookupFlagAssignment, mkFlagName)
import System.Directory (doesFileExist, getCurrentDirectory, withCurrentDirectory)
import System.Exit (ExitCode (..))
import System.FilePath ((</>))
import System.Process (rawSystem)

main :: IO ()
main = defaultMainWithHooks simpleUserHooks { confHook = nativeConfHook }
 where
  nativeConfHook info flags = do
    lbi <- confHook simpleUserHooks info flags
    let nativeOn =
          lookupFlagAssignment (mkFlagName "native") (configConfigurationsFlags flags)
            == Just True
    if not nativeOn
      then pure lbi
      else do
        cwd <- getCurrentDirectory
        let libDir = cwd </> "native" </> "target" </> "release"
            libFile = libDir </> "libgitq_native.a"
        built <- doesFileExist libFile
        unless built $ do
          putStrLn "gitq: building native/ (cargo build --release) ..."
          code <- withCurrentDirectory (cwd </> "native") $
                    rawSystem "cargo" ["build", "--release"]
          case code of
            ExitSuccess -> pure ()
            ExitFailure _ ->
              fail ("gitq: could not build the native backend; install a Rust \
                    \toolchain or run `cd native && cargo build --release`, \
                    \or build without -fnative")
        let pd = localPkgDescr lbi
            addDir bi = bi { extraLibDirs = libDir : extraLibDirs bi }
            pd' = pd { library =
                         fmap (\l -> l { libBuildInfo = addDir (libBuildInfo l) })
                              (library pd) }
        pure lbi { localPkgDescr = pd' }
