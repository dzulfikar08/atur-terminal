# Restore the user's startup directory before zsh reads the remaining startup files.
if [[ ${ATUR_TERM_ZDOTDIR_SET-} == 1 ]]; then
    export ZDOTDIR=$ATUR_TERM_ORIGINAL_ZDOTDIR
else
    unset ZDOTDIR
fi
unset ATUR_TERM_ZDOTDIR_SET ATUR_TERM_ORIGINAL_ZDOTDIR
[[ -r ${ZDOTDIR-$HOME}/.zshenv ]] && source "${ZDOTDIR-$HOME}/.zshenv"
[[ -o interactive && ${ATUR_TERM_SHELL_INTEGRATION-} == 1 ]] && source "$ATUR_TERM_SHELL_INTEGRATION_DIR/atur.zsh"
