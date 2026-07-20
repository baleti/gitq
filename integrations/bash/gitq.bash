# bash completion for the gitq pipeline CLI.
#
# Install: `make install-bash` in the repo root (symlinks this file into
# ${XDG_DATA_HOME:-~/.local/share}/bash-completion/completions/gitq), or
# source it from ~/.bashrc:
#   source /path/to/gitq/integrations/bash/gitq.bash
#
# How it works:
#   gitq takes its whole pipeline as a single string argument, so
#   completion happens *within* that one argument, exactly like the zsh
#   integration (integrations/zsh/_gitq).  We hand the pipeline text to
#   `gitq --complete PREFIX`, which prints the candidate set for that
#   position, one per line; gitq's tokenizer finds the token boundary.
#   No grammar is duplicated shell-side.
#
#   The wrinkle is bash word-splitting.  When the pipeline is a single
#   quoted argument (`gitq 'commits wh<TAB>`), readline groups the whole
#   thing into one COMP_WORD, so the "word" it replaces is the entire
#   `'commits wh`, not just `wh` — the replacement text therefore has to
#   carry the already-typed head back with it.  When the pipeline is left
#   unquoted (`gitq commits wh<TAB>`), readline splits normally and only
#   `wh` is replaced.  We compute the head to preserve relative to the
#   current word, so both cases produce the right replacement.

_gitq_complete() {
    # The pipeline text so far, reconstructed from the raw line (COMP_WORDS
    # is lossy for the quoting gitq cares about).  Everything after the
    # command word is the single pipeline argument.
    local line="${COMP_LINE:0:COMP_POINT}"
    local pipeline="${line#* }"
    [[ "$pipeline" == "$line" ]] && pipeline=""
    # Strip a leading quote so gitq's tokenizer sees the pipeline itself.
    pipeline="${pipeline#[\"\']}"

    local IFS=$'\n'
    local cands
    cands=$(gitq --complete "$pipeline" 2>/dev/null) || return 0
    [[ -z "$cands" ]] && return 0

    # The word readline will replace, and — within it — the partial token
    # being completed plus the head (prior tokens / opening quote) to keep.
    local w="${COMP_WORDS[COMP_CWORD]}"
    local partial="${w##* }"
    local whead=""
    [[ "$partial" != "$w" ]] && whead="${w% *} "
    # Preserve an opening quote that sits on the partial token itself, but
    # match candidates against the unquoted text.
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

complete -o default -F _gitq_complete gitq
