#!/bin/sh
# Install or verify the `tessera` terminfo entry (ADR 0005).
#
# Usage:
#   install.sh                 compile into ~/.terminfo on this machine
#   install.sh --ssh HOST      compile into ~/.terminfo on HOST over ssh
#   install.sh --check         compile into a temp dir and print the
#                              extended capabilities; exit 1 if any is missing
#
# Only `tic` (ncurses) is needed, locally or on the remote host. The source is
# streamed over stdin so nothing has to be copied first, the same trick kitty
# and WezTerm use. Extended capabilities require `tic -x`.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
src="$here/tessera.terminfo"
required="fullkbd Smulx Setulc Su Tc Sync kxIN kxOUT"

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

check() {
    dir=$(mktemp -d)
    trap 'rm -rf "$dir"' EXIT
    tic -x -o "$dir" "$src"
    dump=$(TERMINFO="$dir" infocmp -1 -x tessera)
    status=0
    for cap in $required; do
        if printf '%s\n' "$dump" | grep -q "^[[:space:]]*${cap}[,=]"; then
            printf 'ok       %s\n' "$cap"
        else
            printf 'MISSING  %s\n' "$cap"
            status=1
        fi
    done
    printf '%s\n' "$dump" | grep -E '^[[:space:]]*(fullkbd|Smulx|Setulc|Sync)' || true
    exit "$status"
}

case "${1:-}" in
    "")
        mkdir -p "$HOME/.terminfo"
        tic -x -o "$HOME/.terminfo" "$src"
        echo "installed 'tessera' into $HOME/.terminfo"
        ;;
    --ssh)
        [ $# -eq 2 ] || usage
        # shellcheck disable=SC2029 # the quoted string is meant to run remotely
        ssh "$2" 'mkdir -p "$HOME/.terminfo" && tic -x -o "$HOME/.terminfo" /dev/stdin' < "$src"
        echo "installed 'tessera' into ~/.terminfo on $2"
        ;;
    --check)
        check
        ;;
    *)
        usage
        ;;
esac
