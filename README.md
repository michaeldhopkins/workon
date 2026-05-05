# workon

Development workspace launcher using Zellij.

Opens a project directory in a Zellij session using a layout you pick. The default layout is 3 panes:
- **Left top (80%)**: Claude CLI
- **Left bottom (20%)**: Terminal
- **Right (50%)**: branchdiff

You can switch to a different layout per invocation with `-c <name>` — see [Custom configs](#custom-configs).

## Dependencies

- [zellij](https://zellij.dev/) - Terminal multiplexer (always required)
- [jj](https://martinvonz.github.io/jj/) - Required for `-w` (workspace) mode

The remaining dependencies are derived from whichever config you launch. The default config requires:

- [claude](https://claude.ai/code) - Claude CLI
- [branchdiff](https://github.com/michaeldhopkins/branchdiff) - Git/jj diff TUI

A different config (see [Custom configs](#custom-configs)) may require different binaries — workon will detect them by parsing the active layout and tell you what's missing.

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
workon                       # open current directory with the default config
workon mbc                   # open ~/workspace/mbc
workon -n mbc                # force new session (destroys existing)
workon -w mbc                # ephemeral jj workspace (parallel session)
workon -c opencode           # open with the "opencode" custom config
workon -w fix-bug -c opencode  # workspace using a custom config
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

## Custom configs

A "config" is a named Zellij layout file. Pick one with `-c <name>`; the default is used when `-c` is omitted (or `-c default` is passed explicitly).

### Creating a config

Configs are zellij layout files (`.kdl`) stored in `~/.config/workon/configs/`. To make one:

**1. Create the directory** (one-time, if it doesn't exist yet):

```bash
mkdir -p ~/.config/workon/configs
```

**2. Pick a name** — letters, digits, `-`, or `_` only. The filename must match what you'll pass to `-c`. For example, to make a config called `opencode`:

```bash
$EDITOR ~/.config/workon/configs/opencode.kdl
```

**3. Write the layout.** Here's a 4-pane starter (opencode + branchdiff + specdiff) you can adapt:

```kdl
default_mode "locked"

layout {
    tab {
        pane split_direction="vertical" {
            pane split_direction="horizontal" size="50%" {
                pane command="opencode" size="80%" focus=true
                pane size="20%"
            }
            pane split_direction="horizontal" size="50%" {
                pane command="branchdiff" size="80%"
                pane command="specdiff" size="20%"
            }
        }
    }
}

on_force_close "quit"
session_serialization false
```

**4. Launch it:**

```bash
workon -c opencode
```

**Tips:**

- Mark exactly one pane with `focus=true`. workon uses it to detect which config a running session was launched with. Without it, the [layout-mismatch guard](#layout-mismatch-guard) can't reliably distinguish your configs.
- Every `command="..."` in the layout must be a binary on your `PATH`. workon checks before launching and tells you what's missing.
- For the full layout syntax, see the [zellij layout docs](https://zellij.dev/documentation/creating-a-layout.html).

### Where configs are loaded from

The default config (no `-c` flag, or `-c default`) is resolved in this order:

1. `~/.config/workon/configs/default.kdl` — your override, if present
2. `~/.config/workon/layout.kdl` — legacy single-config path, still honored
3. The embedded default layout (claude + branchdiff)

Named lookup (`-c foo`) only checks `~/.config/workon/configs/foo.kdl` and errors if the file is missing.

### Layout-mismatch guard

A zellij session is named after its project directory, and zellij ignores the layout when attaching to an existing session. To prevent silently re-attaching with the wrong layout, workon inspects the running session's process tree and refuses to attach if the requested config's focused command isn't present:

```
$ workon -c opencode    # but a default-config session is already running
Error: zellij session 'workon' is already running in the main worktree with a
different layout. Zellij keeps the original layout when reattaching, so
'opencode' would be ignored.

To open it as a separate workspace, run: workon -w -c opencode
To replace the running session instead:  workon -n -c opencode
```

This works in both directions — bare `workon` against a session you started with `-c opencode` will also be refused.

### `--resume` requires a claude config

`-r/--resume` injects `--session-id` into the layout's `claude` pane. Configs without a `command="claude"` pane are rejected up-front:

```
$ workon -w --resume <id> -c opencode
Error: --resume only works with claude-based configs (active config: opencode)
```

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
