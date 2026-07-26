# gitq — Rust build.
#
# One toolchain, one command.  There is no separate "native" build any more:
# the gix in-process backend is a module, not a linked staticlib behind a
# cabal flag, so `cargo build` is the whole story.  GITQ_NO_NATIVE=1 at
# runtime still forces the subprocess-git path, which is how the two are
# A/B'd from a single binary.

# Find cargo even when the shell hasn't been told about it.  rustup appends
# its PATH line to ~/.profile, which zsh does not read, so a rustup install
# is routinely invisible to a zsh user's `make` — the failure looked like a
# missing toolchain when the toolchain was there all along.
CARGO ?= $(shell command -v cargo 2>/dev/null || \
                 (test -x "$(HOME)/.cargo/bin/cargo" && echo "$(HOME)/.cargo/bin/cargo"))

ifeq ($(strip $(CARGO)),)
$(error cargo not found. Install Rust from https://rustup.rs, then either \
open a new shell or add `. "$$HOME/.cargo/env"` to your shell rc — rustup \
only writes that line to ~/.profile, which zsh does not read. \
Override with `make CARGO=/path/to/cargo`)
endif

build:
	$(CARGO) build --release

test:
	$(CARGO) test
	@# The zsh completer reconstructs the pipeline from the command line;
	@# that logic has no Rust to test it, so it gets its own check.
	@command -v zsh >/dev/null 2>&1 \
		&& zsh tools/test-zsh-completion.zsh \
		|| echo "(zsh not installed - skipping completion prefix tests)"
	@# The Emacs client re-implements two specs the binary also owns
	@# (`gitq--stats-line` / preview_stats, `gitq--selection-step` /
	@# selection_step).  Two implementations of one spec drift silently, so
	@# they get a suite that asserts the same answers.
	@command -v emacs >/dev/null 2>&1 \
		&& emacs --batch -l integrations/emacs/gitq.el \
			-l integrations/emacs/gitq-tests.el \
			-f ert-run-tests-batch-and-exit \
		|| echo "(emacs not installed - skipping elisp tests)"

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

# Per-user zsh integration: one file, sourced from ~/.zshrc.
#
# There is deliberately only one target and one file.  gitq.zsh carries the
# completion function, the TAB TUI widget and the scrollback widgets, and
# defers everything order-sensitive to the first prompt — so it needs no
# fpath entry, no compinit ordering, and no particular position relative to
# other plugins.
#
# It goes in gitq's own data dir, NOT an fpath directory, and that is a
# deliberate correction.  An fpath directory means *autoload*: compinit
# scans it for `#compdef` files and will never `source` anything.  The old
# layout put sourced widget files in ~/.local/share/zsh/completions, where
# they sat inert — installed, on fpath, and never loaded, which is exactly
# the bug this layout removes.  This file is neither a completion function
# nor an autoloadable function; it is configuration to be sourced.
#
# `install`, not `cp`: it unlinks the destination first.  `cp` follows an
# existing destination symlink and writes through it — and when that symlink
# points back at this repo (as it does for anyone who installed while these
# targets still symlinked) `cp` refuses outright with "are the same file",
# so the upgrade path was broken.
# We only print the ~/.zshrc line that's needed; we never edit the file.
XDG_DATA_HOME ?= $(HOME)/.local/share
ZSH_DIR ?= $(XDG_DATA_HOME)/gitq

install-zsh:
	@mkdir -p $(ZSH_DIR)
	@install -m 644 $(CURDIR)/integrations/zsh/gitq.zsh $(ZSH_DIR)/gitq.zsh
	@echo "Copied gitq.zsh into $(ZSH_DIR)."
	@echo "Add this one line to ~/.zshrc — anywhere, order does not matter:"
	@echo "  source $(ZSH_DIR)/gitq.zsh"
	@echo "Then: exec zsh"
	@echo
	@echo "It sets up TAB (gitq's completer TUI), M-b/M-e (scrollback), and"
	@echo "menu completion."
	@echo
	@echo "Upgrading from the old three-file layout? Those files lived in an"
	@echo "fpath dir, which only ever autoloaded _gitq and silently ignored"
	@echo "the rest. Remove the fpath line and the old files:"
	@echo "  rm -f $(XDG_DATA_HOME)/zsh/completions/{_gitq,gitq-complete.zsh,gitq-scrollback.zsh}"

# Per-user bash integration.  Installed next to the zsh one, in gitq's own
# data dir rather than the bash-completion directory: it is sourced, and it
# does more than complete (TAB opens the TUI).
install-bash:
	@mkdir -p $(ZSH_DIR)
	@install -m 644 $(CURDIR)/integrations/bash/gitq.bash $(ZSH_DIR)/gitq.bash
	@echo "Copied gitq.bash into $(ZSH_DIR)."
	@echo "Add this one line to ~/.bashrc:"
	@echo "  source $(ZSH_DIR)/gitq.bash"
	@echo "TAB on a gitq command line then opens the completer TUI; every"
	@echo "other command's TAB is untouched."

clean:
	$(CARGO) clean

.PHONY: build test lint corpus install install-zsh install-bash clean
