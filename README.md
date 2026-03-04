# workon

Development workspace launcher using Zellij.

Opens a project directory in a 3-pane Zellij layout:
- **Left top (80%)**: Claude CLI
- **Left bottom (20%)**: Terminal
- **Right (50%)**: branchdiff

## Dependencies

- [zellij](https://zellij.dev/) - Terminal multiplexer
- [claude](https://claude.ai/code) - Claude CLI
- [branchdiff](https://github.com/michaeldhopkins/branchdiff) - Git/jj diff TUI
- [jj](https://martinvonz.github.io/jj/) - Required for `-w` (workspace) mode

## Installation

Add to your `~/.zshrc` or `~/.bashrc`:

```bash
source /path/to/workon/shell/init.sh
```

## Usage

```bash
workon                # open current directory
workon mbc            # open ~/workspace/mbc
workon -n mbc         # force new session (destroys existing)
workon -w mbc         # ephemeral jj workspace (parallel session)
```

## Workspace mode (`-w`)

Creates an ephemeral jj workspace in `~/.worktrees/` for running a second independent session on the same repo. The workspace is cleaned up when the Zellij session closes.

What it does:
1. Initializes jj (colocated) if the project only has git
2. Creates a jj workspace branched from trunk (main/master)
3. Symlinks `.claude/` and copies `.env` from the main repo
4. For Rails apps: creates an isolated test database and loads the schema
5. Launches a Zellij session in the workspace
6. On exit: prompts to bookmark uncommitted work, then cleans up the workspace, test database, and directory

The primary session (`workon mbc`) is unaffected — it works directly in the project directory as before.

### Limitations

- The workspace shares the development database with the primary session. Don't run migrations or the Rails server from a workspace.
- `parallel_rspec` uses shared test databases. Use `bundle exec rspec` in the workspace for isolated specs.

## Session management

Sessions are named after the directory basename. Running `workon mbc` twice reattaches to the existing session. Use `-n` to start fresh.

Workspace sessions are named `<project>-ws-<id>` (e.g., `mbc-ws-a1b2c3`) and don't collide with primary sessions.

## Claude Code setup

To skip the workspace trust prompt and auto-allow file operations in worktrees, add to `~/.claude/settings.json`:

```json
{
  "permissions": {
    "allow": [
      "Edit(~/.worktrees/**)",
      "Write(~/.worktrees/**)",
      "Read(~/.worktrees/**)"
    ]
  }
}
```

Workspace trust is also pre-seeded in `~/.claude.json` automatically on each `workon -w` launch.

## Tips

- **Click URLs**: `Cmd+Shift+Click` to open hyperlinks (Shift bypasses zellij mouse handling)
- **Locked mode**: Zellij starts in locked mode. Press `Ctrl+G` to unlock for Zellij features.
