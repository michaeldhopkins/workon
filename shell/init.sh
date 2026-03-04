#!/usr/bin/env bash

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
    local target_dir
    local tab_name
    local new_session=false
    local use_workspace=false

    if ! _workon_check_deps; then
        echo "Install missing dependencies and try again." >&2
        return 1
    fi

    while [[ "$1" == -* ]]; do
        case "$1" in
            -n) new_session=true; shift ;;
            -w) use_workspace=true; shift ;;
            *) echo "Unknown flag: $1"; return 1 ;;
        esac
    done

    if [[ -n "$1" ]]; then
        if [[ -d "$1" ]]; then
            target_dir="$(cd "$1" && pwd)"
        elif [[ -d "$HOME/workspace/$1" ]]; then
            target_dir="$HOME/workspace/$1"
        else
            echo "Directory not found: $1 (also checked ~/workspace/$1)"
            return 1
        fi
    else
        target_dir="$(pwd)"
    fi

    local project_name
    project_name="$(basename "$target_dir")"

    if [[ "$use_workspace" == true ]]; then
        _workon_ensure_jj "$target_dir" || return 1
        _workon_workspace "$target_dir" "$project_name"
    else
        tab_name="$project_name"
        cd "$target_dir"

        if zellij list-sessions --no-formatting 2>/dev/null | grep -q "^$tab_name "; then
            if [[ "$new_session" == true ]]; then
                zellij delete-session "$tab_name" --force
                zellij --new-session-with-layout "$_WORKON_ROOT/layouts/workon.kdl" --session "$tab_name"
            else
                zellij attach "$tab_name"
            fi
        else
            zellij --new-session-with-layout "$_WORKON_ROOT/layouts/workon.kdl" --session "$tab_name"
        fi
    fi
}

_workon_ensure_jj() {
    local project_dir="$1"

    if [[ -d "$project_dir/.git" ]] && [[ ! -d "$project_dir/.jj" ]]; then
        echo "Initializing jj colocated repo in $project_dir..."
        jj git init --colocate -R "$project_dir"

        local main_branch
        if git -C "$project_dir" rev-parse --verify origin/master >/dev/null 2>&1; then
            main_branch="master"
        else
            main_branch="main"
        fi

        jj bookmark track "${main_branch}@origin" -R "$project_dir"
        jj config set --repo remotes.origin.auto-track-bookmarks "glob:*" -R "$project_dir"
        echo "jj initialized, tracking ${main_branch}@origin"
    fi
}

_workon_workspace() {
    local project_dir="$1"
    local project_name="$2"
    local ws_id
    ws_id="ws-$(head -c 4 /dev/urandom | xxd -p | head -c 6)"
    local ws_dir="$HOME/.worktrees/${project_name}-${ws_id}"
    local tab_name="${project_name}-${ws_id}"
    local created_db=""

    mkdir -p "$HOME/.worktrees"

    local main_branch
    main_branch=$(jj -R "$project_dir" log -r 'trunk()' --no-graph -T 'bookmarks' --limit 1 | sed 's/@.*//')

    echo "Creating jj workspace ${ws_id}..."
    if ! jj -R "$project_dir" workspace add "$ws_dir" --name "$ws_id" -r "$main_branch" 2>&1; then
        echo "Failed to create jj workspace"
        return 1
    fi

    if [[ -d "$project_dir/.claude" ]]; then
        ln -s "$project_dir/.claude" "$ws_dir/.claude"
    fi

    if [[ -f "$project_dir/.env" ]]; then
        cp "$project_dir/.env" "$ws_dir/.env"
    fi

    if [[ -f "$ws_dir/config/database.yml" ]]; then
        local db_name="${project_name}_ws_${ws_id}_test"
        db_name="${db_name//-/_}"
        echo "Creating test database ${db_name}..."
        if createdb "$db_name" 2>/dev/null; then
            created_db="$db_name"
            echo "DATABASE_URL=postgresql://localhost/${db_name}" > "$ws_dir/.env.test.local"
            echo "Loading schema..."
            (cd "$ws_dir" && RAILS_ENV=test DATABASE_URL="postgresql://localhost/${db_name}" bundle exec rails db:schema:load 2>&1)
        else
            echo "Warning: could not create test database ${db_name}"
        fi
    fi

    # Pre-approve workspace trust so Claude doesn't prompt on launch
    local claude_json="$HOME/.claude.json"
    if [[ -f "$claude_json" ]]; then
        jq --arg dir "$ws_dir" '.projects[$dir].hasTrustDialogAccepted = true' "$claude_json" > "${claude_json}.tmp" \
            && mv "${claude_json}.tmp" "$claude_json"
    fi

    cd "$ws_dir"
    zellij --new-session-with-layout "$_WORKON_ROOT/layouts/workon.kdl" --session "$tab_name"

    echo ""
    echo "Cleaning up workspace ${ws_id}..."

    local has_changes
    has_changes=$(jj -R "$project_dir" log --ignore-working-copy -r "${ws_id}@" --no-graph -T 'if(empty, "", "changes")' 2>/dev/null)

    if [[ -n "$has_changes" ]]; then
        echo "Workspace has uncommitted changes."
        echo -n "Auto-bookmark as workon/${ws_id}? [y/N] "
        read -r answer
        if [[ "$answer" == [yY] ]]; then
            jj -R "$project_dir" bookmark set "workon/${ws_id}" -r "${ws_id}@"
            echo "Bookmarked as workon/${ws_id}"
        fi
    fi

    jj -R "$project_dir" workspace forget "$ws_id" 2>/dev/null
    echo "Forgot jj workspace ${ws_id}"

    if [[ -n "$created_db" ]]; then
        dropdb "$created_db" 2>/dev/null && echo "Dropped test database ${created_db}"
    fi

    rm -rf "$ws_dir"
    echo "Removed workspace directory"

    cd "$project_dir"
}
