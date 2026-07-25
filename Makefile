# gitq — Rust build.
#
# One toolchain, one command.  There is no separate "native" build any more:
# the gix in-process backend is a module, not a linked staticlib behind a
# cabal flag, so `cargo build` is the whole story.  GITQ_NO_NATIVE=1 at
# runtime still forces the subprocess-git path, which is how the two are
# A/B'd from a single binary.

CARGO ?= cargo

build:
	$(CARGO) build --release

test:
	$(CARGO) test

lint:
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) fmt --check

# Golden corpus: run the corpus through a binary and diff two runs.  Used
# during the Haskell->Rust port and kept because it is the only check that
# covers the CLI surface end to end (plain output, --sexp, the error
# catalogue, and completion) against a real repository.
#
#   make corpus                      A/B the two backends against each other
#   make corpus REF=/path/to/other   diff this build against another binary
BIN := target/release/gitq
CORPUS_DIR ?= $(CURDIR)/target/corpus

corpus: build
	@bash tools/fixture.sh $(CORPUS_DIR)/fixture
	@GITQ_NO_NATIVE=1 bash tools/golden.sh $(BIN) $(CORPUS_DIR)/subprocess $(CORPUS_DIR)/fixture
	@bash tools/golden.sh $(if $(REF),$(REF),$(BIN)) $(CORPUS_DIR)/candidate $(CORPUS_DIR)/fixture
	@bash tools/compare.sh $(CORPUS_DIR)/subprocess $(CORPUS_DIR)/candidate

# Install by copying the release build.  Override BINDIR for a different
# destination.
BINDIR ?= $(HOME)/.local/bin

install: build
	install -d $(BINDIR)
	install -m 755 $(BIN) $(BINDIR)/gitq
	@echo "Installed $(BINDIR)/gitq"
	@echo "The Emacs and zsh integrations resolve gitq on \$$PATH, so they"
	@echo "pick this up with no further steps."

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

clean:
	$(CARGO) clean

.PHONY: build test lint corpus install install-zsh install-bash \
        install-zsh-scrollback clean
