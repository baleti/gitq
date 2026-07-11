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

# Per-user zsh completion: symlink _gitq into a directory on your fpath.
# Nothing system-wide. If the live shell already has a writable $HOME
# directory on fpath (frameworks and plugin managers add one), the link
# goes there and no zshrc changes are needed at all; otherwise the XDG
# data dir is used (completion functions are data, mirroring the system
# site-functions dir) and the two required zshrc lines are printed.
# Override with ZSH_COMP_DIR=... (e.g. $$ZDOTDIR/completions).
XDG_DATA_HOME ?= $(HOME)/.local/share
ZSH_COMP_DIR ?= $(XDG_DATA_HOME)/zsh/completions

install-zsh:
	@set -e; DIR='$(ZSH_COMP_DIR)'; ONFPATH=''; \
	if [ '$(origin ZSH_COMP_DIR)' != 'command line' ] && command -v zsh >/dev/null 2>&1; then \
	  CAND=$$(zsh -ic 'for d in $$fpath; do if [[ $$d = $$HOME/* && -d $$d && -w $$d ]]; then print -r -- $$d; break; fi; done' 2>/dev/null | head -n1); \
	  if [ -n "$$CAND" ]; then DIR="$$CAND"; ONFPATH=1; \
	  elif zsh -ic 'for d in $$fpath; do [[ $$d = '"'"'$(ZSH_COMP_DIR)'"'"' ]] && exit 0; done; exit 1' >/dev/null 2>&1; then ONFPATH=1; fi; \
	fi; \
	mkdir -p "$$DIR"; \
	ln -sf $(CURDIR)/integrations/zsh/_gitq "$$DIR/_gitq"; \
	echo "Linked _gitq into $$DIR."; \
	if [ -n "$$ONFPATH" ]; then \
	  echo "That directory is already on your fpath — no zshrc changes needed."; \
	  echo "New completions need a cache refresh: rm -f ~/.zcompdump && exec zsh"; \
	else \
	  echo "Ensure ~/.zshrc has, BEFORE compinit:"; \
	  echo "  fpath=($$DIR \$$fpath)"; \
	  echo "  autoload -Uz compinit && compinit"; \
	  echo "If completion doesn't appear, refresh the cache: rm -f ~/.zcompdump && compinit"; \
	fi

.PHONY: build native test test-native install install-native install-zsh
