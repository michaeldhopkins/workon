#!/usr/bin/env bash

# workon - Development workspace launcher
# Source this file in your shell config: source /path/to/workon/shell/init.sh

# Detect script location (works in bash and zsh)
if [[ -n "${BASH_SOURCE[0]}" ]]; then
    _WORKON_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
elif [[ -n "${(%):-%x}" ]]; then
    _WORKON_ROOT="$(cd "$(dirname "${(%):-%x}")/.." && pwd)"
else
    echo "workon: unable to detect script location" >&2
fi

_workon_check_deps() {
    local missing=()

    command -v zellij >/dev/null 2>&1 || missing+=("zellij")
    command -v claude >/dev/null 2>&1 || missing+=("claude")
    command -v branchdiff >/dev/null 2>&1 || missing+=("branchdiff")

    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "workon: missing dependencies: ${missing[*]}" >&2
        return 1
    fi
    return 0
}

workon() {
    local dir="${1:-.}"

    if ! _workon_check_deps; then
        echo "Install missing dependencies and try again." >&2
        return 1
    fi

    if [[ ! -d "$dir" ]]; then
        echo "workon: directory not found: $dir" >&2
        return 1
    fi

    dir="$(cd "$dir" && pwd)"
    cd "$dir" || return 1
    zellij --layout "$_WORKON_ROOT/layouts/workon.kdl"
}
