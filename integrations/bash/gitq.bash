# gitq — bash integration.
#
# Install: `make install-bash` in the repo root, or source it from ~/.bashrc:
#   source ~/.local/share/gitq/gitq.bash
#
# TAB on a `gitq …` command line opens gitq's columnar completer TUI, exactly
# as it does under zsh.  Every other command's TAB is untouched.
#
# That last point is why this is a *completion function* and not a key
# binding.  bash has no equivalent of zsh's `zle ${GITQ_FALLBACK_TAB}` —
# nothing that means "otherwise do whatever TAB normally does" — so binding
# TAB with `bind -x` would replace completion globally and break every other
# command.  `complete -F` is per-command, so gitq takes over its own TAB and
# nothing else's.
#
# The TUI needs nothing from the shell: it draws on /dev/tty and prints the
# chosen pipeline to stdout.  This captures that and offers it as the single
# completion, which bash substitutes into the line.
#
# Inside tmux it runs in a `display-popup`, sized against the client rather
# than the pane, so it fills the terminal however small the pane it was
# invoked from.  Outside tmux it uses the alternate screen.
#
# Opt-outs, set before sourcing:
#   GITQ_NO_TAB=1                          plain candidate completion instead
#   GITQ_POPUP_WIDTH / GITQ_POPUP_HEIGHT   popup geometry (default 100%/100%)

# The pipeline typed so far: everything after the command word, minus a
# wrapping quote, so gitq's tokenizer sees the pipeline itself.
_gitq_pipeline_of_line() {
    local line="${COMP_LINE:0:COMP_POINT}"
    local pipeline="${line#* }"
    [[ "$pipeline" == "$line" ]] && pipeline=""
    pipeline="${pipeline#[\"\']}"
    printf '%s' "$pipeline"
}

# Candidate completion, the same registry zsh and Emacs use — no grammar is
# duplicated shell-side.  Used when the TUI is switched off.
_gitq_plain_complete() {
    local pipeline
    pipeline=$(_gitq_pipeline_of_line)

    local IFS=$'\n'
    local cands
    cands=$(gitq --complete "$pipeline" 2>/dev/null) || return 0
    [[ -z "$cands" ]] && return 0

    # bash word-splitting: with the pipeline quoted (`gitq 'commits wh<TAB>`)
    # readline groups it all into one word, so the replacement has to carry
    # the already-typed head back with it.  Unquoted, only the last token is
    # replaced.
    local w="${COMP_WORDS[COMP_CWORD]}"
    local partial="${w##* }"
    local whead=""
    [[ "$partial" != "$w" ]] && whead="${w% *} "
    local qlead=""
    case "$partial" in
        \'*) qlead="'"; partial="${partial#\'}" ;;
        \"*) qlead='"'; partial="${partial#\"}" ;;
    esac

    COMPREPLY=()
    local c
    for c in $cands; do
        [[ "$c" == "$partial"* ]] && COMPREPLY+=("${whead}${qlead}${c}")
    done
}

_gitq_complete() {
    if [[ -n ${GITQ_NO_TAB-} ]] || ! command -v gitq >/dev/null 2>&1; then
        _gitq_plain_complete
        return
    fi

    local pipeline
    pipeline=$(_gitq_pipeline_of_line)

    # stdout comes back through a file: inside a popup the command runs in a
    # separate pane, so its stdout cannot be read from here.
    local tmp
    tmp=$(mktemp "${TMPDIR:-/tmp}/gitq-tui.XXXXXX") || return
    local bin
    bin=$(command -v gitq)

    if [[ -n ${TMUX-} ]] && command -v tmux >/dev/null 2>&1; then
        # an ABSOLUTE path: display-popup runs its command with the tmux
        # *server's* environment, not this shell's, so a gitq on a PATH set
        # in ~/.bashrc is simply not found and the popup dies with 127
        tmux display-popup -B -E \
            -w "${GITQ_POPUP_WIDTH:-100%}" -h "${GITQ_POPUP_HEIGHT:-100%}" \
            "$(printf %q "$bin") --complete-tui $(printf %q "$pipeline") > $(printf %q "$tmp")" \
            2>/dev/null
    else
        "$bin" --complete-tui "$pipeline" > "$tmp" 2>/dev/null
    fi

    local result
    result=$(<"$tmp")
    rm -f "$tmp"

    if [[ -n $result ]]; then
        # one completion replacing the whole pipeline argument, quoted so a
        # value with spaces survives; nospace keeps bash from adding one
        # after the closing quote
        COMPREPLY=( "'${result//\'/\'\\\'\'}'" )
        compopt -o nospace 2>/dev/null
    else
        # cancelled: leave the line as the user left it
        COMPREPLY=()
    fi
}

complete -F _gitq_complete gitq
