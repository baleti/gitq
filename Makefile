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

# Install by copying the in-tree build (avoids cabal install's
# build-from-sdist detour, which would rebuild the Rust crate in a temp
# dir).  Override BINDIR for a different destination.
BINDIR ?= $(HOME)/.local/bin

install:
	cabal build exe:gitq
	install -m 755 "$$(cabal list-bin exe:gitq)" $(BINDIR)/gitq

install-native:
	cd native && cargo build --release
	cabal build -fnative exe:gitq
	install -m 755 "$$(cabal list-bin -fnative exe:gitq)" $(BINDIR)/gitq

.PHONY: build native test test-native install install-native
