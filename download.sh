#!/usr/bin/env bash
# Download all *.txt URL lists in a directory. Music goes to the current directory.
set -euo pipefail

usage() {
    printf 'Usage: %s <cookie> <user-agent> [lists-directory]\n' "${0##*/}"
    printf 'Quote the cookie, user-agent, and paths containing spaces. Default lists directory: current directory.\n'
}

if [[ $# -eq 1 && ( $1 == --help || $1 == -h ) ]]; then
    usage
    exit 0
fi
if [[ $# -lt 2 || $# -gt 3 ]]; then
    usage >&2
    exit 2
fi
if [[ -z $1 || -z $2 ]]; then
    printf 'Cookie and user-agent must not be empty.\n' >&2
    exit 2
fi
lists_dir=${3:-.}
if [[ ! -d $lists_dir || ! -r $lists_dir || ! -x $lists_dir ]]; then
    printf 'Cannot read lists directory: %s\n' "$lists_dir" >&2
    exit 1
fi
if ! command -v lucida >/dev/null 2>&1; then
    printf 'Cannot find lucida in PATH.\n' >&2
    exit 1
fi

# Prefix relative paths so a directory starting with '-' cannot become an option.
[[ $lists_dir == /* ]] || lists_dir="./$lists_dir"
printf 'Downloading URL lists in %s; music output directory: %s\n' "$lists_dir" "$PWD" >&2
exec lucida --cf-clearance "$1" --user-agent "$2" -f "$lists_dir"
