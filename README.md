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

```bash
brew install michaeldhopkins/tap/workon
```

Or build from source:

```bash
cargo install workon
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
3. Clones gitignored files (build artifacts, `node_modules/`, `target/`, etc.) using APFS `clonefile(2)` on macOS for near-instant copy-on-write directory cloning, with cross-platform reflink fallback via [clonetree](https://crates.io/crates/clonetree)
4. For Rails apps: creates an isolated test database and loads the schema
5. Launches a Zellij session in the workspace
6. On exit: prompts to bookmark uncommitted work, forgets the jj workspace, drops any test database, and removes the directory in the background

The primary session (`workon mbc`) is unaffected — it works directly in the project directory as before.

### Limitations

- The workspace shares the development database with the primary session. Don't run migrations or the Rails server from a workspace.
- `parallel_rspec` uses shared test databases. Use `bundle exec rspec` in the workspace for isolated specs.

## Custom layout

The default Zellij layout is embedded in the binary. To override it, create `~/.config/workon/layout.kdl` with your preferred layout.

## Session management

Sessions are named after the directory basename. Running `workon mbc` twice reattaches to the existing session. Use `-n` to start fresh.

If a zellij server is hung, workon detects the unresponsive IPC (5s timeout), kills only the server bound to your project's session (other sessions are left alone), removes the stale socket, and launches a fresh session. Recovery runs whether or not you pass `-n`. Pre-flight checks before `attach` and `launch` ensure the no-timeout interactive zellij commands won't block on a wedged server.

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
