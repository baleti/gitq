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

# Per-user zsh completion: copy _gitq into a fixed XDG-style dir.
# Nothing system-wide, and no fpath auto-detection — always the same
# directory, so it's predictable across machines. We only print the
# ~/.zshrc line that's needed; we never edit the file ourselves.
XDG_DATA_HOME ?= $(HOME)/.local/share
ZSH_COMP_DIR ?= $(XDG_DATA_HOME)/zsh/completions

install-zsh:
	@mkdir -p $(ZSH_COMP_DIR)
	@cp $(CURDIR)/integrations/zsh/_gitq $(ZSH_COMP_DIR)/_gitq
	@echo "Copied _gitq into $(ZSH_COMP_DIR)"
	@echo "Add this to ~/.zshrc, BEFORE 'autoload -Uz compinit' / 'compinit':"
	@echo "  fpath=($(ZSH_COMP_DIR) \$$fpath)"
	@echo "If compinit already cached completions once, refresh it after adding the line:"
	@echo "  rm -f ~/.zcompdump && exec zsh"

# Per-user bash completion: symlink gitq.bash into the bash-completion
# completions dir.  Far simpler than zsh — bash has no fpath-style
# autodiscovery, so if the bash-completion package doesn't auto-source
# that directory we print the one `source` line to add to ~/.bashrc.
# Override with BASH_COMP_DIR=... .
BASH_COMP_DIR ?= $(XDG_DATA_HOME)/bash-completion/completions

install-bash:
	@mkdir -p $(BASH_COMP_DIR)
	@ln -sf $(CURDIR)/integrations/bash/gitq.bash $(BASH_COMP_DIR)/gitq
	@echo "Linked gitq bash completion into $(BASH_COMP_DIR)."
	@echo "If your bash-completion package doesn't auto-source that dir, add"
	@echo "to ~/.bashrc:  source $(BASH_COMP_DIR)/gitq"

# Per-user zsh scrollback widgets.  Unlike _gitq these are *sourced*, not
# autoloaded, so there's no fpath to discover — we just symlink the file
# next to _gitq (or wherever ZSH_COMP_DIR points) and print the one source
# line to add to ~/.zshrc.
install-zsh-scrollback:
	@mkdir -p $(ZSH_COMP_DIR)
	@ln -sf $(CURDIR)/integrations/zsh/gitq-scrollback.zsh $(ZSH_COMP_DIR)/gitq-scrollback.zsh
	@echo "Linked gitq-scrollback.zsh into $(ZSH_COMP_DIR)."
	@echo "Add to ~/.zshrc (widgets are sourced, not autoloaded):"
	@echo "  source $(ZSH_COMP_DIR)/gitq-scrollback.zsh"
	@echo "Then Meta-b browses scrollback, Meta-e sends it to Emacs (both need tmux)."

.PHONY: build native test test-native install install-native install-zsh \
        install-bash install-zsh-scrollback
