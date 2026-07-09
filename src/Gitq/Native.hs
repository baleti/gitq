{-# LANGUAGE CPP #-}
-- | The optional in-process git backend: a Rust static library (native/,
-- libgit2 via the git2 crate) linked into this executable when the @native@
-- cabal flag is set.  Built without the flag, every function here is a
-- no-op returning Nothing and callers use the subprocess git path.
--
-- The boundary contract is deliberately tiny: one C function,
-- @gitq_native_commits@, taking whitespace-separated full SHAs and
-- returning commit records in the /exact/ byte format of gitq's
-- @git log@ format string — so 'parseCommitLine' parses both backends and
-- the two can never drift.  Any native-side failure returns NULL and the
-- caller falls back to subprocess git; set @GITQ_NO_NATIVE=1@ to force the
-- fallback at runtime (useful for A/B benchmarks with a single binary).
module Gitq.Native
  ( nativeEnabled
  , nativeCommits
  ) where

import Gitq.Frame (Frame)

#ifdef NATIVE

import qualified Data.ByteString as BS
import qualified Data.Text as T
import qualified Data.Text.Encoding as TE
import qualified Data.Text.Encoding.Error as TEE
import Data.Maybe (isJust)
import Data.Word (Word8)
import Foreign.C.String (CString, withCString)
import Foreign.C.Types (CInt (..), CSize (..))
import Foreign.Marshal.Alloc (alloca)
import Foreign.Ptr (Ptr, castPtr, nullPtr)
import Foreign.Storable (peek)
import System.Environment (lookupEnv)
import Gitq.Git (parseCommitLine)

foreign import ccall safe "gitq_native_commits"
  c_gitqNativeCommits :: CString -> CString -> CInt -> Ptr CSize -> IO (Ptr Word8)

foreign import ccall unsafe "gitq_native_free"
  c_gitqNativeFree :: Ptr Word8 -> CSize -> IO ()

nativeEnabled :: Bool
nativeEnabled = True

-- | Resolve full SHAs in-process: their ancestor closure when @walk@, just
-- the SHAs themselves otherwise.  Nothing means "use the subprocess path"
-- (native failure, no SHAs, or GITQ_NO_NATIVE set) — never an error.
nativeCommits :: Bool -> [String] -> IO (Maybe [Frame])
nativeCommits _ [] = pure Nothing
nativeCommits walk shas = do
  disabled <- lookupEnv "GITQ_NO_NATIVE"
  if isJust disabled
    then pure Nothing
    else
      withCString "." $ \repoC ->
      withCString (unlines shas) $ \startsC ->
      alloca $ \lenP -> do
        ptr <- c_gitqNativeCommits repoC startsC (if walk then 1 else 0) lenP
        if ptr == nullPtr
          then pure Nothing
          else do
            len <- peek lenP
            bs <- BS.packCStringLen (castPtr ptr, fromIntegral len)
            c_gitqNativeFree ptr len
            let text = TE.decodeUtf8With TEE.lenientDecode bs
            pure (Just [f | Just f <- map parseCommitLine (lines (T.unpack text))])

#else

nativeEnabled :: Bool
nativeEnabled = False

-- | Without the @native@ flag this backend does not exist; callers always
-- take the subprocess path.
nativeCommits :: Bool -> [String] -> IO (Maybe [Frame])
nativeCommits _ _ = pure Nothing

#endif
