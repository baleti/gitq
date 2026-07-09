# Plain Haskell build (no Rust toolchain needed; subprocess git everywhere)
build:
	cabal build exe:gitq

# Build with the Rust in-process git backend linked in (native/, libgit2).
# One binary; at runtime it falls back to subprocess git if the native
# backend fails, and GITQ_NO_NATIVE=1 forces the fallback for A/B testing.
native:
	cd native && cargo build --release
	cabal build -fnative exe:gitq

test:
	cabal test

test-native:
	cd native && cargo build --release
	cabal test -fnative

install:
	cabal install exe:gitq --overwrite-policy=always

install-native:
	cd native && cargo build --release
	cabal install -fnative exe:gitq --overwrite-policy=always

.PHONY: build native test test-native install install-native
